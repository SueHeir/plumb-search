//! Common Crawl's domain ranks: the whole file, or only its header and
//! first rows, cut from the response as it arrives.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use flate2::write::MultiGzDecoder;
use tracing::info;

use super::fetch::{download_to_file, megabytes, part_path, Progress};

/// URL of the domain ranks file for a web graph release name such as
/// `cc-main-2025-26-nov-dec-jan` (names are listed on
/// <https://commoncrawl.org/web-graphs>):
/// `https://data.commoncrawl.org/projects/hyperlinkgraph/<release>/domain/<release>-domain-ranks.txt.gz`.
pub fn cc_domain_ranks_url(release: &str) -> String {
    format!(
        "https://data.commoncrawl.org/projects/hyperlinkgraph/{release}/domain/{release}-domain-ranks.txt.gz"
    )
}

/// Downloads `url` (normally [`cc_domain_ranks_url`]) to `dir/<file name in the URL>`.
///
/// This is the whole file, gigabytes for a recent release; see
/// [`download_cc_domain_ranks_top`] to keep only the best-ranked rows.
pub async fn download_cc_domain_ranks(
    client: &reqwest::Client,
    url: &str,
    dir: &Path,
) -> Result<PathBuf> {
    let dest = dir.join(file_name_from_url(url)?);
    download_to_file(client, url, &dest).await?;
    Ok(dest)
}

/// The last path segment of `url`, e.g. `x-domain-ranks.txt.gz`; an error
/// when the URL is invalid or its path ends in `/`.
pub fn file_name_from_url(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url).with_context(|| format!("invalid URL {url:?}"))?;
    let name = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("");
    if name.is_empty() || name == "." || name == ".." || name.contains('\\') {
        bail!("cannot tell a file name from the URL {url:?}");
    }
    Ok(name.to_string())
}

/// File name [`download_cc_domain_ranks_top`] saves to: the file name in
/// `url` without `.gz` and `.txt`, plus `-top<rows>.txt`, e.g.
/// `cc-main-2025-26-nov-dec-jan-domain-ranks-top1000000.txt`.
pub fn cc_domain_ranks_top_file_name(url: &str, rows: usize) -> Result<String> {
    let name = file_name_from_url(url)?;
    let stem = name.strip_suffix(".gz").unwrap_or(&name);
    let stem = stem.strip_suffix(".txt").unwrap_or(stem);
    Ok(format!("{stem}-top{rows}.txt"))
}

/// Downloads only the top of a domain ranks file (normally
/// [`cc_domain_ranks_url`]) to `dir/<`[`cc_domain_ranks_top_file_name`]`>`
/// and returns that path.
///
/// A ranks file lists every domain in the web graph, best harmonic
/// centrality first, and is gigabytes long. This streams the response
/// through a gzip decoder, writes the header line and the first `rows` rows
/// after it as plain text, then drops the connection, so only the start of
/// the file is ever downloaded. A response that is not gzipped is copied as
/// it is. Blank lines are copied but not counted. A file with fewer rows is
/// saved whole once its gzip checksum checks out; one that ends early, or
/// holds no line at all, is an error.
///
/// The text goes to a `.part` file that is renamed when complete, so the
/// destination is either whole or absent, as with [`download_to_file`].
pub async fn download_cc_domain_ranks_top(
    client: &reqwest::Client,
    url: &str,
    dir: &Path,
    rows: usize,
) -> Result<PathBuf> {
    let dest = dir.join(cc_domain_ranks_top_file_name(url, rows)?);
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("downloading {url} failed: HTTP {status}");
    }
    crate::storage::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let part = part_path(&dest);
    let total = response.content_length();
    info!(
        "saving the header and the first {rows} rows of {url} ({}) to {}",
        total.map_or_else(|| "size unknown".to_string(), megabytes),
        dest.display()
    );
    let saved = match save_top_lines(&mut response, &part, rows).await {
        Ok(saved) => saved,
        Err(err) => {
            let _ = crate::storage::remove_file(&part).await;
            return Err(err.context(format!("downloading {url}")));
        }
    };
    // Dropping the response closes the connection: the rest is never fetched.
    drop(response);
    crate::storage::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!(
        "saved {} rows to {} after downloading {}",
        saved.rows,
        dest.display(),
        megabytes(saved.received)
    );
    Ok(dest)
}

/// What [`save_top_lines`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TopLinesSaved {
    /// Rows after the header line.
    rows: usize,
    /// Bytes of the response body read, as sent (compressed).
    received: u64,
}

/// Reads the response until [`TopLines`] has the rows it wants (or the body
/// ends) and writes the kept text to `part`.
async fn save_top_lines(
    response: &mut reqwest::Response,
    part: &Path,
    rows: usize,
) -> Result<TopLinesSaved> {
    let mut file = crate::storage::OutputFile::create(part)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    let mut top = TopLines::new(rows);
    let mut progress = Progress::new(u64::try_from(rows).ok());
    let mut received = 0u64;
    while !top.is_done() {
        let Some(chunk) = response.chunk().await.context("reading the response")? else {
            top.finish().context("decompressing the response")?;
            break;
        };
        received += chunk.len() as u64;
        top.write(&chunk).context("decompressing the response")?;
        file.write_all(&top.take_output())
            .await
            .with_context(|| format!("writing {}", part.display()))?;
        let kept = top.rows() as u64;
        if !top.is_done() && progress.should_report(kept) {
            info!("{}: {kept} of {rows} rows", part.display());
        }
    }
    file.write_all(&top.take_output())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    if !top.has_header() {
        bail!("the response holds no lines");
    }
    file.flush()
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    file.sync_all()
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    Ok(TopLinesSaved {
        rows: top.rows(),
        received,
    })
}

/// Turns the body of a ranks file, fed in pieces as it arrives, into the
/// text to keep: gunzipped (when the body starts with the gzip magic bytes)
/// and cut after the header line and `rows` rows.
#[derive(Debug)]
struct TopLines {
    rows: usize,
    decoder: Decoder,
}

#[derive(Debug)]
enum Decoder {
    /// Fewer than two bytes so far, so it is not known yet whether the body
    /// is gzipped.
    Sniffing(Vec<u8>),
    /// Gzip, with any number of members (Common Crawl writes several).
    Gzip(Box<MultiGzDecoder<LinePrefix>>),
    Plain(LinePrefix),
}

impl TopLines {
    fn new(rows: usize) -> Self {
        TopLines {
            rows,
            decoder: Decoder::Sniffing(Vec::new()),
        }
    }

    /// Decodes the next piece of the body. Once [`TopLines::is_done`],
    /// more input is accepted and ignored.
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        if let Decoder::Sniffing(seen) = &mut self.decoder {
            seen.extend_from_slice(data);
            if seen.len() < 2 {
                return Ok(());
            }
            let seen = std::mem::take(seen);
            self.decide(&seen);
            return self.write(&seen);
        }
        match &mut self.decoder {
            Decoder::Gzip(gz) => gz.write_all(data),
            Decoder::Plain(lines) => {
                lines.push(data);
                Ok(())
            }
            Decoder::Sniffing(_) => unreachable!("decided above"),
        }
    }

    /// Picks the decoder from the first bytes of the body.
    fn decide(&mut self, first: &[u8]) {
        let lines = LinePrefix::new(self.rows);
        self.decoder = if first.starts_with(&[0x1f, 0x8b]) {
            Decoder::Gzip(Box::new(MultiGzDecoder::new(lines)))
        } else {
            Decoder::Plain(lines)
        };
    }

    /// Ends the input: decodes what the gzip decoder still holds and checks
    /// the checksum of the last member, so a body cut short is an error.
    fn finish(&mut self) -> io::Result<()> {
        if let Decoder::Sniffing(seen) = &mut self.decoder {
            // Zero or one byte in all: too short to be gzip.
            let seen = std::mem::take(seen);
            self.decide(&[]);
            self.write(&seen)?;
        }
        match &mut self.decoder {
            Decoder::Gzip(gz) => {
                gz.try_finish()?;
                gz.get_mut().end();
            }
            Decoder::Plain(lines) => lines.end(),
            Decoder::Sniffing(_) => unreachable!("decided above"),
        }
        Ok(())
    }

    fn lines(&self) -> Option<&LinePrefix> {
        match &self.decoder {
            Decoder::Sniffing(_) => None,
            Decoder::Gzip(gz) => Some(gz.get_ref()),
            Decoder::Plain(lines) => Some(lines),
        }
    }

    /// The text kept since the last call.
    fn take_output(&mut self) -> Vec<u8> {
        let lines = match &mut self.decoder {
            Decoder::Sniffing(_) => return Vec::new(),
            Decoder::Gzip(gz) => gz.get_mut(),
            Decoder::Plain(lines) => lines,
        };
        std::mem::take(&mut lines.out)
    }

    /// True once the header and all the rows wanted are in.
    fn is_done(&self) -> bool {
        self.lines().is_some_and(|lines| lines.done)
    }

    /// Rows kept so far, after the header.
    fn rows(&self) -> usize {
        self.lines().map_or(0, |lines| lines.rows)
    }

    fn has_header(&self) -> bool {
        self.lines().is_some_and(|lines| lines.header_seen)
    }
}

/// Keeps the first non-blank line (the header) and the `wanted` non-blank
/// lines after it, of the text pushed in, and drops everything after them.
#[derive(Debug)]
struct LinePrefix {
    wanted: usize,
    rows: usize,
    header_seen: bool,
    /// The line being read has a character other than whitespace.
    line_has_text: bool,
    done: bool,
    /// Text kept and not yet taken.
    out: Vec<u8>,
}

impl LinePrefix {
    fn new(wanted: usize) -> Self {
        LinePrefix {
            wanted,
            rows: 0,
            header_seen: false,
            line_has_text: false,
            done: false,
            out: Vec::new(),
        }
    }

    fn push(&mut self, mut text: &[u8]) {
        while !self.done && !text.is_empty() {
            let (line, rest, ends) = match text.iter().position(|&b| b == b'\n') {
                Some(i) => (&text[..=i], &text[i + 1..], true),
                None => (text, &[][..], false),
            };
            self.out.extend_from_slice(line);
            self.line_has_text |= line.iter().any(|b| !b.is_ascii_whitespace());
            if ends {
                self.end_line();
            }
            text = rest;
        }
    }

    /// The end of the input: a last line without a line break counts too.
    fn end(&mut self) {
        if !self.done {
            self.end_line();
        }
    }

    fn end_line(&mut self) {
        if std::mem::take(&mut self.line_has_text) {
            if self.header_seen {
                self.rows += 1;
            } else {
                self.header_seen = true;
            }
            self.done = self.rows >= self.wanted;
        }
    }
}

impl Write for LinePrefix {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.push(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
