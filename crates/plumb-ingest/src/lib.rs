//! Loads the public datasets that seed a Plumb Search index and folds them
//! into [`plumb_core::SiteRecord`]s.
//!
//! Seed sources (used once to bootstrap; the network grows from its own crawls after that):
//! - Tranco top-1M list ([`tranco`])
//! - Common Crawl domain-level web graph ranks ([`ccranks`])
//! - Common Crawl WAT files: homepage titles, descriptions and inbound link text ([`wat`])
//! - Wikidata "official website" (P856) statements ([`wikidata`]), and the
//!   country and kind of the organizations behind them ([`facts`])
//! - The first sentences of the best-known ones' English Wikipedia
//!   articles ([`intros`])

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result};
use flate2::read::MultiGzDecoder;

pub mod articles;
pub mod builder;
pub mod ccranks;
pub mod core_ac;
pub mod docs;
pub mod download;
pub mod facts;
pub mod films;
pub mod github;
pub mod intros;
pub mod item_facts;
pub mod kind_sites;
pub mod musicbrainz;
pub mod openalex;
pub mod openlibrary;
pub mod osm;
pub mod packages;
pub mod paper_names;
pub mod podcasts;
pub mod profiles;
pub mod stackexchange;
pub mod tranco;
pub mod wat;
pub mod wet;
pub mod wikidata;

pub use builder::Builder;
pub use ccranks::{load_cc_domain_ranks, CcRank, DEFAULT_CC_RANKS_LIMIT};
pub use facts::{attach_facts, load_site_facts, FactsByItem, SiteFacts};
pub use intros::{attach_intros, load_intros, IntrosByItem};
pub use tranco::{load_tranco, TrancoEntry};
pub use wat::{
    parse_wat, HomepageMeta, WarcReader, WarcRecord, WatExtract, WatPage, WatStats, WatWriter,
};
pub use wikidata::{load_misread_official_sites, load_wikidata_official_sites, OfficialSite};

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

/// Longest line the seed file readers accept, in bytes, not counting its
/// `\n`. A longer line is skipped without being held in memory, and the
/// loaders count it as a malformed row.
pub(crate) const MAX_LINE_BYTES: usize = 1 << 20;

/// One line from a [`LineReader`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Line<'a> {
    /// The line's text, without its line ending.
    Text(Cow<'a, str>),
    /// A line longer than [`MAX_LINE_BYTES`], skipped unread.
    TooLong,
}

/// How a skipped over-long line shows up in warnings and errors.
pub(crate) fn too_long_note() -> String {
    format!("a line over {MAX_LINE_BYTES} bytes, skipped")
}

/// Reads text lines into one reused buffer, for the line-oriented seed files.
/// Lines come back without their `\n` or `\r\n`, and invalid UTF-8 is replaced
/// rather than failing the whole file. Lines over [`MAX_LINE_BYTES`] come
/// back as [`Line::TooLong`], so a file without line breaks cannot make the
/// reader buffer all of it.
pub(crate) struct LineReader<R> {
    reader: R,
    buf: Vec<u8>,
    line_no: u64,
    /// The last line returned was [`Line::TooLong`] and the rest of it is
    /// still unread. It is skipped on the next call, so a caller that gives
    /// up on an over-long line does not wait for the rest of it.
    in_long_line: bool,
}

impl<R: BufRead> LineReader<R> {
    pub(crate) fn new(reader: R) -> Self {
        LineReader {
            reader,
            buf: Vec::new(),
            line_no: 0,
            in_long_line: false,
        }
    }

    /// The next line and its 1-based line number, or `None` at the end of the input.
    pub(crate) fn next_line(&mut self) -> std::io::Result<Option<(u64, Line<'_>)>> {
        if self.in_long_line {
            skip_rest_of_line(&mut self.reader)?;
            self.in_long_line = false;
        }
        self.buf.clear();
        let n = (&mut self.reader)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut self.buf)?;
        if n == 0 {
            return Ok(None);
        }
        self.line_no += 1;
        if n > MAX_LINE_BYTES && self.buf.last() != Some(&b'\n') {
            self.in_long_line = true;
            return Ok(Some((self.line_no, Line::TooLong)));
        }
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
        Ok(Some((
            self.line_no,
            Line::Text(String::from_utf8_lossy(line)),
        )))
    }
}

/// Consumes input up to and including the next `\n` (or to the end), a
/// buffer at a time, keeping none of it.
fn skip_rest_of_line<B: BufRead + ?Sized>(reader: &mut B) -> std::io::Result<()> {
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if available.is_empty() {
            return Ok(());
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                reader.consume(i + 1);
                return Ok(());
            }
            None => {
                let n = available.len();
                reader.consume(n);
            }
        }
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

    /// Every line as `Some(text)`, or `None` for a line skipped as too long.
    fn all_lines(reader: impl BufRead) -> Vec<(u64, Option<String>)> {
        let mut lines = LineReader::new(reader);
        let mut got = Vec::new();
        while let Some((line_no, line)) = lines.next_line().unwrap() {
            let text = match line {
                Line::Text(text) => Some(text.into_owned()),
                Line::TooLong => None,
            };
            got.push((line_no, text));
        }
        got
    }

    #[test]
    fn line_reader_handles_crlf_bom_and_bad_utf8() {
        let data: &[u8] = b"\xEF\xBB\xBFfirst\r\nsecond\n\nbad \xFF byte\r\nlast";
        assert_eq!(
            all_lines(data),
            vec![
                (1, Some("first".to_string())),
                (2, Some("second".to_string())),
                (3, Some(String::new())),
                (4, Some("bad \u{FFFD} byte".to_string())),
                (5, Some("last".to_string())),
            ]
        );
    }

    #[test]
    fn line_reader_skips_over_long_lines_without_buffering_them() {
        let cap = MAX_LINE_BYTES as u64;
        // A 3 MiB line between two short ones, streamed rather than built in memory.
        let data = (&b"first\n"[..])
            .chain(std::io::repeat(b'a').take(3 * cap))
            .chain(&b"\nlast\n"[..]);
        let mut lines = LineReader::new(BufReader::new(data));
        assert_eq!(
            lines.next_line().unwrap(),
            Some((1, Line::Text("first".into())))
        );
        assert_eq!(lines.next_line().unwrap(), Some((2, Line::TooLong)));
        assert_eq!(
            lines.next_line().unwrap(),
            Some((3, Line::Text("last".into())))
        );
        assert_eq!(lines.next_line().unwrap(), None);
        assert!(lines.buf.capacity() <= 2 * (MAX_LINE_BYTES + 1));

        // An over-long last line without a line break.
        let unterminated = (&b"first\n"[..]).chain(std::io::repeat(b'b').take(2 * cap));
        assert_eq!(
            all_lines(BufReader::new(unterminated)),
            vec![(1, Some("first".to_string())), (2, None)]
        );

        // A line right at the cap is kept and one byte more is not, whether
        // a line break follows or the input ends there.
        for (extra, ending) in [(0, &b"\nnext"[..]), (0, b""), (1, b"\nnext"), (1, b"")] {
            let line = std::io::repeat(b'c').take(cap + extra).chain(ending);
            let lengths: Vec<Option<usize>> = all_lines(BufReader::new(line))
                .iter()
                .map(|(_, text)| text.as_ref().map(String::len))
                .collect();
            let kept = (extra == 0).then_some(MAX_LINE_BYTES);
            let mut expected = vec![kept];
            if !ending.is_empty() {
                expected.push(Some(4));
            }
            assert_eq!(lengths, expected, "extra={extra} ending={ending:?}");
        }
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
