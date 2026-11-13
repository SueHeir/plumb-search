//! `plumb-tools live-eval`: the test searches asked of a node that is
//! already running, through `/api/search?full=1`.
//!
//! [`eval`](crate::eval) measures an index this machine holds; this
//! measures what a node serves, which is what a person or an AI app sees.
//! It needs no index, no page set files and no model, so it works against
//! a node on a server, in Docker or on a laptop, and tells the three
//! outcomes apart that an index alone cannot:
//!
//! - the expected answer is among the rows the node lists (`#1`, `top 3`);
//! - it is in the answer but not among the rows: a results page lists at
//!   most two pages ([`plumb_index::pages::MAX_PAGES_LISTED`]), so a film,
//!   a song or a repository the search names can be in `pages` while the
//!   rows show sites. Shown as `deeper`;
//! - it is only the panel above the results (`profile`), as a song's
//!   lyrics are. Shown as `panel`.
//!
//! The queries files are the ones [`eval`](crate::eval) reads
//! ([`eval::parse_queries`](crate::eval::parse_queries)), so a suite
//! measures the same expected answers either way. `fact_queries.tsv` is
//! not a site or page suite: its answers are text an instant answer should
//! hold, and `--facts` checks the node's `answer` against them instead.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use plumb_core::registrable_domain;
use serde_json::Value;

use crate::cli::LiveEvalArgs;
use crate::eval::{half_of, is_expected, parse_queries, EvalQuery};

/// How long one search may take before it counts as failed.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(45);

/// Where one query's expected answer was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// Among the rows the node lists, at this 1-based place.
    Row(usize),
    /// In `pages`, past the rows listed.
    Deeper,
    /// Only in the panel above the results.
    Panel,
    /// Nowhere in the answer.
    Nowhere,
}

/// What one queries file came to.
#[derive(Debug, Default, Clone)]
pub struct FileTotals {
    pub asked: usize,
    pub top1: usize,
    pub top3: usize,
    pub deeper: usize,
    pub panel: usize,
    pub nowhere: usize,
    pub failed: usize,
    /// Seconds each search took, in the order asked.
    pub times: Vec<f64>,
}

impl FileTotals {
    fn count(&mut self, found: Found) {
        self.asked += 1;
        match found {
            Found::Row(place) => {
                self.top1 += usize::from(place == 1);
                self.top3 += usize::from(place <= 3);
            }
            Found::Deeper => self.deeper += 1,
            Found::Panel => self.panel += 1,
            Found::Nowhere => self.nowhere += 1,
        }
    }

    fn median_secs(&self) -> f64 {
        if self.times.is_empty() {
            return 0.0;
        }
        let mut times = self.times.clone();
        times.sort_by(f64::total_cmp);
        times[times.len() / 2]
    }
}

/// The keys one listed row counts under: a site's registrable domain and
/// its address, or a page's address. Keyed as
/// [`eval`](crate::eval) keys results, so the same expected answers match.
fn row_keys(row: &Value) -> Vec<String> {
    match row.get("kind").and_then(Value::as_str) {
        Some("site") => {
            let site = row.get("site").unwrap_or(&Value::Null);
            let domain = site
                .get("domain")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut keys = Vec::new();
            if let Some(domain) = registrable_domain(domain) {
                keys.push(domain);
            } else if !domain.is_empty() {
                keys.push(domain.to_ascii_lowercase());
            }
            if let Some(url) = site.get("url").and_then(Value::as_str) {
                keys.push(url.to_string());
            }
            keys
        }
        Some("page") => page_keys(row.get("page").unwrap_or(&Value::Null)),
        _ => Vec::new(),
    }
}

/// A page row's address and the site it is on. A node before 0.5.0 gives
/// the page's own members in `page`; a newer one gives its item in `item`.
fn page_keys(page: &Value) -> Vec<String> {
    let item = page
        .get("item")
        .or_else(|| page.get("page"))
        .unwrap_or(page);
    let mut keys = Vec::new();
    if let Some(url) = item.get("url").and_then(Value::as_str) {
        keys.push(url.to_string());
        if let Some(domain) = registrable_domain(url) {
            keys.push(domain);
        }
    }
    keys
}

/// Whether any key of `row` is one of the expected answers.
fn is_answer(expected: &[String], keys: &[String]) -> bool {
    keys.iter()
        .any(|key| expected.iter().any(|want| is_expected(want, key)))
}

/// Where the answer to `query` is in what the node returned.
fn found_in(answer: &Value, expected: &[String]) -> Found {
    let rows = answer
        .pointer("/assembled/rows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (i, row) in rows.iter().enumerate() {
        if is_answer(expected, &row_keys(row)) {
            return Found::Row(i + 1);
        }
    }
    let pages = answer
        .get("pages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if pages.iter().any(|p| is_answer(expected, &page_keys(p))) {
        return Found::Deeper;
    }
    let panel = answer
        .pointer("/profile/url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !panel.is_empty() && is_answer(expected, &[panel.to_string()]) {
        return Found::Panel;
    }
    Found::Nowhere
}

/// Whether the node's instant answer holds one of the expected texts, as
/// `fact_queries.tsv` means them: compared without case, commas or spaces,
/// so "5,895 m" answers "5895".
fn answers_fact(answer: &Value, expected: &[String]) -> bool {
    let shown = answer
        .get("answer")
        .map(ToString::to_string)
        .unwrap_or_default();
    let shown = plain(&shown);
    expected.iter().any(|want| shown.contains(&plain(want)))
}

fn plain(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric() || *c == '.')
        .collect::<String>()
        .to_lowercase()
}

/// Asks the node one search and returns its answer and how long it took.
async fn ask(client: &reqwest::Client, node: &str, query: &str) -> Result<(Value, f64)> {
    let url = reqwest::Url::parse_with_params(
        &format!("{}/api/search", node.trim_end_matches('/')),
        &[("q", query), ("full", "1")],
    )
    .with_context(|| format!("building the address for {query:?}"))?;
    let started = Instant::now();
    let response = client
        .get(url)
        .timeout(SEARCH_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("asking for {query:?}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("reading the answer to {query:?}"))?;
    if !status.is_success() {
        bail!(
            "the node answered {status} to {query:?}: {}",
            cut(&body, 200)
        );
    }
    let answer: Value = serde_json::from_str(&body)
        .with_context(|| format!("reading the answer to {query:?} as JSON"))?;
    Ok((answer, started.elapsed().as_secs_f64()))
}

fn cut(text: &str, most: usize) -> String {
    let mut end = most.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// The queries of one file this run asks: its half when one was given, and
/// at most `most` of them, spread evenly so a sample covers the whole file.
fn chosen(queries: Vec<EvalQuery>, args: &LiveEvalArgs) -> Vec<EvalQuery> {
    let mut queries: Vec<EvalQuery> = match args.half {
        Some(half) => queries
            .into_iter()
            .filter(|q| half_of(&q.query) == half)
            .collect(),
        None => queries,
    };
    if let Some(most) = args.limit {
        if most > 0 && queries.len() > most {
            let step = queries.len() as f64 / most as f64;
            queries = (0..most)
                .map(|i| queries[(i as f64 * step) as usize].clone())
                .collect();
        }
    }
    queries
}

/// Asks every query of every file and prints a line per file.
pub fn run(args: LiveEvalArgs) -> Result<()> {
    if args.queries.is_empty() {
        bail!("give at least one --queries file");
    }
    let client = reqwest::Client::builder()
        .user_agent(concat!("plumb-tools/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("starting an HTTP client")?;
    // One runtime for the whole run: searches go one at a time, so a
    // public node sees no more load than one reader.
    let runtime = plumb_node::runtime()?;
    let mut totals: BTreeMap<String, FileTotals> = BTreeMap::new();
    for path in &args.queries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        // fact_queries.tsv's answers are text an instant answer should
        // hold, not sites, so that file is always measured against the
        // node's answer rather than its results.
        let facts = args.facts || name == "fact_queries.tsv";
        let queries = chosen(parse_queries(&text)?, &args);
        let mut file = FileTotals::default();
        for query in &queries {
            match runtime.block_on(ask(&client, &args.node, &query.query)) {
                Ok((answer, took)) => {
                    file.times.push(took);
                    let found = if facts {
                        if answers_fact(&answer, &query.expected) {
                            Found::Row(1)
                        } else {
                            Found::Nowhere
                        }
                    } else {
                        found_in(&answer, &query.expected)
                    };
                    file.count(found);
                    if args.misses && !matches!(found, Found::Row(1)) {
                        println!("  {name}: {:?} {found:?}", query.query);
                    }
                }
                Err(err) => {
                    file.failed += 1;
                    eprintln!("  {name}: {err:#}");
                }
            }
        }
        println!(
            "{name:26} asked={:3} #1={:3} top3={:3} deeper={:3} panel={:3} \
             nowhere={:3} failed={:2} median={:.2}s",
            file.asked,
            file.top1,
            file.top3,
            file.deeper,
            file.panel,
            file.nowhere,
            file.failed,
            file.median_secs(),
        );
        totals.insert(name, file);
    }
    let all = totals.values().fold(FileTotals::default(), |mut all, f| {
        all.asked += f.asked;
        all.top1 += f.top1;
        all.top3 += f.top3;
        all.deeper += f.deeper;
        all.panel += f.panel;
        all.nowhere += f.nowhere;
        all.failed += f.failed;
        all.times.extend(f.times.iter().copied());
        all
    });
    println!(
        "\nall: asked={} #1={} ({:.0}%) top3={} ({:.0}%) deeper={} panel={} \
         nowhere={} failed={} median={:.2}s",
        all.asked,
        all.top1,
        share(all.top1, all.asked) * 100.0,
        all.top3,
        share(all.top3, all.asked) * 100.0,
        all.deeper,
        all.panel,
        all.nowhere,
        all.failed,
        all.median_secs(),
    );
    Ok(())
}

fn share(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

#[cfg(test)]
mod tests;
