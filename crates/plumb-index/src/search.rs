//! Opening the index and searching it: the entry points ([`Searcher::search`]
//! and on), the typo check around a search and the site searches it
//! hands off to.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::{
    is_function_word, normalize_text, registrable_domain, search_link, search_template_for,
    Operators,
};
use tantivy::collector::TopDocs;
use tantivy::query::TermQuery;
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{DocAddress, Index, ReloadPolicy, TantivyDocument, Term};

use crate::hits::{docs_site_first, hit_texts, suggested_site_second};
use crate::names::{named_by_links, without_intent_words};
use crate::query::{matching_docs, words_after, ParsedQuery, COMPARISON_WORDS};
use crate::rank::{best_link_score, bm25_of, domain_of, link_scores};
use crate::schema::Fields;
use crate::{
    analysis, health, pages, schema, spell, spell_model, CandidatePool, Hit, Meaning, RankConfig,
    SearchOptions, SearchResults, Searcher, SiteSearch, Spelling, WordsOrder, FILLER_WORDS,
    WELL_KNOWN_LINK_SCORE,
};

/// A query with operators ranks this many times its limit, and at least
/// [`OPERATOR_CANDIDATES`], before they narrow the hits.
const OPERATOR_WIDENING: usize = 5;

const OPERATOR_CANDIDATES: usize = 200;

/// How much more link score a well-known site whose name is a typo away
/// must have than what a query finds as typed for the query to be taken as
/// that typo: twitter.com over twiter.com.
pub const TYPO_POPULARITY_MARGIN: f32 = 0.3;

/// A site the whole query names with this link score or more keeps its
/// name even when a far better-known one is a typo away. Typo-squatters
/// such as twiter.com score below it (0.25 to 0.37 on real data).
pub const KEEPS_ITS_NAME_LINK_SCORE: f32 = 0.4;

/// Most words of a site's name at the end of a query
/// ([`Searcher::site_search_at_end`]): "new york times".
const MAX_END_NAME_WORDS: usize = 3;

impl Searcher {
    /// Opens the index in `dir`. Fails when there is none, or when it was
    /// built with a different schema (rebuild it then).
    pub fn open(dir: &Path) -> Result<Self> {
        let index = Index::open_in_dir(dir)
            .with_context(|| format!("opening search index in {}", dir.display()))?;
        analysis::register_sites(index.tokenizers());
        if !schema::readable(&index.schema()) {
            bail!(
                "the index in {} was built by another version of plumb-index; rebuild it",
                dir.display()
            );
        }
        let fields = Fields::new(&index.schema())?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .with_context(|| format!("reading search index in {}", dir.display()))?;
        Ok(Searcher {
            reader,
            fields,
            words: analysis::words_analyzer(),
            joined: analysis::joined_analyzer(),
            spelling: spell_model::Model::open(dir),
        })
    }

    /// The spelling model learned from the index's words, if the index
    /// has one ([`spell_model`]).
    pub fn spelling_model(&self) -> Option<&spell_model::Model> {
        self.spelling.as_ref()
    }

    /// In how many sites `word` (a normalized word) is found, counting a
    /// site once for each of the fields typo correction reads: how known a
    /// word is to it.
    pub fn word_sites(&self, word: &str) -> u64 {
        let searcher = self.reader.searcher();
        spell::word_fields(&self.fields)
            .into_iter()
            .map(|field| {
                searcher
                    .doc_freq(&Term::from_field_text(field, word))
                    .unwrap_or(0)
            })
            .sum()
    }

    /// Whether many sites' titles have `word` ([`KNOWN_WORD_SITES`] or
    /// more): "first" and "health" do, a name such as "medscape" does not.
    pub fn is_common_word(&self, word: &str) -> bool {
        self.names_by_common_word(&self.reader.searcher(), word, 1)
            .unwrap_or(true)
    }

    /// Number of documents (sites) in the index.
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Whether the index holds `domain`, a canonical domain.
    pub fn has_domain(&self, domain: &str) -> bool {
        let term = Term::from_field_text(self.fields.domain, domain);
        self.reader.searcher().doc_freq(&term).is_ok_and(|n| n > 0)
    }

    /// The language of `domain`'s homepage as the index keeps it
    /// ([`plumb_core::site_language`]), when the index holds the site and
    /// knows it.
    pub fn language(&self, domain: &str) -> Option<String> {
        let searcher = self.reader.searcher();
        let term = Term::from_field_text(self.fields.domain, domain);
        let found = searcher
            .search(
                &TermQuery::new(term, IndexRecordOption::Basic),
                &TopDocs::with_limit(1).order_by_score(),
            )
            .ok()?;
        let document: TantivyDocument = searcher.doc(found.first()?.1).ok()?;
        let language = document.get_first(self.fields.language)?.as_str()?;
        (!language.is_empty()).then(|| language.to_string())
    }

    /// The site `domain`, a canonical domain, as a result no query found:
    /// no score, no text match, not named.
    pub fn site(&self, domain: &str) -> Result<Option<Hit>> {
        if !self.has_domain(domain) {
            return Ok(None);
        }
        let options = SearchOptions {
            exact: true,
            ..SearchOptions::default()
        };
        let found = self.search_full(domain, 1, &RankConfig::default(), &options)?;
        Ok(found
            .hits
            .into_iter()
            .find(|hit| hit.domain == domain)
            .map(|hit| Hit {
                score: 0.0,
                text_score: 0.0,
                placing_text_score: None,
                named: false,
                query_evidence: None,
                ..hit
            }))
    }

    /// [`Searcher::search_with`] using [`RankConfig::default`].
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.search_with(query, limit, &RankConfig::default())
    }

    /// Best `limit` hits for `query`, best first, with no home country. See
    /// [`Searcher::search_full`].
    pub fn search_with(&self, query: &str, limit: usize, cfg: &RankConfig) -> Result<Vec<Hit>> {
        Ok(self
            .search_full(query, limit, cfg, &SearchOptions::default())?
            .hits)
    }

    /// Best `limit` hits for `query`, best first, and a link into a named
    /// site's own search when the query goes on after the site's name. A
    /// query with no letters or digits finds nothing.
    ///
    /// The top `max(cfg.candidates, limit)` documents by BM25, plus every
    /// site whose name the query starts with and every site of the kind
    /// the query names, are re-ranked by the blended score (see the
    /// [crate docs](crate)); ties go to the higher link score, then to the
    /// alphabetically first domain.
    pub fn search_full(
        &self,
        query_text: &str,
        limit: usize,
        cfg: &RankConfig,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        self.search_meaning(query_text, limit, cfg, options, None)
    }

    /// [`Searcher::search_full`], also ranking by `meaning` when no site is
    /// named by the whole query and the query names no kind of site: the
    /// sites [`Meaning::nearest`] the query are re-ranked too, and
    /// [`RankConfig::meaning_weight`] of every text match is how close the
    /// site is in meaning. Queries that name a site rank as without it.
    ///
    /// The results are always for the query as typed. Typos get a suggested
    /// correction ([`SearchResults::spelling`], see [`spell`]) unless
    /// [`SearchOptions::exact`] is set; a hostname only the address of a
    /// better-known site it is a slip of ([`Searcher::suggest_domain`]). A query
    /// that names a site in full gets one only when its first words are a
    /// typo away from the name of a well-known site
    /// ([`WELL_KNOWN_LINK_SCORE`]) with [`TYPO_POPULARITY_MARGIN`] more
    /// link score than what the query finds as typed (the sites it names
    /// in full, if below [`KEEPS_ITS_NAME_LINK_SCORE`], else its best hit):
    /// "twiter" lists the typo-squatter twiter.com and asks "Did you mean
    /// twitter?". Any other query gets one when the correction finds
    /// something.
    ///
    /// Search operators ([`Operators`]) narrow the results: `site:`
    /// keeps the sites on that host and lists the site itself after them,
    /// with a link into its own search; `"quotes"` keep the sites whose
    /// name, title or description has the words in that order; `-word`
    /// leaves out the sites that have the word. Queries with operators are
    /// not corrected for typos.
    pub fn search_meaning(
        &self,
        query_text: &str,
        limit: usize,
        cfg: &RankConfig,
        options: &SearchOptions,
        meaning: Option<&dyn Meaning>,
    ) -> Result<SearchResults> {
        let ops = Operators::parse(query_text);
        if !ops.any() {
            let mut results = self.search_words(query_text, limit, cfg, options, meaning)?;
            if cfg.health_authorities {
                health::authorities_first(
                    query_text,
                    options.country.as_deref(),
                    &mut results.hits,
                    limit,
                    &|domain| self.site(domain).ok().flatten(),
                    &|word| self.is_common_word(word),
                );
            }
            return Ok(results);
        }
        let mut results = SearchResults::default();
        if limit == 0 {
            return Ok(results);
        }
        let options = SearchOptions {
            exact: true,
            ..options.clone()
        };
        if !ops.words.is_empty() {
            let wider = limit
                .saturating_mul(OPERATOR_WIDENING)
                .max(OPERATOR_CANDIDATES);
            let found = self.search_words(&ops.words, wider, cfg, &options, meaning)?;
            results.hits = found
                .hits
                .into_iter()
                .filter(|hit| ops.allows(&hit.domain, hit_texts(hit)))
                .collect();
            results.site_search = found
                .site_search
                .filter(|link| ops.allows_host(&link.domain));
        }
        // The sites `site:` names come after what matched in them, even
        // when their homepage does not have the words: the link into their
        // own search finds the rest.
        let fallback = Operators {
            phrases: Vec::new(),
            ..ops.clone()
        };
        for site in &ops.sites {
            let Some(domain) = registrable_domain(site) else {
                continue;
            };
            if !results.hits.iter().any(|hit| hit.domain == domain) {
                let found = self.search_words(&domain, 1, cfg, &options, None)?;
                results.hits.extend(found.hits.into_iter().filter(|hit| {
                    hit.domain == domain && fallback.allows(&hit.domain, hit_texts(hit))
                }));
            }
            if results.site_search.is_none() {
                results.site_search = self.site_search_of(&domain, &ops.site_terms)?;
            }
        }
        results.hits.truncate(limit);
        Ok(results)
    }

    /// [`Searcher::search_meaning`] for a query without operators.
    fn search_words(
        &self,
        query_text: &str,
        limit: usize,
        cfg: &RankConfig,
        options: &SearchOptions,
        meaning: Option<&dyn Meaning>,
    ) -> Result<SearchResults> {
        let (mut results, named) = self.rank(query_text, limit, cfg, options, meaning)?;
        if !named.typed {
            self.add_news_sites(query_text, &mut results, limit, cfg, options)?;
        }
        if options.exact || limit == 0 {
            return Ok(results);
        }
        if named.typed {
            return self.suggest_domain(query_text, results, limit, cfg, options, meaning);
        }
        // "paypal login" is also ranked as "paypal": a well-known site that
        // name alone names keeps the better of its two scores, so
        // paypal.com goes above paypal-login.us. Lesser sites the name
        // names keep the whole query's score: postgres.ai is not what
        // "postgres docs" is after. A well-known site named by all of it
        // keeps the query to itself (readthedocs.org for "read the docs").
        // The link into the named site's own search still uses every word.
        if let Some(name) = without_intent_words(query_text) {
            let named_in_full = named
                .full_link_score
                .is_some_and(|score| score >= WELL_KNOWN_LINK_SCORE);
            if !named_in_full {
                let mut found = self.search_words(&name, limit, cfg, options, meaning)?;
                let mut best: HashMap<String, Hit> = HashMap::new();
                let by_name = std::mem::take(&mut found.hits)
                    .into_iter()
                    .filter(|hit| hit.named && hit.link_score >= WELL_KNOWN_LINK_SCORE);
                for hit in results.hits.into_iter().chain(by_name) {
                    match best.entry(hit.domain.clone()) {
                        Entry::Occupied(mut kept) if kept.get().score < hit.score => {
                            kept.insert(hit);
                        }
                        Entry::Occupied(_) => {}
                        Entry::Vacant(slot) => {
                            slot.insert(hit);
                        }
                    }
                }
                let mut hits: Vec<Hit> = best.into_values().collect();
                hits.sort_by(|a, b| {
                    b.score
                        .total_cmp(&a.score)
                        .then_with(|| b.link_score.total_cmp(&a.link_score))
                        .then_with(|| a.domain.cmp(&b.domain))
                });
                docs_site_first(query_text, &name, &mut hits);
                hits.truncate(limit);
                found.hits = hits;
                found.site_search = results.site_search.or(found.site_search);
                // A suggested spelling of the name keeps the words after it:
                // "postgres docs" suggests "postgresql docs".
                if let Some(spelling) = &mut found.spelling {
                    let words = normalize_text(query_text);
                    let after = words
                        .split_whitespace()
                        .skip(name.split_whitespace().count());
                    for word in after {
                        spelling.query.push(' ');
                        spelling.query.push_str(word);
                    }
                }
                return Ok(found);
            }
        }
        // A site named in full with popularity of its own keeps its name;
        // so does a kind of site.
        match named.full_link_score {
            Some(score) if score >= KEEPS_ITS_NAME_LINK_SCORE => return Ok(results),
            None if named.kind => return Ok(results),
            _ => {}
        }
        let searcher = self.reader.searcher();
        let link_score = |docs: &HashSet<DocAddress>| best_link_score(&searcher, docs);
        let named_words = if named.full_link_score.is_some() {
            0
        } else {
            named.words
        };
        let tuning = spell::Tuning {
            channel: cfg.spelling_channel,
            real_word_weight: cfg.real_word_weight,
            context_weight: cfg.context_weight,
        };
        let Some(fix) = spell::correct(
            &searcher,
            &self.fields,
            &self.words,
            self.spelling.as_ref(),
            tuning,
            query_text,
            named_words,
            &link_score,
        )?
        else {
            return Ok(results);
        };
        let typed_link_score = named
            .full_link_score
            .or_else(|| results.hits.first().map(|hit| hit.link_score))
            .unwrap_or(0.0);
        // A site the whole query names is not taken for a typo of a name
        // that only some of its words are a typo of: "linus tech tips" is
        // linustechtips.com, not "linux" with two more words.
        let fixes_the_named_name =
            named.full_link_score.is_none() || fix.name_covers >= named.words;
        let far_more_popular = fixes_the_named_name
            && fix.name_link_score.is_some_and(|score| {
                score >= WELL_KNOWN_LINK_SCORE && score >= typed_link_score + TYPO_POPULARITY_MARGIN
            });
        if named.full_link_score.is_some() && !far_more_popular {
            return Ok(results);
        }
        // The results stay those of the query as typed; the correction is
        // only offered ("Did you mean ...?"), and only when it finds
        // something.
        let (fixed, _) = self.rank(&fix.query, 1, cfg, options, meaning)?;
        if fixed.hits.is_empty() {
            return Ok(results);
        }
        // Nothing has all the words typed, and the spelling finds what
        // does: show its results instead ("kayne west stronger").
        let typed_found_nothing = results.hits.iter().all(|hit| hit.missing_words);
        if typed_found_nothing && fixed.hits.first().is_some_and(|hit| !hit.missing_words) {
            let (fixed, _) = self.rank(&fix.query, limit, cfg, options, meaning)?;
            return Ok(SearchResults {
                spelling: Some(Spelling {
                    query: fix.query,
                    site: None,
                    applied: true,
                }),
                ..fixed
            });
        }
        let site = fixed
            .hits
            .first()
            .filter(|hit| {
                fix.name_link_score.is_some()
                    && hit.named
                    && hit.link_score >= spell::MIN_FIX_LINK_SCORE
            })
            .map(|hit| hit.domain.clone());
        // A misspelled name matches no site's words, so only meaning finds
        // the site it means ("adobee" is close to adobe.com): the results as
        // typed keep what meaning alone finds rather than cutting it as
        // filler.
        let results = match cfg.meaning_only_relevance {
            Some(_) if site.is_some() => {
                let cfg = RankConfig {
                    meaning_only_relevance: None,
                    whole_query_relevance: false,
                    ..*cfg
                };
                self.rank(query_text, limit, &cfg, options, meaning)?.0
            }
            _ => results,
        };
        // The site the correction names is listed second, under the best
        // match as typed: "saples" still lists what has those letters
        // first, and staples.com right under it rather than nowhere.
        let mut results = results;
        if let Some(site) = &site {
            match fixed.hits.into_iter().next() {
                Some(fixed_site) if !results.hits.iter().any(|hit| hit.domain == *site) => {
                    let at = results.hits.len().min(1);
                    results.hits.insert(at, fixed_site);
                    results.hits.truncate(limit);
                }
                _ => suggested_site_second(&mut results.hits, site),
            }
        }
        Ok(SearchResults {
            spelling: Some(Spelling {
                query: fix.query,
                site,
                applied: false,
            }),
            ..results
        })
    }

    /// `results` for `query_text`, a web address, with the address of the
    /// site it is a slip of suggested ([`spell::fix_domain`]) when that
    /// site has [`TYPO_POPULARITY_MARGIN`] more link score than the one
    /// typed, and listed: first when no site has the address typed
    /// ("amzon.com", "wwwmove.mil"), else second, under the look-alike
    /// that has it ("gogle.com"). A site typed with popularity of its own
    /// ([`KEEPS_ITS_NAME_LINK_SCORE`]) keeps its address.
    fn suggest_domain(
        &self,
        query_text: &str,
        mut results: SearchResults,
        limit: usize,
        cfg: &RankConfig,
        options: &SearchOptions,
        meaning: Option<&dyn Meaning>,
    ) -> Result<SearchResults> {
        let Some(typed) = crate::names::typed_domain(query_text) else {
            return Ok(results);
        };
        let searcher = self.reader.searcher();
        let link_score = |docs: &HashSet<DocAddress>| best_link_score(&searcher, docs);
        let typed_docs = matching_docs(
            &searcher,
            vec![Term::from_field_text(self.fields.domain, &typed)],
        )?;
        let typed_link_score = link_score(&typed_docs);
        if typed_link_score >= KEEPS_ITS_NAME_LINK_SCORE {
            return Ok(results);
        }
        let model = self.spelling.as_ref().filter(|_| cfg.spelling_channel);
        let Some((domain, score)) =
            spell::fix_domain(&searcher, &self.fields, model, &typed, &link_score)?
        else {
            return Ok(results);
        };
        if score < typed_link_score + TYPO_POPULARITY_MARGIN {
            return Ok(results);
        }
        let (found, _) = self.rank(&domain, 1, cfg, options, meaning)?;
        let Some(mut site) = found.hits.into_iter().find(|hit| hit.domain == domain) else {
            return Ok(results);
        };
        results.hits.retain(|hit| hit.domain != domain);
        if typed_docs.is_empty() {
            if let Some(first) = results.hits.first() {
                site.score = site.score.max(first.score);
            }
            results.hits.insert(0, site);
        } else {
            results.hits.push(site);
            suggested_site_second(&mut results.hits, &domain);
        }
        results.hits.truncate(limit);
        results.spelling = Some(Spelling {
            query: domain.clone(),
            site: Some(domain),
            applied: false,
        });
        Ok(results)
    }

    /// The sites [`Searcher::search_meaning`] ranks for `query_text` as
    /// typed, before ranking, by where each came from: what `plumb-tools eval
    /// --recall` measures. A site the ranking never looks at cannot be
    /// found, however it is scored.
    pub fn candidate_pool(
        &self,
        query_text: &str,
        cfg: &RankConfig,
        options: &SearchOptions,
        meaning: Option<&dyn Meaning>,
    ) -> Result<CandidatePool> {
        let mut pool = CandidatePool::default();
        self.rank_traced(query_text, 1, cfg, options, meaning, Some(&mut pool))?;
        Ok(pool)
    }

    /// The first `depth` sites by how well their words match `query_text`
    /// (BM25 alone, as [`RankConfig::candidates`] are picked), and which of
    /// `domains` have any of the query's words. `None` for a query with no
    /// words.
    pub fn words_order(
        &self,
        query_text: &str,
        depth: usize,
        cfg: &RankConfig,
        domains: &[String],
    ) -> Result<Option<WordsOrder>> {
        let Some(mut query) = ParsedQuery::new(query_text, &self.words, &self.joined) else {
            return Ok(None);
        };
        query.terms_boost = cfg.terms_boost;
        query.filler_words = cfg.filler_words;
        query.asked =
            cfg.questions_name_nothing && query.len >= 3 && pages::asked_as_question(query_text);
        let searcher = self.reader.searcher();
        let num_docs = usize::try_from(searcher.num_docs()).unwrap_or(usize::MAX);
        if num_docs == 0 {
            return Ok(Some(WordsOrder::default()));
        }
        let text_query = query.text_query(&searcher, &self.fields)?;
        let found = searcher.search(
            &text_query,
            &TopDocs::with_limit(depth.max(1).min(num_docs)).order_by_score(),
        )?;
        let order = found
            .into_iter()
            .filter_map(|(_, addr)| domain_of(&searcher, addr))
            .collect();
        let mut docs = Vec::new();
        for domain in domains {
            let term = Term::from_field_text(self.fields.domain, domain);
            docs.extend(matching_docs(&searcher, vec![term])?);
        }
        let matched = bm25_of(&searcher, &text_query, docs)?
            .into_iter()
            .filter(|&(bm25, _)| bm25 > 0.0)
            .filter_map(|(_, addr)| domain_of(&searcher, addr))
            .collect();
        Ok(Some(WordsOrder { order, matched }))
    }

    /// A link into the search of the site `domain` for `terms`, if the
    /// index has the site and it has a search address.
    fn site_search_of(&self, domain: &str, terms: &str) -> Result<Option<SiteSearch>> {
        let searcher = self.reader.searcher();
        let term = Term::from_field_text(self.fields.domain, domain);
        let Some(addr) = matching_docs(&searcher, vec![term])?.into_iter().next() else {
            return Ok(None);
        };
        self.site_search_link(&searcher, addr, terms.trim().to_string())
    }

    /// A link into the search of the site at `addr` for the words of `query`
    /// after its first `words`, if the site has a search address (its own,
    /// or one Plumb knows for big sites).
    ///
    /// A question has no site to search ("can cats eat chocolate"), and
    /// neither do words that go on from the name as one phrase: "capital
    /// of ohio", "house of the dragon", "airbnb vs vrbo".
    pub(crate) fn site_search(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
        query: &str,
        words: usize,
    ) -> Result<Option<SiteSearch>> {
        if pages::asked_as_question(query) {
            return Ok(None);
        }
        let Some(terms) = words_after(&self.words, query, words) else {
            return Ok(None);
        };
        let goes_on = terms
            .split_whitespace()
            .next()
            .map(normalize_text)
            .is_some_and(|word| {
                is_function_word(&word) || matches!(word.as_str(), "vs" | "versus" | "v")
            });
        if goes_on {
            return Ok(None);
        }
        self.site_search_link(searcher, addr, terms)
    }

    /// A link into the search of a well-known site the query's last words
    /// name, for the words before them: reddit.com for "how to fix a leaky
    /// faucet reddit", target.com for "coffee maker target". Only a big
    /// site whose search Plumb knows, and only after two main words or
    /// more, so the site is where to look and not what is asked about
    /// ("how to cancel netflix"). Words that run into the name as one
    /// phrase name nothing: "the cricket and the ant", "nike vs adidas".
    pub(crate) fn site_search_at_end(
        &self,
        searcher: &tantivy::Searcher,
        query: &str,
        cfg: &RankConfig,
    ) -> Result<Option<SiteSearch>> {
        let typed: Vec<&str> = query.split_whitespace().collect();
        let tokens = |words: &[&str]| -> Vec<String> {
            words
                .iter()
                .flat_map(|word| analysis::tokens(&self.words, word))
                .collect()
        };
        let longest = MAX_END_NAME_WORDS.min(typed.len().saturating_sub(1));
        for words in (1..=longest).rev() {
            let (before, name) = typed.split_at(typed.len() - words);
            let last = before.last().map(|word| normalize_text(word));
            if last.is_none_or(|word| {
                is_function_word(&word) || COMPARISON_WORDS.contains(&word.as_str())
            }) {
                continue;
            }
            let main = tokens(before)
                .iter()
                .map(String::as_str)
                .filter(|token| !is_function_word(token) && !FILLER_WORDS.contains(token))
                .count();
            let key = tokens(name).concat();
            if main < 2 || key.is_empty() {
                continue;
            }
            let mut named = matching_docs(
                searcher,
                vec![
                    Term::from_field_text(self.fields.label_key, &key),
                    Term::from_field_text(self.fields.alias_key, &key),
                ],
            )?;
            if let Some(field) = self.fields.link_name_key.filter(|_| cfg.link_names) {
                named.extend(named_by_links(
                    searcher,
                    &Term::from_field_text(field, &key),
                )?);
            }
            for (link_score, addr) in link_scores(searcher, &named) {
                let Some(domain) = domain_of(searcher, addr) else {
                    continue;
                };
                let terms = before.join(" ");
                let link = search_template_for(&domain)
                    .filter(|_| link_score >= WELL_KNOWN_LINK_SCORE)
                    .and_then(|template| search_link(template, &domain, &terms));
                if let Some(url) = link {
                    return Ok(Some(SiteSearch { domain, terms, url }));
                }
            }
        }
        Ok(None)
    }

    /// A link into the search of the site at `addr` for `terms`.
    fn site_search_link(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
        terms: String,
    ) -> Result<Option<SiteSearch>> {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let text = |field| {
            doc.get_first(field)
                .and_then(|value| value.as_str())
                .map(str::to_string)
        };
        let Some(domain) = text(self.fields.domain) else {
            return Ok(None);
        };
        let link = text(self.fields.search_url)
            .and_then(|template| search_link(&template, &domain, &terms))
            .or_else(|| search_link(search_template_for(&domain)?, &domain, &terms));
        Ok(link.map(|url| SiteSearch { domain, terms, url }))
    }
}
