//! The index of single pages ("page sets", starting with Wikipedia
//! articles), searched next to the sites index: "marie curie" finds the
//! article Marie Curie, which no site is named.
//!
//! Pages are found by name only: their title and the other titles that
//! lead to them (aliases), plus a little of their description. A page is
//! ranked by how well the query names it and by how often it is read:
//!
//! `score = name * (1 - POPULARITY_SHARE + POPULARITY_SHARE * popularity)`
//!
//! - `name` is 1 when the whole query is the page's title, with or without
//!   a qualifier in brackets ("python" for "Python (programming
//!   language)"), [`ALIAS_MATCH`] when it is an alias, and otherwise the
//!   share of the title's words the query has, times [`PARTIAL_MATCH`],
//!   when the query has every word of the title, or of an alias, or the
//!   title has every word of the query.
//! - `popularity` is `ln(1 + views) / ln(1 + most views)`, against the
//!   most read page of the same set, so sets that count differently
//!   (Wikipedia's page views, GitHub's stars) compare fairly.
//!
//! How pages and sites are listed together is up to the caller; see
//! [`PageHit::named`].

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::article::{article_url, Article};
use plumb_core::{adult_level, host_of, normalize_text, AdultLevel, Operators, SafeSearch};
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, STORED,
};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, Term};

use crate::analysis::{self, JOINED_ANALYZER, STEMMED_ANALYZER, WORDS_ANALYZER};
use crate::replace::Staging;

/// `name` of a page one of whose aliases the query is.
pub const ALIAS_MATCH: f32 = 0.9;
/// Most `name` of a page whose title has some of the query's words.
pub const PARTIAL_MATCH: f32 = 0.6;
/// How much popularity counts, against how well the query names the page.
pub const POPULARITY_SHARE: f32 = 0.5;
/// Pages whose words match that are looked at, most matching first.
const CANDIDATES: usize = 200;
/// Fewest words (stemmed, without the most common ones) of a query that
/// finds questions by their words.
pub const QUESTION_QUERY_WORDS: usize = 3;
/// Least share of those words a question's title and tags must have.
pub const QUESTION_SHARE: f32 = 0.75;
/// Least share of a question title's stemmed words a query that has all of
/// the question's own must have to ask the question as a whole.
pub const QUESTION_TITLE_SHARE: f32 = 0.5;

/// A single page that can be a result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    /// The page set it belongs to, e.g. `wikipedia-en`.
    pub set: String,
    pub url: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The registrable domain of the site this page is about, when it has
    /// one: a result for that site carries the page instead of both being
    /// listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// How often it was read (or a set's own measure of popularity).
    pub views: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// The Wikidata item a Wikipedia article is about (`Q937`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// The item's official profiles (a YouTube channel, an X account).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<plumb_core::profiles::Profile>,
    /// The item's official website when it is a subdomain or an inner page
    /// of [`Page::site`]: `https://music.youtube.com/` for YouTube Music.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
}

impl Page {
    /// The Wikipedia article `article` of Wikipedia in `lang`.
    pub fn from_article(lang: &str, article: Article) -> Self {
        Page {
            set: format!("wikipedia-{lang}"),
            url: article_url(lang, &article.title),
            title: article.title,
            description: article.description,
            site: article.site,
            views: article.views,
            aliases: article.aliases,
            item: article.item,
            profiles: article.profiles,
            website: article.website,
        }
    }

    /// The GitHub repository `repo`, written as an article: its title is
    /// `owner/name`, its views its stars, its site its homepage's domain.
    pub fn from_repo(repo: Article) -> Self {
        Page {
            set: GITHUB_SET.to_string(),
            url: format!("https://github.com/{}", repo.title),
            title: repo.title,
            description: repo.description,
            site: repo.site,
            views: repo.views,
            aliases: repo.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
        }
    }

    /// The Stack Overflow question `question`, written as an article whose
    /// item is the question's id and whose description is its tags.
    pub fn from_question(question: Article) -> Self {
        Page {
            set: STACKOVERFLOW_SET.to_string(),
            url: format!(
                "https://stackoverflow.com/questions/{}",
                question.item.as_deref().unwrap_or("")
            ),
            title: question.title,
            description: question.description,
            site: None,
            views: question.views,
            aliases: question.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
        }
    }

    /// The Open Library work `book`, written as an article whose item is
    /// the work id (`OL45804W`).
    pub fn from_book(book: Article) -> Self {
        Page {
            set: BOOKS_SET.to_string(),
            url: format!(
                "https://openlibrary.org/works/{}",
                book.item.as_deref().unwrap_or("")
            ),
            title: book.title,
            description: book.description,
            site: None,
            views: book.views,
            aliases: book.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
        }
    }

    /// The paper `paper`, written as an article whose item is its DOI or
    /// else its OpenAlex id, and whose views are its citations.
    pub fn from_paper(paper: Article) -> Self {
        let item = paper.item.as_deref().unwrap_or("");
        let url = if item.starts_with("10.") {
            format!("https://doi.org/{item}")
        } else {
            format!("https://openalex.org/{item}")
        };
        Page {
            set: PAPERS_SET.to_string(),
            url,
            title: paper.title,
            description: paper.description,
            site: None,
            views: paper.views,
            aliases: paper.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
        }
    }

    /// The page of the set `set` written as `article` in its articles
    /// file, `None` for a set without a reader.
    pub fn from_set(set: &str, article: Article) -> Option<Self> {
        Some(match set {
            GITHUB_SET => Page::from_repo(article),
            STACKOVERFLOW_SET => Page::from_question(article),
            BOOKS_SET => Page::from_book(article),
            PAPERS_SET => Page::from_paper(article),
            _ => Page::from_article(set.strip_prefix("wikipedia-")?, article),
        })
    }

    /// The words a question or paper is found by besides its title: its
    /// title, and a question's tags. `None` for pages of other sets, found
    /// by their names only.
    pub fn topic(&self) -> Option<String> {
        if self.set == PAPERS_SET {
            return Some(self.title.clone());
        }
        (self.set == STACKOVERFLOW_SET).then(|| match &self.description {
            Some(tags) => format!("{} {tags}", self.title),
            None => self.title.clone(),
        })
    }

    /// Whether the page may be listed before every site. Books and papers
    /// share their titles with too much ("Python", "Apple") to.
    pub fn may_lead(&self) -> bool {
        self.set != BOOKS_SET && self.set != PAPERS_SET
    }

    /// The name of the set people see: "Wikipedia".
    pub fn set_name(&self) -> &str {
        if self.set.starts_with("wikipedia-") {
            "Wikipedia"
        } else if self.set == GITHUB_SET {
            "GitHub"
        } else if self.set == STACKOVERFLOW_SET {
            "Stack Overflow"
        } else if self.set == BOOKS_SET {
            "Open Library"
        } else if self.set == PAPERS_SET {
            "OpenAlex"
        } else {
            &self.set
        }
    }

    /// The language the page is in, when its set says: `en` for
    /// English Wikipedia, GitHub and Stack Overflow.
    pub fn language(&self) -> Option<&str> {
        if let Some(lang) = self.set.strip_prefix("wikipedia-") {
            Some(lang)
        } else if self.set == GITHUB_SET || self.set == STACKOVERFLOW_SET {
            Some("en")
        } else {
            None
        }
    }

    /// The site whose icon marks the page: wikipedia.org, github.com.
    pub fn set_domain(&self) -> &str {
        if self.set == GITHUB_SET {
            "github.com"
        } else if self.set == STACKOVERFLOW_SET {
            "stackoverflow.com"
        } else if self.set == BOOKS_SET {
            "openlibrary.org"
        } else if self.set == PAPERS_SET {
            "openalex.org"
        } else {
            "wikipedia.org"
        }
    }
}

/// The set of GitHub repositories.
pub const GITHUB_SET: &str = "github";
/// The set of Stack Overflow questions.
pub const STACKOVERFLOW_SET: &str = "stackoverflow";
/// The set of books, from Open Library.
pub const BOOKS_SET: &str = "books";
/// The set of scholarly papers, from OpenAlex.
pub const PAPERS_SET: &str = "papers";
/// How much a book's or paper's score counts against an article's of the
/// same name: "dune" lists the article on the novel before the book.
pub const SHELF_WEIGHT: f32 = 0.8;
/// What a repository's score is weighed by, so that the article named like
/// it comes first ("sonnet", "apollo 11").
pub const REPO_WEIGHT: f32 = 0.8;

/// [`PageHit::popularity`] is kept in the index as a whole number of
/// millionths.
const POPULARITY_SCALE: f32 = 1_000_000.0;

/// A page found for a query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageHit {
    pub page: Page,
    pub score: f32,
    /// The whole query is the page's title or one of its aliases.
    pub named: bool,
    /// How read the page is, in `0..=1` on a log scale where the most read
    /// page of its set is 1, to weigh against a site's
    /// [`crate::Hit::link_score`].
    #[serde(default)]
    pub popularity: f32,
    /// The query asks for this page and nothing else: it holds all of a
    /// question's main words ("undo last git commit"), or a book's title
    /// with its author or the word "book" ("dune frank herbert"). Such a
    /// page may come before the sites.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub whole: bool,
}

struct Fields {
    words: Field,
    keys: Field,
    /// Stemmed words of a question's title and tags; empty for other pages.
    topic: Field,
    popularity: Field,
    page: Field,
}

fn schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let words = builder.add_text_field(
        "words",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(WORDS_ANALYZER)
                .set_index_option(IndexRecordOption::WithFreqs),
        ),
    );
    let keys = builder.add_text_field(
        "keys",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(JOINED_ANALYZER)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let topic = builder.add_text_field(
        "topic",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(STEMMED_ANALYZER)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let popularity = builder.add_u64_field("popularity", FAST | STORED);
    let page = builder.add_text_field("page", STORED);
    (
        builder.build(),
        Fields {
            words,
            keys,
            topic,
            popularity,
            page,
        },
    )
}

/// `title` without a trailing qualifier in brackets: `Python (programming
/// language)` -> `Python`.
fn base_title(title: &str) -> &str {
    match title.rfind(" (") {
        Some(i) if title.ends_with(')') && i > 0 => &title[..i],
        _ => title,
    }
}

/// Words of a short description saying a page is about an organization,
/// a product or a service, which has a website of its own, rather than a
/// person, place, idea or work.
const ORGANIZATION_WORDS: &[&str] = &[
    "agency",
    "airline",
    "app",
    "application",
    "bank",
    "brand",
    "broker",
    "brokerage",
    "business",
    "card",
    "chain",
    "company",
    "conglomerate",
    "cooperative",
    "corporation",
    "exchange",
    "firm",
    "foundation",
    "framework",
    "insurer",
    "library",
    "manufacturer",
    "marketplace",
    "nonprofit",
    "organisation",
    "organization",
    "pharmacy",
    "platform",
    "program",
    "programme",
    "protocol",
    "provider",
    "retailer",
    "service",
    "software",
    "specification",
    "standard",
    "startup",
    "subsidiary",
    "union",
    "website",
];

/// Whether Wikipedia's short description of a page says it is about an
/// organization, product or service ("American auto insurance company",
/// "Open-source software framework"), see [`ORGANIZATION_WORDS`].
fn describes_an_organization(description: Option<&str>) -> bool {
    description.is_some_and(|text| {
        text.split(|c: char| !c.is_alphanumeric())
            .map(str::to_lowercase)
            .any(|word| {
                let one = word.strip_suffix('s').unwrap_or(&word);
                ORGANIZATION_WORDS.contains(&word.as_str()) || ORGANIZATION_WORDS.contains(&one)
            })
    })
}

/// `text` lowercased with only its letters and digits.
fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `domain` is a government's (`.gov`, `.gov.uk`, `.mil`).
fn is_government(domain: &str) -> bool {
    domain
        .split('.')
        .skip(1)
        .any(|label| label == "gov" || label == "mil")
}

/// Whether `domain`'s first label spells the page's title without its
/// qualifier, or that title's first words: cvs.com for "CVS Pharmacy",
/// capitalone.com for "Capital One", tauri.app for "Tauri (software
/// framework)", navyfederal.org for "Navy Federal Credit Union". Such a site is most likely what the page is about.
fn site_is_titled(domain: &str, title: &str) -> bool {
    let label = squash(domain.split('.').next().unwrap_or(""));
    if label.is_empty() {
        return false;
    }
    // The title's first words: "navyfederal" for "Navy Federal Credit
    // Union".
    let mut lead = String::new();
    for word in base_title(title).split_whitespace() {
        lead.push_str(&squash(word));
        if lead == label {
            return true;
        }
        if lead.len() >= label.len() {
            return false;
        }
    }
    false
}

/// What [`build_page_index`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageIndexStats {
    pub pages: u64,
    pub most_views: u64,
}

/// Builds the page index of `pages` in `dir`, replacing any there.
pub fn build_page_index(
    dir: &Path,
    pages: impl IntoIterator<Item = Page>,
) -> Result<PageIndexStats> {
    let staging = Staging::new(dir)?;
    let (schema, fields) = schema();
    let index = Index::create_in_dir(staging.path(), schema)
        .with_context(|| format!("creating the page index in {}", dir.display()))?;
    analysis::register(index.tokenizers());
    let mut writer = index
        .writer_with_num_threads(1, 64 << 20)
        .context("opening the page index for writing")?;
    let mut stats = PageIndexStats::default();
    // Sets come most read first, so the first page of a set is its most
    // read.
    let mut most_by_set: std::collections::HashMap<String, u64> = Default::default();
    for page in pages {
        let most = most_by_set
            .entry(page.set.clone())
            .and_modify(|most| *most = (*most).max(page.views))
            .or_insert(page.views);
        let popularity = if *most == 0 {
            0.0
        } else {
            ((page.views as f32).ln_1p() / (*most as f32).ln_1p()).min(1.0)
        };
        let mut document = TantivyDocument::default();
        document.add_text(fields.words, &page.title);
        for alias in &page.aliases {
            document.add_text(fields.words, alias);
        }
        if let Some(topic) = page.topic() {
            document.add_text(fields.topic, topic);
        }
        document.add_text(fields.keys, &page.title);
        let base = base_title(&page.title);
        if base != page.title {
            document.add_text(fields.keys, base);
        }
        document.add_u64(
            fields.popularity,
            (popularity * POPULARITY_SCALE).round() as u64,
        );
        document.add_text(fields.page, serde_json::to_string(&page)?);
        writer.add_document(document)?;
        stats.pages += 1;
        stats.most_views = stats.most_views.max(page.views);
    }
    writer.commit().context("writing the page index")?;
    drop(writer);
    std::fs::write(
        staging.path().join("pages.json"),
        serde_json::to_vec(&stats)?,
    )?;
    staging.install()?;
    Ok(stats)
}

/// Searches a page index.
pub struct PageSearcher {
    reader: IndexReader,
    fields: Fields,
    words: TextAnalyzer,
    joined: TextAnalyzer,
    stemmed: TextAnalyzer,
    stats: PageIndexStats,
}

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
        })
    }

    pub fn num_pages(&self) -> u64 {
        self.stats.pages
    }

    /// The best `limit` pages for `query`, best first.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<PageHit>> {
        let words = analysis::tokens(&self.words, query);
        let Some(joined) = analysis::tokens(&self.joined, query).pop() else {
            return Ok(Vec::new());
        };
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(
            Occur::Should,
            Box::new(TermQuery::new(
                Term::from_field_text(self.fields.keys, &joined),
                IndexRecordOption::Basic,
            )),
        )];
        // Pages with every word of the query.
        let every_word: Vec<(Occur, Box<dyn Query>)> = words
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
            .collect();
        clauses.push((Occur::Should, Box::new(BooleanQuery::new(every_word))));
        // Books named by their title and "book": "dune book".
        if let Some((title, last)) = query.trim().rsplit_once(char::is_whitespace) {
            if matches!(last.to_lowercase().as_str(), "book" | "novel") {
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
            .search(&BooleanQuery::new(clauses), &by_popularity())?
            .into_iter()
            .map(|(_, address)| address)
            .collect();
        // Questions with most of the query's words, searched apart so they
        // never crowd out pages the query names.
        let stems = self.question_words(query);
        if stems.len() >= QUESTION_QUERY_WORDS {
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
            for (_, address) in searcher.search(&most_words, &by_popularity())? {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        let mut hits = Vec::new();
        for address in addresses {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            let (mut name, mut named) = self.name_match(&page, query, &joined, &query_words);
            let mut whole = false;
            if self.book_match(&page, &words) {
                (name, named, whole) = (name.max(ALIAS_MATCH), true, true);
            } else if !named {
                let (question, asked) = self.question_match(&page, &stems);
                name = name.max(question);
                whole = asked;
            }
            if name <= 0.0 {
                continue;
            }
            let popularity = document
                .get_first(self.fields.popularity)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as f32
                / POPULARITY_SCALE;
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
            });
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit);
        Ok(hits)
    }

    /// The different stemmed words of `query`, as questions are searched.
    fn question_words(&self, query: &str) -> Vec<String> {
        let mut stems = analysis::tokens(&self.stemmed, query);
        let mut seen = HashSet::new();
        stems.retain(|stem| seen.insert(stem.clone()));
        stems
    }

    /// How well a question's words cover the query's stemmed words
    /// `stems`: [`PARTIAL_MATCH`] times the share they have, when that is
    /// at least [`QUESTION_SHARE`] of at least [`QUESTION_QUERY_WORDS`].
    /// And whether the query asks the question as a whole: it has all of
    /// the query's words, and the query at least [`QUESTION_TITLE_SHARE`]
    /// of its title's.
    fn question_match(&self, page: &Page, stems: &[String]) -> (f32, bool) {
        if stems.len() < QUESTION_QUERY_WORDS {
            return (0.0, false);
        }
        let Some(topic) = page.topic() else {
            return (0.0, false);
        };
        let words: HashSet<String> = analysis::tokens(&self.stemmed, &topic)
            .into_iter()
            .collect();
        let share =
            stems.iter().filter(|stem| words.contains(*stem)).count() as f32 / stems.len() as f32;
        if share < QUESTION_SHARE {
            return (0.0, false);
        }
        let title: HashSet<String> = analysis::tokens(&self.stemmed, &page.title)
            .into_iter()
            .collect();
        let asked = share >= 1.0
            && !title.is_empty()
            && title.iter().filter(|word| stems.contains(word)).count() as f32
                >= QUESTION_TITLE_SHARE * title.len() as f32;
        (PARTIAL_MATCH * share, asked)
    }

    /// Whether the query `words` are a book's title followed by words of
    /// its author's name or by "book" or "novel": "dune frank herbert",
    /// "the great gatsby book".
    fn book_match(&self, page: &Page, words: &[String]) -> bool {
        if page.set != BOOKS_SET {
            return false;
        }
        let author = page
            .description
            .as_deref()
            .and_then(|d| d.strip_prefix("Book by "))
            .map(|d| d.rsplit_once(", ").map_or(d, |(name, _)| name))
            .unwrap_or("");
        let author: HashSet<String> = analysis::tokens(&self.words, author).into_iter().collect();
        // The title, or the title without its subtitle: "Frankenstein" for
        // "Frankenstein; or, The Modern Prometheus".
        let short = page.title.split([':', ';']).next().unwrap_or("");
        [page.title.as_str(), short].into_iter().any(|title| {
            let title = analysis::tokens(&self.words, title);
            let rest = match words.strip_prefix(title.as_slice()) {
                Some(rest) if !title.is_empty() && !rest.is_empty() => rest,
                _ => return false,
            };
            rest.iter().all(|word| author.contains(word))
                || matches!(rest, [word] if word == "book" || word == "novel")
        })
    }

    fn name_match(
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
        if spelled(&page.title) == spelled(raw_query) {
            return (1.0, true);
        }
        // "Mozart (film)" and "Mozart!" are no better a match for "mozart"
        // than the redirect "Mozart" to "Wolfgang Amadeus Mozart":
        // popularity decides between them.
        if key(&page.title) == joined
            || key(base_title(&page.title)) == joined
            || page.aliases.iter().any(|alias| key(alias) == joined)
        {
            return (ALIAS_MATCH, true);
        }
        let mut best = 0.0f32;
        for name in
            std::iter::once(base_title(&page.title)).chain(page.aliases.iter().map(String::as_str))
        {
            let words: HashSet<String> = analysis::tokens(&self.words, name).into_iter().collect();
            if words.is_empty() {
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
        (best, false)
    }
}

/// Most pages listed on their own among the sites.
pub const MAX_PAGES_LISTED: usize = 2;
/// Least score of a page the query does not name in full that is listed.
pub const MIN_PARTIAL_SCORE: f32 = 0.3;
/// Where a page the query only names in part is listed: after this many
/// sites.
pub const PARTIAL_AFTER: usize = 3;

/// A page and where it goes among the site results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacedPage {
    #[serde(flatten)]
    pub hit: PageHit,
    /// Shown under the result for this domain, the site the page is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub under: Option<String>,
    /// Otherwise listed before the site result at this position (after all
    /// of them when it is past the end).
    pub at: usize,
}

/// Where `pages` (best first) go among the site results `sites`:
///
/// - A page about one of the sites (its [`Page::site`]) goes under that
///   site's result rather than in a place of its own: "python" lists
///   python.org with the article on the Python language under it.
/// - Of the others, at most [`MAX_PAGES_LISTED`] are listed: pages the
///   query names in full, and others scoring at least
///   [`MIN_PARTIAL_SCORE`]; only one when the best site is named by the
///   query too.
/// - A named page comes right after the best site when that site is
///   probably the website of what the query names: a named page found
///   for the query is about an organization or product
///   ([`describes_an_organization`]) and the site is called after it
///   ("tauri" finds the article "Tauri (software framework)", so tauri.app
///   stays first; "robinhood" finds "Robinhood Markets", so robinhood.com
///   stays above the outlaw). It also comes after an official website
///   ([`crate::Hit::official`]) or one better known than the page is read
///   ([`PageHit::popularity`] against [`crate::Hit::link_score`]).
///   Otherwise the page comes first: "marie curie" lists the article
///   before mariecurie.org. Pages named only in part come after
///   [`PARTIAL_AFTER`] sites, questions and papers with most of the
///   query's words after the best site.
/// - A page the query asks for as a whole ([`PageHit::whole`]: a question,
///   or a book with its author) comes first, unless the best site is
///   named by the query.
/// - A site called exactly what was searched for stays first when the
///   best page named so is an organization or a repository: "us bank"
///   lists usbank.com before the article "U.S. Bancorp".
pub fn place_pages(query: &str, sites: &[crate::Hit], pages: Vec<PageHit>) -> Vec<PlacedPage> {
    let site_named = sites.first().is_some_and(|hit| hit.named);
    let query_word = squash(query);
    let organizations_site = sites.first().is_some_and(|site| {
        let label = squash(site.domain.split('.').next().unwrap_or(""));
        // A site of government ("fafsa.gov") called after the page.
        let government = is_government(&site.domain);
        // The site is called what was searched for, and the best page
        // named so is an organization or a project: usbank.com for "us
        // bank" (the article "U.S. Bancorp"), regex101.com for the
        // repository firasdib/Regex101.
        let called = label == query_word
            && pages.iter().find(|page| page.named).is_some_and(|page| {
                page.page.set == GITHUB_SET
                    || describes_an_organization(page.page.description.as_deref())
            });
        called
            || pages.iter().any(|page| {
                page.named
                    && site_is_titled(&site.domain, &page.page.title)
                    && (government
                    || describes_an_organization(page.page.description.as_deref())
                    // "robinhood" spells robinhood.com, not "Robin Hood".
                    || (!query.trim().contains(' ')
                        && label == query_word
                        && base_title(&page.page.title).contains(' ')))
            })
    });
    // A named page goes first only when the best site may be a namesake.
    let page_first = |page: &PageHit| match sites.first() {
        None => true,
        Some(site) => !organizations_site && !site.official && page.popularity > site.link_score,
    };
    let most = if site_named { 1 } else { MAX_PAGES_LISTED };
    let mut placed: Vec<PlacedPage> = Vec::new();
    let mut listed = 0;
    for hit in pages {
        // A page named like a better one of its set already listed is a
        // namesake of it: once "Eiffel Tower" is under toureiffel.paris, "Eiffel Tower
        // (Six Flags)" only comes after three sites.
        let namesake = hit.named
            && placed.iter().any(|p| {
                p.hit.named
                    && p.hit.page.set == hit.page.set
                    && page_key(base_title(&p.hit.page.title))
                        == page_key(base_title(&hit.page.title))
            });
        if let Some(site) = hit.page.site.as_deref() {
            if sites.iter().any(|s| s.domain == site) {
                if !placed.iter().any(|p| p.under.as_deref() == Some(site)) {
                    placed.push(PlacedPage {
                        under: Some(site.to_string()),
                        at: 0,
                        hit,
                    });
                }
                continue;
            }
        }
        if listed == most || !(hit.named || hit.score >= MIN_PARTIAL_SCORE) {
            continue;
        }
        let at = if hit.whole {
            // Only the best of them leads: another edition of the book
            // comes after the best site.
            let led = placed.iter().any(|p| p.hit.whole);
            usize::from(site_named || led)
        } else if !hit.named && hit.page.topic().is_some() {
            // A question or paper with most of the query's words.
            1
        } else if !hit.named || namesake {
            PARTIAL_AFTER
        } else if hit.page.may_lead() && page_first(&hit) {
            0
        } else {
            1
        };
        listed += 1;
        placed.push(PlacedPage {
            at: at.min(sites.len()),
            under: None,
            hit,
        });
    }
    placed
}

/// Pages looked at for a query with search operators, before they narrow
/// them.
pub const OPERATOR_PAGES: usize = 100;
/// Most pages listed for a `site:` query.
pub const MAX_SITE_PAGES: usize = 10;

/// Whether the search operators `ops` allow `page`: its address's host
/// and its title, description and other names.
pub fn operators_allow(ops: &Operators, page: &Page) -> bool {
    let host = host_of(&page.url).unwrap_or_default();
    let texts = [page.title.as_str()]
        .into_iter()
        .chain(page.description.as_deref())
        .chain(page.aliases.iter().map(String::as_str));
    ops.allows(&host, texts)
}

/// Whether the searcher's `options` allow `page`: one in another
/// language than [`crate::SearchOptions::language`] is left out, and
/// strict safe search leaves out pages whose title or description is
/// suggestive.
pub fn options_allow(options: &crate::SearchOptions, page: &Page) -> bool {
    if let (Some(wanted), Some(language)) = (&options.language, page.language()) {
        if wanted != language {
            return false;
        }
    }
    if options.safe == SafeSearch::Strict {
        let texts = [page.title.as_str()]
            .into_iter()
            .chain(page.description.as_deref());
        if adult_level("", texts) != AdultLevel::None {
            return false;
        }
    }
    true
}

/// [`place_pages`] for a query with search operators `ops`, of the pages
/// they allow. A `site:` query lists up to [`MAX_SITE_PAGES`] pages on
/// that site after the sites, best first: "site:wikipedia.org einstein"
/// lists the articles.
pub fn place_operator_pages(
    ops: &Operators,
    sites: &[crate::Hit],
    pages: Vec<PageHit>,
) -> Vec<PlacedPage> {
    let pages = pages
        .into_iter()
        .filter(|hit| operators_allow(ops, &hit.page));
    if ops.sites.is_empty() {
        return place_pages(&ops.words, sites, pages.collect());
    }
    pages
        .take(MAX_SITE_PAGES)
        .map(|hit| PlacedPage {
            hit,
            under: None,
            at: sites.len(),
        })
        .collect()
}

/// Normalized form of `text` as page keys compare it, for tests and tools.
pub fn page_key(text: &str) -> String {
    normalize_text(text).replace(' ', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(title: &str, views: u64, aliases: &[&str]) -> Page {
        Page::from_article(
            "en",
            Article {
                title: title.into(),
                views,
                aliases: aliases.iter().map(|a| a.to_string()).collect(),
                ..Article::default()
            },
        )
    }

    fn searcher(pages: &[Page]) -> (tempfile::TempDir, PageSearcher) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pages");
        build_page_index(&path, pages.to_vec()).unwrap();
        let searcher = PageSearcher::open(&path).unwrap();
        (dir, searcher)
    }

    fn titles(hits: &[PageHit]) -> Vec<&str> {
        hits.iter().map(|h| h.page.title.as_str()).collect()
    }

    #[test]
    fn whole_titles_win_then_popularity() {
        let (_dir, s) = searcher(&[
            page("Marie Curie", 80_000, &["Madame Curie"]),
            page("Pierre Curie", 30_000, &[]),
            page("Curie (unit)", 2_000, &[]),
            page("Python (programming language)", 900_000, &[]),
            page("Python (genus)", 20_000, &["Pythonidae"]),
            page("Monty Python", 200_000, &[]),
        ]);
        let hits = s.search("Marie Curie", 5).unwrap();
        assert_eq!(titles(&hits)[0], "Marie Curie");
        assert!(hits[0].named);
        let hits = s.search("python", 5).unwrap();
        assert_eq!(
            titles(&hits)[..3],
            [
                "Python (programming language)",
                "Python (genus)",
                "Monty Python"
            ]
        );
        assert!(!hits[2].named);
        let hits = s.search("madame curie", 5).unwrap();
        assert_eq!(titles(&hits)[0], "Marie Curie");
        assert!(hits[0].named);
        assert!(s.search("pythonidae", 1).unwrap()[0].named);
    }

    #[test]
    fn pages_follow_the_language_and_strict_safe_search() {
        let article = page("Einstein", 1, &[]);
        let mut book = page("Sexy beasts", 1, &[]);
        book.set = BOOKS_SET.into();
        let en = crate::SearchOptions {
            language: Some("en".into()),
            ..crate::SearchOptions::default()
        };
        assert!(options_allow(&en, &article));
        assert!(options_allow(&en, &book), "books do not say their language");
        let de = crate::SearchOptions {
            language: Some("de".into()),
            ..crate::SearchOptions::default()
        };
        assert!(!options_allow(&de, &article));
        assert!(options_allow(&crate::SearchOptions::default(), &book));
        let strict = crate::SearchOptions {
            safe: SafeSearch::Strict,
            ..crate::SearchOptions::default()
        };
        assert!(!options_allow(&strict, &book));
        assert!(options_allow(&strict, &article));
    }

    #[test]
    fn operators_pick_and_list_pages() {
        let mut repo = Page::from_repo(Article {
            title: "python/cpython".into(),
            views: 60_000,
            ..Article::default()
        });
        repo.description = Some("The Python programming language".into());
        let (_dir, s) = searcher(&[
            page("Python (programming language)", 900_000, &[]),
            page("Python (genus)", 20_000, &["Pythonidae"]),
            page("Monty Python", 200_000, &[]),
            repo,
        ]);
        let place = |query: &str| {
            let ops = Operators::parse(query);
            let found = s.search(&ops.words, OPERATOR_PAGES).unwrap();
            let placed = place_operator_pages(&ops, &[site("python.org", false)], found);
            placed
                .into_iter()
                .map(|p| (p.hit.page.title, p.at))
                .collect::<Vec<_>>()
        };
        let on_github = place("python site:github.com");
        assert_eq!(on_github, [("python/cpython".to_string(), 1)]);
        let on_wikipedia = place("python site:en.wikipedia.org");
        assert_eq!(on_wikipedia.len(), 3);
        assert!(on_wikipedia.iter().all(|(_, at)| *at == 1));
        let without = place("python -monty -site:github.com");
        assert!(without
            .iter()
            .all(|(t, _)| t != "Monty Python" && t != "python/cpython"));
    }

    #[test]
    fn whole_questions_lead() {
        let question = Page::from_question(Article {
            title: "How do I delete a Git branch locally and remotely?".into(),
            description: Some("git, version-control, git-branch".into()),
            item: Some("2003505".into()),
            views: 12_000_000,
            ..Article::default()
        });
        let (_dir, s) = searcher(&[question]);
        let sites = [known_site("git-scm.com", false, 0.9)];
        let hits = s
            .search("delete a git branch locally and remotely", 5)
            .unwrap();
        assert!(hits[0].whole);
        assert_eq!(
            place_pages("delete a git branch locally and remotely", &sites, hits)[0].at,
            0
        );
        // Most of the query's words: after the best site.
        let hits = s.search("delete git branch remotely fast", 5).unwrap();
        assert!(!hits[0].whole);
        assert_eq!(
            place_pages("delete git branch remotely fast", &sites, hits)[0].at,
            1
        );
    }

    #[test]
    fn sites_called_like_organizations_stay_first() {
        let mut bank = page("U.S. Bancorp", 900_000, &["US Bank"]);
        bank.description = Some("American bank holding company".into());
        let repo = Page::from_repo(Article {
            title: "firasdib/Regex101".into(),
            views: 5_000,
            aliases: vec!["Regex101".into()],
            ..Article::default()
        });
        let (_dir, s) = searcher(&[bank, repo, page("Sonnet", 300_000, &[])]);
        for (query, site) in [("us bank", "usbank.com"), ("regex101", "regex101.com")] {
            let hits = s.search(query, 5).unwrap();
            assert!(hits[0].named, "{query}");
            let placed = place_pages(query, &[known_site(site, false, 0.0)], hits);
            assert_eq!(placed[0].at, 1, "{query}");
        }
        // A site called like a person or a work is not first for it.
        let mut curie = page("Marie Curie", 900_000, &[]);
        curie.description = Some("Polish-French physicist and chemist".into());
        let placed = place_pages(
            "marie curie",
            &[known_site("mariecurie.org", false, 0.0)],
            vec![PageHit {
                page: curie,
                score: 1.0,
                named: true,
                popularity: 0.9,
                whole: false,
            }],
        );
        assert_eq!(placed[0].at, 0);
    }

    #[test]
    fn articles_come_before_repositories_named_alike() {
        let repo = Page::from_repo(Article {
            title: "google-deepmind/sonnet".into(),
            views: 9_000,
            aliases: vec!["sonnet".into()],
            ..Article::default()
        });
        let (_dir, s) = searcher(&[repo, page("Sonnet", 30_000, &[])]);
        let hits = s.search("sonnet", 5).unwrap();
        assert_eq!(titles(&hits), ["Sonnet", "google-deepmind/sonnet"]);
    }

    #[test]
    fn partial_names_score_lower() {
        let (_dir, s) = searcher(&[page("Marie Curie", 80_000, &[])]);
        let hit = &s.search("curie", 1).unwrap()[0];
        assert!(!hit.named);
        assert!(hit.score < 0.5, "{}", hit.score);
        assert!(s.search("marie antoinette", 1).unwrap().is_empty());
        assert!(s.search("", 1).unwrap().is_empty());
    }

    fn site(domain: &str, named: bool) -> crate::Hit {
        known_site(domain, named, 1.0)
    }

    fn known_site(domain: &str, named: bool, link_score: f32) -> crate::Hit {
        crate::Hit {
            domain: domain.into(),
            url: format!("https://{domain}/"),
            title: None,
            description: None,
            score: 1.0,
            text_score: 1.0,
            link_score,
            country: None,
            named,
            official: false,
            key_pages: Vec::new(),
        }
    }

    fn found(title: &str, site: Option<&str>, named: bool, score: f32) -> PageHit {
        let mut page = page(title, 1, &[]);
        page.site = site.map(str::to_string);
        PageHit {
            page,
            score,
            named,
            popularity: score,
            whole: false,
        }
    }

    #[test]
    fn main_articles_beat_their_namesakes() {
        let (_dir, s) = searcher(&[
            page("Albert Einstein", 400_000, &[]),
            page("Albert Einstein (album)", 900, &[]),
            page("Wolfgang Amadeus Mozart", 200_000, &["Mozart"]),
            page("Mozart (film)", 3_000, &[]),
            page("Mozart!", 5_000, &[]),
        ]);
        let hits = s.search("albert einstein", 5).unwrap();
        assert_eq!(titles(&hits)[0], "Albert Einstein");
        let hits = s.search("mozart", 5).unwrap();
        assert_eq!(titles(&hits)[0], "Wolfgang Amadeus Mozart");
        // Once the main article is under its site, a namesake waits until
        // after the sites.
        let sites = [
            known_site("toureiffel.paris", false, 0.5),
            site("a.com", false),
            site("b.com", false),
            site("c.com", false),
        ];
        let placed = place_pages(
            "eiffel tower",
            &sites,
            vec![
                found("Eiffel Tower", Some("toureiffel.paris"), true, 0.9),
                found("Eiffel Tower (Six Flags)", None, true, 0.6),
            ],
        );
        assert_eq!(placed[0].under.as_deref(), Some("toureiffel.paris"));
        assert_eq!(placed[1].at, PARTIAL_AFTER);
    }

    #[test]
    fn pages_about_a_listed_site_go_under_it() {
        let sites = [site("python.org", true), site("pythonanywhere.com", false)];
        let placed = place_pages(
            "",
            &sites,
            vec![
                found(
                    "Python (programming language)",
                    Some("python.org"),
                    true,
                    0.9,
                ),
                found("Python (genus)", None, true, 0.7),
                found("Monty Python", None, false, 0.5),
            ],
        );
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[0].under.as_deref(), Some("python.org"));
        // The site is named, so one page, and the namesake of the page
        // under it only after the sites.
        assert_eq!(placed[1].hit.page.title, "Python (genus)");
        assert_eq!((placed[1].under.as_deref(), placed[1].at), (None, 2));
    }

    #[test]
    fn named_pages_lead_when_no_site_is_named() {
        let sites = [
            known_site("curie.fr", false, 0.4),
            site("a.com", false),
            site("b.com", false),
        ];
        let placed = place_pages(
            "",
            &sites,
            vec![
                found("Marie Curie", None, true, 0.9),
                found("Pierre Curie", None, false, 0.35),
                found("Curie (unit)", None, false, 0.2),
            ],
        );
        let at: Vec<(&str, usize)> = placed
            .iter()
            .map(|p| (p.hit.page.title.as_str(), p.at))
            .collect();
        assert_eq!(at, [("Marie Curie", 0), ("Pierre Curie", 3)]);
        assert!(place_pages("", &[], vec![found("Marie Curie", None, true, 0.9)])[0].at == 0);
    }

    #[test]
    fn sites_the_page_may_be_about_stay_first() {
        let described = |title: &str, description: &str, score: f32| {
            let mut hit = found(title, None, true, score);
            hit.page.description = Some(description.into());
            hit
        };
        // A much read page comes before a little known namesake...
        let placed = place_pages(
            "",
            &[known_site("mariecurie.org", true, 0.2)],
            vec![described("Marie Curie", "Polish-French physicist", 0.8)],
        );
        assert_eq!(placed[0].at, 0);
        // ...but after the website of a company or product it names, even
        // when the page found first is about something else.
        let placed = place_pages(
            "",
            &[known_site("robinhood.com", true, 0.47)],
            vec![
                described("Robin Hood", "Legendary English outlaw", 0.9),
                described(
                    "Robinhood Markets",
                    "American financial services company",
                    0.6,
                ),
            ],
        );
        assert_eq!(placed[0].at, 1);
        let placed = place_pages(
            "",
            &[known_site("cvs.com", false, 0.43)],
            vec![described(
                "CVS Pharmacy",
                "American retail pharmacy chain",
                0.6,
            )],
        );
        assert_eq!(placed[0].at, 1);
        // A one-word query spelling the site, not the page ("robinhood").
        let placed = place_pages(
            "robinhood",
            &[known_site("robinhood.com", true, 0.47)],
            vec![described(
                "Robin Hood",
                "Heroic outlaw in English folklore",
                0.9,
            )],
        );
        assert_eq!(placed[0].at, 1);
        let placed = place_pages(
            "robin hood",
            &[known_site("robinhood.com", true, 0.47)],
            vec![described(
                "Robin Hood",
                "Heroic outlaw in English folklore",
                0.9,
            )],
        );
        assert_eq!(placed[0].at, 0);
        // A government site called after the page.
        let placed = place_pages(
            "fafsa",
            &[known_site("fafsa.gov", true, 0.34)],
            vec![described(
                "FAFSA",
                "Form to determine eligibility for US student aid",
                0.7,
            )],
        );
        assert_eq!(placed[0].at, 1);
        // ...and after an official website or a well known site.
        let mut official = known_site("example.org", false, 0.3);
        official.official = true;
        let known = known_site("example.com", false, 0.9);
        for site in [official, known] {
            let placed = place_pages("", &[site], vec![found("Example", None, true, 0.8)]);
            assert_eq!(placed[0].at, 1);
        }
    }

    #[test]
    fn organizations_by_description() {
        for yes in [
            "Open-source software framework",
            "American auto insurance company",
            "Credit card brand",
            "Online food ordering services",
            "Credit card",
            "Specification for machine-readable interface files",
        ] {
            assert!(describes_an_organization(Some(yes)), "{yes}");
        }
        for no in [
            "Polish-French physicist (1867–1934)",
            "Region of spacetime",
            "1889 painting by Vincent van Gogh",
        ] {
            assert!(!describes_an_organization(Some(no)), "{no}");
        }
        assert!(!describes_an_organization(None));
    }

    #[test]
    fn sites_called_after_pages() {
        assert!(site_is_titled("cvs.com", "CVS Pharmacy"));
        assert!(site_is_titled("capitalone.com", "Capital One"));
        assert!(site_is_titled("turbotax.intuit.com", "TurboTax"));
        assert!(site_is_titled("robinhood.com", "Robin Hood"));
        assert!(site_is_titled("tauri.app", "Tauri (software framework)"));
        assert!(!site_is_titled("leonardodavinci.net", "Marie Curie"));
        assert!(!site_is_titled("curie.fr", "Marie Curie"));
        assert!(site_is_titled(
            "navyfederal.org",
            "Navy Federal Credit Union"
        ));
        assert!(!site_is_titled("navyfed.org", "Navy Federal Credit Union"));
    }

    #[test]
    fn books_and_papers_follow_articles_and_sites() {
        let book = |title: &str, author: &str| {
            Page::from_book(Article {
                title: title.into(),
                description: Some(format!("Book by {author}")),
                item: Some("OL893415W".into()),
                views: 5_000,
                aliases: vec![format!("{title} {author}")],
                ..Article::default()
            })
        };
        let paper = Page::from_paper(Article {
            title: "Attention Is All You Need".into(),
            item: Some("10.48550/arxiv.1706.03762".into()),
            views: 90_000,
            ..Article::default()
        });
        assert_eq!(paper.url, "https://doi.org/10.48550/arxiv.1706.03762");
        assert_eq!(paper.set_name(), "OpenAlex");
        let (_dir, s) = searcher(&[
            book("Dune", "Frank Herbert"),
            book("Frankenstein; or, The Modern Prometheus", "Mary Shelley"),
            page("Dune (novel)", 1_000, &[]),
            paper,
        ]);
        // The article on the novel before the book of the same name.
        let hits = s.search("dune", 5).unwrap();
        assert_eq!(titles(&hits), ["Dune (novel)", "Dune"]);
        assert_eq!(hits[1].page.url, "https://openlibrary.org/works/OL893415W");
        // Alone the title may be anything: the book never comes before
        // every site.
        let placed = place_pages("dune", &[known_site("dunebook.com", false, 0.0)], hits);
        assert_eq!(placed[1].at, 1);
        // With its author or "book" it is the book that is searched for,
        // before any site the query does not name.
        for query in [
            "dune frank herbert",
            "dune herbert",
            "Dune book",
            "frankenstein mary shelley",
        ] {
            let hits = s.search(query, 5).unwrap();
            assert!(hits[0].whole && hits[0].page.set == BOOKS_SET, "{query}");
            let placed = place_pages(query, &[known_site("dunebook.com", false, 0.9)], hits);
            assert_eq!(placed[0].at, 0, "{query}");
        }
        let hits = s.search("dune frank herbert", 5).unwrap();
        let placed = place_pages(
            "dune frank herbert",
            &[known_site("dunefrankherbert.com", true, 0.0)],
            hits,
        );
        assert_eq!(placed[0].at, 1);
        assert!(!s
            .search("dune movie", 5)
            .unwrap()
            .iter()
            .any(|hit| hit.whole));
        // Only one edition leads.
        let best = s.search("dune frank herbert", 5).unwrap().remove(0);
        let placed = place_pages(
            "dune frank herbert",
            &[known_site("dunebook.com", false, 0.9)],
            vec![best.clone(), best],
        );
        assert_eq!(placed.iter().map(|p| p.at).collect::<Vec<_>>(), [0, 1]);
        // Papers are found by most of their words too.
        let hits = s.search("attention all you need paper", 5).unwrap();
        assert_eq!(titles(&hits), ["Attention Is All You Need"]);
    }

    #[test]
    fn urls_and_sets() {
        let p = page("AT&T", 1, &[]);
        assert_eq!(p.url, "https://en.wikipedia.org/wiki/AT%26T");
        assert_eq!(p.set_name(), "Wikipedia");
        assert_eq!(base_title("Python (genus)"), "Python");
        assert_eq!(base_title("(Untitled)"), "(Untitled)");
        assert_eq!(page_key("Marie Curie"), "mariecurie");
    }
}
