//! Common Crawl WAT files: one JSON metadata document per fetched page,
//! holding the HTML `<head>` fields and every link on the page.
//!
//! A WAT file is a WARC file, gzipped one record per gzip member. After a
//! leading `warcinfo` record, every record is `WARC-Type: metadata` with a
//! JSON body (`Content-Type: application/json`). Each WARC record is
//! `WARC/1.0\r\n`, header lines `Name: value\r\n`, a blank line, exactly
//! `Content-Length` bytes of body, then `\r\n\r\n`.
//!
//! The JSON fields Plumb reads:
//! - `Envelope.WARC-Header-Metadata.WARC-Type`: `"response"` for fetched pages
//!   (the `"request"` and `"metadata"` documents are skipped)
//! - `Envelope.WARC-Header-Metadata.WARC-Target-URI`: the page URL
//! - `Envelope.Payload-Metadata.HTTP-Response-Metadata.Response-Message.Status`: e.g. `"200"`
//! - `Envelope.Payload-Metadata.HTTP-Response-Metadata.HTML-Metadata.Head.Title`
//! - `...HTML-Metadata.Head.Metas`: objects such as
//!   `{"name": "description", "content": "..."}` or `{"property": "og:site_name", "content": "..."}`
//! - `...HTML-Metadata.Links`: objects such as `{"path": "A@/href", "url": "/about", "text": "About us"}`.
//!   Only `A@/href` entries are links to other pages; `url` may be relative
//!   and `text` may be missing.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use plumb_core::{
    collapse_whitespace, is_homepage_path, normalize_text, registrable_domain, truncate_chars,
    MAX_TEXT_CHARS,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::debug;
use url::Url;

use crate::snippet;

/// Longest anchor text kept, in characters after normalization.
const MAX_ANCHOR_CHARS: usize = 100;

/// Normalized link texts that say nothing about the site they point to.
const GENERIC_ANCHORS: &[&str] = &[
    "about",
    "about us",
    "back",
    "back to top",
    "click",
    "click here",
    "click to visit",
    "com",
    "contact",
    "contact us",
    "continue",
    "continue reading",
    "details",
    "download",
    "external link",
    "find out more",
    "full story",
    "go",
    "go to site",
    "go to website",
    "here",
    "home",
    "home page",
    "homepage",
    "http",
    "https",
    "info",
    "learn more",
    "link",
    "links",
    "main page",
    "more",
    "more info",
    "more information",
    "next",
    "official site",
    "official web site",
    "official website",
    "open",
    "previous",
    "read",
    "read more",
    "see more",
    "site",
    "source",
    "this",
    "this link",
    "top",
    "url",
    "view",
    "view more",
    "view site",
    "view website",
    "visit",
    "visit our website",
    "visit site",
    "visit the website",
    "visit website",
    "web",
    "web site",
    "website",
    "www",
];

/// Homepage fields taken from a WAT response document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomepageMeta {
    /// Registrable domain of `url`.
    pub domain: String,
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// `og:site_name`, often the plain brand name ("U.S. Bank").
    pub site_name: Option<String>,
}

/// What Plumb keeps from WAT files, aggregated by registrable domain.
#[derive(Debug, Clone, Default)]
pub struct WatExtract {
    /// Homepage metadata by domain.
    pub homepages: HashMap<String, HomepageMeta>,
    /// Inbound link text by target domain, from links to its front page:
    /// normalized text -> number of links using it.
    pub anchors: HashMap<String, HashMap<String, u32>>,
    /// Distinct linking domains by target domain.
    pub linking_domains: HashMap<String, HashSet<String>>,
}

/// Counters from reading WAT files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatStats {
    /// WARC records read, including `warcinfo`.
    pub records: u64,
    /// JSON documents describing a fetched page (`WARC-Type: response`).
    pub responses: u64,
    /// Responses accepted as homepages.
    pub homepages: u64,
    /// Cross-domain `A@/href` links counted.
    pub links: u64,
    /// Records whose JSON could not be parsed.
    pub bad_records: u64,
}

impl WatStats {
    pub fn add(&mut self, other: &WatStats) {
        self.records += other.records;
        self.responses += other.responses;
        self.homepages += other.homepages;
        self.links += other.links;
        self.bad_records += other.bad_records;
    }
}

impl WatExtract {
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one WAT JSON document into the extract.
    ///
    /// Homepages are `200` responses whose URL path passes
    /// [`plumb_core::is_homepage_path`] (empty, `/`, `/index.<ext>`,
    /// `/default.<ext>`, optionally under one or two locale segments such as
    /// `/en/`), with no query string. Per domain, a page on the canonical
    /// host (the registrable domain itself or `www.` + it) replaces one on
    /// any other subdomain, whatever the order they come in; between pages
    /// equal on that, an `https` URL replaces an `http` one; then the bare
    /// root (empty or `/` path) replaces a locale or index path; otherwise
    /// the first one seen is kept. Title and description are
    /// whitespace-collapsed and cut to [`plumb_core::MAX_TEXT_CHARS`].
    ///
    /// Links are `A@/href` entries from any response (not only homepages),
    /// resolved against the page URL, `http`/`https` only, whose target
    /// registrable domain differs from the page's. Every such link adds the
    /// page's domain to the target's `linking_domains`. Its text goes into
    /// `anchors` only when the link points at a front page
    /// ([`plumb_core::is_homepage_path`] on the target's path; a query string
    /// is fine), since text on deeper links names the page (a headline, a
    /// product, a social profile), not the site. That text is normalized with
    /// [`plumb_core::normalize_text`] and dropped if it is empty, longer than
    /// 100 characters, or generic ("click here", "here", "home", "homepage",
    /// "read more", "learn more", "more", "website", "official website",
    /// "visit website", "link", "this", "www", "http", "https", ...), or
    /// equal to the target URL itself.
    pub fn add_document(&mut self, doc: &Value, stats: &mut WatStats) {
        let envelope = &doc["Envelope"];
        let warc = &envelope["WARC-Header-Metadata"];
        if warc["WARC-Type"].as_str() != Some("response") {
            return;
        }
        stats.responses += 1;
        let Some(mut page_url) = warc["WARC-Target-URI"]
            .as_str()
            .and_then(|uri| Url::parse(uri.trim()).ok())
        else {
            return;
        };
        if !is_http(&page_url) {
            return;
        }
        page_url.set_fragment(None);
        let Some(page_domain) = registrable_domain(page_url.as_str()) else {
            return;
        };
        let http = &envelope["Payload-Metadata"]["HTTP-Response-Metadata"];
        let html = &http["HTML-Metadata"];
        if status_code(&http["Response-Message"]["Status"]) == Some(200)
            && page_url.query().is_none_or(str::is_empty)
            && is_homepage_path(page_url.path())
        {
            self.add_homepage(&page_domain, &page_url, &html["Head"], stats);
        }
        for link in html["Links"].as_array().into_iter().flatten() {
            self.add_link(&page_url, &page_domain, link, stats);
        }
    }

    fn add_homepage(&mut self, domain: &str, url: &Url, head: &Value, stats: &mut WatStats) {
        let replace = match self.homepages.get(domain) {
            None => true,
            Some(kept) => {
                let kept_preference = Url::parse(&kept.url).map_or((false, false, false), |kept| {
                    homepage_preference(&kept, domain)
                });
                homepage_preference(url, domain) > kept_preference
            }
        };
        if !replace {
            return;
        }
        let metas = head["Metas"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let meta = HomepageMeta {
            domain: domain.to_string(),
            url: url.to_string(),
            title: head["Title"].as_str().and_then(clean_text),
            description: meta_content(metas, |m| attr_is(m, "name", "description")),
            site_name: meta_content(metas, |m| {
                attr_is(m, "property", "og:site_name") || attr_is(m, "name", "og:site_name")
            }),
        };
        self.homepages.insert(domain.to_string(), meta);
        stats.homepages += 1;
    }

    fn add_link(&mut self, page_url: &Url, page_domain: &str, link: &Value, stats: &mut WatStats) {
        if link["path"].as_str() != Some("A@/href") {
            return;
        }
        let Some(href) = link["url"].as_str().map(str::trim) else {
            return;
        };
        let Ok(target) = page_url.join(href) else {
            return;
        };
        if !is_http(&target) {
            return;
        }
        let Some(target_domain) = registrable_domain(target.as_str()) else {
            return;
        };
        if target_domain == page_domain {
            return;
        }
        stats.links += 1;
        // Text on a link to a deeper page names that page, not the site.
        if is_homepage_path(target.path()) {
            let text = normalize_text(link["text"].as_str().unwrap_or(""));
            if is_useful_anchor(&text, href, &target) {
                let count = self
                    .anchors
                    .entry(target_domain.clone())
                    .or_default()
                    .entry(text)
                    .or_insert(0);
                *count = count.saturating_add(1);
            }
        }
        let linkers = self.linking_domains.entry(target_domain).or_default();
        if !linkers.contains(page_domain) {
            linkers.insert(page_domain.to_string());
        }
    }
}

/// Reads every record of a WAT file (gzipped or plain, see
/// [`crate::open_maybe_gz`]) into `out`, returning counters for this file.
/// A record with unparsable JSON is counted in `bad_records` and skipped; a
/// truncated or malformed WARC framing is an error.
///
/// Only `metadata` records with a JSON body (or no `Content-Type`) are
/// parsed. On error, `out` keeps the documents read before it.
pub fn parse_wat(path: &Path, out: &mut WatExtract) -> Result<WatStats> {
    let mut reader = WarcReader::new(crate::open_maybe_gz(path)?);
    let mut stats = WatStats::default();
    while let Some(record) = reader
        .next_record()
        .with_context(|| format!("reading WAT file {}", path.display()))?
    {
        stats.records += 1;
        if !is_json_metadata(&record) {
            continue;
        }
        match serde_json::from_slice::<Value>(&record.body) {
            Ok(doc) => out.add_document(&doc, &mut stats),
            Err(err) => {
                stats.bad_records += 1;
                debug!(
                    "{}: skipping record {} with invalid JSON: {err}",
                    path.display(),
                    stats.records
                );
            }
        }
    }
    debug!(
        "{}: {} records, {} responses, {} homepages, {} cross-domain links, {} bad records",
        path.display(),
        stats.records,
        stats.responses,
        stats.homepages,
        stats.links,
        stats.bad_records
    );
    Ok(stats)
}

fn is_json_metadata(record: &WarcRecord) -> bool {
    record
        .header("WARC-Type")
        .is_some_and(|t| t.eq_ignore_ascii_case("metadata"))
        && record.header("Content-Type").is_none_or(|ct| {
            ct.trim()
                .to_ascii_lowercase()
                .starts_with("application/json")
        })
}

fn is_http(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

/// The HTTP status, which WAT files store as a string (`"200"`).
fn status_code(value: &Value) -> Option<u16> {
    match value {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
        _ => None,
    }
}

/// How well a homepage URL stands for `domain`, compared in order (greater
/// is better): served from the canonical host (`domain` itself or `www.` +
/// it), then served over https, then at the bare root rather than a locale
/// or index path.
fn homepage_preference(url: &Url, domain: &str) -> (bool, bool, bool) {
    let host = url.host_str().unwrap_or_default().trim_end_matches('.');
    let canonical = host == domain || host.strip_prefix("www.") == Some(domain);
    let bare_root = matches!(url.path(), "" | "/");
    (canonical, url.scheme() == "https", bare_root)
}

/// Whitespace-collapsed and length-capped text, `None` when empty.
fn clean_text(text: &str) -> Option<String> {
    let text = truncate_chars(&collapse_whitespace(text), MAX_TEXT_CHARS);
    (!text.is_empty()).then_some(text)
}

/// True when the meta object's `key` attribute is `value`, ignoring ASCII case.
fn attr_is(meta: &Value, key: &str, value: &str) -> bool {
    meta[key]
        .as_str()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case(value))
}

/// The cleaned `content` of the first matching meta tag that has some.
fn meta_content(metas: &[Value], matches: impl Fn(&Value) -> bool) -> Option<String> {
    metas
        .iter()
        .filter(|m| matches(m))
        .find_map(|m| m["content"].as_str().and_then(clean_text))
}

/// Whether a normalized link text is worth keeping for the link's target.
fn is_useful_anchor(text: &str, href: &str, target: &Url) -> bool {
    !text.is_empty()
        && text.chars().count() <= MAX_ANCHOR_CHARS
        && !GENERIC_ANCHORS.contains(&text)
        && text != normalize_text(href)
        && text != normalize_text(target.as_str())
}

/// Longest WARC header line accepted, in bytes; longer means the framing is off.
const MAX_WARC_LINE: usize = 64 * 1024;

/// One WARC record, as [`WarcReader`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarcRecord {
    /// The version line, e.g. `WARC/1.0`.
    pub version: String,
    /// Header fields in file order: names as written, values trimmed.
    pub headers: Vec<(String, String)>,
    /// The record block: exactly `Content-Length` bytes.
    pub body: Vec<u8>,
}

impl WarcRecord {
    /// The value of the first header called `name`, ignoring ASCII case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Reads WARC records one at a time from an uncompressed stream (wrap a
/// gzipped file in [`crate::open_maybe_gz`] first).
///
/// Header lines may end in `\r\n` or `\n`, folded header lines are joined,
/// and the body is exactly `Content-Length` bytes. The blank lines after a
/// body are optional before the end of input. Truncated input, a missing or
/// invalid `Content-Length`, or anything other than blank lines between a
/// body and the next `WARC/` line is an error.
pub struct WarcReader<R> {
    reader: R,
    /// Bytes consumed so far, for error messages.
    offset: u64,
    /// Records returned so far.
    records: u64,
}

impl<R: BufRead> WarcReader<R> {
    /// Reads from `reader`, which must be at the start of a record (or of the file).
    pub fn new(reader: R) -> Self {
        WarcReader {
            reader,
            offset: 0,
            records: 0,
        }
    }

    /// The next record, or `None` at a clean end of input.
    pub fn next_record(&mut self) -> Result<Option<WarcRecord>> {
        let number = self.records + 1;
        // Skip the blank lines that end the previous record.
        let (start, version) = loop {
            let start = self.offset;
            match self.read_line()? {
                None => return Ok(None),
                Some((line, _)) if line.trim().is_empty() => continue,
                Some((line, _)) => break (start, line),
            }
        };
        let at = || format!("WARC record {number} at uncompressed byte {start}");
        if !version.starts_with("WARC/") {
            bail!(
                "{}: expected a `WARC/` version line, found {}",
                at(),
                snippet(&version)
            );
        }

        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let Some((line, true)) = self.read_line()? else {
                bail!("{}: input ends inside the record header (truncated?)", at());
            };
            if line.trim().is_empty() {
                break;
            }
            if line.starts_with([' ', '\t']) {
                let Some((_, value)) = headers.last_mut() else {
                    bail!("{}: header starts with a continuation line", at());
                };
                value.push(' ');
                value.push_str(line.trim());
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                bail!("{}: malformed header line {}", at(), snippet(&line));
            };
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }

        let length = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("Content-Length"))
            .map(|(_, v)| v.as_str())
            .with_context(|| format!("{}: no Content-Length header", at()))?;
        let length: u64 = length
            .parse()
            .with_context(|| format!("{}: invalid Content-Length {}", at(), snippet(length)))?;
        let mut body = Vec::with_capacity(length.min(1 << 20) as usize);
        let read = (&mut self.reader)
            .take(length)
            .read_to_end(&mut body)
            .with_context(|| format!("{}: reading the body", at()))?;
        self.offset += read as u64;
        if (read as u64) < length {
            bail!(
                "{}: truncated body, Content-Length is {length} but only {read} bytes remain",
                at()
            );
        }
        self.records += 1;
        Ok(Some(WarcRecord {
            version,
            headers,
            body,
        }))
    }

    /// The next line without its `\n` or `\r\n`, and whether it had a line
    /// ending (only the last line of the input can lack one); `None` at the
    /// end of input.
    fn read_line(&mut self) -> Result<Option<(String, bool)>> {
        let mut buf = Vec::new();
        let n = (&mut self.reader)
            .take(MAX_WARC_LINE as u64 + 1)
            .read_until(b'\n', &mut buf)
            .with_context(|| format!("reading WARC data at uncompressed byte {}", self.offset))?;
        if n == 0 {
            return Ok(None);
        }
        if n > MAX_WARC_LINE && buf.last() != Some(&b'\n') {
            bail!(
                "line at uncompressed byte {} is longer than {MAX_WARC_LINE} bytes; not WARC framing",
                self.offset
            );
        }
        self.offset += n as u64;
        let mut line = buf.as_slice();
        let terminated = match line.strip_suffix(b"\n") {
            Some(rest) => {
                line = rest;
                true
            }
            None => false,
        };
        if let Some(rest) = line.strip_suffix(b"\r") {
            line = rest;
        }
        Ok(Some((
            String::from_utf8_lossy(line).into_owned(),
            terminated,
        )))
    }
}

/// One fetched page, as [`WatWriter`] writes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatPage {
    pub url: String,
    /// HTTP status; HTML metadata is only written for 2xx.
    pub status: u16,
    pub title: Option<String>,
    pub description: Option<String>,
    pub site_name: Option<String>,
    /// `(href, anchor text)` pairs; hrefs may be relative.
    pub links: Vec<(String, String)>,
}

const FIXTURE_DATE: &str = "2026-09-15T00:00:00Z";

/// Writes WAT files in Common Crawl's layout, for fixtures and tests.
///
/// Each page becomes a `request` and a `response` metadata record, like a
/// real WAT file (which also has a third `metadata` document per page; the
/// reader skips those as well).
pub struct WatWriter<W: Write> {
    inner: W,
    gzip: bool,
    filename: String,
    records: u64,
}

impl<W: Write> WatWriter<W> {
    /// Starts a file and writes its `warcinfo` record. With `gzip`, every
    /// record is compressed as its own gzip member, as Common Crawl does.
    pub fn new(inner: W, gzip: bool, filename: &str) -> io::Result<Self> {
        let mut writer = WatWriter {
            inner,
            gzip,
            filename: filename.to_string(),
            records: 0,
        };
        let body = "software: plumb-search WatWriter\r\n\
                    format: WARC File Format 1.0\r\n\
                    description: synthetic WAT file\r\n";
        let headers = vec![
            ("WARC-Type", "warcinfo".to_string()),
            ("WARC-Date", FIXTURE_DATE.to_string()),
            ("WARC-Filename", writer.filename.clone()),
            ("WARC-Record-ID", writer.next_record_id()),
            ("Content-Type", "application/warc-fields".to_string()),
        ];
        writer.write_record(&headers, body.as_bytes())?;
        Ok(writer)
    }

    /// Writes the `request` and `response` metadata records for one page.
    pub fn write_page(&mut self, page: &WatPage) -> io::Result<()> {
        let request = json!({
            "Container": self.container(),
            "Envelope": {
                "Format": "WARC",
                "WARC-Header-Metadata": {
                    "WARC-Type": "request",
                    "WARC-Date": FIXTURE_DATE,
                    "WARC-Target-URI": page.url,
                },
                "Payload-Metadata": {
                    "Actual-Content-Type": "application/http; msgtype=request",
                    "HTTP-Request-Metadata": {
                        "Request-Message": { "Method": "GET", "Path": "/", "Version": "HTTP/1.1" },
                        "Headers": { "User-Agent": "CCBot/2.0 (https://commoncrawl.org/faq/)" },
                    },
                },
            },
        });
        self.write_json(&page.url, &request)?;

        let mut response_meta = json!({
            "Response-Message": {
                "Version": "HTTP/1.1",
                "Status": page.status.to_string(),
                "Reason": reason_phrase(page.status),
            },
            "Headers": { "Content-Type": "text/html; charset=UTF-8" },
        });
        if (200..300).contains(&page.status) {
            let mut metas = Vec::new();
            if let Some(description) = &page.description {
                metas.push(json!({ "name": "description", "content": description }));
            }
            if let Some(site_name) = &page.site_name {
                metas.push(json!({ "property": "og:site_name", "content": site_name }));
            }
            let mut head = json!({ "Metas": metas });
            if let Some(title) = &page.title {
                head["Title"] = json!(title);
            }
            let links: Vec<Value> = page
                .links
                .iter()
                .map(|(href, text)| json!({ "path": "A@/href", "url": href, "text": text }))
                .collect();
            response_meta["HTML-Metadata"] = json!({ "Head": head, "Links": links });
        }
        let response = json!({
            "Container": self.container(),
            "Envelope": {
                "Format": "WARC",
                "WARC-Header-Metadata": {
                    "WARC-Type": "response",
                    "WARC-Date": FIXTURE_DATE,
                    "WARC-Target-URI": page.url,
                },
                "Payload-Metadata": {
                    "Actual-Content-Type": "application/http; msgtype=response",
                    "HTTP-Response-Metadata": response_meta,
                },
            },
        });
        self.write_json(&page.url, &response)
    }

    /// Flushes and returns the underlying writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.inner.flush()?;
        Ok(self.inner)
    }

    fn container(&self) -> Value {
        json!({ "Filename": self.filename, "Compressed": self.gzip })
    }

    fn write_json(&mut self, target_uri: &str, doc: &Value) -> io::Result<()> {
        let body = serde_json::to_vec(doc)?;
        let headers = vec![
            ("WARC-Type", "metadata".to_string()),
            ("WARC-Target-URI", target_uri.to_string()),
            ("WARC-Date", FIXTURE_DATE.to_string()),
            ("WARC-Record-ID", self.next_record_id()),
            ("Content-Type", "application/json".to_string()),
        ];
        self.write_record(&headers, &body)
    }

    fn next_record_id(&self) -> String {
        format!("<urn:uuid:00000000-0000-4000-8000-{:012x}>", self.records)
    }

    fn write_record(&mut self, headers: &[(&str, String)], body: &[u8]) -> io::Result<()> {
        let mut record = Vec::with_capacity(body.len() + 512);
        record.extend_from_slice(b"WARC/1.0\r\n");
        for (name, value) in headers {
            write!(record, "{name}: {value}\r\n")?;
        }
        write!(record, "Content-Length: {}\r\n\r\n", body.len())?;
        record.extend_from_slice(body);
        record.extend_from_slice(b"\r\n\r\n");
        if self.gzip {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&record)?;
            self.inner.write_all(&encoder.finish()?)?;
        } else {
            self.inner.write_all(&record)?;
        }
        self.records += 1;
        Ok(())
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::BufWriter;
    use std::path::PathBuf;

    use super::*;

    fn page(url: &str) -> WatPage {
        WatPage {
            url: url.to_string(),
            status: 200,
            ..Default::default()
        }
    }

    fn titled(url: &str, title: &str) -> WatPage {
        WatPage {
            title: Some(title.to_string()),
            ..page(url)
        }
    }

    fn linking(url: &str, links: &[(&str, &str)]) -> WatPage {
        WatPage {
            links: links
                .iter()
                .map(|(href, text)| (href.to_string(), text.to_string()))
                .collect(),
            ..page(url)
        }
    }

    fn wat_bytes(gzip: bool, pages: &[WatPage]) -> Vec<u8> {
        let mut writer = WatWriter::new(Vec::new(), gzip, "test.warc.wat.gz").unwrap();
        for p in pages {
            writer.write_page(p).unwrap();
        }
        writer.finish().unwrap()
    }

    fn write_wat(dir: &Path, name: &str, gzip: bool, pages: &[WatPage]) -> PathBuf {
        let path = dir.join(name);
        let file = BufWriter::new(File::create(&path).unwrap());
        let mut writer = WatWriter::new(file, gzip, name).unwrap();
        for p in pages {
            writer.write_page(p).unwrap();
        }
        writer.finish().unwrap();
        path
    }

    /// Writes `pages` as a plain WAT file and parses it back.
    fn extract(pages: &[WatPage]) -> (WatExtract, WatStats) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_wat(dir.path(), "pages.wat", false, pages);
        let mut out = WatExtract::new();
        let stats = parse_wat(&path, &mut out).unwrap();
        (out, stats)
    }

    fn read_all(data: &[u8]) -> Result<Vec<WarcRecord>> {
        let mut reader = WarcReader::new(data);
        let mut records = Vec::new();
        while let Some(record) = reader.next_record()? {
            records.push(record);
        }
        Ok(records)
    }

    fn error_of(data: &[u8]) -> String {
        format!("{:#}", read_all(data).unwrap_err())
    }

    /// One raw WARC record with CRLF framing.
    fn record(headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut out = b"WARC/1.0\r\n".to_vec();
        for (name, value) in headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\r\n\r\n");
        out
    }

    fn anchors(out: &WatExtract, domain: &str) -> Vec<(String, u32)> {
        let mut texts: Vec<(String, u32)> = out
            .anchors
            .get(domain)
            .map(|m| m.iter().map(|(t, c)| (t.clone(), *c)).collect())
            .unwrap_or_default();
        texts.sort();
        texts
    }

    fn pairs(items: &[(&str, u32)]) -> Vec<(String, u32)> {
        items.iter().map(|(t, c)| (t.to_string(), *c)).collect()
    }

    fn linkers(out: &WatExtract, domain: &str) -> Vec<String> {
        let mut domains: Vec<String> = out
            .linking_domains
            .get(domain)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        domains.sort();
        domains
    }

    fn response_doc(url: &str, status: &str, links: Value) -> Value {
        json!({
            "Envelope": {
                "WARC-Header-Metadata": { "WARC-Type": "response", "WARC-Target-URI": url },
                "Payload-Metadata": { "HTTP-Response-Metadata": {
                    "Response-Message": { "Status": status },
                    "HTML-Metadata": { "Head": { "Title": "A page" }, "Links": links },
                }},
            }
        })
    }

    #[test]
    fn writer_output_parses_back_plain_and_gzipped() {
        let pages = vec![
            WatPage {
                title: Some("U.S. Bank | Personal Banking".into()),
                description: Some("Banking, credit cards, loans".into()),
                site_name: Some("U.S. Bank".into()),
                links: vec![
                    ("/about".into(), "About".into()),
                    ("https://www.example.com/".into(), "Example".into()),
                ],
                ..page("https://www.usbank.com/")
            },
            linking(
                "https://news.example.org/story",
                &[
                    ("https://www.usbank.com/", "U.S. Bank"),
                    ("//usbank.com/en/", "US Bank"),
                    ("https://www.usbank.com/credit-cards", "U.S. Bank Visa Card"),
                ],
            ),
        ];
        let dir = tempfile::tempdir().unwrap();
        let plain = write_wat(dir.path(), "a.warc.wat", false, &pages);
        let gz = write_wat(dir.path(), "a.warc.wat.gz", true, &pages);
        assert_eq!(&std::fs::read(&gz).unwrap()[..2], &[0x1f, 0x8b]);

        let mut from_plain = WatExtract::new();
        let plain_stats = parse_wat(&plain, &mut from_plain).unwrap();
        let mut from_gz = WatExtract::new();
        let gz_stats = parse_wat(&gz, &mut from_gz).unwrap();
        let expected_stats = WatStats {
            records: 5,
            responses: 2,
            homepages: 1,
            links: 4,
            bad_records: 0,
        };
        assert_eq!(plain_stats, expected_stats);
        assert_eq!(gz_stats, expected_stats);
        assert_eq!(from_plain.homepages, from_gz.homepages);
        assert_eq!(from_plain.anchors, from_gz.anchors);
        assert_eq!(from_plain.linking_domains, from_gz.linking_domains);

        assert_eq!(
            from_plain.homepages["usbank.com"],
            HomepageMeta {
                domain: "usbank.com".into(),
                url: "https://www.usbank.com/".into(),
                title: Some("U.S. Bank | Personal Banking".into()),
                description: Some("Banking, credit cards, loans".into()),
                site_name: Some("U.S. Bank".into()),
            }
        );
        assert_eq!(from_plain.homepages.len(), 1);
        assert_eq!(anchors(&from_plain, "usbank.com"), pairs(&[("us bank", 2)]));
        assert_eq!(linkers(&from_plain, "usbank.com"), ["example.org"]);
        assert_eq!(
            anchors(&from_plain, "example.com"),
            pairs(&[("example", 1)])
        );
        assert_eq!(linkers(&from_plain, "example.com"), ["usbank.com"]);
    }

    #[test]
    fn warc_reader_reads_writer_records() {
        let data = wat_bytes(false, &[titled("https://a.com/", "A")]);
        let records = read_all(&data).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].version, "WARC/1.0");
        assert_eq!(records[0].header("warc-type"), Some("warcinfo"));
        for record in &records[1..] {
            assert_eq!(record.header("WARC-Type"), Some("metadata"));
            assert_eq!(record.header("Content-Type"), Some("application/json"));
            assert_eq!(record.header("WARC-Target-URI"), Some("https://a.com/"));
            let length: usize = record.header("Content-Length").unwrap().parse().unwrap();
            assert_eq!(record.body.len(), length);
        }
        let response: Value = serde_json::from_slice(&records[2].body).unwrap();
        assert_eq!(
            response["Envelope"]["WARC-Header-Metadata"]["WARC-Type"],
            "response"
        );
    }

    #[test]
    fn warc_reader_accepts_lf_line_endings_and_folded_headers() {
        let data = b"WARC/1.0\nWARC-Type: metadata\nWARC-Target-URI: https://a.com/\n  continued\nContent-Length: 5\n\nhello\n\n\
                     WARC/1.1\r\nWARC-Type: resource\r\ncontent-length: 0\r\n\r\n\r\n\r\n";
        let records = read_all(data).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].header("WARC-Target-URI"),
            Some("https://a.com/ continued")
        );
        assert_eq!(records[0].body, b"hello");
        assert_eq!(records[1].version, "WARC/1.1");
        assert_eq!(records[1].header("WARC-Type"), Some("resource"));
        assert!(records[1].body.is_empty());
    }

    #[test]
    fn warc_reader_honors_content_length() {
        // The body looks like the end of a record and the start of another.
        let body = b"line one\r\n\r\nWARC/1.0\r\nContent-Length: 999\r\n\r\n";
        let mut data = record(&[("WARC-Type", "metadata")], body);
        data.extend(record(&[("WARC-Type", "metadata")], b"{}"));
        let records = read_all(&data).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].body, body);
        assert_eq!(records[1].body, b"{}");
    }

    #[test]
    fn warc_reader_allows_missing_trailer_at_end() {
        let records = read_all(b"WARC/1.0\r\nContent-Length: 2\r\n\r\nhi").unwrap();
        assert_eq!(records[0].body, b"hi");
        let records = read_all(b"WARC/1.0\r\nContent-Length: 2\r\n\r\nhi\r\n").unwrap();
        assert_eq!(records[0].body, b"hi");
    }

    #[test]
    fn warc_reader_empty_input() {
        assert!(read_all(b"").unwrap().is_empty());
        assert!(read_all(b"\r\n\n").unwrap().is_empty());
    }

    #[test]
    fn warc_reader_rejects_truncation() {
        let err = error_of(b"WARC/1.0\r\nContent-Length: 10\r\n\r\nshort");
        assert!(err.contains("truncated body"), "{err}");
        let err = error_of(b"WARC/1.0\r\nWARC-Type: metadata\r\nContent-Le");
        assert!(err.contains("inside the record header"), "{err}");
        let err = error_of(b"WARC/1.0");
        assert!(err.contains("inside the record header"), "{err}");

        // The first record is fine; the error comes with the second.
        let mut data = record(&[("WARC-Type", "warcinfo")], b"info");
        let second = record(&[("WARC-Type", "metadata")], b"{\"a\": 1}");
        data.extend_from_slice(&second[..second.len() - 8]);
        let mut reader = WarcReader::new(data.as_slice());
        assert_eq!(reader.next_record().unwrap().unwrap().body, b"info");
        let err = format!("{:#}", reader.next_record().unwrap_err());
        assert!(err.contains("WARC record 2"), "{err}");
        assert!(err.contains("truncated body"), "{err}");
    }

    #[test]
    fn warc_reader_rejects_bad_framing() {
        // Content-Length shorter than the body leaves junk before the next record.
        let err = error_of(b"WARC/1.0\r\nContent-Length: 3\r\n\r\nhello\r\n\r\n");
        assert!(err.contains("expected a `WARC/` version line"), "{err}");
        let err = error_of(b"<html><body>Not Found</body></html>\n");
        assert!(err.contains("expected a `WARC/` version line"), "{err}");
        let err = error_of(b"WARC/1.0\r\nWARC-Type: metadata\r\n\r\n{}");
        assert!(err.contains("no Content-Length"), "{err}");
        let err = error_of(b"WARC/1.0\r\nContent-Length: lots\r\n\r\n{}");
        assert!(err.contains("invalid Content-Length"), "{err}");
        let err = error_of(b"WARC/1.0\r\nno colon here\r\nContent-Length: 0\r\n\r\n");
        assert!(err.contains("malformed header line"), "{err}");
        let err = error_of(b"WARC/1.0\r\n continuation first\r\nContent-Length: 0\r\n\r\n");
        assert!(err.contains("continuation"), "{err}");
        let long = [b"WARC/1.0\r\nX: ".as_slice(), &[b'a'; MAX_WARC_LINE + 10]].concat();
        let err = error_of(&long);
        assert!(err.contains("longer than"), "{err}");
    }

    #[test]
    fn parse_wat_rejects_truncated_files() {
        let pages = [
            linking("https://a.com/", &[("https://usbank.com/", "U.S. Bank")]),
            linking("https://b.com/", &[("https://usbank.com/", "U.S. Bank")]),
        ];
        let dir = tempfile::tempdir().unwrap();
        for gzip in [true, false] {
            let bytes = wat_bytes(gzip, &pages);
            let path = dir.path().join(format!("cut-{gzip}.wat"));
            std::fs::write(&path, &bytes[..bytes.len() - 40]).unwrap();
            let mut out = WatExtract::new();
            let err = parse_wat(&path, &mut out).unwrap_err();
            let err = format!("{err:#}");
            assert!(err.contains("cut-"), "{err}");
            // What came before the cut was read.
            assert_eq!(linkers(&out, "usbank.com"), ["a.com"], "gzip={gzip}");
        }
        assert!(parse_wat(&dir.path().join("missing.wat"), &mut WatExtract::new()).is_err());
    }

    #[test]
    fn parse_wat_counts_bad_json_and_skips_other_records() {
        let good = response_doc(
            "https://a.com/",
            "200",
            json!([{ "path": "A@/href", "url": "https://usbank.com/", "text": "U.S. Bank" }]),
        );
        let json_type = [
            ("WARC-Type", "metadata"),
            ("Content-Type", "application/json"),
        ];
        let mut data = record(&[("WARC-Type", "warcinfo")], b"software: test\r\n");
        data.extend(record(&json_type, b"{\"Envelope\": {"));
        data.extend(record(&json_type, good.to_string().as_bytes()));
        data.extend(record(&[("WARC-Type", "resource")], b"not json"));
        data.extend(record(
            &[("WARC-Type", "metadata"), ("Content-Type", "text/plain")],
            b"not json either",
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.wat");
        std::fs::write(&path, data).unwrap();
        let mut out = WatExtract::new();
        let stats = parse_wat(&path, &mut out).unwrap();
        assert_eq!(
            stats,
            WatStats {
                records: 5,
                responses: 1,
                homepages: 1,
                links: 1,
                bad_records: 1,
            }
        );
        assert_eq!(out.homepages["a.com"].title.as_deref(), Some("A page"));
    }

    #[test]
    fn homepage_selection() {
        let long_title = "word ".repeat(100);
        let pages = vec![
            // Not homepages: deeper paths, a query string, non-200 statuses.
            titled("https://www.usbank.com/about", "About U.S. Bank"),
            titled("https://www.usbank.com/?ref=nav", "With a query"),
            WatPage {
                status: 404,
                ..titled("https://www.usbank.com/", "Missing")
            },
            WatPage {
                status: 301,
                ..titled("https://example.com/", "Redirect")
            },
            titled("https://example.net/index", "No extension"),
            titled("https://example.net/news/", "Not a locale"),
            // Another subdomain's front page is kept until the canonical host
            // shows up, even over plain http...
            titled("https://careers.usbank.com/", "Careers at U.S. Bank"),
            titled("http://www.usbank.com/", "Plain http"),
            // ...then https beats http on the canonical host...
            WatPage {
                description: Some("  Banking,\n credit cards ".into()),
                site_name: Some("U.S. Bank".into()),
                ..titled(
                    "https://www.usbank.com/",
                    "  U.S. Bank |\n Personal   Banking ",
                )
            },
            // ...and after that the first one stays.
            titled("http://usbank.com/", "Later http"),
            titled("https://usbank.com/", "Later https"),
            titled("https://online.usbank.com/", "Later subdomain"),
            // Index files and locale front pages are homepages too.
            titled("https://example.org/en/index.html", "Example"),
            titled("https://example.com/", &long_title),
        ];
        let (out, stats) = extract(&pages);
        assert_eq!(stats.responses, 14);
        assert_eq!(stats.homepages, 5);
        assert_eq!(out.homepages.len(), 3);
        assert_eq!(
            out.homepages["usbank.com"],
            HomepageMeta {
                domain: "usbank.com".into(),
                url: "https://www.usbank.com/".into(),
                title: Some("U.S. Bank | Personal Banking".into()),
                description: Some("Banking, credit cards".into()),
                site_name: Some("U.S. Bank".into()),
            }
        );
        assert_eq!(
            out.homepages["example.org"].url,
            "https://example.org/en/index.html"
        );
        assert_eq!(out.homepages["example.org"].description, None);
        let title = out.homepages["example.com"].title.clone().unwrap();
        assert_eq!(title.chars().count(), MAX_TEXT_CHARS - 1);
        assert!(long_title.starts_with(&title));
        assert!(!out.homepages.contains_key("example.net"));
    }

    #[test]
    fn canonical_host_beats_other_subdomains_in_any_order() {
        let kept = |urls: &[&str]| {
            let mut out = WatExtract::new();
            let mut stats = WatStats::default();
            for url in urls {
                out.add_document(&response_doc(url, "200", json!([])), &mut stats);
            }
            out.homepages["usbank.com"].url.clone()
        };
        let careers = "https://careers.usbank.com/";
        let www_http = "http://www.usbank.com/";
        assert_eq!(kept(&[careers, www_http]), www_http);
        assert_eq!(kept(&[www_http, careers]), www_http);
        // The bare domain is canonical too; `www2.` is just another subdomain.
        assert_eq!(
            kept(&["https://www2.usbank.com/", "http://usbank.com/"]),
            "http://usbank.com/"
        );
        assert_eq!(
            kept(&[careers, "https://www.usbank.com./"]),
            "https://www.usbank.com./"
        );
        // Between other subdomains, https beats http and otherwise the first stays.
        assert_eq!(
            kept(&["http://careers.usbank.com/", "https://online.usbank.com/"]),
            "https://online.usbank.com/"
        );
        assert_eq!(
            kept(&[
                "https://online.usbank.com/",
                "http://careers.usbank.com/",
                careers
            ]),
            "https://online.usbank.com/"
        );
        // Between canonical pages, https beats http and otherwise the first stays.
        assert_eq!(
            kept(&[www_http, "https://usbank.com/", "https://www.usbank.com/"]),
            "https://usbank.com/"
        );
    }

    #[test]
    fn bare_root_beats_locale_and_index_paths() {
        let kept = |urls: &[&str]| {
            let mut out = WatExtract::new();
            let mut stats = WatStats::default();
            for url in urls {
                out.add_document(&response_doc(url, "200", json!([])), &mut stats);
            }
            out.homepages["usbank.com"].url.clone()
        };
        let root = "https://www.usbank.com/";
        let locale = "https://www.usbank.com/en/";
        let index = "https://www.usbank.com/index.html";
        assert_eq!(kept(&[locale, root]), root);
        assert_eq!(kept(&[index, root]), root);
        assert_eq!(kept(&[root, locale, index]), root);
        // Between two non-root paths, the first stays.
        assert_eq!(kept(&[locale, index]), locale);
        assert_eq!(kept(&[index, locale]), index);
        // The root only breaks ties: the canonical host and https come first.
        assert_eq!(kept(&["http://www.usbank.com/", locale]), locale);
        assert_eq!(kept(&["https://careers.usbank.com/", locale]), locale);
        assert_eq!(kept(&[locale, "https://careers.usbank.com/"]), locale);
    }

    #[test]
    fn homepage_urls() {
        for (url, expected) in [
            ("https://a.com", true),
            ("https://a.com/", true),
            ("https://a.com/?", true),
            ("https://a.com/#top", true),
            ("https://a.com/INDEX.PHP", true),
            ("https://a.com/default.aspx", true),
            ("https://a.com/en/", true),
            ("https://a.com/us/en/index.html", true),
            ("https://a.com/?lang=en", false),
            ("https://a.com/en/?lang=en", false),
            ("https://a.com/index", false),
            ("https://a.com/index.html/x", false),
            ("https://a.com/en/about/", false),
            ("https://a.com/news/", false),
        ] {
            let mut out = WatExtract::new();
            out.add_document(
                &response_doc(url, "200", json!([])),
                &mut WatStats::default(),
            );
            assert_eq!(out.homepages.contains_key("a.com"), expected, "{url}");
        }
    }

    #[test]
    fn link_rules() {
        let mut out = WatExtract::new();
        let mut stats = WatStats::default();
        let doc = response_doc(
            "https://blog.example.org/posts/1?page=2",
            "200",
            json!([
                { "path": "A@/href", "url": "https://www.usbank.com/", "text": "U.S. Bank" },
                // Relative URLs are resolved against the page.
                { "path": "A@/href", "url": "//usbank.com/en/", "text": "US Bank online" },
                { "path": "A@/href", "url": "../about", "text": "About this blog" },
                // Same registrable domain, so not an inbound link.
                { "path": "A@/href", "url": "https://shop.example.org/", "text": "Shop" },
                // Other front-page forms; a query string on the target is fine.
                { "path": "A@/href", "url": "https://usbank.com/?ref=partner", "text": "USBank" },
                { "path": "A@/href", "url": "https://www.usbank.com/default.aspx", "text": "U.S. Bank Home" },
                // Deeper links name the page, not the site: only the linking domain counts.
                { "path": "A@/href", "url": "https://www.usbank.com/credit-cards", "text": "U.S. Bank Visa Card" },
                { "path": "A@/href", "url": "https://www.facebook.com/usbank", "text": "U.S. Bank" },
                // Generic, URL-like, too long or missing text: only the linking domain counts.
                { "path": "A@/href", "url": "https://www.usbank.com/", "text": "Click here!" },
                { "path": "A@/href", "url": "https://www.usbank.com/", "text": "  Official Website " },
                { "path": "A@/href", "url": "https://www.usbank.com/", "text": "https://www.usbank.com" },
                { "path": "A@/href", "url": "https://www.usbank.com/", "text": "x".repeat(101) },
                { "path": "A@/href", "url": "https://www.usbank.com/", "text": "" },
                { "path": "A@/href", "url": "https://www.usbank.com/" },
                // Not http(s), not an anchor, or no registrable domain.
                { "path": "A@/href", "url": "mailto:info@usbank.com", "text": "Email" },
                { "path": "A@/href", "url": "javascript:void(0)", "text": "Menu" },
                { "path": "IMG@/src", "url": "https://cdn.other.com/logo.png", "text": "Logo" },
                { "path": "LINK@/href", "url": "https://fonts.example.net/css", "text": "Fonts" },
                { "path": "A@/href", "url": "http://192.168.1.1/", "text": "Router" },
                { "path": "A@/href", "url": "https://Example.COM/", "text": "Example Domain" },
            ]),
        );
        out.add_document(&doc, &mut stats);
        assert_eq!(stats.responses, 1);
        assert_eq!(stats.homepages, 0);
        assert_eq!(stats.links, 13);
        assert_eq!(
            anchors(&out, "usbank.com"),
            pairs(&[
                ("us bank", 1),
                ("us bank home", 1),
                ("us bank online", 1),
                ("usbank", 1)
            ])
        );
        assert_eq!(linkers(&out, "usbank.com"), ["example.org"]);
        assert_eq!(anchors(&out, "facebook.com"), pairs(&[]));
        assert_eq!(linkers(&out, "facebook.com"), ["example.org"]);
        assert_eq!(
            anchors(&out, "example.com"),
            pairs(&[("example domain", 1)])
        );
        assert_eq!(linkers(&out, "example.com"), ["example.org"]);
        assert_eq!(out.anchors.len(), 2);
        assert_eq!(out.linking_domains.len(), 3);

        // Links on any response count, even a 404 page; requests and junk are ignored.
        let not_found = response_doc(
            "http://other.net/missing",
            "404",
            json!([{ "path": "A@/href", "url": "https://usbank.com/", "text": "U.S. bank" }]),
        );
        out.add_document(&not_found, &mut stats);
        let request = json!({ "Envelope": { "WARC-Header-Metadata": {
            "WARC-Type": "request", "WARC-Target-URI": "https://usbank.com/" } } });
        out.add_document(&request, &mut stats);
        out.add_document(&json!([1, 2, 3]), &mut stats);
        out.add_document(&json!({ "Envelope": "nope" }), &mut stats);
        assert_eq!(stats.responses, 2);
        assert_eq!(stats.links, 14);
        assert_eq!(
            anchors(&out, "usbank.com"),
            pairs(&[
                ("us bank", 2),
                ("us bank home", 1),
                ("us bank online", 1),
                ("usbank", 1)
            ])
        );
        assert_eq!(linkers(&out, "usbank.com"), ["example.org", "other.net"]);
    }

    #[test]
    fn counts_add_up_across_documents_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let first = write_wat(
            dir.path(),
            "first.warc.wat.gz",
            true,
            &[
                linking(
                    "https://a.com/",
                    &[
                        ("https://www.usbank.com/", "U.S. Bank"),
                        // A deep link: counted as a link, but its text is not kept.
                        ("https://usbank.com/mortgage", "U.S. Bank"),
                    ],
                ),
                linking("https://b.com/page", &[("https://usbank.com/", "us bank")]),
            ],
        );
        let second = write_wat(
            dir.path(),
            "second.warc.wat",
            false,
            &[
                linking("https://c.com/", &[("https://usbank.com/", "US Bank")]),
                linking(
                    "https://www.a.com/x",
                    &[("https://usbank.com/", "U.S. Bank")],
                ),
                linking("https://c.com/y", &[("https://usbank.com/", "here")]),
            ],
        );
        let mut out = WatExtract::new();
        let mut total = WatStats::default();
        total.add(&parse_wat(&first, &mut out).unwrap());
        total.add(&parse_wat(&second, &mut out).unwrap());
        assert_eq!(
            total,
            WatStats {
                records: 5 + 7,
                responses: 5,
                homepages: 2,
                links: 6,
                bad_records: 0,
            }
        );
        assert_eq!(anchors(&out, "usbank.com"), pairs(&[("us bank", 4)]));
        assert_eq!(linkers(&out, "usbank.com"), ["a.com", "b.com", "c.com"]);
    }

    #[test]
    fn generic_anchor_list_is_normalized() {
        for text in GENERIC_ANCHORS {
            assert_eq!(&normalize_text(text), text);
        }
        let unique: HashSet<&&str> = GENERIC_ANCHORS.iter().collect();
        assert_eq!(unique.len(), GENERIC_ANCHORS.len());
    }
}
