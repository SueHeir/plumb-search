//! `plumb crawl`: fetches homepages and merges what they say into the
//! records, including links that discover new domains. A long-running node
//! ([`crate::node`]) crawls through the same [`select_targets`] and
//! [`crawl_rolling`].
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
//! A site is due again `window` after its last try (30 days by default)
//! when that try got an answer: a page, a robots.txt refusal, an HTTP error,
//! or a failure on the site's side such as a robots.txt server error or a
//! redirect loop. A site that could not be reached at all
//! ([`is_connection_failure`]) is retried sooner: after 1 day, then 2, 4,
//! 8... days for each further failure in a row, never longer than `window`.
//! A dropped uplink costs the sites it hit a day, not a month.
//!
//! Each homepage is fetched at `https://<domain>/` first. When that gets no
//! answer, the crawler tries the URL the site was last reached at (the
//! record's `url`), then the `www.` host and plain http.
//!
//! # Saving
//!
//! Homepages are started [`CRAWL_BATCH_SIZE`] at a time, with more kept in
//! flight while the slowest of a batch finish, and saved in batches of that
//! size in the order they finish. Before a batch is started, its sites are
//! saved as tried and failed, so that if the crawler
//! dies on a hostile page (an out-of-memory abort cannot be caught), the
//! next run does not start with the same sites again: they come back after
//! the failure wait above, which grows if it happens again. As each batch
//! finishes, its results and each site's real outcome are saved. When the
//! crawl is cut short (on shutdown) or a batch looks offline, every site
//! started and not saved gets its old marks back instead.
//! Everything goes to the records file's journal ([`crate::records`]),
//! flushed to disk at once and folded into the file now and then, and when
//! `plumb crawl` ends.
//!
//! # Offline
//!
//! A batch in which [`OFFLINE_FAILED_PERCENT`]% or more of the homepages that
//! should answer (ones never tried, or reached on their last try; at least
//! [`OFFLINE_MIN_EXPECTED`] of them) could not be fetched, for whatever
//! reason, means that the network is down, or that a firewall, a proxy or
//! a setting on this side is in the way (an HTTP client that cannot be
//! built fails every site), rather than the sites. That batch is not saved.
//! `plumb crawl` waits and fetches it again, after 30 seconds, then 1, 2
//! and 4 minutes ([`OFFLINE_WAITS`]), since a home router that was swamped
//! usually recovers; when the batch still looks offline, it stops with an
//! error. A node does not wait: it tries again at its next round.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;

use anyhow::{bail, Result};
use plumb_core::{now_unix, RecordSet, SiteRecord};
use plumb_crawl::{
    to_records, CrawlConfig, CrawlOutcome, CrawlResult, CrawlTarget, HomepageCrawler,
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
/// homepages that should answer could not be fetched.
pub(crate) const OFFLINE_FAILED_PERCENT: usize = 90;

/// How long `plumb crawl` waits before fetching a batch that looked offline
/// again, one wait per try; after the last, the crawl stops.
const OFFLINE_WAITS: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
];

pub fn run(args: CrawlArgs) -> Result<()> {
    let runtime = runtime()?;
    let cfg = CrawlConfig {
        concurrency: args.concurrency,
        dns_lookups: args.dns_lookups,
        use_system_proxy: args.use_system_proxy,
        ..CrawlConfig::default()
    };
    let mut fetcher = Rolling {
        crawler: HomepageCrawler::new(cfg),
        concurrency: args.concurrency,
        runtime: runtime.handle(),
    };
    crawl_file_with(&args, &OFFLINE_WAITS, &mut fetcher)
}

/// `plumb crawl`, with homepages fetched by `fetch` a batch at a time.
#[cfg(test)]
fn crawl_file(
    args: &CrawlArgs,
    mut fetch: impl FnMut(Vec<CrawlTarget>) -> Vec<CrawlResult>,
) -> Result<()> {
    let mut fetcher = Batches {
        crawl: |batch| Some(fetch(batch)),
        queued: Vec::new(),
    };
    crawl_file_with(args, &[], &mut fetcher)
}

/// `plumb crawl`, with homepages fetched by `fetcher` and a batch that looks
/// offline fetched again after each of `offline_waits`.
fn crawl_file_with(
    args: &CrawlArgs,
    offline_waits: &[Duration],
    fetcher: &mut impl Fetcher,
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
    let totals = crawl_rolling(
        &mut set,
        &targets,
        CRAWL_BATCH_SIZE,
        &mut store,
        offline_waits,
        fetcher,
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
            "  errors: {} HTTP status, {} not HTML, {} bot checks, {} redirected off-site, \
             {} failed ({} could not be reached)",
            o.http_status, o.not_html, o.bot_check, o.offsite_redirect, o.failed, o.unreachable
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
/// never tried. The picks come best link score first, ties by domain, as
/// targets made by [`target_for`].
pub(crate) fn select_targets<'a>(
    records: impl Iterator<Item = &'a SiteRecord>,
    budget: usize,
    now: u64,
    window: u64,
) -> Vec<CrawlTarget> {
    select_targets_with(records, budget, now, window, |_| false)
}

/// [`select_targets`], also counting the crawled sites for which `due_now`
/// is true as due again, whenever they were last crawled.
pub(crate) fn select_targets_with<'a>(
    records: impl Iterator<Item = &'a SiteRecord>,
    budget: usize,
    now: u64,
    window: u64,
    due_now: impl Fn(&SiteRecord) -> bool,
) -> Vec<CrawlTarget> {
    let mut never: Vec<(f32, &SiteRecord)> = Vec::new();
    let mut again: Vec<(f32, &SiteRecord)> = Vec::new();
    for record in records {
        let scored = (record.link_score(), record);
        match due_at(record, window) {
            None => never.push(scored),
            Some(due) if due <= now || due_now(record) => again.push(scored),
            Some(_) => {}
        }
    }
    let best = |mut sites: Vec<(f32, &'a SiteRecord)>| {
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
    let mut picked: Vec<(f32, &SiteRecord)> = never[..take_never]
        .iter()
        .chain(&again[..take_again])
        .copied()
        .collect();
    picked.sort_by(by_score);
    picked
        .into_iter()
        .map(|(_, record)| target_for(record))
        .collect()
}

/// Best link score first, ties by domain.
fn by_score(a: &(f32, &SiteRecord), b: &(f32, &SiteRecord)) -> std::cmp::Ordering {
    b.0.total_cmp(&a.0)
        .then_with(|| a.1.domain.cmp(&b.1.domain))
}

/// The homepage of `record` to fetch: `https://<domain>/`, falling back to
/// the record's `url` (where the homepage was last reached, after
/// redirects) when that gets no answer.
pub(crate) fn target_for(record: &SiteRecord) -> CrawlTarget {
    CrawlTarget {
        known_url: record.url.clone(),
        ..CrawlTarget::new(&record.domain)
    }
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

/// Whether an outcome means the site could not be reached at all, at any of
/// the URLs the crawler tried: no connection, or no answer (the crawler's
/// `network` flag). These get the short retry wait; other failures, such as
/// a robots.txt server error, are the site's answer.
pub(crate) fn is_connection_failure(outcome: &CrawlOutcome) -> bool {
    matches!(outcome, CrawlOutcome::Failed { network: true, .. })
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

/// A batch in which nearly every homepage that should answer could not be
/// fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OfflineBatch {
    /// Homepages that should have answered but could not be fetched.
    pub(crate) failed: usize,
    /// Homepages in the batch that should answer: never tried, or reached
    /// on their last try.
    pub(crate) expected: usize,
}

impl fmt::Display for OfflineBatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} of {} homepages that should have answered could not be fetched, so the \
             network seems to be down or blocked; that batch of homepages was not saved",
            self.failed, self.expected
        )
    }
}

/// Fetches homepages for [`crawl_rolling`]: it starts the targets it is
/// given and says when they are done.
pub(crate) trait Fetcher {
    /// Targets [`crawl_rolling`] keeps started beyond the `batch_size` it
    /// waits for, so fetches go on while a batch's slowest sites finish.
    fn ahead(&self) -> usize {
        0
    }
    /// Starts fetching `targets`, after those started before.
    fn start(&mut self, targets: Vec<CrawlTarget>);
    /// Waits until `n` started targets are done (fewer when fewer are left)
    /// and returns the domains done with their results; a domain done
    /// without a result counts as not reached. `None` stops the run.
    fn finished(&mut self, n: usize) -> Option<(Vec<String>, Vec<CrawlResult>)>;
}

/// A [`Fetcher`] that fetches each batch whole with a closure, and starts
/// nothing ahead.
#[cfg(test)]
struct Batches<F> {
    crawl: F,
    queued: Vec<CrawlTarget>,
}

#[cfg(test)]
impl<F: FnMut(Vec<CrawlTarget>) -> Option<Vec<CrawlResult>>> Fetcher for Batches<F> {
    fn start(&mut self, targets: Vec<CrawlTarget>) {
        self.queued.extend(targets);
    }

    fn finished(&mut self, _n: usize) -> Option<(Vec<String>, Vec<CrawlResult>)> {
        let batch = std::mem::take(&mut self.queued);
        let done = batch.iter().map(|target| target.domain.clone()).collect();
        Some((done, (self.crawl)(batch)?))
    }
}

/// Fetches `targets` with `crawl`, `batch_size` at a time, saving to `store`
/// as the module docs describe, and calls `saved` with the totals so far
/// after each batch; an error from `saved` ends the run.
///
/// `crawl` returns `None` to stop early, say on shutdown. The run also ends
/// at a batch that looks offline. Either way, that batch's sites get their
/// old marks back, and [`RunTotals::end`] says why the run ended.
#[cfg(test)]
pub(crate) fn crawl_in_batches(
    set: &mut RecordSet,
    targets: &[CrawlTarget],
    batch_size: usize,
    store: &mut RecordStore,
    crawl: impl FnMut(Vec<CrawlTarget>) -> Option<Vec<CrawlResult>>,
    saved: impl FnMut(&RunTotals) -> Result<()>,
) -> Result<RunTotals> {
    let mut fetcher = Batches {
        crawl,
        queued: Vec::new(),
    };
    crawl_rolling(set, targets, batch_size, store, &[], &mut fetcher, saved)
}

/// [`crawl_in_batches`] with any [`Fetcher`]. Targets are started
/// `batch_size` at a time, each batch saved as tried and failed first, and
/// whenever `batch_size` of them are done, in whatever order they finish,
/// their results are saved and judged for being offline. A batch that looks
/// offline is started again after each of `offline_waits` in turn (the
/// thread sleeps meanwhile; each batch gets every wait). On a stop, or a
/// batch still offline after the last wait, every site started and not
/// done gets its old marks back.
pub(crate) fn crawl_rolling(
    set: &mut RecordSet,
    targets: &[CrawlTarget],
    batch_size: usize,
    store: &mut RecordStore,
    offline_waits: &[Duration],
    fetcher: &mut impl Fetcher,
    mut saved: impl FnMut(&RunTotals) -> Result<()>,
) -> Result<RunTotals> {
    let batch_size = batch_size.max(1);
    let batches = targets.len().div_ceil(batch_size);
    let mut merger = BatchMerger::default();
    let mut totals = RunTotals::default();
    let mut chunks = targets.chunks(batch_size);
    // Sites started and not done, with their marks from before.
    let mut started: HashMap<String, Mark> = HashMap::new();
    let mut batch = 0;
    // Waits used since the last batch that was saved.
    let mut tries = 0;
    loop {
        while started.len() < batch_size + fetcher.ahead() {
            let Some(chunk) = chunks.next() else {
                break;
            };
            let attempted_at = now_unix();
            let before: Vec<Mark> = chunk
                .iter()
                .map(|target| Mark::of(set, &target.domain))
                .collect();
            // Until a site is done, it counts as tried and failed.
            let pending = before
                .iter()
                .map(|mark| mark.failed(attempted_at))
                .collect();
            commit(set, store, pending)?;
            for mark in before {
                started.insert(mark.domain.clone(), mark);
            }
            fetcher.start(chunk.to_vec());
        }
        if started.is_empty() {
            return Ok(totals);
        }

        let Some((done, results)) = fetcher.finished(batch_size.min(started.len())) else {
            commit(set, store, started.values().map(Mark::change).collect())?;
            info!(
                "crawl stopped after {} of {} homepages",
                totals.attempted,
                targets.len()
            );
            totals.end = RunEnd::Stopped;
            return Ok(totals);
        };
        let before: Vec<Mark> = done
            .iter()
            .filter_map(|domain| started.remove(domain))
            .collect();
        if before.is_empty() {
            bail!(
                "the crawler finished no homepage of the {} started",
                started.len()
            );
        }
        let attempted_at = now_unix();
        let outcomes: HashMap<&str, &CrawlOutcome> = results
            .iter()
            .map(|result| (result.domain.as_str(), &result.outcome))
            .collect();
        if let Some(offline) = offline_batch(&before, &outcomes) {
            warn!("{offline}");
            if let Some(&wait) = offline_waits.get(tries) {
                tries += 1;
                info!(
                    "fetching those {} homepages again in {} seconds",
                    before.len(),
                    wait.as_secs()
                );
                std::thread::sleep(wait);
                let again: Vec<CrawlTarget> = targets
                    .iter()
                    .filter(|target| before.iter().any(|mark| mark.domain == target.domain))
                    .cloned()
                    .collect();
                for mark in before {
                    started.insert(mark.domain.clone(), mark);
                }
                fetcher.start(again);
                continue;
            }
            let restore = before.iter().chain(started.values());
            commit(set, store, restore.map(Mark::change).collect())?;
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
        totals.attempted += before.len();
        totals.outcomes.add(&counted);
        totals.discovered += set.len().saturating_sub(known);
        if store.wants_compaction() {
            let written = store.compact(set)?;
            info!(
                "folded the journal into {} ({written} records)",
                store.path().display()
            );
        }
        batch += 1;
        tries = 0;
        info!(
            "batch {batch}/{batches}: fetched {} of {} homepages",
            counted.fetched,
            before.len()
        );
        saved(&totals)?;
    }
}

/// A [`Fetcher`] that keeps `cfg.concurrency` homepages in flight with a
/// [`HomepageCrawler`], driven on `runtime`.
pub(crate) struct Rolling<'a> {
    pub(crate) crawler: HomepageCrawler,
    pub(crate) concurrency: usize,
    pub(crate) runtime: &'a tokio::runtime::Handle,
}

impl Rolling<'_> {
    /// The next `n` results, or fewer when fewer are pending.
    pub(crate) async fn next_results(crawler: &mut HomepageCrawler, n: usize) -> Vec<CrawlResult> {
        let mut results = Vec::with_capacity(n);
        while results.len() < n {
            match crawler.next().await {
                Some(result) => results.push(result),
                None => break,
            }
        }
        plumb_crawl::log_summary(&results);
        results
    }
}

impl Fetcher for Rolling<'_> {
    fn ahead(&self) -> usize {
        self.concurrency.max(1)
    }

    fn start(&mut self, targets: Vec<CrawlTarget>) {
        self.crawler.push(targets);
    }

    fn finished(&mut self, n: usize) -> Option<(Vec<String>, Vec<CrawlResult>)> {
        let results = self
            .runtime
            .block_on(Self::next_results(&mut self.crawler, n));
        let done = results.iter().map(|result| result.domain.clone()).collect();
        Some((done, results))
    }
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
/// [`OFFLINE_MIN_EXPECTED`] sites that should answer (never tried, or reached
/// on their last try), and [`OFFLINE_FAILED_PERCENT`]% or more of those
/// could not be fetched: no result, or a [`CrawlOutcome::Failed`] of any
/// kind, since failing for nearly every site points at this side. Sites
/// that could not be reached last time are left out: they often fail again.
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
                .is_none_or(|outcome| matches!(outcome, CrawlOutcome::Failed { .. }))
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
    /// Bot checks standing in for the homepage.
    pub(crate) bot_check: usize,
    pub(crate) offsite_redirect: usize,
    pub(crate) failed: usize,
    /// Of `failed`, the homepages that could not be reached at all
    /// ([`is_connection_failure`]).
    pub(crate) unreachable: usize,
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
                CrawlOutcome::BotCheck { .. } => s.bot_check += 1,
                CrawlOutcome::OffsiteRedirect { .. } => s.offsite_redirect += 1,
                CrawlOutcome::Failed { .. } => {
                    s.failed += 1;
                    s.unreachable += usize::from(is_connection_failure(&result.outcome));
                }
            }
        }
        s
    }

    fn add(&mut self, other: &CrawlSummary) {
        self.fetched += other.fetched;
        self.robots_disallowed += other.robots_disallowed;
        self.http_status += other.http_status;
        self.not_html += other.not_html;
        self.bot_check += other.bot_check;
        self.offsite_redirect += other.offsite_redirect;
        self.failed += other.failed;
        self.unreachable += other.unreachable;
    }

    /// Everything that was neither fetched nor blocked by robots.txt.
    pub(crate) fn errors(&self) -> usize {
        self.http_status + self.not_html + self.bot_check + self.offsite_redirect + self.failed
    }
}

#[cfg(test)]
mod tests;
