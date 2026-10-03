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
use reqwest::header::{HeaderMap, ACCEPT, CONTENT_TYPE, RETRY_AFTER};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::error::Category;
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

/// Streams `url` into `dest` (via a `.part` file renamed when complete),
/// logging progress. Fails on non-2xx responses. Returns bytes written.
///
/// Creates `dest`'s parent directory, replaces an existing `dest`, and
/// removes the `.part` file again when the download fails.
pub async fn download_to_file(client: &reqwest::Client, url: &str, dest: &Path) -> Result<u64> {
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("downloading {url} failed: HTTP {status}");
    }
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let part = part_path(dest);
    let total = response.content_length();
    info!(
        "downloading {url} to {} ({})",
        dest.display(),
        total.map_or_else(|| "size unknown".to_string(), megabytes)
    );
    let written = match stream_body(&mut response, &part, total).await {
        Ok(written) => written,
        Err(err) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(err.context(format!("downloading {url}")));
        }
    };
    tokio::fs::rename(&part, dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!("saved {} ({})", dest.display(), megabytes(written));
    Ok(written)
}

/// Writes the response body to `part`, returning the bytes written.
async fn stream_body(
    response: &mut reqwest::Response,
    part: &Path,
    total: Option<u64>,
) -> Result<u64> {
    let mut file = tokio::fs::File::create(part)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    let mut progress = Progress::new(total);
    let mut written = 0u64;
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
    tokio::fs::create_dir_all(dir)
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
            let _ = tokio::fs::remove_file(&part).await;
            return Err(err.context(format!("downloading {url}")));
        }
    };
    // Dropping the response closes the connection: the rest is never fetched.
    drop(response);
    tokio::fs::rename(&part, &dest)
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
    let mut file = tokio::fs::File::create(part)
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
/// another, and a band that Wikidata still stops at its time limit is asked
/// for again in halves; see [`download_wikidata_official_sites_paced`]. The
/// rows of every band go into the one file.
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
    /// The wait before each query but the first. Wikidata's query service
    /// limits the query time each client uses per minute, and answers HTTP
    /// 429 to clients over the limit.
    pub pause: Duration,
    /// The wait before trying a query again after HTTP 429 or 5xx without a
    /// `Retry-After`, or after a failed connection. It doubles with each try
    /// of the same query.
    pub retry_wait: Duration,
}

impl Default for WikidataPacing {
    /// 5 seconds between two queries; 5, 10, then 20 seconds before tries again.
    fn default() -> Self {
        WikidataPacing {
            pause: Duration::from_secs(5),
            retry_wait: Duration::from_secs(5),
        }
    }
}

/// Tries of one query that gets HTTP 429 or 5xx, or cannot connect.
const WIKIDATA_TRIES: u32 = 4;

/// The longest `Retry-After` a download waits for; an answer that asks for
/// more fails the download, to be tried again much later.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// A query without a whole answer after this long is treated like one that
/// Wikidata stopped at its 60-second limit.
const WIKIDATA_QUERY_TIMEOUT: Duration = Duration::from_secs(5 * 60);

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
/// lowest first, waiting `pacing.pause` before each query but the first,
/// and each answer is logged. An answer must be whole: when Wikidata stops a
/// query at its time limit, the answer still comes as HTTP 200, but its JSON
/// breaks off and the query service's Java exception follows. A band whose
/// answer breaks off, or ends in that exception (also as HTTP 500), or that
/// has no whole answer within 5 minutes, is asked for again in two halves
/// ([`SitelinkBand::halves`]), and so on down to a single count of
/// sitelinks. A single count that is still cut short fails the download.
///
/// HTTP 429 and 5xx, and failed connections, are tried again up to 4 times
/// per query, after the wait the answer's `Retry-After` asks for (at most 10
/// minutes; an answer that asks for more fails the download) or else
/// `pacing.retry_wait`, doubling. Other failures, such as HTTP 4xx or an
/// answer that is not SPARQL JSON, fail the download at once. The file is
/// written only once every band is in, through a `.part` file renamed when
/// complete.
pub async fn download_wikidata_official_sites_paced(
    client: &reqwest::Client,
    endpoint: &str,
    dir: &Path,
    min_sitelinks: u32,
    pacing: WikidataPacing,
) -> Result<PathBuf> {
    let bands = wikidata_sitelink_bands(min_sitelinks);
    info!(
        "asking Wikidata for official websites of items with at least {min_sitelinks} sitelinks, \
         in {} queries by number of sitelinks",
        bands.len()
    );
    let started = Instant::now();
    // The bands still to ask for, the next one last.
    let mut todo: Vec<SitelinkBand> = bands.into_iter().rev().collect();
    let mut tsv = OfficialSitesTsv::new();
    let mut queries = 0u32;
    while let Some(band) = todo.pop() {
        if queries > 0 {
            tokio::time::sleep(pacing.pause).await;
        }
        queries += 1;
        let asked = Instant::now();
        let answer = query_band(client, endpoint, band, pacing)
            .await
            .with_context(|| format!("asking Wikidata for items with {band}"))?;
        match answer {
            Answer::Rows(rows) => {
                let got = rows.len();
                let repeated = got - tsv.add(rows);
                let repeated = match repeated {
                    0 => String::new(),
                    n => format!(" ({n} of them already in)"),
                };
                info!(
                    "Wikidata: {got} official websites of items with {band} in {:.1} s{repeated}; \
                     {} in all, {} queries to go",
                    asked.elapsed().as_secs_f64(),
                    tsv.rows(),
                    todo.len()
                );
            }
            Answer::Cut(why) => {
                let Some((lower, upper)) = band.halves() else {
                    bail!(
                        "Wikidata stopped the query for items with {band} at its time limit \
                         ({why}), and that cannot be split any further; try again later, or ask \
                         for items with more sitelinks"
                    );
                };
                warn!(
                    "Wikidata stopped the query for items with {band} at its time limit ({why}); \
                     asking for items with {lower} and with {upper} separately"
                );
                todo.push(upper);
                todo.push(lower);
            }
        }
    }

    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(WIKIDATA_FILE_NAME);
    let part = part_path(&dest);
    tokio::fs::write(&part, tsv.text.as_bytes())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    tokio::fs::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!(
        "wrote {} official websites to {} after {queries} queries in {:.0} s",
        tsv.rows(),
        dest.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(dest)
}

/// A range of sitelink counts that one Wikidata query asks for: `min` to
/// `max`, both included, or `min` and up when `max` is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SitelinkBand {
    pub min: u32,
    pub max: Option<u32>,
}

/// Where the bands of [`wikidata_sitelink_bands`] start from 25 sitelinks up.
const WIKIDATA_BAND_STARTS: [u32; 8] = [25, 27, 30, 34, 40, 50, 70, 100];

/// An open band, `min` sitelinks or more, is not split once `min` is this
/// high: no Wikidata item has nearly that many sitelinks.
const OPEN_BAND_SPLIT_LIMIT: u32 = 1_000;

/// The bands of sitelink counts that [`download_wikidata_official_sites`]
/// asks for, lowest first. Together they cover every count from
/// `min_sitelinks` up, each once: below 25 each count is a band of its own,
/// then come 25-26, 27-29, 30-33, 34-39, 40-49, 50-69, 70-99 and 100 or more.
///
/// From 25 up there were 131,386 rows in October 2026, too many for one
/// query to list within Wikidata's 60-second limit. Most items have few
/// sitelinks, so the bands widen as the counts grow, to hold similar shares:
/// if the rows with at least n sitelinks thin out like n^-1.3 to n^-2 (only
/// the total is known), each band holds 8,000 to 22,000 of them, and each
/// count from 15 to 24 holds 9,000 to 33,000. A band that turns out too big
/// all the same is split while downloading.
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
        let filter = match self.max {
            Some(max) => format!("?sitelinks >= {} && ?sitelinks <= {max}", self.min),
            None => format!("?sitelinks >= {}", self.min),
        };
        format!(
            "SELECT ?item ?itemLabel ?website WHERE {{ ?item wdt:P856 ?website ; wikibase:sitelinks ?sitelinks . FILTER({filter}) SERVICE wikibase:label {{ bd:serviceParam wikibase:language \"en,mul\". }} }}"
        )
    }

    /// The band in two, lower half first, to ask for once Wikidata stopped
    /// the band's query at its time limit; `None` when it cannot be split. A
    /// band with an upper end is cut in the middle, down to single counts.
    /// One without, `min` and up, is cut into `min` to `2 * min - 1` and
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
}

impl fmt::Display for SitelinkBand {
    /// `exactly 25 sitelinks`, `25 to 26 sitelinks` or `100 or more sitelinks`.
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

/// What the query for one band came to.
#[derive(Debug)]
enum Answer {
    /// The whole answer.
    Rows(Vec<WikidataRow>),
    /// Stopped at the time limit, or cut short some other way: ask for
    /// less. Says what was seen.
    Cut(String),
}

/// Why one try of a query has no [`Answer`].
#[derive(Debug)]
enum Failed {
    /// Worth another try: after `after`, when the server says how long to wait.
    Again {
        why: String,
        after: Option<Duration>,
    },
    /// Not worth another try.
    Fatal(anyhow::Error),
}

/// Asks for one band, trying again after HTTP 429 or 5xx (other than the
/// query service's timeout error, which is an [`Answer::Cut`]) and after a
/// failed connection; see [`download_wikidata_official_sites_paced`].
async fn query_band(
    client: &reqwest::Client,
    endpoint: &str,
    band: SitelinkBand,
    pacing: WikidataPacing,
) -> Result<Answer> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("query", &band.sparql_query())
        .finish();
    let mut backoff = pacing.retry_wait;
    let mut tries = 0;
    loop {
        tries += 1;
        let (why, after) = match try_query(client, endpoint, &form).await {
            Ok(answer) => return Ok(answer),
            Err(Failed::Fatal(err)) => return Err(err),
            Err(Failed::Again { why, after }) => (why, after),
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
            None => backoff,
        };
        warn!(
            "Wikidata query for items with {band}: {why}; trying again in {:.1} s",
            wait.as_secs_f64()
        );
        tokio::time::sleep(wait).await;
        backoff = backoff.saturating_mul(2);
    }
}

/// Sends a query once and reads its answer.
async fn try_query(client: &reqwest::Client, endpoint: &str, form: &str) -> Result<Answer, Failed> {
    let sent = client
        .post(endpoint)
        .header(ACCEPT, "application/sparql-results+json")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .timeout(WIKIDATA_QUERY_TIMEOUT)
        .body(form.to_string())
        .send()
        .await;
    let response = match sent {
        Ok(response) => response,
        Err(err) if err.is_timeout() && !err.is_connect() => {
            return Ok(Answer::Cut(no_answer_in_time()));
        }
        Err(err) if err.is_builder() => {
            let err = anyhow::Error::new(err).context(format!("querying {endpoint}"));
            return Err(Failed::Fatal(err));
        }
        Err(err) => {
            return Err(Failed::Again {
                why: format!("{:#}", anyhow::Error::new(err)),
                after: None,
            });
        }
    };
    let status = response.status();
    if status.is_success() {
        return match response.bytes().await {
            Ok(body) => match parse_sparql_rows(&body) {
                Ok(rows) => Ok(Answer::Rows(rows)),
                Err(BadAnswer::Cut(why)) => Ok(Answer::Cut(why)),
                Err(BadAnswer::Invalid(err)) => Err(Failed::Fatal(err)),
            },
            Err(err) if err.is_timeout() => Ok(Answer::Cut(no_answer_in_time())),
            Err(err) => Ok(Answer::Cut(format!(
                "the answer broke off: {:#}",
                anyhow::Error::new(err)
            ))),
        };
    }
    let after = retry_after(response.headers());
    let body = response.bytes().await.unwrap_or_default();
    if status.is_server_error() && has_timeout_marker(&body) {
        return Ok(Answer::Cut(format!(
            "HTTP {status} with the query service's timeout error"
        )));
    }
    let why = format!(
        "HTTP {status}: {}",
        plumb_core::truncate_chars(String::from_utf8_lossy(&body).trim(), 500)
    );
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        Err(Failed::Again { why, after })
    } else {
        Err(Failed::Fatal(anyhow!("Wikidata query failed: {why}")))
    }
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
            "SELECT ?item ?itemLabel ?website WHERE { ?item wdt:P856 ?website ; wikibase:sitelinks ?sitelinks . FILTER(?sitelinks >= 20) SERVICE wikibase:label { bd:serviceParam wikibase:language \"en,mul\". } }"
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
                band(25, Some(26)),
                band(27, Some(29)),
                band(30, Some(33)),
                band(34, Some(39)),
                band(40, Some(49)),
                band(50, Some(69)),
                band(70, Some(99)),
                band(100, None),
            ]
        );
        assert_eq!(
            wikidata_sitelink_bands(45),
            [
                band(45, Some(49)),
                band(50, Some(69)),
                band(70, Some(99)),
                band(100, None)
            ]
        );
        assert_eq!(wikidata_sitelink_bands(100), [band(100, None)]);
        assert_eq!(wikidata_sitelink_bands(250), [band(250, None)]);
        // Below 25, every count is a band of its own.
        assert_eq!(
            wikidata_sitelink_bands(22)[..4],
            [
                band(22, Some(22)),
                band(23, Some(23)),
                band(24, Some(24)),
                band(25, Some(26))
            ]
        );
        assert_eq!(wikidata_sitelink_bands(0).len(), 25 + 8);
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
    fn band_queries_and_names() {
        assert_eq!(
            band(25, Some(26)).sparql_query(),
            "SELECT ?item ?itemLabel ?website WHERE { ?item wdt:P856 ?website ; wikibase:sitelinks ?sitelinks . FILTER(?sitelinks >= 25 && ?sitelinks <= 26) SERVICE wikibase:label { bd:serviceParam wikibase:language \"en,mul\". } }"
        );
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
        let filter = query.split_once("FILTER(")?.1.split_once(')')?.0;
        let (mut min, mut max) = (None, None);
        for part in filter.split("&&").map(str::trim) {
            if let Some(n) = part.strip_prefix("?sitelinks >= ") {
                min = Some(n.parse().ok()?);
            } else {
                let n = part.strip_prefix("?sitelinks <= ")?;
                max = Some(n.parse().ok()?);
            }
        }
        Some(band(min?, max))
    }

    /// The bands a stand-in endpoint was asked for, in order.
    type Asked = Arc<Mutex<Vec<SitelinkBand>>>;

    /// A stand-in for Wikidata's SPARQL endpoint on a loopback port; no
    /// outside network. It reads the band out of each form-encoded query and
    /// answers `respond(band, n)`, where `n` counts the queries before it.
    async fn sparql_endpoint<F>(respond: F) -> (String, Asked)
    where
        F: Fn(SitelinkBand, usize) -> Vec<u8> + Send + 'static,
    {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/sparql", listener.local_addr().unwrap());
        let asked = Asked::default();
        let log = Arc::clone(&asked);
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
                    let mut log = log.lock().unwrap();
                    log.push(band);
                    log.len() - 1
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

    #[tokio::test]
    async fn downloads_in_bands_and_halves_the_ones_wikidata_stops() {
        // The stand-in lists at most 200 rows within its "time limit". Past
        // that, it stops a band with an upper end as Wikidata does (JSON
        // broken off, then the exception) and cuts off the open band's JSON
        // with nothing after it.
        const LIMIT: usize = 200;
        let data = Arc::new(made_up_wikidata());
        let served = Arc::clone(&data);
        let (url, asked) = sparql_endpoint(move |band, _| {
            let rows = rows_in(&served, band);
            let answer = sparql_answer(rows.iter().copied());
            let body = if rows.len() <= LIMIT {
                answer
            } else if band.max.is_some() {
                stopped_at_time_limit(&answer, &band.sparql_query())
            } else {
                answer[..answer.len() / 2].to_string()
            };
            http_response("200 OK", &[], body.as_bytes())
        })
        .await;

        let dir = tempfile::tempdir().unwrap();
        let path = download_wikidata_official_sites_paced(
            &loopback_client(),
            &url,
            &dir.path().join("seed"),
            25,
            quick(),
        )
        .await
        .unwrap();
        assert_eq!(path, dir.path().join("seed").join(WIKIDATA_FILE_NAME));
        assert!(!part_path(&path).exists());

        // Every row from 25 sitelinks up, once.
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("item\tlabel\twebsite"));
        let got: Vec<&str> = lines.collect();
        let expected: HashSet<String> = rows_in(&data, band(25, None))
            .into_iter()
            .map(|(item, label, website)| format!("{item}\t{label}\t{website}"))
            .collect();
        assert_eq!(got.len(), expected.len(), "a row came twice");
        let got: HashSet<String> = got.into_iter().map(str::to_string).collect();
        assert_eq!(got, expected);
        let sites = crate::load_wikidata_official_sites(&path).unwrap();
        assert_eq!(sites.len(), expected.len());
        assert!(sites
            .iter()
            .any(|site| site.item == "Q2" && site.domain == "moved.org"));

        // The default bands came first, lowest first. One too big for the
        // time limit was asked for again in halves, right away, and those
        // until they fit: the open band too.
        let asked = asked.lock().unwrap().clone();
        assert_eq!(
            asked[..4],
            [
                band(25, Some(26)),
                band(27, Some(29)),
                band(27, Some(28)),
                band(29, Some(29))
            ]
        );
        for (i, &b) in asked.iter().enumerate() {
            if rows_in(&data, b).len() > LIMIT {
                let (lower, upper) = b.halves().unwrap();
                assert_eq!(asked[i + 1], lower, "{b:?}");
                assert!(asked[i + 1..].contains(&upper), "{b:?}");
            }
        }
        assert!(asked.contains(&band(100, None)));
        assert!(asked.contains(&band(200, None)));
        // The answers that were whole cover every count from 25 up once.
        let mut whole: Vec<SitelinkBand> = asked
            .iter()
            .copied()
            .filter(|&b| rows_in(&data, b).len() <= LIMIT)
            .collect();
        whole.sort_by_key(|b| b.min);
        assert_eq!(whole[0].min, 25);
        assert_eq!(whole.last().unwrap().max, None);
        for pair in whole.windows(2) {
            assert_eq!(
                pair[0].max.map(|max| max + 1),
                Some(pair[1].min),
                "{pair:?}"
            );
        }
        assert!(asked.len() > whole.len());
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
            0 => http_response(
                "429 Too Many Requests",
                &[("Retry-After", "1")],
                b"slow down",
            ),
            1 => http_response("503 Service Unavailable", &[], b"try later"),
            // Stopped at the time limit before the answer started.
            2 => http_response(
                "500 Internal Server Error",
                &[],
                stopped_at_time_limit("", &band.sparql_query()).as_bytes(),
            ),
            _ => http_response("200 OK", &[], answer_for(band).as_bytes()),
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let path = download_wikidata_official_sites_paced(
            &loopback_client(),
            &url,
            dir.path(),
            100,
            quick(),
        )
        .await
        .unwrap();
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "waited as long as Retry-After asked"
        );
        assert_eq!(
            *asked.lock().unwrap(),
            [
                band(100, None),
                band(100, None),
                band(100, None),
                band(100, Some(199)),
                band(200, None)
            ]
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "item\tlabel\twebsite\n\
             Q150\tOne fifty\thttps://one-fifty.org/\n\
             Q250\tTwo fifty\thttps://two-fifty.org/\n"
        );
    }

    #[tokio::test]
    async fn an_answer_the_connection_cuts_short_is_split() {
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
            100,
            quick(),
        )
        .await
        .unwrap();
        assert_eq!(
            *asked.lock().unwrap(),
            [band(100, None), band(100, Some(199)), band(200, None)]
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
    }

    #[tokio::test]
    async fn a_single_count_wikidata_still_stops_fails_the_download() {
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
            100,
            quick(),
        )
        .await
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.starts_with(
                "Wikidata stopped the query for items with exactly 100 sitelinks at its time limit"
            ),
            "{err}"
        );
        assert!(err.contains("cannot be split any further"), "{err}");
        assert!(!err.contains("JSON") && !err.contains("expected"), "{err}");
        let asked: Vec<String> = asked
            .lock()
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            asked,
            [
                "100 or more sitelinks",
                "100 to 199 sitelinks",
                "100 to 149 sitelinks",
                "100 to 124 sitelinks",
                "100 to 112 sitelinks",
                "100 to 106 sitelinks",
                "100 to 103 sitelinks",
                "100 to 101 sitelinks",
                "exactly 100 sitelinks",
            ]
        );
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
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
                http_response("503 Service Unavailable", &[], b"down"),
                WIKIDATA_TRIES as usize,
                "HTTP 503 Service Unavailable: down (tried 4 times)",
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
                100,
                quick(),
            )
            .await
            .unwrap_err();
            let err = format!("{err:#}");
            assert!(
                err.starts_with("asking Wikidata for items with 100 or more sitelinks: "),
                "{err}"
            );
            assert!(err.contains(expected), "{err}");
            assert_eq!(asked.lock().unwrap().len(), requests, "{err}");
            assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        }
    }
}
