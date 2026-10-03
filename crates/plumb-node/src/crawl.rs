//! `plumb crawl`: fetches homepages and merges what they say into the
//! records, including links that discover new domains. A long-running node
//! ([`crate::node`]) crawls through the same [`select_targets`] and
//! [`crawl_in_batches`].
//!
//! # Which homepages
//!
//! Each run (or node round) has a budget of homepages. Half of it goes to
//! sites never tried, the other half to sites due again, best link score
//! first within each half; a half with too few candidates leaves the rest of
//! its share to the other. So a node keeps reaching sites it has never seen
//! while it refreshes the best ones, instead of re-crawling its best few
//! hundred thousand sites forever.
//!
//! A site is due again `window` after its homepage was fetched or answered
//! (30 days by default; a robots.txt refusal or an HTTP error is an answer).
//! A site that could not be reached at all ([`is_connection_failure`]) is
//! retried sooner: after 1 day, then 2, 4, 8... days for each further
//! failure in a row, never longer than `window`. A dropped uplink costs the
//! sites it hit a day, not a month.
//!
//! # Saving
//!
//! Homepages are fetched [`CRAWL_BATCH_SIZE`] at a time. Before a batch is
//! fetched, its sites are saved as tried and failed, so that if the crawler
//! dies on a hostile page (an out-of-memory abort cannot be caught), the
//! next run does not start with the same sites again: they come back after
//! the failure wait above, which grows if it happens again. After the batch,
//! its results and each site's real outcome are saved. A batch that is cut
//! short (on shutdown) or looks offline gets its old marks back instead.
//! Everything goes to the records file's journal ([`crate::records`]),
//! flushed to disk at once and folded into the file now and then, and when
//! `plumb crawl` ends.
//!
//! # Offline
//!
//! A batch in which [`OFFLINE_FAILED_PERCENT`]% or more of the homepages that
//! should answer (ones never tried, or that answered last time; at least
//! [`OFFLINE_MIN_EXPECTED`] of them) failed to connect means that the
//! network is down, or that a firewall or proxy is in the way, rather than
//! the sites. That batch is not saved: `plumb crawl` stops with an error,
//! and a node tries again later.

use std::collections::{HashMap, HashSet};
use std::fmt;

use anyhow::{bail, Result};
use plumb_core::{now_unix, RecordSet, SiteRecord};
use plumb_crawl::{
    crawl_homepages, to_records, CrawlConfig, CrawlOutcome, CrawlResult, CrawlTarget,
};
use tracing::{info, warn};

use crate::cli::CrawlArgs;
use crate::records::{load_records, Change, RecordStore};
use crate::runtime;

pub(crate) const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// Homepages fetched per batch: the unit that is saved, and that is judged
/// for being offline.
pub(crate) const CRAWL_BATCH_SIZE: usize = 500;

/// How long a site that could not be reached waits before it is tried
/// again; the wait doubles with each further failure in a row, up to the
/// recrawl window.
pub(crate) const FIRST_RETRY_AFTER: u64 = SECONDS_PER_DAY;

/// A batch needs at least this many homepages that should answer to be
/// judged offline.
pub(crate) const OFFLINE_MIN_EXPECTED: usize = 20;

/// A batch is offline when at least this share, in percent, of its
/// homepages that should answer failed to connect.
pub(crate) const OFFLINE_FAILED_PERCENT: usize = 90;

pub fn run(args: CrawlArgs) -> Result<()> {
    let runtime = runtime()?;
    let cfg = CrawlConfig {
        concurrency: args.concurrency,
        use_system_proxy: args.use_system_proxy,
        ..CrawlConfig::default()
    };
    crawl_file(&args, |batch| {
        runtime.block_on(crawl_homepages(batch, &cfg))
    })
}

/// `plumb crawl`, with homepages fetched by `fetch`.
fn crawl_file(
    args: &CrawlArgs,
    mut fetch: impl FnMut(Vec<CrawlTarget>) -> Vec<CrawlResult>,
) -> Result<()> {
    let mut set = load_records(&args.records)?;
    let out = args.out.as_deref().unwrap_or(&args.records);
    let mut store = RecordStore::open(out);
    if out != args.records {
        // The journal of `out` holds changes to `out`, which starts as what
        // was read.
        store.compact(&set)?;
    }
    let window = args
        .skip_crawled_within_days
        .saturating_mul(SECONDS_PER_DAY);
    let targets = select_targets(set.iter(), args.top, now_unix(), window);
    info!(
        "crawling {} of {} homepages, {} at a time",
        targets.len(),
        set.len(),
        args.concurrency
    );
    let totals = crawl_in_batches(
        &mut set,
        &targets,
        CRAWL_BATCH_SIZE,
        &mut store,
        |batch| Some(fetch(batch)),
        |_| Ok(()),
    )?;
    let written = store.compact(&set)?;

    let o = &totals.outcomes;
    println!(
        "crawled {} homepages: {} fetched, {} blocked by robots.txt, {} errors",
        totals.attempted,
        o.fetched,
        o.robots_disallowed,
        o.errors()
    );
    if o.errors() > 0 {
        println!(
            "  errors: {} HTTP status, {} not HTML, {} redirected off-site, {} failed to connect or read",
            o.http_status, o.not_html, o.offsite_redirect, o.failed
        );
    }
    println!("discovered {} new domains", totals.discovered);
    println!("wrote {written} records to {}", out.display());
    if let RunEnd::Offline(offline) = totals.end {
        let proxy = if args.use_system_proxy {
            ""
        } else {
            " (if this machine reaches the internet through a proxy, add --use-system-proxy)"
        };
        bail!(
            "stopped crawling: {offline}. Check the network connection{proxy} and run the \
             crawl again"
        );
    }
    Ok(())
}

/// The homepages to fetch at `now`, at most `budget`: half of them from sites
/// never tried and half from sites due again ([`due_at`]), best link score
/// first within each half; a half with too few candidates leaves the rest of
/// its share to the other, and an odd budget gives the extra one to sites
/// never tried. The picks come best link score first, ties by domain.
pub(crate) fn select_targets<'a>(
    records: impl Iterator<Item = &'a SiteRecord>,
    budget: usize,
    now: u64,
    window: u64,
) -> Vec<CrawlTarget> {
    let mut never: Vec<(f32, &str)> = Vec::new();
    let mut again: Vec<(f32, &str)> = Vec::new();
    for record in records {
        let scored = (record.link_score(), record.domain.as_str());
        match due_at(record, window) {
            None => never.push(scored),
            Some(due) if due <= now => again.push(scored),
            Some(_) => {}
        }
    }
    let best = |mut sites: Vec<(f32, &'a str)>| {
        if sites.len() > budget {
            sites.select_nth_unstable_by(budget, by_score);
            sites.truncate(budget);
        }
        sites.sort_by(by_score);
        sites
    };
    let (never, again) = (best(never), best(again));
    let never_share = budget - budget / 2;
    let take_never = never
        .len()
        .min(never_share.max(budget.saturating_sub(again.len())));
    let take_again = again.len().min(budget - take_never);
    let mut picked: Vec<(f32, &str)> = never[..take_never]
        .iter()
        .chain(&again[..take_again])
        .copied()
        .collect();
    picked.sort_by(by_score);
    picked
        .into_iter()
        .map(|(_, domain)| CrawlTarget::homepage(domain))
        .collect()
}

/// Best link score first, ties by domain.
fn by_score(a: &(f32, &str), b: &(f32, &str)) -> std::cmp::Ordering {
    b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1))
}

/// When a site's homepage is due for another try, in Unix seconds: `window`
/// after it was last fetched or answered, or [`retry_after`] its last try
/// when that one could not reach it. `None` for a site never tried.
pub(crate) fn due_at(record: &SiteRecord, window: u64) -> Option<u64> {
    let last = record.crawled_at.max(record.crawl_attempted_at)?;
    let wait = match record.crawl_failures {
        0 => window,
        failures => retry_after(failures, window),
    };
    Some(last.saturating_add(wait))
}

/// The wait before trying a site again after `failures` tries in a row
/// could not reach it: [`FIRST_RETRY_AFTER`], doubled for each failure after
/// the first, and never longer than `window`.
pub(crate) fn retry_after(failures: u32, window: u64) -> u64 {
    let doublings = failures.saturating_sub(1);
    let factor = 1u64.checked_shl(doublings).unwrap_or(u64::MAX);
    FIRST_RETRY_AFTER.saturating_mul(factor).min(window)
}

/// Whether an outcome means the site could not be reached at all: no
/// connection, or no answer. These get the short retry wait and are what
/// makes a batch look offline. For now every [`CrawlOutcome::Failed`]
/// counts, robots.txt server errors included.
pub(crate) fn is_connection_failure(outcome: &CrawlOutcome) -> bool {
    matches!(outcome, CrawlOutcome::Failed { .. })
}

/// What a whole crawl run did, over all its batches.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunTotals {
    /// Homepages tried, in batches that were saved.
    pub(crate) attempted: usize,
    pub(crate) outcomes: CrawlSummary,
    /// Domains that were not in the records before the run.
    pub(crate) discovered: usize,
    pub(crate) end: RunEnd,
}

/// Why a crawl run ended.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunEnd {
    /// Every target was tried.
    #[default]
    Finished,
    /// The crawl was told to stop.
    Stopped,
    /// A batch looked offline and was not saved.
    Offline(OfflineBatch),
}

/// A batch in which nearly every homepage that should answer failed to connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OfflineBatch {
    /// Homepages that should have answered but could not be reached.
    pub(crate) failed: usize,
    /// Homepages in the batch that should answer: never tried, or answered
    /// last time.
    pub(crate) expected: usize,
}

impl fmt::Display for OfflineBatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} of {} homepages that should have answered could not be reached, so the \
             network seems to be down or blocked; that batch of homepages was not saved",
            self.failed, self.expected
        )
    }
}

/// Fetches `targets` with `crawl`, `batch_size` at a time, saving to `store`
/// as the module docs describe, and calls `saved` with the totals so far
/// after each batch; an error from `saved` ends the run.
///
/// `crawl` returns `None` to stop early, say on shutdown. The run also ends
/// at a batch that looks offline. Either way, that batch's sites get their
/// old marks back, and [`RunTotals::end`] says why the run ended.
pub(crate) fn crawl_in_batches(
    set: &mut RecordSet,
    targets: &[CrawlTarget],
    batch_size: usize,
    store: &mut RecordStore,
    mut crawl: impl FnMut(Vec<CrawlTarget>) -> Option<Vec<CrawlResult>>,
    mut saved: impl FnMut(&RunTotals) -> Result<()>,
) -> Result<RunTotals> {
    let batch_size = batch_size.max(1);
    let batches = targets.len().div_ceil(batch_size);
    let mut merger = BatchMerger::default();
    let mut totals = RunTotals::default();
    for (i, batch) in targets.chunks(batch_size).enumerate() {
        let attempted_at = now_unix();
        let before: Vec<Mark> = batch
            .iter()
            .map(|target| Mark::of(set, &target.domain))
            .collect();
        // Until the batch is done, its sites count as tried and failed.
        let pending = before
            .iter()
            .map(|mark| mark.failed(attempted_at))
            .collect();
        commit(set, store, pending)?;

        let Some(results) = crawl(batch.to_vec()) else {
            commit(set, store, before.iter().map(Mark::change).collect())?;
            info!(
                "crawl stopped after {} of {} homepages",
                totals.attempted,
                targets.len()
            );
            totals.end = RunEnd::Stopped;
            return Ok(totals);
        };
        let outcomes: HashMap<&str, &CrawlOutcome> = results
            .iter()
            .map(|result| (result.domain.as_str(), &result.outcome))
            .collect();
        if let Some(offline) = offline_batch(&before, &outcomes) {
            commit(set, store, before.iter().map(Mark::change).collect())?;
            warn!("{offline}");
            totals.end = RunEnd::Offline(offline);
            return Ok(totals);
        }

        let known = set.len();
        let mut changes = merger.changes(&results);
        changes.extend(
            before
                .iter()
                .map(|mark| mark.after(attempted_at, outcomes.get(mark.domain.as_str()).copied())),
        );
        commit(set, store, changes)?;
        let counted = CrawlSummary::of(&results);
        totals.attempted += batch.len();
        totals.outcomes.add(&counted);
        totals.discovered += set.len().saturating_sub(known);
        if store.wants_compaction() {
            let written = store.compact(set)?;
            info!(
                "folded the journal into {} ({written} records)",
                store.path().display()
            );
        }
        info!(
            "batch {}/{batches}: fetched {} of {} homepages",
            i + 1,
            counted.fetched,
            batch.len()
        );
        saved(&totals)?;
    }
    Ok(totals)
}

/// Saves `changes`, then makes them to `set`.
fn commit(set: &mut RecordSet, store: &mut RecordStore, changes: Vec<Change>) -> Result<()> {
    store.save(&changes)?;
    for change in changes {
        change.apply(set);
    }
    Ok(())
}

/// A site's crawl marks before a batch: when its homepage was last tried and
/// how many tries in a row failed to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mark {
    domain: String,
    attempted_at: Option<u64>,
    failures: u32,
}

impl Mark {
    fn of(set: &RecordSet, domain: &str) -> Mark {
        let record = set.get(domain);
        Mark {
            domain: domain.to_string(),
            attempted_at: record.and_then(|r| r.crawl_attempted_at),
            failures: record.map_or(0, |r| r.crawl_failures),
        }
    }

    /// These marks again.
    fn change(&self) -> Change {
        Change::Mark {
            domain: self.domain.clone(),
            attempted_at: self.attempted_at,
            failures: self.failures,
        }
    }

    /// Tried at `at`, and failed to reach the site.
    fn failed(&self, at: u64) -> Change {
        Change::Mark {
            domain: self.domain.clone(),
            attempted_at: Some(at),
            failures: self.failures.saturating_add(1),
        }
    }

    /// Tried at `at` with `outcome`; no outcome at all counts as a failure.
    fn after(&self, at: u64, outcome: Option<&CrawlOutcome>) -> Change {
        match outcome {
            Some(outcome) if !is_connection_failure(outcome) => Change::Mark {
                domain: self.domain.clone(),
                attempted_at: Some(at),
                failures: 0,
            },
            _ => self.failed(at),
        }
    }
}

/// The batch as an [`OfflineBatch`] when it looks offline: it holds at least
/// [`OFFLINE_MIN_EXPECTED`] sites that should answer (never tried, or that
/// answered last time), and [`OFFLINE_FAILED_PERCENT`]% or more of those
/// could not be reached. Sites that failed last time are left out: they
/// often fail again.
fn offline_batch(before: &[Mark], outcomes: &HashMap<&str, &CrawlOutcome>) -> Option<OfflineBatch> {
    let expected: Vec<&Mark> = before.iter().filter(|mark| mark.failures == 0).collect();
    if expected.len() < OFFLINE_MIN_EXPECTED {
        return None;
    }
    let failed = expected
        .iter()
        .filter(|mark| {
            outcomes
                .get(mark.domain.as_str())
                .is_none_or(|outcome| is_connection_failure(outcome))
        })
        .count();
    (failed * 100 >= expected.len() * OFFLINE_FAILED_PERCENT).then_some(OfflineBatch {
        failed,
        expected: expected.len(),
    })
}

/// Turns crawl batches into changes to a record set.
#[derive(Debug, Default)]
struct BatchMerger {
    /// Distinct crawled domains linking to each domain, over the whole run.
    /// [`to_records`] counts the linking domains of one batch only, and
    /// merging keeps the larger of two counts, so without this a site linked
    /// from several batches would keep only the count of its best batch.
    linkers: HashMap<String, HashSet<String>>,
}

impl BatchMerger {
    /// The records [`to_records`] makes of a batch's results, with
    /// `linking_domains` counted over the whole run, as changes to merge.
    fn changes(&mut self, results: &[CrawlResult]) -> Vec<Change> {
        for result in results {
            let CrawlOutcome::Fetched(page) = &result.outcome else {
                continue;
            };
            for link in &page.meta.links {
                // The links `to_records` counts: self-links are not inbound.
                if link.target_domain.is_empty() || link.target_domain == page.domain {
                    continue;
                }
                self.linkers
                    .entry(link.target_domain.clone())
                    .or_default()
                    .insert(page.domain.clone());
            }
        }
        to_records(results)
            .into_iter()
            .map(|mut record| {
                if let Some(linkers) = self.linkers.get(&record.domain) {
                    let count = u32::try_from(linkers.len()).unwrap_or(u32::MAX);
                    let signals = &mut record.signals;
                    signals.linking_domains = signals.linking_domains.max(count);
                }
                Change::Merge { record }
            })
            .collect()
    }
}

/// Crawl outcomes counted by kind.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CrawlSummary {
    pub(crate) fetched: usize,
    pub(crate) robots_disallowed: usize,
    pub(crate) http_status: usize,
    pub(crate) not_html: usize,
    pub(crate) offsite_redirect: usize,
    pub(crate) failed: usize,
}

impl CrawlSummary {
    pub(crate) fn of(results: &[CrawlResult]) -> Self {
        let mut s = CrawlSummary::default();
        for result in results {
            match &result.outcome {
                CrawlOutcome::Fetched(_) => s.fetched += 1,
                CrawlOutcome::RobotsDisallowed => s.robots_disallowed += 1,
                CrawlOutcome::HttpStatus { .. } => s.http_status += 1,
                CrawlOutcome::NotHtml { .. } => s.not_html += 1,
                CrawlOutcome::OffsiteRedirect { .. } => s.offsite_redirect += 1,
                CrawlOutcome::Failed { .. } => s.failed += 1,
            }
        }
        s
    }

    fn add(&mut self, other: &CrawlSummary) {
        self.fetched += other.fetched;
        self.robots_disallowed += other.robots_disallowed;
        self.http_status += other.http_status;
        self.not_html += other.not_html;
        self.offsite_redirect += other.offsite_redirect;
        self.failed += other.failed;
    }

    /// Everything that was neither fetched nor blocked by robots.txt.
    pub(crate) fn errors(&self) -> usize {
        self.http_status + self.not_html + self.offsite_redirect + self.failed
    }
}

#[cfg(test)]
mod tests;
