//! What the page index keeps of each page in its stored field, and how a
//! search reads the page back from it.
//!
//! The index keeps each page as the item of the data format it is
//! ([`SetItem`]), with the page set it came from: the item as the
//! project's item set of the page set holds it, or, for a page of an
//! articles file, the item [`page_item`](crate::page_items::page_item)
//! makes of it, with what `Page` works out from its set (a site a person
//! borrows left out, the profiles, facts and lead of its line of
//! profiles) in its fields. A search reads the page back from the item
//! ([`page_of_item`]).
//!
//! Indexes built before kept each page's own JSON, and are read too: a
//! node searches the index it has while it builds the next, and builds
//! the index of the pages it wanted ([`super::WANTED_SET`]) again only
//! when they change.

use anyhow::{Context, Result};
use plumb_format::Item;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tantivy::schema::Value as _;
use tantivy::TantivyDocument;

use super::{Page, PageSearcher};
use crate::items::page_of_item;
use crate::page_items::index_item;

/// How the JSON of a [`SetItem`] starts. A page's own JSON never starts
/// so: its first member is `paper` or `set`.
const ITEM_START: &str = "{\"item\":";

/// A page as the page index keeps it: the item it is, and the page set it
/// came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetItem {
    pub item: Item,
    /// The page set, `wikipedia-en`.
    pub set: String,
    /// Members of the page, as its JSON has them, that its item gives
    /// otherwise ([`SetItem::of_page`]), by name; a member the page does
    /// not have is null. Hardly any page has them.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub members: Map<String, Value>,
}

impl SetItem {
    /// `item` of the project's item set of the page set `set`, as the set
    /// holds it.
    pub fn new(set: &str, item: Item) -> Self {
        SetItem {
            item,
            set: set.to_string(),
            members: Map::new(),
        }
    }

    /// `page`, of a page set's articles file, as its item: the one
    /// [`page_item`](crate::page_items::page_item) makes, but with every
    /// field however big, and a URL that has no canonical form as it is.
    /// Where an item can't say a member of the page as the page has it
    /// (text the format would change), [`SetItem::members`] keeps it, so
    /// the page read back is `page`.
    pub fn of_page(page: &Page) -> Self {
        let mut kept = SetItem {
            item: index_item(page),
            set: page.set.clone(),
            members: Map::new(),
        };
        let back = kept.page();
        if back.as_ref() != Some(page) {
            kept.members = members_otherwise(page, back.as_ref());
        }
        kept
    }

    /// The page it is, `None` for an item without a URL.
    pub fn page(&self) -> Option<Page> {
        let page = page_of_item(&self.item, &self.set)?;
        if self.members.is_empty() {
            return Some(page);
        }
        let Ok(Value::Object(mut json)) = serde_json::to_value(&page) else {
            return Some(page);
        };
        for (name, value) in &self.members {
            match value {
                Value::Null => json.remove(name),
                value => json.insert(name.clone(), value.clone()),
            };
        }
        Some(serde_json::from_value(Value::Object(json)).unwrap_or(page))
    }

    /// The text the page index's stored field holds of it.
    pub(crate) fn stored(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }
}

/// The members of `page`, as its JSON has them, that `back` has
/// otherwise; null for one `page` does not have.
fn members_otherwise(page: &Page, back: Option<&Page>) -> Map<String, Value> {
    let json = |page: Option<&Page>| match page.map(serde_json::to_value) {
        Some(Ok(Value::Object(json))) => json,
        _ => Map::new(),
    };
    let (page, back) = (json(Some(page)), json(back));
    let mut members = Map::new();
    for name in page.keys().chain(back.keys()) {
        let value = page.get(name);
        if value != back.get(name) {
            members.insert(name.clone(), value.cloned().unwrap_or(Value::Null));
        }
    }
    members
}

/// The page a stored field holds: a [`SetItem`]'s, or, in an index built
/// before, the page's own JSON.
fn page_of_stored(stored: &str) -> Result<Page> {
    if !stored.starts_with(ITEM_START) {
        return Ok(serde_json::from_str(stored)?);
    }
    let kept: SetItem = serde_json::from_str(stored)?;
    kept.page()
        .with_context(|| format!("a page of {} without a URL", kept.set))
}

impl PageSearcher {
    /// The page `document` holds, as every search reads it.
    pub(crate) fn read_page(&self, document: &TantivyDocument) -> Result<Option<Page>> {
        document
            .get_first(self.fields.page)
            .and_then(|v| v.as_str())
            .map(page_of_stored)
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(value: Value) -> Page {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_page_comes_back_from_its_stored_item() {
        let page = page(serde_json::json!({
            "set": "wikipedia-en",
            "url": "https://en.wikipedia.org/wiki/Tim_Cook",
            "title": "Tim Cook",
            "description": "American business executive",
            "views": 90000,
            "item": "Q265852",
            "site": "apple.com",
            "profiles": [{"service": "x", "id": "tim_cook"}],
            "facts": [{"kind": "born", "value": "1960-11-01"}],
            "lead": "Timothy Donald Cook is an American business executive.",
        }));
        let kept = SetItem::of_page(&page);
        assert!(kept.members.is_empty());
        let stored = kept.stored().unwrap();
        assert!(stored.starts_with(ITEM_START), "{stored}");
        assert_eq!(page_of_stored(&stored).unwrap(), page);
    }

    #[test]
    fn what_an_item_cannot_say_is_kept_beside_it() {
        // A fact and a profile not in NFC, which an item's fields carry
        // in NFC; a title not in NFC, which its `page` field gives as it
        // is; a URL with no canonical form; passages too big for an item.
        let page = page(serde_json::json!({
            "set": "docs",
            "url": "ftp://example.org/Cafe\u{301}",
            "title": "Cafe\u{301}",
            "views": 3,
            "profiles": [{"service": "x", "id": "cafe\u{301}"}],
            "facts": [{"kind": "born", "value": "Cafe\u{301}"}],
            "search": {
                "version": plumb_core::article::SEARCH_VERSION,
                "symbols": [],
                "passages": [{"heading": "", "text": "x".repeat(70_000)}],
            },
        }));
        let kept = SetItem::of_page(&page);
        assert_eq!(
            kept.item.url.as_deref(),
            Some("ftp://example.org/Cafe\u{301}")
        );
        assert_eq!(
            kept.members.keys().collect::<Vec<_>>(),
            ["facts", "profiles"]
        );
        assert_eq!(page_of_stored(&kept.stored().unwrap()).unwrap(), page);
    }

    #[test]
    fn a_page_an_older_index_stored_is_read_as_it_was() {
        let page = page(serde_json::json!({
            "set": "wikipedia-en",
            "url": "https://en.wikipedia.org/wiki/Mars",
            "title": "Mars",
            "views": 5,
            "item": "Q111",
        }));
        let stored = serde_json::to_string(&page).unwrap();
        assert_eq!(page_of_stored(&stored).unwrap(), page);
    }
}
