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
//! `plumb-tools web-pages` builds both; nodes only read them.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, INDEXED, STORED,
};
use tantivy::{DocAddress, Index, IndexReader, ReloadPolicy, Searcher, TantivyDocument, Term};

use crate::analysis::{self, STEMMED_ANALYZER};

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
    WebFields {
        schema: builder.build(),
        row,
        url,
        title,
        body,
        lead,
    }
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
        Ok(WebWords {
            index,
            reader,
            fields,
        })
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

    #[test]
    fn pages_are_found_by_words_and_row() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let words = dir.path().join(WEB_WORDS_DIR);
        std::fs::create_dir(&words)?;
        let (index, fields) = create_web_words(&words)?;
        let mut writer = index.writer::<TantivyDocument>(15_000_000)?;
        let pages = [
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
        ];
        for (row, (url, title, text)) in pages.iter().enumerate() {
            let mut document = TantivyDocument::new();
            document.add_u64(fields.row, row as u64);
            document.add_text(fields.url, url);
            document.add_text(fields.title, title);
            document.add_text(fields.body, text);
            document.add_text(fields.lead, text);
            writer.add_document(document)?;
        }
        writer.commit()?;
        let web = WebWords::open(dir.path())?;
        assert_eq!(web.len(), 3);
        assert_eq!(web.by_words("open csv files", 10)?, vec![0, 1]);
        assert!(web.by_words("the of", 10)?.is_empty());
        let page = web.page(2)?.expect("row 2 is held");
        assert_eq!(
            (page.url.as_str(), page.title.as_str()),
            ("https://example.com/cats", "Cats")
        );
        assert_eq!(web.page(3)?, None);
        Ok(())
    }
}
