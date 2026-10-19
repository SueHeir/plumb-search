//! A site as a node keeps it on disk: a line of its records file, its
//! journal, its inbox and its bucket table. Everything that reads or
//! writes those goes through here, so that their shape can change in one
//! place. A line is the site's stored item ([`crate::site`]): the item it
//! sends ([`crate::site::to_item`]) with all else the node keeps of it in
//! fields of its own. Its line starts `{"id":`.
//!
//! Versions up to 0.4 wrote the site's record JSON ([`old_record`]), a
//! line that starts `{"domain":`, and reading takes either shape, so a
//! file can hold both. Nothing moves a file over on its own: its lines
//! become items as the node writes them again, as a fold of the journal or
//! compaction writes every site; trimming copies the lines it keeps as
//! they are. The file keeps its name, `records.jsonl`, so that a node going
//! back to version 0.4, which reads items too, finds it as it was left.
//!
//! A site is written as it is, not cleaned: a set cleans what it takes
//! ([`crate::sites::Sites::upsert`]), as a node changes its sites and as it
//! loads them, so what a node writes of its sets is clean already, and a
//! change in the journal replays as the change it was. The one exception
//! is a site whose item would not read back as it is: an edge is a ref
//! ([`plumb_format::Ref`]), which reading checks, and a crawl or another
//! node's site can link to a host no ref can name (with a `_`) or hold a
//! key page at a URL that is not canonical. Such a site is written cleaned
//! ([`crate::site::clean`]), as a set would clean it on taking it; written
//! as it is, the line would be refused, which stops a records file from
//! loading and drops a journal's change.
//!
//! What a node sends is not stored and does not come through here: crawl
//! batches and bucket answers carry items ([`crate::site::to_line`]), and
//! private search hands a browser trimmed items
//! ([`crate::keys::slim_site`]). Only `/plumb/fill/1` sends what it
//! stores, as the record's JSON ([`record_json`]) and never the stored
//! item, since every node reads a fill answer as a record's JSON.
//!
//! [`read_item`] reads a line of either shape as the site's stored item,
//! and [`write_item`] and [`item_as_line`] write it. A site inside another
//! stored value, such as a change in the journal, is written and read the
//! same way with `#[serde(with = "plumb_core::stored::item")]` ([`item`]).

use std::borrow::Cow;
use std::io::Write;

use anyhow::{ensure, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use plumb_format::{Item, Ref};
use serde_json::Value;

use crate::site_item::{plainly_canonical, SITE};
use crate::{site, Redirect, Signals};

pub mod old_record;

/// The site's stored item a stored line holds, in either shape. A record's
/// JSON is taken as it is ([`old_record::into_item`]), not cleaned, as an
/// item is; a set cleans what it takes ([`crate::sites::Sites::upsert`]).
pub fn read_item(line: &[u8]) -> Result<Item> {
    if is_item(line) {
        return site_item(serde_json::from_slice(line)?);
    }
    Ok(old_record::into_item(serde_json::from_slice(line)?))
}

/// Writes a site's stored item as a stored line, without its line break:
/// the item as it is, unless it would not read back so ([`as_stored`]).
pub fn write_item<W: Write>(out: W, site: &Item) -> Result<()> {
    serde_json::to_writer(out, &*as_stored(site))?;
    Ok(())
}

/// A site's stored item as a stored line, without its line break, as
/// [`write_item`] writes it.
pub fn item_as_line(site: &Item) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&*as_stored(site))?)
}

/// `site` as a stored line holds it: as it is, unless an edge of it is no
/// ref reading takes ([`Ref::check`]), as a link to a host with a `_` or a
/// key page at a URL that is not canonical, and then cleaned
/// ([`crate::site::clean`]), which leaves only edges that read. A key
/// page's URL of the plain shape most are is told canonical without
/// reading it.
fn as_stored(site: &Item) -> Cow<'_, Item> {
    let reads = |to: &Ref| match to {
        Ref::Url(url) if plainly_canonical(url) => true,
        to => to.check().is_ok(),
    };
    if site.edges.iter().all(|edge| reads(&edge.to)) {
        return Cow::Borrowed(site);
    }
    let mut clean = site.clone();
    crate::site::clean(&mut clean);
    Cow::Owned(clean)
}

/// A stored item read as it is, once it is a site's.
fn site_item(site: Item) -> Result<Item> {
    ensure!(site.kind == SITE, "item {:?} is not a site", site.id);
    Ok(site)
}

/// Whether a stored line is a stored item rather than a record's JSON.
fn is_item(line: &[u8]) -> bool {
    let line = line.trim_ascii_start();
    line.strip_prefix(b"{")
        .is_some_and(|rest| rest.trim_ascii_start().starts_with(b"\"id\""))
}

/// A stored line as the site's record JSON ([`old_record`]), which
/// `/plumb/fill/1` answers with: for a line of that JSON, the line itself,
/// byte for byte, and for a stored item, its record's ([`record_json_of`]).
pub fn record_json(line: String) -> Result<String> {
    if is_item(line.as_bytes()) {
        return record_json_of(&serde_json::from_slice(line.as_bytes())?);
    }
    Ok(line)
}

/// The record JSON ([`old_record`]) of a site's stored item, as it is:
/// what [`record_json`] answers with for the item's line.
pub fn record_json_of(site: &Item) -> Result<String> {
    Ok(serde_json::to_string(&old_record::of_item(site)?)?)
}

/// The stored item of a site sent as its record JSON, as a `/plumb/fill/1`
/// answer carries each ([`record_json`]): the item of the record as it is
/// ([`old_record::into_item`]), so that a node keeps a filled site as it
/// kept the record. A crawl time no item can hold (past the year 9999) is
/// refused, as a crawl from the future is ([`crate::site::set_crawled`]).
pub fn read_record_json(line: &str) -> Result<Item> {
    let record: old_record::SiteRecord = serde_json::from_str(line)?;
    ensure!(
        record
            .crawled_at
            .is_none_or(|at| crate::site_item::timestamp(at).is_some()),
        "{} was crawled past the year 9999",
        record.domain
    );
    Ok(old_record::into_item(record))
}

/// What a stored line says of its site that putting sites in order and
/// folding redirects needs, read without the rest of the line.
#[derive(Debug)]
pub struct Head {
    /// The site's domain, its item's id.
    pub domain: String,
    /// [`site::signals`].
    pub signals: Signals,
    /// [`site::redirect`].
    pub redirect: Option<Redirect>,
    /// When the site was judged gone ([`site::CrawlMarks::gone`]).
    pub gone_at: Option<u64>,
}

/// The [`Head`] of a stored line, in either shape.
pub fn head(line: &[u8]) -> Result<Head> {
    if !is_item(line) {
        return old_record::head(line);
    }
    /// The members and fields of a stored item a [`Head`] reads.
    #[derive(Deserialize)]
    struct ItemHead {
        id: String,
        #[serde(default)]
        kind: String,
        #[serde(default)]
        weight: Option<f64>,
        #[serde(default)]
        fields: HeadFields,
    }
    #[derive(Deserialize, Default)]
    struct HeadFields {
        ranks: Option<Value>,
        wikidata: Option<Value>,
        fact_checks: Option<Value>,
        redirect: Option<Value>,
        crawl: Option<Value>,
    }
    let head: ItemHead = serde_json::from_slice(line)?;
    ensure!(head.kind == SITE, "item {:?} is not a site", head.id);
    let fields = head.fields;
    let fields = [
        ("ranks", fields.ranks),
        ("wikidata", fields.wikidata),
        ("fact_checks", fields.fact_checks),
        ("redirect", fields.redirect),
        ("crawl", fields.crawl),
    ];
    let item = Item {
        id: head.id,
        kind: head.kind,
        weight: head.weight,
        fields: fields
            .into_iter()
            .filter_map(|(name, value)| Some((name.to_string(), value?)))
            .collect(),
        ..Item::default()
    };
    Ok(Head {
        signals: site::signals(&item),
        redirect: site::redirect(&item),
        gone_at: site::crawl_marks(&item).gone,
        domain: item.id,
    })
}

/// Whether a stored value is a record's JSON rather than a stored item.
fn is_record(value: &Value) -> bool {
    value.get("domain").is_some() || value.get("id").is_none()
}

/// A site's stored item inside another stored value, such as a change in
/// the journal, with `#[serde(with = "plumb_core::stored::item")]`:
/// written as [`write_item`] writes it, and read in either shape as
/// [`read_item`] reads it.
pub mod item {
    use super::*;

    /// Writes a site's stored item inside another stored value, as
    /// [`write_item`] writes it.
    pub fn serialize<S: Serializer>(site: &Item, serializer: S) -> Result<S::Ok, S::Error> {
        as_stored(site).serialize(serializer)
    }

    /// Reads a site's stored item inside another stored value, in either
    /// shape.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Item, D::Error> {
        use serde::de::Error;
        let value = Value::deserialize(deserializer)?;
        if is_record(&value) {
            let record = old_record::SiteRecord::deserialize(value).map_err(D::Error::custom)?;
            return Ok(old_record::into_item(record));
        }
        let site = Item::deserialize(value).map_err(D::Error::custom)?;
        site_item(site).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site::tests::full_site;
    use crate::KeyPage;

    /// A site as a crawl or another node can make it, not clean but with
    /// edges a reader takes: text to tidy, a redirect to a site no item
    /// sent elsewhere can name, and the node's own crawl marks, icon and
    /// headlines, which its stored item keeps.
    fn untidy_site() -> Item {
        let mut site = full_site();
        site.title = Some(" Welcome\u{7} to Python ".into());
        site.names.push("Python".into());
        site.text = Some(format!("{} ", "word ".repeat(400)));
        let mut redirect = site::redirect(&site).unwrap();
        redirect.to = "foo_bar.com".into();
        site::set_redirect(&mut site, Some(redirect));
        site
    }

    #[test]
    fn a_stored_line_is_the_site_s_stored_item_as_it_is() {
        let mut lines = Vec::new();
        for site in [untidy_site(), full_site(), site::new("example.com")] {
            let line = item_as_line(&site).unwrap();
            assert!(line.starts_with(b"{\"id\":"));
            assert_eq!(line, serde_json::to_vec(&site).unwrap());
            let mut written = Vec::new();
            write_item(&mut written, &site).unwrap();
            assert_eq!(written, line);
            assert_eq!(read_item(&line).unwrap(), site);

            // Its record JSON, as versions up to 0.4 wrote it, reads as the
            // same item, and says the same of its head.
            let old = record_json_of(&site).unwrap();
            assert!(old.starts_with("{\"domain\":"), "{old}");
            assert_eq!(read_item(old.as_bytes()).unwrap(), site);
            let (new, old_head) = (head(&line).unwrap(), head(old.as_bytes()).unwrap());
            assert_eq!(new.domain, site.id);
            assert_eq!(old_head.domain, site.id);
            assert_eq!(new.signals, site::signals(&site));
            assert_eq!(old_head.signals, new.signals);
            assert_eq!(new.redirect, site::redirect(&site));
            assert_eq!(old_head.redirect, new.redirect);
            assert_eq!(new.gone_at, site::crawl_marks(&site).gone);
            assert_eq!(old_head.gone_at, new.gone_at);

            // `/plumb/fill/1` answers with the record JSON, byte for byte
            // what it answered when the line was that JSON.
            let json = record_json(String::from_utf8(line.clone()).unwrap()).unwrap();
            assert_eq!(json, old);
            assert_eq!(record_json(old.clone()).unwrap(), old);
            lines.push(line);
            lines.push(old.into_bytes());
        }
        let gone = head(&lines[0]).unwrap().gone_at;
        assert_eq!(gone, Some(1_791_000_000));
        let redirect = head(&lines[1]).unwrap().redirect.unwrap();
        assert_eq!(
            (redirect.to.as_str(), redirect.at, redirect.inner),
            ("foo_bar.com", 1_791_605_520, true)
        );
        assert!(head(br#"{"id": "example.com", "kind": "page"}"#).is_err());
    }

    #[test]
    fn a_site_whose_edges_would_not_read_back_is_stored_clean() {
        // A link to a host no site ref can name and a key page at a URL
        // that is not canonical, as a crawl can find them.
        let mut site = untidy_site();
        let mut links = site::links_to(&site);
        links.push("foo_bar.blogspot.com".into());
        site::set_links_to(&mut site, links);
        let mut pages = site::key_pages(&site);
        pages.push(KeyPage {
            label: " Docs ".into(),
            url: "HTTPS://Docs.Python.org/3/#top".into(),
        });
        site::set_key_pages(&mut site, pages);
        let refused = serde_json::to_vec(&site).unwrap();
        assert!(read_item(&refused).is_err());

        let mut clean = site.clone();
        site::clean(&mut clean);
        let line = item_as_line(&site).unwrap();
        assert_eq!(line, serde_json::to_vec(&clean).unwrap());
        assert_eq!(read_item(&line).unwrap(), clean);
        assert_eq!(site::links_to(&clean), ["psf.org", "pypi.org"]);
        let docs = site::key_pages(&clean).pop().unwrap();
        assert_eq!(
            (docs.label.as_str(), docs.url.as_str()),
            ("Docs", "https://docs.python.org/3/")
        );
        // The whole page text, the crawl marks and a redirect no item sent
        // elsewhere can hold are kept.
        assert_eq!(clean.text.as_ref().unwrap().len(), 1_999);
        assert_eq!(clean.title.as_deref(), Some("Welcome  to Python"));
        assert_eq!(site::crawl_marks(&clean), site::crawl_marks(&site));
        assert_eq!(site::redirect(&clean).unwrap().to, "foo_bar.com");

        // A URL told canonical without reading it is one.
        let mut plain = site::new("example.com");
        let docs = KeyPage {
            label: "Docs".into(),
            url: "https://www.example.com/docs".into(),
        };
        site::set_key_pages(&mut plain, vec![docs]);
        let plain_line = item_as_line(&plain).unwrap();
        assert_eq!(plain_line, serde_json::to_vec(&plain).unwrap());
        assert_eq!(read_item(&plain_line).unwrap(), plain);
    }

    #[test]
    fn only_a_site_s_item_is_a_stored_site() {
        assert!(read_item(br#"{"id": "example.com", "kind": "page"}"#).is_err());
        assert!(record_json(r#"{"id": "example.com", "kind": "page"}"#.into()).is_err());
        assert!(read_item(b"not json").is_err());
        assert!(read_item(br#"{"title": "no domain"}"#).is_err());
    }

    #[test]
    fn a_site_inside_another_stored_value_is_written_as_its_stored_item() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Change {
            #[serde(with = "crate::stored::item")]
            site: Item,
        }
        let change = Change {
            site: untidy_site(),
        };
        let json = serde_json::to_string(&change).unwrap();
        let line = String::from_utf8(item_as_line(&change.site).unwrap()).unwrap();
        assert_eq!(json, format!(r#"{{"site":{line}}}"#));
        assert_eq!(serde_json::from_str::<Change>(&json).unwrap(), change);

        // As versions up to 0.4 wrote it, with the record JSON.
        let old = record_json_of(&change.site).unwrap();
        let old = format!(r#"{{"site": {old}}}"#);
        assert_eq!(serde_json::from_str::<Change>(&old).unwrap(), change);

        // One that would not read back as it is goes in clean.
        let mut crawled = change.site;
        let mut links = site::links_to(&crawled);
        links.push("foo_bar.blogspot.com".into());
        site::set_links_to(&mut crawled, links);
        let json = serde_json::to_string(&Change {
            site: crawled.clone(),
        })
        .unwrap();
        let back: Change = serde_json::from_str(&json).unwrap();
        site::clean(&mut crawled);
        assert_eq!(back.site, crawled);

        let page = r#"{"site": {"id": "example.com", "kind": "page"}}"#;
        assert!(serde_json::from_str::<Change>(page).is_err());
    }

    /// A fill answer's site reads as the stored item it was sent of, and
    /// back as the same JSON, but for a crawl no item can date.
    #[test]
    fn a_record_s_json_reads_as_its_stored_item() {
        for site in [full_site(), untidy_site(), site::new("a.com")] {
            let json = record_json_of(&site).unwrap();
            assert_eq!(read_record_json(&json).unwrap(), site);
            let line = item_as_line(&site).unwrap();
            assert_eq!(record_json(String::from_utf8(line).unwrap()).unwrap(), json);
        }
        let late = r#"{"domain":"a.com","signals":{},"crawled_at":300000000000}"#;
        assert!(read_record_json(late).is_err());
        assert!(read_record_json("{\"title\": \"no domain\"}").is_err());
    }
}
