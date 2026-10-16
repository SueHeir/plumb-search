//! The page index's documents written out as text, for the golden files
//! that check what a build makes ([`PageSearcher::dump`]).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::Result;
use tantivy::postings::Postings;
use tantivy::schema::{FieldType, IndexRecordOption, Value};
use tantivy::{DocSet, TantivyDocument, TERMINATED};

use super::PageSearcher;

impl PageSearcher {
    /// Every document of the index as text, the documents in the order of
    /// their sets and addresses: the page as a search reads it back, its
    /// popularity, and each term of each indexed field, with how often it
    /// comes where the field counts that. Golden files check a build by it.
    pub fn dump(&self) -> Result<String> {
        let searcher = self.reader.searcher();
        let schema = searcher.schema().clone();
        let mut documents: BTreeMap<(String, String, usize), Vec<String>> = BTreeMap::new();
        for (ord, segment) in searcher.segment_readers().iter().enumerate() {
            let mut lines: BTreeMap<u32, Vec<String>> = BTreeMap::new();
            for (field, entry) in schema.fields() {
                if !entry.is_indexed() {
                    continue;
                }
                let freqs = entry
                    .field_type()
                    .get_index_record_option()
                    .is_some_and(|option| option.has_freq());
                let numeric = matches!(entry.field_type(), FieldType::U64(_));
                let inverted = segment.inverted_index(field)?;
                let mut terms = inverted.terms().stream()?;
                while terms.advance() {
                    let key = terms.key();
                    let term = match <[u8; 8]>::try_from(key) {
                        Ok(bytes) if numeric => u64::from_be_bytes(bytes).to_string(),
                        _ => serde_json::to_string(&String::from_utf8_lossy(key))?,
                    };
                    let mut postings = inverted
                        .read_postings_from_terminfo(terms.value(), IndexRecordOption::WithFreqs)?;
                    while postings.doc() != TERMINATED {
                        let mut line = format!("{} {term}", entry.name());
                        if freqs {
                            write!(line, " x{}", postings.term_freq())?;
                        }
                        lines.entry(postings.doc()).or_default().push(line);
                        postings.advance();
                    }
                }
            }
            for doc in 0..segment.max_doc() {
                let document: TantivyDocument =
                    searcher.doc(tantivy::DocAddress::new(ord as u32, doc))?;
                let page = self.read_page(&document)?;
                let popularity = document
                    .get_first(self.fields.popularity)
                    .and_then(|v| v.as_u64());
                let mut text = vec![
                    format!("page {}", serde_json::to_string(&page)?),
                    format!("popularity {popularity:?}"),
                ];
                text.extend(lines.remove(&doc).unwrap_or_default());
                let (set, url) = page.map(|page| (page.set, page.url)).unwrap_or_default();
                documents.insert((set, url, documents.len()), text);
            }
        }
        let mut out = String::new();
        for ((set, url, _), lines) in documents {
            writeln!(out, "doc {set} {url}")?;
            for line in lines {
                writeln!(out, "  {line}")?;
            }
        }
        Ok(out)
    }
}
