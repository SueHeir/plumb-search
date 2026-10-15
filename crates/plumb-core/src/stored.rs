//! A site as a node keeps it on disk: a line of its records file, its
//! journal, its inbox and its bucket table. Everything that reads or
//! writes those goes through here, so that their shape can change in one
//! place. Today a line is written as the [`SiteRecord`]'s own JSON; site
//! items of the data format are to take its place, as the stored item
//! [`SiteRecord::to_stored_item`] makes: what [`SiteRecord::to_item`]
//! makes, with all else a clean record holds in fields of its own.
//! Reading takes either shape already, so that a node can go back to this
//! version once a later one writes items. A stored item's line starts
//! `{"id":`, and a record's `{"domain":`.
//!
//! What a node sends is not stored and does not come through here: crawl
//! batches and bucket answers carry items ([`SiteRecord::to_line`]), and
//! private search hands a browser trimmed records. Only `/plumb/fill/1`
//! sends what it stores, as the record's own JSON ([`record_json`]).
//!
//! A record inside another stored value, such as a change in the journal,
//! is written and read the same way with
//! `#[serde(with = "plumb_core::stored")]`.
//!
//! Code that holds sites as their stored items ([`crate::sites::Sites`])
//! reads and writes them here too, while the files still hold records'
//! JSON: [`read_item`] reads a line of either shape as the site's stored
//! item, and [`write_item`] and [`item_as_line`] write a stored item as
//! the record's JSON ([`SiteRecord::from_stored_item`]), as [`write`] and
//! [`line`] write a record. An item inside another stored value goes
//! through [`item`] in the same way. A later version switches these to
//! write the stored item itself.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use plumb_format::Item;
use serde_json::Value;

use crate::site_item::SITE;
use crate::{Redirect, Signals, SiteRecord};

/// Writes `record` as a stored line, without its line break.
pub fn write<W: Write>(out: W, record: &SiteRecord) -> Result<()> {
    serde_json::to_writer(out, record)?;
    Ok(())
}

/// `record` as a stored line, without its line break.
pub fn line(record: &SiteRecord) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(record)?)
}

/// `record` as a stored item's line ([`SiteRecord::to_stored_item`]),
/// without its line break: what [`line`] is to write once nodes read
/// either shape.
pub fn item_line(record: &SiteRecord) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&record.to_stored_item())?)
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
/// for now the record's JSON, as [`write`] writes it, so that the file
/// stays one every node reads. A later version writes the item itself.
pub fn write_item<W: Write>(out: W, site: &Item) -> Result<()> {
    write(out, &SiteRecord::from_stored_item(site)?)
}

/// A site's stored item as a stored line, without its line break, as
/// [`write_item`] writes it: for now the record's JSON, as [`line`] makes
/// it. A later version makes the item's own line.
pub fn item_as_line(site: &Item) -> Result<Vec<u8>> {
    line(&SiteRecord::from_stored_item(site)?)
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
/// with. A line written as JSON is that already.
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

/// Writes a record inside another stored value; see the module doc.
pub fn serialize<S: Serializer>(record: &SiteRecord, serializer: S) -> Result<S::Ok, S::Error> {
    record.serialize(serializer)
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
/// written as [`write_item`] writes it, for now the record's JSON, and read
/// in either shape as [`read_item`] reads it.
pub mod item {
    use super::*;

    /// Writes a site's stored item inside another stored value: for now
    /// the record's JSON. A later version writes the item itself.
    pub fn serialize<S: Serializer>(site: &Item, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error;
        let record = SiteRecord::from_stored_item(site).map_err(S::Error::custom)?;
        record.serialize(serializer)
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

    #[test]
    fn a_stored_line_reads_back_as_its_record() {
        let mut record = crate::home_site_record();
        record.gone_at = Some(5);
        record.signals.tranco_rank = Some(10);
        record.redirect = Some(Redirect {
            to: "example.com".into(),
            at: 7,
            inner: false,
        });
        let line = line(&record).unwrap();
        assert_eq!(read(&line).unwrap(), record);

        let mut written = Vec::new();
        write(&mut written, &record).unwrap();
        assert_eq!(written, line);

        let head = head(&line).unwrap();
        assert_eq!(head.domain, record.domain);
        assert_eq!(head.signals, record.signals);
        assert_eq!(head.redirect, record.redirect);
        assert_eq!(head.gone_at, Some(5));

        // Today's lines are the record's own JSON, which nodes already hold.
        assert_eq!(line, serde_json::to_vec(&record).unwrap());
        let json = record_json(String::from_utf8(line.clone()).unwrap()).unwrap();
        assert_eq!(serde_json::from_str::<SiteRecord>(&json).unwrap(), record);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let other = SiteRecord::new("example.org");
        let mut file = line;
        file.extend_from_slice(b"\n\n");
        file.extend_from_slice(&super::line(&other).unwrap());
        file.push(b'\n');
        std::fs::write(&path, file).unwrap();
        assert_eq!(read_file(&path).unwrap(), vec![record, other]);
    }

    #[test]
    fn a_stored_item_reads_back_as_its_clean_record() {
        let mut record = crate::home_site_record();
        record.title = Some(" Home\u{7}page ".into());
        record.body_text = Some("word ".repeat(400));
        record.gone_at = Some(5);
        record.crawl_attempted_at = Some(6);
        record.crawl_failures = 3;
        record.signals.tranco_rank = Some(10);
        record.redirect = Some(Redirect {
            to: "foo_bar.com".into(),
            at: 7,
            inner: false,
        });
        // A crawl another node shared, as the inbox holds it.
        record.icon = Some("iVBORw0KGgo=".into());
        record.news = vec![crate::Headline {
            title: "Plumb 0.4".into(),
            url: "https://plumbsearch.org/news".into(),
            at: 8,
        }];
        let mut clean = record.clone();
        clean.clean();
        let line = item_line(&record).unwrap();
        assert!(line.starts_with(b"{\"id\":"));
        assert_eq!(read(&line).unwrap(), clean);
        // The whole page text, the crawl marks, and a redirect no item
        // sent elsewhere can hold.
        assert_eq!(clean.body_text.as_ref().unwrap().len(), 1_999);
        assert_eq!(clean.redirect.as_ref().unwrap().to, "foo_bar.com");

        let head = head(&line).unwrap();
        assert_eq!(head.domain, clean.domain);
        assert_eq!(head.signals, clean.signals);
        assert_eq!(head.redirect, clean.redirect);
        assert_eq!(head.gone_at, Some(5));

        let json = record_json(String::from_utf8(line.clone()).unwrap()).unwrap();
        assert_eq!(serde_json::from_str::<SiteRecord>(&json).unwrap(), clean);

        // Inside another stored value, such as a journal's change.
        #[derive(Deserialize)]
        struct Change {
            #[serde(with = "crate::stored")]
            record: SiteRecord,
        }
        for inner in [line, super::line(&clean).unwrap()] {
            let change = format!(r#"{{"record": {}}}"#, String::from_utf8(inner).unwrap());
            let change: Change = serde_json::from_str(&change).unwrap();
            assert_eq!(change.record, clean);
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let mut file = item_line(&record).unwrap();
        file.push(b'\n');
        file.extend_from_slice(&super::line(&clean).unwrap());
        std::fs::write(&path, file).unwrap();
        assert_eq!(read_file(&path).unwrap(), vec![clean.clone(), clean]);
    }

    #[test]
    fn a_stored_line_reads_as_its_stored_item_and_writes_back_as_it_was() {
        let mut full = full_record();
        full.clean();
        for record in [full, SiteRecord::new("example.com")] {
            let line = line(&record).unwrap();
            let site = read_item(&line).unwrap();
            assert_eq!(site, record.to_stored_item());
            assert_eq!(read_item(&item_line(&record).unwrap()).unwrap(), site);
            // Written as the record's JSON, byte for byte, for now.
            let mut written = Vec::new();
            write_item(&mut written, &site).unwrap();
            assert_eq!(written, line);
            assert_eq!(item_as_line(&site).unwrap(), line);
        }
        // A record's line is read as it is, as `read` reads it.
        let mut untidy = SiteRecord::new("example.com");
        untidy.title = Some(" Example\u{7} ".into());
        let site = read_item(&line(&untidy).unwrap()).unwrap();
        assert_eq!(site, untidy.clone().stored_item_as_is());
        assert_eq!(read(&item_as_line(&site).unwrap()).unwrap(), untidy);
        // Only a site's item is a stored site.
        assert!(read_item(br#"{"id": "example.com", "kind": "page"}"#).is_err());
        assert!(read(br#"{"id": "example.com", "kind": "page"}"#).is_err());
        assert!(read_item(b"not json").is_err());
    }

    #[test]
    fn a_stored_item_inside_another_stored_value_is_written_as_its_record() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Change {
            #[serde(with = "crate::stored::item")]
            site: Item,
        }
        #[derive(Serialize)]
        struct RecordChange {
            #[serde(with = "crate::stored")]
            site: SiteRecord,
        }
        let mut record = full_record();
        record.clean();
        let change = Change {
            site: record.to_stored_item(),
        };
        let json = serde_json::to_string(&change).unwrap();
        let record_change = RecordChange {
            site: record.clone(),
        };
        assert_eq!(json, serde_json::to_string(&record_change).unwrap());
        assert_eq!(serde_json::from_str::<Change>(&json).unwrap(), change);
        // A stored item's JSON reads too, as a later version will write it.
        let line = String::from_utf8(item_line(&record).unwrap()).unwrap();
        let json = format!(r#"{{"site": {line}}}"#);
        assert_eq!(serde_json::from_str::<Change>(&json).unwrap(), change);
        let page = r#"{"site": {"id": "example.com", "kind": "page"}}"#;
        assert!(serde_json::from_str::<Change>(page).is_err());
    }
}
