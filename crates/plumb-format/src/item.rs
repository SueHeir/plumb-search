//! The one record every set is made of.
//!
//! An item is either a **thing**, which stands on its own with a kind and
//! usually a title and URL (an article, a crawled homepage, a package), or
//! an **extension**, which names another item with `extends` and adds to
//! it: names, labels, fields, edges, vectors. A set of extensions is a
//! layer. Both are the same [`Item`].
//!
//! A key is fixed here only when the core itself reads it, or a safety
//! rule does, so a publisher cannot rename a field to get around the
//! denylist, safe search, the license rule or keeping personal data at
//! home. Everything else goes in `fields`, typed by the set's manifest.
//! Keys this version does not know are kept and passed on, so an older
//! node never drops what a newer one wrote.

use std::collections::BTreeMap;

use anyhow::{ensure, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::refs::{is_namespace, Ref};
use crate::urls::is_canonical;
use crate::value::{check_text, hex, is_date, is_language};

/// Longest item, as JSON: the cap a crawl record has today. A manifest's
/// `limits` can lower it, never raise it.
pub const MAX_ITEM_BYTES: usize = 16 * 1024;
/// Longest id, in bytes.
pub const MAX_ID_BYTES: usize = 256;
/// Longest `text`, in characters, when an item is shared.
pub const MAX_TEXT_CHARS: usize = 1_000;
/// Most edges one item keeps.
pub const MAX_EDGES: usize = 100;
/// Most vectors one item keeps.
pub const MAX_VECTORS: usize = 4;

/// One record of a set: a thing, or an extension of one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// Unique within its set. It names the thing, never its label, so it
    /// survives a rename; it never changes and is never reused.
    pub id: String,
    /// What the record is (`site`, `article`, `package`), not what the
    /// thing is. Picks a kind handler when one is installed; the core never
    /// compares it to a particular value but `deny` ([`crate::deny`]) and
    /// `site-rule` ([`crate::sites`]).
    /// Needed for things; an extension takes its target's.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// The item this one adds to. Set for extensions, never for things.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<Ref>,
    /// Bumped when the item changes; the newer copy wins in a delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<u64>,
    /// Set on a tombstone: the item is gone from its set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<Deleted>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The canonical URL ([`crate::urls`]); http or https only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The site of the host `url` is on. A node works it out from `url`
    /// itself; publishers set it only on items without a URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// Other names, searched as names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// One line under the title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// The snippet, searched as body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The full text the builder read from the page, by hash: a
    /// `blob:sha256-...` ref. A node never needs the text itself; vectors
    /// and summaries made from it name the same hash in `of`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Ref>,
    /// A BCP 47 language tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// Popularity within its set; the manifest says what it counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    /// Dropped after this date (headlines, leads).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// Outside ids by namespace (`wd`, `doi`, `pypi`), which `wd:` and
    /// `id:` refs match.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ids: BTreeMap<String, String>,
    /// Policy labels: `adult`, `personal` (never shared),
    /// `machine-written`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// An SPDX license expression; defaults to the set's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<Edge>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vectors: Vec<Vector>,
    /// Everything else, typed by the set's manifest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, Value>,
    /// Keys this version doesn't know, kept as they came.
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

/// What a tombstone says about the item it removes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Deleted {
    /// When, as a date ([`crate::value::is_date`]).
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

/// A link from an item to another thing: `links_to`, `key_page`, `by`,
/// `part_of`, `about`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub rel: String,
    pub to: Ref,
    /// The link's words, when they say something (a link text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// How much the link counts, when not all links count alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<f64>,
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

/// A vector for search by meaning, made by `model` from the text whose
/// SHA-256 is `of`, and ignored where that text no longer matches. `b64`
/// holds its values, one signed byte each (a unit vector's values times
/// 127, rounded), in base64 with padding.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vector {
    pub model: String,
    pub of: String,
    pub b64: String,
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

impl Vector {
    /// The vector of `values` that the model whose id is `model` made from
    /// the text whose SHA-256 is `of`, both in hex.
    pub fn new(model: &[u8], of: &[u8], values: &[i8]) -> Self {
        let bytes: Vec<u8> = values.iter().map(|&value| value as u8).collect();
        Vector {
            model: hex(model),
            of: hex(of),
            b64: STANDARD.encode(bytes),
            other: BTreeMap::new(),
        }
    }

    /// The values `b64` holds, when it is base64.
    pub fn values(&self) -> Option<Vec<i8>> {
        let bytes = STANDARD.decode(&self.b64).ok()?;
        Some(bytes.into_iter().map(|byte| byte as i8).collect())
    }
}

impl Item {
    /// True for an extension of another item, false for a thing.
    pub fn is_extension(&self) -> bool {
        self.extends.is_some()
    }

    /// True for a tombstone.
    pub fn is_deleted(&self) -> bool {
        self.deleted.is_some()
    }

    /// Checks the rules every item keeps, whatever its set: a stable id,
    /// a kind for things and no title or URL for extensions, canonical
    /// http or https URLs, refs a reader takes, well-formed dates, ids and
    /// language, text in NFC, and the size caps. Field types are checked against the set's
    /// manifest ([`crate::Manifest::check_item`]).
    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty() && self.id.len() <= MAX_ID_BYTES,
            "item id {:?} is empty or longer than {MAX_ID_BYTES} bytes",
            self.id
        );
        ensure!(
            !self.id.chars().any(char::is_control),
            "item id {:?} has control characters",
            self.id
        );
        if self.is_extension() {
            ensure!(
                self.title.is_none() && self.url.is_none(),
                "extension {:?} has a title or URL of its own",
                self.id
            );
        } else if !self.is_deleted() {
            ensure!(!self.kind.is_empty(), "item {:?} has no kind", self.id);
        }
        if let Some(url) = &self.url {
            ensure!(
                is_canonical(url),
                "item {:?}: {url:?} is not a canonical URL",
                self.id
            );
        }
        if let Some(body) = &self.body {
            ensure!(
                matches!(body, Ref::Blob(_)),
                "item {:?}: body {body} is not a blob: ref",
                self.id
            );
        }
        if let Some(site) = &self.site {
            ensure!(
                crate::refs::is_domain(site),
                "item {:?}: {site:?} is not a domain",
                self.id
            );
        }
        // Reading an item checks its refs; one made in code is not, so each
        // must read back as itself, as it will where the item is read.
        let edges = self.edges.iter().map(|edge| &edge.to);
        for to in self.extends.iter().chain(&self.body).chain(edges) {
            let text = to.to_string();
            ensure!(
                text.parse::<Ref>().is_ok_and(|read| read == *to),
                "item {:?}: {text:?} is not a valid ref",
                self.id
            );
        }
        for date in [&self.published, &self.expires].into_iter().flatten() {
            ensure!(is_date(date), "item {:?}: {date:?} is not a date", self.id);
        }
        if let Some(deleted) = &self.deleted {
            ensure!(
                is_date(&deleted.at),
                "item {:?}: deleted at {:?} is not a date",
                self.id,
                deleted.at
            );
        }
        if let Some(lang) = &self.lang {
            ensure!(
                is_language(lang),
                "item {:?}: {lang:?} is not a language tag",
                self.id
            );
        }
        for namespace in self.ids.keys() {
            ensure!(
                is_namespace(namespace),
                "item {:?}: {namespace:?} is not an id namespace",
                self.id
            );
        }
        let texts = [&self.title, &self.about, &self.text].into_iter().flatten();
        for text in texts.chain(&self.names).chain(&self.labels) {
            check_text(text)?;
        }
        if let Some(text) = &self.text {
            ensure!(
                text.chars().count() <= MAX_TEXT_CHARS,
                "item {:?}: text is longer than {MAX_TEXT_CHARS} characters",
                self.id
            );
        }
        ensure!(
            self.edges.len() <= MAX_EDGES,
            "item {:?} has more than {MAX_EDGES} edges",
            self.id
        );
        ensure!(
            self.vectors.len() <= MAX_VECTORS,
            "item {:?} has more than {MAX_VECTORS} vectors",
            self.id
        );
        if let Some(weight) = self.weight {
            ensure!(
                weight.is_finite() && weight >= 0.0,
                "item {:?}: weight {weight} is not a count",
                self.id
            );
        }
        let bytes = serde_json::to_vec(self)?.len();
        ensure!(
            bytes <= MAX_ITEM_BYTES,
            "item {:?} is {bytes} bytes of JSON, more than {MAX_ITEM_BYTES}",
            self.id
        );
        Ok(())
    }

    /// True when `vector`, from this item or a layer on it, was made from
    /// what the item holds now: its `of` is the SHA-256 of the item's
    /// `text`, or the hash in its `body`. A node ignores a vector that
    /// isn't, since the page has changed since.
    pub fn is_current(&self, vector: &Vector) -> bool {
        let of_text = self
            .text
            .as_deref()
            .map(|text| hex(&Sha256::digest(text.as_bytes())));
        of_text.as_deref() == Some(vector.of.as_str())
            || matches!(&self.body, Some(Ref::Blob(hex)) if *hex == vector.of)
    }

    /// True when the item may be passed to another node: not labelled
    /// `personal`.
    pub fn may_share(&self) -> bool {
        !self.labels.iter().any(|label| label == "personal")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn australia() -> Item {
        serde_json::from_value(json!({
            "id": "4689264",
            "kind": "article",
            "title": "Australia",
            "url": "https://en.wikipedia.org/wiki/Australia",
            "ids": {"wd": "Q408"},
            "about": "country in Oceania",
            "text": "Australia, officially the Commonwealth of Australia, is a country.",
            "weight": 1234.0,
            "lang": "en"
        }))
        .unwrap()
    }

    #[test]
    fn a_thing_and_a_layer_entry() {
        australia().check().unwrap();
        let facts: Item = serde_json::from_value(json!({
            "id": "Q408",
            "extends": "wd:Q408",
            "fields": {"capital": {"v": "Canberra", "src": "wd:Q3114"}}
        }))
        .unwrap();
        facts.check().unwrap();
        assert!(facts.is_extension());
        assert_eq!(facts.extends, Some(Ref::Wd("Q408".into())));
    }

    #[test]
    fn unknown_keys_pass_through() {
        let text = r#"{"id":"1","kind":"article","future":{"a":[1,2]},"title":"T"}"#;
        let item: Item = serde_json::from_str(text).unwrap();
        assert_eq!(item.other["future"], json!({"a": [1, 2]}));
        let again: Value = serde_json::to_value(&item).unwrap();
        assert_eq!(again, serde_json::from_str::<Value>(text).unwrap());
    }

    #[test]
    fn broken_items() {
        let mut no_kind = australia();
        no_kind.kind.clear();
        let mut magnet = australia();
        magnet.url = Some("magnet:?xt=urn:btih:abc".into());
        let mut not_canonical = australia();
        not_canonical.url = Some("https://EN.wikipedia.org/wiki/Australia#x".into());
        let mut titled_extension = australia();
        titled_extension.extends = Some(Ref::Wd("Q408".into()));
        let mut long_text = australia();
        long_text.text = Some("a".repeat(MAX_TEXT_CHARS + 1));
        let mut huge = australia();
        huge.fields
            .insert("blob".into(), json!("x".repeat(MAX_ITEM_BYTES)));
        let mut bad_ids = australia();
        bad_ids.ids.insert("Wikidata".into(), "Q408".into());
        let mut bad_date = australia();
        bad_date.published = Some("last week".into());
        // Refs no reader would take: the URL is not canonical, and no
        // site is named with a `_`.
        let edge = |to: Ref| Edge {
            rel: "links_to".into(),
            to,
            text: None,
            w: None,
            other: BTreeMap::new(),
        };
        let mut bad_url_edge = australia();
        bad_url_edge
            .edges
            .push(edge(Ref::Url("https://example.com/%Ab".into())));
        let mut bad_site_edge = australia();
        bad_site_edge
            .edges
            .push(edge(Ref::Site("foo_bar.com".into())));
        let mut bad_extends = australia();
        bad_extends.title = None;
        bad_extends.url = None;
        bad_extends.extends = Some(Ref::Wd("Q0408".into()));
        for item in [
            no_kind,
            magnet,
            not_canonical,
            titled_extension,
            long_text,
            huge,
            bad_ids,
            bad_date,
            bad_url_edge,
            bad_site_edge,
            bad_extends,
        ] {
            assert!(item.check().is_err(), "{item:?}");
        }
    }

    #[test]
    fn tombstones_need_no_kind() {
        let gone: Item = serde_json::from_value(json!({
            "id": "4689264", "rev": 7, "deleted": {"at": "2026-10-12", "reason": "merged"}
        }))
        .unwrap();
        gone.check().unwrap();
        assert!(gone.is_deleted());
    }

    #[test]
    fn vectors_hold_while_the_text_or_body_they_came_from_does() {
        let body = "9f2c3b0e7d6a5f4e3d2c1b0a9f8e7d6c5b4a39281706f5e4d3c2b1a098f7e6d5";
        let mut item = australia();
        item.body = Some(format!("blob:sha256-{body}").parse().unwrap());
        item.check().unwrap();
        let text = item.text.clone().unwrap();
        let vector = |of: String| Vector {
            model: "bge-small-en-v1.5".into(),
            of,
            b64: String::new(),
            other: BTreeMap::new(),
        };
        let of_text: String = Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(item.is_current(&vector(of_text.clone())));
        assert!(item.is_current(&vector(body.into())));
        assert!(!item.is_current(&vector("0".repeat(64))));

        item.text = Some("Australia is a country.".into());
        item.body = None;
        assert!(!item.is_current(&vector(of_text)));
        assert!(!item.is_current(&vector(body.into())));

        let mut not_a_blob = australia();
        not_a_blob.body = Some(Ref::Wd("Q408".into()));
        assert!(not_a_blob.check().is_err());
    }

    #[test]
    fn a_vector_holds_its_values_as_signed_bytes() {
        let vector = Vector::new(&[0xab; 32], &[1; 32], &[-127, -1, 0, 1, 127]);
        assert_eq!(vector.model, "ab".repeat(32));
        assert_eq!(vector.of, "01".repeat(32));
        assert_eq!(vector.b64, "gf8AAX8=");
        assert_eq!(vector.values().unwrap(), [-127, -1, 0, 1, 127]);
        let not_base64 = Vector {
            b64: "not base64!".into(),
            ..vector
        };
        assert!(not_base64.values().is_none());
    }

    #[test]
    fn personal_items_are_not_shared() {
        let mut item = australia();
        assert!(item.may_share());
        item.labels.push("personal".into());
        assert!(!item.may_share());
    }
}
