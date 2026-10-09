//! Common Crawl WET files: the plain text of each fetched page, one
//! `WARC-Type: conversion` record per page with its address in
//! `WARC-Target-URI` and its visible text, a paragraph a line, as the
//! body. Like a WAT file ([`crate::wat`]), it is a WARC file gzipped a
//! record per member.

use std::path::Path;

use anyhow::{Context, Result};

use crate::wat::WarcReader;

/// What [`for_each_wet_page`] read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WetStats {
    pub records: u64,
    pub pages: u64,
    /// Records too big to keep or not text.
    pub skipped: u64,
}

/// Calls `each` with the address and text of every page of the WET file
/// at `path`, gzipped or not.
pub fn for_each_wet_page(path: &Path, mut each: impl FnMut(&str, &str)) -> Result<WetStats> {
    let mut reader = WarcReader::new(crate::open_maybe_gz(path)?);
    let mut stats = WetStats::default();
    while let Some(record) = reader
        .next_record()
        .with_context(|| format!("reading WET file {}", path.display()))?
    {
        stats.records += 1;
        let conversion = record
            .header("WARC-Type")
            .is_some_and(|t| t.eq_ignore_ascii_case("conversion"));
        if !conversion {
            continue;
        }
        let Some(url) = record.header("WARC-Target-URI") else {
            stats.skipped += 1;
            continue;
        };
        if record.oversized {
            stats.skipped += 1;
            continue;
        }
        let text = String::from_utf8_lossy(&record.body);
        stats.pages += 1;
        each(url, &text);
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn record(kind: &str, url: Option<&str>, body: &str) -> String {
        let mut out = format!("WARC/1.0\r\nWARC-Type: {kind}\r\n");
        if let Some(url) = url {
            out.push_str(&format!("WARC-Target-URI: {url}\r\n"));
        }
        out.push_str(&format!(
            "Content-Length: {}\r\n\r\n{body}\r\n\r\n",
            body.len()
        ));
        out
    }

    #[test]
    fn reads_conversion_records_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.warc.wet.gz");
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        for r in [
            record("warcinfo", None, "software: test"),
            record(
                "conversion",
                Some("https://example.com/a"),
                "Line one.\nLine two.",
            ),
            record("conversion", None, "no address"),
            record("conversion", Some("https://example.org/"), "Héllo"),
        ] {
            gz.write_all(r.as_bytes()).unwrap();
        }
        gz.finish().unwrap();
        let mut pages = Vec::new();
        let stats = for_each_wet_page(&path, |url, text| {
            pages.push((url.to_string(), text.to_string()))
        })
        .unwrap();
        assert_eq!(
            pages,
            vec![
                (
                    "https://example.com/a".to_string(),
                    "Line one.\nLine two.".to_string()
                ),
                ("https://example.org/".to_string(), "Héllo".to_string()),
            ]
        );
        assert_eq!(
            stats,
            WetStats {
                records: 4,
                pages: 2,
                skipped: 1
            }
        );
    }
}
