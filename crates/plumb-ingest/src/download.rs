//! Downloads the seed datasets. These hosts must be reachable from the
//! machine running `plumb fetch-data` (or a `plumb run` node on first start).

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use flate2::write::MultiGzDecoder;
use reqwest::header::{
    HeaderMap, ACCEPT, ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_ENCODING, CONTENT_RANGE,
    CONTENT_TYPE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE, RETRY_AFTER,
};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::error::Category;
#[cfg(test)]
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::snippet;
use crate::wikidata::bare_item_id;

/// Sent with every request, so site owners and dataset hosts can see who is fetching.
pub const USER_AGENT: &str = concat!(
    "PlumbSearch/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/SueHeir/plumb-search)"
);

/// The latest Tranco list, as a zip holding `top-1m.csv`.
pub const TRANCO_LATEST_URL: &str = "https://tranco-list.eu/top-1m.csv.zip";

/// Wikidata's public SPARQL endpoint.
pub const WIKIDATA_SPARQL_URL: &str = "https://query.wikidata.org/sparql";

/// QLever's copy of Wikidata (<https://qlever.dev>), a SPARQL endpoint
/// that lists every item with an official website in seconds and has no
/// 60-second limit. It is rebuilt from Wikidata's dumps, so it can be a
/// few days behind. The official websites and their facts are asked of it
/// first, and of [`WIKIDATA_SPARQL_URL`] when it fails.
pub const QLEVER_WIKIDATA_URL: &str = "https://qlever.dev/api/wikidata";

/// The prefixes Wikidata's query service declares by itself, declared for
/// endpoints such as [`QLEVER_WIKIDATA_URL`] that need them.
pub const WIKIDATA_PREFIXES: &str = "PREFIX wd: <http://www.wikidata.org/entity/> \
     PREFIX wdt: <http://www.wikidata.org/prop/direct/> \
     PREFIX wikibase: <http://wikiba.se/ontology#> \
     PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#> \
     PREFIX skos: <http://www.w3.org/2004/02/skos/core#> \
     PREFIX schema: <http://schema.org/> \
     PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> ";

/// File name [`download_tranco`] saves to.
pub const TRANCO_FILE_NAME: &str = "tranco-top-1m.csv.zip";

/// File name [`download_wikidata_official_sites`] saves to.
pub const WIKIDATA_FILE_NAME: &str = "wikidata-official-sites.tsv";

/// Progress is logged about this often when the download size is unknown.
const PROGRESS_BYTES_UNKNOWN_LENGTH: u64 = 64 << 20;

/// A client with [`USER_AGENT`], gzip off (we store files as served) and generous timeouts.
///
/// Connecting may take 30 seconds and each read 120 seconds; there is no
/// overall timeout, since dataset files can be gigabytes. Every response
/// decompression is turned off, even when another crate in the build enables
/// reqwest's `gzip` feature.
pub fn http_client() -> Result<reqwest::Client> {
    client_builder().build().context("building the HTTP client")
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
}

/// Times a download that broke off is carried on within one call to
/// [`download_to_file`].
const MAX_RESUMES: u32 = 5;
/// Pause before carrying on a download that broke off.
const RESUME_PAUSE: Duration = Duration::from_secs(1);

/// Streams `url` into `dest` (via a `.part` file renamed when complete),
/// logging progress. Fails on non-2xx responses. Returns bytes written.
///
/// Creates `dest`'s parent directory and replaces an existing `dest`. When
/// the server takes byte ranges and names the file's version (a strong
/// `ETag`, or `Last-Modified`), a download that breaks off is carried on
/// from where it stopped: up to [`MAX_RESUMES`] times in this call, and
/// from the `.part` file it leaves (with a `.part.json` beside it naming
/// the version) by a later call for the same URL, unless the file has
/// changed since. Otherwise the `.part` file is removed when the download
/// fails.
pub async fn download_to_file(client: &reqwest::Client, url: &str, dest: &Path) -> Result<u64> {
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        crate::storage::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let part = part_path(dest);
    let info = part_info_path(dest);
    let mut resumes = 0;
    let written = loop {
        let from = PartInfo::resumable(&part, &info, url);
        match download_attempt(client, url, dest, &part, &info, from).await {
            Ok(written) => break written,
            Err(failed) if failed.resumable && resumes < MAX_RESUMES => {
                resumes += 1;
                warn!("downloading {url}: {:#}; carrying on", failed.error);
                tokio::time::sleep(RESUME_PAUSE).await;
            }
            Err(failed) => {
                if !failed.resumable {
                    let _ = crate::storage::remove_file(&part).await;
                    let _ = crate::storage::remove_file(&info).await;
                }
                return Err(failed.error.context(format!("downloading {url}")));
            }
        }
    };
    let _ = crate::storage::remove_file(&info).await;
    crate::storage::rename(&part, dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!("saved {} ({})", dest.display(), megabytes(written));
    Ok(written)
}

/// Why one try at a download failed, and whether its `.part` file can be
/// carried on from.
struct AttemptFailed {
    error: anyhow::Error,
    resumable: bool,
}

impl AttemptFailed {
    fn new(error: anyhow::Error, resumable: bool) -> Self {
        AttemptFailed { error, resumable }
    }
}

/// What is saved beside a `.part` file that can be carried on from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
struct PartInfo {
    url: String,
    /// The file's version: a strong `ETag`, else its `Last-Modified`. Sent
    /// as `If-Range`, so a changed file comes whole.
    version: String,
    /// The whole file's size, when the server said.
    total: Option<u64>,
}

impl PartInfo {
    /// The bytes in `part` and what `info` says of them, when they are of
    /// `url` and can be carried on from.
    fn resumable(part: &Path, info: &Path, url: &str) -> Option<(u64, PartInfo)> {
        let saved: PartInfo = serde_json::from_slice(&std::fs::read(info).ok()?).ok()?;
        let have = std::fs::metadata(part).ok()?.len();
        let fits = saved.total.is_none_or(|total| have < total);
        (saved.url == url && have > 0 && fits).then_some((have, saved))
    }

    /// The version of the file a response holds, when the server takes
    /// byte ranges of it.
    fn of(url: &str, response: &reqwest::Response) -> Option<PartInfo> {
        let headers = response.headers();
        let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
        if !header(ACCEPT_RANGES).is_some_and(|v| v.trim().eq_ignore_ascii_case("bytes")) {
            return None;
        }
        let version = header(ETAG)
            .filter(|etag| !etag.starts_with("W/"))
            .or_else(|| header(LAST_MODIFIED))?;
        Some(PartInfo {
            url: url.to_string(),
            version: version.to_string(),
            total: response.content_length(),
        })
    }
}

/// `dest` with `.part.json` appended to its file name.
fn part_info_path(dest: &Path) -> PathBuf {
    let mut info = OsString::from(dest.as_os_str());
    info.push(".part.json");
    PathBuf::from(info)
}

/// The first byte and whole length in a `Content-Range` of
/// `bytes <first>-<last>/<length or *>`.
fn content_range(response: &reqwest::Response) -> Option<(u64, Option<u64>)> {
    let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let (range, length) = value.strip_prefix("bytes ")?.split_once('/')?;
    let first = range.split_once('-')?.0.trim().parse().ok()?;
    Some((first, length.trim().parse().ok()))
}

/// One request for `url` into `part`, carrying on `from` the bytes already
/// there when the server agrees. Returns the file's whole size.
async fn download_attempt(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    part: &Path,
    info_path: &Path,
    from: Option<(u64, PartInfo)>,
) -> Result<u64, AttemptFailed> {
    let mut request = client.get(url);
    if let Some((have, info)) = &from {
        request = request
            .header(RANGE, format!("bytes={have}-"))
            .header(IF_RANGE, info.version.as_str());
    }
    // A part file kept from before is kept through a request that fails.
    let kept = from.is_some();
    let mut response = request
        .send()
        .await
        .with_context(|| format!("requesting {url}"))
        .map_err(|err| AttemptFailed::new(err, kept))?;
    let status = response.status();
    if !status.is_success() {
        let transient = status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS;
        let err = anyhow!("downloading {url} failed: HTTP {status}");
        return Err(AttemptFailed::new(err, kept && transient));
    }
    let carried = match (&from, status) {
        (Some((have, info)), StatusCode::PARTIAL_CONTENT) => match content_range(&response) {
            Some((first, total)) if first == *have => Some((*have, total.or(info.total))),
            _ => {
                let err = anyhow!("the server sent another range than the one asked for");
                return Err(AttemptFailed::new(err, false));
            }
        },
        (None, StatusCode::PARTIAL_CONTENT) => {
            let err = anyhow!("the server sent part of the file when asked for all of it");
            return Err(AttemptFailed::new(err, false));
        }
        _ => None,
    };
    let (start, total) = match carried {
        Some((have, total)) => {
            info!(
                "carrying on with {url} from {} ({})",
                megabytes(have),
                total.map_or_else(|| "size unknown".to_string(), megabytes)
            );
            (have, total)
        }
        None => {
            // The whole file: a fresh start, or a file that has changed.
            let total = response.content_length();
            info!(
                "downloading {url} to {} ({})",
                dest.display(),
                total.map_or_else(|| "size unknown".to_string(), megabytes)
            );
            let saved = match PartInfo::of(url, &response) {
                Some(info) => {
                    let bytes = serde_json::to_vec(&info).map_err(anyhow::Error::from);
                    match bytes {
                        Ok(bytes) => crate::storage::write(info_path, bytes).await.is_ok(),
                        Err(_) => false,
                    }
                }
                None => false,
            };
            if !saved {
                let _ = crate::storage::remove_file(info_path).await;
            }
            (0, total)
        }
    };
    let resumable = tokio::fs::try_exists(info_path).await.unwrap_or(false);
    stream_body(&mut response, part, start, total)
        .await
        .map_err(|err| AttemptFailed::new(err, resumable))
}

/// Writes the response body to `part` after its first `start` bytes,
/// returning the file's length then.
async fn stream_body(
    response: &mut reqwest::Response,
    part: &Path,
    start: u64,
    total: Option<u64>,
) -> Result<u64> {
    let mut file = crate::storage::OutputFile::open(part, start == 0)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    if start > 0 {
        file.seek(io::SeekFrom::Start(start))
            .await
            .with_context(|| format!("writing {}", part.display()))?;
    }
    let mut progress = Progress::new(total);
    let mut written = start;
    while let Some(chunk) = response.chunk().await.context("reading the response")? {
        file.write_all(&chunk)
            .await
            .with_context(|| format!("writing {}", part.display()))?;
        written += chunk.len() as u64;
        if progress.should_report(written) {
            match total {
                Some(total) => info!(
                    "{}: {}% ({} of {})",
                    part.display(),
                    written * 100 / total.max(1),
                    megabytes(written),
                    megabytes(total)
                ),
                None => info!("{}: {}", part.display(), megabytes(written)),
            }
        }
    }
    file.flush()
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    file.sync_all()
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    if let Some(total) = total {
        if written != total {
            bail!("expected {total} bytes but received {written}");
        }
    }
    Ok(written)
}

/// `dest` with `.part` appended to its file name.
pub(crate) fn part_path(dest: &Path) -> PathBuf {
    let mut part = OsString::from(dest.as_os_str());
    part.push(".part");
    PathBuf::from(part)
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// Decides when to log download progress: about every 5% of a known
/// length, or every 64 MB when the length is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Progress {
    step: u64,
    next: u64,
}

impl Progress {
    fn new(total: Option<u64>) -> Self {
        let step = match total {
            Some(total) if total > 0 => (total / 20).max(1),
            _ => PROGRESS_BYTES_UNKNOWN_LENGTH,
        };
        Progress { step, next: step }
    }

    /// True when `written` bytes reach the next reporting point.
    fn should_report(&mut self, written: u64) -> bool {
        if written < self.next {
            return false;
        }
        self.next = (written / self.step + 1) * self.step;
        true
    }
}

/// Downloads the latest Tranco list to `dir/tranco-top-1m.csv.zip` and returns that path.
pub async fn download_tranco(client: &reqwest::Client, dir: &Path) -> Result<PathBuf> {
    let dest = dir.join(TRANCO_FILE_NAME);
    download_to_file(client, TRANCO_LATEST_URL, &dest).await?;
    Ok(dest)
}

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

/// Asks Wikidata's SPARQL endpoint for items with an official website (P856)
/// and at least `min_sitelinks` sitelinks (a notability filter), and writes
/// `dir/wikidata-official-sites.tsv` with the header `item\tlabel\twebsite`.
///
/// Wikidata's public query service stops every query after 60 seconds, and
/// listing all these items takes longer than that. So they are asked for in
/// bands of sitelink counts ([`wikidata_sitelink_bands`]), one query after
/// another. Each band takes 20 to 50 seconds even when small, since most of
/// the work is the same scan of every item with an official website, so
/// whether it ends within the limit depends on how busy the service is: a
/// band that is stopped is asked for again after a wait. See
/// [`download_wikidata_official_sites_paced`]. Each band is saved as it comes
/// in, so a later call asks only for the bands still missing.
pub async fn download_wikidata_official_sites(
    client: &reqwest::Client,
    dir: &Path,
    min_sitelinks: u32,
) -> Result<PathBuf> {
    download_wikidata_official_sites_from(client, WIKIDATA_SPARQL_URL, dir, min_sitelinks).await
}

/// [`download_wikidata_official_sites`], asking the SPARQL endpoint at
/// `endpoint` instead of [`WIKIDATA_SPARQL_URL`], such as a mirror.
pub async fn download_wikidata_official_sites_from(
    client: &reqwest::Client,
    endpoint: &str,
    dir: &Path,
    min_sitelinks: u32,
) -> Result<PathBuf> {
    download_wikidata_official_sites_paced(
        client,
        endpoint,
        dir,
        min_sitelinks,
        WikidataPacing::default(),
    )
    .await
}

/// How [`download_wikidata_official_sites_paced`] spaces out its queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WikidataPacing {
    /// The wait before each band's query but the first. Wikidata's query
    /// service limits the query time each client uses per minute, and
    /// answers HTTP 429 to clients over the limit.
    pub pause: Duration,
    /// The first wait before asking for a band again, after Wikidata was too
    /// busy to answer it whole (or answered HTTP 429 or 5xx without a
    /// `Retry-After`, or could not be reached). It doubles with each try of
    /// the same band, up to 10 times this. All the waits of one download
    /// together may come to 40 times this ([`WikidataPacing::budget`]).
    pub retry_wait: Duration,
}

impl Default for WikidataPacing {
    /// 5 seconds between two bands; 30, 60, 120, 240 and 300 seconds before
    /// asking for a band again; 20 minutes of such waits in all.
    fn default() -> Self {
        WikidataPacing {
            pause: Duration::from_secs(5),
            retry_wait: Duration::from_secs(30),
        }
    }
}

impl WikidataPacing {
    /// The wait before try `tries + 1` of a band: `retry_wait`, doubled for
    /// each try after the first, and at most 10 times `retry_wait`.
    pub fn wait_after(&self, tries: u32) -> Duration {
        let doubled = 1u32
            .checked_shl(tries.saturating_sub(1))
            .unwrap_or(u32::MAX)
            .min(MAX_WAIT_FACTOR);
        self.retry_wait.saturating_mul(doubled)
    }

    /// The shortest wait after HTTP 429 without a `Retry-After`: twice
    /// `retry_wait`, 60 seconds by default.
    pub fn rate_limit_wait(&self) -> Duration {
        self.retry_wait.saturating_mul(2)
    }

    /// How long one download may wait, in all, before asking for bands
    /// again: 40 times `retry_wait`. A download that would wait longer
    /// stops; what it has is saved for the next.
    pub fn budget(&self) -> Duration {
        self.retry_wait.saturating_mul(WAIT_BUDGET_FACTOR)
    }
}

/// The longest wait before asking for a band again, as a multiple of
/// [`WikidataPacing::retry_wait`].
const MAX_WAIT_FACTOR: u32 = 10;

/// All the waits of one download, as a multiple of
/// [`WikidataPacing::retry_wait`].
const WAIT_BUDGET_FACTOR: u32 = 40;

/// Tries of one band before it is split, or the download given up.
const WIKIDATA_TRIES: u32 = 6;

/// Answers of one band saying Wikidata is too busy before it is halved: a
/// narrower query is more likely to finish than more waiting (on
/// 2026-10-03 the 25 to 29 sitelinks band timed out five times in a row
/// while 25-26 and 27-29 had come in that morning).
const BUSY_TRIES: u32 = 3;

/// How many times a band of [`wikidata_sitelink_bands`] may be halved: a
/// band becomes at most 4 queries. Halving barely speeds a query up, so it
/// is only done when a band failed all its tries.
const MAX_SPLITS: u32 = 2;

/// The longest `Retry-After` a download waits for; an answer that asks for
/// more fails the download, to be tried again much later.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// A query without a whole answer after this long is treated like one that
/// Wikidata stopped at its 60-second limit.
const WIKIDATA_QUERY_TIMEOUT: Duration = Duration::from_secs(180);

/// Bands saved by an earlier download longer ago than this are asked for again.
const SAVED_BAND_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The folder next to the TSV where each band is saved as it comes in,
/// until the TSV is written.
pub const WIKIDATA_BANDS_DIR_NAME: &str = "wikidata-official-sites.bands";

/// What Wikidata's query service (Blazegraph) writes when it stops a query
/// at its time limit: after the results sent so far when the answer had
/// already started as HTTP 200, or as the body of an HTTP 500.
const TIMEOUT_MARKERS: [&str; 2] = [
    "java.util.concurrent.TimeoutException",
    "com.bigdata.bop.engine.QueryTimeoutException",
];

/// [`download_wikidata_official_sites_from`] with the waits of `pacing`.
///
/// The bands of [`wikidata_sitelink_bands`] are asked for one at a time,
/// lowest first, waiting `pacing.pause` before each but the first, and each
/// answer is logged. Every band that comes in whole is saved at once in
/// `dir/wikidata-official-sites.bands/`; a download finds the bands an
/// earlier one saved there (in the last week, for the same or a lower
/// minimum) and asks only for the rest. Once every band is in, their rows go
/// into the TSV, through a `.part` file renamed when complete, and the saved
/// bands are removed.
///
/// An answer must be whole: when Wikidata stops a query at its time limit,
/// the answer still comes as HTTP 200, but its JSON breaks off and the query
/// service's Java exception follows. Such an answer, one that ends in that
/// exception (also as HTTP 500), HTTP 503 or 504, and no whole answer within
/// 3 minutes all mean that Wikidata was too busy: the band is asked for
/// again after a wait ([`WikidataPacing::wait_after`]). A band too busy 3
/// times is asked for in two halves
/// ([`SitelinkBand::halves`]), and a band of the defaults is halved at most
/// twice; then the download fails.
///
/// A band still too busy when the waits run out is halved too, each half
/// getting at least one try, so a long wait on one band does not end the
/// download while narrower queries might get through.
///
/// HTTP 429 and other 5xx, and failed connections, are tried again the same
/// way, but not halved; after HTTP 429 the wait is at least
/// [`WikidataPacing::rate_limit_wait`]. A `Retry-After` is waited for
/// instead (at most 10 minutes; an answer that asks for more fails the
/// download). The waits of
/// one download come to at most [`WikidataPacing::budget`]; past that, it
/// fails. Other failures, such as HTTP 4xx or an answer that is not SPARQL
/// JSON, fail the download at once. A download that fails keeps the bands it
/// saved for the next.
pub async fn download_wikidata_official_sites_paced(
    client: &reqwest::Client,
    endpoint: &str,
    dir: &Path,
    min_sitelinks: u32,
    pacing: WikidataPacing,
) -> Result<PathBuf> {
    let bands_dir = dir.join(WIKIDATA_BANDS_DIR_NAME);
    let mut done = saved_bands(&bands_dir, min_sitelinks)?;
    let missing = missing_bands(&wikidata_sitelink_bands(min_sitelinks), &done);
    if done.is_empty() {
        info!(
            "asking Wikidata for official websites of items with at least {min_sitelinks} \
             sitelinks, in {} queries by number of sitelinks",
            missing.len()
        );
    } else {
        info!(
            "asking Wikidata for official websites of items with at least {min_sitelinks} \
             sitelinks: {} bands of sitelink counts were saved by an earlier try, {} to go",
            done.len(),
            missing.len()
        );
    }
    let started = Instant::now();
    let mut budget = pacing.budget();
    // The bands still to ask for, the next one last, with how many times
    // each was halved.
    let mut todo: Vec<(SitelinkBand, u32)> = missing.into_iter().rev().map(|b| (b, 0)).collect();
    let mut queries = 0u32;
    while let Some((band, splits)) = todo.pop() {
        if queries > 0 {
            tokio::time::sleep(pacing.pause).await;
        }
        let asked = Instant::now();
        let answer = query_band(client, endpoint, band, pacing, &mut budget, &mut queries).await;
        match answer {
            Ok(rows) => {
                let got = rows.len();
                save_band(&bands_dir, band, rows).await?;
                done.push(band);
                info!(
                    "Wikidata: {got} official websites of items with {band} in {:.1} s; \
                     {} bands to go",
                    asked.elapsed().as_secs_f64(),
                    todo.len()
                );
            }
            Err(BandFailure::Busy(why)) => {
                let halves = band.halves().filter(|_| splits < MAX_SPLITS);
                let Some((lower, upper)) = halves else {
                    bail!(
                        "Wikidata was too busy to answer the query for items with {band} \
                         ({why}); {}{}",
                        match splits {
                            0 => "it cannot be split; ".to_string(),
                            n => format!("it is already split {n} times; "),
                        },
                        try_later(&done)
                    );
                };
                warn!(
                    "Wikidata was too busy to answer the query for items with {band} ({why}); \
                     asking for items with {lower} and with {upper} separately"
                );
                todo.push((upper, splits + 1));
                todo.push((lower, splits + 1));
            }
            Err(BandFailure::Fatal(err)) => {
                return Err(err.context(format!(
                    "asking Wikidata for items with {band} ({})",
                    try_later(&done)
                )));
            }
        }
    }

    done.sort_by_key(|band| band.min);
    let mut tsv = OfficialSitesTsv::new();
    for &band in &done {
        let path = bands_dir.join(band.file_name());
        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        tsv.add(saved_rows(&text));
    }
    crate::storage::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(WIKIDATA_FILE_NAME);
    let part = part_path(&dest);
    crate::storage::write(&part, tsv.text.as_bytes())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    crate::storage::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    if let Err(err) = crate::storage::remove_dir_all(&bands_dir).await {
        if err.kind() != io::ErrorKind::NotFound {
            warn!("could not remove {}: {err}", bands_dir.display());
        }
    }
    info!(
        "wrote {} official websites to {} after {queries} queries in {:.0} s",
        tsv.rows(),
        dest.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(dest)
}

/// [`download_wikidata_official_sites_paced`], asking the `mirror` first
/// when there is one ([`download_wikidata_official_sites_bulk`]): one query
/// there lists them all in seconds, while Wikidata's own query service
/// needs a query per band of sitelink counts, each close to its 60-second
/// limit. When the mirror fails, the bands are asked of `endpoint`, from
/// [`ENDPOINT_MIN_SITELINKS`] up at least: below that, bands of a single
/// count run past its limit too (exactly 3 sitelinks did on 2026-10-09),
/// and a node is better off with the better-known sites than with none.
pub async fn download_wikidata_official_sites_with(
    client: &reqwest::Client,
    mirror: Option<&str>,
    endpoint: &str,
    dir: &Path,
    min_sitelinks: u32,
    pacing: WikidataPacing,
) -> Result<PathBuf> {
    if let Some(mirror) = mirror {
        match download_wikidata_official_sites_bulk(client, mirror, dir, min_sitelinks, pacing)
            .await
        {
            Ok(path) => return Ok(path),
            Err(err) => {
                let min = min_sitelinks.max(ENDPOINT_MIN_SITELINKS);
                warn!(
                    "could not list the official websites at {mirror} ({err:#}); \
                     asking {endpoint} band by band, from {min} sitelinks up"
                );
                return download_wikidata_official_sites_paced(client, endpoint, dir, min, pacing)
                    .await;
            }
        }
    }
    download_wikidata_official_sites_paced(client, endpoint, dir, min_sitelinks, pacing).await
}

/// The fewest sitelinks [`download_wikidata_official_sites_with`] asks
/// Wikidata's own endpoint for once the mirror has failed.
pub const ENDPOINT_MIN_SITELINKS: u32 = 25;

/// The fewest sitelinks of the items whose official websites are fetched
/// by default. On 2026-10-09, going from 25 down to 3 (131k to 627k sites)
/// put the right site in the top 10 for 64.7% of described searches
/// instead of 56.9% (tune half; 71.0% instead of 68.8% held out), with
/// brand and ai searches about level: Expedia, Indeed, Glassdoor and
/// Instacart are below 25.
pub const DEFAULT_MIN_SITELINKS: u32 = 3;

/// The query for every item with an official website and at least
/// `min_sitelinks` sitelinks, with its English (or multilingual) label, or
/// its id when it has neither, as Wikidata's label service gives. It needs
/// no label service, so endpoints other than Wikidata's own can answer it.
/// The sitelink count is read as a number first, since QLever compares its
/// counts with a plain number as never equal.
pub fn bulk_official_sites_query(min_sitelinks: u32) -> String {
    format!(
        "{WIKIDATA_PREFIXES}SELECT ?item ?itemLabel ?website WHERE {{ \
         ?item wdt:P856 ?website ; wikibase:sitelinks ?s . \
         FILTER(xsd:integer(?s) >= {min_sitelinks}) \
         OPTIONAL {{ ?item rdfs:label ?en . FILTER(LANG(?en) = \"en\") }} \
         OPTIONAL {{ ?item rdfs:label ?mul . FILTER(LANG(?mul) = \"mul\") }} \
         BIND(COALESCE(?en, ?mul, STRAFTER(STR(?item), \"/entity/\")) AS ?itemLabel) }}"
    )
}

/// Fewer rows than this from [`download_wikidata_official_sites_bulk`]
/// means the endpoint did not understand the query as meant: there were
/// 131,386 items from 25 sitelinks up in October 2026, and an empty answer
/// would leave a node without official websites.
const MIN_BULK_ROWS: usize = 1_000;

/// Asks `endpoint`, such as [`QLEVER_WIKIDATA_URL`], for every official
/// website of items with at least `min_sitelinks` sitelinks in one query
/// ([`bulk_official_sites_query`]), and writes the same TSV as
/// [`download_wikidata_official_sites_paced`]. An answer that breaks off,
/// HTTP 429 or 5xx and failed connections are tried again after the waits
/// of `pacing` (or the `Retry-After`), up to 6 tries; an answer with fewer
/// than 1,000 rows is an error, so a mirror that reads the query
/// differently is not taken for the truth.
pub async fn download_wikidata_official_sites_bulk(
    client: &reqwest::Client,
    endpoint: &str,
    dir: &Path,
    min_sitelinks: u32,
    pacing: WikidataPacing,
) -> Result<PathBuf> {
    info!(
        "asking {endpoint} for official websites of items with at least {min_sitelinks} \
         sitelinks, in one query"
    );
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("query", &bulk_official_sites_query(min_sitelinks))
        .finish();
    let started = Instant::now();
    let mut tries = 0;
    let rows = loop {
        tries += 1;
        let (why, after, rate_limited) = match try_query(client, endpoint, &form).await {
            Ok(rows) => break rows,
            Err(Failed::Fatal(err)) => return Err(err),
            Err(Failed::Again {
                why,
                after,
                rate_limited,
                ..
            }) => (why, after, rate_limited),
        };
        if tries >= WIKIDATA_TRIES {
            bail!("{why} (tried {tries} times)");
        }
        let wait = match after {
            Some(after) if after > MAX_RETRY_AFTER => bail!(
                "{why}; the answer asks to wait {} seconds before the next query",
                after.as_secs()
            ),
            Some(after) => after,
            None if rate_limited => pacing.wait_after(tries).max(pacing.rate_limit_wait()),
            None => pacing.wait_after(tries),
        };
        warn!(
            "{endpoint}: {why}; trying again in {:.1} s",
            wait.as_secs_f64()
        );
        tokio::time::sleep(wait).await;
    };
    if rows.len() < MIN_BULK_ROWS {
        bail!(
            "only {} official websites came back, fewer than the {MIN_BULK_ROWS} expected",
            rows.len()
        );
    }
    let mut tsv = OfficialSitesTsv::new();
    tsv.add(rows);
    crate::storage::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(WIKIDATA_FILE_NAME);
    let part = part_path(&dest);
    crate::storage::write(&part, tsv.text.as_bytes())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    crate::storage::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!(
        "wrote {} official websites to {} after {tries} queries in {:.0} s",
        tsv.rows(),
        dest.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(dest)
}

/// What a failed download says about trying again, given the bands saved.
fn try_later(done: &[SitelinkBand]) -> String {
    match done.len() {
        0 => "try again later".to_string(),
        1 => "try again later; the 1 band already in is saved".to_string(),
        n => format!("try again later; the {n} bands already in are saved"),
    }
}

/// The bands that an earlier download saved in `bands_dir` and that this
/// one can use, lowest first: saved in the last week, with at least
/// `min_sitelinks` sitelinks, and not overlapping one another. Others found
/// there are removed. None when `bands_dir` does not exist.
fn saved_bands(bands_dir: &Path, min_sitelinks: u32) -> Result<Vec<SitelinkBand>> {
    let entries = match std::fs::read_dir(bands_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", bands_dir.display())),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", bands_dir.display()))?;
        let band = entry
            .file_name()
            .to_str()
            .and_then(SitelinkBand::from_file_name);
        let recent = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| {
                SystemTime::now()
                    .duration_since(modified)
                    .map_or(true, |age| age < SAVED_BAND_MAX_AGE)
            });
        match band {
            Some(band) if recent && band.min >= min_sitelinks => found.push(band),
            _ => {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    found.sort_by_key(|band| band.min);
    let mut usable: Vec<SitelinkBand> = Vec::new();
    for band in found {
        let overlaps = usable
            .last()
            .is_some_and(|last| last.max.is_none_or(|max| band.min <= max));
        if overlaps {
            let _ = std::fs::remove_file(bands_dir.join(band.file_name()));
        } else {
            usable.push(band);
        }
    }
    Ok(usable)
}

/// The parts of `wanted` (lowest first, not overlapping) that no band of
/// `saved` (the same) covers, lowest first. A band of `wanted` that is
/// partly covered leaves its uncovered stretches.
fn missing_bands(wanted: &[SitelinkBand], saved: &[SitelinkBand]) -> Vec<SitelinkBand> {
    let mut missing = Vec::new();
    for &band in wanted {
        // The lowest count of the band not yet seen to be covered; `None`
        // once all of it is.
        let mut from = Some(band.min);
        for s in saved {
            let Some(start) = from else { break };
            if s.max.is_some_and(|max| max < start) {
                continue;
            }
            if band.max.is_some_and(|max| s.min > max) {
                break;
            }
            if s.min > start {
                missing.push(SitelinkBand {
                    min: start,
                    max: Some(s.min - 1),
                });
            }
            from = s
                .max
                .and_then(|max| max.checked_add(1))
                .filter(|&next| band.max.is_none_or(|max| next <= max));
        }
        if let Some(start) = from {
            missing.push(SitelinkBand {
                min: start,
                max: band.max,
            });
        }
    }
    missing
}

/// Saves the rows of `band` in `bands_dir`, through a `.part` file.
async fn save_band(bands_dir: &Path, band: SitelinkBand, rows: Vec<WikidataRow>) -> Result<()> {
    crate::storage::create_dir_all(bands_dir)
        .await
        .with_context(|| format!("creating {}", bands_dir.display()))?;
    let mut tsv = OfficialSitesTsv::new();
    tsv.add(rows);
    let dest = bands_dir.join(band.file_name());
    let part = part_path(&dest);
    crate::storage::write(&part, tsv.text.as_bytes())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    crate::storage::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))
}

/// The rows of a band that [`save_band`] saved.
fn saved_rows(text: &str) -> Vec<WikidataRow> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let (item, label, website) = (fields.next()?, fields.next()?, fields.next()?);
            Some(WikidataRow {
                item: item.to_string(),
                label: label.to_string(),
                website: website.to_string(),
            })
        })
        .collect()
}

/// A range of sitelink counts that one Wikidata query asks for: `min` to
/// `max`, both included, or `min` and up when `max` is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SitelinkBand {
    pub min: u32,
    pub max: Option<u32>,
}

/// Where the bands of [`wikidata_sitelink_bands`] start from 25 sitelinks up.
const WIKIDATA_BAND_STARTS: [u32; 7] = [25, 30, 36, 45, 60, 85, 130];

/// An open band, `min` sitelinks or more, is not split once `min` is this
/// high: no Wikidata item has nearly that many sitelinks.
const OPEN_BAND_SPLIT_LIMIT: u32 = 1_000;

/// The bands of sitelink counts that [`download_wikidata_official_sites`]
/// asks for, lowest first. Together they cover every count from
/// `min_sitelinks` up, each once: below 25 each count is a band of its own,
/// then come 25-29, 30-35, 36-44, 45-59, 60-84, 85-129 and 130 or more.
///
/// From 25 up there were 131,386 rows in October 2026, too many for one
/// query to list within Wikidata's 60-second limit. These bands came in
/// whole then, holding 27,397, 28,773, 38,990, 24,556, 6,942, 3,314 and
/// 1,414 rows, in 21 to 47 seconds each.
pub fn wikidata_sitelink_bands(min_sitelinks: u32) -> Vec<SitelinkBand> {
    let starts: Vec<u32> = std::iter::once(min_sitelinks)
        .chain(min_sitelinks.saturating_add(1)..WIKIDATA_BAND_STARTS[0])
        .chain(
            WIKIDATA_BAND_STARTS
                .into_iter()
                .filter(|&start| start > min_sitelinks),
        )
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(i, &min)| SitelinkBand {
            min,
            max: starts.get(i + 1).map(|next| next - 1),
        })
        .collect()
}

impl SitelinkBand {
    /// The SPARQL query for the items in the band, as
    /// [`wikidata_sparql_query`] is for those from a minimum up.
    pub fn sparql_query(&self) -> String {
        if self.max == Some(self.min) {
            // One count: naming it lets Wikidata start from its index of
            // sitelink counts instead of every item with a website (on
            // 2026-10-09 the filter for exactly 20 ran past the limit,
            // while this answered in 50 seconds).
            return format!(
                "SELECT ?item ?itemLabel ?website WHERE {{ ?item wikibase:sitelinks {} . ?item wdt:P856 ?website . SERVICE wikibase:label {{ bd:serviceParam wikibase:language \"en,mul\". }} }}",
                self.min
            );
        }
        let filter = match self.max {
            Some(max) => format!("?s >= {} && ?s < {}", self.min, u64::from(max) + 1),
            None => format!("?s >= {}", self.min),
        };
        format!(
            "SELECT ?item ?itemLabel ?website WHERE {{ ?item wdt:P856 ?website ; wikibase:sitelinks ?s . FILTER({filter}) SERVICE wikibase:label {{ bd:serviceParam wikibase:language \"en,mul\". }} }}"
        )
    }

    /// The band in two, lower half first, to ask for once Wikidata was too
    /// busy for the band's query; `None` when it cannot be split. A band
    /// with an upper end is cut in the middle, down to single counts. One
    /// without, `min` and up, is cut into `min` to `2 * min - 1` and
    /// `2 * min` and up, since items with more sitelinks are much rarer,
    /// unless `min` is 1,000 or more.
    pub fn halves(&self) -> Option<(SitelinkBand, SitelinkBand)> {
        let band = |min, max| SitelinkBand { min, max };
        match self.max {
            Some(max) if max > self.min => {
                let mid = self.min + (max - self.min) / 2;
                Some((band(self.min, Some(mid)), band(mid + 1, Some(max))))
            }
            Some(_) => None,
            None if self.min >= OPEN_BAND_SPLIT_LIMIT => None,
            None => {
                let upper = self.min.saturating_mul(2).max(self.min + 1);
                Some((band(self.min, Some(upper - 1)), band(upper, None)))
            }
        }
    }

    /// The name of the file the band is saved in: `25-29.tsv`, or
    /// `130-up.tsv` for 130 or more sitelinks.
    fn file_name(&self) -> String {
        match self.max {
            Some(max) => format!("{}-{max}.tsv", self.min),
            None => format!("{}-up.tsv", self.min),
        }
    }

    /// The band saved in a file named `name` by [`SitelinkBand::file_name`].
    fn from_file_name(name: &str) -> Option<SitelinkBand> {
        let (min, max) = name.strip_suffix(".tsv")?.split_once('-')?;
        let min = min.parse().ok()?;
        let max = match max {
            "up" => None,
            max => Some(max.parse().ok()?),
        };
        max.is_none_or(|max| min <= max)
            .then_some(SitelinkBand { min, max })
    }
}

impl fmt::Display for SitelinkBand {
    /// `exactly 25 sitelinks`, `25 to 29 sitelinks` or `130 or more sitelinks`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.max {
            Some(1) if self.min == 1 => f.write_str("exactly 1 sitelink"),
            Some(max) if max == self.min => write!(f, "exactly {max} sitelinks"),
            Some(max) => write!(f, "{} to {max} sitelinks", self.min),
            None => write!(f, "{} or more sitelinks", self.min),
        }
    }
}

/// The SPARQL query for items with an official website and at least
/// `min_sitelinks` sitelinks, with English (or multilingual) labels. The
/// download asks for narrower bands of it ([`SitelinkBand::sparql_query`]).
pub fn wikidata_sparql_query(min_sitelinks: u32) -> String {
    SitelinkBand {
        min: min_sitelinks,
        max: None,
    }
    .sparql_query()
}

/// Why the query for one band has no rows.
#[derive(Debug)]
enum BandFailure {
    /// Wikidata stayed too busy to answer it whole through every try: ask
    /// for less. Says what was seen last.
    Busy(String),
    /// Not worth asking again in this download.
    Fatal(anyhow::Error),
}

/// Why one try of a query has no rows.
#[derive(Debug)]
enum Failed {
    /// Worth another try: after `after`, when the server says how long to
    /// wait. `busy` when Wikidata was too busy to answer it whole, so that
    /// a narrower query may do once tries run out; `rate_limited` after
    /// HTTP 429, which calls for a longer wait.
    Again {
        why: String,
        after: Option<Duration>,
        busy: bool,
        rate_limited: bool,
    },
    /// Not worth another try.
    Fatal(anyhow::Error),
}

impl Failed {
    fn busy(why: String) -> Self {
        Failed::Again {
            why,
            after: None,
            busy: true,
            rate_limited: false,
        }
    }
}

/// Asks for one band, again after a wait while Wikidata is too busy or
/// answers HTTP 429 or 5xx, or cannot be reached; see
/// [`download_wikidata_official_sites_paced`]. `budget` is what is left of
/// the download's waits, and `queries` counts every query sent.
async fn query_band(
    client: &reqwest::Client,
    endpoint: &str,
    band: SitelinkBand,
    pacing: WikidataPacing,
    budget: &mut Duration,
    queries: &mut u32,
) -> Result<Vec<WikidataRow>, BandFailure> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("query", &band.sparql_query())
        .finish();
    let mut tries = 0;
    let mut busy_tries = 0;
    loop {
        tries += 1;
        *queries += 1;
        let (why, after, busy, rate_limited) = match try_query(client, endpoint, &form).await {
            Ok(rows) => return Ok(rows),
            Err(Failed::Fatal(err)) => return Err(BandFailure::Fatal(err)),
            Err(Failed::Again {
                why,
                after,
                busy,
                rate_limited,
            }) => (why, after, busy, rate_limited),
        };
        busy_tries += u32::from(busy);
        if busy && busy_tries >= BUSY_TRIES {
            return Err(BandFailure::Busy(format!("{why} (tried {tries} times)")));
        }
        if tries >= WIKIDATA_TRIES {
            let why = format!("{why} (tried {tries} times)");
            return Err(if busy {
                BandFailure::Busy(why)
            } else {
                BandFailure::Fatal(anyhow!(why))
            });
        }
        let wait = match after {
            Some(after) if after > MAX_RETRY_AFTER => {
                return Err(BandFailure::Fatal(anyhow!(
                    "{why}; the answer asks to wait {} seconds before the next query",
                    after.as_secs()
                )))
            }
            Some(after) => after,
            None if rate_limited => pacing.wait_after(tries).max(pacing.rate_limit_wait()),
            None => pacing.wait_after(tries),
        };
        if wait > *budget && busy {
            // Out of waiting time: a smaller query is the better bet.
            return Err(BandFailure::Busy(format!(
                "{why} (tried {tries} times, with no waiting time left)"
            )));
        }
        if wait > *budget {
            return Err(BandFailure::Fatal(anyhow!(
                "{why} (tried {tries} times); waiting {wait:?} more would pass the {:?} this \
                 download may wait in all",
                pacing.budget()
            )));
        }
        *budget -= wait;
        warn!(
            "Wikidata query for items with {band}: {why}; trying again in {:.1} s",
            wait.as_secs_f64()
        );
        tokio::time::sleep(wait).await;
    }
}

/// Sends a query once and reads its answer.
async fn try_query(
    client: &reqwest::Client,
    endpoint: &str,
    form: &str,
) -> Result<Vec<WikidataRow>, Failed> {
    let sent = client
        .post(endpoint)
        .header(ACCEPT, "application/sparql-results+json")
        .header(ACCEPT_ENCODING, "gzip")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .timeout(WIKIDATA_QUERY_TIMEOUT)
        .body(form.to_string())
        .send()
        .await;
    let response = match sent {
        Ok(response) => response,
        Err(err) if err.is_timeout() && !err.is_connect() => {
            return Err(Failed::busy(no_answer_in_time()));
        }
        Err(err) if err.is_builder() => {
            let err = anyhow::Error::new(err).context(format!("querying {endpoint}"));
            return Err(Failed::Fatal(err));
        }
        Err(err) => {
            return Err(Failed::Again {
                why: format!("{:#}", anyhow::Error::new(err)),
                after: None,
                busy: false,
                rate_limited: false,
            });
        }
    };
    let status = response.status();
    let gzipped = response
        .headers()
        .get(CONTENT_ENCODING)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"gzip"));
    if status.is_success() {
        return match response.bytes().await {
            Ok(body) => match parse_sparql_rows(&decoded(&body, gzipped)) {
                Ok(rows) => Ok(rows),
                Err(BadAnswer::Cut(why)) => Err(Failed::busy(why)),
                Err(BadAnswer::Invalid(err)) => Err(Failed::Fatal(err)),
            },
            Err(err) if err.is_timeout() => Err(Failed::busy(no_answer_in_time())),
            Err(err) => Err(Failed::busy(format!(
                "the answer broke off: {:#}",
                anyhow::Error::new(err)
            ))),
        };
    }
    let after = retry_after(response.headers());
    let body = response.bytes().await.unwrap_or_default();
    let body = decoded(&body, gzipped);
    if status.is_server_error() && has_timeout_marker(&body) {
        return Err(Failed::busy(format!(
            "HTTP {status} with the query service's timeout error"
        )));
    }
    let why = format!(
        "HTTP {status}: {}",
        plumb_core::truncate_chars(String::from_utf8_lossy(&body).trim(), 500)
    );
    let busy = status == StatusCode::GATEWAY_TIMEOUT || status == StatusCode::SERVICE_UNAVAILABLE;
    let rate_limited = status == StatusCode::TOO_MANY_REQUESTS;
    if busy || rate_limited || status.is_server_error() {
        Err(Failed::Again {
            why,
            after,
            busy,
            rate_limited,
        })
    } else {
        Err(Failed::Fatal(anyhow!("Wikidata query failed: {why}")))
    }
}

/// `body` gunzipped when `gzipped`, as far as it goes: an answer cut off in
/// the middle gives what came before the cut.
fn decoded(body: &[u8], gzipped: bool) -> Vec<u8> {
    if !gzipped {
        return body.to_vec();
    }
    let mut out = Vec::new();
    let _ = io::Read::read_to_end(&mut flate2::read::MultiGzDecoder::new(body), &mut out);
    out
}

fn no_answer_in_time() -> String {
    format!(
        "no whole answer within {} seconds",
        WIKIDATA_QUERY_TIMEOUT.as_secs()
    )
}

/// How long a `Retry-After` header asks to wait: a number of seconds or an
/// HTTP date. `None` without one, or when it cannot be read.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(
        at.duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

/// True when `body` holds the error that Wikidata's query service writes
/// when it stops a query at its time limit ([`TIMEOUT_MARKERS`]).
fn has_timeout_marker(body: &[u8]) -> bool {
    TIMEOUT_MARKERS.iter().any(|marker| {
        body.windows(marker.len())
            .any(|window| window == marker.as_bytes())
    })
}

/// Why a SPARQL answer gave no rows.
#[derive(Debug)]
enum BadAnswer {
    /// The answer breaks off, as when Wikidata stops a query at its time
    /// limit, so a narrower query may do. Says what was seen.
    Cut(String),
    /// Not SPARQL JSON results at all.
    Invalid(anyhow::Error),
}

impl BadAnswer {
    /// Why `body`, which `err` says is not whole SPARQL JSON results, gave
    /// no rows: [`BadAnswer::Cut`] when it holds the query service's timeout
    /// error or is JSON that breaks off (that ends early, or goes on with
    /// something else), [`BadAnswer::Invalid`] otherwise.
    fn of(body: &[u8], err: serde_json::Error) -> Self {
        let size = megabytes(body.len() as u64);
        if has_timeout_marker(body) {
            return BadAnswer::Cut(format!(
                "the answer breaks off after {size} with the query service's timeout error"
            ));
        }
        let starts_as_json = body.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{');
        match err.classify() {
            Category::Eof => BadAnswer::Cut(format!("the answer stops after {size}")),
            Category::Syntax if starts_as_json => {
                BadAnswer::Cut(format!("the answer breaks off after {size}"))
            }
            _ => {
                let start = String::from_utf8_lossy(&body[..body.len().min(400)]);
                BadAnswer::Invalid(anyhow::Error::new(err).context(format!(
                    "Wikidata's answer is not SPARQL JSON results: {}",
                    snippet(start.trim())
                )))
            }
        }
    }

    fn into_error(self) -> anyhow::Error {
        match self {
            BadAnswer::Cut(why) => anyhow!("Wikidata stopped the query at its time limit: {why}"),
            BadAnswer::Invalid(err) => err,
        }
    }
}

/// The rows of a SPARQL JSON answer, or why it has none.
fn parse_sparql_rows(body: &[u8]) -> Result<Vec<WikidataRow>, BadAnswer> {
    match serde_json::from_slice::<SparqlResponse>(body) {
        Ok(response) => Ok(response
            .results
            .bindings
            .iter()
            .filter_map(WikidataRow::from_binding)
            .collect()),
        Err(err) => Err(BadAnswer::of(body, err)),
    }
}

/// Converts SPARQL JSON results (`results.bindings[]` with `item`,
/// `itemLabel` and `website`) into the TSV that
/// [`crate::load_wikidata_official_sites`] reads: the header
/// `item\tlabel\twebsite`, then one row per binding.
///
/// Items lose their entity URL prefix (`http://www.wikidata.org/entity/Q1`
/// becomes `Q1`), tabs and line breaks inside values become spaces, and rows
/// missing a field (or with an empty one), or repeating an item and website
/// pair, are skipped. Results that break off, as when Wikidata stopped the
/// query at its time limit, are an error that says so.
pub fn wikidata_json_to_tsv(json: &[u8]) -> Result<String> {
    let rows = parse_sparql_rows(json).map_err(BadAnswer::into_error)?;
    let mut tsv = OfficialSitesTsv::new();
    tsv.add(rows);
    Ok(tsv.text)
}

/// One result: an item, its label and an official website, each made safe
/// for a TSV cell.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WikidataRow {
    item: String,
    label: String,
    website: String,
}

impl WikidataRow {
    /// `None` when a field is missing or blank.
    fn from_binding(binding: &SparqlBinding) -> Option<Self> {
        let item = tsv_field(&binding.item)?;
        let label = tsv_field(&binding.item_label)?;
        let website = tsv_field(&binding.website)?;
        let item = bare_item_id(&item);
        (!item.is_empty()).then(|| WikidataRow {
            item: item.to_string(),
            label,
            website,
        })
    }
}

/// The TSV that [`crate::load_wikidata_official_sites`] reads, made of the
/// rows of every band: the header `item\tlabel\twebsite`, then the rows in
/// the order they were added, each item and website pair once. A pair can
/// come twice when an item's sitelinks change between two queries.
#[derive(Debug)]
struct OfficialSitesTsv {
    text: String,
    seen: HashSet<(String, String)>,
}

impl OfficialSitesTsv {
    fn new() -> Self {
        OfficialSitesTsv {
            text: String::from("item\tlabel\twebsite\n"),
            seen: HashSet::new(),
        }
    }

    /// Adds the rows whose item and website pair is not in yet; returns
    /// how many.
    fn add(&mut self, rows: Vec<WikidataRow>) -> usize {
        let mut added = 0;
        for row in rows {
            if !self.seen.insert((row.item.clone(), row.website.clone())) {
                continue;
            }
            for (field, end) in [(&row.item, '\t'), (&row.label, '\t'), (&row.website, '\n')] {
                self.text.push_str(field);
                self.text.push(end);
            }
            added += 1;
        }
        added
    }

    /// Rows after the header.
    fn rows(&self) -> usize {
        self.seen.len()
    }
}

/// A binding value made safe for one TSV cell; `None` when missing or blank.
fn tsv_field(term: &Option<SparqlTerm>) -> Option<String> {
    let value = term.as_ref()?.value.replace(['\t', '\n', '\r'], " ");
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[derive(Debug, Deserialize)]
struct SparqlResponse {
    results: SparqlResults,
}

#[derive(Debug, Deserialize)]
struct SparqlResults {
    bindings: Vec<SparqlBinding>,
}

#[derive(Debug, Deserialize)]
struct SparqlBinding {
    item: Option<SparqlTerm>,
    #[serde(rename = "itemLabel")]
    item_label: Option<SparqlTerm>,
    website: Option<SparqlTerm>,
}

#[derive(Debug, Deserialize)]
struct SparqlTerm {
    value: String,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn sparql_query_text() {
        assert_eq!(
            wikidata_sparql_query(20),
            "SELECT ?item ?itemLabel ?website WHERE { ?item wdt:P856 ?website ; wikibase:sitelinks ?s . FILTER(?s >= 20) SERVICE wikibase:label { bd:serviceParam wikibase:language \"en,mul\". } }"
        );
    }

    #[test]
    fn sparql_json_to_tsv() {
        let json = r#"{
          "head": {"vars": ["item", "itemLabel", "website"]},
          "results": {"bindings": [
            {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q739868"},
             "website": {"type": "uri", "value": "https://www.usbank.com/"},
             "itemLabel": {"xml:lang": "en", "type": "literal", "value": "U.S. Bancorp"}},
            {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q2"},
             "website": {"type": "uri", "value": "https://example.org/"},
             "itemLabel": {"type": "literal", "value": "Tabbed\tand\nsplit\r\nlabel"}},
            {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q3"},
             "itemLabel": {"type": "literal", "value": "No website"}},
            {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q4"},
             "website": {"type": "uri", "value": "https://nolabel.org/"}},
            {"website": {"type": "uri", "value": "https://noitem.org/"},
             "itemLabel": {"type": "literal", "value": "No item"}},
            {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q6"},
             "website": {"type": "uri", "value": "https://blank.org/"},
             "itemLabel": {"type": "literal", "value": " \t "}},
            {"item": {"type": "uri", "value": "Q7"},
             "website": {"type": "uri", "value": "https://bare.org/"},
             "itemLabel": {"type": "literal", "value": "Bare id"}}
          ]}
        }"#;
        assert_eq!(
            wikidata_json_to_tsv(json.as_bytes()).unwrap(),
            "item\tlabel\twebsite\n\
             Q739868\tU.S. Bancorp\thttps://www.usbank.com/\n\
             Q2\tTabbed and split  label\thttps://example.org/\n\
             Q7\tBare id\thttps://bare.org/\n"
        );
        let empty = r#"{"head": {"vars": []}, "results": {"bindings": []}}"#;
        assert_eq!(
            wikidata_json_to_tsv(empty.as_bytes()).unwrap(),
            "item\tlabel\twebsite\n"
        );
        assert!(wikidata_json_to_tsv(b"<html>Query timeout</html>").is_err());
        assert!(wikidata_json_to_tsv(br#"{"head": {}}"#).is_err());
    }

    #[test]
    fn tsv_round_trips_through_the_loader() {
        let json = r#"{"results": {"bindings": [
            {"item": {"value": "http://www.wikidata.org/entity/Q739868"},
             "itemLabel": {"value": "U.S. Bancorp"},
             "website": {"value": "https://www.usbank.com/"}}
        ]}}"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WIKIDATA_FILE_NAME);
        std::fs::write(&path, wikidata_json_to_tsv(json.as_bytes()).unwrap()).unwrap();
        let sites = crate::load_wikidata_official_sites(&path).unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].item, "Q739868");
        assert_eq!(sites[0].label, "U.S. Bancorp");
        assert_eq!(sites[0].domain, "usbank.com");
    }

    #[test]
    fn file_names_from_urls() {
        assert_eq!(
            file_name_from_url(&cc_domain_ranks_url("cc-main-2025-26-nov-dec-jan")).unwrap(),
            "cc-main-2025-26-nov-dec-jan-domain-ranks.txt.gz"
        );
        assert_eq!(
            file_name_from_url("https://example.org/a/b.txt?x=1#frag").unwrap(),
            "b.txt"
        );
        assert!(file_name_from_url("https://example.org/dir/").is_err());
        assert!(file_name_from_url("https://example.org").is_err());
        assert!(file_name_from_url("not a url").is_err());
    }

    #[test]
    fn part_files_sit_next_to_the_destination() {
        assert_eq!(
            part_path(Path::new("data/ranks.txt.gz")),
            PathBuf::from("data/ranks.txt.gz.part")
        );
    }

    #[test]
    fn progress_every_five_percent_or_64_mb() {
        let mut known = Progress::new(Some(1000));
        let reports: Vec<u64> = (1..=100)
            .map(|i| i * 10)
            .filter(|&w| known.should_report(w))
            .collect();
        assert_eq!(reports, (1..=20).map(|i| i * 50).collect::<Vec<u64>>());

        // A big chunk that jumps several steps logs once.
        let mut jump = Progress::new(Some(1000));
        assert!(jump.should_report(730));
        assert!(!jump.should_report(749));
        assert!(jump.should_report(750));

        let mb = 1 << 20;
        let mut unknown = Progress::new(None);
        assert!(!unknown.should_report(63 * mb));
        assert!(unknown.should_report(64 * mb));
        assert!(!unknown.should_report(100 * mb));
        assert!(unknown.should_report(128 * mb));

        let mut tiny = Progress::new(Some(3));
        assert!(tiny.should_report(1));
        assert!(tiny.should_report(3));
    }

    #[test]
    fn client_builds() {
        http_client().unwrap();
    }

    /// Serves one canned HTTP response on a loopback port; no outside network.
    async fn serve_once(response: Vec<u8>) -> String {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let _ = socket.write_all(&response).await;
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}/files/data.bin")
    }

    fn loopback_client() -> reqwest::Client {
        client_builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn downloads_via_part_file() {
        let body = vec![b'x'; 100_000];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let url = serve_once(response).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("nested/data.bin");
        let written = download_to_file(&loopback_client(), &url, &dest)
            .await
            .unwrap();
        assert_eq!(written, body.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!part_path(&dest).exists());
    }

    #[tokio::test]
    async fn downloads_without_content_length() {
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nstreamed until close".to_vec(),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("data.bin");
        let written = download_to_file(&loopback_client(), &url, &dest)
            .await
            .unwrap();
        assert_eq!(written, 20);
        assert_eq!(std::fs::read(&dest).unwrap(), b"streamed until close");
    }

    #[tokio::test]
    async fn non_success_status_is_an_error() {
        let url = serve_once(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found"
                .to_vec(),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("data.bin");
        let err = download_to_file(&loopback_client(), &url, &dest)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("404"), "{err:#}");
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
    }

    #[tokio::test]
    async fn truncated_body_is_an_error_and_leaves_no_files() {
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\nshort".to_vec(),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("data.bin");
        assert!(download_to_file(&loopback_client(), &url, &dest)
            .await
            .is_err());
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
    }

    /// Serves `responses` on a loopback port, one per connection, and
    /// keeps each request's head (lowercased) in the returned list.
    async fn serve_each(
        responses: Vec<Vec<u8>>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&requests);
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&request).to_lowercase();
                seen.lock().unwrap().push(head);
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{addr}/files/data.bin"), requests)
    }

    /// A response with `head` lines, then `body`.
    fn response(head: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes = format!("{head}\r\nConnection: close\r\n\r\n").into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    fn numbered(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn a_download_that_breaks_off_carries_on() {
        let body = numbered(1000);
        let (url, requests) = serve_each(vec![
            // Says 1000 bytes, sends 400 and hangs up.
            response(
                "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nAccept-Ranges: bytes\r\nETag: \"v1\"",
                &body[..400],
            ),
            response(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: 600\r\n\
                 Content-Range: bytes 400-999/1000",
                &body[400..],
            ),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("data.bin");
        let written = download_to_file(&loopback_client(), &url, &dest)
            .await
            .unwrap();
        assert_eq!(written, 1000);
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!part_path(&dest).exists());
        assert!(!part_info_path(&dest).exists());
        let requests = requests.lock().unwrap();
        assert!(!requests[0].contains("range:"), "{}", requests[0]);
        assert!(
            requests[1].contains("\r\nrange: bytes=400-\r\n"),
            "{}",
            requests[1]
        );
        assert!(
            requests[1].contains("\r\nif-range: \"v1\"\r\n"),
            "{}",
            requests[1]
        );
    }

    #[tokio::test]
    async fn a_part_file_is_carried_on_by_a_later_download() {
        let body = numbered(1000);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("data.bin");
        let resume = |url: &str| {
            std::fs::write(part_path(&dest), &body[..300]).unwrap();
            let info = PartInfo {
                url: url.to_string(),
                version: "Mon, 05 Oct 2026 00:00:00 GMT".into(),
                total: Some(1000),
            };
            std::fs::write(part_info_path(&dest), serde_json::to_vec(&info).unwrap()).unwrap();
        };
        let (url, requests) = serve_each(vec![response(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 300-999/1000\r\n\
             Content-Length: 700",
            &body[300..],
        )])
        .await;
        resume(&url);
        download_to_file(&loopback_client(), &url, &dest)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let head = requests.lock().unwrap()[0].clone();
        assert!(head.contains("\r\nrange: bytes=300-\r\n"), "{head}");
        assert!(head.contains("if-range: mon, 05 oct 2026"), "{head}");

        // The file changed since: the server sends it whole, which replaces
        // the bytes kept.
        let changed = vec![b'n'; 500];
        let (url, _) = serve_each(vec![response(
            "HTTP/1.1 200 OK\r\nContent-Length: 500",
            &changed,
        )])
        .await;
        resume(&url);
        download_to_file(&loopback_client(), &url, &dest)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), changed);
        assert!(!part_info_path(&dest).exists());

        // A part of some other address is not carried on.
        let (url, requests) = serve_each(vec![response(
            "HTTP/1.1 200 OK\r\nContent-Length: 500",
            &changed,
        )])
        .await;
        resume("http://example.com/other.bin");
        download_to_file(&loopback_client(), &url, &dest)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), changed);
        assert!(!requests.lock().unwrap()[0].contains("range:"));
    }

    #[test]
    fn top_file_names() {
        assert_eq!(
            cc_domain_ranks_top_file_name(
                &cc_domain_ranks_url("cc-main-2025-26-nov-dec-jan"),
                1_000_000
            )
            .unwrap(),
            "cc-main-2025-26-nov-dec-jan-domain-ranks-top1000000.txt"
        );
        assert_eq!(
            cc_domain_ranks_top_file_name("https://example.org/r.txt", 5).unwrap(),
            "r-top5.txt"
        );
        assert_eq!(
            cc_domain_ranks_top_file_name("https://example.org/ranks", 5).unwrap(),
            "ranks-top5.txt"
        );
        assert!(cc_domain_ranks_top_file_name("https://example.org/dir/", 5).is_err());
    }

    const RANKS_HEADER: &str =
        "#harmonicc_pos\t#harmonicc_val\t#pr_pos\t#pr_val\t#host_rev\t#n_hosts\n";

    /// A ranks file with `rows` rows, some ending in CRLF, with blank lines
    /// before the header and among the rows.
    fn ranks_text(rows: usize) -> String {
        let mut text = format!("\n{RANKS_HEADER}");
        for i in 1..=rows {
            let end = if i % 3 == 0 { "\r\n" } else { "\n" };
            text.push_str(&format!("{i}\t1.0E7\t{i}\t0.001\tcom.site{i}\t{i}{end}"));
            if i % 50 == 0 {
                text.push_str(" \t \n");
            }
        }
        text
    }

    /// What [`TopLines`] should keep of `text`: everything up to the end of
    /// the line holding the `wanted`th row after the header.
    fn expected_top(text: &str, wanted: usize) -> String {
        let mut kept = String::new();
        let mut header_seen = false;
        let mut rows = 0;
        for line in text.split_inclusive('\n') {
            kept.push_str(line);
            if line.trim().is_empty() {
                continue;
            }
            if header_seen {
                rows += 1;
            }
            header_seen = true;
            if rows >= wanted {
                break;
            }
        }
        kept
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    /// `data` gzipped as `members` members, split at arbitrary byte offsets
    /// (mid-line, too), the way Common Crawl writes some of its files.
    fn gzip_members(data: &[u8], members: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..members {
            let (from, to) = (data.len() * i / members, data.len() * (i + 1) / members);
            out.extend(gzip(&data[from..to]));
        }
        out
    }

    /// Feeds `body` to a [`TopLines`] in pieces of `chunk` bytes, as a
    /// download would: stops once it is done, finishes at the end of the body.
    fn run_top_lines(body: &[u8], chunk: usize, wanted: usize) -> io::Result<(String, TopLines)> {
        let mut top = TopLines::new(wanted);
        let mut kept = Vec::new();
        let mut pieces = body.chunks(chunk);
        while !top.is_done() {
            match pieces.next() {
                Some(piece) => top.write(piece)?,
                None => {
                    top.finish()?;
                    break;
                }
            }
            kept.extend(top.take_output());
        }
        kept.extend(top.take_output());
        Ok((String::from_utf8(kept).unwrap(), top))
    }

    #[test]
    fn top_lines_keeps_the_header_and_the_first_rows() {
        let text = ranks_text(300);
        let bodies = [
            ("gzip", gzip(text.as_bytes())),
            ("3 gzip members", gzip_members(text.as_bytes(), 3)),
            ("plain", text.clone().into_bytes()),
        ];
        for (kind, body) in &bodies {
            for chunk in [1, 2, 3, 7, 64, 997, usize::MAX] {
                for wanted in [0, 1, 7, 50, 299, 300, 301, 5_000] {
                    let what = format!("{kind}, {chunk}-byte chunks, {wanted} rows");
                    let (kept, top) = run_top_lines(body, chunk, wanted).expect(&what);
                    assert_eq!(kept, expected_top(&text, wanted), "{what}");
                    assert_eq!(top.rows(), wanted.min(300), "{what}");
                    assert_eq!(top.is_done(), wanted <= 300, "{what}");
                    assert!(top.has_header(), "{what}");
                }
            }
        }
        // Nothing past the last row wanted is kept, not even the next line break.
        let (kept, _) = run_top_lines(&gzip(text.as_bytes()), 64, 2).unwrap();
        assert_eq!(
            kept,
            format!("\n{RANKS_HEADER}1\t1.0E7\t1\t0.001\tcom.site1\t1\n2\t1.0E7\t2\t0.001\tcom.site2\t2\n")
        );
    }

    #[test]
    fn top_lines_reads_what_load_cc_domain_ranks_reads() {
        let text = ranks_text(120);
        let dir = tempfile::tempdir().unwrap();
        let whole = dir.path().join("whole.txt");
        std::fs::write(&whole, &text).unwrap();
        let (kept, _) = run_top_lines(&gzip_members(text.as_bytes(), 4), 100, 75).unwrap();
        let top = dir.path().join("top.txt");
        std::fs::write(&top, kept).unwrap();
        let expected = crate::load_cc_domain_ranks(&whole, Some(75)).unwrap();
        assert_eq!(expected.len(), 75);
        assert_eq!(crate::load_cc_domain_ranks(&top, None).unwrap(), expected);
    }

    #[test]
    fn top_lines_rejects_a_gzip_body_cut_short() {
        let text = ranks_text(300);
        let body = gzip_members(text.as_bytes(), 2);
        let cut = &body[..body.len() - 20];
        for chunk in [1, 64, usize::MAX] {
            assert!(run_top_lines(cut, chunk, 1_000).is_err(), "{chunk}");
            // A cut that leaves enough rows is fine: the rest is never read.
            let (kept, top) = run_top_lines(cut, chunk, 100).unwrap();
            assert!(top.is_done());
            assert_eq!(kept, expected_top(&text, 100));
        }
        assert!(run_top_lines(b"\x1f\x8b not really gzip", 4, 10).is_err());
    }

    #[test]
    fn top_lines_short_bodies() {
        let (kept, top) = run_top_lines(b"", 1, 10).unwrap();
        assert_eq!(
            (kept.as_str(), top.has_header(), top.is_done()),
            ("", false, false)
        );
        let (kept, top) = run_top_lines(b"\n \n", 1, 10).unwrap();
        assert_eq!((kept.as_str(), top.has_header()), ("\n \n", false));
        let (kept, top) = run_top_lines(b"h", 1, 0).unwrap();
        assert_eq!(
            (kept.as_str(), top.has_header(), top.is_done()),
            ("h", true, true)
        );
        // A last row without a line break counts.
        let (kept, top) = run_top_lines(b"h\nr1\nr2", 1, 2).unwrap();
        assert_eq!(
            (kept.as_str(), top.rows(), top.is_done()),
            ("h\nr1\nr2", 2, true)
        );
    }

    /// Serves one response on a loopback port, at a URL whose file name is
    /// `name`: `head`, then the pieces of `body` until they run out or the
    /// client hangs up. The task returns how many pieces were sent.
    async fn serve_pieces<I>(
        name: &str,
        head: String,
        body: I,
    ) -> (String, tokio::task::JoinHandle<usize>)
    where
        I: IntoIterator<Item = Vec<u8>> + Send + 'static,
        I::IntoIter: Send,
    {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return 0;
            };
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => return 0,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            if socket.write_all(head.as_bytes()).await.is_err() {
                return 0;
            }
            let mut sent = 0;
            for piece in body {
                if socket.write_all(&piece).await.is_err() {
                    return sent;
                }
                sent += 1;
            }
            let _ = socket.shutdown().await;
            sent
        });
        (format!("http://{addr}/graph/{name}"), task)
    }

    #[tokio::test]
    async fn downloads_only_the_top_of_a_ranks_file() {
        // An endless ranks file: the header, then the same 1,000 rows over
        // and over, one gzip member at a time, with no length given.
        let rows: String = ranks_text(1_000)
            .lines()
            .skip(2)
            .map(|l| format!("{l}\n"))
            .collect();
        let member = gzip(rows.as_bytes());
        const MEMBERS: usize = 100_000;
        let body = std::iter::once(gzip(RANKS_HEADER.as_bytes()))
            .chain(std::iter::repeat_n(member, MEMBERS));
        let head = "HTTP/1.1 200 OK\r\nContent-Type: application/gzip\r\nConnection: close\r\n\r\n";
        let (url, server) = serve_pieces("x-domain-ranks.txt.gz", head.to_string(), body).await;

        let dir = tempfile::tempdir().unwrap();
        let path = tokio::time::timeout(
            Duration::from_secs(60),
            download_cc_domain_ranks_top(&loopback_client(), &url, dir.path(), 2_500),
        )
        .await
        .expect("the download stops by itself")
        .unwrap();
        assert_eq!(path, dir.path().join("x-domain-ranks-top2500.txt"));
        assert!(!part_path(&path).exists());

        let saved = std::fs::read_to_string(&path).unwrap();
        let mut expected = RANKS_HEADER.to_string();
        expected.push_str(&rows);
        expected.push_str(&rows);
        expected.push_str(&expected_top(&format!("h\n{rows}"), 500)[2..]);
        assert_eq!(saved, expected);
        let ranks = crate::load_cc_domain_ranks(&path, None).unwrap();
        assert_eq!(ranks.len(), 2_500);
        assert_eq!(ranks[0].domain, "site1.com");

        // The client hung up long before the server ran out of members.
        let sent = tokio::time::timeout(Duration::from_secs(30), server)
            .await
            .expect("the server notices the client left")
            .unwrap();
        assert!(sent < MEMBERS, "sent all {sent} members");
    }

    #[tokio::test]
    async fn saves_a_short_ranks_file_whole() {
        let text = ranks_text(40);
        let body = gzip_members(text.as_bytes(), 2);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let (url, _) = serve_pieces("r.txt.gz", head, [body]).await;
        let dir = tempfile::tempdir().unwrap();
        let path =
            download_cc_domain_ranks_top(&loopback_client(), &url, &dir.path().join("seed"), 1_000)
                .await
                .unwrap();
        assert_eq!(path, dir.path().join("seed/r-top1000.txt"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[tokio::test]
    async fn cut_or_failed_ranks_downloads_leave_no_files() {
        let text = ranks_text(40);
        let body = gzip(text.as_bytes());
        // Complete as HTTP goes, but the gzip stream stops short.
        let cut = body[..body.len() / 2].to_vec();
        let responses = [
            (
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    cut.len()
                ),
                cut,
            ),
            // The connection drops before the promised length.
            (
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len() + 100
                ),
                body.clone(),
            ),
            (
                "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\n"
                    .to_string(),
                b"not found".to_vec(),
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                Vec::new(),
            ),
        ];
        for (head, body) in responses {
            let (url, _) = serve_pieces("r.txt.gz", head.clone(), [body]).await;
            let dir = tempfile::tempdir().unwrap();
            let err = download_cc_domain_ranks_top(&loopback_client(), &url, dir.path(), 1_000)
                .await
                .unwrap_err();
            assert!(format!("{err:#}").contains(&url), "{err:#}");
            let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
            assert!(left.is_empty(), "{head}: {left:?}");
        }
    }

    fn band(min: u32, max: Option<u32>) -> SitelinkBand {
        SitelinkBand { min, max }
    }

    /// No waits, so that tests of retries run fast.
    fn quick() -> WikidataPacing {
        WikidataPacing {
            pause: Duration::ZERO,
            retry_wait: Duration::from_millis(1),
        }
    }

    #[test]
    fn sitelink_bands_cover_every_count_once() {
        assert_eq!(
            wikidata_sitelink_bands(25),
            [
                band(25, Some(29)),
                band(30, Some(35)),
                band(36, Some(44)),
                band(45, Some(59)),
                band(60, Some(84)),
                band(85, Some(129)),
                band(130, None),
            ]
        );
        assert_eq!(
            wikidata_sitelink_bands(45),
            [
                band(45, Some(59)),
                band(60, Some(84)),
                band(85, Some(129)),
                band(130, None)
            ]
        );
        assert_eq!(
            wikidata_sitelink_bands(100),
            [band(100, Some(129)), band(130, None)]
        );
        assert_eq!(wikidata_sitelink_bands(250), [band(250, None)]);
        // Below 25, every count is a band of its own.
        assert_eq!(
            wikidata_sitelink_bands(22)[..4],
            [
                band(22, Some(22)),
                band(23, Some(23)),
                band(24, Some(24)),
                band(25, Some(29))
            ]
        );
        assert_eq!(wikidata_sitelink_bands(0).len(), 25 + 7);
        for min in [0, 1, 10, 24, 25, 26, 27, 33, 99, 100, 101, 999, u32::MAX] {
            let bands = wikidata_sitelink_bands(min);
            assert_eq!(bands[0].min, min, "{min}");
            assert_eq!(bands.last().unwrap().max, None, "{min}");
            for pair in bands.windows(2) {
                let end = pair[0].max.expect("only the last band is open");
                assert!(pair[0].min <= end, "{min}: {pair:?}");
                assert_eq!(pair[1].min, end + 1, "{min}: {pair:?}");
            }
        }
    }

    #[test]
    fn bands_split_in_halves_down_to_single_counts() {
        assert_eq!(
            band(25, Some(26)).halves(),
            Some((band(25, Some(25)), band(26, Some(26))))
        );
        assert_eq!(
            band(27, Some(29)).halves(),
            Some((band(27, Some(28)), band(29, Some(29))))
        );
        assert_eq!(
            band(70, Some(99)).halves(),
            Some((band(70, Some(84)), band(85, Some(99))))
        );
        assert_eq!(band(25, Some(25)).halves(), None);
        // A band without an upper end is cut at twice its start.
        assert_eq!(
            band(100, None).halves(),
            Some((band(100, Some(199)), band(200, None)))
        );
        assert_eq!(
            band(0, None).halves(),
            Some((band(0, Some(0)), band(1, None)))
        );
        assert_eq!(band(OPEN_BAND_SPLIT_LIMIT, None).halves(), None);
        assert_eq!(band(u32::MAX, None).halves(), None);
        assert_eq!(
            band(u32::MAX - 1, Some(u32::MAX)).halves(),
            Some((
                band(u32::MAX - 1, Some(u32::MAX - 1)),
                band(u32::MAX, Some(u32::MAX))
            ))
        );

        // Halving again and again covers the same counts, in order, and
        // ends in single counts (or an open band too high to split).
        for start in [
            band(25, Some(26)),
            band(70, Some(99)),
            band(100, None),
            band(0, None),
        ] {
            let mut todo = vec![start];
            let mut leaves = Vec::new();
            while let Some(b) = todo.pop() {
                match b.halves() {
                    Some((lower, upper)) => {
                        todo.push(upper);
                        todo.push(lower);
                    }
                    None => leaves.push(b),
                }
            }
            assert_eq!(leaves[0].min, start.min, "{start:?}");
            assert_eq!(leaves.last().unwrap().max, start.max, "{start:?}");
            for pair in leaves.windows(2) {
                assert_eq!(pair[0].max, Some(pair[0].min), "{start:?}: {pair:?}");
                assert_eq!(pair[1].min, pair[0].min + 1, "{start:?}: {pair:?}");
            }
        }
        // Even the widest band reaches a single count within 32 halvings.
        let mut lowest = band(0, Some(u32::MAX));
        let mut halvings = 0;
        while let Some((lower, _)) = lowest.halves() {
            lowest = lower;
            halvings += 1;
        }
        assert_eq!((lowest, halvings), (band(0, Some(0)), 32));
    }

    #[test]
    fn one_count_bands_start_from_the_count() {
        assert_eq!(
            band(20, Some(20)).sparql_query(),
            "SELECT ?item ?itemLabel ?website WHERE { ?item wikibase:sitelinks 20 . ?item wdt:P856 ?website . SERVICE wikibase:label { bd:serviceParam wikibase:language \"en,mul\". } }"
        );
    }

    #[test]
    fn the_bulk_query_needs_no_label_service() {
        let query = bulk_official_sites_query(3);
        assert!(query.starts_with(WIKIDATA_PREFIXES), "{query}");
        assert!(query.contains("FILTER(xsd:integer(?s) >= 3)"), "{query}");
        assert!(query.contains("COALESCE(?en, ?mul, STRAFTER(STR(?item), \"/entity/\"))"));
        assert!(!query.contains("SERVICE"));
    }

    /// `n` made-up official websites.
    fn many_rows(n: usize) -> Vec<(String, String, String)> {
        (0..n)
            .map(|i| {
                (
                    format!("Q{i}"),
                    format!("Item {i}"),
                    format!("https://item{i}.org/"),
                )
            })
            .collect()
    }

    fn sparql_ok(rows: &[(String, String, String)]) -> Vec<u8> {
        let answer = sparql_answer(
            rows.iter()
                .map(|(item, label, website)| (item.as_str(), label.as_str(), website.as_str())),
        );
        http_response("200 OK", &[], answer.as_bytes())
    }

    #[tokio::test]
    async fn the_mirror_lists_every_site_in_one_query() {
        let rows = many_rows(MIN_BULK_ROWS + 5);
        let (url, asked) = sparql_endpoint(move |_, n| match n {
            0 => http_response("429 Too Many Requests", &[], b"slow down"),
            _ => sparql_ok(&rows),
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = download_wikidata_official_sites_with(
            &loopback_client(),
            Some(&url),
            "http://127.0.0.1:9/never",
            dir.path(),
            3,
            quick(),
        )
        .await
        .unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text.lines().count(), MIN_BULK_ROWS + 6);
        assert!(text.contains("Q7\tItem 7\thttps://item7.org/\n"));
        assert_eq!(asked.bands(), [band(3, None), band(3, None)]);
    }

    #[tokio::test]
    async fn without_the_mirror_wikidata_is_asked_from_25_up() {
        let (mirror, _) = sparql_endpoint(|_, _| http_response("502 Bad Gateway", &[], b"")).await;
        let (main, asked) = sparql_endpoint(|_, _| sparql_ok(&many_rows(2))).await;
        let dir = tempfile::tempdir().unwrap();
        download_wikidata_official_sites_with(
            &loopback_client(),
            Some(&mirror),
            &main,
            dir.path(),
            3,
            quick(),
        )
        .await
        .unwrap();
        assert_eq!(
            asked.bands(),
            wikidata_sitelink_bands(ENDPOINT_MIN_SITELINKS)
        );
    }

    #[tokio::test]
    async fn a_mirror_with_too_few_rows_falls_back_to_the_bands() {
        let (mirror, _) = sparql_endpoint(|_, _| sparql_ok(&many_rows(3))).await;
        let (main, asked) = sparql_endpoint(|_, _| sparql_ok(&many_rows(2))).await;
        let dir = tempfile::tempdir().unwrap();
        // From 130 up there is one band, so one query.
        let path = download_wikidata_official_sites_with(
            &loopback_client(),
            Some(&mirror),
            &main,
            dir.path(),
            130,
            quick(),
        )
        .await
        .unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert_eq!(asked.bands(), [band(130, None)]);
    }

    #[test]
    fn band_queries_and_names() {
        assert_eq!(
            band(25, Some(29)).sparql_query(),
            "SELECT ?item ?itemLabel ?website WHERE { ?item wdt:P856 ?website ; wikibase:sitelinks ?s . FILTER(?s >= 25 && ?s < 30) SERVICE wikibase:label { bd:serviceParam wikibase:language \"en,mul\". } }"
        );
        assert_eq!(
            band(130, None).sparql_query(),
            "SELECT ?item ?itemLabel ?website WHERE { ?item wdt:P856 ?website ; wikibase:sitelinks ?s . FILTER(?s >= 130) SERVICE wikibase:label { bd:serviceParam wikibase:language \"en,mul\". } }"
        );
        assert!(band(0, Some(u32::MAX))
            .sparql_query()
            .contains("FILTER(?s >= 0 && ?s < 4294967296)"));
        assert_eq!(band(100, None).sparql_query(), wikidata_sparql_query(100));
        let names: Vec<String> = [
            band(25, Some(25)),
            band(25, Some(26)),
            band(100, None),
            band(1, Some(1)),
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(
            names,
            [
                "exactly 25 sitelinks",
                "25 to 26 sitelinks",
                "100 or more sitelinks",
                "exactly 1 sitelink"
            ]
        );
    }

    /// What Wikidata's SPARQL endpoint answers for `rows` (item id, label,
    /// website), pretty-printed as it does.
    fn sparql_answer<'a>(rows: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>) -> String {
        let bindings: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|(item, label, website)| {
                serde_json::json!({
                    "item": {"type": "uri", "value": format!("http://www.wikidata.org/entity/{item}")},
                    "website": {"type": "uri", "value": website},
                    "itemLabel": {"xml:lang": "en", "type": "literal", "value": label},
                })
            })
            .collect();
        serde_json::to_string_pretty(&serde_json::json!({
            "head": {"vars": ["item", "itemLabel", "website"]},
            "results": {"bindings": bindings},
        }))
        .unwrap()
    }

    /// `answer` as Wikidata sends it when it stops `query` at its time
    /// limit: broken off inside an item's URL, then the query and the Java
    /// exception of the query service. The quotes in the query close the
    /// broken string, so that a JSON parser trips over what follows, as it
    /// did on the real thing.
    fn stopped_at_time_limit(answer: &str, query: &str) -> String {
        let from = answer.len() * 3 / 5;
        let cut = answer[from..]
            .find("entity/Q")
            .map_or(from, |at| from + at + "entity/Q".len());
        format!(
            "{}SPARQL-QUERY: queryStr={query}\n\
             java.util.concurrent.TimeoutException\n\
             \tat java.util.concurrent.FutureTask.get(FutureTask.java:205)\n\
             \tat com.bigdata.rdf.sail.webapp.BigdataServlet.submitApiTask(BigdataServlet.java:292)\n\
             \tat com.bigdata.rdf.sail.webapp.QueryServlet.doSparqlQuery(QueryServlet.java:678)\n",
            &answer[..cut]
        )
    }

    #[test]
    fn answers_cut_short_are_told_apart_from_bad_ones() {
        let answer = sparql_answer([
            ("Q1", "One", "https://one.org/"),
            ("Q2", "Two", "https://two.org/"),
            ("Q3", "Three", "https://three.org/"),
        ]);
        assert_eq!(parse_sparql_rows(answer.as_bytes()).unwrap().len(), 3);
        let is_cut =
            |body: &str| matches!(parse_sparql_rows(body.as_bytes()), Err(BadAnswer::Cut(_)));
        let is_invalid = |body: &str| {
            matches!(
                parse_sparql_rows(body.as_bytes()),
                Err(BadAnswer::Invalid(_))
            )
        };

        // What Wikidata sends when it stops a query at its time limit.
        let stopped = stopped_at_time_limit(&answer, &wikidata_sparql_query(25));
        let err = serde_json::from_str::<SparqlResponse>(&stopped).unwrap_err();
        assert_eq!(err.classify(), Category::Syntax, "{err}");
        assert!(is_cut(&stopped));
        // JSON that breaks off anywhere, with nothing after it or something else.
        for end in [1, answer.len() / 3, answer.len() / 2, answer.len() - 1] {
            let cut = &answer[..end];
            assert!(is_cut(cut), "{cut}");
            assert!(
                is_cut(&format!("{cut}\n<html>502 Bad Gateway</html>")),
                "{cut}"
            );
        }
        assert!(is_cut(""));
        assert!(is_cut(" \n"));
        // Whole, but followed by the exception.
        assert!(is_cut(&format!(
            "{answer}\njava.util.concurrent.TimeoutException"
        )));
        assert!(is_cut(&format!(
            "{answer}\ncom.bigdata.bop.engine.QueryTimeoutException: Query deadline is expired."
        )));
        // A whole answer that only mentions the exception is fine.
        let mentions = sparql_answer([(
            "Q4",
            "java.util.concurrent.TimeoutException",
            "https://java.org/",
        )]);
        assert_eq!(parse_sparql_rows(mentions.as_bytes()).unwrap().len(), 1);
        // Not SPARQL JSON results at all.
        for body in [
            "<html>Query timeout</html>",
            "{\"head\": {}}",
            "[1, 2]",
            "null",
        ] {
            assert!(is_invalid(body), "{body}");
        }

        // Reported as Wikidata's time limit, not as a JSON error.
        let err = format!(
            "{:#}",
            wikidata_json_to_tsv(stopped.as_bytes()).unwrap_err()
        );
        assert!(
            err.starts_with(
                "Wikidata stopped the query at its time limit: the answer breaks off after"
            ),
            "{err}"
        );
        assert!(err.contains("timeout error"), "{err}");
        assert!(!err.contains("expected"), "{err}");
        let err = format!(
            "{:#}",
            wikidata_json_to_tsv(b"<html>Query timeout</html>").unwrap_err()
        );
        assert!(
            err.contains("not SPARQL JSON results: \"<html>Query timeout</html>\""),
            "{err}"
        );
    }

    fn row(item: &str, label: &str, website: &str) -> WikidataRow {
        WikidataRow {
            item: item.to_string(),
            label: label.to_string(),
            website: website.to_string(),
        }
    }

    #[test]
    fn rows_of_every_band_are_merged_once_each() {
        let mut tsv = OfficialSitesTsv::new();
        let first = vec![
            row("Q1", "One", "https://one.org/"),
            row("Q2", "Two", "https://two.org/"),
            row("Q2", "Two", "https://two.com/"),
        ];
        assert_eq!(tsv.add(first), 3);
        // An item whose sitelinks changed between two queries comes again,
        // maybe under a new label: the first row is kept.
        let second = vec![
            row("Q2", "Two", "https://two.org/"),
            row("Q1", "Renamed", "https://one.org/"),
            row("Q3", "Three", "https://three.org/"),
        ];
        assert_eq!(tsv.add(second), 1);
        assert_eq!(tsv.add(Vec::new()), 0);
        assert_eq!(tsv.rows(), 4);
        assert_eq!(
            tsv.text,
            "item\tlabel\twebsite\n\
             Q1\tOne\thttps://one.org/\n\
             Q2\tTwo\thttps://two.org/\n\
             Q2\tTwo\thttps://two.com/\n\
             Q3\tThree\thttps://three.org/\n"
        );
    }

    #[test]
    fn retry_after_headers() {
        let after = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, value.parse().unwrap());
            retry_after(&headers)
        };
        assert_eq!(after("120"), Some(Duration::from_secs(120)));
        assert_eq!(after("0"), Some(Duration::ZERO));
        let soon = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(90));
        let wait = after(&soon).unwrap();
        assert!(
            (Duration::from_secs(85)..=Duration::from_secs(90)).contains(&wait),
            "{wait:?}"
        );
        let past = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(90));
        assert_eq!(after(&past), Some(Duration::ZERO));
        assert_eq!(after("soon"), None);
        assert_eq!(after("-5"), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    fn http_response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut response = head.into_bytes();
        response.extend_from_slice(body);
        response
    }

    /// The band a query from [`SitelinkBand::sparql_query`] asks for.
    fn band_of_query(query: &str) -> Option<SitelinkBand> {
        if let Some(rest) = query.split_once("?item wikibase:sitelinks ") {
            let n = rest.1.split_once(' ')?.0.parse().ok()?;
            return Some(band(n, Some(n)));
        }
        if let Some(rest) = query.split_once("FILTER(xsd:integer(?s) >= ") {
            return Some(band(rest.1.split_once(')')?.0.parse().ok()?, None));
        }
        let filter = query.split_once("FILTER(")?.1.split_once(')')?.0;
        let (mut min, mut max) = (None, None);
        for part in filter.split("&&").map(str::trim) {
            if let Some(n) = part.strip_prefix("?s >= ") {
                min = Some(n.parse().ok()?);
            } else {
                let n: u64 = part.strip_prefix("?s < ")?.parse().ok()?;
                max = Some(u32::try_from(n - 1).ok()?);
            }
        }
        Some(band(min?, max))
    }

    /// The bands a stand-in endpoint was asked for, in order, with the head
    /// of each request, lowercased.
    #[derive(Debug, Default, Clone)]
    struct Asked(Arc<Mutex<Vec<(SitelinkBand, String)>>>);

    impl Asked {
        fn bands(&self) -> Vec<SitelinkBand> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(band, _)| *band)
                .collect()
        }

        fn heads(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(_, head)| head.clone())
                .collect()
        }
    }

    /// A stand-in for Wikidata's SPARQL endpoint on a loopback port; no
    /// outside network. It reads the band out of each form-encoded query and
    /// answers `respond(band, n)`, where `n` counts the earlier queries for
    /// that same band.
    async fn sparql_endpoint<F>(respond: F) -> (String, Asked)
    where
        F: Fn(SitelinkBand, usize) -> Vec<u8> + Send + 'static,
    {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/sparql", listener.local_addr().unwrap());
        let asked = Asked::default();
        let log = asked.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                let body_start = loop {
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(end + 4);
                    }
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break None,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                };
                let Some(body_start) = body_start else {
                    continue;
                };
                let head = String::from_utf8_lossy(&request[..body_start]).to_ascii_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|n| n.trim().parse().ok())
                    .unwrap_or(0);
                while request.len() < body_start + length {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let query = url::form_urlencoded::parse(&request[body_start..])
                    .find(|(name, _)| name == "query")
                    .map(|(_, query)| query.into_owned())
                    .unwrap_or_default();
                let band = band_of_query(&query).expect("a query for a band of sitelinks");
                let n = {
                    let mut log = log.0.lock().unwrap();
                    let n = log.iter().filter(|(b, _)| *b == band).count();
                    log.push((band, head));
                    n
                };
                let _ = socket.write_all(&respond(band, n)).await;
                let _ = socket.shutdown().await;
            }
        });
        (url, asked)
    }

    /// Rows of a made-up Wikidata as (sitelinks, item, label, website):
    /// 2000 / n items with n sitelinks for n from 20 to 340, one website
    /// each; an item with two websites; and one listed with 26 and with 30
    /// sitelinks, as when an item gains sitelinks between two queries.
    fn made_up_wikidata() -> Vec<(u32, String, String, String)> {
        let mut rows = Vec::new();
        for n in 20..=340u32 {
            for i in 0..2000 / n {
                let id = n * 1000 + i;
                rows.push((
                    n,
                    format!("Q{id}"),
                    format!("Item {id}"),
                    format!("https://item{id}.org/"),
                ));
            }
        }
        for website in ["https://two.org/", "https://two.net/"] {
            rows.push((40, "Q1".into(), "Two websites".into(), website.into()));
        }
        for n in [26, 30] {
            rows.push((n, "Q2".into(), "Moved".into(), "https://moved.org/".into()));
        }
        rows
    }

    fn rows_in(
        rows: &[(u32, String, String, String)],
        band: SitelinkBand,
    ) -> Vec<(&str, &str, &str)> {
        rows.iter()
            .filter(|(n, ..)| band.min <= *n && band.max.is_none_or(|max| *n <= max))
            .map(|(_, item, label, website)| (item.as_str(), label.as_str(), website.as_str()))
            .collect()
    }

    /// The answer of the made-up Wikidata for `band`, gzipped as Wikidata
    /// sends it, whole or, when `stopped`, stopped at the time limit.
    fn made_up_answer(
        data: &[(u32, String, String, String)],
        band: SitelinkBand,
        stopped: bool,
    ) -> Vec<u8> {
        let answer = sparql_answer(rows_in(data, band));
        let body = match stopped {
            true => stopped_at_time_limit(&answer, &band.sparql_query()),
            false => answer,
        };
        http_response(
            "200 OK",
            &[("Content-Encoding", "gzip")],
            &gzip(body.as_bytes()),
        )
    }

    /// Checks that `path` holds every made-up row from `min` sitelinks up, once.
    fn assert_has_every_row(path: &Path, data: &[(u32, String, String, String)], min: u32) {
        let text = std::fs::read_to_string(path).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("item\tlabel\twebsite"));
        let got: Vec<&str> = lines.collect();
        let expected: HashSet<String> = rows_in(data, band(min, None))
            .into_iter()
            .map(|(item, label, website)| format!("{item}\t{label}\t{website}"))
            .collect();
        assert_eq!(got.len(), expected.len(), "a row came twice");
        let got: HashSet<String> = got.into_iter().map(str::to_string).collect();
        assert_eq!(got, expected);
        let sites = crate::load_wikidata_official_sites(path).unwrap();
        assert_eq!(sites.len(), expected.len());
        assert!(sites
            .iter()
            .any(|site| site.item == "Q2" && site.domain == "moved.org"));
    }

    /// The names in `dir`, sorted.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn asks_again_for_bands_wikidata_stops_and_halves_them_last() {
        // Wikidata stops the first query of every band at its time limit,
        // and every query for 36 to 44 sitelinks.
        let data = Arc::new(made_up_wikidata());
        let served = Arc::clone(&data);
        let (url, asked) = sparql_endpoint(move |asked_for, n| {
            let stopped = n == 0 || asked_for == band(36, Some(44));
            made_up_answer(&served, asked_for, stopped)
        })
        .await;

        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed");
        let path =
            download_wikidata_official_sites_paced(&loopback_client(), &url, &seed, 25, quick())
                .await
                .unwrap();
        assert_eq!(path, seed.join(WIKIDATA_FILE_NAME));
        assert_has_every_row(&path, &data, 25);
        // The saved bands are gone with the `.part` file.
        assert_eq!(names_in(&seed), [WIKIDATA_FILE_NAME]);

        // Each band was asked for again after it was stopped; the one that
        // was stopped every time was split once its tries ran out.
        let mut expected = Vec::new();
        for b in wikidata_sitelink_bands(25) {
            if b == band(36, Some(44)) {
                expected.extend([b; BUSY_TRIES as usize]);
                expected.extend([band(36, Some(40)); 2]);
                expected.extend([band(41, Some(44)); 2]);
            } else {
                expected.extend([b; 2]);
            }
        }
        assert_eq!(asked.bands(), expected);
        for head in asked.heads() {
            assert!(head.starts_with("post /sparql "), "{head}");
            assert!(
                head.contains("\r\naccept: application/sparql-results+json\r\n"),
                "{head}"
            );
            assert!(head.contains("\r\naccept-encoding: gzip\r\n"), "{head}");
            assert!(
                head.contains("\r\ncontent-type: application/x-www-form-urlencoded\r\n"),
                "{head}"
            );
            assert!(head.contains("\r\nuser-agent: plumbsearch/"), "{head}");
        }
    }

    #[tokio::test]
    async fn resumes_with_the_bands_an_earlier_try_saved() {
        // The first try: 30 to 35 sitelinks is always stopped, and once it
        // is split, the upper half is refused.
        let data = Arc::new(made_up_wikidata());
        let served = Arc::clone(&data);
        let (url, asked) = sparql_endpoint(move |asked_for, _| {
            if asked_for == band(33, Some(35)) {
                return http_response("403 Forbidden", &[], b"blocked");
            }
            made_up_answer(&served, asked_for, asked_for == band(30, Some(35)))
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let err = download_wikidata_official_sites_paced(
            &loopback_client(),
            &url,
            dir.path(),
            25,
            quick(),
        )
        .await
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.starts_with(
                "asking Wikidata for items with 33 to 35 sitelinks (try again later; the 2 bands \
                 already in are saved): Wikidata query failed: HTTP 403 Forbidden: blocked"
            ),
            "{err}"
        );
        let mut expected = vec![band(25, Some(29))];
        expected.extend([band(30, Some(35)); BUSY_TRIES as usize]);
        expected.extend([band(30, Some(32)), band(33, Some(35))]);
        assert_eq!(asked.bands(), expected);
        let bands_dir = dir.path().join(WIKIDATA_BANDS_DIR_NAME);
        assert_eq!(names_in(dir.path()), [WIKIDATA_BANDS_DIR_NAME]);
        assert_eq!(names_in(&bands_dir), ["25-29.tsv", "30-32.tsv"]);

        // The next try asks only for what is missing.
        let served = Arc::clone(&data);
        let (url, asked) =
            sparql_endpoint(move |band, _| made_up_answer(&served, band, false)).await;
        let path = download_wikidata_official_sites_paced(
            &loopback_client(),
            &url,
            dir.path(),
            25,
            quick(),
        )
        .await
        .unwrap();
        assert_eq!(
            asked.bands(),
            [
                band(33, Some(35)),
                band(36, Some(44)),
                band(45, Some(59)),
                band(60, Some(84)),
                band(85, Some(129)),
                band(130, None),
            ]
        );
        assert_has_every_row(&path, &data, 25);
        assert_eq!(names_in(dir.path()), [WIKIDATA_FILE_NAME]);

        // A try with nothing saved asks for every band.
        let (url, asked) = sparql_endpoint(move |band, _| made_up_answer(&data, band, false)).await;
        download_wikidata_official_sites_paced(&loopback_client(), &url, dir.path(), 25, quick())
            .await
            .unwrap();
        assert_eq!(asked.bands(), wikidata_sitelink_bands(25));
    }

    #[test]
    fn saved_bands_are_found_and_the_missing_ones_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        assert!(saved_bands(&dir.path().join("none"), 25)
            .unwrap()
            .is_empty());
        for name in [
            "20-24.tsv",
            "25-29.tsv",
            "28-31.tsv",
            "36-40.tsv",
            "130-up.tsv",
            "30-32.tsv.part",
            "41-40.tsv",
            "notes.txt",
        ] {
            std::fs::write(dir.path().join(name), "item\tlabel\twebsite\n").unwrap();
        }
        // Below the minimum, overlapping a lower band, or not a band: removed.
        assert_eq!(
            saved_bands(dir.path(), 25).unwrap(),
            [band(25, Some(29)), band(36, Some(40)), band(130, None)]
        );
        assert_eq!(
            names_in(dir.path()),
            ["130-up.tsv", "25-29.tsv", "36-40.tsv"]
        );

        let wanted = wikidata_sitelink_bands(25);
        assert_eq!(missing_bands(&wanted, &[]), wanted);
        assert_eq!(
            missing_bands(
                &wanted,
                &[band(25, Some(29)), band(36, Some(40)), band(130, None)]
            ),
            [
                band(30, Some(35)),
                band(41, Some(44)),
                band(45, Some(59)),
                band(60, Some(84)),
                band(85, Some(129)),
            ]
        );
        // Saved bands that do not line up with the wanted ones.
        assert_eq!(
            missing_bands(
                &wanted,
                &[
                    band(27, Some(32)),
                    band(38, Some(39)),
                    band(41, Some(44)),
                    band(200, None)
                ]
            ),
            [
                band(25, Some(26)),
                band(33, Some(35)),
                band(36, Some(37)),
                band(40, Some(40)),
                band(45, Some(59)),
                band(60, Some(84)),
                band(85, Some(129)),
                band(130, Some(199)),
            ]
        );
        assert!(missing_bands(&wanted, &[band(25, None)]).is_empty());
        assert_eq!(
            missing_bands(&[band(0, Some(u32::MAX))], &[band(5, Some(u32::MAX))]),
            [band(0, Some(4))]
        );

        for b in [band(25, Some(29)), band(7, Some(7)), band(130, None)] {
            assert_eq!(SitelinkBand::from_file_name(&b.file_name()), Some(b));
        }
    }

    #[tokio::test]
    async fn halving_stops_at_the_cap() {
        // Wikidata never answers whole. No waits, so the waits allowed in all
        // never run out.
        let no_waits = WikidataPacing {
            pause: Duration::ZERO,
            retry_wait: Duration::ZERO,
        };
        let (url, asked) = sparql_endpoint(|band, _| {
            let body = stopped_at_time_limit(&answer_for(band), &band.sparql_query());
            http_response("200 OK", &[], body.as_bytes())
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let err = download_wikidata_official_sites_paced(
            &loopback_client(),
            &url,
            dir.path(),
            130,
            no_waits,
        )
        .await
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.starts_with(
                "Wikidata was too busy to answer the query for items with 130 to 194 sitelinks \
                 (the answer breaks off after"
            ),
            "{err}"
        );
        assert!(
            err.ends_with("(tried 3 times)); it is already split 2 times; try again later"),
            "{err}"
        );
        let tries = BUSY_TRIES as usize;
        let mut expected = vec![band(130, None); tries];
        expected.extend(vec![band(130, Some(259)); tries]);
        expected.extend(vec![band(130, Some(194)); tries]);
        assert_eq!(asked.bands(), expected);
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());

        // A single count cannot be split at all.
        let (url, asked) = sparql_endpoint(|_, _| {
            http_response("504 Gateway Timeout", &[], b"upstream timed out")
        })
        .await;
        let err = download_wikidata_official_sites_paced(
            &loopback_client(),
            &url,
            dir.path(),
            24,
            no_waits,
        )
        .await
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "Wikidata was too busy to answer the query for items with exactly 24 sitelinks \
             (HTTP 504 Gateway Timeout: upstream timed out (tried 3 times)); it cannot be \
             split; try again later"
        );
        assert_eq!(asked.bands(), vec![band(24, Some(24)); tries]);
    }

    #[tokio::test]
    async fn waits_stop_at_the_budget() {
        let (url, asked) =
            sparql_endpoint(|_, _| http_response("503 Service Unavailable", &[], b"busy")).await;
        let dir = tempfile::tempdir().unwrap();
        let pacing = quick();
        assert_eq!(pacing.budget(), Duration::from_millis(40));
        let err = download_wikidata_official_sites_paced(
            &loopback_client(),
            &url,
            dir.path(),
            130,
            pacing,
        )
        .await
        .unwrap_err();
        // 1 + 2 ms for the open band, 1 + 2 for its lower half and 1 + 2
        // for that one's lower half, then the halving stops.
        assert_eq!(
            format!("{err:#}"),
            "Wikidata was too busy to answer the query for items with 130 to 194 sitelinks \
             (HTTP 503 Service Unavailable: busy (tried 3 times)); it is already split 2 times; \
             try again later"
        );
        let mut expected = vec![band(130, None); 3];
        expected.extend([band(130, Some(259)); 3]);
        expected.extend([band(130, Some(194)); 3]);
        assert_eq!(asked.bands(), expected);

        // Out of waiting time, a busy band is handed back to be halved
        // rather than failing the download.
        let (url, asked) =
            sparql_endpoint(|_, _| http_response("503 Service Unavailable", &[], b"busy")).await;
        let mut budget = Duration::ZERO;
        let mut queries = 0;
        let failure = query_band(
            &loopback_client(),
            &url,
            band(130, None),
            quick(),
            &mut budget,
            &mut queries,
        )
        .await
        .unwrap_err();
        match failure {
            BandFailure::Busy(why) => assert!(why.contains("no waiting time left"), "{why}"),
            BandFailure::Fatal(err) => panic!("not busy: {err:#}"),
        }
        assert_eq!(asked.bands(), [band(130, None)]);
    }

    #[test]
    fn waits_double_up_to_ten_times_the_first() {
        let pacing = WikidataPacing::default();
        let waits: Vec<u64> = (1..=7).map(|t| pacing.wait_after(t).as_secs()).collect();
        assert_eq!(waits, [30, 60, 120, 240, 300, 300, 300]);
        assert_eq!(pacing.budget(), Duration::from_secs(20 * 60));
        assert_eq!(pacing.rate_limit_wait(), Duration::from_secs(60));
        assert_eq!(pacing.wait_after(u32::MAX), Duration::from_secs(300));
    }

    /// The made-up rows of [`retries_rate_limits_and_server_errors`].
    fn answer_for(band: SitelinkBand) -> String {
        let rows = [
            (150, "Q150", "One fifty", "https://one-fifty.org/"),
            (250, "Q250", "Two fifty", "https://two-fifty.org/"),
        ];
        sparql_answer(
            rows.into_iter()
                .filter(|(n, ..)| band.min <= *n && band.max.is_none_or(|max| *n <= max))
                .map(|(_, item, label, website)| (item, label, website)),
        )
    }

    #[tokio::test]
    async fn retries_rate_limits_and_server_errors() {
        let (url, asked) = sparql_endpoint(|band, n| match n {
            // Without Retry-After: at least twice the first wait.
            0 => http_response("429 Too Many Requests", &[], b"slow down"),
            1 => http_response("504 Gateway Timeout", &[], b"upstream timed out"),
            // Stopped at the time limit before the answer started.
            2 => http_response(
                "500 Internal Server Error",
                &[],
                stopped_at_time_limit("", &band.sparql_query()).as_bytes(),
            ),
            3 => http_response("502 Bad Gateway", &[], b"bad gateway"),
            4 => http_response(
                "429 Too Many Requests",
                &[("Retry-After", "1")],
                b"slow down",
            ),
            _ => http_response("200 OK", &[], answer_for(band).as_bytes()),
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let pacing = WikidataPacing {
            pause: Duration::ZERO,
            retry_wait: Duration::from_millis(50),
        };
        let path = download_wikidata_official_sites_paced(
            &loopback_client(),
            &url,
            dir.path(),
            130,
            pacing,
        )
        .await
        .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(1800),
            "waited 100 (not 50), 100, 200 and 400 ms, then as long as Retry-After asked"
        );
        assert_eq!(asked.bands(), [band(130, None); 6]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "item\tlabel\twebsite\n\
             Q150\tOne fifty\thttps://one-fifty.org/\n\
             Q250\tTwo fifty\thttps://two-fifty.org/\n"
        );
    }

    #[tokio::test]
    async fn an_answer_the_connection_cuts_short_is_asked_for_again() {
        let (url, asked) = sparql_endpoint(|band, n| {
            let answer = answer_for(band);
            if n > 0 {
                return http_response("200 OK", &[], answer.as_bytes());
            }
            // Promises more than it sends, then hangs up.
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                answer.len() + 1000
            )
            .into_bytes();
            response.extend_from_slice(answer.as_bytes());
            response
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = download_wikidata_official_sites_paced(
            &loopback_client(),
            &url,
            dir.path(),
            130,
            quick(),
        )
        .await
        .unwrap();
        assert_eq!(asked.bands(), [band(130, None); 2]);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
    }

    #[tokio::test]
    async fn gives_up_on_client_errors_bad_answers_and_long_waits() {
        let cases = [
            (
                http_response("400 Bad Request", &[], b"bad query"),
                1,
                "Wikidata query failed: HTTP 400 Bad Request: bad query",
            ),
            (
                http_response("502 Bad Gateway", &[], b"down"),
                WIKIDATA_TRIES as usize,
                "HTTP 502 Bad Gateway: down (tried 6 times)",
            ),
            (
                http_response("429 Too Many Requests", &[("Retry-After", "3600")], b""),
                1,
                "the answer asks to wait 3600 seconds before the next query",
            ),
            (
                http_response("200 OK", &[], b"<html>Sign in to the Wi-Fi</html>"),
                1,
                "not SPARQL JSON results",
            ),
        ];
        for (response, requests, expected) in cases {
            let (url, asked) = sparql_endpoint(move |_, _| response.clone()).await;
            let dir = tempfile::tempdir().unwrap();
            let err = download_wikidata_official_sites_paced(
                &loopback_client(),
                &url,
                dir.path(),
                130,
                quick(),
            )
            .await
            .unwrap_err();
            let err = format!("{err:#}");
            assert!(
                err.starts_with(
                    "asking Wikidata for items with 130 or more sitelinks (try again later): "
                ),
                "{err}"
            );
            assert!(err.contains(expected), "{err}");
            assert_eq!(asked.bands().len(), requests, "{err}");
            assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        }
    }
}
