//! The web index: pages of popular sites from an open crawl, for searches
//! that want one page inside a site ("open csv files", a page of
//! lifewire.com), which nothing a node knows of the site finds. A page is
//! found by its words here, in a Tantivy index of its title and first
//! words (BM25, the title counted [`TITLE_BOOST`] times), and by its
//! meaning in plumb-embed's `RowVectors`. A search takes the first
//! [`WEB_FIRST`] of each and puts them in one order ([`fuse`]): in the
//! page-index trial that listed more searches' pages in the first 10 than
//! either alone, at every size from 16,000 to a million pages.
//!
//! A page's row is its number in the index and the row of its vector.
//! `plumb-tools web-pages` builds both; nodes only read them. A node lists
//! the pages after the page sets' ([`list_web_pages`]), the node and the
//! evals alike ([`crate::pages::Listing`]).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use plumb_core::{adult_level, registrable_domain};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, INDEXED, STORED,
    STRING,
};
use tantivy::{DocAddress, Index, IndexReader, ReloadPolicy, Searcher, TantivyDocument, Term};

use crate::analysis::{self, STEMMED_ANALYZER};
use crate::pages::{options_allow, Page, PageHit, PlacedPage, WEB_SET};
use crate::{RankConfig, SearchOptions, SearchResults};

/// The words index's folder in a web index's folder.
pub const WEB_WORDS_DIR: &str = "words";
/// The vectors and their signs (plumb-embed's `RowVectors`).
pub const WEB_VECTORS_FILE: &str = "vectors.bin";
pub const WEB_SIGNS_FILE: &str = "signs.bin";

/// How many times a word of the title counts for one of the text, as in
/// the trial.
pub const TITLE_BOOST: f32 = 3.0;
/// Pages each of words and meaning give the fused order.
pub const WEB_FIRST: usize = 100;
/// Pages the signs of their vectors pick to be read again in full. Over
/// the trial's 1.06 million pages, 1,000 kept 356 of the 360 searches
/// reading every vector lists in the first 10, and all 296 in the first 3.
pub const WEB_READ_AGAIN: usize = 1_000;
/// Pages of the fused order looked through for those to list, at least.
const WEB_LOOKED: usize = 10;
/// Reciprocal rank fusion's constant: a page's share is `1 / (FUSE +
/// rank)` in each order it is in.
const FUSE: f32 = 60.0;

/// The words index's fields.
pub struct WebFields {
    pub schema: Schema,
    /// The page's row, indexed to look a page up by it.
    pub row: Field,
    pub url: Field,
    pub title: Field,
    /// The page's first words, searched and not kept.
    pub body: Field,
    /// The first words shown and embedded with the title.
    pub lead: Field,
    /// The page's [`page_key`], to tell whether the index holds a page.
    pub key: Field,
}

pub fn web_fields() -> WebFields {
    let mut builder = Schema::builder();
    let searched = || {
        TextFieldIndexing::default()
            .set_tokenizer(STEMMED_ANALYZER)
            .set_index_option(IndexRecordOption::WithFreqs)
    };
    let row = builder.add_u64_field("row", INDEXED | FAST);
    let url = builder.add_text_field("url", STORED);
    let title = builder.add_text_field(
        "title",
        TextOptions::default()
            .set_indexing_options(searched())
            .set_stored(),
    );
    let body = builder.add_text_field(
        "body",
        TextOptions::default().set_indexing_options(searched()),
    );
    let lead = builder.add_text_field("lead", STORED);
    let key = builder.add_text_field("key", STRING);
    WebFields {
        schema: builder.build(),
        row,
        url,
        title,
        body,
        lead,
        key,
    }
}

/// A page's address without its scheme, `www.`, trailing slash and
/// fragment, its host in lowercase, which a click and a crawl write either
/// way; `None` for what is no web address.
pub fn page_key(address: &str) -> Option<String> {
    let rest = address
        .strip_prefix("https://")
        .or_else(|| address.strip_prefix("http://"))?;
    let rest = rest.split('#').next().unwrap_or(rest);
    let (rest, query) = rest
        .split_once('?')
        .map_or((rest, None), |(rest, query)| (rest, Some(query)));
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let path = path.trim_end_matches('/');
    Some(match query {
        Some(query) => format!("{host}{path}?{query}"),
        None => format!("{host}{path}"),
    })
}

/// An empty words index in `dir`, read with the analyzers searches use.
pub fn create_web_words(dir: &Path) -> Result<(Index, WebFields)> {
    let fields = web_fields();
    let index = Index::create_in_dir(dir, fields.schema.clone())
        .with_context(|| format!("creating a web words index in {}", dir.display()))?;
    analysis::register(index.tokenizers());
    Ok((index, fields))
}

/// A page of the web index.
#[derive(Debug, Clone, PartialEq)]
pub struct WebPage {
    pub row: u32,
    pub url: String,
    pub title: String,
    pub lead: String,
}

/// The web index's words, and its pages by row.
pub struct WebWords {
    index: Index,
    reader: IndexReader,
    fields: WebFields,
    /// Whether it keeps its pages' [`page_key`]s, which an index built
    /// before they were kept lacks.
    keyed: bool,
}

impl WebWords {
    /// Opens the words index of the web index in `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        let words = dir.join(WEB_WORDS_DIR);
        let index = Index::open_in_dir(&words)
            .with_context(|| format!("opening the web words index in {}", words.display()))?;
        analysis::register(index.tokenizers());
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let fields = web_fields();
        let keyed = index.schema().get_field("key").is_ok();
        Ok(WebWords {
            index,
            reader,
            fields,
            keyed,
        })
    }

    /// Whether it holds the page at `address`, give or take what
    /// [`page_key`] leaves out.
    pub fn holds(&self, address: &str) -> Result<bool> {
        let Some(key) = page_key(address).filter(|_| self.keyed) else {
            return Ok(false);
        };
        let query = TermQuery::new(
            Term::from_field_text(self.fields.key, &key),
            IndexRecordOption::Basic,
        );
        Ok(self.reader.searcher().search(&query, &Count)? > 0)
    }

    /// How many pages it holds.
    pub fn len(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The rows of the `n` pages that best match `query`'s words, best
    /// first: any of its words, in the title or the text.
    pub fn by_words(&self, query: &str, n: usize) -> Result<Vec<u32>> {
        let mut analyzer = self.index.tokenizer_for_field(self.fields.title)?;
        let mut stream = analyzer.token_stream(query);
        let mut seen = HashSet::new();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        while let Some(token) = stream.next() {
            if !seen.insert(token.text.clone()) {
                continue;
            }
            let term = |field| {
                TermQuery::new(
                    Term::from_field_text(field, &token.text),
                    IndexRecordOption::WithFreqs,
                )
            };
            let title = BoostQuery::new(Box::new(term(self.fields.title)), TITLE_BOOST);
            clauses.push((Occur::Should, Box::new(title)));
            clauses.push((Occur::Should, Box::new(term(self.fields.body))));
        }
        if clauses.is_empty() || n == 0 {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        let found = searcher.search(
            &BooleanQuery::new(clauses),
            &TopDocs::with_limit(n).order_by_score(),
        )?;
        let mut columns = HashMap::new();
        Ok(found
            .into_iter()
            .filter_map(|(_, address)| row_at(&searcher, &mut columns, address))
            .collect())
    }

    /// The page at `row`.
    pub fn page(&self, row: u32) -> Result<Option<WebPage>> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_u64(self.fields.row, u64::from(row)),
            IndexRecordOption::Basic,
        );
        let Some((_, address)) = searcher
            .search(&query, &TopDocs::with_limit(1).order_by_score())?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let document: TantivyDocument = searcher.doc(address)?;
        let text = |field| {
            document
                .get_first(field)
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string()
        };
        Ok(Some(WebPage {
            row,
            url: text(self.fields.url),
            title: text(self.fields.title),
            lead: text(self.fields.lead),
        }))
    }
}

/// The row of the page at `address`, its segment's column kept in
/// `columns`.
fn row_at(
    searcher: &Searcher,
    columns: &mut HashMap<u32, Option<tantivy::columnar::Column<u64>>>,
    address: DocAddress,
) -> Option<u32> {
    let column = columns.entry(address.segment_ord).or_insert_with(|| {
        searcher
            .segment_reader(address.segment_ord)
            .fast_fields()
            .u64("row")
            .ok()
    });
    u32::try_from(column.as_ref()?.first(address.doc_id)?).ok()
}

/// The rows of `words` and `meaning`, each best first, in one order by
/// reciprocal rank fusion: no weight to set, and a page both find goes
/// before one only either finds at the same place. Ties go to the better
/// by words, then to the lower row.
pub fn fuse(words: &[u32], meaning: &[u32]) -> Vec<u32> {
    let mut shares: HashMap<u32, (f32, usize)> = HashMap::new();
    for (rank, row) in words.iter().enumerate() {
        let share = shares.entry(*row).or_insert((0.0, usize::MAX));
        share.0 += 1.0 / (FUSE + rank as f32 + 1.0);
        share.1 = share.1.min(rank);
    }
    for (rank, row) in meaning.iter().enumerate() {
        shares.entry(*row).or_insert((0.0, usize::MAX)).0 += 1.0 / (FUSE + rank as f32 + 1.0);
    }
    let mut fused: Vec<(u32, (f32, usize))> = shares.into_iter().collect();
    fused.sort_by(|(a, (x, i)), (b, (y, j))| y.total_cmp(x).then(i.cmp(j)).then(a.cmp(b)));
    fused.into_iter().map(|(row, _)| row).collect()
}

/// Lists the web index's pages for `query` among `results`' sites, after
/// the page sets' pages are placed, fusing the pages its words find with
/// those nearest it in meaning ([`SearchResults::web_meant`]).
///
/// - A listed site's own page goes under its result, when it is among the
///   first [`RankConfig::web_under`] of the fused order and the site
///   carries no inner page yet: the row then holds both, and no site moves.
/// - Of the other pages, the first [`RankConfig::web_alone`] are listed on
///   their own after [`RankConfig::web_after`] sites, one a site, none of a
///   listed site (it belongs under that site or nowhere).
///
/// A homepage is left to the sites' index, and so is a page already
/// listed. Pages are kept as the search's options and safe search allow,
/// and as `keep` says (a node's adult blocklist).
pub(crate) fn list_web_pages(
    web: &WebWords,
    query: &str,
    rank: &RankConfig,
    options: &SearchOptions,
    keep: &dyn Fn(&PageHit) -> bool,
    results: &mut SearchResults,
) -> Result<()> {
    if rank.web_under == 0 && rank.web_alone == 0 {
        return Ok(());
    }
    let (sites, pages, meant) = (&results.hits, &mut results.pages, &results.web_meant);
    let words = web.by_words(query, WEB_FIRST)?;
    let listed: HashSet<String> = sites
        .iter()
        .map(|site| site.url.clone())
        .chain(pages.iter().map(|placed| placed.hit.page.url.clone()))
        .collect();
    let mut found = Vec::new();
    for (at, row) in fuse(&words, meant)
        .into_iter()
        .take(rank.web_under.max(WEB_LOOKED))
        .enumerate()
    {
        let Some(page) = web.page(row)? else {
            continue;
        };
        let Some(domain) = registrable_domain(&page.url) else {
            continue;
        };
        let hit = as_hit(page);
        let texts = [hit.page.title.as_str()]
            .into_iter()
            .chain(hit.page.description.as_deref());
        if !is_homepage(&hit.page.url)
            && !listed.contains(&hit.page.url)
            && options_allow(options, &hit.page)
            && !options.safe.hides(adult_level(&domain, texts))
            && keep(&hit)
        {
            let both = words.contains(&row) && meant.contains(&row);
            found.push((at, domain, both, hit));
        }
    }
    let mut sites_taken = HashSet::new();
    for (i, site) in sites.iter().enumerate() {
        let Some(domain) = registrable_domain(&site.domain) else {
            continue;
        };
        let carries_inner = pages.iter().any(|placed| {
            placed.under.as_deref() == Some(site.domain.as_str())
                && (placed.hit.page.is_site_page() || placed.hit.page.set == WEB_SET)
        });
        let own = found
            .iter()
            .position(|(at, of, _, _)| *of == domain && *at < rank.web_under);
        if let (false, Some(own)) = (carries_inner || sites_taken.contains(&domain), own) {
            let (_, _, _, hit) = found.remove(own);
            pages.push(PlacedPage {
                hit,
                under: Some(site.domain.clone()),
                at: i,
            });
        }
        sites_taken.insert(domain);
    }
    let after = rank.web_after.min(sites.len());
    let alone: Vec<PageHit> = found
        .into_iter()
        .filter(|(_, domain, both, _)| {
            (*both || !rank.web_agree) && sites_taken.insert(domain.clone())
        })
        .take(rank.web_alone)
        .map(|(_, _, _, hit)| hit)
        .collect();
    pages.extend(alone.into_iter().map(|hit| PlacedPage {
        hit,
        under: None,
        at: after,
    }));
    Ok(())
}

/// `page` as a page found, placed by where the fused order puts it rather
/// than by a score.
fn as_hit(page: WebPage) -> PageHit {
    PageHit {
        page: Page {
            set: WEB_SET.to_string(),
            url: page.url,
            title: page.title,
            description: Some(page.lead).filter(|lead| !lead.is_empty()),
            // The pages kept are in English, which the model reads.
            content_language: Some("en".to_string()),
            ..Page::default()
        },
        score: 0.0,
        named: false,
        popularity: 0.0,
        whole: false,
        learned: None,
        meant: false,
        unsure: false,
    }
}

/// Whether `url` is a site's homepage: no path past the host.
fn is_homepage(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.find('/').is_none_or(|slash| &rest[slash..] == "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fused_pages_both_find_come_first() {
        // 7 is second by both; 1 first by words only, 9 first by meaning only.
        assert_eq!(fuse(&[1, 7, 3], &[9, 7, 4]), vec![7, 1, 9, 3, 4]);
        assert_eq!(fuse(&[], &[5, 6]), vec![5, 6]);
        assert!(fuse(&[], &[]).is_empty());
    }

    /// A words index in `dir` of `pages` (address, title, text), each
    /// page's row its place.
    fn words_of(dir: &Path, pages: &[(&str, &str, &str)]) -> Result<WebWords> {
        let words = dir.join(WEB_WORDS_DIR);
        std::fs::create_dir(&words)?;
        let (index, fields) = create_web_words(&words)?;
        let mut writer = index.writer::<TantivyDocument>(15_000_000)?;
        for (row, (url, title, text)) in pages.iter().enumerate() {
            let mut document = TantivyDocument::new();
            document.add_u64(fields.row, row as u64);
            document.add_text(fields.url, url);
            document.add_text(fields.title, title);
            document.add_text(fields.body, text);
            document.add_text(fields.lead, text);
            document.add_text(fields.key, page_key(url).unwrap_or_default());
            writer.add_document(document)?;
        }
        writer.commit()?;
        WebWords::open(dir)
    }

    const PAGES: [(&str, &str, &str); 4] = [
        (
            "https://www.lifewire.com/csv-file-2622708",
            "How to Open CSV Files",
            "A CSV file is a comma separated values file.",
        ),
        (
            "https://example.com/files",
            "Files",
            "Opening files on a computer, of every kind.",
        ),
        (
            "https://example.com/cats",
            "Cats",
            "Cats are small animals.",
        ),
        (
            "https://www.lifewire.com/",
            "Lifewire: open CSV files and more",
            "Tech news and how-tos.",
        ),
    ];

    #[test]
    fn pages_are_found_by_words_and_row() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let web = words_of(dir.path(), &PAGES[..3])?;
        assert_eq!(web.len(), 3);
        assert_eq!(web.by_words("open csv files", 10)?, vec![0, 1]);
        assert!(web.by_words("the of", 10)?.is_empty());
        let page = web.page(2)?.expect("row 2 is held");
        assert_eq!(
            (page.url.as_str(), page.title.as_str()),
            ("https://example.com/cats", "Cats")
        );
        assert_eq!(web.page(3)?, None);
        assert!(web.holds("http://lifewire.com/csv-file-2622708/")?);
        assert!(!web.holds("https://www.lifewire.com/csv-file")?);
        Ok(())
    }

    #[test]
    fn a_page_key_leaves_out_http_www_and_a_slash() {
        let key = page_key("http://www.Lifewire.com/csv-file/#open");
        assert_eq!(key.as_deref(), Some("lifewire.com/csv-file"));
        assert_eq!(page_key("https://lifewire.com/csv-file"), key);
        assert_eq!(
            page_key("https://a.com/find/?q=1").as_deref(),
            Some("a.com/find?q=1")
        );
        assert_eq!(page_key("https://a.com/").as_deref(), Some("a.com"));
        assert_eq!(page_key("a.com/x"), None);
    }

    /// Where [`list_web_pages`] lists the pages found for `query` among
    /// sites `domains`, with `meant` the rows nearest in meaning: each
    /// page's address, with the site it is under or the site it comes before.
    fn listed(
        web: &WebWords,
        query: &str,
        meant: &[u32],
        rank: &RankConfig,
        domains: &[&str],
    ) -> Result<Vec<(String, Option<String>, usize)>> {
        let mut results = SearchResults {
            hits: domains
                .iter()
                .map(|domain| crate::health::tests::hit(domain, false, 0.5))
                .collect(),
            web_meant: meant.to_vec(),
            ..SearchResults::default()
        };
        let options = SearchOptions::default();
        list_web_pages(web, query, rank, &options, &|_| true, &mut results)?;
        Ok(results
            .pages
            .into_iter()
            .map(|placed| (placed.hit.page.url, placed.under, placed.at))
            .collect())
    }

    #[test]
    fn a_listed_sites_page_goes_under_it_and_the_best_other_after_the_third_site() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let web = words_of(dir.path(), &PAGES)?;
        let sites = ["a.com", "example.com", "b.com", "c.com", "d.com"];
        let rank = RankConfig::default();
        // lifewire.com's homepage is the sites' to list, and of
        // example.com's pages only the first found goes under it.
        assert_eq!(
            listed(&web, "open csv files", &[], &rank, &sites)?,
            vec![
                (
                    "https://example.com/files".into(),
                    Some("example.com".into()),
                    1
                ),
                ("https://www.lifewire.com/csv-file-2622708".into(), None, 3),
            ]
        );
        // The page is found by meaning too: "cats" finds none by words.
        let found = listed(&web, "kittens", &[2], &rank, &sites)?;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.as_deref(), Some("example.com"));
        // With few sites, the page comes after them all.
        let alone = listed(&web, "open csv files", &[], &rank, &["a.com"])?;
        assert_eq!(
            alone[0],
            ("https://www.lifewire.com/csv-file-2622708".into(), None, 1)
        );
        // Only those both words and meaning find, when so asked.
        let agree = RankConfig {
            web_agree: true,
            ..rank
        };
        assert_eq!(
            listed(&web, "open csv files", &[], &agree, &["a.com"])?,
            vec![]
        );
        assert_eq!(
            listed(&web, "open csv files", &[0], &agree, &["a.com"])?.len(),
            1
        );
        let off = RankConfig {
            web_under: 0,
            web_alone: 0,
            ..rank
        };
        assert!(listed(&web, "open csv files", &[], &off, &sites)?.is_empty());
        Ok(())
    }
}
