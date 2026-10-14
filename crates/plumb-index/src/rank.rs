//! Ranking the candidates a query finds: the blend of text match, names and
//! popularity the crate's documentation describes.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use plumb_core::{
    is_function_word, kind_key, language_fits, normalize_country, registrable_domain, AdultLevel,
};
use tantivy::collector::TopDocs;
use tantivy::query::{EnableScoring, Query, Scorer};
use tantivy::schema::Value;
use tantivy::{DocAddress, DocSet, Order, TantivyDocument, Term};

use crate::hits::{without_copies, without_near_copies};
use crate::names::{is_reserved_name, navigation_subject, NameMatch, Named};
use crate::query::{matching_docs, ParsedQuery};
use crate::{
    analysis, health, pages, schema, CandidateEvidence, CandidatePool, Hit, Meaning, RankConfig,
    SearchOptions, SearchResults, Searcher, WELL_KNOWN_LINK_SCORE,
};

/// The most popular sites matching any query word that are ranked even if
/// BM25 put them below [`RankConfig::candidates`] others: a query that
/// describes a big site ("watch videos online") matches many small ones
/// that repeat its words.
const POPULAR_CANDIDATES: usize = 50;

/// The sites nearest a query in meaning ([`Meaning::nearest`]) that are
/// ranked, nearest first.
pub(crate) const NEAREST_RANKED: usize = 50;

/// The most popular of the other sites [`Meaning::nearest`] gives that are
/// ranked too.
const NEAREST_POPULAR: usize = 50;

/// Results looked at past the ones asked for, to fill the places of the
/// copies [`without_copies`] leaves out.
const COPY_LOOKAHEAD: usize = 10;

impl Searcher {
    /// The ranked results of `query_text` as typed, and how the query names
    /// sites.
    pub(crate) fn rank(
        &self,
        query_text: &str,
        limit: usize,
        cfg: &RankConfig,
        options: &SearchOptions,
        meaning: Option<&dyn Meaning>,
    ) -> Result<(SearchResults, Named)> {
        self.rank_traced(query_text, limit, cfg, options, meaning, None)
    }

    /// [`Searcher::rank`], noting in `pool` where the sites it ranks came
    /// from.
    pub(crate) fn rank_traced(
        &self,
        query_text: &str,
        limit: usize,
        cfg: &RankConfig,
        options: &SearchOptions,
        meaning: Option<&dyn Meaning>,
        pool: Option<&mut CandidatePool>,
    ) -> Result<(SearchResults, Named)> {
        if limit == 0 {
            return Ok(Default::default());
        }
        let Some(mut query) = ParsedQuery::new(query_text, &self.words, &self.joined) else {
            return Ok(Default::default());
        };
        query.terms_boost = cfg.terms_boost;
        query.filler_words = cfg.filler_words;
        query.asked =
            cfg.questions_name_nothing && query.len >= 3 && pages::asked_as_question(query_text);
        let searcher = self.reader.searcher();
        let num_docs = usize::try_from(searcher.num_docs()).unwrap_or(usize::MAX);
        if num_docs == 0 {
            return Ok(Default::default());
        }

        let text_query = query.text_query(&searcher, &self.fields)?;
        let num_candidates = cfg.candidates.max(limit).min(num_docs);
        let mut candidates = searcher.search(
            &text_query,
            &TopDocs::with_limit(num_candidates).order_by_score(),
        )?;
        let popular: Vec<DocAddress> = searcher
            .search(
                &text_query,
                &TopDocs::with_limit(POPULAR_CANDIDATES.min(num_docs))
                    .order_by_fast_field::<f64>(schema::LINK_SCORE, Order::Desc),
            )?
            .into_iter()
            .map(|(_, addr)| addr)
            .collect();
        // A site the query names is ranked even if BM25 put others first:
        // it is what the look-alikes are measured against. So is every site
        // of the kind the query names.
        let mut names = self.name_matches(&searcher, &query, cfg)?;
        let mut kinds = match &query.kind {
            Some(key) => matching_docs(
                &searcher,
                vec![Term::from_field_text(self.fields.kind_key, key)],
            )?,
            None => HashSet::new(),
        };
        // A kind of two words or more that a candidate's own title or
        // description names ("Wikipedia is a free online encyclopedia")
        // makes it one of that kind too: Wikidata does not tag every site
        // with every kind it is (wikipedia.org's item is a "Wikimedia
        // content project"). Single words ("airlines") are too often just
        // mentioned ("cheap airline tickets") to count.
        if !kinds.is_empty() && query.len >= 2 {
            let key = kind_key(query_text);
            for &(_, addr) in &candidates {
                if !kinds.contains(&addr)
                    && self.describes_itself_as(&searcher, addr, &key, query.len)?
                {
                    kinds.insert(addr);
                }
            }
        }
        // A query that is a kind of thing ("banks") asks for every site of
        // that kind: a site linked to with the word is not named by it.
        if !kinds.is_empty() {
            for name in names.values_mut() {
                name.linked = 0;
            }
            names.retain(|_, name| name.words() > 0 || name.typed);
        }
        // Meaning helps with queries that describe a site, not with names.
        // Words count as a name when some site is known by them: a domain
        // that only spells them (email-provider.eu, globalwarming.org) is a
        // namesake of the words, and leaves the query described.
        let mut named_in_full = !kinds.is_empty();
        for (&addr, name) in &names {
            if named_in_full {
                break;
            }
            named_in_full = name.words() >= query.len
                && (query.len < 2
                    || name.typed
                    || name.linked >= query.len
                    || self.known_by_name(&searcher, addr)?);
        }
        // Still asked, for a query named in full, whether a site that has
        // only some of its words is anything like it (debian.org and
        // systemofadown.com for "solar system").
        let any_meaning = meaning;
        let meaning = meaning.filter(|_| !named_in_full);
        // The nearest sites in meaning, and the most popular of the next
        // nearest: among hundreds of thousands of sites, small ones whose
        // text repeats the query's words crowd out the big site it
        // describes, which may say little about itself.
        let nearest = match meaning {
            Some(meaning) => {
                let domains = meaning.nearest();
                let terms = |domains: &[String]| -> Vec<Term> {
                    domains
                        .iter()
                        .map(|domain| Term::from_field_text(self.fields.domain, domain))
                        .collect()
                };
                let split = domains.len().min(NEAREST_RANKED);
                let ranked = matching_docs(&searcher, terms(&domains[..split]))?;
                let mut docs = ranked.clone();
                let next = matching_docs(&searcher, terms(&domains[split..]))?;
                let mut next = link_scores(&searcher, &next);
                next.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                docs.extend(next.into_iter().take(NEAREST_POPULAR).map(|(_, addr)| addr));
                docs
            }
            None => HashSet::new(),
        };
        let known: HashSet<DocAddress> = candidates.iter().map(|&(_, addr)| addr).collect();
        let mut unranked: Vec<DocAddress> = names
            .keys()
            .chain(kinds.iter())
            .chain(nearest.iter())
            .chain(popular.iter())
            .filter(|addr| !known.contains(addr))
            .copied()
            .collect();
        unranked.sort_unstable();
        unranked.dedup();
        let mut pool = pool;
        if let Some(pool) = pool.as_deref_mut() {
            let domains = |docs: &mut dyn Iterator<Item = DocAddress>| -> Vec<String> {
                docs.filter_map(|addr| domain_of(&searcher, addr)).collect()
            };
            *pool = CandidatePool {
                words: domains(&mut candidates.iter().map(|&(_, addr)| addr)),
                popular: domains(&mut popular.iter().copied()),
                named: domains(&mut names.keys().copied()),
                kind: domains(&mut kinds.iter().copied()),
                meaning: domains(&mut nearest.iter().copied()),
                evidence: Vec::new(),
            };
        }
        candidates.extend(bm25_of(&searcher, &text_query, unranked)?);
        let full_names: HashSet<DocAddress> = names
            .iter()
            .filter(|(_, name)| name.words() >= query.len)
            .map(|(&addr, _)| addr)
            .collect();
        let named = Named {
            words: names.values().map(NameMatch::words).max().unwrap_or(0),
            full_link_score: (!full_names.is_empty())
                .then(|| best_link_score(&searcher, &full_names)),
            kind: !kinds.is_empty(),
            typed: query.domain.is_some(),
        };
        if candidates.is_empty() {
            return Ok((SearchResults::default(), named));
        }

        let columns = searcher
            .segment_readers()
            .iter()
            .map(|segment| {
                let fast = segment.fast_fields();
                Ok(Columns {
                    link_scores: fast.f64(schema::LINK_SCORE)?,
                    domains: fast.str(schema::DOMAIN)?,
                    countries: fast.str(schema::COUNTRY)?,
                    languages: fast.str(schema::LANGUAGE)?,
                    adult: fast.u64(schema::ADULT)?,
                })
            })
            .collect::<tantivy::Result<Vec<_>>>()?;
        let link_score_of = |addr: DocAddress| {
            columns[addr.segment_ord as usize]
                .link_scores
                .first(addr.doc_id)
                .unwrap_or(0.0) as f32
        };

        let default = RankConfig::default();
        let navigation_names = match navigation_subject(query_text)
            .and_then(|subject| ParsedQuery::new(&subject, &self.words, &self.joined))
        {
            Some(subject) => self
                .name_matches(&searcher, &subject, cfg)?
                .into_iter()
                .filter(|&(addr, name)| {
                    link_score_of(addr) >= WELL_KNOWN_LINK_SCORE && name.words() >= subject.len
                })
                .map(|(addr, _)| addr)
                .collect::<HashSet<_>>(),
            None => HashSet::new(),
        };
        // A leading name needs an explicit navigation intent, rather than
        // merely the popularity of a namesake (Amazon in Amazon River).
        let navigational = named_in_full
            || query.domain.is_some()
            || names.iter().any(|(&addr, name)| {
                name.typed
                    || (name.words() > 0
                        && (!cfg.whole_query_relevance || navigation_names.contains(&addr))
                        && link_score_of(addr) >= WELL_KNOWN_LINK_SCORE)
            })
            || !navigation_names.is_empty();
        let alpha = match cfg.described_alpha {
            Some(described) if !navigational => unit_or(described, default.alpha),
            _ => unit_or(cfg.alpha, default.alpha),
        };
        let question_floor = cfg
            .question_relevance
            .filter(|_| query.len >= 2 && !named_in_full && pages::asked_as_question(query_text));
        let relevance_floor = match (
            if navigational {
                cfg.navigational_relevance
            } else {
                cfg.described_relevance
            },
            question_floor,
        ) {
            (Some(floor), Some(question)) => Some(floor.max(question)),
            (floor, question) => floor.or(question),
        }
        .filter(|floor| *floor > 0.0);
        let partial_label_bonus = cfg.partial_label_bonus.unwrap_or(cfg.exact_label_bonus);
        let untrusted_share = unit_or(cfg.untrusted_share, default.untrusted_share);
        let country_boost = unit_or(cfg.country_boost, default.country_boost);
        let home = options.country.as_deref().and_then(normalize_country);
        // The evidence a site needs for its text to count in full: none
        // unless the query is a site's name plus more words, and never more
        // than that site has. A query that is just the name needs no guard:
        // the named site wins it on its own.
        let named_link_score = names
            .iter()
            .filter(|(_, name)| name.words() < query.len)
            .map(|(&addr, _)| link_score_of(addr))
            .fold(0.0, f32::max);
        let trusted_link_score =
            unit_or(cfg.trusted_link_score, default.trusted_link_score).min(named_link_score);
        // Sites the query names in full that nothing else says anything
        // about: never crawled (no title, description or Wikidata words),
        // no link text and no other name (from Wikidata or a redirect), and
        // less linked than the site the query names by its first words.
        // Such a bare name (you-tubemusic.com) is only a spelling of the
        // query, so it gets no more trust than a site with no links does:
        // youtube.com wins "youtube music". A site people link to by name
        // is not bare, crawled or not (homedepot.com, which turns crawlers
        // away, for "home depot"), nor is one with pages of its own
        // (json-schema.org for "json schema").
        let mut bare: HashSet<DocAddress> = HashSet::new();
        if trusted_link_score > 0.0 {
            for (&addr, name) in &names {
                if name.typed || name.words() < query.len || link_score_of(addr) >= named_link_score
                {
                    continue;
                }
                let segment = searcher.segment_reader(addr.segment_ord);
                let mut said = false;
                for field in [
                    self.fields.title,
                    self.fields.description,
                    self.fields.about,
                    self.fields.anchors,
                    self.fields.aliases,
                ] {
                    if let Some(norms) = segment.fieldnorms_readers().get_field(field)? {
                        said |= norms.fieldnorm(addr.doc_id) > 0;
                    }
                }
                if !said {
                    bare.insert(addr);
                }
            }
        }
        let query_words = query.len as f32;
        let meaning_weight = unit_or(cfg.meaning_weight, default.meaning_weight);

        // A site with no embedding (no text to make one from) is taken to
        // be as close in meaning as its words match, times the share of the
        // query they cover: electric.net matches "electric car maker" well
        // but by one word in three, so it is not put level with the sites
        // the query describes; schwab.com, matching all of "charles
        // schwab", is.
        let closeness_of = |addr: DocAddress| {
            any_meaning.and_then(|meaning| {
                columns[addr.segment_ord as usize]
                    .domain(addr.doc_id)
                    .and_then(|domain| meaning.closeness(&domain))
            })
        };
        let plain_closeness_of = |addr: DocAddress| {
            meaning.and_then(|meaning| {
                columns[addr.segment_ord as usize]
                    .domain(addr.doc_id)
                    .map(|domain| meaning.plain_closeness(&domain))
            })
        };
        let domain_collision_of = |addr: DocAddress| {
            columns[addr.segment_ord as usize]
                .domain(addr.doc_id)
                .is_some_and(|domain| {
                    let label = analysis::tokens(&self.words, &schema::label_text(&domain));
                    !label.is_empty()
                        && label.iter().all(|word| {
                            query.words.contains(word)
                                || query.others.iter().flatten().any(|other| other == word)
                        })
                })
        };
        let addresses: Vec<_> = candidates.iter().map(|&(_, addr)| addr).collect();
        let evidence = query.whole_query_evidence(&searcher, &self.fields, &addresses)?;
        let tokens = analysis::tokens(&self.words, query_text);
        let prefix_distinct: HashMap<_, _> = names
            .values()
            .map(|name| {
                (
                    name.words(),
                    tokens
                        .iter()
                        .take(name.words())
                        .filter(|token| query.words.contains(token))
                        .collect::<HashSet<_>>()
                        .len(),
                )
            })
            .collect();
        // label_key also contains structured names and redirect identities.
        // Preserve those independently of borrowed homepage text, without
        // treating a domain spelling repeated in its own text as an alias.
        let independent_labels: HashMap<_, _> = names
            .iter()
            .filter_map(|(&addr, name)| {
                let domain = columns[addr.segment_ord as usize].domain(addr.doc_id)?;
                let label = plumb_core::joined(&schema::label_text(&domain));
                let raw_words = query
                    .leading
                    .iter()
                    .filter(|(key, _)| *key == label)
                    .map(|(_, words)| *words)
                    .max()
                    .unwrap_or(0);
                (name.label > raw_words).then_some((addr, name.label))
            })
            .collect();
        let full_identity = names.iter().any(|(addr, name)| {
            name.typed
                || name.alias >= query.len
                || name.linked >= query.len
                || independent_labels
                    .get(addr)
                    .is_some_and(|&words| words >= query.len)
        });
        let semantic_floor = cfg.meaning_only_relevance.unwrap_or(0.35).max(f32::EPSILON);
        // Whole-query support resolves competing established identities.
        // Without it, only an independently dominant link name or one
        // unambiguous identity can scope the query. Weak semantic evidence
        // does not erase an otherwise known subject and let a task-only
        // nearest result claim it. Complete names precede partial ones.
        let named_subject = if cfg.whole_query_relevance && !full_identity {
            let subject_names: Vec<_> = names
                .iter()
                .flat_map(|(&addr, name)| {
                    [
                        (name.alias, "alias"),
                        (name.linked, "independent_links"),
                        (
                            independent_labels.get(&addr).copied().unwrap_or(0),
                            "structured_name",
                        ),
                    ]
                    .map(move |(words, reason)| (addr, words, reason))
                })
                .filter(|(_, words, _)| {
                    *words > 0
                        && *words < query.len
                        && !tokens
                            .get(*words)
                            .is_some_and(|word| is_function_word(word))
                })
                .map(|(addr, words, reason)| {
                    let compatible = evidence.get(&addr).is_some_and(|lexical| {
                        let distinct = tokens.iter().take(words).collect::<HashSet<_>>().len();
                        lexical.substantive >= 0.75
                            && lexical
                                .task_coverage(distinct)
                                .is_none_or(|task| task > 0.0)
                            || closeness_of(addr)
                                .is_some_and(|score| unit_or(score, 0.0) >= semantic_floor)
                    });
                    (addr, words, reason, compatible)
                })
                .collect();
            let mut identities: HashMap<usize, HashSet<String>> = HashMap::new();
            for &(addr, words, _, _) in &subject_names {
                if let Some(domain) = columns[addr.segment_ord as usize].domain(addr.doc_id) {
                    identities.entry(words).or_default().insert(domain);
                }
            }
            subject_names
                .into_iter()
                .filter(|(_, words, reason, compatible)| {
                    *compatible
                        || *reason == "independent_links"
                        || identities.get(words).is_some_and(|sites| sites.len() == 1)
                })
                .max_by(
                    |(a, a_words, a_reason, a_compatible), (b, b_words, b_reason, b_compatible)| {
                        a_compatible
                            .cmp(b_compatible)
                            .then_with(|| {
                                (*a_reason == "independent_links")
                                    .cmp(&(*b_reason == "independent_links"))
                            })
                            .then_with(|| a_words.cmp(b_words))
                            .then_with(|| {
                                closeness_of(*a)
                                    .unwrap_or(0.0)
                                    .total_cmp(&closeness_of(*b).unwrap_or(0.0))
                            })
                            .then_with(|| {
                                columns[b.segment_ord as usize]
                                    .domain(b.doc_id)
                                    .cmp(&columns[a.segment_ord as usize].domain(a.doc_id))
                            })
                    },
                )
                .map(|(addr, words, reason, compatible)| {
                    let reason = match (reason, compatible) {
                        ("alias", false) => "unambiguous_alias",
                        ("structured_name", false) => "unambiguous_structured_name",
                        (reason, _) => reason,
                    };
                    (addr, words, reason)
                })
        } else {
            None
        };
        let named_subject_words = named_subject.map_or(0, |(_, words, _)| words);
        let named_subject_domain = named_subject
            .and_then(|(addr, _, _)| columns[addr.segment_ord as usize].domain(addr.doc_id));
        let named_subject_reason = named_subject.map(|(_, _, reason)| reason.to_string());
        let subject_prefix = (named_subject_words > 0).then(|| {
            tokens
                .iter()
                .take(named_subject_words)
                .collect::<HashSet<_>>()
                .len()
        });

        let max_bm25 = candidates.iter().map(|&(bm25, _)| bm25).fold(0.0, f32::max);
        let mut ranked: Vec<Ranked> = Vec::with_capacity(candidates.len());
        let language = self.language_for(options, query_text);
        for (bm25, addr) in candidates {
            let column = &columns[addr.segment_ord as usize];
            if options.safe.hides(column.adult(addr.doc_id)) {
                continue;
            }
            let country = column.country(addr.doc_id);
            let country_bonus = match (&home, &country) {
                (Some(home), Some(country)) if home == country => country_boost,
                (Some(_), Some(_)) if options.only_country => continue,
                (Some(_), Some(_)) => -country_boost,
                _ => 0.0,
            };
            let link_score = link_score_of(addr);
            let is_kind = kinds.contains(&addr);
            let name = names.get(&addr).copied().unwrap_or_default();
            let lexical = &evidence[&addr];
            // A full-name rival can disable semantic score blending, but
            // cannot erase available whole-query evidence for this site.
            let semantic_evidence = closeness_of(addr).map(|score| unit_or(score, 0.0));
            // The complete hostname written as words retains address
            // lookup, including its suffix ("americanairlines fr").
            let full_hostname = column
                .domain(addr.doc_id)
                .is_some_and(|domain| analysis::tokens(&self.words, &domain) == tokens);
            let full_name = name.typed || full_hostname || name.words() >= query.len;
            let explicit_navigation = navigation_names.contains(&addr);
            // A full domain spelling cannot override a resolved subject.
            // Without one, retain exact-name lookup. Typed addresses and
            // independently indexed full names keep their controls.
            let protected = full_name && subject_prefix.is_none()
                || name.typed
                || full_hostname
                || name.alias >= query.len
                || name.linked >= query.len
                || independent_labels
                    .get(&addr)
                    .is_some_and(|&words| words >= query.len)
                || is_kind
                || explicit_navigation;
            let subject_coverage = subject_prefix.and_then(|prefix| {
                if name
                    .alias
                    .max(name.linked)
                    .max(independent_labels.get(&addr).copied().unwrap_or(0))
                    >= named_subject_words
                {
                    Some(1.0)
                } else {
                    lexical.subject(prefix)
                }
            });
            let subject_remaining = subject_prefix.and_then(|prefix| lexical.task_coverage(prefix));
            let subject_share = if protected {
                1.0
            } else {
                subject_coverage.unwrap_or(1.0)
            };
            let subject_supported = subject_share >= 1.0 - f32::EPSILON;
            // Relative nearest-result scores can be high for a task-only
            // provider. They cannot establish a missing named subject.
            // Missing indexed task words remain unknown: semantic support
            // for the whole query may still support ordering, not verification.
            let qualified_semantic = semantic_evidence.map(|s| s * subject_share);
            let semantic = meaning.and(qualified_semantic);
            // Without a resolved subject, retain descriptive semantic recall
            // and its existing stricter check for literal domain collisions.
            let domain_collision = domain_collision_of(addr);
            let convincing = protected
                || subject_supported
                    && (lexical.substantive >= 0.75
                        && subject_remaining.is_none_or(|remaining| remaining > 0.0)
                        || qualified_semantic.is_some_and(|s| {
                            if subject_coverage.is_some() {
                                // With no indexed task words, retain the
                                // existing stronger calibration for literal
                                // domain collisions. Other subject-supported
                                // results keep descriptive semantic recall.
                                let floor = if domain_collision && subject_remaining == Some(0.0) {
                                    cfg.partial_closeness.unwrap_or(0.5).max(semantic_floor)
                                } else {
                                    semantic_floor
                                };
                                s >= floor
                            } else {
                                s >= cfg.partial_closeness.unwrap_or(0.5).max(0.5)
                                    || !domain_collision && s >= semantic_floor.max(f32::EPSILON)
                            }
                        }));
            let whole_share = if !cfg.whole_query_relevance || convincing {
                1.0
            } else {
                // Retain weak lexical/semantic results as fallbacks; a
                // missing vector does not remove their lexical evidence.
                lexical.coverage.max(qualified_semantic.unwrap_or(0.0)) * subject_share
            };
            let remainder =
                lexical.remaining(prefix_distinct.get(&name.words()).copied().unwrap_or(0));
            let name_share = if !cfg.whole_query_relevance || convincing {
                1.0
            } else {
                remainder.max(qualified_semantic.unwrap_or(0.0)) * subject_share
            };
            let relevance_tier = u8::from(cfg.whole_query_relevance && convincing);
            // A site whose domain the whole query names stays whatever its
            // language: "spiegel" finds spiegel.de with English chosen. So
            // does a well-known site the query's first two words or more
            // name: "real madrid tickets" keeps realmadrid.com, "deutsche
            // bahn tickets" bahn.de. One word is too often a common one
            // ("free games" is not free.fr's).
            let known_name = name.typed
                || name.label >= query.len
                || name.words() >= 2 && link_score >= WELL_KNOWN_LINK_SCORE;
            if let (Some(wanted), Some(site)) = (&language, column.language(addr.doc_id)) {
                if !language_fits(wanted, &site) && !known_name {
                    continue;
                }
            }
            let mut placing_text_score = None;
            let text_score = if is_kind || name.label >= query.len || name.linked >= query.len {
                // Being what the query names, or being named by all of it,
                // is a full match, however little of the site's own text
                // says so: among sites the query names in full, popularity
                // decides, so aa.com wins "american airlines" over
                // americanairlines.com. What most links to a site call it
                // names it as well as its label does.
                1.0
            } else {
                let words = if max_bm25 > 0.0 {
                    (bm25 / max_bm25).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let closeness = semantic.or_else(|| meaning.map(|_| lexical.words * words));
                if let Some(Some(plain)) = plain_closeness_of(addr) {
                    placing_text_score = Some(
                        match plain
                            .map(|s| s * subject_share)
                            .or_else(|| meaning.map(|_| lexical.words * words * subject_share))
                        {
                            Some(plain) => {
                                (1.0 - meaning_weight) * words
                                    + meaning_weight * plain.clamp(0.0, 1.0)
                            }
                            None => words,
                        } * subject_share,
                    );
                }
                match closeness {
                    Some(closeness) => {
                        (1.0 - meaning_weight) * words + meaning_weight * closeness.clamp(0.0, 1.0)
                    }
                    None => words,
                }
            } * whole_share;
            let closeness = if is_kind || name.label >= query.len {
                None
            } else {
                let words = if max_bm25 > 0.0 {
                    (bm25 / max_bm25).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                semantic.or_else(|| meaning.map(|_| lexical.words * words))
            };
            let label_bonus = if name.label >= query.len {
                cfg.exact_label_bonus
            } else {
                partial_label_bonus
            };
            let mut name_bonus = (label_bonus * name.label as f32 / query_words)
                .max(cfg.exact_alias_bonus * name.alias as f32 / query_words)
                .max(
                    link_name_bonus(cfg, name.linked, query.len) * name.linked as f32 / query_words,
                );
            if is_kind {
                name_bonus = name_bonus.max(cfg.kind_bonus);
            }
            name_bonus *= name_share;
            let trust = if name.typed || trusted_link_score <= 0.0 {
                1.0
            } else if bare.contains(&addr) {
                untrusted_share
            } else {
                let evidence = (link_score / trusted_link_score).min(1.0);
                untrusted_share + (1.0 - untrusted_share) * evidence
            };
            // Within a segment, term ordinals sort like the domains themselves.
            let domain_ord = column
                .domains
                .as_ref()
                .and_then(|domains| domains.term_ords(addr.doc_id).next())
                .unwrap_or(u64::MAX);
            let unnamed = !is_kind
                && if cfg.whole_query_relevance {
                    !protected
                } else {
                    name.words() == 0
                };
            let prior = match (relevance_floor, cfg.meaning_only_relevance) {
                // No word of the query: only meaning speaks for it.
                (_, Some(floor)) if unnamed && bm25 <= 0.0 && floor > 0.0 => {
                    link_score * (text_score / floor).min(1.0)
                }
                (Some(floor), _) if unnamed => link_score * (text_score / floor).min(1.0),
                _ => link_score,
            } * whole_share;
            let mut query_evidence = CandidateEvidence {
                domain: column.domain(addr.doc_id).unwrap_or_default(),
                lexical_score: bm25,
                semantic_closeness: semantic_evidence,
                query_coverage: lexical.coverage,
                substantive_coverage: lexical.substantive,
                remaining_coverage: remainder,
                named_subject_words,
                named_subject_domain: named_subject_domain.clone(),
                named_subject_reason: named_subject_reason.clone(),
                subject_coverage,
                subject_remaining_coverage: subject_remaining,
                full_name,
                typed_domain: name.typed,
                partial_name_words: if full_name { 0 } else { name.words() },
                relevance_tier,
                source_quality: None,
            };
            if let Some(pool) = pool.as_deref_mut() {
                let doc: TantivyDocument = searcher.doc(addr)?;
                let text = |field| doc.get_first(field).and_then(|value| value.as_str());
                query_evidence.source_quality = health::source_quality(
                    &query_evidence.domain,
                    text(self.fields.title),
                    text(self.fields.description),
                    Some(link_score),
                );
                pool.evidence.push(query_evidence.clone());
            }
            ranked.push(Ranked {
                addr,
                score: alpha * prior
                    + trust * ((1.0 - alpha) * text_score + name_bonus)
                    + country_bonus,
                text_score,
                placing_text_score,
                link_score,
                country,
                named: name.typed || name.words() >= query.len,
                label_names_query: query.len >= 2 && name.label >= query.len,
                closeness,
                relevance_tier,
                query_evidence: Some(query_evidence),
                tie_break: (addr.segment_ord, domain_ord),
            });
        }
        ranked.sort_by(|a, b| {
            b.relevance_tier
                .cmp(&a.relevance_tier)
                .then_with(|| b.score.total_cmp(&a.score))
                .then_with(|| b.link_score.total_cmp(&a.link_score))
                .then_with(|| a.tie_break.cmp(&b.tie_break))
        });
        // Keep the score-order contract used by downstream blending and
        // network results. A weak fallback cannot regain its old place
        // merely because another caller sorts the displayed scores.
        if let Some(first_weak) = ranked
            .iter()
            .position(|row| row.relevance_tier == 0)
            .filter(|&at| at > 0)
        {
            let boundary = ranked[first_weak - 1].score;
            let ceiling = boundary - f32::EPSILON * boundary.abs().max(1.0);
            for row in &mut ranked[first_weak..] {
                row.score = row.score.min(ceiling);
            }
        }

        // Matching a word or two of a longer query, found by no name and
        // not near it in meaning: no answer, however big the site
        // (victoriassecret.com for "secret of mana walkthrough", hbo.com
        // for "paschen series").
        if let Some((floor, any)) = cfg
            .partial_closeness
            .filter(|floor| *floor > 0.0)
            .zip(any_meaning)
        {
            let convincing_alternative = ranked.iter().any(|row| row.relevance_tier > 0);
            let (named_inside, alias_inside) = self.names_inside(&searcher, &query, query_text)?;
            let mut partial = Vec::new();
            for r in &ranked {
                if r.named || kinds.contains(&r.addr) || r.relevance_tier > 0 {
                    continue;
                }
                // A name only counts for a site known by it: hilton.com in
                // "back bay hilton", not valley.com in "elephant valley". An
                // official site is known by its Wikidata name, not by its
                // domain label (valley.com is Valley National Bank's).
                // Nor for one that has no other main word of the query and
                // is not among the sites nearest it in meaning, unless the
                // query starts with its name: that is a namesake of one
                // word (x.com for "derivative of sin x", web.com for "visit
                // japan web"). delta.com has "flights" for "flights to paris
                // delta", and target.com is near "coffee maker target". An
                // address typed from "www" starts with its name too
                // (netflix.com for "www.netflix").
                let namesake = evidence[&r.addr].main_words <= 1
                    && query.words.first().is_none_or(|word| word != "www")
                    && r.query_evidence.as_ref().is_some_and(|e| {
                        e.partial_name_words == 0 && e.semantic_closeness.is_some_and(|c| c <= 0.0)
                    });
                if named_inside.contains(&r.addr)
                    && !namesake
                    && (r.link_score >= WELL_KNOWN_LINK_SCORE
                        || (alias_inside.contains(&r.addr)
                            && self.is_official(&searcher, r.addr)?))
                {
                    continue;
                }
                let closeness = match meaning {
                    Some(_) => r.closeness,
                    None => columns[r.addr.segment_ord as usize]
                        .domain(r.addr.doc_id)
                        .and_then(|domain| any.closeness(&domain)),
                };
                // Indexed semantic support at the existing descriptive
                // floor is enough to retain a noncollision paraphrase,
                // even when sparse text shares some generic query words.
                // This changes retention, not its score or relevance tier.
                let described = !domain_collision_of(r.addr)
                    && r.query_evidence
                        .as_ref()
                        .is_some_and(|e| e.semantic_closeness.is_some())
                    && cfg.meaning_only_relevance.is_some_and(|minimum| {
                        minimum > 0.0 && closeness.is_some_and(|s| s >= minimum)
                    });
                if described {
                    continue;
                }
                // A purely semantic paraphrase can remain as a weak
                // fallback with real similarity. Nearest-list membership
                // itself never exempts a partial lexical/name match.
                let semantic_fallback = cfg.whole_query_relevance
                    && !convincing_alternative
                    && evidence[&r.addr].coverage <= 0.0
                    && closeness.is_some_and(|s| s >= floor * 0.5);
                if !semantic_fallback && closeness.is_none_or(|closeness| closeness < floor) {
                    partial.push(r.addr);
                }
            }
            let missing = query.missing_main_words(&searcher, &self.fields, &partial)?;
            ranked.retain(|r| !missing.contains(&r.addr));
        }

        // A number shared with a query of other words is no match, however
        // near in meaning: the year of "glastonbury 2026" says which one,
        // not what (a rugby championship's or a lottery's "2026"). A site
        // the query starts by naming keeps its place (163.com).
        let unnamed: Vec<DocAddress> = ranked
            .iter()
            .filter(|r| !r.named && names.get(&r.addr).is_none_or(|name| name.words() == 0))
            .map(|r| r.addr)
            .collect();
        let numbers_only = query.only_numbers(&searcher, &self.fields, &unnamed)?;
        ranked.retain(|r| !numbers_only.contains(&r.addr));

        // Far below a site the query names: filler.
        if let (Some(share), Some(top)) = (cfg.named_share, ranked.first()) {
            // Only a site people know by that name: vacationrentals.com
            // for "vacation rentals" is a namesake of the words, and
            // airbnb.com is still what the query is after.
            if top.named && top.link_score >= WELL_KNOWN_LINK_SCORE && share > 0.0 {
                let least = top.score * share;
                ranked.retain(|r| r.named || r.score >= least);
            }
        }

        // Named in full by a site known by that name: what has only some
        // of its words is filler, unless the query names it by its first
        // words and it is known by that name too (youtube.com for "youtube
        // music").
        if cfg.named_needs_all_words && query.words.len() >= 2 {
            let mut known_name = false;
            for &addr in &full_names {
                if self.known_by_name(&searcher, addr)? {
                    known_name = true;
                    break;
                }
            }
            if known_name {
                let mut partial = Vec::new();
                for r in &ranked {
                    if r.named || kinds.contains(&r.addr) {
                        continue;
                    }
                    let first_words = names.get(&r.addr).is_some_and(|name| name.words() > 0);
                    if first_words
                        && (r.link_score >= WELL_KNOWN_LINK_SCORE
                            || self.is_official(&searcher, r.addr)?)
                    {
                        continue;
                    }
                    partial.push(r.addr);
                }
                let coverage = query.coverage(&searcher, &self.fields, &partial)?;
                ranked.retain(|r| coverage.get(&r.addr).is_none_or(|&share| share > 0.999));
            }
        }

        // Copies of the site the query names on other top-level domains
        // (gmail.ru, gmail.hu for "gmail"; paypal.biz for "paypal"): never
        // crawled, nothing says what they are, little linked. They are
        // left out: a bare facebook.co under facebook.com only looks like
        // a trap. Typed, they are still found. Country sites of the brand
        // are kept where they rank: they are well known (amazon.de) or
        // crawled (toyota.jp).
        if let Some(top) = ranked
            .first()
            .filter(|top| top.named && top.link_score >= WELL_KNOWN_LINK_SCORE)
        {
            let top_label = columns[top.addr.segment_ord as usize]
                .domain(top.addr.doc_id)
                .and_then(|domain| domain.split('.').next().map(str::to_owned));
            if let Some(top_label) = top_label {
                let mut copies = HashSet::new();
                for r in ranked.iter().skip(1) {
                    if r.link_score >= WELL_KNOWN_LINK_SCORE {
                        continue;
                    }
                    let same_label = columns[r.addr.segment_ord as usize]
                        .domain(r.addr.doc_id)
                        .is_some_and(|domain| domain.split('.').next() == Some(&top_label));
                    if same_label && !self.says_what_it_is(&searcher, r.addr)? {
                        copies.insert(r.addr);
                    }
                }
                ranked.retain(|r| !copies.contains(&r.addr));
            }
        }

        // The first result, when the query's first words name it, with
        // words left over to search it for. A site named lower down is not
        // what the query is about: capital.edu for "capital one my
        // account". Nor is a little-known site named by a common word
        // (get.gov for "get rid of fruit flies", yoga.com for "yoga studio
        // near seattle").
        let named_site = ranked.first().and_then(|r| {
            let words = names.get(&r.addr)?.words();
            (words > 0 && words < query.len).then_some((r, words))
        });
        let site_search = match named_site {
            Some((r, words))
                if r.link_score >= WELL_KNOWN_LINK_SCORE
                    || !self.names_by_common_word(&searcher, query_text, words)? =>
            {
                self.site_search(&searcher, r.addr, query_text, words)?
            }
            _ => None,
        };

        // Left out unless the query names them: sites on names reserved
        // for tests (uniteddolls.test), online pill shops named after a
        // drug ("ibuprofen dosage" found celebrex365o24.com), and little
        // known homepages that are only a company's sign-in page for
        // someone else's product ("Outlook Web App" on bpl.net for
        // "microsoft outlook"). live.com, titled "Outlook", is Microsoft's.
        let ranked = self.untitled_last(&searcher, ranked, query_text, limit)?;
        // A few more than asked for, so that copies left out below leave
        // the page full.
        let wanted = limit.saturating_add(COPY_LOOKAHEAD);
        let unnamed: Vec<DocAddress> = ranked
            .iter()
            .take(wanted.saturating_mul(2))
            .filter(|r| !r.named)
            .map(|r| r.addr)
            .collect();
        let missing = query.missing_main_words(&searcher, &self.fields, &unnamed)?;
        let mut hits: Vec<Hit> = Vec::with_capacity(wanted.min(ranked.len()));
        let mut fingerprints: Vec<Option<u64>> = Vec::with_capacity(hits.capacity());
        for ranked in ranked {
            if hits.len() == wanted {
                break;
            }
            let named = ranked.named;
            let missing_words = missing.contains(&ranked.addr);
            let (mut hit, fingerprint) = self.hit(&searcher, ranked)?;
            hit.missing_words = missing_words;
            let left_out = is_reserved_name(&hit.domain)
                || health::source_quality(
                    &hit.domain,
                    hit.title.as_deref(),
                    hit.description.as_deref(),
                    Some(hit.link_score),
                )
                .is_some_and(|quality| quality.informational_penalty())
                || (hit.link_score < WELL_KNOWN_LINK_SCORE
                    && hit
                        .title
                        .as_deref()
                        .is_some_and(|title| plumb_core::is_sign_in_portal(&hit.domain, title)));
            if named || !left_out {
                hits.push(hit);
                fingerprints.push(fingerprint);
            }
        }
        let typed = registrable_domain(query_text.trim());
        let hits = without_near_copies(hits, &fingerprints, typed.as_deref(), home.as_deref());
        let mut hits = without_copies(hits, typed.as_deref(), home.as_deref());
        hits.truncate(limit);
        let results = SearchResults {
            hits,
            pages: Vec::new(),
            site_search,
            spelling: None,
        };
        Ok((results, named))
    }
}

/// The best link score among `docs`, 0 for none.
pub(crate) fn best_link_score(searcher: &tantivy::Searcher, docs: &HashSet<DocAddress>) -> f32 {
    link_scores(searcher, docs)
        .into_iter()
        .map(|(score, _)| score)
        .fold(0.0, f32::max)
}

/// The link score of each of `docs` that has one.
fn link_scores(searcher: &tantivy::Searcher, docs: &HashSet<DocAddress>) -> Vec<(f32, DocAddress)> {
    let mut columns: HashMap<u32, Option<tantivy::columnar::Column<f64>>> = HashMap::new();
    let mut scores = Vec::with_capacity(docs.len());
    for &addr in docs {
        let column = columns.entry(addr.segment_ord).or_insert_with(|| {
            searcher
                .segment_reader(addr.segment_ord)
                .fast_fields()
                .f64(schema::LINK_SCORE)
                .ok()
        });
        if let Some(score) = column.as_ref().and_then(|c| c.first(addr.doc_id)) {
            scores.push((score as f32, addr));
        }
    }
    scores
}

/// The domain of the site at `addr`.
pub(crate) fn domain_of(searcher: &tantivy::Searcher, addr: DocAddress) -> Option<String> {
    let column = searcher
        .segment_reader(addr.segment_ord)
        .fast_fields()
        .str(schema::DOMAIN)
        .ok()??;
    let ord = column.term_ords(addr.doc_id).next()?;
    let mut domain = String::new();
    column.ord_to_str(ord, &mut domain).ok()?;
    Some(domain)
}

/// A candidate with its blended score.
pub(crate) struct Ranked {
    pub(crate) addr: DocAddress,
    pub(crate) score: f32,
    pub(crate) text_score: f32,
    pub(crate) placing_text_score: Option<f32>,
    pub(crate) link_score: f32,
    pub(crate) country: Option<String>,
    pub(crate) named: bool,
    /// The domain's label is the whole query of two or more words
    /// (awesome-python.com for "awesome python").
    pub(crate) label_names_query: bool,
    /// How close in meaning the site is to the query, or as its words
    /// match for a site with no embedding; `None` without a [`Meaning`] or
    /// for a site the query names in full.
    pub(crate) closeness: Option<f32>,
    pub(crate) relevance_tier: u8,
    pub(crate) query_evidence: Option<CandidateEvidence>,
    pub(crate) tie_break: (u32, u64),
}

/// The fast columns of one segment.
pub(crate) struct Columns {
    pub(crate) link_scores: tantivy::columnar::Column<f64>,
    pub(crate) domains: Option<tantivy::columnar::StrColumn>,
    pub(crate) countries: Option<tantivy::columnar::StrColumn>,
    pub(crate) languages: Option<tantivy::columnar::StrColumn>,
    pub(crate) adult: tantivy::columnar::Column<u64>,
}

impl Columns {
    fn domain(&self, doc: tantivy::DocId) -> Option<String> {
        let domains = self.domains.as_ref()?;
        let ord = domains.term_ords(doc).next()?;
        let mut domain = String::new();
        domains.ord_to_str(ord, &mut domain).ok()?;
        Some(domain)
    }

    pub(crate) fn country(&self, doc: tantivy::DocId) -> Option<String> {
        let countries = self.countries.as_ref()?;
        let ord = countries.term_ords(doc).next()?;
        let mut country = String::new();
        countries.ord_to_str(ord, &mut country).ok()?;
        (!country.is_empty()).then_some(country)
    }

    pub(crate) fn language(&self, doc: tantivy::DocId) -> Option<String> {
        let languages = self.languages.as_ref()?;
        let ord = languages.term_ords(doc).next()?;
        let mut language = String::new();
        languages.ord_to_str(ord, &mut language).ok()?;
        (!language.is_empty()).then_some(language)
    }

    pub(crate) fn adult(&self, doc: tantivy::DocId) -> AdultLevel {
        schema::adult_from(self.adult.first(doc).unwrap_or(0))
    }
}

/// `value` clamped to `0..=1`, or `default` when it is not a number.
pub(crate) fn unit_or(value: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        default
    }
}

/// The BM25 score of each of `docs` for `query`, 0 where it does not match.
pub(crate) fn bm25_of(
    searcher: &tantivy::Searcher,
    query: &dyn Query,
    mut docs: Vec<DocAddress>,
) -> Result<Vec<(f32, DocAddress)>> {
    if docs.is_empty() {
        return Ok(Vec::new());
    }
    docs.sort_unstable();
    let weight = query.weight(EnableScoring::enabled_from_searcher(searcher))?;
    let mut scored = Vec::with_capacity(docs.len());
    let mut scorer: Option<(u32, Box<dyn Scorer>)> = None;
    for addr in docs {
        let scorer = match &mut scorer {
            Some((segment_ord, scorer)) if *segment_ord == addr.segment_ord => scorer,
            _ => {
                let reader = searcher.segment_reader(addr.segment_ord);
                let segment_scorer = weight.scorer(reader, 1.0)?;
                &mut scorer.insert((addr.segment_ord, segment_scorer)).1
            }
        };
        // Documents come in order, so the scorer only moves forward.
        if scorer.doc() < addr.doc_id {
            scorer.seek(addr.doc_id);
        }
        let bm25 = if scorer.doc() == addr.doc_id {
            scorer.score()
        } else {
            0.0
        };
        scored.push((bm25, addr));
    }
    Ok(scored)
}

/// The bonus of a site a link text covering `linked` of the query's `len`
/// words names: [`RankConfig::link_name_bonus`] for the whole query, at
/// most the label bonus for its first words, so a longer name still wins
/// ("chase center tickets": chasecenter.com before chase.com).
pub(crate) fn link_name_bonus(cfg: &RankConfig, linked: usize, len: usize) -> f32 {
    if linked >= len {
        cfg.link_name_bonus
    } else {
        cfg.link_name_bonus.min(cfg.exact_label_bonus)
    }
}
