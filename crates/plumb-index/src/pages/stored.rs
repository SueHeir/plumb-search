//! What the page index keeps of each page in its stored field, and how a
//! search reads the page back from it.

use anyhow::Result;
use tantivy::schema::Value;
use tantivy::TantivyDocument;

use super::{Page, PageSearcher};

impl PageSearcher {
    /// The page `document` holds, as every search reads it.
    pub(crate) fn read_page(&self, document: &TantivyDocument) -> Result<Option<Page>> {
        let stored = document
            .get_first(self.fields.page)
            .and_then(|v| v.as_str());
        Ok(stored.map(serde_json::from_str).transpose()?)
    }
}
