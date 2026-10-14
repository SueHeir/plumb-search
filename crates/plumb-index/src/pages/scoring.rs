//! One search of the page index: the candidates a query finds and how well
//! it names each (the score the module documentation describes).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use plumb_core::registrable_domain;
use plumb_core::singular;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{TantivyDocument, Term};

use super::docs::docs_asked;
use super::page::is_item_id;
use super::sites::{base_title, short_titles};
use super::{
    Page, PageHit, PageSearcher, ALIAS_MATCH, ASKING_WORDS, BOOKS_SET, CANDIDATES,
    DESCRIBED_MISSING_FROM, DESCRIBED_QUERY_WORDS, DESCRIBED_WORD, DOCS_SET, DUPLICATE_WORD_SHARE,
    FILMS_SET, GITHUB_SET, LEAD_CANDIDATES, LEAD_MATCH, MUSIC_SET, OTHER_NAME_MATCH, PACKAGES_SET,
    PAPERS_SET, PAPER_TITLE_WORDS, PARTIAL_MATCH, PODCASTS_SET, POPULARITY_SCALE, POPULARITY_SHARE,
    QUESTION_QUERY_WORDS, QUESTION_SHARE, QUESTION_TITLE_SHARE, REFERENCE2_SET,
    REFERENCE_QUERY_WORDS, REFERENCE_SET, REPO_WEIGHT, SHELF_WEIGHT, SUBPAGES_SET,
    TITLE_INSIDE_CANDIDATES, UNSURE_WORD_SHARE, WANTED_SET, WELL_KNOWN_PACKAGE,
};
use crate::analysis;

/// Words a Wikipedia article's short description ends a work's kind
/// with, before "by" and its maker, each with the words a search may say
/// it is with: "1997 studio album by Radiohead".
const WORKS: &[(&str, &[&str])] = &[
    ("album", &["album"]),
    ("ep", &["ep", "album"]),
    ("mixtape", &["mixtape", "album"]),
    ("soundtrack", &["soundtrack", "album"]),
    ("song", &["song"]),
    ("single", &["single", "song"]),
    ("novel", &["novel", "book"]),
    ("novella", &["novella", "book"]),
    ("book", &["book"]),
    ("film", plumb_core::films::FILM_WORDS),
    ("play", &["play"]),
    ("poem", &["poem"]),
    ("painting", &["painting"]),
    ("opera", &["opera"]),
    ("musical", &["musical"]),
];

/// Who made the work a Wikipedia article whose short description is
/// `description` is on, and the words a search may say what it is with:
/// "Radiohead" and "album" of "1997 studio album by Radiohead". `None`
/// unless the word before "by" is a work's kind ([`WORKS`]) and nothing
/// before it ties the article to another one's work: "Protagonist of 1949
/// novel by George Orwell" is a character, not the novel.
pub fn made_by(description: &str) -> Option<(&str, &'static [&'static str])> {
    let (what, by) = description.rsplit_once(" by ")?;
    let what: Vec<String> = what.split_whitespace().map(str::to_lowercase).collect();
    let (kind, before) = what.split_last()?;
    if before.iter().any(|word| plumb_core::is_function_word(word)) {
        return None;
    }
    WORKS
        .iter()
        .find(|(work, _)| work == kind)
        .map(|(_, asked)| (by.trim(), *asked))
}

/// A word of a query matched against what articles say of themselves
/// ([`PageSearcher::described_words`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Described {
    /// Its stem, which finds the articles and matches their names.
    pub(crate) stem: String,
    /// The word in the singular. An article that only says another word
    /// with the same stem does not say it: "scheduled" is not "schedule".
    pub(crate) form: String,
}

/// The stems and the singular words of what a Wikipedia article says of
/// itself ([`PageSearcher::said`]).
struct Said {
    stems: HashSet<String>,
    forms: HashSet<String>,
}

impl Said {
    fn says(&self, word: &Described) -> bool {
        self.stems.contains(&word.stem) && self.forms.contains(&word.form)
    }
}

impl PageSearcher {
    pub(crate) fn search_once(
        &self,
        query: &str,
        limit: usize,
        scope: Option<&dyn Query>,
    ) -> Result<Vec<PageHit>> {
        let words = analysis::tokens(&self.words, query);
        let sharped = analysis::sharp_letters(query);
        let Some(joined) = self.query_key(query)? else {
            return Ok(Vec::new());
        };
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        // Pages named by the whole query are searched apart, so pages that
        // merely have its words never crowd them out: the many Stack
        // Overflow questions on "google maps" are more read than the
        // article Google Maps.
        let named_by_query = TermQuery::new(
            Term::from_field_text(self.fields.keys, &joined),
            IndexRecordOption::Basic,
        );
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        // Pages with every word of the query.
        let every_word = BooleanQuery::new(
            words
                .iter()
                .map(|word| {
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.words, word),
                            IndexRecordOption::WithFreqs,
                        )) as Box<dyn Query>,
                    )
                })
                .collect(),
        );
        clauses.push((Occur::Should, Box::new(every_word.clone())));
        // Books, podcasts, songs and albums named by their title and what
        // they are: "dune book", "hardcore history podcast", "hey jude song".
        if let Some((title, last)) = query.trim().rsplit_once(char::is_whitespace) {
            if matches!(
                last.to_lowercase().as_str(),
                "book" | "novel" | "podcast" | "song" | "album"
            ) {
                let title_words: Vec<(Occur, Box<dyn Query>)> =
                    analysis::tokens(&self.words, title)
                        .iter()
                        .map(|word| {
                            (
                                Occur::Must,
                                Box::new(TermQuery::new(
                                    Term::from_field_text(self.fields.words, word),
                                    IndexRecordOption::Basic,
                                )) as Box<dyn Query>,
                            )
                        })
                        .collect();
                if !title_words.is_empty() {
                    clauses.push((Occur::Should, Box::new(BooleanQuery::new(title_words))));
                }
                if let Some(key) = analysis::tokens(&self.joined, title).pop() {
                    clauses.push((
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.keys, &key),
                            IndexRecordOption::Basic,
                        )),
                    ));
                }
            }
        }
        let query_words: HashSet<&str> = words.iter().map(String::as_str).collect();
        let by_popularity = || {
            TopDocs::with_limit(CANDIDATES)
                .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc)
        };
        let mut addresses: Vec<_> = searcher
            .search(
                self.language_query(scoped_query(&named_by_query, scope).as_ref())
                    .as_ref(),
                &by_popularity(),
            )?
            .into_iter()
            .map(|(_, address)| address)
            .collect();
        let found = searcher.search(
            self.language_query(scoped_query(&BooleanQuery::new(clauses), scope).as_ref())
                .as_ref(),
            &by_popularity(),
        )?;
        // Site pages are as popular as their sites, more than all but the
        // most read articles: when they fill the candidates, the other
        // pages with every word are searched apart, or an article named
        // only by an alias ("nfl" for National Football League, "nhs") is
        // lost behind hundreds of pages titled "NFL Scores".
        let crowded = found.len() >= CANDIDATES;
        for (_, address) in found {
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        if crowded {
            let mut apart: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, Box::new(every_word))];
            for set in [SUBPAGES_SET, REFERENCE_SET, REFERENCE2_SET, WANTED_SET] {
                apart.push((Occur::MustNot, self.scope_term(&format!("set:{set}"))));
            }
            for (_, address) in searcher.search(
                self.language_query(scoped_query(&BooleanQuery::new(apart), scope).as_ref())
                    .as_ref(),
                &by_popularity(),
            )? {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        // Books and papers named by their title and then their author:
        // "random forests breiman". Searched apart, the titles of two words
        // or more that start the query; a page found only so is kept only
        // when it is asked for so.
        let mut by_title_first = HashSet::new();
        // Films and shows by a title of one word too: "inception 2010".
        // Other pages found so must be films, or articles on works, asked
        // for so: "1989 by taylor swift".
        let mut film_title_first = HashSet::new();
        for end in 1..words.len() {
            let Some(key) = self.span_key(&words[..end], &sharped)? else {
                continue;
            };
            let named = TermQuery::new(
                Term::from_field_text(self.fields.keys, &key),
                IndexRecordOption::Basic,
            );
            for (_, address) in searcher.search(
                self.language_query(scoped_query(&named, scope).as_ref())
                    .as_ref(),
                &by_popularity(),
            )? {
                if !addresses.contains(&address) {
                    addresses.push(address);
                    by_title_first.insert(address);
                    if end == 1 {
                        film_title_first.insert(address);
                    }
                }
            }
        }
        // A repository's short name is one word of its names, not a key:
        // "adguard home" names AdGuardHome. Searched apart too, and only
        // among repositories: many pages say "notebook", and they must not
        // crowd out the article "note book" names.
        if words.len() > 1 {
            let short_name = BooleanQuery::new(vec![
                (Occur::Must, self.scope_term(&format!("set:{GITHUB_SET}"))),
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.words, &joined),
                        IndexRecordOption::Basic,
                    )),
                ),
            ]);
            let most_read = TopDocs::with_limit(TITLE_INSIDE_CANDIDATES)
                .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc);
            for (_, address) in searcher.search(
                self.language_query(scoped_query(&short_name, scope).as_ref())
                    .as_ref(),
                &most_read,
            )? {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        // Packages, only ever found when the query asks for one: "serde
        // crate", "latest version of requests python", "serde_json".
        let package_query = plumb_core::packages::package_query(query);
        let package_key = package_query
            .as_ref()
            .and_then(|asked| analysis::tokens(&self.joined, &asked.name).pop());
        if let Some(key) = &package_key {
            let named = TermQuery::new(
                Term::from_field_text(self.fields.keys, key),
                IndexRecordOption::Basic,
            );
            for (_, address) in searcher.search(
                self.language_query(scoped_query(&named, scope).as_ref())
                    .as_ref(),
                &by_popularity(),
            )? {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        // Questions with most of the query's words, searched apart so they
        // never crowd out pages the query names.
        let stems = self.question_words(query);
        if stems.len() >= QUESTION_QUERY_WORDS.min(REFERENCE_QUERY_WORDS) {
            let needed = (stems.len() as f32 * QUESTION_SHARE).ceil() as usize;
            let most_words = BooleanQuery::with_minimum_required_clauses(
                stems
                    .iter()
                    .map(|stem| {
                        (
                            Occur::Should,
                            Box::new(TermQuery::new(
                                Term::from_field_text(self.fields.topic, stem),
                                IndexRecordOption::Basic,
                            )) as Box<dyn Query>,
                        )
                    })
                    .collect(),
                needed,
            );
            for (_, address) in searcher.search(
                self.language_query(scoped_query(&most_words, scope).as_ref())
                    .as_ref(),
                &by_popularity(),
            )? {
                // Found by most of the query's words, not only by a title
                // that starts it: "react usestate hook" is the docs page
                // "React useState" and more of its words.
                by_title_first.remove(&address);
                film_title_first.remove(&address);
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        // Articles that say what the query asks for, in their names or
        // description: "queen album" finds Queen (album), "mckinley
        // president" William McKinley.
        let described = self.described_words(query);
        if described.len() >= DESCRIBED_QUERY_WORDS {
            let mut needed = vec![described.len()];
            if described.len() >= DESCRIBED_MISSING_FROM {
                needed.push(described.len() - 1);
            }
            for needed in needed {
                let most_words = BooleanQuery::with_minimum_required_clauses(
                    described
                        .iter()
                        .map(|word| {
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(
                                    Term::from_field_text(self.fields.about, &word.stem),
                                    IndexRecordOption::Basic,
                                )) as Box<dyn Query>,
                            )
                        })
                        .collect(),
                    needed,
                );
                for (_, address) in searcher.search(
                    self.language_query(scoped_query(&most_words, scope).as_ref())
                        .as_ref(),
                    &by_popularity(),
                )? {
                    if !addresses.contains(&address) {
                        addresses.push(address);
                    }
                }
            }
        }
        // Articles whose lead, names and description have every such word:
        // "triassic jurassic cretaceous" finds Mesozoic, "manubrium
        // sternum" Sternum. Ranked by how well the leads match, not by
        // how often the articles are read.
        let mut lead_scores: HashMap<tantivy::DocAddress, f32> = HashMap::new();
        if described.len() >= DESCRIBED_QUERY_WORDS {
            let every_word = BooleanQuery::new(
                described
                    .iter()
                    .map(|word| {
                        let either: Vec<(Occur, Box<dyn Query>)> = [
                            (self.fields.lead, IndexRecordOption::WithFreqs),
                            (self.fields.about, IndexRecordOption::Basic),
                        ]
                        .into_iter()
                        .map(|(field, option)| {
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(
                                    Term::from_field_text(field, &word.stem),
                                    option,
                                )) as Box<dyn Query>,
                            )
                        })
                        .collect();
                        (
                            Occur::Must,
                            Box::new(BooleanQuery::new(either)) as Box<dyn Query>,
                        )
                    })
                    .collect(),
            );
            for (score, address) in searcher.search(
                self.language_query(scoped_query(&every_word, scope).as_ref())
                    .as_ref(),
                &TopDocs::with_limit(LEAD_CANDIDATES).order_by_score(),
            )? {
                // The stems are not enough: "liverpool schedule" is not
                // said by a "scheduled monument in Liverpool".
                let document: TantivyDocument = searcher.doc(address)?;
                let page: Option<Page> = document
                    .get_first(self.fields.page)
                    .and_then(|v| v.as_str())
                    .map(serde_json::from_str)
                    .transpose()?;
                let said = page.as_ref().and_then(|page| self.said(page));
                if said.is_some_and(|said| !described.iter().all(|word| said.says(word))) {
                    continue;
                }
                lead_scores.insert(address, score);
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        let best_lead = lead_scores.values().copied().fold(0.0f32, f32::max);
        // The query's rarest word that some question has, of those that
        // do not only ask: what it is about. A question without it has only the asking words ("how to
        // get rid of aphids" found "How do I get rid of my bounty?"). A
        // word the pages know that no question has is rarer still: no
        // question asks about it, so none is found. A word nothing knows
        // is likely a typo, and is passed over.
        let mut topic_word: Option<(u64, &String)> = None;
        let mut unknown = HashSet::new();
        if stems.len() >= QUESTION_QUERY_WORDS.min(REFERENCE_QUERY_WORDS) {
            for stem in &stems {
                let found = searcher.doc_freq(&Term::from_field_text(self.fields.topic, stem))?;
                if found == 0 && !self.knows_stem(query, stem)? {
                    unknown.insert(stem);
                } else if described.iter().any(|word| &word.stem == stem)
                    && topic_word.is_none_or(|(least, _)| found < least)
                {
                    topic_word = Some((found, stem));
                }
            }
        }
        let topic_word = topic_word.map(|(_, stem)| stem.as_str());
        let sure: Vec<String> = described
            .iter()
            .map(|word| &word.stem)
            .filter(|stem| !unknown.contains(stem))
            .cloned()
            .collect();
        let inside = self.named_inside(&searcher, &words, &sharped, scope, &mut addresses)?;
        let mut hits = Vec::new();
        let mut described_repos = super::repos::DescribedRepos::default();
        for address in addresses {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let mut page: Page = serde_json::from_str(stored)?;
            page.drop_borrowed_site();
            let popularity = document
                .get_first(self.fields.popularity)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as f32
                / POPULARITY_SCALE;
            if page.set == PACKAGES_SET {
                let asked = match (&package_query, &package_key) {
                    (Some(asked), Some(key)) => self.package_match(&page, asked, key, popularity),
                    _ => false,
                };
                if asked {
                    // Searched by its name alone ("serde_json"), the
                    // package is asked for as a whole.
                    let whole = std::iter::once(&page.title)
                        .chain(&page.aliases)
                        .any(|name| name.eq_ignore_ascii_case(query.trim()));
                    hits.push(PageHit {
                        score: ALIAS_MATCH
                            * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity),
                        page,
                        named: true,
                        popularity,
                        whole,
                        learned: None,
                    });
                }
                continue;
            }
            let asked_as_film = self.film_match(&page, &words);
            let asked_in_full = asked_as_film || self.book_match(&page, &words);
            // A podcast asked for by the end of its title alone ("hardcore
            // history podcast") ranks below one asked for by its whole
            // title or its host: "the daily podcast" is The Daily before
            // "On the Daily", "joe rogan podcast" The Joe Rogan Experience
            // before "At Least its Not Joe Rogan".
            let title_end =
                !asked_in_full && page.set == PODCASTS_SET && self.podcast_named(&page, &words);
            let asked_by_title = asked_in_full || title_end;
            // Songs and albums share their titles with too much ("Dead
            // Sea", "Notion", "Lord of the Flies"): one is only listed
            // when asked for by its artist or as a song or album.
            if page.set == MUSIC_SET && !asked_by_title {
                continue;
            }
            // Named by part of the query, by its title or another name in
            // full: "queen tour" is not about Queen (album).
            let span = inside.span(&page, &self.joined);
            let left_out = span.is_none()
                && (by_title_first.contains(&address) && !asked_by_title
                    || film_title_first.contains(&address)
                        && !(asked_as_film || asked_by_title && page.is_article())
                    || inside.only.contains(&address) && !asked_by_title);
            let repo_described =
                self.repo_described(&page, popularity, &words, &mut described_repos);
            if left_out && repo_described.is_none() {
                continue;
            }
            let (mut name, mut named) = self.name_match(&page, query, &joined, &query_words);
            let mut whole = false;
            if asked_by_title {
                let least = if title_end {
                    PARTIAL_MATCH
                } else {
                    ALIAS_MATCH
                };
                (name, named, whole) = (name.max(least), true, true);
            } else if !named && (page.set != DOCS_SET || docs_asked(&page, query)) {
                let (question, asked) = self.question_match(&page, &stems, &sure, topic_word);
                name = name.max(question);
                whole = asked;
            }
            if !named && !whole {
                name = name.max(self.described_match(&page, &described));
                if let Some(&lead) = lead_scores.get(&address).filter(|_| page.is_article()) {
                    name = name
                        .max(LEAD_MATCH * (0.5 + 0.5 * lead / best_lead.max(f32::MIN_POSITIVE)));
                }
                if let Some(span) = span {
                    name = name.max(self.inside_match(&page, span, &described));
                }
            }
            if name <= 0.0 && repo_described.is_none() {
                continue;
            }
            if let Some(key) = repo_described.filter(|_| !named) {
                described_repos.keep(hits.len(), key, left_out || name <= 0.0);
            }
            let mut score = name * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity);
            if !page.may_lead() {
                score *= SHELF_WEIGHT;
            } else if page.set == GITHUB_SET {
                score *= REPO_WEIGHT;
            }
            hits.push(PageHit {
                page,
                score,
                named,
                popularity,
                whole,
                learned: None,
            });
        }
        described_repos.name(&mut hits);
        // A name alone is a package's only when nothing else is called so:
        // "content-type" is the header's, "left-pad" its repository's.
        if let Some(key) = package_key.filter(|_| package_query.is_some_and(|asked| asked.alone)) {
            let called = |page: &Page| {
                std::iter::once(&page.title)
                    .chain(&page.aliases)
                    .chain(&page.names)
                    .any(|name| {
                        analysis::tokens(&self.joined, name).pop().as_deref() == Some(key.as_str())
                    })
            };
            if hits
                .iter()
                .any(|hit| hit.named && hit.page.package.is_none() && called(&hit.page))
            {
                hits.retain(|hit| hit.page.package.is_none());
            }
        }
        match words.as_slice() {
            [word] => {
                self.name_by_surname(word, &joined, &mut hits);
                self.ask_about_identifier(query, word, &mut hits);
            }
            [title, name @ ..] => self.name_by_title(title, name, &joined, &mut hits),
            [] => {}
        }
        if let [the, rest @ ..] = words.as_slice() {
            if the == "the" && !rest.is_empty() {
                self.name_without_the(rest, &mut hits);
            }
        }
        if let [name @ .., what] = words.as_slice() {
            self.name_by_description(name, what, &mut hits);
        }
        let mut hits = fold_films(hits);
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        truncate_keeping_inner_pages(&mut hits, limit);
        Ok(hits)
    }

    /// Finds the pages named by part of the query, adding them to
    /// `addresses`: articles whose title is some of the query and the rest
    /// says more about it ("titanic sinking", "radium discovery marie
    /// curie", "catholic homily"), repositories by the words of their names
    /// ("frp reverse proxy", [`PageSearcher::repo_described`]) and docs
    /// pages by their product and title ("python asyncio tutorial"). They lack the query's other words, so nothing
    /// else finds them. A part is any run of the query's `words` but the
    /// whole query, as long as it is not only function or asking words,
    /// keyed with the sharps it was typed with (`sharped`).
    fn named_inside(
        &self,
        searcher: &tantivy::Searcher,
        words: &[String],
        sharped: &[String],
        scope: Option<&dyn Query>,
        addresses: &mut Vec<tantivy::DocAddress>,
    ) -> Result<Inside> {
        let mut inside = Inside::default();
        let most_read = || {
            TopDocs::with_limit(TITLE_INSIDE_CANDIDATES)
                .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc)
        };
        let word = |text: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(self.fields.words, text),
                IndexRecordOption::Basic,
            ))
        };
        for start in 0..words.len() {
            for end in start + 1..=words.len() {
                let span = &words[start..end];
                if span.len() == words.len()
                    || span.iter().all(|word| {
                        crate::is_function_word(word) || ASKING_WORDS.contains(&word.as_str())
                    })
                {
                    continue;
                }
                let Some(key) = self.span_key(span, sharped)? else {
                    continue;
                };
                let mut lanes: Vec<Box<dyn Query>> = vec![Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.keys, &key),
                    IndexRecordOption::Basic,
                ))];
                // A repository's short name is words of its names, not a
                // key: "frp", "rust-cache". Only short runs with a word
                // few pages have, so a run of common words costs nothing.
                let rarest = span
                    .iter()
                    .map(|w| searcher.doc_freq(&Term::from_field_text(self.fields.words, w)))
                    .collect::<tantivy::Result<Vec<u64>>>()?
                    .into_iter()
                    .min()
                    .unwrap_or(0);
                if span.len() <= super::repos::NAME_WORDS && rarest <= REPO_NAME_PAGES {
                    let named = BooleanQuery::new(vec![
                        (Occur::Should, word(&key)),
                        (
                            Occur::Should,
                            Box::new(BooleanQuery::new(
                                span.iter().map(|w| (Occur::Must, word(w))).collect(),
                            )),
                        ),
                    ]);
                    lanes.push(Box::new(BooleanQuery::new(vec![
                        (Occur::Must, self.scope_term(&format!("set:{GITHUB_SET}"))),
                        (Occur::Must, Box::new(named)),
                    ])));
                }
                for lane in lanes {
                    for (_, address) in searcher.search(
                        self.language_query(scoped_query(lane.as_ref(), scope).as_ref())
                            .as_ref(),
                        &most_read(),
                    )? {
                        // Other pages found only so are left out. This
                        // comes after every other way pages are found, so
                        // "only so" means it.
                        if !addresses.contains(&address) {
                            addresses.push(address);
                            inside.only.insert(address);
                        }
                    }
                }
                let longest = inside.keys.entry(key).or_default();
                if span.len() > longest.len() {
                    *longest = span.to_vec();
                }
            }
        }
        Ok(inside)
    }

    /// How well the article or docs page `page`, named by `span`, part of
    /// the query, matches the whole of it, whose words are `described`
    /// ([`PageSearcher::described_words`]), by stem: [`ALIAS_MATCH`] times
    /// the share of them its name has, a word only what it says of itself
    /// has counting [`DESCRIBED_WORD`]. Nothing when that is half or less:
    /// such a name leaves most of the query unsaid. "terry stafford death"
    /// is the singer, but "london hotels" is not the article London. An
    /// article's qualifier is part of its name, so "the chemical element
    /// mercury" is closer to Mercury (element) than to Chemical element.
    fn inside_match(&self, page: &Page, span: &[String], described: &[Described]) -> f32 {
        if described.is_empty() {
            return 0.0;
        }
        let qualifier = super::sites::title_qualifier(&page.title).filter(|_| page.is_article());
        let named: HashSet<String> = self
            .question_words(&format!(
                "{} {}",
                span.join(" "),
                qualifier.unwrap_or_default()
            ))
            .into_iter()
            .collect();
        let mut said = page.about().or_else(|| page.topic()).unwrap_or_else(|| {
            std::iter::once(&page.title)
                .chain(&page.aliases)
                .chain(&page.description)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        });
        if let Some(lead) = &page.lead {
            said.push(' ');
            said.push_str(lead);
        }
        let said: HashSet<String> = self.question_words(&said).into_iter().collect();
        let in_name = described
            .iter()
            .filter(|word| named.contains(&word.stem))
            .count();
        let only_said = described
            .iter()
            .filter(|word| !named.contains(&word.stem) && said.contains(&word.stem))
            .count();
        let share = (in_name as f32 + DESCRIBED_WORD * only_said as f32) / described.len() as f32;
        if share > 0.5 {
            ALIAS_MATCH * share
        } else {
            0.0
        }
    }

    /// Whether the query `words` are a film's or show's title followed by
    /// words of its description (its year, director or cast) or by what it
    /// is: "inception 2010", "inception christopher nolan", "dune movie",
    /// "breaking bad tv show".
    fn film_match(&self, page: &Page, words: &[String]) -> bool {
        if page.set != FILMS_SET {
            return false;
        }
        let kinds = if page.is_show() {
            plumb_core::films::SHOW_WORDS
        } else {
            plumb_core::films::FILM_WORDS
        };
        let title = analysis::tokens(&self.words, &page.title);
        let rest = match words.strip_prefix(title.as_slice()) {
            Some(rest) if !title.is_empty() && !rest.is_empty() => rest,
            _ => return false,
        };
        let told: HashSet<String> =
            analysis::tokens(&self.words, page.description.as_deref().unwrap_or(""))
                .into_iter()
                .collect();
        rest.iter()
            .all(|word| told.contains(word) || kinds.contains(&word.as_str()))
    }

    /// Whether the package `page` is the one `asked` for, whose name's key
    /// is `key`: it is called so (or so for short: "gin" for
    /// github.com/gin-gonic/gin), on a registry the query names, and well
    /// known unless the query names a registry rather than a language.
    fn package_match(
        &self,
        page: &Page,
        asked: &plumb_core::packages::PackageQuery,
        key: &str,
        popularity: f32,
    ) -> bool {
        let Some(registry) = page.registry() else {
            return false;
        };
        let called = std::iter::once(&page.title)
            .chain(&page.aliases)
            .any(|name| analysis::tokens(&self.joined, name).pop().as_deref() == Some(key));
        called && asked.wants(registry.key) && (asked.surely || popularity >= WELL_KNOWN_PACKAGE)
    }

    /// The different stemmed words of `query`, as questions are searched.
    pub(crate) fn question_words(&self, query: &str) -> Vec<String> {
        let mut stems = analysis::tokens(&self.stemmed, query);
        let mut seen = HashSet::new();
        stems.retain(|stem| seen.insert(stem.clone()));
        stems
    }

    /// Whether a word of `query` whose stem is `stem` is one the pages
    /// know ([`PageSearcher::knows_word`]), as typed or as its stem:
    /// "aphids" is known by the names that say "aphid".
    fn knows_stem(&self, query: &str, stem: &str) -> Result<bool> {
        for word in analysis::tokens(&self.words, query) {
            let stems = analysis::tokens(&self.stemmed, &word);
            if stems.first().map(String::as_str) == Some(stem)
                && (self.knows_word(&word)? || self.knows_word(stem)?)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The words of `query` with different stems, without the words that
    /// only ask ([`ASKING_WORDS`]), as articles are matched on what they
    /// say of themselves: "resin" of "what does resin mean".
    pub(crate) fn described_words(&self, query: &str) -> Vec<Described> {
        let mut seen = HashSet::new();
        let mut described = Vec::new();
        for word in analysis::tokens(&self.words, query) {
            if ASKING_WORDS.contains(&word.as_str()) {
                continue;
            }
            for stem in analysis::tokens(&self.stemmed, &word) {
                if seen.insert(stem.clone()) {
                    let form = singular(&word);
                    described.push(Described { stem, form });
                }
            }
        }
        described
    }

    /// What a Wikipedia article says of itself ([`Page::about`], and its
    /// lead); `None` for other pages.
    fn said(&self, page: &Page) -> Option<Said> {
        let mut text = page.about()?;
        if let Some(lead) = &page.lead {
            text.push(' ');
            text.push_str(lead);
        }
        Some(Said {
            stems: analysis::tokens(&self.stemmed, &text).into_iter().collect(),
            forms: analysis::tokens(&self.words, &text)
                .iter()
                .map(|word| singular(word))
                .collect(),
        })
    }

    /// How well a Wikipedia article covers the query's `words`
    /// ([`PageSearcher::described_words`]) with one of its names, matched by
    /// stem, and what it says of itself ([`PageSearcher::said`]): 0 unless
    /// a name has some of them and the article all of them, or all but one
    /// when the name is whole in the query and the query has
    /// [`DESCRIBED_MISSING_FROM`] words.
    /// Otherwise [`PARTIAL_MATCH`] times the share of the query in the
    /// name, a word the article only says counting [`DESCRIBED_WORD`], and
    /// halfway down by the share of the name the query lacks: "queen
    /// album" covers Queen (album) by 0.45.
    pub(crate) fn described_match(&self, page: &Page, words: &[Described]) -> f32 {
        if words.len() < DESCRIBED_QUERY_WORDS {
            return 0.0;
        }
        let Some(said) = self.said(page) else {
            return 0.0;
        };
        let mut best = 0.0f32;
        for name in
            std::iter::once(base_title(&page.title)).chain(page.aliases.iter().map(String::as_str))
        {
            let name: HashSet<String> = analysis::tokens(&self.stemmed, name).into_iter().collect();
            let in_name = words
                .iter()
                .filter(|word| name.contains(&word.stem))
                .count();
            if in_name == 0 {
                continue;
            }
            let missing = words
                .iter()
                .filter(|word| !name.contains(&word.stem) && !said.says(word))
                .count();
            let whole_name = name
                .iter()
                .all(|stem| words.iter().any(|word| &word.stem == stem));
            if missing > 1 || missing == 1 && !(whole_name && words.len() >= DESCRIBED_MISSING_FROM)
            {
                continue;
            }
            let only_said = words.len() - in_name - missing;
            let query_share =
                (in_name as f32 + DESCRIBED_WORD * only_said as f32) / words.len() as f32;
            let name_share = name
                .iter()
                .filter(|stem| words.iter().any(|word| &word.stem == *stem))
                .count() as f32
                / name.len() as f32;
            best = best.max(PARTIAL_MATCH * query_share * (0.5 + 0.5 * name_share));
        }
        best
    }

    /// Whether `name` is only asking and function words once its code is
    /// dropped: the question "What does :(){ : :& };: do?" says in words
    /// what every "what does the liver do" says, and asks about nothing.
    /// "Stand for" asks as "mean" does: "What does ~$ stand for?".
    fn only_asks(&self, name: &str) -> bool {
        analysis::tokens(&self.words, name).iter().all(|word| {
            crate::is_function_word(word)
                || ASKING_WORDS.contains(&word.as_str())
                || matches!(word.as_str(), "stand" | "stands")
        })
    }

    /// How well a question's words cover the query's stemmed words
    /// `stems`: [`PARTIAL_MATCH`] times the share they have, when that is
    /// at least [`QUESTION_SHARE`] of at least [`QUESTION_QUERY_WORDS`].
    /// A word not in `sure`, the query's words that say what it asks (not
    /// [`ASKING_WORDS`], nor a word no page knows), counts
    /// [`UNSURE_WORD_SHARE`].
    /// And whether the query asks the question as a whole: it has all of
    /// the query's words, and the query at least [`QUESTION_TITLE_SHARE`]
    /// of its title's. A question without `topic_word`, the query's rarest
    /// word, or whose title has none of `sure`, matches not at all. The
    /// question's words are its title and
    /// tags, or the title of a duplicate of it and its tags, whichever
    /// covers the query best ([`Page::asked_as`]). A word only the
    /// duplicate's title has counts [`DUPLICATE_WORD_SHARE`], unless the
    /// query asks the duplicate as a whole.
    pub(crate) fn question_match(
        &self,
        page: &Page,
        stems: &[String],
        sure: &[String],
        topic_word: Option<&str>,
    ) -> (f32, bool) {
        let fewest = if page.is_site_page() {
            REFERENCE_QUERY_WORDS
        } else {
            QUESTION_QUERY_WORDS
        };
        if stems.len() < fewest {
            return (0.0, false);
        }
        let weight = |stem: &String| {
            if sure.contains(stem) {
                1.0
            } else {
                UNSURE_WORD_SHARE
            }
        };
        let all: f32 = stems.iter().map(weight).sum();
        let stemmed = |text: &str| -> HashSet<String> {
            analysis::tokens(&self.stemmed, text).into_iter().collect()
        };
        let asked_as = page.asked_as();
        let own = asked_as
            .first()
            .map(|(_, topic)| stemmed(topic))
            .unwrap_or_default();
        let mut best = (0.0f32, false);
        for (title, topic) in &asked_as {
            if page.is_question() && self.only_asks(title) {
                continue;
            }
            let words = stemmed(topic);
            if topic_word.is_some_and(|word| !words.contains(word)) {
                continue;
            }
            let covered = |duplicate_word: f32| {
                stems
                    .iter()
                    .filter(|stem| words.contains(*stem))
                    .map(|stem| {
                        weight(stem)
                            * if own.contains(stem) {
                                1.0
                            } else {
                                duplicate_word
                            }
                    })
                    .sum::<f32>()
                    / all
            };
            let share = covered(1.0);
            if share < QUESTION_SHARE {
                continue;
            }
            let title = stemmed(title);
            // Its tags alone do not make a question about the query: "What
            // is the tax status of assets recovered via bankruptcy
            // proceedings?", tagged capital-gains-tax and australia, does
            // not ask "what is the capital of australia".
            if page.is_question()
                && !sure.is_empty()
                && !sure.iter().any(|stem| title.contains(stem))
            {
                continue;
            }
            let asked = share >= 1.0
                && !title.is_empty()
                && title.iter().filter(|word| stems.contains(word)).count() as f32
                    >= QUESTION_TITLE_SHARE * title.len() as f32;
            // A duplicate's own words may be its asker's detail, not what
            // the question answers: "python list sort" asks nothing of
            // "Remove all whitespace in a string", though its duplicate "How
            // to sort a list of string ignoring whitespace in python?" has
            // every word.
            let share = if asked {
                share
            } else {
                covered(DUPLICATE_WORD_SHARE)
            };
            if share < QUESTION_SHARE {
                continue;
            }
            let found = (PARTIAL_MATCH * share, asked);
            if found.0 > best.0 || found.0 == best.0 && found.1 && !best.1 {
                best = found;
            }
        }
        best
    }

    /// Whether the query `words` are a book's, podcast's, paper's, song's
    /// or album's title followed by words of its author's name or by what
    /// it is ("book", "novel", "podcast", "paper"), or both: "dune frank
    /// herbert", "the great gatsby book", "nevermind nirvana album",
    /// "the daily podcast", "random forests breiman". A paper's year and
    /// venue may follow too, and a paper's whole title of
    /// [`PAPER_TITLE_WORDS`] words or more asks for it alone. A Wikipedia
    /// article on a work ([`made_by`]) is asked for so too, by its title
    /// or another name: "1984 george orwell" for Nineteen Eighty-Four. A
    /// podcast is asked for by its host and "podcast" too: "joe rogan
    /// podcast".
    pub(crate) fn book_match(&self, page: &Page, words: &[String]) -> bool {
        let description = page.description.as_deref().unwrap_or("");
        let byline = |byline: &str| description.strip_prefix(byline).unwrap_or("");
        let (by, kinds): (&str, &[&str]) = match page.set.as_str() {
            BOOKS_SET => (byline("Book by "), &["book", "novel"]),
            PODCASTS_SET => (byline("Podcast by "), &["podcast"]),
            PAPERS_SET => (byline("Paper by "), &["paper"]),
            MUSIC_SET if page.is_song() => (byline("Song by "), &["song"]),
            MUSIC_SET => (byline("Album by "), &["album"]),
            _ if page.is_article() => match made_by(description) {
                Some(work) => work,
                None => return false,
            },
            _ => return false,
        };
        // "Book by AUTHOR, YEAR", "Podcast by AUTHOR · CATEGORY", "Paper by
        // AUTHOR et al., YEAR, VENUE", "1997 studio album by ARTIST".
        let author = if page.set == PAPERS_SET {
            by.split(", ")
                .collect::<Vec<_>>()
                .join(" ")
                .replace(" et al.", "")
        } else if page.is_article() {
            by.to_string()
        } else {
            let by = by.split(" · ").next().unwrap_or(by);
            by.rsplit_once(", ")
                .map_or(by, |(name, _)| name)
                .to_string()
        };
        // Asked for artist first, the main artist alone: "kanye west drive
        // slow" for "Kanye West feat. Paul Wall & GLC".
        let main = [" feat. ", " ft. "]
            .iter()
            .find_map(|joiner| author.split_once(joiner))
            .map_or(author.as_str(), |(main, _)| main);
        let artist = analysis::tokens(&self.words, main);
        // A band led by someone is asked for by its leader's name, not by
        // its backing band's alone: "sports news" is not Huey Lewis and the
        // News's album Sports.
        let lower = main.to_lowercase();
        let leader = [" and the ", " & the "]
            .iter()
            .find_map(|joiner| lower.split_once(joiner))
            .map(|(leader, _)| analysis::tokens(&self.words, leader));
        let author: HashSet<String> = analysis::tokens(&self.words, &author).into_iter().collect();
        // The title, or the title without its subtitle: "Frankenstein" for
        // "Frankenstein; or, The Modern Prometheus". An article's without
        // what tells it from its namesakes ("1989 (Taylor Swift album)"),
        // and its other names.
        let short = page.title.split([':', ';']).next().unwrap_or("");
        let mut titles = vec![page.title.as_str(), short];
        if page.is_article() {
            titles.push(base_title(&page.title));
            titles.extend(page.aliases.iter().map(|alias| base_title(alias)));
        }
        titles.into_iter().enumerate().any(|(i, title)| {
            let title = analysis::tokens(&self.words, title);
            // Songs, albums and other works are as often asked for
            // artist first: "vampire weekend step", "nirvana nevermind
            // album", "beatles hey jude", "radiohead ok computer".
            let unthe = match artist.as_slice() {
                [the, rest @ ..] if the == "the" && !rest.is_empty() => rest,
                artist => artist,
            };
            if (page.set == MUSIC_SET || page.is_article())
                && !artist.is_empty()
                && !title.is_empty()
            {
                if let Some(rest) = words
                    .strip_prefix(artist.as_slice())
                    .or_else(|| words.strip_prefix(unthe))
                    .and_then(|rest| rest.strip_prefix(title.as_slice()))
                {
                    if rest.is_empty() || matches!(rest, [word] if kinds.contains(&word.as_str())) {
                        return true;
                    }
                }
            }
            let rest = match words.strip_prefix(title.as_slice()) {
                Some([]) => {
                    return page.set == PAPERS_SET && i == 0 && title.len() >= PAPER_TITLE_WORDS
                }
                Some(rest) if !title.is_empty() => rest,
                _ => return false,
            };
            // "hey jude by the beatles", "dune book", "nevermind nirvana
            // album": the maker, what the work is, or both.
            let rest = match rest {
                [by, rest @ ..] if by == "by" && !rest.is_empty() => rest,
                rest => rest,
            };
            let maker = match rest {
                [maker @ .., kind] if kinds.contains(&kind.as_str()) => maker,
                rest => rest,
            };
            maker.iter().all(|word| author.contains(word))
                && (maker.is_empty()
                    || leader
                        .as_ref()
                        .is_none_or(|leader| maker.iter().any(|word| leader.contains(word))))
        }) || page.set == PODCASTS_SET
            && words.split_last().is_some_and(|(kind, host)| {
                kinds.contains(&kind.as_str()) && !host.is_empty() && host == artist
            })
    }

    /// Whether `words` are the end of a podcast's title, two words or more,
    /// and "podcast": "hardcore history podcast" for "Dan Carlin's Hardcore
    /// History".
    fn podcast_named(&self, page: &Page, words: &[String]) -> bool {
        let Some((last, named)) = words.split_last() else {
            return false;
        };
        let title = analysis::tokens(&self.words, &page.title);
        last == "podcast" && named.len() >= 2 && title.ends_with(named)
    }

    pub(crate) fn name_match(
        &self,
        page: &Page,
        raw_query: &str,
        joined: &str,
        query: &HashSet<&str>,
    ) -> (f32, bool) {
        let key = |text: &str| {
            analysis::tokens(&self.joined, text)
                .pop()
                .unwrap_or_default()
        };
        let spelled = |text: &str| plumb_core::collapse_whitespace(text).to_lowercase();
        // A docs page is named only by one of its names in full: its title
        // alone ("Glossary") or part of it says too little.
        if page.set == DOCS_SET {
            let named = page.aliases.iter().any(|alias| key(alias) == joined);
            return if named {
                (ALIAS_MATCH, true)
            } else {
                (0.0, false)
            };
        }
        if spelled(&page.title) == spelled(raw_query) {
            return (1.0, true);
        }
        // A reference page is named only by its whole title: part of it
        // ("Stool") says too little, and it is found by its words
        // otherwise ([`Self::question_match`]).
        if page.is_site_page() {
            return if key(&page.title) == joined {
                (ALIAS_MATCH, true)
            } else {
                (0.0, false)
            };
        }
        // "Mozart (film)" and "Mozart!" are no better a match for "mozart"
        // than the redirect "Mozart" to "Wolfgang Amadeus Mozart":
        // popularity decides between them, as between "No Way Home (1996
        // film)" and "Spider-Man: No Way Home" for "no way home", or "Far
        // side of the Moon" (a redirect "Dark side of the Moon") and "The
        // Dark Side of the Moon" for "dark side of the moon".
        if key(&page.title) == joined
            || key(base_title(&page.title)) == joined
            || page.aliases.iter().any(|alias| key(alias) == joined)
            || page.is_article()
                && short_titles(&page.title)
                    .into_iter()
                    .any(|short| key(short) == joined)
        {
            return (ALIAS_MATCH, true);
        }
        // "ohm law" is Ohm's law, as its redirect would say if it had one.
        if page.is_article()
            && super::sites::without_possessive(base_title(&page.title))
                .is_some_and(|bare| key(&bare) == joined)
        {
            return (ALIAS_MATCH, true);
        }
        let mut best = if page.names.iter().any(|name| key(name) == joined) {
            OTHER_NAME_MATCH
        } else {
            0.0f32
        };
        for name in
            std::iter::once(base_title(&page.title)).chain(page.aliases.iter().map(String::as_str))
        {
            let words: HashSet<String> = analysis::tokens(&self.words, name).into_iter().collect();
            if words.is_empty() || page.is_question() && self.only_asks(name) {
                continue;
            }
            let shared = words
                .iter()
                .filter(|w| query.contains(w.as_ref() as &str))
                .count();
            // The query names the page and says more ("marie curie
            // radium"), or names part of it ("curie" for "Marie Curie").
            if shared == words.len() || shared == query.len() {
                let share = shared as f32 / words.len().max(query.len()) as f32;
                best = best.max(PARTIAL_MATCH * share);
            }
        }
        // "lincoln assassination" is about the Assassination of Abraham
        // Lincoln, which no redirect may call so.
        if page.is_article() && super::sites::event_of(&page.title, raw_query) {
            return (ALIAS_MATCH, true);
        }
        (best, false)
    }

    /// A search of one name written with capitals inside, which names no
    /// page, asks about it: "CrashLoopBackOff", "UnicodeDecodeError",
    /// "ECONNREFUSED". The questions with `word` in their title are
    /// about it, so they are listed as a question with every word of a
    /// longer search is, the most read first. "YouTube" and "iPhone"
    /// name their articles, and a word in lower case may be anything.
    fn ask_about_identifier(&self, query: &str, word: &str, hits: &mut [PageHit]) {
        let query = query.trim();
        let inner_capital = query.chars().skip(1).any(char::is_uppercase)
            && query.chars().all(char::is_alphanumeric);
        if !inner_capital || hits.iter().any(|hit| hit.named) {
            return;
        }
        for hit in hits.iter_mut().filter(|hit| hit.page.is_question()) {
            if analysis::tokens(&self.words, &hit.page.title)
                .iter()
                .any(|w| w == word)
            {
                let question =
                    PARTIAL_MATCH * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * hit.popularity);
                hit.score = hit.score.max(question);
            }
        }
    }

    /// Names the person a query of one `word` (whose key is `joined`) is
    /// the surname of when Wikipedia has no redirect from it: "thatcher"
    /// is Margaret Thatcher and "kafka" Franz Kafka, not the film Kafka.
    /// Only when they are read [`super::SURNAME_MARGIN`] times as much as
    /// every other article with the word in its title ("darwin" is a city
    /// too, "ford" two presidents), and no article is called the word
    /// ("swift", "clinton") nor a company's site ("tesla", "hilton").
    fn name_by_surname(&self, word: &str, joined: &str, hits: &mut [PageHit]) {
        let called =
            |name: &str| analysis::tokens(&self.joined, name).pop().as_deref() == Some(joined);
        let articles = || {
            hits.iter()
                .enumerate()
                .filter(|(_, hit)| hit.page.is_article())
        };
        let taken = articles().any(|(_, hit)| {
            std::iter::once(&hit.page.title)
                .chain(&hit.page.aliases)
                .any(|name| called(name))
                || super::sites::describes_an_organization(hit.page.description.as_deref())
                    && hit
                        .page
                        .site
                        .as_deref()
                        .and_then(|site| site.split('.').next())
                        .is_some_and(called)
        });
        let titled = |hit: &PageHit| analysis::tokens(&self.words, &hit.page.title);
        let Some((best, views)) = articles()
            .filter(|(_, hit)| {
                !hit.named
                    && hit.page.is_person()
                    && titled(hit).last().map(String::as_str) == Some(word)
            })
            .max_by_key(|(_, hit)| hit.page.views)
            .map(|(at, hit)| (at, hit.page.views))
        else {
            return;
        };
        if taken
            || articles().any(|(at, hit)| {
                at != best
                    && hit.page.views * super::SURNAME_MARGIN > views
                    && titled(hit).iter().any(|title_word| title_word == word)
            })
        {
            return;
        }
        name_by_alias(&mut hits[best]);
    }

    /// Names the person a `title` and words of their `name` call when no
    /// article is called the whole query (whose key is `joined`): "sir
    /// isaac newton" is Isaac Newton, "president obama" Barack Obama,
    /// "princess diana" Diana, Princess of Wales, not the song. Every word
    /// of the name must be in the person's title or another name, and an
    /// office ([`super::OFFICES`]) in their title or description. Only
    /// when they are read [`super::SURNAME_MARGIN`] times as much as every
    /// other article that fits and, when they live, every other living one:
    /// "queen elizabeth" is two queens.
    fn name_by_title(&self, title: &str, name: &[String], joined: &str, hits: &mut [PageHit]) {
        let office = super::OFFICES.contains(&title);
        if !office && !super::HONORIFICS.contains(&title) {
            return;
        }
        let elected = super::ELECTED.contains(&title);
        let words = |text: &str| analysis::tokens(&self.words, text);
        let fits = |page: &Page| {
            let names: HashSet<String> = std::iter::once(&page.title)
                .chain(&page.aliases)
                .flat_map(|text| words(text))
                .collect();
            let said = || {
                words(&page.title)
                    .into_iter()
                    .chain(words(page.description.as_deref().unwrap_or("")))
                    .any(|word| word == title || elected && word == "politician")
            };
            page.is_article() && name.iter().all(|word| names.contains(word)) && (!office || said())
        };
        let called =
            |text: &str| analysis::tokens(&self.joined, text).pop().as_deref() == Some(joined);
        if hits.iter().any(|hit| {
            hit.page.is_article()
                && std::iter::once(&hit.page.title)
                    .chain(&hit.page.aliases)
                    .any(|text| called(text))
        }) {
            return;
        }
        let Some((best, views)) = hits
            .iter()
            .enumerate()
            .filter(|(_, hit)| !hit.named && hit.page.is_person() && fits(&hit.page))
            .max_by_key(|(_, hit)| hit.page.views)
            .map(|(at, hit)| (at, hit.page.views))
        else {
            return;
        };
        // Past holders are called by their numbers, so a living one is
        // called by the title and the bare name: "king charles" is
        // Charles III though Charles I is read a third as much.
        let died = |page: &Page| {
            page.facts
                .iter()
                .any(|fact| fact.kind == plumb_core::facts::FactKind::Died)
        };
        let living = !died(&hits[best].page);
        if hits.iter().enumerate().any(|(at, hit)| {
            at != best
                && hit.page.views * super::SURNAME_MARGIN > views
                && fits(&hit.page)
                && !(living && died(&hit.page))
        }) {
            return;
        }
        name_by_alias(&mut hits[best]);
    }

    /// Names the person the query calls by their `name` and then says
    /// `what` they are, as their description does, when nothing is named:
    /// "tom hanks actor" is Tom Hanks, "American actor and filmmaker", and
    /// "mozart composer" Wolfgang Amadeus Mozart. The name is their title
    /// without its qualifier, another name or, alone, the title's last
    /// word. Only when they are read
    /// [`super::SURNAME_MARGIN`] times as much as every other person who
    /// fits: "chris smith footballer" is several.
    fn name_by_description(&self, name: &[String], what: &str, hits: &mut [PageHit]) {
        if name.is_empty() || hits.iter().any(|hit| hit.named) {
            return;
        }
        let key = |text: &str| analysis::tokens(&self.joined, text).pop();
        let Some(called) = key(&name.join(" ")) else {
            return;
        };
        let surname = |page: &Page| match name {
            [word] => analysis::tokens(&self.words, base_title(&page.title)).last() == Some(word),
            _ => false,
        };
        let fits = |page: &Page| {
            page.is_person()
                && (std::iter::once(base_title(&page.title))
                    .chain(page.aliases.iter().map(String::as_str))
                    .any(|text| key(text).as_ref() == Some(&called))
                    || surname(page))
                && analysis::tokens(&self.words, page.description.as_deref().unwrap_or(""))
                    .iter()
                    .any(|word| word == what)
        };
        let Some((best, views)) = hits
            .iter()
            .enumerate()
            .filter(|(_, hit)| fits(&hit.page))
            .max_by_key(|(_, hit)| hit.page.views)
            .map(|(at, hit)| (at, hit.page.views))
        else {
            return;
        };
        if hits.iter().enumerate().any(|(at, hit)| {
            at != best && hit.page.views * super::SURNAME_MARGIN > views && fits(&hit.page)
        }) {
            return;
        }
        name_by_alias(&mut hits[best]);
    }

    /// Names the articles a query of "the" and a name calls without its
    /// "the" (`rest`): "the pope" is Pope, not the book The Pope, and "the
    /// titanic" the ship before the song. Only when they are read
    /// [`super::WITHOUT_THE_MARGIN`] times as much as every article the
    /// whole query names: "the police" and "the strokes" are the bands.
    fn name_without_the(&self, rest: &[String], hits: &mut [PageHit]) {
        let Some(joined) = analysis::tokens(&self.joined, &rest.join(" ")).pop() else {
            return;
        };
        let called = |text: &str| {
            analysis::tokens(&self.joined, text).pop().as_deref() == Some(joined.as_str())
        };
        let most_named = hits
            .iter()
            .filter(|hit| hit.named && hit.page.is_article())
            .map(|hit| hit.page.views)
            .max()
            .unwrap_or(0);
        for hit in hits.iter_mut().filter(|hit| {
            !hit.named
                && hit.page.is_article()
                && hit.page.views >= most_named.saturating_mul(super::WITHOUT_THE_MARGIN)
        }) {
            if std::iter::once(base_title(&hit.page.title))
                .chain(hit.page.aliases.iter().map(String::as_str))
                .any(called)
            {
                name_by_alias(hit);
            }
        }
    }
}

/// Marks `hit` named by the query and scores it as if by an alias.
pub(super) fn name_by_alias(hit: &mut PageHit) {
    hit.named = true;
    hit.score = ALIAS_MATCH * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * hit.popularity);
}

/// Folds the films and shows in `hits` that English Wikipedia has an
/// article on into that article: such a film is listed only when the query
/// asks for it ([`PageHit::whole`]: "inception 2010"), and then as its
/// article when that was found too, which the query then asks for. The
/// article alone covers its title.
fn fold_films(hits: Vec<PageHit>) -> Vec<PageHit> {
    let (films, mut kept): (Vec<PageHit>, Vec<PageHit>) = hits
        .into_iter()
        .partition(|hit| hit.page.is_film_with_article());
    for film in films.into_iter().filter(|film| film.whole) {
        match kept
            .iter_mut()
            .find(|hit| hit.page.is_article() && hit.page.item == film.page.item)
        {
            Some(article) => {
                article.named = true;
                article.whole = true;
                article.score = article.score.max(film.score);
            }
            None => kept.push(film),
        }
    }
    kept
}

/// Most pages whose names have the rarest word of a part of a query looked
/// up as a repository's name ([`PageSearcher::named_inside`]).
const REPO_NAME_PAGES: u64 = 100_000;

/// The parts of a query that may name a page ([`PageSearcher::named_inside`]).
#[derive(Default)]
struct Inside {
    /// Each part's key ([`PageSearcher::span_key`]) and words, the
    /// longest part of a key.
    keys: HashMap<String, Vec<String>>,
    /// Pages found only by a part, left out unless it names them.
    only: HashSet<tantivy::DocAddress>,
}

impl Inside {
    /// The longest part of the query that names `page` in full: the title
    /// or another name of an article, or another name of a docs page, whose
    /// title alone names nothing ("Glossary"). Repositories are told apart
    /// by what they say of themselves ([`PageSearcher::repo_described`]),
    /// and pages of other sets share their titles with too much to be
    /// named so. An article's title without its qualifier names it when
    /// another part says the qualifier: "the chemical element mercury"
    /// names Mercury (element), but "queen tour" not Queen (album).
    fn span(&self, page: &Page, joined: &tantivy::tokenizer::TextAnalyzer) -> Option<&[String]> {
        let key = |name: &str| analysis::tokens(joined, name).pop();
        let names: Vec<&str> = if page.is_article() {
            let qualified = super::sites::title_qualifier(&page.title)
                .and_then(key)
                .is_some_and(|qualifier| self.keys.contains_key(&qualifier));
            std::iter::once(page.title.as_str())
                .chain(qualified.then(|| base_title(&page.title)))
                .chain(page.aliases.iter().map(String::as_str))
                .collect()
        } else if page.set == DOCS_SET {
            page.aliases.iter().map(String::as_str).collect()
        } else {
            return None;
        };
        names
            .into_iter()
            .filter_map(|name| self.keys.get(&key(name)?))
            .max_by_key(|span| span.len())
            .map(Vec::as_slice)
    }
}

/// How far below the article the query names [`site_pages_after_named_article`]
/// puts a site's inner page.
const BELOW_NAMED_ARTICLE: f32 = 0.99;

/// Scores the inner pages of sites (the subpages and reference sets) below
/// the best Wikipedia article the query names. Such a page's popularity is
/// its site's weight over its depth, not how much its subject is read, so
/// any page titled just "Napoleon" or "Plato" outranked the article and
/// took its place among the results, info box and all: a chess engine's
/// page at chessprogramming.org for "napoleon", loc.gov's for "abraham
/// lincoln". A page on the article's own website keeps its score: nps.gov's
/// "Yosemite National Park" is the park's own page.
pub fn site_pages_after_named_article(hits: &mut [PageHit]) {
    let Some(article) = hits
        .iter()
        .filter(|hit| {
            hit.named && hit.page.is_article() && hit.page.item.as_deref().is_some_and(is_item_id)
        })
        .max_by(|a, b| a.score.total_cmp(&b.score))
    else {
        return;
    };
    let (most, own_site) = (
        article.score * BELOW_NAMED_ARTICLE,
        article.page.site.clone(),
    );
    for hit in hits.iter_mut().filter(|hit| hit.page.is_site_page()) {
        if own_site.is_none() || registrable_domain(&hit.page.url) != own_site {
            hit.score = hit.score.min(most);
        }
    }
}

/// Cuts `hits`, best first, to `limit`, keeping the best docs page and the
/// best subpage/reference page among them: the many Stack Overflow questions with the
/// words of "javascript array sort" are more read than MDN's page on it,
/// and would crowd it out.
pub(crate) fn truncate_keeping_inner_pages(hits: &mut Vec<PageHit>, limit: usize) {
    let kept = [
        DOCS_SET,
        SUBPAGES_SET,
        REFERENCE_SET,
        REFERENCE2_SET,
        WANTED_SET,
    ];
    for set in kept {
        if hits.len() <= limit || limit == 0 || hits[..limit].iter().any(|h| h.page.set == set) {
            continue;
        }
        let Some(best) = hits.iter().position(|h| h.page.set == set) else {
            continue;
        };
        // In place of the last hit that is not one kept already.
        let Some(last) = (0..limit)
            .rev()
            .find(|&i| !kept.contains(&hits[i].page.set.as_str()))
        else {
            continue;
        };
        let hit = hits.remove(best);
        hits.insert(last, hit);
    }
    hits.truncate(limit);
}

pub(crate) fn scoped_query(query: &dyn Query, scope: Option<&dyn Query>) -> Box<dyn Query> {
    match scope {
        Some(scope) => Box::new(BooleanQuery::new(vec![
            (Occur::Must, query.box_clone()),
            (Occur::Must, scope.box_clone()),
        ])),
        None => query.box_clone(),
    }
}
