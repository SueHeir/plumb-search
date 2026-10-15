//! A site as a node keeps it on disk: a line of its records file, its
//! journal, its inbox and its bucket table. Everything that reads or
//! writes those goes through here, so that their shape can change in one
//! place. Today a line is the [`SiteRecord`]'s own JSON; site items of the
//! data format ([`SiteRecord::to_item`]) are to take its place.
//!
//! What a node sends is not stored and does not come through here: crawl
//! batches and bucket answers carry items ([`SiteRecord::to_line`]), and
//! private search hands a browser trimmed records. Only `/plumb/fill/1`
//! sends what it stores, as the record's own JSON ([`record_json`]).
//!
//! A record inside another stored value, such as a change in the journal,
//! is written and read the same way with
//! `#[serde(with = "plumb_core::stored")]`.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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

/// The site a stored line holds.
pub fn read(line: &[u8]) -> Result<SiteRecord> {
    Ok(serde_json::from_slice(line)?)
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
/// with. Today the line is that JSON already.
pub fn record_json(line: String) -> Result<String> {
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

/// The [`Head`] of a stored line.
pub fn head(line: &[u8]) -> Result<Head> {
    Ok(serde_json::from_slice(line)?)
}

/// Writes a record inside another stored value; see the module doc.
pub fn serialize<S: Serializer>(record: &SiteRecord, serializer: S) -> Result<S::Ok, S::Error> {
    record.serialize(serializer)
}

/// Reads a record inside another stored value; see the module doc.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SiteRecord, D::Error> {
    SiteRecord::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
