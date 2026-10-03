//! Downloads the seed datasets. These hosts must be reachable from the
//! machine running `plumb fetch-data` (or a `plumb run` node on first start).

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use flate2::write::MultiGzDecoder;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tracing::info;

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
/// and at least `min_sitelinks` Wikipedia sitelinks (a notability filter
/// that keeps the query small enough to finish), and writes
/// `dir/wikidata-official-sites.tsv` with the header `item\tlabel\twebsite`.
///
/// The query is [`wikidata_sparql_query`], sent as a form POST; the JSON
/// results go through [`wikidata_json_to_tsv`].
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
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("query", &wikidata_sparql_query(min_sitelinks))
        .finish();
    info!("asking Wikidata for official websites of items with at least {min_sitelinks} sitelinks");
    let response = client
        .post(endpoint)
        .header(reqwest::header::ACCEPT, "application/sparql-results+json")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form)
        .send()
        .await
        .with_context(|| format!("querying {endpoint}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "Wikidata query failed: HTTP {status}: {}",
            plumb_core::truncate_chars(body.trim(), 500)
        );
    }
    let json = response
        .bytes()
        .await
        .context("reading the Wikidata response")?;
    let tsv = wikidata_json_to_tsv(&json)?;

    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(WIKIDATA_FILE_NAME);
    let part = part_path(&dest);
    tokio::fs::write(&part, tsv.as_bytes())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    tokio::fs::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!(
        "wrote {} official websites to {}",
        tsv.lines().count().saturating_sub(1),
        dest.display()
    );
    Ok(dest)
}

/// The SPARQL query for items with an official website and at least
/// `min_sitelinks` sitelinks, with English (or multilingual) labels.
pub fn wikidata_sparql_query(min_sitelinks: u32) -> String {
    format!(
        "SELECT ?item ?itemLabel ?website WHERE {{ ?item wdt:P856 ?website ; wikibase:sitelinks ?sitelinks . FILTER(?sitelinks >= {min_sitelinks}) SERVICE wikibase:label {{ bd:serviceParam wikibase:language \"en,mul\". }} }}"
    )
}

/// Converts SPARQL JSON results (`results.bindings[]` with `item`,
/// `itemLabel` and `website`) into the TSV that
/// [`crate::load_wikidata_official_sites`] reads: the header
/// `item\tlabel\twebsite`, then one row per binding.
///
/// Items lose their entity URL prefix (`http://www.wikidata.org/entity/Q1`
/// becomes `Q1`), tabs and line breaks inside values become spaces, and rows
/// missing a field (or with an empty one) are skipped.
pub fn wikidata_json_to_tsv(json: &[u8]) -> Result<String> {
    let response: SparqlResponse =
        serde_json::from_slice(json).context("parsing Wikidata SPARQL results")?;
    let mut tsv = String::from("item\tlabel\twebsite\n");
    for binding in &response.results.bindings {
        let (Some(item), Some(label), Some(website)) = (
            tsv_field(&binding.item),
            tsv_field(&binding.item_label),
            tsv_field(&binding.website),
        ) else {
            continue;
        };
        let item = bare_item_id(&item);
        if item.is_empty() {
            continue;
        }
        tsv.push_str(item);
        tsv.push('\t');
        tsv.push_str(&label);
        tsv.push('\t');
        tsv.push_str(&website);
        tsv.push('\n');
    }
    Ok(tsv)
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
}
