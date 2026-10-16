//! A site as a node keeps it on disk: a line of its records file, its
//! journal, its inbox and its bucket table. Everything that reads or
//! writes those goes through here, so that their shape can change in one
//! place. A line is the site's stored item ([`SiteRecord::to_stored_item`]):
//! what [`SiteRecord::to_item`] makes, with all else a record holds in
//! fields of its own. Its line starts `{"id":`.
//!
//! Versions up to 0.4 wrote the [`SiteRecord`]'s own JSON, a line that
//! starts `{"domain":`, and reading takes either shape, so a file can hold
//! both. Nothing moves a file over on its own: its lines become items as
//! the node writes them again, as a fold of the journal or compaction
//! writes every site; trimming copies the lines it keeps as they are. The
//! file keeps its name, `records.jsonl`, so that a node going back to
//! version 0.4, which reads items too, finds it as it was left.
//!
//! A site is written as it is ([`SiteRecord::stored_item_as_is`]), not
//! cleaned: a set cleans what it takes ([`crate::RecordSet::upsert`],
//! [`crate::sites::Sites::upsert`]), as a node changes its sites and as it
//! loads them, so what a node writes of its sets is clean already, and a
//! change in the journal replays as the change it was. The one exception
//! is a site whose item would not read back as it is: an edge is a ref
//! ([`plumb_format::Ref`]), which reading checks, and a crawl or another
//! node's record can link to a host no ref can name (with a `_`) or hold
//! a key page at a URL that is not canonical. Such a site is written
//! cleaned ([`crate::site::clean`]), as a set would clean it on taking it;
//! written as it is, the line would be refused, which stops a records
//! file from loading and drops a journal's change.
//!
//! What a node sends is not stored and does not come through here: crawl
//! batches and bucket answers carry items ([`SiteRecord::to_line`]), and
//! private search hands a browser trimmed records. Only `/plumb/fill/1`
//! sends what it stores, as the record's own JSON ([`record_json`]) and
//! never the stored item, since every node reads a fill answer as a
//! record's JSON.
//!
//! A record inside another stored value, such as a change in the journal,
//! is written and read the same way with
//! `#[serde(with = "plumb_core::stored")]`.
//!
//! Code that holds sites as their stored items ([`crate::sites::Sites`])
//! reads and writes them here too: [`read_item`] reads a line of either
//! shape as the site's stored item, and [`write_item`] and
//! [`item_as_line`] write it, as [`write`] and [`line`] write a record. An
//! item inside another stored value goes through [`item`] in the same way.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use plumb_format::{Item, Ref};
use serde_json::Value;

use crate::site_item::{plainly_canonical, SITE};
use crate::{Redirect, Signals, SiteRecord};

/// Writes `record` as a stored line, without its line break: its stored
/// item as it is ([`SiteRecord::stored_item_as_is`]); see the module doc.
pub fn write<W: Write>(out: W, record: &SiteRecord) -> Result<()> {
    write_item(out, &record.clone().stored_item_as_is())
}

/// `record` as a stored line, without its line break, as [`write`] writes
/// it.
pub fn line(record: &SiteRecord) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    write(&mut line, record)?;
    Ok(line)
}

/// The site a stored line holds, in either shape.
pub fn read(line: &[u8]) -> Result<SiteRecord> {
    if is_item(line) {
        let item: Item = serde_json::from_slice(line)?;
        return SiteRecord::from_stored_item(&item);
    }
    Ok(serde_json::from_slice(line)?)
}

/// The stored item ([`SiteRecord::to_stored_item`]) a stored line holds,
/// in either shape. A record's JSON is mapped as it is
/// ([`SiteRecord::stored_item_as_is`]), not cleaned, since [`read`] does
/// not clean either; a set cleans what it takes
/// ([`crate::sites::Sites::upsert`]).
pub fn read_item(line: &[u8]) -> Result<Item> {
    if is_item(line) {
        return site_item(serde_json::from_slice(line)?);
    }
    let record: SiteRecord = serde_json::from_slice(line)?;
    Ok(record.stored_item_as_is())
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

/// A stored item read as it is, once it is a site's, as [`read`] takes
/// only a site.
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

/// Every site of a file of stored lines, in file order, as
/// [`crate::read_jsonl`] reads one: each line trimmed, blank ones skipped.
pub fn read_file(path: &Path) -> Result<Vec<SiteRecord>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut sites = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let site = read(line.as_bytes())
            .with_context(|| format!("{}:{}: invalid JSON line", path.display(), i + 1))?;
        sites.push(site);
    }
    Ok(sites)
}

/// A stored line as the record's own JSON, which `/plumb/fill/1` answers
/// with: the JSON of the record [`read`] reads, which for a line of the
/// record's JSON is that line, byte for byte.
pub fn record_json(line: String) -> Result<String> {
    if is_item(line.as_bytes()) {
        return Ok(serde_json::to_string(&read(line.as_bytes())?)?);
    }
    Ok(line)
}

/// What a stored line says of its site that putting sites in order and
/// folding redirects needs, read without the rest of the line.
#[derive(Debug, Deserialize)]
pub struct Head {
    /// [`SiteRecord::domain`].
    pub domain: String,
    /// [`SiteRecord::signals`].
    #[serde(default)]
    pub signals: Signals,
    /// [`SiteRecord::redirect`].
    #[serde(default)]
    pub redirect: Option<Redirect>,
    /// [`SiteRecord::gone_at`].
    #[serde(default)]
    pub gone_at: Option<u64>,
}

/// The [`Head`] of a stored line, in either shape.
pub fn head(line: &[u8]) -> Result<Head> {
    if !is_item(line) {
        return Ok(serde_json::from_slice(line)?);
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
    let record = SiteRecord::from_stored_item(&item)?;
    Ok(Head {
        domain: record.domain,
        signals: record.signals,
        redirect: record.redirect,
        gone_at: record.gone_at,
    })
}

/// Writes a record inside another stored value as its stored item, as
/// [`write`] writes it; see the module doc.
pub fn serialize<S: Serializer>(record: &SiteRecord, serializer: S) -> Result<S::Ok, S::Error> {
    item::serialize(&record.clone().stored_item_as_is(), serializer)
}

/// Reads a record inside another stored value, in either shape; see the
/// module doc.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SiteRecord, D::Error> {
    use serde::de::Error;
    let value = Value::deserialize(deserializer)?;
    if is_record(&value) {
        return SiteRecord::deserialize(value).map_err(D::Error::custom);
    }
    let item = Item::deserialize(value).map_err(D::Error::custom)?;
    SiteRecord::from_stored_item(&item).map_err(D::Error::custom)
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
            let record = SiteRecord::deserialize(value).map_err(D::Error::custom)?;
            return Ok(record.stored_item_as_is());
        }
        let site = Item::deserialize(value).map_err(D::Error::custom)?;
        site_item(site).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site_item::tests::full_record;
    use crate::KeyPage;

    /// A line as versions up to 0.4 wrote it: the record's own JSON.
    fn record_line(record: &SiteRecord) -> Vec<u8> {
        serde_json::to_vec(record).unwrap()
    }

    /// A record as a crawl or another node can make it, not clean but
    /// with edges a reader takes: text to tidy, and the node's own crawl
    /// marks, icon and headlines, which its stored item keeps.
    fn untidy_record() -> SiteRecord {
        let mut record = full_record();
        record.title = Some(" Welcome\u{7} to Python ".into());
        record.aliases.push("Python".into());
        record.body_text = Some(format!("{} ", "word ".repeat(400)));
        record.redirect.as_mut().unwrap().to = "foo_bar.com".into();
        record
    }

    #[test]
    fn a_stored_line_is_the_site_s_stored_item_as_it_is() {
        let mut clean = full_record();
        clean.clean();
        for record in [untidy_record(), clean, SiteRecord::new("example.com")] {
            let line = line(&record).unwrap();
            assert!(line.starts_with(b"{\"id\":"));
            assert_eq!(
                line,
                serde_json::to_vec(&record.clone().stored_item_as_is()).unwrap()
            );
            let mut written = Vec::new();
            write(&mut written, &record).unwrap();
            assert_eq!(written, line);

            // It reads back as the record as it is, as the record's own
            // JSON does, and as the same stored item.
            assert_eq!(read(&line).unwrap(), record);
            assert_eq!(read(&record_line(&record)).unwrap(), record);
            assert_eq!(
                read_item(&line).unwrap(),
                record.clone().stored_item_as_is()
            );
            assert_eq!(
                read_item(&line).unwrap(),
                read_item(&record_line(&record)).unwrap()
            );
            let head = head(&line).unwrap();
            let old = super::head(&record_line(&record)).unwrap();
            assert_eq!(head.domain, old.domain);
            assert_eq!(head.signals, old.signals);
            assert_eq!(head.redirect, old.redirect);
            assert_eq!(head.gone_at, old.gone_at);

            // `/plumb/fill/1` answers with the record's JSON, byte for byte
            // what it answered when the line was that JSON.
            let json = record_json(String::from_utf8(line.clone()).unwrap()).unwrap();
            assert_eq!(json.as_bytes(), record_line(&record));
            let old = String::from_utf8(record_line(&record)).unwrap();
            assert_eq!(record_json(old.clone()).unwrap(), old);

            // A site's item is written as the same line.
            let site = read_item(&line).unwrap();
            assert_eq!(item_as_line(&site).unwrap(), line);
            let mut written = Vec::new();
            write_item(&mut written, &site).unwrap();
            assert_eq!(written, line);
        }
        let untidy = untidy_record();
        assert_eq!(read(&line(&untidy).unwrap()).unwrap().title, untidy.title);

        // A file can hold both shapes, as one written by an older node
        // holds until each of its lines is written again.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let other = SiteRecord::new("example.org");
        let mut file = line(&untidy).unwrap();
        file.extend_from_slice(b"\n\n");
        file.extend_from_slice(&record_line(&other));
        file.push(b'\n');
        std::fs::write(&path, file).unwrap();
        assert_eq!(read_file(&path).unwrap(), vec![untidy, other]);
    }

    #[test]
    fn a_site_whose_edges_would_not_read_back_is_stored_clean() {
        // A link to a host no site ref can name and a key page at a URL
        // that is not canonical, as a crawl can find them.
        let mut record = untidy_record();
        record.links_to.push("foo_bar.blogspot.com".into());
        record.key_pages.push(KeyPage {
            label: " Docs ".into(),
            url: "HTTPS://Docs.Python.org/3/#top".into(),
        });
        let as_is = record.clone().stored_item_as_is();
        let refused = serde_json::to_vec(&as_is).unwrap();
        assert!(read(&refused).is_err());

        let mut clean = record.clone();
        clean.clean();
        let line = line(&record).unwrap();
        assert_eq!(line, serde_json::to_vec(&record.to_stored_item()).unwrap());
        assert_eq!(read(&line).unwrap(), clean);
        assert_eq!(item_as_line(&as_is).unwrap(), line);
        assert_eq!(clean.links_to, full_record().links_to);
        assert_eq!(
            clean.key_pages.last().unwrap().url,
            "https://docs.python.org/3/"
        );
        // The whole page text, the crawl marks and a redirect no item sent
        // elsewhere can hold are kept.
        assert_eq!(clean.body_text.as_ref().unwrap().len(), 1_999);
        assert_eq!(clean.crawl_failures, record.crawl_failures);
        assert_eq!(clean.redirect.as_ref().unwrap().to, "foo_bar.com");

        // A URL told canonical without reading it is one.
        let mut plain = SiteRecord::new("example.com");
        plain.key_pages.push(KeyPage {
            label: "Docs".into(),
            url: "https://www.example.com/docs".into(),
        });
        let plain_line = super::line(&plain).unwrap();
        assert_eq!(read(&plain_line).unwrap(), plain);
    }

    #[test]
    fn a_stored_line_is_read_as_its_stored_item_whatever_its_shape() {
        let mut full = full_record();
        full.clean();
        for record in [full, untidy_record(), SiteRecord::new("example.com")] {
            let site = read_item(&record_line(&record)).unwrap();
            assert_eq!(site, record.clone().stored_item_as_is());
            assert_eq!(read_item(&line(&record).unwrap()).unwrap(), site);
        }
        // Only a site's item is a stored site.
        assert!(read_item(br#"{"id": "example.com", "kind": "page"}"#).is_err());
        assert!(read(br#"{"id": "example.com", "kind": "page"}"#).is_err());
        assert!(read_item(b"not json").is_err());
    }

    #[test]
    fn a_site_inside_another_stored_value_is_written_as_its_stored_item() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Change {
            #[serde(with = "crate::stored::item")]
            site: Item,
        }
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct RecordChange {
            #[serde(with = "crate::stored")]
            site: SiteRecord,
        }
        let record = untidy_record();
        let change = Change {
            site: record.clone().stored_item_as_is(),
        };
        let json = serde_json::to_string(&change).unwrap();
        let line = String::from_utf8(line(&record).unwrap()).unwrap();
        assert_eq!(json, format!(r#"{{"site":{line}}}"#));
        let record_change = RecordChange {
            site: record.clone(),
        };
        assert_eq!(json, serde_json::to_string(&record_change).unwrap());
        assert_eq!(serde_json::from_str::<Change>(&json).unwrap(), change);
        assert_eq!(
            serde_json::from_str::<RecordChange>(&json).unwrap(),
            record_change
        );

        // As versions up to 0.4 wrote it, with the record's own JSON.
        let old = String::from_utf8(record_line(&record)).unwrap();
        let old = format!(r#"{{"site": {old}}}"#);
        assert_eq!(serde_json::from_str::<Change>(&old).unwrap(), change);
        assert_eq!(
            serde_json::from_str::<RecordChange>(&old).unwrap(),
            record_change
        );

        // One that would not read back as it is goes in clean.
        let mut crawled = record;
        crawled.links_to.push("foo_bar.blogspot.com".into());
        let json = serde_json::to_string(&RecordChange {
            site: crawled.clone(),
        })
        .unwrap();
        let mut clean = crawled.clone();
        clean.clean();
        let back: RecordChange = serde_json::from_str(&json).unwrap();
        assert_eq!(back.site, clean);
        let json = serde_json::to_string(&Change {
            site: crawled.stored_item_as_is(),
        })
        .unwrap();
        let back: Change = serde_json::from_str(&json).unwrap();
        assert_eq!(back.site, clean.stored_item_as_is());

        let page = r#"{"site": {"id": "example.com", "kind": "page"}}"#;
        assert!(serde_json::from_str::<Change>(page).is_err());
    }
}
