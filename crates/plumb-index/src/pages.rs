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
//! - `popularity` is `ln(1 + views) / ln(1 + most views)`, the views of the
//!   most read page in the index.
//!
//! How pages and sites are listed together is up to the caller; see
//! [`PageHit::named`].

use std::collections::HashSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::article::{article_url, Article};
use plumb_core::normalize_text;
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, STORED,
};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, Term};

use crate::analysis::{self, JOINED_ANALYZER, WORDS_ANALYZER};
use crate::replace::Staging;

/// `name` of a page one of whose aliases the query is.
pub const ALIAS_MATCH: f32 = 0.9;
/// Most `name` of a page whose title has some of the query's words.
pub const PARTIAL_MATCH: f32 = 0.6;
/// How much popularity counts, against how well the query names the page.
pub const POPULARITY_SHARE: f32 = 0.5;
/// Pages whose words match that are looked at, most matching first.
const CANDIDATES: usize = 200;

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
        }
    }

    /// The name of the set people see: "Wikipedia".
    pub fn set_name(&self) -> &str {
        if self.set.starts_with("wikipedia-") {
            "Wikipedia"
        } else {
            &self.set
        }
    }
}

/// A page found for a query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageHit {
    pub page: Page,
    pub score: f32,
    /// The whole query is the page's title or one of its aliases.
    pub named: bool,
}

struct Fields {
    words: Field,
    keys: Field,
    views: Field,
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
    let views = builder.add_u64_field("views", FAST | STORED);
    let page = builder.add_text_field("page", STORED);
    (
        builder.build(),
        Fields {
            words,
            keys,
            views,
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
    for page in pages {
        let mut document = TantivyDocument::default();
        document.add_text(fields.words, &page.title);
        for alias in &page.aliases {
            document.add_text(fields.words, alias);
        }
        document.add_text(fields.keys, &page.title);
        let base = base_title(&page.title);
        if base != page.title {
            document.add_text(fields.keys, base);
        }
        document.add_u64(fields.views, page.views);
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
        let query_words: HashSet<&str> = words.iter().map(String::as_str).collect();
        let top = searcher.search(
            &BooleanQuery::new(clauses),
            &TopDocs::with_limit(CANDIDATES)
                .order_by_fast_field::<u64>("views", tantivy::Order::Desc),
        )?;
        let most = (self.stats.most_views.max(1) as f32).ln_1p();
        let mut hits = Vec::new();
        for (_, address) in top {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            let (name, named) = self.name_match(&page, &joined, &query_words);
            if name <= 0.0 {
                continue;
            }
            let popularity = (page.views as f32).ln_1p() / most;
            let score = name * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity);
            hits.push(PageHit { page, score, named });
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit);
        Ok(hits)
    }

    fn name_match(&self, page: &Page, joined: &str, query: &HashSet<&str>) -> (f32, bool) {
        let key = |text: &str| {
            analysis::tokens(&self.joined, text)
                .pop()
                .unwrap_or_default()
        };
        if key(&page.title) == joined || key(base_title(&page.title)) == joined {
            return (1.0, true);
        }
        if page.aliases.iter().any(|alias| key(alias) == joined) {
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
///   [`MIN_PARTIAL_SCORE`]. When the best site is named by the query too,
///   site names win: only one page is listed, after that site. Otherwise
///   a named page comes first ("marie curie"), and pages named only in
///   part come after [`PARTIAL_AFTER`] sites.
pub fn place_pages(sites: &[crate::Hit], pages: Vec<PageHit>) -> Vec<PlacedPage> {
    let site_named = sites.first().is_some_and(|hit| hit.named);
    let most = if site_named { 1 } else { MAX_PAGES_LISTED };
    let mut placed: Vec<PlacedPage> = Vec::new();
    let mut listed = 0;
    for hit in pages {
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
        let at = if !hit.named {
            PARTIAL_AFTER
        } else if site_named {
            1
        } else {
            0
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
    fn partial_names_score_lower() {
        let (_dir, s) = searcher(&[page("Marie Curie", 80_000, &[])]);
        let hit = &s.search("curie", 1).unwrap()[0];
        assert!(!hit.named);
        assert!(hit.score < 0.5, "{}", hit.score);
        assert!(s.search("marie antoinette", 1).unwrap().is_empty());
        assert!(s.search("", 1).unwrap().is_empty());
    }

    fn site(domain: &str, named: bool) -> crate::Hit {
        crate::Hit {
            domain: domain.into(),
            url: format!("https://{domain}/"),
            title: None,
            description: None,
            score: 1.0,
            text_score: 1.0,
            link_score: 1.0,
            country: None,
            named,
        }
    }

    fn found(title: &str, site: Option<&str>, named: bool, score: f32) -> PageHit {
        let mut page = page(title, 1, &[]);
        page.site = site.map(str::to_string);
        PageHit { page, score, named }
    }

    #[test]
    fn pages_about_a_listed_site_go_under_it() {
        let sites = [site("python.org", true), site("pythonanywhere.com", false)];
        let placed = place_pages(
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
        // The site is named, so one page, after it.
        assert_eq!(placed[1].hit.page.title, "Python (genus)");
        assert_eq!((placed[1].under.as_deref(), placed[1].at), (None, 1));
    }

    #[test]
    fn named_pages_lead_when_no_site_is_named() {
        let sites = [
            site("curie.fr", false),
            site("a.com", false),
            site("b.com", false),
        ];
        let placed = place_pages(
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
        assert!(place_pages(&[], vec![found("Marie Curie", None, true, 0.9)])[0].at == 0);
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
