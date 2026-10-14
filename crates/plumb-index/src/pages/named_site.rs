//! The inner page of a site that a search names along with the site:
//! "census population clock" is census.gov's Population Clock, "who
//! malaria" who.int's page on malaria. And the page at a web address
//! typed as the search: "en.wikipedia.org/wiki/Albert_Einstein".

use std::collections::HashSet;

use anyhow::Result;
use plumb_core::{domain_label, registrable_domain, Operators};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{TantivyDocument, Term};

use super::placing::{options_allow, subpage_asked};
use super::sites::squash;
use super::{
    asked_as_question, Page, PageHit, PageSearcher, PlacedPage, ASKING_WORDS, PARTIAL_MATCH,
    POPULARITY_SCALE, POPULARITY_SHARE, REFERENCE2_SET, REFERENCE_SET, SUBPAGES_SET, WANTED_SET,
    WIKIDATA_SET, WIKTIONARY_SET,
};
use crate::analysis;

/// Least share of a page's title, not counting the site's own name, that
/// the rest of the query must be: "malaria" is who.int's "Malaria", not
/// its "Malaria rapid diagnostic test performance".
const NAMED_SITE_PAGE_SHARE: f32 = 0.5;

/// Least share of the rest of the query that a title, or a part of it,
/// must be when the rest names it whole and says more ([`Naming::Within`]):
/// "malaria fact sheet" is who.int's "Malaria", "burns and scalds
/// treatment" nhs.uk's "Burns and scalds".
const NAMED_TITLE_SHARE: f32 = 1.0 / 3.0;

/// How the rest of a search names a page's title, or a part of it, the
/// site's name aside ([`PageSearcher::named_share`]).
#[derive(Clone, Copy)]
enum Naming {
    /// The title has every word of the rest, and they are at least
    /// [`NAMED_SITE_PAGE_SHARE`] of it.
    Covers,
    /// The rest has every word of the title, which is at least
    /// [`NAMED_TITLE_SHARE`] of it and mostly not the site's name.
    Within,
}

/// Best sites looked through for one whose name starts or ends the
/// search.
const NAMED_SITES: usize = 5;

/// Pages of the site looked at for each way of finding them.
const NAMED_SITE_CANDIDATES: usize = 20;

/// Where a title's subject ends and a subtitle or the site's name starts:
/// "Plantar fasciitis - Symptoms and causes - Mayo Clinic".
const TITLE_BREAKS: &[&str] = &[":", " - ", " | ", " -- ", " — ", " – ", " · "];

/// Pages on a typed address's host looked at for the one at it
/// ([`PageSearcher::list_typed_page`]).
const ADDRESS_CANDIDATES: usize = 50;

/// Most parts of a typed address's path searched for...
const ADDRESS_SEGMENTS: usize = 8;

/// ...and most of their words.
const ADDRESS_WORDS: usize = 12;

impl PageSearcher {
    /// Puts under the first of the best [`NAMED_SITES`] of `sites` whose
    /// name starts or ends `query` its inner page (of the subpages and
    /// reference sets) that the rest of the query names: "census population clock"
    /// carries census.gov's Population Clock, which the site's own name
    /// would otherwise leave to a newsroom story with "population" in it.
    /// The rest names the page's title by [`Naming::Covers`], or failing
    /// that by [`Naming::Within`] ([`PageSearcher::named_share`]). Placed,
    /// the site's other inner pages are left out
    /// ([`place_pages`](super::place_pages)), so one the search found that
    /// says more of the rest stays instead. Nothing changes when the whole
    /// query names the site or a page ("amazon rainforest" is the article,
    /// not amazon.com's page on it).
    pub fn add_named_site_page(
        &self,
        query: &str,
        sites: &[crate::Hit],
        found: &mut Vec<PageHit>,
    ) -> Result<()> {
        if sites.first().is_none_or(|site| site.named) || found.iter().any(|hit| hit.named) {
            return Ok(());
        }
        let words = analysis::tokens(&self.words, query);
        // Sites on the rest may come before it: who.int for "who
        // diabetes".
        let Some((site, rest)) = sites
            .iter()
            .take(NAMED_SITES)
            .filter(|site| !site.named)
            .find_map(|site| Some((site, rest_of_name(&words, site)?)))
        else {
            return Ok(());
        };
        // The "s" of "where's" says nothing.
        let rest: Vec<String> = rest
            .into_iter()
            .filter(|word| {
                word.chars().count() > 1
                    && !crate::is_function_word(word)
                    && !ASKING_WORDS.contains(&word.as_str())
            })
            .collect();
        let asked: HashSet<String> = analysis::tokens(&self.stemmed, &rest.join(" "))
            .into_iter()
            .collect();
        if asked.is_empty() {
            return Ok(());
        }
        // The site's name in the query and its title, which its pages'
        // titles repeat: "U.S. Census Bureau QuickFacts".
        let mut site_words: HashSet<String> = analysis::tokens(&self.stemmed, query)
            .into_iter()
            .chain(analysis::tokens(
                &self.stemmed,
                site.title.as_deref().unwrap_or(""),
            ))
            .collect();
        site_words.retain(|word| !asked.contains(word));
        // Words of the rest that may stand for a run of a title's words, as
        // typed: "sp" for "Special Publication", not "ir" of "irs".
        let mut initials: Vec<String> = rest
            .iter()
            .filter(|word| {
                (2..=5).contains(&word.chars().count())
                    && word.chars().all(char::is_alphabetic)
                    && asked.contains(*word)
            })
            .cloned()
            .collect();
        initials.sort();
        initials.dedup();
        let named = Named {
            query,
            site,
            sites,
            asked: &asked,
            site_words: &site_words,
            initials: &initials,
        };
        let Some(best) = self.best_named_site_page(&named, &rest)? else {
            return Ok(());
        };
        // A page of the site found by its words that says more of the rest:
        // who.int's "Questions and answers on RTS,S and R21 malaria
        // vaccines" for "who malaria vaccine", not its "Malaria".
        let said = self.named_words_said(&best.page, &named);
        let own = registrable_domain(&site.domain);
        if found.iter().any(|hit| {
            hit.page.is_site_page()
                && own.is_some()
                && registrable_domain(&hit.page.url) == own
                && self.named_words_said(&hit.page, &named) > said
        }) {
            return Ok(());
        }
        found.retain(|hit| hit.page.url != best.page.url);
        found.insert(0, best);
        Ok(())
    }

    /// Lists first among `placed` the page held at the web address that
    /// `query` is, or the nearest above it: "en.wikipedia.org/wiki/Albert_Einstein"
    /// is the article, not Wikipedia's homepage, "github.com/torvalds/linux"
    /// the repository, and a Stack Overflow question's address with its
    /// title after the number the question. It counts as named by the
    /// query, so an article gets the info box, in whatever language the
    /// searcher reads. Nothing changes for an address without a path, one
    /// no page held is at, or a page safe search leaves out.
    pub fn list_typed_page(
        &self,
        query: &str,
        options: &crate::SearchOptions,
        placed: &mut Vec<PlacedPage>,
    ) -> Result<()> {
        let any_language = crate::SearchOptions {
            language: None,
            ..options.clone()
        };
        let Some(hit) = self
            .page_at(query)?
            .filter(|hit| options_allow(&any_language, &hit.page))
        else {
            return Ok(());
        };
        let key = address_key(&hit.page.url);
        placed.retain(|p| address_key(&p.hit.page.url) != key);
        placed.insert(
            0,
            PlacedPage {
                hit,
                under: None,
                at: 0,
            },
        );
        Ok(())
    }

    /// The page held at the address `query` is, or else the one whose
    /// address is the longest start of its path. Found among the pages on
    /// its host named or worded like parts of its path.
    fn page_at(&self, query: &str) -> Result<Option<PageHit>> {
        let Some(typed) = typed_address(query) else {
            return Ok(None);
        };
        let typed_path = typed
            .split_once('?')
            .map_or(typed.as_str(), |(path, _)| path);
        let Some((host, path)) = typed_path.split_once('/') else {
            return Ok(None);
        };
        // A page is named by a part of its address ("Albert_Einstein",
        // "why-is-processing-…") or by two ("torvalds/linux").
        let segments: Vec<String> = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .take(ADDRESS_SEGMENTS)
            .map(|segment| percent_decoded(segment).replace(['_', '-'], " "))
            .collect();
        let names = segments
            .iter()
            .cloned()
            .chain(segments.windows(2).map(|pair| pair.join("/")));
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for name in names {
            if let Some(key) = analysis::tokens(&self.joined, &name).pop() {
                clauses.push((
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.keys, &key),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
        }
        let asked = typed.split_once('?').map_or("", |(_, asked)| asked);
        let words = analysis::tokens(
            &self.words,
            &format!("{} {}", segments.join(" "), percent_decoded(asked)),
        );
        for word in words
            .iter()
            .filter(|word| !plumb_core::is_function_word(word))
            .take(ADDRESS_WORDS)
        {
            clauses.push((
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.words, word),
                    IndexRecordOption::WithFreqs,
                )),
            ));
        }
        if clauses.is_empty() {
            return Ok(None);
        }
        let on_host = BooleanQuery::new(vec![
            (Occur::Must, self.scope_term(&format!("under:{host}"))),
            (Occur::Must, Box::new(BooleanQuery::new(clauses))),
        ]);
        let searcher = self.reader.searcher();
        let mut best: Option<(usize, PageHit)> = None;
        for (_, address) in searcher.search(
            &on_host,
            &TopDocs::with_limit(ADDRESS_CANDIDATES).order_by_score(),
        )? {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            let Some(at) = address_key(&page.url) else {
                continue;
            };
            // A page whose address has a query is that page only: "?id=5"
            // is not "?id=6". A query on the typed address alone is a
            // share link's ("?wprov=sfla1"). On a wiki a "/" is part of a
            // title, so "…/wiki/9/11" is not below "…/wiki/9".
            let wiki =
                page.is_article() || [WIKTIONARY_SET, WIKIDATA_SET].contains(&page.set.as_str());
            let holds = if at.contains('?') {
                typed == at
            } else {
                typed_path == at || (!wiki && typed_path.starts_with(&format!("{at}/")))
            };
            if !holds || best.as_ref().is_some_and(|(kept, _)| *kept >= at.len()) {
                continue;
            }
            let popularity = document
                .get_first(self.fields.popularity)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as f32
                / POPULARITY_SCALE;
            let hit = PageHit {
                page,
                score: 1.0,
                named: true,
                popularity,
                whole: true,
                learned: None,
            };
            best = Some((at.len(), hit));
        }
        Ok(best.map(|(_, hit)| hit))
    }

    /// The page of `named.site` whose title the words `rest` name best, as
    /// [`PageSearcher::add_named_site_page`] takes it, with [`Page::site`]
    /// set to the site: one placing would list.
    fn best_named_site_page(&self, named: &Named, rest: &[String]) -> Result<Option<PageHit>> {
        let site = named.site;
        let ops = Operators {
            sites: vec![site.domain.clone()],
            ..Operators::default()
        };
        let Some(scope) = self.page_scope(&ops, false) else {
            return Ok(None);
        };
        let sets: Box<dyn Query> = Box::new(BooleanQuery::new(
            [REFERENCE_SET, REFERENCE2_SET, SUBPAGES_SET, WANTED_SET]
                .iter()
                .map(|set| (Occur::Should, self.scope_term(&format!("set:{set}"))))
                .collect(),
        ));
        let with = |occur, field, option, terms: Vec<&String>| -> Box<dyn Query> {
            let words: Vec<(Occur, Box<dyn Query>)> = terms
                .into_iter()
                .map(|term| {
                    (
                        occur,
                        Box::new(TermQuery::new(Term::from_field_text(field, term), option))
                            as Box<dyn Query>,
                    )
                })
                .collect();
            self.language_query(&BooleanQuery::new(vec![
                (Occur::Must, Box::new(BooleanQuery::new(words))),
                (Occur::Must, scope.box_clone()),
                (Occur::Must, sets.box_clone()),
            ]))
        };
        // Titles with every word as typed, the shortest scoring best, else
        // pages about every word, whose titles may say it another way
        // ("shareholder letter"), else titles with any of the words, which
        // may spell some by their initials ("sp 811"); last, titles the
        // rest names whole ("malaria fact sheet").
        let searcher = self.reader.searcher();
        let any_word = (
            Occur::Should,
            self.fields.words,
            IndexRecordOption::WithFreqs,
            rest.iter().collect::<Vec<_>>(),
        );
        let ways = [
            (
                (
                    Occur::Must,
                    self.fields.words,
                    IndexRecordOption::WithFreqs,
                    rest.iter().collect(),
                ),
                Naming::Covers,
            ),
            (
                (
                    Occur::Must,
                    self.fields.topic,
                    IndexRecordOption::Basic,
                    named.asked.iter().collect(),
                ),
                Naming::Covers,
            ),
            (any_word.clone(), Naming::Covers),
            (any_word, Naming::Within),
        ];
        for ((occur, field, option, terms), naming) in ways {
            let mut best: Option<(f32, PageHit)> = None;
            for (_, address) in searcher.search(
                with(occur, field, option, terms).as_ref(),
                &TopDocs::with_limit(NAMED_SITE_CANDIDATES).order_by_score(),
            )? {
                let document: TantivyDocument = searcher.doc(address)?;
                let Some(stored) = document
                    .get_first(self.fields.page)
                    .and_then(|v| v.as_str())
                else {
                    continue;
                };
                let mut page: Page = serde_json::from_str(stored)?;
                let share = self.named_share(&page, named, naming);
                if !page.is_site_page()
                    || share == 0.0
                    || best.as_ref().is_some_and(|(kept, _)| share < *kept)
                {
                    continue;
                }
                let popularity = document
                    .get_first(self.fields.popularity)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as f32
                    / POPULARITY_SCALE;
                if best
                    .as_ref()
                    .is_some_and(|(kept, hit)| share == *kept && popularity <= hit.popularity)
                {
                    continue;
                }
                page.site = Some(site.domain.clone());
                let hit = PageHit {
                    page,
                    score: PARTIAL_MATCH
                        * share
                        * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity),
                    named: false,
                    popularity,
                    whole: false,
                    learned: None,
                };
                // One placing would leave out is no use: one in another
                // language, or a drug offered without a prescription.
                if crate::health::page_allowed(named.query, &hit.page)
                    && subpage_asked(named.query, named.sites, &hit)
                {
                    best = Some((share, hit));
                }
            }
            if let Some((_, hit)) = best {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }

    /// How well the rest of the search (`named.asked`) names `page`'s
    /// title, or a part of it before or after a break ([`TITLE_BREAKS`]),
    /// by `naming`, the words of the site's name aside and initials read
    /// as the words they stand for ([`join_initials`]): for
    /// [`Naming::Covers`], the share of the part the rest is; for
    /// [`Naming::Within`], the share of the rest the title and the page's
    /// path say ([`PageSearcher::named_words_said`]). 0 when no part is
    /// named so.
    fn named_share(&self, page: &Page, named: &Named, naming: Naming) -> f32 {
        let asked = named.asked;
        let title = page.title.as_str();
        let parts = TITLE_BREAKS
            .iter()
            .filter_map(|mark| title.split_once(mark))
            .flat_map(|(head, tail)| [head, tail]);
        let mut shares = std::iter::once(title).chain(parts).map(|part| {
            let mut words = analysis::tokens(&self.stemmed, part);
            let all = words.len();
            words.retain(|word| !named.site_words.contains(word));
            // Mostly the site's name: "World Health Organization (WHO)"
            // says nothing of "who won the world cup".
            let mostly_site = 2 * words.len() <= all;
            join_initials(&mut words, named.initials);
            let words: HashSet<String> = words.into_iter().collect();
            let both = words.intersection(asked).count();
            match naming {
                Naming::Covers if both == asked.len() => both as f32 / words.len() as f32,
                Naming::Within if both == words.len() && !mostly_site => {
                    both as f32 / asked.len() as f32
                }
                _ => 0.0,
            }
        });
        match naming {
            Naming::Covers => shares
                .filter(|share| *share >= NAMED_SITE_PAGE_SHARE)
                .fold(0.0, f32::max),
            // The path may say the rest: who.int's "Malaria" at
            // /fact-sheets/detail/malaria is the "malaria fact sheet", not
            // its page "Fact sheets".
            Naming::Within if shares.any(|share| share >= NAMED_TITLE_SHARE) => {
                self.named_words_said(page, named) as f32 / asked.len() as f32
            }
            Naming::Within => 0.0,
        }
    }

    /// How many words of the rest of the search `page`'s title and path
    /// have, initials read as the words they stand for.
    fn named_words_said(&self, page: &Page, named: &Named) -> usize {
        let mut words = analysis::tokens(&self.stemmed, &page.title);
        join_initials(&mut words, named.initials);
        let path = page.url.split_once("://").map_or("", |(_, rest)| rest);
        let path = path.split(['?', '#']).next().unwrap_or("");
        let path = path.split_once('/').map_or("", |(_, path)| path);
        words.extend(analysis::tokens(&self.stemmed, path));
        named
            .asked
            .iter()
            .filter(|word| words.contains(word))
            .count()
    }
}

/// Joins the first run of `words` whose initials spell each of `initials`
/// into it: "sp" for "Special Publication" in "Special Publication 811".
fn join_initials(words: &mut Vec<String>, initials: &[String]) {
    for short in initials {
        if words.contains(short) {
            continue;
        }
        let n = short.chars().count();
        let run = words.windows(n).position(|run| {
            run.iter()
                .filter_map(|word| word.chars().next())
                .eq(short.chars())
        });
        if let Some(at) = run {
            words.splice(at..at + n, [short.clone()]);
        }
    }
}

/// What [`PageSearcher::best_named_site_page`] looks for: the search, the
/// site it names and all its sites, the stemmed words of the rest of it,
/// the words of the site's name, which pages' titles repeat, and the
/// words of the rest that may be initials, sorted.
struct Named<'a> {
    query: &'a str,
    site: &'a crate::Hit,
    sites: &'a [crate::Hit],
    asked: &'a HashSet<String>,
    site_words: &'a HashSet<String>,
    initials: &'a [String],
}

/// The words of `words` other than the name of `site` that they start or
/// end with: "population clock" of "census population clock" for
/// census.gov. The name the ranking found the query to start with comes
/// first, which may be another name of the site ("census bureau",
/// "world health organization"), then the site's name in its domain at
/// either end; a site on a public suffix (www.nhs.uk) by its whole host
/// or its first label. `None` when neither end is the site's name, or
/// when a question starts with it: "who is the president" asks nothing of
/// who.int.
fn rest_of_name(words: &[String], site: &crate::Hit) -> Option<Vec<String>> {
    let host = site.domain.trim_start_matches("www.");
    let label = domain_label(host);
    let mut names = vec![squash(&label)];
    if label == host {
        names.extend(host.split('.').next().map(squash));
    }
    let lead = site
        .query_evidence
        .as_ref()
        .map_or(0, |evidence| evidence.partial_name_words);
    let (rest, leading) = if (1..words.len()).contains(&lead) {
        (&words[lead..], true)
    } else {
        names.iter().find_map(|label| {
            (1..words.len()).find_map(|n| {
                if squash(&words[..n].concat()) == *label {
                    Some((&words[n..], true))
                } else {
                    let end = words.len() - n;
                    (squash(&words[end..].concat()) == *label).then(|| (&words[..end], false))
                }
            })
        })?
    };
    let asks = leading
        && asked_as_question(&words.join(" "))
        && rest.first().is_some_and(|word| {
            crate::is_function_word(word) || ASKING_WORDS.contains(&word.as_str())
        });
    (!asks).then(|| rest.to_vec())
}

/// The address `query` is, when it is a web address with a path, as
/// [`address_key`] writes it, without a phone site's `m.`:
/// "en.wikipedia.org/wiki/albert_einstein" for
/// "https://en.m.wikipedia.org/wiki/Albert_Einstein".
fn typed_address(query: &str) -> Option<String> {
    crate::names::typed_domain(query)?;
    let query = query.trim();
    let url = if query.contains("://") {
        query.to_string()
    } else {
        format!("https://{query}")
    };
    let key = address_key(&url)?;
    let (host, path) = key.split_once('/')?;
    if path.starts_with('?') || path.is_empty() {
        return None;
    }
    let labels: Vec<&str> = host.split('.').filter(|label| *label != "m").collect();
    let host = if labels.len() >= 2 {
        labels.join(".")
    } else {
        host.to_string()
    };
    Some(format!("{host}/{path}"))
}

/// `url` written to compare with others: its match key, which leaves out
/// the scheme, `www.`, a trailing `/` and tracking parameters, in lower
/// case, as Wikipedia and GitHub take an address whatever its case.
fn address_key(url: &str) -> Option<String> {
    Some(plumb_format::urls::match_key(url)?.to_lowercase())
}

/// `text` with its `%xx` escapes decoded, read as UTF-8.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = text
            .get(i + 1..i + 3)
            .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[i], escaped) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
