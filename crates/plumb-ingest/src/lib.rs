//! Loads the public datasets that seed a Plumb Search index and folds them
//! into [`plumb_core::SiteRecord`]s.
//!
//! Seed sources (used once to bootstrap; the network grows from its own crawls after that):
//! - Tranco top-1M list ([`tranco`])
//! - Common Crawl domain-level web graph ranks ([`ccranks`])
//! - Common Crawl WAT files: homepage titles, descriptions and inbound link text ([`wat`])
//! - Wikidata "official website" (P856) statements ([`wikidata`])

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result};
use flate2::read::MultiGzDecoder;

pub mod builder;
pub mod ccranks;
pub mod download;
pub mod tranco;
pub mod wat;
pub mod wikidata;

pub use builder::Builder;
pub use ccranks::{load_cc_domain_ranks, CcRank};
pub use tranco::{load_tranco, TrancoEntry};
pub use wat::{
    parse_wat, HomepageMeta, WarcReader, WarcRecord, WatExtract, WatPage, WatStats, WatWriter,
};
pub use wikidata::{load_wikidata_official_sites, OfficialSite};

/// Opens a file for buffered reading, gunzipping it when it starts with the
/// gzip magic bytes. Multi-member gzip files (one member per record, as
/// Common Crawl writes them) are read to the end.
pub fn open_maybe_gz(path: &Path) -> Result<Box<dyn BufRead>> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut magic = [0u8; 2];
    let n =
        read_up_to(&mut file, &mut magic).with_context(|| format!("reading {}", path.display()))?;
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    if n == 2 && magic == [0x1f, 0x8b] {
        Ok(Box::new(BufReader::with_capacity(
            1 << 16,
            MultiGzDecoder::new(BufReader::new(file)),
        )))
    } else {
        Ok(Box::new(BufReader::with_capacity(1 << 16, file)))
    }
}

fn read_up_to(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Reads text lines into one reused buffer, for the line-oriented seed files.
/// Lines come back without their `\n` or `\r\n`, and invalid UTF-8 is replaced
/// rather than failing the whole file.
pub(crate) struct LineReader<R> {
    reader: R,
    buf: Vec<u8>,
    line_no: u64,
}

impl<R: BufRead> LineReader<R> {
    pub(crate) fn new(reader: R) -> Self {
        LineReader {
            reader,
            buf: Vec::new(),
            line_no: 0,
        }
    }

    /// The next line and its 1-based line number, or `None` at the end of the input.
    pub(crate) fn next_line(&mut self) -> std::io::Result<Option<(u64, Cow<'_, str>)>> {
        self.buf.clear();
        if self.reader.read_until(b'\n', &mut self.buf)? == 0 {
            return Ok(None);
        }
        self.line_no += 1;
        let mut line = self.buf.as_slice();
        if let Some(rest) = line.strip_suffix(b"\n") {
            line = rest;
        }
        if let Some(rest) = line.strip_suffix(b"\r") {
            line = rest;
        }
        if self.line_no == 1 {
            line = line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line);
        }
        Ok(Some((self.line_no, String::from_utf8_lossy(line))))
    }
}

/// A short, printable excerpt of a line for error messages.
pub(crate) fn snippet(text: &str) -> String {
    let text = plumb_core::truncate_chars(text, 80);
    format!("{text:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_reader_handles_crlf_bom_and_bad_utf8() {
        let data: &[u8] = b"\xEF\xBB\xBFfirst\r\nsecond\n\nbad \xFF byte\r\nlast";
        let mut lines = LineReader::new(data);
        let mut got = Vec::new();
        while let Some((line_no, line)) = lines.next_line().unwrap() {
            got.push((line_no, line.into_owned()));
        }
        assert_eq!(
            got,
            vec![
                (1, "first".to_string()),
                (2, "second".to_string()),
                (3, String::new()),
                (4, "bad \u{FFFD} byte".to_string()),
                (5, "last".to_string()),
            ]
        );
    }

    #[test]
    fn open_maybe_gz_reads_plain_and_gzip() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.txt");
        std::fs::write(&plain, "hello\n").unwrap();
        let gz = dir.path().join("two-members.gz");
        let mut bytes = Vec::new();
        for part in ["hello ", "world\n"] {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            enc.write_all(part.as_bytes()).unwrap();
            bytes.extend(enc.finish().unwrap());
        }
        std::fs::write(&gz, bytes).unwrap();
        let mut text = String::new();
        open_maybe_gz(&plain)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello\n");
        text.clear();
        open_maybe_gz(&gz)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "hello world\n");
    }
}
