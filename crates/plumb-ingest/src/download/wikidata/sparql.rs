//! One SPARQL query sent and its answer read: the rows of a whole answer,
//! or why there are none, and the TSV the rows are written to.

use std::collections::HashSet;
use std::io;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use reqwest::header::{
    HeaderMap, ACCEPT, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, RETRY_AFTER,
};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::error::Category;

use crate::download::fetch::megabytes;
use crate::snippet;
use crate::wikidata::bare_item_id;

/// A query without a whole answer after this long is treated like one that
/// Wikidata stopped at its 60-second limit.
const WIKIDATA_QUERY_TIMEOUT: Duration = Duration::from_secs(180);

/// What Wikidata's query service (Blazegraph) writes when it stops a query
/// at its time limit: after the results sent so far when the answer had
/// already started as HTTP 200, or as the body of an HTTP 500.
const TIMEOUT_MARKERS: [&str; 2] = [
    "java.util.concurrent.TimeoutException",
    "com.bigdata.bop.engine.QueryTimeoutException",
];

/// Why one try of a query has no rows.
#[derive(Debug)]
pub(super) enum Failed {
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

/// Sends a query once and reads its answer.
pub(super) async fn try_query(
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
pub(super) struct WikidataRow {
    pub(super) item: String,
    pub(super) label: String,
    pub(super) website: String,
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
pub(super) struct OfficialSitesTsv {
    pub(super) text: String,
    seen: HashSet<(String, String)>,
}

impl OfficialSitesTsv {
    pub(super) fn new() -> Self {
        OfficialSitesTsv {
            text: String::from("item\tlabel\twebsite\n"),
            seen: HashSet::new(),
        }
    }

    /// Adds the rows whose item and website pair is not in yet; returns
    /// how many.
    pub(super) fn add(&mut self, rows: Vec<WikidataRow>) -> usize {
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
    pub(super) fn rows(&self) -> usize {
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
mod tests;
