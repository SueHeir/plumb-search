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

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::article::{article_url, Article};
use plumb_core::packages::PackageInfo;
use plumb_core::{adult_level, host_of, normalize_text, AdultLevel, Operators, SafeSearch};
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, STORED, STRING,
};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, Term};

use crate::analysis::{self, JOINED_ANALYZER, STEMMED_ANALYZER, WORDS_ANALYZER};
use crate::replace::Staging;

/// `name` of a page one of whose aliases the query is.
pub const ALIAS_MATCH: f32 = 0.9;
/// Most `name` of a page whose title has some of the query's words.
pub const PARTIAL_MATCH: f32 = 0.6;
/// How well a Wikipedia article matches a query that is one of its other
/// names ([`Page::names`]): a less read redirect, or one to a section of
/// it ("manubrium" for Sternum). Not its name, so never above a page the
/// query names, but enough to be listed.
pub const OTHER_NAME_MATCH: f32 = 0.6;
/// How much popularity counts, against how well the query names the page.
pub const POPULARITY_SHARE: f32 = 0.5;
/// Pages whose words match that are looked at, most matching first.
const CANDIDATES: usize = 200;
/// Fewest words (stemmed, without the most common ones) of a query that
/// finds questions by their words.
pub const QUESTION_QUERY_WORDS: usize = 3;
/// Least share of those words a question's title and tags must have.
pub const QUESTION_SHARE: f32 = 0.75;
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
/// title.
pub const DESCRIBED_WORD: f32 = 0.5;
/// Most `name` of a Wikipedia article whose lead ([`Page::lead`]), with
/// its names and description, has every word of a query (stemmed, without
/// the asking words) of at least [`DESCRIBED_QUERY_WORDS`]: the article
/// whose lead matches them best gets it, others less by how much worse
/// theirs does. "triassic jurassic cretaceous" finds Mesozoic.
pub const LEAD_MATCH: f32 = 0.55;
/// Most articles found by their leads looked at for one query.
const LEAD_CANDIDATES: usize = 20;
/// Words that only ask ("what does resin mean"), left out of a query
/// matched against what articles say of themselves.
const ASKING_WORDS: &[&str] = &[
    "about",
    "are",
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
            package: None,
            facts: article.facts,
            lead: article.lead,
            names: article.names,
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
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
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
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
        }
    }

    /// The question `question` of another Stack Exchange site, written as
    /// an article whose item is the site and question number
    /// (`diy.stackexchange.com/12345`) and whose description is its tags;
    /// `None` for a site not in [`plumb_core::stack_exchange::SITES`].
    pub fn from_exchange(question: Article) -> Option<Self> {
        let (site, id) =
            plumb_core::stack_exchange::parse_question_item(question.item.as_deref()?)?;
        Some(Page {
            set: STACKEXCHANGE_SET.to_string(),
            url: site.question_url(id),
            title: question.title,
            description: question.description,
            site: None,
            views: question.views,
            aliases: question.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
        })
    }

    /// The Stack Exchange site of a page of the `stackexchange` set.
    fn exchange_site(&self) -> Option<&'static plumb_core::stack_exchange::ExchangeSite> {
        if self.set != STACKEXCHANGE_SET {
            return None;
        }
        let host = self.url.strip_prefix("https://")?.split('/').next()?;
        plumb_core::stack_exchange::site_of(host)
    }

    /// Whether the page is a Wikipedia article.
    pub fn is_article(&self) -> bool {
        self.set.starts_with("wikipedia-")
    }

    /// Whether the page is a question, of Stack Overflow or another Stack
    /// Exchange site.
    pub fn is_question(&self) -> bool {
        self.set == STACKOVERFLOW_SET || self.set == STACKEXCHANGE_SET
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
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
        }
    }

    /// The podcast `podcast`, written as an article whose item is its
    /// Podcast Index id and whose site is its website's, when it has one
    /// of its own.
    pub fn from_podcast(podcast: Article) -> Self {
        Page {
            set: PODCASTS_SET.to_string(),
            url: format!(
                "https://podcastindex.org/podcast/{}",
                podcast.item.as_deref().unwrap_or("")
            ),
            title: podcast.title,
            description: podcast.description,
            site: podcast.site,
            views: podcast.views,
            aliases: podcast.aliases,
            item: None,
            profiles: podcast.profiles,
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
        }
    }

    /// The song or album `music`, written as an article whose item is
    /// `recording/MBID` or `release-group/MBID` and whose views are its
    /// listeners; `None` for an item that is neither.
    pub fn from_music(music: Article) -> Option<Self> {
        let item = music.item.as_deref()?;
        let (kind, mbid) = item.split_once('/')?;
        let mbid_ok = mbid.len() == 36 && mbid.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
        if !matches!(kind, "recording" | "release-group") || !mbid_ok {
            return None;
        }
        Some(Page {
            set: MUSIC_SET.to_string(),
            url: format!("https://musicbrainz.org/{item}"),
            title: music.title,
            description: music.description,
            site: None,
            views: music.views,
            aliases: music.aliases,
            item: None,
            profiles: music.profiles,
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
        })
    }

    /// The film or TV show `film`, written as an article whose item is its
    /// Wikidata item (`Q25188`), followed by `/` and its English Wikipedia
    /// article's address path when it has one (`Q25188/Inception`), and
    /// whose views are its sitelinks. Its address is that article, or else
    /// the item on Wikidata. `None` for an item that does not read.
    pub fn from_film(film: Article) -> Option<Self> {
        let item = film.item.as_deref()?;
        let (id, article) = match item.split_once('/') {
            Some((id, path)) => (id, Some(path)),
            None => (item, None),
        };
        if !is_item_id(id) {
            return None;
        }
        let url = match article {
            None => format!("https://www.wikidata.org/wiki/{id}"),
            Some(path)
                if !path.is_empty()
                    && path.chars().all(|c| {
                        c.is_ascii_graphic() && !matches!(c, '"' | '<' | '>' | '\\' | '|')
                    }) =>
            {
                format!("{ENGLISH_WIKIPEDIA}{path}")
            }
            Some(_) => return None,
        };
        Some(Page {
            set: FILMS_SET.to_string(),
            url,
            title: film.title,
            description: film.description,
            site: None,
            views: film.views,
            aliases: film.aliases,
            item: Some(id.to_string()),
            profiles: film.profiles,
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
        })
    }

    /// The software docs page `doc`, written as an article whose item is
    /// the page's address (see `plumb_ingest::docs`). `None` for an item
    /// that is not an https:// address.
    pub fn from_docs(doc: Article) -> Option<Self> {
        let url = doc.item.filter(|item| {
            item.starts_with("https://")
                && item.len() > "https://".len()
                && !item.contains(char::is_whitespace)
        })?;
        Some(Page {
            set: DOCS_SET.to_string(),
            url,
            title: doc.title,
            description: doc.description,
            site: None,
            views: doc.views,
            aliases: doc.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
        })
    }

    /// The page of the reference set written as `page` in its articles
    /// file, whose item is its address.
    pub fn from_reference(page: Article) -> Option<Self> {
        Page::from_site_page(REFERENCE_SET, page)
    }

    /// The page of the set `set` (reference or subpages) written as `page`
    /// in its articles file, whose item is its address.
    fn from_site_page(set: &str, page: Article) -> Option<Self> {
        let url = page.item.filter(|item| {
            item.starts_with("https://")
                && item.len() > "https://".len()
                && !item.contains(char::is_whitespace)
        })?;
        Some(Page {
            set: set.to_string(),
            url,
            title: page.title,
            description: page.description,
            site: None,
            views: page.views,
            aliases: page.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
        })
    }

    /// Whether the page is an inner page of a site of the reference or
    /// subpages set, found by the same rules.
    pub fn is_site_page(&self) -> bool {
        self.set == REFERENCE_SET || self.set == SUBPAGES_SET
    }

    /// The host of a docs, reference or subpages page's address, without
    /// `www.`: `docs.python.org`, `healthline.com`.
    fn docs_host(&self) -> Option<&str> {
        if self.set != DOCS_SET && !self.is_site_page() {
            return None;
        }
        let host = self
            .url
            .split_once("://")?
            .1
            .split(['/', '?', '#'])
            .next()
            .filter(|host| !host.is_empty())?;
        Some(host.strip_prefix("www.").unwrap_or(host))
    }

    /// Where a paper can be read free, when it is not its own address:
    /// its copy on arXiv, the publisher's open version, a repository's.
    pub fn free_copy(&self) -> Option<&str> {
        if self.set != PAPERS_SET {
            return None;
        }
        self.website.as_deref().filter(|url| *url != self.url)
    }

    /// Whether the page is a TV show of the films set, rather than a film.
    pub fn is_show(&self) -> bool {
        self.set == FILMS_SET
            && self
                .description
                .as_deref()
                .is_some_and(plumb_core::films::describes_a_show)
    }

    /// Whether the page is a film or show of the films set that English
    /// Wikipedia has an article on, which is its address.
    pub fn is_film_with_article(&self) -> bool {
        self.set == FILMS_SET && self.url.starts_with(ENGLISH_WIKIPEDIA)
    }

    /// Whether the page is a song of the music set, rather than an album.
    pub fn is_song(&self) -> bool {
        self.set == MUSIC_SET && self.url.starts_with("https://musicbrainz.org/recording/")
    }

    /// The paper `paper`, written as an article whose item is its DOI or
    /// else its OpenAlex id, whose views are its citations and whose
    /// website is where it can be read free.
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
            website: paper.website,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
        }
    }

    /// The Wikidata item `item`, written as an article whose title is its
    /// English name and whose views are its sitelinks.
    pub fn from_item(item: Article) -> Self {
        Page {
            set: WIKIDATA_SET.to_string(),
            url: format!(
                "https://www.wikidata.org/wiki/{}",
                item.item.as_deref().unwrap_or("")
            ),
            title: item.title,
            description: item.description,
            site: item.site,
            views: item.views,
            aliases: item.aliases,
            item: item.item,
            profiles: item.profiles,
            website: None,
            package: None,
            facts: item.facts,
            lead: None,
            names: Vec::new(),
        }
    }

    /// The English word `word` of Wiktionary, written as an article whose
    /// title is the word and whose description says what it means.
    pub fn from_word(word: Article) -> Self {
        let url = plumb_core::article::article_url("en", &word.title).replacen(
            "en.wikipedia.org",
            "en.wiktionary.org",
            1,
        );
        Page {
            set: WIKTIONARY_SET.to_string(),
            url,
            title: word.title,
            description: word.description,
            site: None,
            views: word.views,
            aliases: Vec::new(),
            item: None,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
        }
    }

    /// The software package `package`, written as an article whose item is
    /// `registry:name` and whose views are its share of its registry's
    /// most used package's downloads (see [`plumb_core::packages`]).
    /// `None` when it has no card, or one whose registry or name does not
    /// read.
    pub fn from_package(package: Article) -> Option<Self> {
        let info = package.package?;
        let url = info.page_url()?;
        Some(Page {
            set: PACKAGES_SET.to_string(),
            url,
            title: package.title,
            description: package.description,
            site: None,
            views: package.views,
            aliases: package.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
            package: Some(info),
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
        })
    }

    /// Whether [`Page::from_set`] can read pages of the set `set`. A
    /// package's line needs its card, so an empty article can't tell.
    pub fn has_reader(set: &str) -> bool {
        set == PACKAGES_SET
            || set == STACKEXCHANGE_SET
            || set == MUSIC_SET
            || set == FILMS_SET
            || set == DOCS_SET
            || set == REFERENCE_SET
            || set == SUBPAGES_SET
            || Page::from_set(set, Article::default()).is_some()
    }

    /// The page of the set `set` written as `article` in its articles
    /// file, `None` for a set without a reader (or a package without a card,
    /// or a question of a site not known).
    pub fn from_set(set: &str, article: Article) -> Option<Self> {
        Some(match set {
            GITHUB_SET => Page::from_repo(article),
            STACKOVERFLOW_SET => Page::from_question(article),
            BOOKS_SET => Page::from_book(article),
            PODCASTS_SET => Page::from_podcast(article),
            PAPERS_SET => Page::from_paper(article),
            WIKIDATA_SET => Page::from_item(article),
            WIKTIONARY_SET => Page::from_word(article),
            PACKAGES_SET => Page::from_package(article)?,
            STACKEXCHANGE_SET => Page::from_exchange(article)?,
            MUSIC_SET => Page::from_music(article)?,
            FILMS_SET => Page::from_film(article)?,
            DOCS_SET => Page::from_docs(article)?,
            REFERENCE_SET => Page::from_reference(article)?,
            SUBPAGES_SET => Page::from_site_page(SUBPAGES_SET, article)?,
            _ => Page::from_article(set.strip_prefix("wikipedia-")?, article),
        })
    }

    /// The words a question, paper, docs or reference page is found by
    /// besides its title: its title, a question's tags and the titles of
    /// its duplicates, and a docs, reference or subpages page's other names and
    /// description. `None` for pages of other sets, found by their names
    /// only.
    pub fn topic(&self) -> Option<String> {
        if self.set == PAPERS_SET {
            return Some(self.title.clone());
        }
        if self.set == DOCS_SET || self.is_site_page() {
            let mut topic = self.title.clone();
            for text in self.aliases.iter().chain(&self.description) {
                topic.push(' ');
                topic.push_str(text);
            }
            return Some(topic);
        }
        self.is_question().then(|| {
            let mut topic = self.title.clone();
            for text in self.aliases.iter().chain(&self.description) {
                topic.push(' ');
                topic.push_str(text);
            }
            topic
        })
    }

    /// The titles a question is asked by, each with its tags: its own,
    /// then those of the questions closed as its duplicates. A paper or
    /// docs page is asked by its title and [`Page::topic`], pages of other
    /// sets by none.
    fn asked_as(&self) -> Vec<(&str, String)> {
        if !self.is_question() {
            return self
                .topic()
                .map(|topic| (self.title.as_str(), topic))
                .into_iter()
                .collect();
        }
        std::iter::once(&self.title)
            .chain(&self.aliases)
            .map(|title| {
                let words = match &self.description {
                    Some(tags) => format!("{title} {tags}"),
                    None => title.clone(),
                };
                (title.as_str(), words)
            })
            .collect()
    }

    /// What a Wikipedia article is matched on beyond its names: its title,
    /// the other titles that lead to it and its description ("1973 studio
    /// album by Queen"), so "queen album" finds Queen (album). `None` for
    /// pages of other sets.
    pub fn about(&self) -> Option<String> {
        if !self.is_article() {
            return None;
        }
        let mut about = self.title.clone();
        for text in self
            .aliases
            .iter()
            .chain(&self.names)
            .chain(&self.description)
        {
            about.push(' ');
            about.push_str(text);
        }
        Some(about)
    }

    /// Whether the page may be listed before every site. Books, podcasts,
    /// papers, songs, albums, films and shows share their titles with too
    /// much ("Python", "Apple", "Hello", "Up") to.
    pub fn may_lead(&self) -> bool {
        self.set != BOOKS_SET
            && self.set != PAPERS_SET
            && self.set != PODCASTS_SET
            && self.set != MUSIC_SET
            && self.set != FILMS_SET
    }

    /// The name of the set people see: "Wikipedia".
    pub fn set_name(&self) -> &str {
        if self.set.starts_with("wikipedia-") || self.is_film_with_article() {
            "Wikipedia"
        } else if self.set == GITHUB_SET {
            "GitHub"
        } else if self.set == STACKOVERFLOW_SET {
            "Stack Overflow"
        } else if let Some(site) = self.exchange_site() {
            site.name
        } else if self.set == BOOKS_SET {
            "Open Library"
        } else if self.set == PODCASTS_SET {
            "Podcast Index"
        } else if self.set == MUSIC_SET {
            "MusicBrainz"
        } else if self.set == PAPERS_SET {
            "OpenAlex"
        } else if self.set == WIKIDATA_SET || self.set == FILMS_SET {
            "Wikidata"
        } else if self.set == WIKTIONARY_SET {
            "Wiktionary"
        } else if let Some(registry) = self.registry() {
            registry.name
        } else if let Some(host) = self.docs_host() {
            host
        } else {
            &self.set
        }
    }

    /// The registry of a package.
    pub fn registry(&self) -> Option<&'static plumb_core::packages::Registry> {
        self.package.as_ref()?.registry()
    }

    /// The language the page is in, when its set says: `en` for
    /// English Wikipedia, GitHub and Stack Exchange's questions.
    pub fn language(&self) -> Option<&str> {
        if let Some(lang) = self.set.strip_prefix("wikipedia-") {
            Some(lang)
        } else if self.set == GITHUB_SET || self.is_question() || self.is_film_with_article() {
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
        } else if let Some(site) = self.exchange_site() {
            site.domain
        } else if self.set == BOOKS_SET {
            "openlibrary.org"
        } else if self.set == PODCASTS_SET {
            "podcastindex.org"
        } else if self.set == MUSIC_SET {
            "musicbrainz.org"
        } else if self.set == PAPERS_SET {
            "openalex.org"
        } else if self.set == FILMS_SET && !self.is_film_with_article() {
            "wikidata.org"
        } else if let Some(registry) = self.registry() {
            registry.domain
        } else if let Some(host) = self.docs_host() {
            host
        } else {
            "wikipedia.org"
        }
    }
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
/// `plumb_core::reference`).
pub const REFERENCE_SET: &str = "reference";
/// The set of inner pages of other well-known sites: universities and
/// labs, big companies, government agencies, entertainment and museums
/// (see `plumb_core::subpages`). Found like reference pages.
pub const SUBPAGES_SET: &str = "subpages";
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
/// ([`plumb_core::article`] files made by `plumb fetch-pages --set
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

struct Fields {
    words: Field,
    keys: Field,
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
    let about = builder.add_text_field(
        "about",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(STEMMED_ANALYZER)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let lead = builder.add_text_field(
        "lead",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(STEMMED_ANALYZER)
                .set_index_option(IndexRecordOption::WithFreqs),
        ),
    );
    let word = builder.add_text_field(
        "word",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(JOINED_ANALYZER)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let popularity = builder.add_u64_field("popularity", FAST | STORED);
    let site = builder.add_text_field("site", STRING);
    let page = builder.add_text_field("page", STORED);
    (
        builder.build(),
        Fields {
            words,
            keys,
            topic,
            about,
            lead,
            word,
            popularity,
            site,
            page,
        },
    )
}

/// Whether `id` is a Wikidata item's: `Q25188`.
fn is_item_id(id: &str) -> bool {
    id.len() > 1 && id.starts_with('Q') && id[1..].bytes().all(|b| b.is_ascii_digit())
}

/// `title` without a trailing qualifier in brackets: `Python (programming
/// language)` -> `Python`.
fn base_title(title: &str) -> &str {
    match title.rfind(" (") {
        Some(i) if title.ends_with(')') && i > 0 => &title[..i],
        _ => title,
    }
}

/// What kind of page words around a name ask for ([`hinted_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hint {
    /// Its encyclopedia article: "lululemon wikipedia".
    Article,
    /// A paper of that title: "batch normalization paper".
    Paper,
    /// Whatever is named so: "who is galileo", "ariana grande age".
    Any,
}

/// Words before a name that ask about it.
const HINT_LEADS: &[&str] = &[
    "who is ",
    "who was ",
    "who are ",
    "who were ",
    "tell me about ",
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
];

/// Words after a name that say what about it is wanted.
const HINT_TAILS: &[(&str, Hint)] = &[
    (" wikipedia", Hint::Article),
    (" wiki", Hint::Article),
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
    (" cast", Hint::Any),
    (" author", Hint::Any),
    (" movie", Hint::Any),
    (" film", Hint::Any),
    (" meaning", Hint::Any),
    (" mean", Hint::Any),
    (" means", Hint::Any),
    (" definition", Hint::Any),
    (" explained", Hint::Any),
    (" quotes", Hint::Any),
];

/// The name in `query` without the words around it that only say what is
/// wanted, and what kind of page they ask for: "lululemon" of "lululemon
/// wikipedia". `None` when there are none, or nothing else.
fn hinted_name(query: &str) -> Option<(String, Hint)> {
    let q = plumb_core::collapse_whitespace(query.trim().trim_end_matches('?')).to_lowercase();
    let mut name = q.as_str();
    let mut hint = None;
    if let Some(rest) = HINT_LEADS.iter().find_map(|lead| name.strip_prefix(lead)) {
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

/// Words of a short description saying a page is about an organization,
/// a product or a service, which has a website of its own, rather than a
/// person, place, idea or work.
const ORGANIZATION_WORDS: &[&str] = &[
    "agency",
    "airline",
    "app",
    "application",
    "bank",
    "banking",
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
    "institution",
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

/// Whether `domain`'s first label is the initials of the page's title,
/// three or more of them, small words aside: nba.com for "National
/// Basketball Association".
fn site_is_initials(domain: &str, title: &str) -> bool {
    let label = squash(domain.split('.').next().unwrap_or(""));
    let initials: String = base_title(title)
        .split_whitespace()
        .map(squash)
        .filter(|word| !word.is_empty() && !["of", "and", "the", "for"].contains(&word.as_str()))
        .filter_map(|word| word.chars().next())
        .collect();
    label.len() >= 3 && initials == label
}

/// Whether `site`'s title may be another page's: an official site whose
/// title has not its own name, as when its homepage sent the crawler on
/// to a section ("California Post" for nypost.com). Its article's title
/// says which ([`PageSearcher::title_untitled`]).
fn may_be_borrowed(site: &crate::Hit) -> bool {
    let label = squash(site.domain.split('.').next().unwrap_or(""));
    site.official
        && label.len() >= 3
        && site
            .title
            .as_deref()
            .is_some_and(|title| !squash(title).contains(&label))
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
        document.add_u64(
            fields.popularity,
            (popularity * POPULARITY_SCALE).round() as u64,
        );
        if page.set == WIKTIONARY_SET {
            document.add_text(fields.word, &page.title);
            document.add_text(fields.page, serde_json::to_string(&page)?);
            writer.add_document(document)?;
            stats.pages += 1;
            continue;
        }
        document.add_text(fields.words, &page.title);
        for alias in &page.aliases {
            document.add_text(fields.words, alias);
        }
        for name in &page.names {
            document.add_text(fields.words, name);
            document.add_text(fields.keys, name);
        }
        if let Some(topic) = page.topic() {
            document.add_text(fields.topic, topic);
        }
        if let Some(about) = page.about() {
            document.add_text(fields.about, about);
        }
        if let Some(lead) = page.lead.as_deref().filter(|_| page.is_article()) {
            document.add_text(fields.lead, lead);
        }
        // A docs page's title alone ("Introduction") names nothing: it is
        // named by its product's name and title ("python sorting
        // techniques").
        if page.set != DOCS_SET {
            document.add_text(fields.keys, &page.title);
            let base = base_title(&page.title);
            if base != page.title {
                document.add_text(fields.keys, base);
            }
        }
        // A package is asked for by its short name too: "gin golang".
        if page.package.is_some() || page.set == DOCS_SET {
            for alias in &page.aliases {
                document.add_text(fields.keys, alias);
            }
        }
        if let Some(site) = page.site.as_deref().filter(|_| page.is_article()) {
            document.add_text(fields.site, site);
        }
        document.add_text(fields.page, serde_json::to_string(&page)?);
        writer.add_document(document)?;
        stats.pages += 1;
        stats.most_views = stats.most_views.max(page.views);
    }
    writer.commit().context("writing the page index")?;
    // A merge the commit started must end before the index is put in
    // place: one cut short leaves its segment files behind for good.
    writer
        .wait_merging_threads()
        .context("finishing the page index merges")?;
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
    /// [`crate::Hit::demand`]), for [`place_pages`] to weigh against the
    /// pages a query names. A few, not one: a node may reorder the first
    /// results for its searcher after.
    pub fn note_demand(&self, sites: &mut [crate::Hit]) -> Result<()> {
        for site in sites.iter_mut().take(DEMAND_NOTED) {
            site.demand = self.site_popularity(&site.domain)?;
        }
        Ok(())
    }

    /// Gives the sites among `sites` that have no title the title of the
    /// most read Wikipedia article about what each is the official website
    /// of, without its qualifier: "Notion" for notion.so, "Yelp" for
    /// yelp.com, whose homepages turn crawlers away. Only an article whose
    /// official website is the site's homepage ([`Page::website`]) and
    /// whose title the domain spells ([`site_is_titled`]) or abbreviates
    /// ([`site_is_initials`], nba.com) counts:
    /// "Schitt's Creek", whose website is a page on cbc.ca, is not what
    /// cbc.ca is. An official site whose title may be borrowed
    /// ([`may_be_borrowed`]) and names none of its articles takes the most
    /// read one's title too: "New York Post" for nypost.com, not
    /// "California Post".
    pub fn title_untitled(&self, sites: &mut [crate::Hit]) -> Result<()> {
        let searcher = self.reader.searcher();
        for site in sites.iter_mut() {
            let untitled = site
                .title
                .as_deref()
                .is_none_or(|title| title.trim().is_empty());
            if !untitled && !may_be_borrowed(site) {
                continue;
            }
            let query = TermQuery::new(
                Term::from_field_text(self.fields.site, &site.domain),
                IndexRecordOption::Basic,
            );
            let best = searcher.search(
                &query,
                &TopDocs::with_limit(TITLE_CANDIDATES)
                    .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc),
            )?;
            let mut articles = Vec::new();
            for (_, address) in best {
                let document: TantivyDocument = searcher.doc(address)?;
                let Some(stored) = document
                    .get_first(self.fields.page)
                    .and_then(|v| v.as_str())
                else {
                    continue;
                };
                let page: Page = serde_json::from_str(stored)?;
                if page.is_article() && page.website.is_none() {
                    articles.push(page.title);
                }
            }
            let names = articles.iter().map(|title| base_title(title).trim());
            let title = if untitled {
                articles
                    .iter()
                    .find(|title| {
                        site_is_titled(&site.domain, title) || site_is_initials(&site.domain, title)
                    })
                    .map(|title| base_title(title).trim())
            } else {
                // A title that names what any article about the site is
                // stays ("Welcome to Steam" for steampowered.com, whose
                // articles are Valve's and Steam's); else the most read
                // one names it.
                let shown = squash(site.title.as_deref().unwrap_or(""));
                let named = names.clone().any(|name| {
                    let name = squash(name);
                    name.len() < 3 || shown.contains(&name)
                });
                names.clone().next().filter(|_| !named)
            };
            if let Some(title) = title.filter(|title| !title.is_empty()) {
                site.title = Some(title.to_string());
            }
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
    /// the noisy channel ([`crate::spell_model`]). A word some pages have
    /// is kept unless the slip is likelier than the word ("inkala" is not
    /// "ikala"). `None` when no word changes.
    pub fn suggest_spelling(
        &self,
        query: &str,
        model: Option<&crate::spell_model::Model>,
        site_known: &dyn Fn(&str) -> bool,
    ) -> Result<Option<crate::Spelling>> {
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
                if model.is_some() && typed_docs > 0 && likelihood < (typed_docs as f64).ln() {
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

    /// The best `limit` pages for `query`, best first.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<PageHit>> {
        let mut hits = self.search_once(query, limit)?;
        // "lululemon wikipedia", "batch normalization paper", "who is
        // galileo": the words that say what is wanted are not part of the
        // name, so the name is searched for too.
        let Some((name, hint)) = hinted_name(query) else {
            return Ok(hits);
        };
        let mut found = self.search_once(&name, limit)?;
        match hint {
            Hint::Article => found.retain(|hit| hit.page.set.starts_with("wikipedia-")),
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
            if !hits.iter().any(|h| h.page == hit.page) {
                hits.push(hit);
            }
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        truncate_keeping_inner_pages(&mut hits, limit);
        Ok(hits)
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

    fn search_once(&self, query: &str, limit: usize) -> Result<Vec<PageHit>> {
        let words = analysis::tokens(&self.words, query);
        let Some(joined) = analysis::tokens(&self.joined, query).pop() else {
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
            .search(&named_by_query, &by_popularity())?
            .into_iter()
            .map(|(_, address)| address)
            .collect();
        for (_, address) in searcher.search(&BooleanQuery::new(clauses), &by_popularity())? {
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        // Books and papers named by their title and then their author:
        // "random forests breiman". Searched apart, the titles of two words
        // or more that start the query; a page found only so is kept only
        // when it is asked for so.
        let mut by_title_first = HashSet::new();
        // Films and shows by a title of one word too: "inception 2010".
        // Other pages found so must be films asked for so.
        let mut film_title_first = HashSet::new();
        for end in 1..words.len() {
            let Some(key) = analysis::tokens(&self.joined, &words[..end].join(" ")).pop() else {
                continue;
            };
            let named = TermQuery::new(
                Term::from_field_text(self.fields.keys, &key),
                IndexRecordOption::Basic,
            );
            for (_, address) in searcher.search(&named, &by_popularity())? {
                if !addresses.contains(&address) {
                    addresses.push(address);
                    by_title_first.insert(address);
                    if end == 1 {
                        film_title_first.insert(address);
                    }
                }
            }
        }
        // Packages, only ever found when the query asks for one: "serde
        // crate", "latest version of requests python".
        let package_query = plumb_core::packages::package_query(query);
        let package_key = package_query
            .as_ref()
            .and_then(|asked| analysis::tokens(&self.joined, &asked.name).pop());
        if let Some(key) = &package_key {
            let named = TermQuery::new(
                Term::from_field_text(self.fields.keys, key),
                IndexRecordOption::Basic,
            );
            for (_, address) in searcher.search(&named, &by_popularity())? {
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
            for (_, address) in searcher.search(&most_words, &by_popularity())? {
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
                        .map(|stem| {
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(
                                    Term::from_field_text(self.fields.about, stem),
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
                    .map(|stem| {
                        let either: Vec<(Occur, Box<dyn Query>)> = [
                            (self.fields.lead, IndexRecordOption::WithFreqs),
                            (self.fields.about, IndexRecordOption::Basic),
                        ]
                        .into_iter()
                        .map(|(field, option)| {
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(Term::from_field_text(field, stem), option))
                                    as Box<dyn Query>,
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
                &every_word,
                &TopDocs::with_limit(LEAD_CANDIDATES).order_by_score(),
            )? {
                lead_scores.insert(address, score);
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        let best_lead = lead_scores.values().copied().fold(0.0f32, f32::max);
        // The query's rarest word that some question has: what it is
        // about. A question without it has only the asking words ("how to
        // get rid of aphids" found "How do I get rid of my bounty?").
        let mut topic_word: Option<(u64, &String)> = None;
        if stems.len() >= QUESTION_QUERY_WORDS.min(REFERENCE_QUERY_WORDS) {
            for stem in &stems {
                let found = searcher.doc_freq(&Term::from_field_text(self.fields.topic, stem))?;
                if found > 0 && topic_word.is_none_or(|(least, _)| found < least) {
                    topic_word = Some((found, stem));
                }
            }
        }
        let topic_word = topic_word.map(|(_, stem)| stem.as_str());
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
                    hits.push(PageHit {
                        score: ALIAS_MATCH
                            * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity),
                        page,
                        named: true,
                        popularity,
                        whole: false,
                        learned: None,
                    });
                }
                continue;
            }
            let asked_as_film = self.film_match(&page, &words);
            let asked_by_title = asked_as_film || self.book_match(&page, &words);
            // Songs and albums share their titles with too much ("Dead
            // Sea", "Notion", "Lord of the Flies"): one is only listed
            // when asked for by its artist or as a song or album.
            if page.set == MUSIC_SET && !asked_by_title {
                continue;
            }
            if by_title_first.contains(&address) && !asked_by_title
                || film_title_first.contains(&address) && !asked_as_film
            {
                continue;
            }
            let (mut name, mut named) = self.name_match(&page, query, &joined, &query_words);
            let mut whole = false;
            if asked_by_title {
                (name, named, whole) = (name.max(ALIAS_MATCH), true, true);
            } else if !named && (page.set != DOCS_SET || docs_asked(&page, query)) {
                let (question, asked) = self.question_match(&page, &stems, topic_word);
                name = name.max(question);
                whole = asked;
            }
            if !named && !whole {
                name = name.max(self.described_match(&page, &described));
                if let Some(&lead) = lead_scores.get(&address).filter(|_| page.is_article()) {
                    name = name
                        .max(LEAD_MATCH * (0.5 + 0.5 * lead / best_lead.max(f32::MIN_POSITIVE)));
                }
            }
            if name <= 0.0 {
                continue;
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
        let mut hits = fold_films(hits);
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        truncate_keeping_inner_pages(&mut hits, limit);
        Ok(hits)
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
    fn question_words(&self, query: &str) -> Vec<String> {
        let mut stems = analysis::tokens(&self.stemmed, query);
        let mut seen = HashSet::new();
        stems.retain(|stem| seen.insert(stem.clone()));
        stems
    }

    /// The different stemmed words of `query` without the words that only
    /// ask ([`ASKING_WORDS`]), as articles are matched on what they say of
    /// themselves: "resin" of "what does resin mean".
    fn described_words(&self, query: &str) -> Vec<String> {
        let asked: Vec<String> = analysis::tokens(&self.words, query)
            .into_iter()
            .filter(|word| !ASKING_WORDS.contains(&word.as_str()))
            .collect();
        self.question_words(&asked.join(" "))
    }

    /// How well a Wikipedia article covers the query's words `stems`
    /// ([`PageSearcher::described_words`]) with one of its names and what it
    /// says of itself ([`Page::about`], and its lead): 0 unless a name has some of them
    /// and the article all of them, or all but one when the name is whole
    /// in the query and the query has [`DESCRIBED_MISSING_FROM`] words.
    /// Otherwise [`PARTIAL_MATCH`] times the share of the query in the
    /// name, a word the article only says counting [`DESCRIBED_WORD`], and
    /// halfway down by the share of the name the query lacks: "queen
    /// album" covers Queen (album) by 0.45.
    fn described_match(&self, page: &Page, stems: &[String]) -> f32 {
        if stems.len() < DESCRIBED_QUERY_WORDS {
            return 0.0;
        }
        let Some(about) = page.about() else {
            return 0.0;
        };
        let mut about: HashSet<String> = analysis::tokens(&self.stemmed, &about)
            .into_iter()
            .collect();
        if let Some(lead) = &page.lead {
            about.extend(analysis::tokens(&self.stemmed, lead));
        }
        let missing = stems.iter().filter(|stem| !about.contains(*stem)).count();
        let mut best = 0.0f32;
        for name in
            std::iter::once(base_title(&page.title)).chain(page.aliases.iter().map(String::as_str))
        {
            let name: HashSet<String> = analysis::tokens(&self.stemmed, name).into_iter().collect();
            let in_name = stems.iter().filter(|stem| name.contains(*stem)).count();
            if in_name == 0 {
                continue;
            }
            let whole_name = name.iter().all(|word| stems.contains(word));
            if missing > 1 || missing == 1 && !(whole_name && stems.len() >= DESCRIBED_MISSING_FROM)
            {
                continue;
            }
            let said = stems.len() - in_name - missing;
            let query_share = (in_name as f32 + DESCRIBED_WORD * said as f32) / stems.len() as f32;
            let name_share =
                name.iter().filter(|word| stems.contains(*word)).count() as f32 / name.len() as f32;
            best = best.max(PARTIAL_MATCH * query_share * (0.5 + 0.5 * name_share));
        }
        best
    }

    /// How well a question's words cover the query's stemmed words
    /// `stems`: [`PARTIAL_MATCH`] times the share they have, when that is
    /// at least [`QUESTION_SHARE`] of at least [`QUESTION_QUERY_WORDS`].
    /// And whether the query asks the question as a whole: it has all of
    /// the query's words, and the query at least [`QUESTION_TITLE_SHARE`]
    /// of its title's. A question without `topic_word`, the query's rarest
    /// word, matches not at all. The question's words are its title and
    /// tags, or the title of a duplicate of it and its tags, whichever
    /// covers the query best ([`Page::asked_as`]).
    fn question_match(
        &self,
        page: &Page,
        stems: &[String],
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
        let mut best = (0.0f32, false);
        for (title, topic) in page.asked_as() {
            let words: HashSet<String> = analysis::tokens(&self.stemmed, &topic)
                .into_iter()
                .collect();
            if topic_word.is_some_and(|word| !words.contains(word)) {
                continue;
            }
            let share = stems.iter().filter(|stem| words.contains(*stem)).count() as f32
                / stems.len() as f32;
            if share < QUESTION_SHARE {
                continue;
            }
            let title: HashSet<String> =
                analysis::tokens(&self.stemmed, title).into_iter().collect();
            let asked = share >= 1.0
                && !title.is_empty()
                && title.iter().filter(|word| stems.contains(word)).count() as f32
                    >= QUESTION_TITLE_SHARE * title.len() as f32;
            let found = (PARTIAL_MATCH * share, asked);
            if found.0 > best.0 || found.0 == best.0 && found.1 && !best.1 {
                best = found;
            }
        }
        best
    }

    /// Whether the query `words` are a book's, podcast's or paper's title
    /// followed by words of its author's name or by what it is ("book",
    /// "novel", "podcast", "paper"): "dune frank herbert", "the great gatsby
    /// book", "hardcore history podcast", "random forests breiman". A
    /// paper's year and venue may follow too, and a paper's whole title of
    /// [`PAPER_TITLE_WORDS`] words or more asks for it alone.
    fn book_match(&self, page: &Page, words: &[String]) -> bool {
        let (byline, kinds): (&str, &[&str]) = match page.set.as_str() {
            BOOKS_SET => ("Book by ", &["book", "novel"]),
            PODCASTS_SET => ("Podcast by ", &["podcast"]),
            PAPERS_SET => ("Paper by ", &["paper"]),
            MUSIC_SET if page.is_song() => ("Song by ", &["song"]),
            MUSIC_SET => ("Album by ", &["album"]),
            _ => return false,
        };
        let by = page
            .description
            .as_deref()
            .and_then(|d| d.strip_prefix(byline))
            .unwrap_or("");
        // "Book by AUTHOR, YEAR", "Podcast by AUTHOR · CATEGORY", "Paper by
        // AUTHOR et al., YEAR, VENUE".
        let author = if page.set == PAPERS_SET {
            by.split(", ")
                .collect::<Vec<_>>()
                .join(" ")
                .replace(" et al.", "")
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
        let author: HashSet<String> = analysis::tokens(&self.words, &author).into_iter().collect();
        // The title, or the title without its subtitle: "Frankenstein" for
        // "Frankenstein; or, The Modern Prometheus".
        let short = page.title.split([':', ';']).next().unwrap_or("");
        [page.title.as_str(), short]
            .into_iter()
            .enumerate()
            .any(|(i, title)| {
                let title = analysis::tokens(&self.words, title);
                // Songs and albums are as often asked for artist first:
                // "vampire weekend step", "nirvana nevermind album",
                // "beatles hey jude".
                let unthe = match artist.as_slice() {
                    [the, rest @ ..] if the == "the" && !rest.is_empty() => rest,
                    artist => artist,
                };
                if page.set == MUSIC_SET && !artist.is_empty() && !title.is_empty() {
                    if let Some(rest) = words
                        .strip_prefix(artist.as_slice())
                        .or_else(|| words.strip_prefix(unthe))
                        .and_then(|rest| rest.strip_prefix(title.as_slice()))
                    {
                        if rest.is_empty()
                            || matches!(rest, [word] if kinds.contains(&word.as_str()))
                        {
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
                // "hey jude by the beatles".
                let rest = match rest {
                    [by, rest @ ..] if by == "by" && !rest.is_empty() => rest,
                    rest => rest,
                };
                rest.iter().all(|word| author.contains(word))
                    || matches!(rest, [word] if kinds.contains(&word.as_str()))
            })
            || (page.set == PODCASTS_SET && self.podcast_named(page, words))
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
        // popularity decides between them.
        if key(&page.title) == joined
            || key(base_title(&page.title)) == joined
            || page.aliases.iter().any(|alias| key(alias) == joined)
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

/// Least listeners of a song or album that [`PageSearcher::known_song`]
/// takes a search of its title alone to be for: about the 7,000 most
/// listened.
pub const KNOWN_SONG_LISTENERS: u64 = 50_000;
/// How many times the listeners of every other song or album of its title
/// such a one has.
pub const KNOWN_SONG_MARGIN: u64 = 5;

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

/// How much more read ([`PageHit::popularity`]) than the article about
/// the best site's subject an article the query names must be to come
/// before that site ([`crate::Hit::demand`]): about twice the views on
/// English Wikipedia.
pub const DEMAND_MARGIN: f32 = 0.05;

/// The text match ([`crate::Hit::text_score`]) below which a site the
/// query does not name in full comes after an article the query names.
pub const WEAK_SITE_MATCH: f32 = 0.5;

/// How many of the first sites [`PageSearcher::note_demand`] notes.
pub const DEMAND_NOTED: usize = 3;

/// Most site results looked through by [`lift_named_sites`].
const LIFTED_FROM: usize = 5;
/// How much better known than a site the whole query names the page's
/// official site must be to be put above it: global.toyota (0.62) stays
/// below toyota.com (0.47), aliexpress.com (0.75) goes above aliexpress.us
/// (0.41).
const LIFT_LINK_MARGIN: f32 = 0.2;

/// Leaves out the sites that lack some of the query's main words
/// ([`crate::Hit::missing_words`]) when the whole query is the name of an
/// article or Wikidata item: "toy story" names the film, so rawstory.com
/// and codastory.com, having one word of it, are no answer. The page's
/// own site stays. Returns how many were left out.
pub fn drop_namesakes_of_words(sites: &mut Vec<crate::Hit>, pages: &[PageHit]) -> usize {
    let Some(named) = pages
        .iter()
        .find(|hit| hit.named && hit.page.item.is_some())
    else {
        return 0;
    };
    let own = named.page.site.as_deref();
    let before = sites.len();
    sites.retain(|hit| !hit.missing_words || Some(hit.domain.as_str()) == own);
    before - sites.len()
}

/// Adds the official site of the best article the query names when the
/// sites found leave it out, as the [`LIFTED_FROM`]th site or last, so
/// that [`lift_named_sites`] can weigh it: "better call saul" names the
/// article whose site is amc.com, which no word of the query matches.
/// `site` gives the site of a domain, crawled or not: the page's site is
/// the item's official website in Wikidata, which is enough to list it
/// (adultswim.com for "rick and morty", never fetched on some nodes).
/// Returns whether it added one.
pub fn add_named_site(
    sites: &mut Vec<crate::Hit>,
    pages: &[PageHit],
    site: impl Fn(&str) -> Option<crate::Hit>,
) -> bool {
    let Some(domain) = pages
        .iter()
        .find(|hit| hit.named && hit.page.item.is_some())
        .and_then(|hit| hit.page.site.as_deref())
    else {
        return false;
    };
    if sites.is_empty() || sites.iter().any(|hit| hit.domain == domain) {
        return false;
    }
    let Some(mut added) = site(domain) else {
        return false;
    };
    let at = sites.len().min(LIFTED_FROM - 1);
    // Scored between its neighbours, so a later sort by score keeps it
    // there.
    let above = sites[at - 1].score;
    added.score = match sites.get(at) {
        Some(below) => (above + below.score) / 2.0,
        None => above - above.abs().max(1.0) * 1e-3,
    };
    sites.insert(at, added);
    true
}

/// Puts first the official site of the best page the query names, when
/// it is among the first [`LIFTED_FROM`] sites: what Wikipedia and
/// Wikidata call exactly what was searched for says which site it is.
/// "youtube music" names the article YouTube Music, whose site is
/// youtube.com, so youtube.com goes above youtube.de; "google maps" puts
/// google.com above googlemaps.com. An official site the whole query
/// names keeps first place.
///
/// The homepage of a repository the query names goes first too when the
/// query names that site as well: "awesome python" puts awesome-python.com,
/// with vinta/awesome-python under it, above python.org. Never crawled, it
/// would otherwise be taken for a mere spelling of the query.
pub fn lift_named_sites(sites: &mut [crate::Hit], pages: &[PageHit]) {
    let shown = sites.len().min(LIFTED_FROM);
    let lifted = |hit: &PageHit| {
        let site = hit.page.site.as_deref()?;
        let at = sites[..shown].iter().position(|s| s.domain == site)?;
        let ok = if hit.page.item.is_some() {
            sites[at].official
        } else {
            hit.page.set == GITHUB_SET && sites[at].named
        };
        ok.then_some(at)
    };
    let article = pages
        .iter()
        .find(|hit| hit.named && hit.page.item.is_some() && hit.page.set != FILMS_SET);
    let repo = pages
        .iter()
        .find(|hit| hit.named && hit.page.set == GITHUB_SET);
    let Some(at) = article.and_then(lifted).or_else(|| repo.and_then(lifted)) else {
        return;
    };
    // An official or well-known site named by all of the query stays
    // first: google.com for "google", not about.google, Google's own site
    // in Wikidata. So does any site named by all of it when the page's
    // site is too and is not far better known: both are names of what was
    // searched for, and the ranking already weighed them, so toyota.com
    // stays above global.toyota for "toyota". aliexpress.us, though, does
    // not stay above aliexpress.com, which far more sites link to.
    let about_as_known = sites[0].link_score + LIFT_LINK_MARGIN >= sites[at].link_score;
    if at > 0
        && sites[0].named
        && ((sites[at].named && about_as_known)
            || sites[0].official
            || sites[0].link_score >= crate::WELL_KNOWN_LINK_SCORE)
    {
        return;
    }
    sites[..=at].rotate_right(1);
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
///
/// A page the learned ranking listed on its own ([`PageHit::learned`])
/// stays where it put it: before the same site, or last, except that a
/// docs page found by its words never comes before the best site.
pub fn place_pages(query: &str, sites: &[crate::Hit], mut pages: Vec<PageHit>) -> Vec<PlacedPage> {
    if !asks_for_podcasts(query) {
        // Podcasts it does not ask for take no other page's place
        // ([`keep_page_rules`]).
        pages.sort_by_key(|hit| hit.page.set == PODCASTS_SET);
    }
    let mut placed = place_pages_by_rules(query, sites, pages);
    for page in placed.iter_mut().filter(|p| p.under.is_none()) {
        match &page.hit.learned {
            Some(LearnedPlace::Before(domain)) => {
                if let Some(at) = sites.iter().position(|s| &s.domain == domain) {
                    page.at = at;
                }
            }
            Some(LearnedPlace::Last) => page.at = sites.len(),
            None => {}
        }
    }
    keep_page_rules(query, sites, &mut placed);
    placed
}

/// The places no ranking overrides, applied after [`place_pages`] and
/// after [`crate::learned::reorder`]: a docs page found by its words never
/// comes before the best site unless that site is the docs' own (see
/// [`docs_kept_below`]), and, unless the query asks for them,
/// podcasts come after the best site and the other pages listed on their
/// own ("better call saul" wants amc.com and the article first).
pub fn keep_page_rules(query: &str, sites: &[crate::Hit], placed: &mut Vec<PlacedPage>) {
    for page in placed.iter_mut().filter(|p| p.under.is_none()) {
        if docs_kept_below(&page.hit, sites) {
            page.at = page.at.max(1).min(sites.len());
        }
    }
    if asks_for_podcasts(query) {
        return;
    }
    let others = placed
        .iter()
        .filter(|p| p.under.is_none() && p.hit.page.set != PODCASTS_SET)
        .map(|p| p.at)
        .max()
        .unwrap_or(0);
    let (podcasts, mut rest): (Vec<PlacedPage>, Vec<PlacedPage>) = std::mem::take(placed)
        .into_iter()
        .partition(|p| p.under.is_none() && p.hit.page.set == PODCASTS_SET);
    for mut page in podcasts {
        page.at = page.at.max(others).max(1).min(sites.len());
        rest.push(page);
    }
    *placed = rest;
}

/// `query` with its last word in the other number
/// ([`plumb_core::other_number`]): "tariffs" -> "tariff",
/// "no kings protest" -> "no kings protests". `None` when the word has no
/// other number ("news") or the query is an address.
fn last_word_in_other_number(query: &str) -> Option<String> {
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

/// Whether `query` asks for podcasts or episodes.
fn asks_for_podcasts(query: &str) -> bool {
    query.split_whitespace().any(|word| {
        matches!(
            word.to_lowercase().as_str(),
            "podcast" | "podcasts" | "episode" | "episodes"
        )
    })
}

fn place_pages_by_rules(query: &str, sites: &[crate::Hit], pages: Vec<PageHit>) -> Vec<PlacedPage> {
    let site_named = sites.first().is_some_and(|hit| hit.named);
    let query_word = squash(query);
    let organizations_site = sites.first().is_some_and(|site| {
        let label = squash(site.domain.split('.').next().unwrap_or(""));
        // A site of government called after the page and named by the
        // query: fafsa.gov for "fafsa", not ada.gov (the Americans with
        // Disabilities Act) for "ada lovelace".
        let government = site.named && is_government(&site.domain);
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
    // A named page goes first only when the best site may be a namesake:
    // one less known than the page is read, or, when Wikipedia has an
    // article about what the site is, one whose article is read far less
    // than the page. What people look up decides between namesakes, not
    // how big the site is: "mars" means the planet, read many times more
    // than Mars Inc. of mars.com; "napoleon" the emperor, not Napoleon,
    // North Dakota.
    // When the site and an article are both called just what was searched
    // for, the article is the one Wikipedia gives that name to, so being
    // read more at all is enough ("password manager" still means a site): on
    // plumbsearch.org the planet "Mars" was read 1.7 times as much as Mars
    // Inc. that day.
    // A site that matches the query only a little is no namesake at all:
    // the article "Nikola Tesla" before tesla.com, "Genghis Khan" before
    // khanacademy.org. One the query names by an official name or as a
    // kind of thing ("veterans affairs", "search engine") matches it in
    // full and is not passed over.
    let page_first = |page: &PageHit| match sites.first() {
        None => true,
        Some(_) if organizations_site => false,
        Some(site)
            if !site.named
                && site.placing_text_score.unwrap_or(site.text_score) < WEAK_SITE_MATCH
                && page.page.is_article() =>
        {
            true
        }
        Some(site) => match site.demand {
            Some(demand) if page.page.is_article() => {
                let margin = if site.named && squash(&page.page.title) == query_word {
                    0.0
                } else {
                    DEMAND_MARGIN
                };
                page.popularity > demand + margin
            }
            _ => !site.official && page.popularity > site.link_score,
        },
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
                // The best page about the site goes under it, unless a
                // later one is named by the query: "google maps" carries
                // Google Maps under google.com, not Google.
                match placed.iter_mut().find(|p| p.under.as_deref() == Some(site)) {
                    None => placed.push(PlacedPage {
                        under: Some(site.to_string()),
                        at: 0,
                        hit,
                    }),
                    Some(carried) if hit.named && !carried.hit.named => carried.hit = hit,
                    Some(_) => {}
                }
                continue;
            }
        }
        // An item without an article is only told apart from its
        // namesakes by its website.
        if hit.page.set == WIKIDATA_SET {
            continue;
        }
        // One docs page found by its words is enough: more crowd out the
        // questions that answer the same search.
        let docs_found = |p: &PlacedPage| p.hit.page.set == DOCS_SET && !p.hit.named;
        if docs_found_by_words(&hit) && placed.iter().any(docs_found) {
            continue;
        }
        // The first docs page found by its words is listed even when
        // questions took the places for pages: "react usestate hook" wants
        // React's page as well as the questions on it.
        if listed >= most && !docs_found_by_words(&hit)
            || !(hit.named || hit.score >= MIN_PARTIAL_SCORE)
        {
            continue;
        }
        let at = if docs_found_by_words(&hit) {
            // A docs page found by its words comes after the best site:
            // "docker desktop download" wants docker.com first.
            1
        } else if hit.whole {
            // Only the best of them leads: another edition of the book
            // comes after the best site.
            let led = placed.iter().any(|p| p.hit.whole);
            usize::from(site_named || led)
        } else if !hit.named && hit.page.topic().is_some() {
            // A question or paper with most of the query's words: the best
            // question leads a search asked as a question that names no
            // site, as "how to unclog a drain" wants the answer before a
            // site that shares a word with it (cityofdrain.org). A search
            // that only names something ("irs refund status") keeps its
            // site first.
            // A reference page leads too when the best site matches the
            // query only a little: "foul smelling stool" wants the symptom
            // page, not a site called "stool".
            let led = placed.iter().any(|p| {
                p.at == 0
                    && p.under.is_none()
                    && (p.hit.page.is_question() || p.hit.page.is_site_page())
            });
            let weak_site = sites.first().is_some_and(|site| {
                !site.named && site.placing_text_score.unwrap_or(site.text_score) < WEAK_SITE_MATCH
            });
            let leads = !site_named
                && !led
                && (hit.page.is_question() && asked_as_question(query)
                    || hit.page.is_site_page() && (asked_as_question(query) || weak_site));
            usize::from(!leads)
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

/// Cuts `hits`, best first, to `limit`, keeping the best docs page and the
/// best subpage among them: the many Stack Overflow questions with the
/// words of "javascript array sort" are more read than MDN's page on it,
/// and would crowd it out.
fn truncate_keeping_inner_pages(hits: &mut Vec<PageHit>, limit: usize) {
    for set in [DOCS_SET, SUBPAGES_SET] {
        if hits.len() <= limit || limit == 0 || hits[..limit].iter().any(|h| h.page.set == set) {
            continue;
        }
        let Some(best) = hits.iter().position(|h| h.page.set == set) else {
            continue;
        };
        // In place of the last hit that is not one kept already.
        let Some(last) = (0..limit)
            .rev()
            .find(|&i| hits[i].page.set != DOCS_SET && hits[i].page.set != SUBPAGES_SET)
        else {
            continue;
        };
        let hit = hits.remove(best);
        hits.insert(last, hit);
    }
    hits.truncate(limit);
}

/// Whether `hit` is a docs page the search found by most of its words
/// rather than named.
pub(crate) fn docs_found_by_words(hit: &PageHit) -> bool {
    hit.page.set == DOCS_SET && !hit.named
}

/// Whether `hit` is a docs page found by its words that must come after
/// the best of `sites`: "python package index" wants pypi.org first, and
/// "bash parameter expansion" anything but expansion.com. Only the docs'
/// own site may come after it, as python.org for docs.python.org's
/// "python sorting techniques".
pub(crate) fn docs_kept_below(hit: &PageHit, sites: &[crate::Hit]) -> bool {
    docs_found_by_words(hit)
        && sites
            .first()
            .is_some_and(|best| !docs_of(&hit.page, &best.domain))
}

/// Whether the docs page `page` is on `domain` or one of its subdomains.
fn docs_of(page: &Page, domain: &str) -> bool {
    plumb_core::host_of(&page.url).is_some_and(|host| {
        host == domain
            || host
                .strip_suffix(domain)
                .is_some_and(|sub| sub.ends_with('.'))
    })
}

/// Whether `query` asks about something in the docs of the docs page
/// `page`'s site (see [`plumb_core::docs::asks_about`]): "python sort
/// list" does for a page of Python's docs; "note taking app" for none.
fn docs_asked(page: &Page, query: &str) -> bool {
    plumb_core::docs::site_of_url(&page.url)
        .is_some_and(|site| plumb_core::docs::asks_about(site, query))
}

/// Words a search asked as a question starts with.
const QUESTION_WORDS: &[&str] = &[
    "how", "why", "what", "whats", "what's", "when", "where", "which", "who", "can", "could",
    "should", "do", "does", "did", "is", "are", "will", "would",
];

/// Whether `query` is asked as a question: "how to unclog a drain", "can
/// you freeze cooked rice".
pub(crate) fn asked_as_question(query: &str) -> bool {
    query
        .split_whitespace()
        .next()
        .is_some_and(|word| QUESTION_WORDS.contains(&word.to_lowercase().as_str()))
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

    fn package(registry: &str, name: &str, views: u64) -> Page {
        let registry = plumb_core::packages::registry(registry).unwrap();
        Page::from_package(Article {
            title: name.into(),
            item: Some(plumb_core::packages::package_item(registry.key, name)),
            views,
            aliases: registry
                .short_name(name)
                .map(str::to_string)
                .into_iter()
                .collect(),
            package: Some(PackageInfo {
                registry: registry.key.into(),
                name: name.into(),
                version: Some("1.0.0".into()),
                ..PackageInfo::default()
            }),
            ..Article::default()
        })
        .unwrap()
    }

    fn docs_page(url: &str, title: &str, aliases: &[&str], description: &str) -> Page {
        Page::from_docs(Article {
            title: title.into(),
            description: Some(description.into()),
            item: Some(url.into()),
            views: 5_000,
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
            ..Article::default()
        })
        .unwrap()
    }

    fn reference_page(url: &str, title: &str, description: &str) -> Page {
        Page::from_reference(Article {
            title: title.into(),
            description: Some(description.into()),
            item: Some(url.into()),
            views: 5_000,
            ..Article::default()
        })
        .unwrap()
    }

    #[test]
    fn reference_pages_are_found_by_most_words_and_lead_weak_sites() {
        let stool = reference_page(
            "https://www.healthline.com/health/foul-smelling-stool",
            "Foul-Smelling Stool: Causes and Treatment",
            "Foul-smelling stools have an unusually strong, putrid smell.",
        );
        assert_eq!(stool.set_name(), "healthline.com");
        assert_eq!(stool.set_domain(), "healthline.com");
        let prioritize = reference_page(
            "https://www.merriam-webster.com/dictionary/prioritize",
            "Prioritize Definition & Meaning",
            "The meaning of PRIORITIZE is to list or rate in order of priority.",
        );
        let (_dir, searcher) = searcher(&[stool.clone(), prioritize.clone()]);
        let found = |query: &str| -> Vec<(String, bool)> {
            searcher
                .search(query, 10)
                .unwrap()
                .into_iter()
                .map(|hit| (hit.page.title, hit.named))
                .collect()
        };
        assert_eq!(found("foul smelling stool"), [(stool.title.clone(), false)]);
        // Two words are enough.
        assert_eq!(
            found("prioritize meaning"),
            [(prioritize.title.clone(), false)]
        );
        // Part of its title names nothing; its whole title does.
        assert!(found("stool").is_empty());
        assert_eq!(
            found("Prioritize Definition & Meaning"),
            [(prioritize.title.clone(), true)]
        );
        assert!(found("smelling salts").is_empty());
        let read = Page::from_set(
            REFERENCE_SET,
            Article {
                title: stool.title.clone(),
                description: stool.description.clone(),
                item: Some(stool.url.clone()),
                views: stool.views,
                ..Article::default()
            },
        );
        assert_eq!(read, Some(stool.clone()));
        assert!(Page::has_reader(REFERENCE_SET));

        // Asked in full, it leads any site the query does not name; found
        // by most of its words, only one that shares a word with the query.
        let placed_at = |query: &str, best: crate::Hit| {
            let hit = searcher.search(query, 10).unwrap().remove(0);
            let sites = [best, site("other.com", false)];
            place_pages(query, &sites, vec![hit])[0].at
        };
        let mut weak = site("stool.com", false);
        weak.text_score = 0.2;
        assert_eq!(
            placed_at("foul smelling stool", site("stool.com", false)),
            0
        );
        assert_eq!(placed_at("foul smelling stool", site("stool.com", true)), 1);
        assert_eq!(placed_at("stool smell", weak), 0);
        assert_eq!(placed_at("stool smell", site("stool.com", false)), 1);
        assert_eq!(placed_at("stool smell", site("stool.com", true)), 1);
    }

    #[test]
    fn docs_pages_are_found_by_product_and_title_or_most_words() {
        let sorting = docs_page(
            "https://docs.python.org/3/howto/sorting.html",
            "Sorting Techniques",
            &["Python Sorting Techniques", "Sorting Techniques Python"],
            "Python lists have a built-in list.sort() method that modifies the list in-place.",
        );
        assert_eq!(sorting.set_name(), "docs.python.org");
        assert_eq!(sorting.set_domain(), "docs.python.org");
        let glossary = docs_page(
            "https://docs.python.org/3/glossary.html",
            "Glossary",
            &["Python Glossary", "Glossary Python"],
            "The default Python prompt of the interactive shell.",
        );
        let (_dir, searcher) = searcher(&[
            page("Glossary", 2_000, &[]),
            sorting.clone(),
            glossary.clone(),
        ]);
        let found = |query: &str| -> Vec<(String, bool)> {
            searcher
                .search(query, 10)
                .unwrap()
                .into_iter()
                .filter(|hit| hit.page.set == DOCS_SET)
                .map(|hit| (hit.page.title, hit.named))
                .collect()
        };
        assert_eq!(
            found("python sorting techniques"),
            [("Sorting Techniques".to_string(), true)]
        );
        assert_eq!(found("python glossary"), [("Glossary".to_string(), true)]);
        // Its title alone names nothing.
        assert!(found("glossary").is_empty());
        assert!(found("sorting techniques").is_empty());
        // Most of the query's words, with the product named.
        assert_eq!(
            found("sort a list in python"),
            [("Sorting Techniques".to_string(), false)]
        );
        assert!(found("sort a list in place").is_empty());
        assert!(Page::from_docs(Article {
            item: Some("http://docs.python.org/3/".into()),
            ..Article::default()
        })
        .is_none());
        let read = Page::from_set(
            DOCS_SET,
            Article {
                title: sorting.title.clone(),
                description: sorting.description.clone(),
                item: Some(sorting.url.clone()),
                views: sorting.views,
                aliases: sorting.aliases.clone(),
                ..Article::default()
            },
        );
        assert_eq!(read, Some(sorting.clone()));
        assert!(Page::has_reader(DOCS_SET));

        // Found by its words, it comes after the best site unless that is
        // the docs' own.
        let placed_at = |best: &str| {
            let sites = [site(best, false), site("other.com", false)];
            let mut placed = vec![PlacedPage {
                at: 0,
                under: None,
                hit: PageHit {
                    page: sorting.clone(),
                    score: 0.9,
                    named: false,
                    popularity: 1.0,
                    whole: true,
                    learned: None,
                },
            }];
            keep_page_rules("sort a list in python", &sites, &mut placed);
            placed[0].at
        };
        assert_eq!(placed_at("pypi.org"), 1);
        assert_eq!(placed_at("python.org"), 0);
        assert_eq!(placed_at("docs.python.org"), 0);
        assert_eq!(placed_at("cpython.org"), 1);
    }

    #[test]
    fn docs_pages_named_then_more_words_and_crowded_by_questions_are_listed() {
        let use_state = docs_page(
            "https://react.dev/reference/react/useState",
            "useState",
            &["React useState", "useState React"],
            "useState is a React Hook that lets you add a state variable to your component.",
        );
        let question = |title: &str, item: &str, views| {
            Page::from_question(Article {
                title: title.into(),
                description: Some("reactjs, react-hooks".into()),
                item: Some(item.into()),
                views,
                ..Article::default()
            })
        };
        let mut pages = vec![use_state.clone()];
        for i in 0..12 {
            pages.push(question(
                &format!("React useState hook question {i}"),
                &format!("{i}"),
                1_000_000 + i,
            ));
        }
        let (_dir, searcher) = searcher(&pages);
        // Named by its product and title, then a word of its description:
        // not a book's title and its author.
        let hits = searcher.search("react usestate hook", 10).unwrap();
        assert_eq!(hits.len(), 10);
        // The questions outnumber it, and are more read, but it stays.
        let docs: Vec<&PageHit> = hits.iter().filter(|h| h.page.set == DOCS_SET).collect();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].page.title, "useState");
        assert!(!docs[0].named);
        // Listed after the best site, besides the questions that took
        // the places for pages.
        let sites = [site("react.dev", true), site("other.com", false)];
        let placed = place_pages("react usestate hook", &sites, hits);
        let alone: Vec<&str> = placed
            .iter()
            .filter(|p| p.under.is_none())
            .map(|p| p.hit.page.title.as_str())
            .collect();
        assert!(alone.contains(&"useState"), "{alone:?}");
        assert_eq!(
            placed
                .iter()
                .find(|p| p.hit.page.set == DOCS_SET)
                .map(|p| p.at),
            Some(1)
        );
    }

    #[test]
    fn subpages_are_found_like_reference_pages() {
        let page = |url: &str, title: &str, description: &str| {
            Page::from_set(
                SUBPAGES_SET,
                Article {
                    title: title.into(),
                    description: Some(description.into()),
                    item: Some(url.into()),
                    views: 2_000,
                    ..Article::default()
                },
            )
            .unwrap()
        };
        let perft = page(
            "https://chessprogramming.org/Perft_Results",
            "Perft Results",
            "Perft results of the initial position and Kiwipete.",
        );
        let sp811 = page(
            "https://www.nist.gov/pml/special-publication-811",
            "Special Publication 811",
            "NIST Guide to the SI, with conversion factors.",
        );
        assert!(Page::has_reader(SUBPAGES_SET));
        assert!(perft.is_site_page());
        assert_eq!(perft.set_domain(), "chessprogramming.org");
        assert_eq!(sp811.set_name(), "nist.gov");
        assert!(Page::from_set(SUBPAGES_SET, Article::default()).is_none());
        let (_dir, searcher) = searcher(&[perft.clone(), sp811.clone()]);
        let found = |query: &str| -> Vec<(String, bool)> {
            searcher
                .search(query, 10)
                .unwrap()
                .into_iter()
                .map(|hit| (hit.page.title, hit.named))
                .collect()
        };
        // Named by its whole title, or found by most words.
        assert_eq!(found("perft results"), [(perft.title.clone(), true)]);
        assert_eq!(
            found("nist special publication 811"),
            [(sp811.title.clone(), false)]
        );
        // Part of a title names nothing.
        assert!(found("perft").is_empty());
    }

    #[test]
    fn packages_are_found_only_when_asked_for() {
        let (_dir, searcher) = searcher(&[
            page("Serde", 5_000, &[]),
            page("Requests", 900, &[]),
            package("crates", "serde", 1_000_000_000),
            package("crates", "book", 20),
            package("pypi", "requests", 1_000_000_000),
            package("npm", "requests", 2_000),
            package("go", "github.com/gin-gonic/gin", 1_000_000_000),
        ]);
        let urls = |query: &str| -> Vec<String> {
            searcher
                .search(query, 10)
                .unwrap()
                .into_iter()
                .filter(|hit| hit.page.package.is_some())
                .map(|hit| hit.page.url)
                .collect()
        };
        assert!(urls("serde").is_empty());
        assert!(urls("requests").is_empty());
        assert_eq!(urls("serde crate"), ["https://crates.io/crates/serde"]);
        assert_eq!(
            urls("latest version of requests python"),
            ["https://pypi.org/project/requests/"]
        );
        assert_eq!(
            urls("requests npm"),
            ["https://www.npmjs.com/package/requests"]
        );
        // Either registry's, the most used first.
        assert_eq!(
            urls("requests package"),
            [
                "https://pypi.org/project/requests/",
                "https://www.npmjs.com/package/requests"
            ]
        );
        // A language alone asks only for well-known packages.
        assert!(urls("rust book").is_empty());
        assert_eq!(urls("book crate"), ["https://crates.io/crates/book"]);
        assert_eq!(
            urls("gin golang"),
            ["https://pkg.go.dev/github.com/gin-gonic/gin"]
        );
        let hit = searcher.search("serde crate", 10).unwrap().remove(0);
        assert!(hit.named && hit.page.set_name() == "crates.io");
    }

    #[test]
    fn plurals_name_the_article_titled_in_the_singular() {
        let (_dir, s) = searcher(&[
            page("Tariff", 90_000, &[]),
            page("No Kings protests", 50_000, &[]),
            page("Kings", 1_000, &[]),
        ]);
        let found = |query: &str, sites: &[crate::Hit]| {
            let mut hits = s.search(query, 5).unwrap();
            s.add_other_number(query, sites, &mut hits, 5).unwrap();
            hits
        };
        let tariffs_net = known_site("tariffs.net", true, 0.015);
        let hits = found("tariffs", std::slice::from_ref(&tariffs_net));
        assert_eq!(titles(&hits)[0], "Tariff");
        assert!(hits[0].named);
        let hits = found("no kings protest", &[]);
        assert_eq!(titles(&hits)[0], "No Kings protests");
        assert!(hits[0].named);
        // Named as typed: no other number is tried.
        let hits = found("kings", &[]);
        assert_eq!(titles(&hits)[0], "Kings");
        // A site that answers the query well keeps it to the sites.
        let mut tariff_site = known_site("tariffs.gov", false, 0.6);
        tariff_site.text_score = 0.9;
        assert!(found("tariffs", &[tariff_site])
            .iter()
            .all(|hit| !hit.named));
        let mut official = tariffs_net;
        official.official = true;
        assert!(found("tariffs", &[official]).iter().all(|hit| !hit.named));
        assert_eq!(last_word_in_other_number("news"), None);
        assert_eq!(last_word_in_other_number("tariffs.net"), None);
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
    fn questions_without_the_word_asked_about_are_not_found() {
        let question = |title: &str, tags: &str, item: &str| {
            Page::from_question(Article {
                title: title.into(),
                description: Some(tags.into()),
                item: Some(item.into()),
                views: 100_000,
                ..Article::default()
            })
        };
        let pages = [
            question("How do I get rid of my bounty?", "bounty, meta", "1"),
            question(
                "How to get rid of large gaps in text",
                "ms-word, layout",
                "2",
            ),
            question("How do I get rid of aphids on roses?", "pests, roses", "3"),
        ];
        let (_dir, s) = searcher(&pages);
        let hits = s.search("how to get rid of aphids", 5).unwrap();
        assert_eq!(titles(&hits), ["How do I get rid of aphids on roses?"]);
    }

    #[test]
    fn questions_are_found_in_their_duplicates_words() {
        let undo = Page::from_question(Article {
            title: "How do I undo the most recent local commits in Git?".into(),
            description: Some("git, version-control, git-commit, undo".into()),
            item: Some("927358".into()),
            views: 14_000_000,
            aliases: vec!["Revert to a previous Git commit without losing history".into()],
            ..Article::default()
        });
        let (_dir, s) = searcher(&[undo]);
        let hits = s
            .search("revert previous commit losing history", 5)
            .unwrap();
        assert_eq!(
            titles(&hits),
            ["How do I undo the most recent local commits in Git?"]
        );
        assert!(hits[0].whole);
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
        // Most of the query's words, asked as a question naming no site:
        // the question still leads, ahead of a site that only shares a
        // word with it.
        let asked = "how to delete git branch remotely fast";
        let hits = s.search(asked, 5).unwrap();
        assert!(!hits[0].whole);
        assert_eq!(place_pages(asked, &sites, hits)[0].at, 0);
        // Not asked as a question: after the best site.
        let hits = s.search("delete git branch remotely fast", 5).unwrap();
        assert_eq!(
            place_pages("delete git branch remotely fast", &sites, hits)[0].at,
            1
        );
        // A site the question names stays first.
        let named = [known_site("git-scm.com", true, 0.9)];
        let hits = s.search(asked, 5).unwrap();
        assert_eq!(place_pages(asked, &named, hits)[0].at, 1);
    }

    #[test]
    fn sites_called_like_organizations_stay_first() {
        let mut bank = page("U.S. Bancorp", 900_000, &["US Bank"]);
        bank.description = Some("American multinational banking institution".into());
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
                learned: None,
            }],
        );
        assert_eq!(placed[0].at, 0);
    }

    #[test]
    fn namesakes_are_weighed_by_how_much_their_articles_are_read() {
        let mut planet = page("Mars", 2_000_000, &[]);
        planet.description = Some("Fourth planet from the Sun".into());
        let mut company = page("Mars Inc.", 20_000, &["Mars, Incorporated"]);
        company.description = Some("American manufacturer of confectionery".into());
        company.site = Some("mars.com".into());
        let mut repo = Page::from_repo(Article {
            title: "mars/mars".into(),
            views: 9_000_000,
            ..Article::default()
        });
        repo.site = Some("mars.com".into());
        let (_dir, s) = searcher(&[planet, company, repo]);
        // Only articles say what a site is read about.
        assert!((s.site_popularity("mars.com").unwrap().unwrap() - 0.68).abs() < 0.01);
        assert_eq!(s.site_popularity("planet.example").unwrap(), None);

        let mut sites = [known_site("mars.com", true, 0.65)];
        sites[0].official = true;
        let place = |sites: &[crate::Hit]| {
            let hits = s.search("mars", 5).unwrap();
            assert_eq!(hits[0].page.title, "Mars");
            place_pages("mars", sites, hits)
                .into_iter()
                .find(|p| p.hit.page.title == "Mars")
                .unwrap()
                .at
        };
        // An official site was never put after an article.
        assert_eq!(place(&sites), 1);
        // The planet is read a hundred times more than the company.
        s.note_demand(&mut sites).unwrap();
        assert!(sites[0].demand.is_some());
        assert_eq!(place(&sites), 0);
        // Not when the company is read about as much.
        sites[0].demand = Some(1.0);
        assert_eq!(place(&sites), 1);
        // The article Wikipedia calls "Mars" needs only to be read more.
        let planet = s.search("mars", 5).unwrap()[0].popularity;
        sites[0].demand = Some(planet - 0.02);
        assert_eq!(place(&sites), 0);
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

    #[test]
    fn words_of_things_are_corrected_from_page_names() {
        let mut pages = Vec::new();
        for i in 0..25 {
            pages.push(page(&format!("Anubis statue {i}"), 10, &[]));
            pages.push(page(&format!("Budapest hotels {i}"), 10, &[]));
        }
        pages.push(page("Anubas (beetle)", 1, &[]));
        for i in 0..5 {
            pages.push(page(&format!("Perfi album {i}"), 10, &[]));
        }
        for i in 0..25 {
            pages.push(page(&format!("Erft river {i}"), 10, &[]));
        }
        let (_dir, searcher) = searcher(&pages);
        let nothing_known = |_: &str| false;
        let suggest = |query: &str| {
            searcher
                .suggest_spelling(query, None, &nothing_known)
                .unwrap()
                .map(|s| s.query)
        };
        // Even a word one page has is taken for a slip of a far commoner one.
        assert_eq!(suggest("anubas").as_deref(), Some("anubis"));
        assert_eq!(
            suggest("budafest hotels").as_deref(),
            Some("budapest hotels")
        );
        // Known words, short words and words with digits stay.
        assert_eq!(suggest("budapest"), None);
        assert_eq!(suggest("anub"), None);
        assert_eq!(suggest("budafest2"), None);
        // A rare word is no slip of another rare one: few pages say "perfi".
        // Nor of a common one with another first letter: "erft".
        assert_eq!(suggest("perft"), None);
        // A word the sites know is spelled right.
        let sites_know = |word: &str| word == "budafest";
        assert_eq!(
            searcher
                .suggest_spelling("budafest", None, &sites_know)
                .unwrap(),
            None
        );
    }

    #[test]
    fn spellings_keep_the_words_pages_know() {
        let mut pages = Vec::new();
        for i in 0..3 {
            pages.push(page(&format!("PKCE flow {i}"), 10, &[]));
            pages.push(page(&format!("Stain removal {i}"), 10, &[]));
        }
        let (_dir, searcher) = searcher(&pages);
        let spelling = |query: &str, site: Option<&str>| crate::Spelling {
            query: query.into(),
            site: site.map(str::to_string),
            applied: false,
        };
        // Every changed word is known: no suggestion.
        assert_eq!(
            searcher
                .check_spelling("oauth2 pkce flow", spelling("oauth2 pace flow", None))
                .unwrap(),
            None
        );
        // Only the real typo is still fixed.
        assert_eq!(
            searcher
                .check_spelling(
                    "remove red wnie stain",
                    spelling("remove red wine spain", None)
                )
                .unwrap(),
            Some(spelling("remove red wine stain", None))
        );
        // Split or joined words with a known one changed: dropped.
        assert_eq!(
            searcher
                .check_spelling("stain wood", spelling("stainwood", None))
                .unwrap(),
            None
        );
        // A site's name is corrected whatever pages say.
        assert_eq!(
            searcher
                .check_spelling("stain", spelling("spain", Some("spain.info")))
                .unwrap(),
            Some(spelling("spain", Some("spain.info")))
        );
    }

    #[test]
    fn untitled_sites_take_their_article_s_title() {
        let mut notion = page("Notion (productivity software)", 500, &[]);
        notion.site = Some("notion.so".into());
        // A show whose website is a page of cbc.ca is read more than CBC's
        // own article, but it is not what cbc.ca is.
        let mut show = page("Schitt's Creek", 9_000, &[]);
        show.site = Some("cbc.ca".into());
        show.website = Some("https://www.cbc.ca/schittscreek".into());
        let mut cbc = page("CBC Television", 3_000, &[]);
        cbc.site = Some("cbc.ca".into());
        // A domain of the initials: nba.com.
        let mut nba = page("National Basketball Association", 4_000, &[]);
        nba.site = Some("nba.com".into());
        let (_dir, searcher) = searcher(&[notion, show, cbc, nba]);
        let mut sites = vec![
            site("notion.so", true),
            site("other.com", false),
            site("cbc.ca", true),
            site("nba.com", true),
        ];
        sites[1].title = Some("Other".into());
        searcher.title_untitled(&mut sites).unwrap();
        assert_eq!(sites[0].title.as_deref(), Some("Notion"));
        assert_eq!(sites[1].title.as_deref(), Some("Other"));
        assert_eq!(sites[2].title.as_deref(), Some("CBC Television"));
        assert_eq!(
            sites[3].title.as_deref(),
            Some("National Basketball Association")
        );
    }

    #[test]
    fn official_sites_with_another_page_s_title_take_their_article_s() {
        let mut post = page("New York Post", 5_000, &[]);
        post.site = Some("nypost.com".into());
        let mut valve = page("Valve Corporation", 9_000, &[]);
        valve.site = Some("steampowered.com".into());
        let mut steam = page("Steam (service)", 4_000, &[]);
        steam.site = Some("steampowered.com".into());
        let (_dir, searcher) = searcher(&[post, valve, steam]);
        let titled = |domain: &str, title: &str, official: bool| crate::Hit {
            title: Some(title.into()),
            official,
            ..site(domain, false)
        };
        let mut sites = vec![
            titled(
                "nypost.com",
                "California Post – Breaking California News",
                true,
            ),
            titled("steampowered.com", "Welcome to Steam", true),
            // Not official: nothing says which name is its own.
            titled("nypost.com", "California Post", false),
        ];
        searcher.title_untitled(&mut sites).unwrap();
        assert_eq!(sites[0].title.as_deref(), Some("New York Post"));
        assert_eq!(sites[1].title.as_deref(), Some("Welcome to Steam"));
        assert_eq!(sites[2].title.as_deref(), Some("California Post"));
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
            demand: None,
            missing_words: false,
            placing_text_score: None,
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
            learned: None,
        }
    }

    #[test]
    fn podcasts_come_after_the_site_and_the_article_unless_asked_for() {
        let sites = [site("comedycentral.com", false), site("amc.com", false)];
        let podcast = |title: &str| {
            let mut hit = found(title, None, true, 0.95);
            hit.page.set = PODCASTS_SET.into();
            hit.learned = Some(LearnedPlace::Before("comedycentral.com".into()));
            hit
        };
        let article = found("Better Call Saul", None, true, 0.9);
        let pages = vec![
            podcast("Better Call Saul Insider"),
            podcast("Better Call Saul Podcast"),
            article,
        ];
        let placed = place_pages("better call saul", &sites, pages.clone());
        let alone: Vec<(&str, usize)> = placed
            .iter()
            .filter(|p| p.under.is_none())
            .map(|p| (p.hit.page.title.as_str(), p.at))
            .collect();
        let article_at = alone[0].1;
        assert_eq!(alone[0].0, "Better Call Saul");
        assert!(alone[1..].iter().all(|&(_, at)| at >= article_at.max(1)));
        // Asked for, they stay where they were put.
        let placed = place_pages("better call saul podcast", &sites, pages);
        assert!(placed
            .iter()
            .any(|p| p.hit.page.set == PODCASTS_SET && p.at == 0));
    }

    #[test]
    fn pages_with_the_words_never_crowd_out_the_page_named() {
        let mut pages: Vec<Page> = (0..2 * CANDIDATES)
            .map(|i| {
                page(
                    &format!("How do I zoom Google Maps to marker {i}"),
                    900_000,
                    &[],
                )
            })
            .collect();
        pages.push(page("Google Maps", 22_902, &["Gmaps"]));
        let (_dir, s) = searcher(&pages);
        let hits = s.search("google maps", 10).unwrap();
        assert_eq!(titles(&hits)[0], "Google Maps");
        assert!(hits[0].named);
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
    fn the_site_of_the_page_the_query_names_goes_first() {
        let mut official = known_site("youtube.com", false, 1.0);
        official.official = true;
        let mut sites = vec![
            site("youtube.de", false),
            site("youtube.nl", false),
            official,
            site("a.com", false),
        ];
        let mut music = found("YouTube Music", Some("youtube.com"), true, 0.9);
        music.page.item = Some("Q28404534".into());
        lift_named_sites(&mut sites, &[music.clone()]);
        let order: Vec<&str> = sites.iter().map(|s| s.domain.as_str()).collect();
        assert_eq!(order, ["youtube.com", "youtube.de", "youtube.nl", "a.com"]);
        // Not for a page named only in part, nor a site Wikidata does
        // not call official.
        let mut sites = vec![site("youtube.de", false), site("youtube.com", false)];
        lift_named_sites(&mut sites, &[music.clone()]);
        assert_eq!(sites[0].domain, "youtube.de");
        let mut google = known_site("google.com", true, 1.0);
        google.official = true;
        let mut about = known_site("about.google", false, 0.6);
        about.official = true;
        let mut sites = vec![google, about];
        let mut company = found("Google", Some("about.google"), true, 0.9);
        company.page.item = Some("Q95".into());
        lift_named_sites(&mut sites, &[company]);
        assert_eq!(sites[0].domain, "google.com");
        let toyota = known_site("toyota.com", true, 0.9);
        let mut global = known_site("global.toyota", false, 0.6);
        global.official = true;
        let mut sites = vec![toyota, global];
        let mut maker = found("Toyota", Some("global.toyota"), true, 0.9);
        maker.page.item = Some("Q53268".into());
        lift_named_sites(&mut sites, &[maker]);
        assert_eq!(sites[0].domain, "toyota.com");
        // Far less well known than the official site, which both are named
        // by the query: the official site goes first.
        let us = known_site("aliexpress.us", true, 0.41);
        let mut com = known_site("aliexpress.com", true, 0.75);
        com.official = true;
        let mut sites = vec![us, com];
        let mut shop = found("AliExpress", Some("aliexpress.com"), true, 0.9);
        shop.page.item = Some("Q2647593".into());
        lift_named_sites(&mut sites, &[shop]);
        assert_eq!(sites[0].domain, "aliexpress.com");
        // Less well known than global.toyota, but scored higher, and both
        // are named by "toyota".
        let toyota = known_site("toyota.com", true, 0.47);
        let mut global = known_site("global.toyota", true, 0.62);
        global.official = true;
        let mut sites = vec![toyota, global];
        let mut maker = found("Toyota", Some("global.toyota"), true, 0.9);
        maker.page.item = Some("Q53268".into());
        lift_named_sites(&mut sites, &[maker]);
        assert_eq!(sites[0].domain, "toyota.com");
        music.named = false;
        let mut sites = vec![
            site("youtube.de", false),
            known_site("youtube.com", false, 1.0),
        ];
        sites[1].official = true;
        lift_named_sites(&mut sites, &[music]);
        assert_eq!(sites[0].domain, "youtube.de");
    }

    #[test]
    fn sites_with_some_words_of_a_named_article_are_left_out() {
        let mut sites = vec![
            site("toystory.disney.com", false),
            site("rawstory.com", false),
            site("toys.com", false),
        ];
        sites[1].missing_words = true;
        sites[2].missing_words = true;
        let mut film = found("Toy Story", Some("toys.com"), true, 0.9);
        film.page.item = Some("Q171048".into());
        // Not named in full: all stay.
        film.named = false;
        assert_eq!(drop_namesakes_of_words(&mut sites, &[film.clone()]), 0);
        film.named = true;
        assert_eq!(drop_namesakes_of_words(&mut sites, &[film]), 1);
        let order: Vec<&str> = sites.iter().map(|s| s.domain.as_str()).collect();
        // The article's own site stays.
        assert_eq!(order, ["toystory.disney.com", "toys.com"]);
    }

    #[test]
    fn the_site_of_the_article_named_is_added_when_missing() {
        let mut sites = vec![
            site("comedycentral.com", false),
            site("callofduty.com", false),
            site("walmart.com", false),
        ];
        for (i, s) in sites.iter_mut().enumerate() {
            s.score = 1.0 - i as f32 * 0.1;
        }
        let mut show = found("Better Call Saul", Some("amc.com"), true, 0.9);
        show.page.item = Some("Q3010697".into());
        let amc = |domain: &str| {
            let mut hit = known_site(domain, false, 0.7);
            hit.official = domain == "amc.com";
            Some(hit)
        };
        assert!(add_named_site(&mut sites, &[show.clone()], amc));
        let order: Vec<&str> = sites.iter().map(|s| s.domain.as_str()).collect();
        assert_eq!(
            order,
            [
                "comedycentral.com",
                "callofduty.com",
                "walmart.com",
                "amc.com"
            ]
        );
        assert!(sites[3].score < sites[2].score);
        // Once is enough, and a site the index does not hold is not added.
        assert!(!add_named_site(&mut sites, &[show.clone()], amc));
        show.page.site = Some("fans.example".into());
        assert!(!add_named_site(&mut sites, &[show], |_| None));
    }

    #[test]
    fn the_homepage_of_the_repository_the_query_names_goes_first() {
        let mut python = known_site("python.org", false, 0.9);
        python.official = true;
        let mut sites = vec![
            python.clone(),
            site("realpython.com", false),
            known_site("awesome-python.com", true, 0.2),
        ];
        let mut repo = found(
            "vinta/awesome-python",
            Some("awesome-python.com"),
            true,
            0.8,
        );
        repo.page.set = GITHUB_SET.into();
        lift_named_sites(&mut sites, &[repo.clone()]);
        assert_eq!(sites[0].domain, "awesome-python.com");
        let placed = place_pages("awesome python", &sites, vec![repo.clone()]);
        assert_eq!(placed[0].under.as_deref(), Some("awesome-python.com"));
        // Not a homepage the query does not name.
        let mut sites = vec![python, known_site("awesomelists.dev", false, 0.2)];
        repo.page.site = Some("awesomelists.dev".into());
        lift_named_sites(&mut sites, &[repo]);
        assert_eq!(sites[0].domain, "python.org");
    }

    #[test]
    fn the_page_the_query_names_goes_under_its_site() {
        let sites = [site("google.com", false), site("a.com", false)];
        let placed = place_pages(
            "google maps",
            &sites,
            vec![
                found("Google", Some("google.com"), false, 0.9),
                found("Google Maps", Some("google.com"), true, 0.8),
                found("Google Search", Some("google.com"), false, 0.7),
            ],
        );
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].under.as_deref(), Some("google.com"));
        assert_eq!(placed[0].hit.page.title, "Google Maps");
    }

    #[test]
    fn items_without_articles_only_go_under_their_site() {
        let item = |site: &str| {
            let mut hit = found("Linus Tech Tips", Some(site), true, 0.9);
            hit.page.set = WIKIDATA_SET.into();
            hit
        };
        let sites = [site("linustechtips.com", false), site("a.com", false)];
        let placed = place_pages("", &sites, vec![item("linustechtips.com")]);
        assert_eq!(placed[0].under.as_deref(), Some("linustechtips.com"));
        assert!(place_pages("", &sites, vec![item("elsewhere.com")]).is_empty());
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
        // Not one only the page's first word spells.
        let placed = place_pages(
            "ada lovelace",
            &[known_site("ada.gov", false, 0.48)],
            vec![described(
                "Ada Lovelace",
                "English mathematician (1815-1852)",
                0.9,
            )],
        );
        assert_eq!(placed[0].at, 0);
        // ...and after an official website or a well known site the query
        // names too...
        let mut official = known_site("example.org", true, 0.3);
        official.official = true;
        let known = known_site("example.com", true, 0.9);
        for site in [official, known] {
            let placed = place_pages("", &[site], vec![found("Example", None, true, 0.8)]);
            assert_eq!(placed[0].at, 1);
        }
        // ...but not after one that only shares a word with the query.
        let mut tesla = known_site("tesla.com", false, 0.7);
        tesla.official = true;
        tesla.text_score = 0.37;
        let placed = place_pages(
            "nikola tesla",
            &[tesla],
            vec![found("Nikola Tesla", None, true, 0.8)],
        );
        assert_eq!(placed[0].at, 0);
        // A site of the kind searched for matches it in full and stays.
        let mut nasa = known_site("nasa.gov", false, 0.8);
        nasa.official = true;
        let placed = place_pages(
            "space agency",
            &[nasa],
            vec![found("Space agency", None, true, 0.6)],
        );
        assert_eq!(placed[0].at, 1);
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

    fn song(title: &str, by: &str, item: &str, listeners: u64) -> Page {
        Page::from_music(Article {
            title: title.into(),
            description: Some(format!("{by}, 1968")),
            item: Some(item.into()),
            views: listeners,
            aliases: vec![format!("{title} {}", by.split_once(" by ").unwrap().1)],
            ..Article::default()
        })
        .unwrap()
    }

    #[test]
    fn songs_and_albums_are_found_by_their_name_and_artist() {
        let jude = song(
            "Hey Jude",
            "Song by The Beatles",
            "recording/a1b2c3d4-d987-4042-ae91-78d6a3267d69",
            120_000,
        );
        assert_eq!(
            jude.url,
            "https://musicbrainz.org/recording/a1b2c3d4-d987-4042-ae91-78d6a3267d69"
        );
        assert!(jude.is_song() && !jude.may_lead());
        assert_eq!(jude.set_name(), "MusicBrainz");
        // With guests, the main artist first finds it too.
        let slow = song(
            "Drive Slow",
            "Song by Kanye West feat. Paul Wall & GLC",
            "recording/a1b2c3d4-0000-4042-ae91-78d6a3267d69",
            30_000,
        );
        let (_dir, s) = searcher(std::slice::from_ref(&slow));
        for query in [
            "kanye west drive slow",
            "kanye west drive slow song",
            "drive slow kanye west",
        ] {
            let hits = s.search(query, 5).unwrap();
            assert!(
                !hits.is_empty() && hits[0].page.url == slow.url,
                "{query}: {hits:?}"
            );
        }
        let white = song(
            "The Beatles",
            "Album by The Beatles",
            "release-group/a1b2c3d4-a1db-32aa-b14f-bc9cc507b843",
            40_000,
        );
        assert!(!white.is_song());
        let (_dir, s) = searcher(&[
            jude.clone(),
            white,
            page("Hey Jude", 300_000, &[]),
            page("The Beatles", 900_000, &[]),
        ]);
        for query in [
            "hey jude the beatles",
            "hey jude beatles",
            "hey jude by the beatles",
            "hey jude song",
            "the beatles hey jude",
            "beatles hey jude",
            "beatles hey jude song",
        ] {
            let hits = s.search(query, 5).unwrap();
            assert!(
                hits[0].whole && hits[0].named && hits[0].page.url == jude.url,
                "{query}: {hits:?}"
            );
        }
        // Alone, the title asks for the article only, and part of it for
        // no song.
        let hits = s.search("hey jude", 5).unwrap();
        assert_eq!(titles(&hits), ["Hey Jude"]);
        assert!(hits[0].page.is_article());
        assert!(s
            .search("jude", 5)
            .unwrap()
            .iter()
            .all(|hit| hit.page.set != MUSIC_SET));
        // The artist alone, or with words that are not the title, asks
        // for no song.
        for query in ["the beatles", "beatles hey"] {
            assert!(
                s.search(query, 5)
                    .unwrap()
                    .iter()
                    .all(|hit| hit.page.url != jude.url),
                "{query}"
            );
        }
        let hits = s.search("the beatles album", 5).unwrap();
        assert!(hits[0].whole && hits[0].page.set == MUSIC_SET && !hits[0].page.is_song());
        // An item of neither kind is no page.
        assert_eq!(
            Page::from_music(Article {
                title: "x".into(),
                item: Some("artist/a1b2c3d4-d987-4042-ae91-78d6a3267d69".into()),
                ..Article::default()
            }),
            None
        );
    }

    #[test]
    fn a_title_alone_is_for_a_song_far_better_known_than_its_namesakes() {
        let creep = song(
            "Creep",
            "Song by Radiohead",
            "recording/a1b2c3d4-0001-4042-ae91-78d6a3267d69",
            288_000,
        );
        let tlc = song(
            "Creep",
            "Song by TLC",
            "recording/a1b2c3d4-0002-4042-ae91-78d6a3267d69",
            34_000,
        );
        let oasis = song(
            "Hello",
            "Song by Oasis",
            "recording/a1b2c3d4-0003-4042-ae91-78d6a3267d69",
            93_000,
        );
        let evanescence = song(
            "Hello",
            "Song by Evanescence",
            "recording/a1b2c3d4-0004-4042-ae91-78d6a3267d69",
            83_000,
        );
        let quiet = song(
            "Dead Sea",
            "Song by The Lumineers",
            "recording/a1b2c3d4-0005-4042-ae91-78d6a3267d69",
            40_000,
        );
        let (_dir, s) = searcher(&[
            creep.clone(),
            tlc,
            oasis,
            evanescence,
            quiet,
            page("Creep", 50_000, &[]),
        ]);
        assert_eq!(s.known_song("creep").unwrap(), Some(creep.clone()));
        assert_eq!(s.known_song("Creep").unwrap(), Some(creep));
        // Titles shared about equally, too few listeners, or no title.
        assert_eq!(s.known_song("hello").unwrap(), None);
        assert_eq!(s.known_song("dead sea").unwrap(), None);
        assert_eq!(s.known_song("radiohead").unwrap(), None);
        // The title still lists no song.
        assert!(s
            .search("creep", 5)
            .unwrap()
            .iter()
            .all(|hit| hit.page.set != MUSIC_SET));
    }

    fn film(title: &str, description: &str, item: &str, sitelinks: u64) -> Page {
        Page::from_film(Article {
            title: title.into(),
            description: Some(description.into()),
            item: Some(item.into()),
            views: sitelinks,
            ..Article::default()
        })
        .unwrap()
    }

    #[test]
    fn films_and_shows_are_found_by_their_title_and_year_or_maker() {
        let dune = film(
            "Dune",
            "Film by Denis Villeneuve, 2021 · with Timothée Chalamet, Zendaya",
            "Q63985561/Dune_(2021_film)",
            80,
        );
        assert_eq!(dune.url, "https://en.wikipedia.org/wiki/Dune_(2021_film)");
        assert_eq!(dune.item.as_deref(), Some("Q63985561"));
        assert!(dune.is_film_with_article() && !dune.is_show() && !dune.may_lead());
        assert_eq!(dune.set_name(), "Wikipedia");
        let lynch = film(
            "Dune",
            "Film by David Lynch, 1984 · with Kyle MacLachlan",
            "Q114819/Dune_(1984_film)",
            60,
        );
        // No English article: listed as itself, on Wikidata.
        let foreign = film(
            "Les Dents de la nuit",
            "Film by Stephen Cafiero, 2008",
            "Q3230000",
            4,
        );
        assert_eq!(foreign.url, "https://www.wikidata.org/wiki/Q3230000");
        assert_eq!(foreign.set_name(), "Wikidata");
        assert_eq!(foreign.set_domain(), "wikidata.org");
        let bad = film(
            "Breaking Bad",
            "TV series by Vince Gilligan, 2008–2013 · with Bryan Cranston",
            "Q1079/Breaking_Bad",
            90,
        );
        assert!(bad.is_show());
        let mut article_2021 = page("Dune (2021 film)", 400_000, &[]);
        article_2021.item = Some("Q63985561".into());
        let mut novel = page("Dune (novel)", 900_000, &["Dune"]);
        novel.item = Some("Q190192".into());
        let mut show_article = page("Breaking Bad", 800_000, &[]);
        show_article.item = Some("Q1079".into());
        let (_dir, s) = searcher(&[
            dune.clone(),
            lynch.clone(),
            foreign.clone(),
            bad,
            article_2021,
            novel,
            show_article,
        ]);
        // The year, the director or "movie" asks for the film, found as its
        // article.
        for (query, url) in [
            (
                "dune 2021",
                "https://en.wikipedia.org/wiki/Dune_(2021_film)",
            ),
            (
                "dune villeneuve",
                "https://en.wikipedia.org/wiki/Dune_(2021_film)",
            ),
            (
                "dune 1984",
                "https://en.wikipedia.org/wiki/Dune_(1984_film)",
            ),
            (
                "dune david lynch",
                "https://en.wikipedia.org/wiki/Dune_(1984_film)",
            ),
            (
                "dune 2021 movie",
                "https://en.wikipedia.org/wiki/Dune_(2021_film)",
            ),
            (
                "dune zendaya",
                "https://en.wikipedia.org/wiki/Dune_(2021_film)",
            ),
            (
                "breaking bad tv show",
                "https://en.wikipedia.org/wiki/Breaking_Bad",
            ),
            (
                "les dents de la nuit 2008",
                "https://www.wikidata.org/wiki/Q3230000",
            ),
        ] {
            let hits = s.search(query, 5).unwrap();
            assert!(
                hits[0].whole && hits[0].named && hits[0].page.url == url,
                "{query}: {hits:?}"
            );
        }
        // Found with it, the article is what is listed, once.
        let hits = s.search("dune 2021", 5).unwrap();
        assert!(hits[0].page.is_article(), "{hits:?}");
        assert_eq!(
            hits.iter()
                .filter(|h| h.page.item.as_deref() == Some("Q63985561"))
                .count(),
            1
        );
        // Not found with it, the film is listed with its article's address.
        let hits = s.search("dune 1984", 5).unwrap();
        assert_eq!(hits[0].page.set, FILMS_SET);
        // Its title alone asks for the articles, not the films.
        let hits = s.search("dune", 5).unwrap();
        assert!(hits.iter().all(|h| h.page.set != FILMS_SET), "{hits:?}");
        // A show is not asked for as a movie, nor a film as a show.
        let hits = s.search("breaking bad movie", 5).unwrap();
        assert!(hits.iter().all(|h| !h.whole), "{hits:?}");
        let hits = s.search("dune tv series", 5).unwrap();
        assert!(hits.iter().all(|h| !h.whole), "{hits:?}");
        // A film with no article is listed by its title, below the sites.
        let hits = s.search("les dents de la nuit", 5).unwrap();
        assert!(hits[0].named && !hits[0].whole && hits[0].page.url == foreign.url);
        // An item that does not read is no page.
        for item in ["X1", "Q12/bad path", ""] {
            assert_eq!(
                Page::from_film(Article {
                    title: "x".into(),
                    item: Some(item.into()),
                    ..Article::default()
                }),
                None,
                "{item}"
            );
        }
    }

    #[test]
    fn podcasts_are_found_by_their_name_and_podcast() {
        let podcast = Page::from_podcast(Article {
            title: "Dan Carlin's Hardcore History".into(),
            description: Some("Podcast by Dan Carlin · History".into()),
            item: Some("1".into()),
            site: Some("dancarlin.com".into()),
            views: 90_070,
            aliases: vec!["Dan Carlin podcast".into()],
            ..Article::default()
        });
        assert_eq!(podcast.url, "https://podcastindex.org/podcast/1");
        assert_eq!(podcast.set_name(), "Podcast Index");
        let (_dir, s) = searcher(&[podcast, page("History", 500_000, &[])]);
        for query in [
            "hardcore history podcast",
            "dan carlin's hardcore history podcast",
            "dan carlin's hardcore history dan carlin",
        ] {
            let hits = s.search(query, 5).unwrap();
            assert!(
                hits[0].whole && hits[0].page.set == PODCASTS_SET,
                "{query}: {hits:?}"
            );
        }
        let hits = s.search("dan carlin podcast", 5).unwrap();
        assert!(hits[0].named && hits[0].page.set == PODCASTS_SET);
        // Alone, a title of the end of another is no podcast's.
        assert!(!s
            .search("history podcast", 5)
            .unwrap()
            .iter()
            .any(|hit| hit.whole));
    }

    #[test]
    fn words_that_say_what_is_wanted_keep_the_name() {
        let (_dir, s) = searcher(&[
            page("Lululemon", 50_000, &[]),
            page("Galileo Galilei", 90_000, &["Galileo"]),
            page("Batch normalization", 20_000, &[]),
            page("Ariana Grande", 300_000, &[]),
            Page::from_paper(Article {
                title: "Batch Normalization: Accelerating Deep Network Training by Reducing Internal Covariate Shift".into(),
                description: Some("Paper by Sergey Ioffe et al., 2015".into()),
                item: Some("10.1/1".into()),
                views: 40_000,
                ..Article::default()
            }),
        ]);
        let first = |q: &str| {
            let hits = s.search(q, 5).unwrap();
            hits.first().map(|h| (h.page.title.clone(), h.named))
        };
        assert_eq!(
            first("lululemon wikipedia"),
            Some(("Lululemon".into(), true))
        );
        assert_eq!(
            first("who is galileo"),
            Some(("Galileo Galilei".into(), true))
        );
        assert_eq!(
            first("ariana grande age"),
            Some(("Ariana Grande".into(), true))
        );
        let (title, named) = first("batch normalization paper").unwrap();
        assert!(
            title.starts_with("Batch Normalization: Accelerating"),
            "{title}"
        );
        assert!(named);
        // Without such words nothing changes.
        assert_eq!(
            first("batch normalization").unwrap().0,
            "Batch normalization"
        );
        assert_eq!(hinted_name("wikipedia"), None);
        assert_eq!(hinted_name("who is"), None);
        assert_eq!(
            hinted_name("Who is Dalai Lama?"),
            Some(("dalai lama".into(), Hint::Any))
        );
    }

    #[test]
    fn articles_are_found_by_what_they_say_of_themselves() {
        let described = |title: &str, description: &str, views: u64, aliases: &[&str]| {
            let mut page = page(title, views, aliases);
            page.description = Some(description.into());
            page
        };
        let (_dir, s) = searcher(&[
            described("Queen (band)", "British rock band", 900_000, &[]),
            described("Queen (album)", "1973 studio album by Queen", 40_000, &[]),
            described(
                "William McKinley",
                "President of the United States from 1897 to 1901",
                300_000,
                &[],
            ),
            described("Folic acid", "Synthetic form of vitamin B9", 200_000, &[]),
            described("Coworking", "Shared office arrangement", 50_000, &[]),
            described("Resin", "Solid or highly viscous substance", 60_000, &[]),
            described("Album", "Collection of audio recordings", 500_000, &[]),
            described("Acid", "Chemical compound", 500_000, &[]),
        ]);
        let found = |query: &str| -> Vec<(String, f32)> {
            s.search(query, 5)
                .unwrap()
                .into_iter()
                .map(|hit| (hit.page.title, hit.score))
                .collect()
        };
        let first = |query: &str| found(query).first().map(|(title, _)| title.clone());
        assert_eq!(first("queen album").as_deref(), Some("Queen (album)"));
        assert_eq!(
            first("mckinley president").as_deref(),
            Some("William McKinley")
        );
        // A word the article does not say is left over when it names the
        // article in full.
        assert_eq!(first("source of folic acid").as_deref(), Some("Folic acid"));
        // Words that only ask are not looked for.
        assert_eq!(first("what is coworking").as_deref(), Some("Coworking"));
        assert_eq!(first("what does resin mean").as_deref(), Some("Resin"));
        // Such a page is not named, and is listed only when it covers the
        // query well enough.
        let hits = s.search("mckinley president", 5).unwrap();
        assert!(!hits[0].named);
        // A word neither its names nor its description have keeps the
        // article out when the query names it only in part.
        assert!(found("queen tour")
            .iter()
            .all(|(title, _)| title != "Queen (album)"));
        // One word is a name, not a description.
        assert!(found("album")
            .iter()
            .all(|(title, _)| title != "Queen (album)"));
        assert_eq!(
            hinted_name("what does resin mean"),
            Some(("resin".into(), Hint::Any))
        );
        assert_eq!(
            hinted_name("What is a manubrium?"),
            Some(("manubrium".into(), Hint::Any))
        );
    }

    #[test]
    fn articles_are_found_by_their_other_names() {
        let mut sternum = page("Sternum", 300_000, &["Breastbone"]);
        sternum.names = vec!["Manubrium".into(), "Manubrium sterni".into()];
        let (_dir, s) = searcher(&[
            sternum,
            page("Manubrium (band)", 100, &[]),
            page("Clavicle", 400_000, &[]),
        ]);
        let hits = s.search("manubrium", 5).unwrap();
        // The page the query names comes first; the article it names a
        // part of is listed, but not as named.
        assert_eq!(titles(&hits), ["Manubrium (band)", "Sternum"]);
        assert!(!hits[1].named);
        assert!(hits[1].score >= MIN_PARTIAL_SCORE);
        let (_dir, s) = searcher(&[{
            let mut sternum = page("Sternum", 300_000, &[]);
            sternum.names = vec!["Manubrium".into()];
            sternum
        }]);
        assert_eq!(titles(&s.search("manubrium", 5).unwrap()), ["Sternum"]);
        assert_eq!(
            titles(&s.search("what is the manubrium", 5).unwrap()),
            ["Sternum"]
        );
    }

    #[test]
    fn articles_are_found_by_their_leads() {
        let led = |title: &str, views: u64, lead: &str| {
            let mut page = page(title, views, &[]);
            page.lead = Some(lead.into());
            page
        };
        let (_dir, s) = searcher(&[
            led(
                "Mesozoic",
                200_000,
                "The Mesozoic Era is the era of Earth's geological history, comprising the Triassic, Jurassic and Cretaceous Periods.",
            ),
            led(
                "Dinosaur",
                900_000,
                "Dinosaurs are a diverse group of reptiles that emerged during the Triassic period. They became dominant in the Jurassic, and most died out at the end of the Cretaceous, with birds the only survivors of a long history spanning many periods and kinds of animals on every continent.",
            ),
            led(
                "Jurassic Park",
                800_000,
                "Jurassic Park is a 1993 American science fiction film.",
            ),
            led("Okinawa Prefecture", 300_000, "Okinawa Prefecture is the southernmost prefecture of Japan, with a culture of its own."),
        ]);
        let hits = s.search("triassic jurassic cretaceous", 5).unwrap();
        assert_eq!(hits[0].page.title, "Mesozoic", "{:?}", titles(&hits));
        assert!(!hits[0].named && hits[0].score >= MIN_PARTIAL_SCORE);
        assert!(titles(&hits).contains(&"Dinosaur"));
        assert!(!titles(&hits).contains(&"Jurassic Park"));
        assert_eq!(
            titles(&s.search("okinawa culture", 5).unwrap()).first(),
            Some(&"Okinawa Prefecture")
        );
        // One word is a name, not a description.
        assert!(!titles(&s.search("triassic", 5).unwrap()).contains(&"Mesozoic"));
    }

    #[test]
    fn words_are_only_looked_up() {
        let word = |title: &str, description: &str, views: u64| {
            Page::from_word(Article {
                title: title.into(),
                description: Some(description.into()),
                views,
                ..Article::default()
            })
        };
        let (_dir, s) = searcher(&[
            page("Free (album)", 1_000, &[]),
            word("free", "(adjective) Unconstrained.", 900),
            word("Free", "(noun) A surname.", 5),
            word("AWOL", "(adjective) Absent without leave.", 50),
        ]);
        // Never a result.
        assert_eq!(titles(&s.search("free", 5).unwrap()), ["Free (album)"]);
        let free = s.definition("free").unwrap().unwrap();
        assert_eq!(
            free.description.as_deref(),
            Some("(adjective) Unconstrained.")
        );
        assert_eq!(free.url, "https://en.wiktionary.org/wiki/free");
        assert_eq!(s.definition("awol").unwrap().unwrap().title, "AWOL");
        assert_eq!(s.definition("freedom").unwrap(), None);
    }

    #[test]
    fn papers_are_asked_for_by_their_title_author_year_or_venue() {
        let paper = |title: &str, description: &str, views: u64| {
            Page::from_paper(Article {
                title: title.into(),
                description: Some(description.into()),
                item: Some(format!("10.1/{views}")),
                views,
                ..Article::default()
            })
        };
        let (_dir, s) = searcher(&[
            paper(
                "Random Forests",
                "Paper by Leo Breiman, 2001, Machine Learning",
                134_047,
            ),
            paper(
                "Basic local alignment search tool",
                "Paper by Stephen F. Altschul et al., 1990, Journal of Molecular Biology",
                90_000,
            ),
            paper(
                "Deep learning",
                "Paper by Yann LeCun et al., 2015, Nature",
                85_369,
            ),
            page("Random forest", 300_000, &[]),
            page("Tool", 400_000, &[]),
        ]);
        for query in [
            "random forests breiman",
            "random forests leo breiman 2001",
            "basic local alignment search tool",
            "deep learning lecun nature",
            "deep learning paper",
        ] {
            let hits = s.search(query, 5).unwrap();
            assert!(
                hits[0].whole && hits[0].page.set == PAPERS_SET,
                "{query}: {hits:?}"
            );
        }
        // A title of fewer words alone, or with words that are not its
        // byline's, asks for no paper.
        for query in ["deep learning", "random forests in python"] {
            assert!(
                !s.search(query, 5).unwrap().iter().any(|hit| hit.whole),
                "{query}"
            );
        }
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
