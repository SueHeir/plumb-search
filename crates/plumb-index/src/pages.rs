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

use plumb_core::article::SearchContent;
use plumb_core::packages::PackageInfo;
use plumb_core::papers::PaperMetadata;
use serde::{Deserialize, Serialize};
use tantivy::schema::Field;
use tantivy::tokenizer::TextAnalyzer;
use tantivy::IndexReader;

mod building;
mod docs;
mod named_site;
mod page;
mod papers;
mod placing;
mod repos;
mod scoring;
mod search;
mod sites;

pub use self::building::{build_page_index, build_page_index_with_budget, GO_ON_EVERY};
pub use self::docs::{docs_query_topic, docs_symbol_matches};
pub(crate) use self::page::is_site_page_set;
pub use self::placing::{
    add_named_site, drop_far_namesakes, drop_namesakes_of_words, keep_page_rules, lift_named_sites,
    operators_allow, options_allow, page_key, place_operator_pages, place_pages, DEMAND_MARGIN,
    MAX_PAGES_LISTED, MAX_SITE_PAGES, MIN_PARTIAL_SCORE, OPERATOR_PAGES, PARTIAL_AFTER,
    WEAK_SITE_MATCH,
};
pub(crate) use self::placing::{
    asked_as_question, kept_below, named_page_site, names_best_site_page,
};
pub use self::scoring::{made_by, site_pages_after_named_article};
pub use self::search::{DEMAND_NOTED, KNOWN_SONG_LISTENERS, KNOWN_SONG_MARGIN};
pub use self::sites::base_title;

/// `name` of a page one of whose aliases the query is.
pub const ALIAS_MATCH: f32 = 0.9;

/// Most `name` of a page whose title has some of the query's words.
pub const PARTIAL_MATCH: f32 = 0.6;

/// How well a Wikipedia article matches a query that is one of its other
/// names ([`Page::names`]): a less read redirect, or one to a section of
/// it ("manubrium" for Sternum). Not its name, so never above a page the
/// query names, but enough to be listed.
pub const OTHER_NAME_MATCH: f32 = 0.6;

/// How many times as much a person must be read as any other article with
/// their surname in its title to be named by it alone
/// ([`PageSearcher::name_by_surname`]), or by a title and their name.
const SURNAME_MARGIN: u64 = 3;

/// How many times as much an article called a query without its leading
/// "the" must be read as every article the whole query names to be named
/// too ([`PageSearcher::name_without_the`]).
const WITHOUT_THE_MARGIN: u64 = 10;

/// Words before a person's name that are none of it: "sir isaac newton",
/// "st francis of assisi" ([`PageSearcher::name_by_title`]).
const HONORIFICS: &[&str] = &[
    "sir", "dame", "dr", "mr", "mrs", "ms", "saint", "st", "lord", "lady", "tsar", "czar",
];

/// Titles before a person's name that their article's title or
/// description says they held: "president kennedy" is John F. Kennedy,
/// "President of the United States", and not Kennedy the commentator.
const OFFICES: &[&str] = &[
    "president",
    "senator",
    "governor",
    "mayor",
    "chancellor",
    "chairman",
    "pope",
    "emperor",
    "empress",
    "king",
    "queen",
    "prince",
    "princess",
    "general",
    "admiral",
];

/// How much popularity counts, against how well the query names the page.
pub const POPULARITY_SHARE: f32 = 0.5;

/// Pages whose words match that are looked at, most matching first.
const CANDIDATES: usize = 200;

/// Fewest words (stemmed, without the most common ones) of a query that
/// finds questions by their words.
pub const QUESTION_QUERY_WORDS: usize = 3;

/// Least share of those words a question's title and tags must have.
pub const QUESTION_SHARE: f32 = 0.75;
/// How much a word that only asks ([`ASKING_WORDS`]) counts toward a
/// question's [`QUESTION_SHARE`]: nearly every question has some, so they
/// say little of what it asks. Counted whole, "how to change a car battery"
/// found "How many amps should my car pull while it is turned off?" by
/// "how", "car" and "battery". A word no page knows, likely a typo, counts
/// as little: "how to undo git comit" still finds the question.
const UNSURE_WORD_SHARE: f32 = 0.5;

/// Least number of pages whose names have a word for the word to be
/// spelled right: "pkce", "nalgebra", "stain" and "biles" are, however few
/// sites say them.
pub const KNOWN_WORD_PAGES: u64 = 3;

/// A word the pages hardly know is corrected to a near word found in this
/// many times as many pages names ([`PageSearcher::suggest_spelling`])...
pub const PAGE_FIX_RATIO: u64 = 20;

/// ...and in at least this many: a slip of a rare word is no likelier than
/// a rare word typed as meant ("kiwipete" is not "kimipet").
pub const MIN_PAGE_FIX_PAGES: u64 = 20;

/// Most read articles about a site looked at for one about the site itself
/// ([`PageSearcher::title_untitled`]).
const TITLE_CANDIDATES: usize = 20;

/// Fewest words (stemmed, without the most common ones and the asking
/// words) of a query that finds Wikipedia articles by what they say of
/// themselves ([`Page::about`]).
pub const DESCRIBED_QUERY_WORDS: usize = 2;

/// Fewest such words of a query of which an article whose title it has in
/// full may lack one: "source of folic acid" finds Folic acid.
pub const DESCRIBED_MISSING_FROM: usize = 3;

/// How much a query word an article only says counts, against one of its
/// title. It has to say the word itself, in the singular or the plural,
/// not only its stem: "scheduled monument" does not say "schedule".
pub const DESCRIBED_WORD: f32 = 0.5;

/// Most `name` of a Wikipedia article whose lead ([`Page::lead`]), with
/// its names and description, says every word of a query (without the
/// asking words, as [`DESCRIBED_WORD`] says) of at least
/// [`DESCRIBED_QUERY_WORDS`]: the article
/// whose lead matches them best gets it, others less by how much worse
/// theirs does. "triassic jurassic cretaceous" finds Mesozoic.
pub const LEAD_MATCH: f32 = 0.55;

/// Most pages looked at for each part of a query that may be a title
/// ([`PageSearcher::search`]: "titanic sinking" finds Titanic).
const TITLE_INSIDE_CANDIDATES: usize = 20;

/// Most articles found by their leads looked at for one query.
const LEAD_CANDIDATES: usize = 20;

/// Changes whenever the page index schema or indexed search fields change.
pub const PAGE_INDEX_VERSION: &str = "v9-docs-address-symbols-short-titles";

/// Words that only ask ("what does resin mean"), left out of a query
/// matched against what articles say of themselves.
const ASKING_WORDS: &[&str] = &[
    "about",
    "are",
    "can",
    "define",
    "definition",
    "did",
    "do",
    "does",
    "facts",
    "how",
    "info",
    "information",
    "is",
    "mean",
    "meaning",
    "means",
    "should",
    "was",
    "were",
    "what",
    "when",
    "where",
    "which",
    "who",
    "why",
];

/// Least share of a question title's stemmed words a query that has all of
/// the question's own must have to ask the question as a whole.
pub const QUESTION_TITLE_SHARE: f32 = 0.5;

/// A single page that can be a result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    /// Structured scholarly identity, dates and provider count meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paper: Option<PaperMetadata>,
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
    /// A software package's card: its version, install command, docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<PackageInfo>,
    /// Facts about the item from Wikidata ([`plumb_core::facts`]): a
    /// country's capital, a person's birth date.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<plumb_core::facts::Fact>,
    /// A Wikipedia article's first sentences ([`plumb_core::article::lead_of`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
    /// A Wikipedia article's other names than its aliases: less read
    /// redirects, and ones to one of its sections ("Manubrium" to Sternum).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// A docs page's section headings ("List Comprehensions" in Python's
    /// "Data Structures"), which it is found by with [`Page::topic`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<String>,
    /// Declared primary content language. Absent for older files and
    /// pages with no declaration; `language()` can also use set evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_language: Option<String>,
    /// Bounded source symbols and passages, when an inner-page rebuild supplied them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<SearchContent>,
}

/// The set of GitHub repositories.
pub const GITHUB_SET: &str = "github";

/// The set of Stack Overflow questions.
pub const STACKOVERFLOW_SET: &str = "stackoverflow";

/// The set of questions of other Stack Exchange sites (Super User, Home
/// Improvement and more; see [`plumb_core::stack_exchange`]).
pub const STACKEXCHANGE_SET: &str = "stackexchange";

/// The set of books, from Open Library.
pub const BOOKS_SET: &str = "books";

/// Fewest words of a paper's title that ask for the paper with nothing
/// else: "basic local alignment search tool".
const PAPER_TITLE_WORDS: usize = 4;

/// The set of podcasts, from Podcast Index.
pub const PODCASTS_SET: &str = "podcasts";

/// The set of songs and albums, from MusicBrainz, ranked by
/// ListenBrainz's listeners.
pub const MUSIC_SET: &str = "music";

/// The set of films and TV shows, from Wikidata, ranked by sitelinks.
pub const FILMS_SET: &str = "films";

/// The set of software docs pages (MDN, Python's docs and others), fetched
/// from the docs sites' sitemaps.
pub const DOCS_SET: &str = "docs";

/// The set of inner pages of well-known reference sites: health,
/// dictionaries, recipes, how-tos, government (see
/// `plumb_tools::reference_sites`). Found by their words, they are listed
/// only when the search asks for them (`subpage_asked`).
pub const REFERENCE_SET: &str = "reference";

/// The reference set under a name v0.2.0 nodes never request: they list
/// "reference" pages with no such check. Opt-in until a complete,
/// coverage-checked generation of it is selected.
pub const REFERENCE2_SET: &str = "reference2";

/// The set of inner pages of other well-known sites: universities and
/// labs, big companies, government agencies, entertainment and museums
/// (see `plumb_tools::subpage_sites`). Found like reference pages, but listed
/// only when the search asks for them (`subpage_asked`).
///
/// Named "subpages2", not "subpages": nodes before v0.2.1 list the
/// "subpages" set with no such check, so it is never offered under that
/// name again, and they never ask for this one, which they do not know.
pub const SUBPAGES_SET: &str = "subpages2";

/// Inner pages a node fetched itself because its own searches wanted them
/// (`plumb_crawl::follow_links`), found and listed like subpages. They
/// are never shared: each node holds its own.
pub const WANTED_SET: &str = "wanted";

/// The name [`SUBPAGES_SET`] had before v0.2.1, which a node's settings
/// may still use.
pub const OLD_SUBPAGES_SET: &str = "subpages";

/// Fewest words (stemmed, without the most common ones) of a query that
/// finds reference pages by their words: "define prioritize".
pub const REFERENCE_QUERY_WORDS: usize = 2;

/// Where English Wikipedia's articles are.
const ENGLISH_WIKIPEDIA: &str = "https://en.wikipedia.org/wiki/";

/// The set of software packages (npm, PyPI, crates.io and others).
pub const PACKAGES_SET: &str = "packages";

/// Least popularity of a package found by a query that names only its
/// language or asks for a version ("rust book" is not the crate `book`).
pub const WELL_KNOWN_PACKAGE: f32 = 0.6;

/// The set of scholarly papers, from OpenAlex.
pub const PAPERS_SET: &str = "papers";

/// The set of Wikidata items with official profiles but no English
/// Wikipedia article (Linus Tech Tips the channel), each only ever listed
/// under its own website.
pub const WIKIDATA_SET: &str = "wikidata";

/// The set of English words and what they mean, from Wiktionary
/// ([`plumb_core::article`] files made by `plumb-tools fetch-pages --set
/// wiktionary`). Never listed among the results: a word is only looked up
/// ([`PageSearcher::definition`]) for a search that asks what it means.
pub const WIKTIONARY_SET: &str = "wiktionary";

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
    /// Where the learned ranking listed the page on its own
    /// ([`crate::learned::reorder`]), which [`place_pages`] keeps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned: Option<LearnedPlace>,
}

/// Where the learned ranking listed a page on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearnedPlace {
    /// Just before this site's result.
    Before(String),
    /// After all the sites.
    Last,
}

/// Actual indexed paper coverage, not provider-wide coverage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaperCoverage {
    pub total: u64,
    pub publication_year: u64,
    pub publication_day: u64,
    pub earliest_day: Option<String>,
    pub latest_day: Option<String>,
}

/// What [`build_page_index`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageIndexStats {
    #[serde(default)]
    pub papers: PaperCoverage,
    pub pages: u64,
    pub most_views: u64,
    /// Counts of declared/set-supported content languages. Missing in
    /// old manifests; unknown pages are `pages - sum(languages)`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub languages: std::collections::BTreeMap<String, u64>,
}

#[derive(Clone, Copy)]
struct Fields {
    words: Field,
    keys: Field,
    /// Exact eligibility and docs identity terms, applied before TopDocs:
    /// set, host and host suffixes; docs product, overview and symbols.
    scope: Field,
    /// Stemmed words of a question's title and tags; empty for other pages.
    topic: Field,
    /// Stemmed words of a Wikipedia article's names and what it says of
    /// itself ([`Page::about`]); empty for other pages.
    about: Field,
    /// Stemmed words of a Wikipedia article's lead ([`Page::lead`]), with
    /// how often each comes, so the leads most about the query rank first.
    lead: Field,
    /// A Wiktionary word as one token ([`WIKTIONARY_SET`]); such pages have
    /// no other words, so other searches never find them.
    word: Field,
    popularity: Field,
    /// The registrable domain of the official website of what a Wikipedia
    /// article is about ([`Page::site`]), for [`PageSearcher::site_popularity`].
    site: Field,
    language: Field,
    paper_year: Field,
    paper_day: Field,
    page: Field,
}

/// Searches a page index.
pub struct PageSearcher {
    reader: IndexReader,
    fields: Fields,
    words: TextAnalyzer,
    joined: TextAnalyzer,
    stemmed: TextAnalyzer,
    stats: PageIndexStats,
    requested_language: Option<String>,
}

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

#[cfg(test)]
mod tests;
