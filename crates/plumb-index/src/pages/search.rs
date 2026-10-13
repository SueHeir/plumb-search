//! Opening the page index and the searches callers make of it: pages,
//! entities, definitions, songs, spelling, and a site's popularity.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::paper_query::PaperQuery;
use plumb_core::{registrable_domain, Operators};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, ConstScoreQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{Index, ReloadPolicy, TantivyDocument, Term};

use super::building::schema;
use super::page::is_item_id;
use super::scoring::truncate_keeping_inner_pages;
use super::{
    Page, PageHit, PageIndexStats, PageSearcher, PaperCoverage, ALIAS_MATCH, CANDIDATES, FILMS_SET,
    GITHUB_SET, KNOWN_WORD_PAGES, MIN_PAGE_FIX_PAGES, MUSIC_SET, PAGE_FIX_RATIO, PAPERS_SET,
    POPULARITY_SCALE, POPULARITY_SHARE, TITLE_CANDIDATES, WIKIDATA_SET, WIKTIONARY_SET,
};
use crate::analysis;

/// What kind of page words around a name ask for ([`hinted_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hint {
    /// Its encyclopedia article: "lululemon wikipedia".
    Article,
    /// Its repository on GitHub: "tokio github".
    Repo,
    /// A paper of that title: "batch normalization paper".
    Paper,
    /// A person of that name, of whom something is asked: "obama
    /// daughters", "picasso paintings".
    Person,
    /// Whatever is named so: "who is galileo", "ariana grande age".
    Any,
}

/// Words before a name that ask about it. "What's" is read as "what is";
/// "whats" is not, since "whats app" is a misspelled name.
const HINT_LEADS: &[&str] = &[
    "who is ",
    "who was ",
    "who are ",
    "who were ",
    "tell me about ",
    "what is the meaning of ",
    "what is the definition of ",
    "what is a ",
    "what is an ",
    "what is the ",
    "what is ",
    "what are ",
    "what was ",
    "what were ",
    "what does ",
    "define ",
    "definition of ",
    "meaning of ",
    "info about ",
    "information about ",
    "facts about ",
    "what causes a ",
    "what causes an ",
    "what causes the ",
    "what causes ",
    "where is the ",
    "where is ",
    "where are the ",
    "where are ",
    "how many people live in ",
];

/// What "how ... is" asks the measure of: "how tall is the eiffel tower".
const MEASURES: &[&str] = &[
    "tall", "old", "big", "long", "deep", "high", "far", "wide", "large", "heavy",
];

/// What a question of how something works or how big it is asks about:
/// "refrigerator" of "how does a refrigerator work", "eiffel tower" of
/// "how tall is the eiffel tower". `None` for other questions ("how do i
/// make pancakes", where "how do" starts any question) and for "it".
fn asked_how(q: &str) -> Option<&str> {
    let works = ["how does ", "how do "]
        .iter()
        .find_map(|lead| q.strip_prefix(lead)?.strip_suffix(" work"));
    let measured = || {
        let (measure, rest) = q.strip_prefix("how ")?.split_once(' ')?;
        let rest = ["is ", "are ", "was "]
            .iter()
            .find_map(|verb| rest.strip_prefix(verb))?;
        MEASURES.contains(&measure).then_some(rest)
    };
    let about = works.or_else(measured)?;
    let about = ["a ", "an ", "the "]
        .iter()
        .find_map(|article| about.strip_prefix(article))
        .unwrap_or(about);
    (!matches!(about, "it" | "this" | "that" | "they" | "these" | "those")).then_some(about)
}

/// Words after a name that say what about it is wanted.
const HINT_TAILS: &[(&str, Hint)] = &[
    (" wikipedia", Hint::Article),
    (" wiki", Hint::Article),
    (" github repo", Hint::Repo),
    (" github", Hint::Repo),
    (" repository", Hint::Repo),
    (" repo", Hint::Repo),
    (" paper", Hint::Paper),
    (" papers", Hint::Paper),
    (" arxiv", Hint::Paper),
    (" summary", Hint::Any),
    (" plot", Hint::Any),
    (" biography", Hint::Any),
    (" bio", Hint::Any),
    (" age", Hint::Any),
    (" birthday", Hint::Any),
    (" height", Hint::Any),
    (" net worth", Hint::Any),
    (" wife", Hint::Any),
    (" husband", Hint::Any),
    // What people ask about a person, which a person's article answers but
    // whose words it is not called by. "Cause of death" before "death".
    (" cause of death", Hint::Person),
    (" death", Hint::Person),
    (" kids", Hint::Person),
    (" children", Hint::Person),
    (" daughter", Hint::Person),
    (" daughters", Hint::Person),
    (" son", Hint::Person),
    (" sons", Hint::Person),
    (" parents", Hint::Person),
    (" mother", Hint::Person),
    (" father", Hint::Person),
    (" siblings", Hint::Person),
    (" family", Hint::Person),
    (" girlfriend", Hint::Person),
    (" boyfriend", Hint::Person),
    (" education", Hint::Person),
    (" nationality", Hint::Person),
    (" religion", Hint::Person),
    (" awards", Hint::Person),
    (" stats", Hint::Person),
    (" movies", Hint::Person),
    (" films", Hint::Person),
    (" books", Hint::Person),
    (" novels", Hint::Person),
    (" poems", Hint::Person),
    (" songs", Hint::Person),
    (" albums", Hint::Person),
    (" paintings", Hint::Person),
    (" inventions", Hint::Person),
    (" cast", Hint::Any),
    (" author", Hint::Any),
    (" movie", Hint::Any),
    (" film", Hint::Any),
    (" meaning", Hint::Any),
    (" mean", Hint::Any),
    (" means", Hint::Any),
    (" definition", Hint::Any),
    (" stand for", Hint::Any),
    (" stands for", Hint::Any),
    (" explained", Hint::Any),
    (" quotes", Hint::Any),
];

/// The name in `query` without the words around it that only say what is
/// wanted, and what kind of page they ask for: "lululemon" of "lululemon
/// wikipedia". `None` when there are none, or nothing else.
pub(crate) fn hinted_name(query: &str) -> Option<(String, Hint)> {
    let q = plumb_core::collapse_whitespace(query.trim().trim_end_matches('?'))
        .to_lowercase()
        .replace('’', "'");
    let asked = q
        .strip_prefix("what's ")
        .map(|rest| format!("what is {rest}"));
    let mut name = asked.as_deref().unwrap_or(&q);
    let mut hint = None;
    if let Some(rest) =
        asked_how(name).or_else(|| HINT_LEADS.iter().find_map(|lead| name.strip_prefix(lead)))
    {
        name = rest;
        hint = Some(Hint::Any);
    }
    if let Some((rest, tail_hint)) = HINT_TAILS
        .iter()
        .find_map(|(tail, h)| Some((name.strip_suffix(tail)?, *h)))
    {
        name = rest;
        hint = Some(tail_hint);
    }
    let name = name.trim();
    let hint = hint?;
    (!name.is_empty() && name != q).then(|| (name.to_string(), hint))
}

/// Least listeners of a song or album that [`PageSearcher::known_song`]
/// takes a search of its title alone to be for: about the 7,000 most
/// listened.
pub const KNOWN_SONG_LISTENERS: u64 = 50_000;

/// How many times the listeners of every other song or album of its title
/// such a one has.
pub const KNOWN_SONG_MARGIN: u64 = 5;

/// How many of the first sites [`PageSearcher::note_demand`] notes.
pub const DEMAND_NOTED: usize = 3;

impl PageSearcher {
    pub fn open(dir: &Path) -> Result<Self> {
        let index = Index::open_in_dir(dir)
            .with_context(|| format!("opening the page index in {}", dir.display()))?;
        analysis::register(index.tokenizers());
        let (schema, fields) = schema();
        if index.schema() != schema {
            bail!(
                "the page index in {} was built by another version; rebuild it",
                dir.display()
            );
        }
        let stats: PageIndexStats = serde_json::from_slice(
            &std::fs::read(dir.join("pages.json"))
                .with_context(|| format!("reading {}/pages.json", dir.display()))?,
        )?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(PageSearcher {
            reader,
            fields,
            words: analysis::words_analyzer(),
            joined: analysis::joined_analyzer(),
            stemmed: analysis::stemmed_analyzer(),
            stats,
            requested_language: None,
        })
    }

    pub fn num_pages(&self) -> u64 {
        self.stats.pages
    }

    pub fn paper_coverage(&self) -> &PaperCoverage {
        &self.stats.papers
    }

    /// A lightweight view restricted to declared/set-supported language
    /// before each candidate collector's cap. The existing English path
    /// retains old records with unknown language, still labeled unknown;
    /// other languages require actual metadata. Invalid tags match nothing.
    pub fn in_language(&self, language: Option<&str>) -> Self {
        Self {
            reader: self.reader.clone(),
            fields: self.fields,
            words: self.words.clone(),
            joined: self.joined.clone(),
            stemmed: analysis::content_analyzer(
                language.and_then(plumb_core::language_code).as_deref(),
            ),
            stats: self.stats.clone(),
            requested_language: language
                .map(|tag| plumb_core::language_code(tag).unwrap_or_else(|| "invalid".to_string())),
        }
    }

    /// Actual indexed language counts, for explaining empty constrained
    /// results without claiming that the requested pages do not exist.
    pub fn language_coverage(&self) -> (&std::collections::BTreeMap<String, u64>, u64) {
        let known = self
            .stats
            .languages
            .values()
            .fold(0u64, |count, pages| count.saturating_add(*pages));
        (
            &self.stats.languages,
            self.stats.pages.saturating_sub(known),
        )
    }

    pub(crate) fn language_query(&self, query: &dyn Query) -> Box<dyn Query> {
        match &self.requested_language {
            None => query.box_clone(),
            Some(language) => {
                let mut allowed = vec![(
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.language, language),
                        IndexRecordOption::Basic,
                    )) as Box<dyn Query>,
                )];
                if language == "en" {
                    allowed.push((
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.language, "unknown"),
                            IndexRecordOption::Basic,
                        )),
                    ));
                }
                Box::new(BooleanQuery::new(vec![
                    (Occur::Must, query.box_clone()),
                    (
                        Occur::Must,
                        Box::new(ConstScoreQuery::new(
                            Box::new(BooleanQuery::new(allowed)),
                            0.0,
                        )),
                    ),
                ]))
            }
        }
    }

    /// How much the most read Wikipedia article about what `domain` is the
    /// official website of is read, as [`PageHit::popularity`]: how much
    /// people look up what the site is. `None` when no article is about it.
    pub fn site_popularity(&self, domain: &str) -> Result<Option<f32>> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.site, domain),
            IndexRecordOption::Basic,
        );
        let best = searcher.search(
            &query,
            &TopDocs::with_limit(1).order_by_fast_field::<u64>("popularity", tantivy::Order::Desc),
        )?;
        Ok(best
            .first()
            .and_then(|&(popularity, _)| popularity)
            .map(|popularity| popularity as f32 / POPULARITY_SCALE))
    }

    /// Notes on the first [`DEMAND_NOTED`] of `sites` how much the article
    /// about each is read ([`PageSearcher::site_popularity`],
    /// [`crate::Hit::demand`]), for [`place_pages`](super::place_pages) to
    /// weigh against the pages a query names. A few, not one: a node may
    /// reorder the first results for its searcher after.
    pub fn note_demand(&self, sites: &mut [crate::Hit]) -> Result<()> {
        for site in sites.iter_mut().take(DEMAND_NOTED) {
            site.demand = self.site_popularity(&site.domain)?;
        }
        Ok(())
    }

    /// Whether `word` (one word of [`analysis::words_analyzer`]) is in the
    /// names of at least [`KNOWN_WORD_PAGES`] pages.
    pub fn knows_word(&self, word: &str) -> Result<bool> {
        let searcher = self.reader.searcher();
        Ok(searcher.doc_freq(&Term::from_field_text(self.fields.words, word))? >= KNOWN_WORD_PAGES)
    }

    /// `spelling`, suggested for `query`, with the words the pages know
    /// ([`PageSearcher::knows_word`]) put back as typed; `None` when no
    /// change is left. The sites index hardly knows "pkce", "dplyr" or
    /// "stain", so it suggests "pace", "plyr" and "spain"; pages name them.
    /// A correction to a site's name ([`crate::Spelling::site`]) is kept:
    /// "wels fargo" is a typo even though Wels is a town.
    pub fn check_spelling(
        &self,
        query: &str,
        spelling: crate::Spelling,
    ) -> Result<Option<crate::Spelling>> {
        if spelling.site.is_some() {
            return Ok(Some(spelling));
        }
        let typed = analysis::tokens(&self.words, query);
        let mut fixed: Vec<String> = spelling
            .query
            .split_whitespace()
            .map(String::from)
            .collect();
        if typed.len() == fixed.len() {
            for (word, fix) in typed.iter().zip(fixed.iter_mut()) {
                if word != fix && self.knows_word(word)? {
                    fix.clone_from(word);
                }
            }
        } else {
            // Words were split or joined, so they can't be put back one by
            // one: a known word among those changed drops the suggestion.
            for word in &typed {
                if !fixed.contains(word) && self.knows_word(word)? {
                    return Ok(None);
                }
            }
        }
        if fixed == typed {
            return Ok(None);
        }
        Ok(Some(crate::Spelling {
            query: fixed.join(" "),
            ..spelling
        }))
    }

    /// A suggested spelling of `query` from the words of the pages' names,
    /// for queries about things rather than sites ("anubas", "budafest"),
    /// whose words the sites index hardly has. Each word the pages do not
    /// know ([`PageSearcher::knows_word`]) and `site_known` does not either
    /// is replaced by the near word found in the most pages, at least
    /// [`PAGE_FIX_RATIO`] times as many as have the word typed and at least
    /// [`MIN_PAGE_FIX_PAGES`], or with the spelling model, the likeliest by
    /// the noisy channel ([`crate::spell_model`]). With the model, a word
    /// is kept unless the slip is likelier than the word, a word no page
    /// has counting as if one did ("perft" is not "perf"). `None` when no
    /// word changes.
    pub fn suggest_spelling(
        &self,
        query: &str,
        model: Option<&crate::spell_model::Model>,
        site_known: &dyn Fn(&str) -> bool,
    ) -> Result<Option<crate::Spelling>> {
        // A web address is a name, not words: "amazn.com" is not "amazon
        // com". The sites suggest the address it is a slip of.
        if crate::names::typed_domain(query).is_some() {
            return Ok(None);
        }
        let searcher = self.reader.searcher();
        let docs = |word: &str| -> Result<u64> {
            Ok(searcher.doc_freq(&Term::from_field_text(self.fields.words, word))?)
        };
        let typed = analysis::tokens(&self.words, query);
        let mut fixed = typed.clone();
        for word in &mut fixed {
            let chars = word.chars().count();
            let edits = crate::spell::max_edits(chars);
            if edits == 0
                || !word.chars().all(char::is_alphabetic)
                || self.knows_word(word)?
                || site_known(word)
            {
                continue;
            }
            let typed_docs = docs(word)?;
            let needed = MIN_PAGE_FIX_PAGES.max(PAGE_FIX_RATIO.saturating_mul(typed_docs));
            let mut best: Option<(f64, String)> = None;
            for (term, distance) in
                crate::spell::near_terms(&searcher, self.fields.words, word, edits)?
            {
                if !crate::spell::plausible_word_fix(word, &term) {
                    continue;
                }
                let term_docs = docs(&term)?;
                if term_docs < needed {
                    continue;
                }
                // Without a model, an edit costs as much as a thousand
                // times the pages.
                let cost = match model {
                    Some(model) => model.ln_channel(word, &term),
                    None => -7.0 * f64::from(distance),
                };
                let likelihood = cost + (term_docs as f64).ln();
                // As likely as the word typed, a word no page has counting
                // as if one did: "perft" is not "perf".
                if model.is_some() && likelihood < (typed_docs.max(1) as f64).ln() {
                    continue;
                }
                if best.as_ref().is_none_or(|(b, _)| likelihood > *b) {
                    best = Some((likelihood, term));
                }
            }
            if let Some((_, term)) = best {
                *word = term;
            }
        }
        Ok((fixed != typed).then(|| crate::Spelling {
            query: fixed.join(" "),
            site: None,
            applied: false,
        }))
    }

    /// Named entity candidates before site blending or learned placement.
    /// Wikipedia's `about` field is searched separately: reference pages
    /// with the same title cannot consume its bounded candidate window.
    /// Uses existing index fields, so an installed index needs no rebuild.
    pub fn entities(&self, query: &str, limit: usize) -> Result<Vec<PageHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let described = self.described_words(query);
        let words = analysis::tokens(&self.words, query);
        let Some(joined) = analysis::tokens(&self.joined, query).pop() else {
            return Ok(Vec::new());
        };
        let query_words: HashSet<&str> = words.iter().map(String::as_str).collect();
        let mut hits = Vec::new();
        if !described.is_empty() {
            let clauses = described
                .iter()
                .map(|word| {
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.about, &word.stem),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>,
                    )
                })
                .collect();
            let searcher = self.reader.searcher();
            let found = searcher.search(
                self.language_query(&BooleanQuery::new(clauses)).as_ref(),
                &TopDocs::with_limit(CANDIDATES)
                    .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc),
            )?;
            for (_, address) in found {
                let document: TantivyDocument = searcher.doc(address)?;
                let Some(stored) = document
                    .get_first(self.fields.page)
                    .and_then(|v| v.as_str())
                else {
                    continue;
                };
                let page: Page = serde_json::from_str(stored)?;
                if !page.is_article() || !page.item.as_deref().is_some_and(is_item_id) {
                    continue;
                }
                let (name, named) = self.name_match(&page, query, &joined, &query_words);
                let covered = self.described_match(&page, &described);
                if name <= 0.0 && covered <= 0.0 {
                    continue;
                }
                let popularity = document
                    .get_first(self.fields.popularity)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as f32
                    / POPULARITY_SCALE;
                hits.push(PageHit {
                    page,
                    named,
                    whole: covered > 0.0,
                    score: name.max(covered)
                        * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity),
                    popularity,
                    learned: None,
                });
            }
        }
        // Keep exact stop-word names ("The Who"), films and Wikidata profiles.
        for hit in self.search_once(query, CANDIDATES, None)? {
            if (matches!(hit.page.set.as_str(), WIKIDATA_SET | FILMS_SET)
                || hit.page.is_article() && hit.named)
                && hit.page.item.as_deref().is_some_and(is_item_id)
                && (hit.named || hit.whole)
                && !hits.iter().any(|kept| kept.page.url == hit.page.url)
            {
                hits.push(hit);
            }
        }
        hits.sort_by(|a, b| {
            b.named
                .cmp(&a.named)
                .then_with(|| b.score.total_cmp(&a.score))
        });
        hits.truncate(limit.min(CANDIDATES));
        Ok(hits)
    }

    /// The best `limit` pages for `query`, best first.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<PageHit>> {
        let paper = PaperQuery::parse(query)?;
        if paper.constrained {
            return self.search_papers(&paper, limit);
        }
        self.search_naming_docs(query, &Operators::default(), false, limit)
    }

    pub(crate) fn search_general(
        &self,
        query: &str,
        limit: usize,
        scope: Option<&dyn Query>,
    ) -> Result<Vec<PageHit>> {
        let mut hits = self.search_once(query, limit, scope)?;
        // "lululemon wikipedia", "batch normalization paper", "who is
        // galileo": the words that say what is wanted are not part of the
        // name, so the name is searched for too. So is what a fact question
        // asks about, or "when was isaac newton born" lists Newton's mother
        // and namesakes, whose descriptions hold its words, and not him;
        // but not when the query is itself a name ("age of empires"). Of
        // that search only the pages the subject names are kept: "madrid
        // spanish" (of "madrid spanish capital city") names nothing, and
        // the clubs it describes would push Madrid out.
        let hinted = hinted_name(query);
        let asked = hinted.is_none();
        let Some((name, hint)) = hinted.or_else(|| {
            plumb_core::facts::fact_asked(query)
                .filter(|_| !hits.iter().any(|hit| hit.named))
                .map(|asked| (asked.subject, Hint::Any))
        }) else {
            return Ok(hits);
        };
        let mut found = self.search_once(&name, limit, scope)?;
        if asked {
            found.retain(|hit| hit.named);
        }
        match hint {
            // Only a person the name names, and not when a page is called
            // the whole query: "black death" is not about the singer Black,
            // nor "christmas songs" about Christmas.
            Hint::Person => {
                let called = hits.iter().any(|hit| hit.named);
                found.retain(|hit| !called && hit.named && hit.page.is_person());
            }
            Hint::Article => found.retain(|hit| hit.page.set.starts_with("wikipedia-")),
            // Only a repository called so: "reverse repo" is finance.
            Hint::Repo => found.retain(|hit| hit.named && hit.page.set == GITHUB_SET),
            Hint::Paper => {
                if found.iter().any(|hit| hit.page.set == PAPERS_SET) {
                    found.retain(|hit| hit.page.set == PAPERS_SET);
                }
                // A paper whose title starts with the name: "Batch
                // Normalization: Accelerating Deep Network Training…".
                for hit in &mut found {
                    let title = hit.page.title.to_lowercase();
                    let head = title.split(':').next().unwrap_or("").trim();
                    if hit.page.set == PAPERS_SET && head == name {
                        hit.named = true;
                        hit.score = hit.score.max(
                            ALIAS_MATCH
                                * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * hit.popularity),
                        );
                    }
                }
            }
            Hint::Any => {}
        }
        for hit in found {
            // Found both ways, a page keeps its better score and whatever
            // either way says of it.
            match hits.iter_mut().find(|h| h.page == hit.page) {
                Some(kept) => {
                    kept.named |= hit.named;
                    kept.whole |= hit.whole;
                    kept.score = kept.score.max(hit.score);
                }
                None => hits.push(hit),
            }
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        truncate_keeping_inner_pages(&mut hits, limit);
        Ok(hits)
    }

    pub(crate) fn scope_term(&self, text: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(self.fields.scope, text),
            IndexRecordOption::Basic,
        ))
    }

    pub(crate) fn page_scope(&self, ops: &Operators, docs: bool) -> Option<Box<dyn Query>> {
        let mut clauses = Vec::new();
        if docs {
            clauses.push((Occur::Must, self.scope_term("set:docs")));
        }
        if !ops.sites.is_empty() {
            let mut hosts = Vec::new();
            for site in &ops.sites {
                hosts.push((Occur::Should, self.scope_term(&format!("under:{site}"))));
                // Operators also allow a site's registrable domain for a
                // subdomain that is not a separately counted site.
                if let Some(domain) = registrable_domain(site) {
                    hosts.push((Occur::Should, self.scope_term(&format!("host:{domain}"))));
                }
            }
            clauses.push((
                Occur::Must,
                Box::new(BooleanQuery::new(hosts)) as Box<dyn Query>,
            ));
        }
        for site in &ops.not_sites {
            clauses.push((Occur::MustNot, self.scope_term(&format!("under:{site}"))));
        }
        // A BooleanQuery with only exclusions matches nothing.
        if !clauses.is_empty() && clauses.iter().all(|(occur, _)| *occur == Occur::MustNot) {
            clauses.push((Occur::Must, Box::new(tantivy::query::AllQuery)));
        }
        (!clauses.is_empty()).then(|| Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>)
    }

    /// The Wiktionary word `name` is ([`WIKTIONARY_SET`]), written as it
    /// is or with other capitals ("anadromous", "AWOL"): the best-known
    /// such word, the one written alike first.
    pub fn definition(&self, name: &str) -> Result<Option<Page>> {
        let Some(joined) = analysis::tokens(&self.joined, name).pop() else {
            return Ok(None);
        };
        let searcher = self.reader.searcher();
        let named = TermQuery::new(
            Term::from_field_text(self.fields.word, &joined),
            IndexRecordOption::Basic,
        );
        let top = TopDocs::with_limit(TITLE_CANDIDATES)
            .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc);
        let wanted = plumb_core::collapse_whitespace(name);
        let mut best: Option<Page> = None;
        for (_, address) in searcher.search(&named, &top)? {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            if page.set != WIKTIONARY_SET || !page.title.eq_ignore_ascii_case(&wanted) {
                continue;
            }
            if page.title == wanted {
                return Ok(Some(page));
            }
            best.get_or_insert(page);
        }
        Ok(best)
    }

    /// Adds to `found`, the pages [`PageSearcher::search`] found for
    /// `query`, the article titled in the other number of its last word
    /// when none is named as typed and the best of `sites` only shares a
    /// word with the query ([`only_shares_a_word`]): "tariffs" names the
    /// article Tariff over tariffs.net. A query that sites answer well
    /// keeps its sites: "used cars" is not the article Used car.
    pub fn add_other_number(
        &self,
        query: &str,
        sites: &[crate::Hit],
        found: &mut Vec<PageHit>,
        limit: usize,
    ) -> Result<()> {
        if found.iter().any(|hit| hit.named && hit.page.is_article())
            || !sites.first().is_none_or(only_shares_a_word)
        {
            return Ok(());
        }
        let Some(other) = last_word_in_other_number(query) else {
            return Ok(());
        };
        let mut added = false;
        for hit in self.search(&other, limit)? {
            if !hit.named || !hit.page.is_article() {
                continue;
            }
            // Found as typed only by some of its words: named now.
            match found.iter_mut().find(|h| h.page == hit.page) {
                Some(listed) if listed.named => {}
                Some(listed) => *listed = hit,
                None => found.push(hit),
            }
            added = true;
        }
        if added {
            found.sort_by(|a, b| b.score.total_cmp(&a.score));
            found.truncate(limit);
        }
        Ok(())
    }

    /// The song or album of the music set whose title is the whole of
    /// `query` when one is far better known than every other of that
    /// title: Radiohead's "Creep" for "creep", with
    /// [`KNOWN_SONG_MARGIN`] times the listeners of TLC's. `None` for a
    /// title many share about equally ("hello", "yesterday"), or one of
    /// fewer than [`KNOWN_SONG_LISTENERS`] listeners. Such a search does
    /// not list the song (its title is too common a word), but it may be
    /// for it.
    pub fn known_song(&self, query: &str) -> Result<Option<Page>> {
        let Some(joined) = analysis::tokens(&self.joined, query).pop() else {
            return Ok(None);
        };
        let searcher = self.reader.searcher();
        let named = TermQuery::new(
            Term::from_field_text(self.fields.keys, &joined),
            IndexRecordOption::Basic,
        );
        let top = TopDocs::with_limit(CANDIDATES)
            .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc);
        let mut songs = Vec::new();
        for (_, address) in searcher.search(&named, &top)? {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            if page.set == MUSIC_SET {
                songs.push(page);
            }
        }
        songs.sort_by_key(|page| std::cmp::Reverse(page.views));
        let mut songs = songs.into_iter();
        let Some(best) = songs.next() else {
            return Ok(None);
        };
        let runner_up = songs.next().map_or(0, |page| page.views);
        Ok((best.views >= KNOWN_SONG_LISTENERS
            && best.views >= runner_up.saturating_mul(KNOWN_SONG_MARGIN))
        .then_some(best))
    }
}

/// `query` with its last word in the other number
/// ([`plumb_core::other_number`]): "tariffs" -> "tariff",
/// "no kings protest" -> "no kings protests". `None` when the word has no
/// other number ("news") or the query is an address.
pub(crate) fn last_word_in_other_number(query: &str) -> Option<String> {
    if query.contains('.') || query.contains(':') {
        return None;
    }
    let text = plumb_core::normalize_text(query);
    let (head, last) = match text.rsplit_once(' ') {
        Some((head, last)) => (Some(head), last),
        None => (None, text.as_str()),
    };
    let other = plumb_core::other_number(last)?;
    Some(match head {
        Some(head) => format!("{head} {other}"),
        None => other,
    })
}

/// The text match ([`crate::Hit::text_score`]) below which a site the
/// query does not name only shares a word with it (government.ru for
/// "government shutdown", at 0.15). Sites a description finds by meaning
/// match more ("used cars").
const SHARES_A_WORD_MATCH: f32 = 0.25;

/// The link score below which a site the query names is next to unknown
/// (tariffs.net, at 0.015).
const BARELY_LINKED: f32 = 0.1;

/// Whether `site`, the best site found, only shares a word with the
/// query: a site it does not name that matches it weakly
/// ([`SHARES_A_WORD_MATCH`]), or one it names that is not official and
/// next to nobody links to ([`BARELY_LINKED`]).
fn only_shares_a_word(site: &crate::Hit) -> bool {
    if site.named {
        !site.official && site.link_score < BARELY_LINKED
    } else {
        site.placing_text_score.unwrap_or(site.text_score) < SHARES_A_WORD_MATCH
    }
}
