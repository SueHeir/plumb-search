//! The background work of a node: setup on first start, crawls, index
//! builds and the refresh schedule, with retries after failures.
//!
//! Each turn of [`run`] looks at what is on disk and in the saved state and
//! does the next piece of work, so the same code resumes after a restart:
//!
//! 1. no records file: download the seed data, ingest it, build the first
//!    index ([`set_up`]);
//! 2. no index, or one older than the records: build one ([`rebuild`]);
//! 3. homepages left in a round: crawl them, then rebuild ([`crawl`]);
//! 4. a refresh due or asked for: start a round ([`start_round`]);
//! 5. otherwise wait for the next refresh.
//!
//! Slow work runs on Tokio's blocking threads and checks for shutdown only
//! where stopping leaves nothing half-done; downloads and the homepage
//! fetches of a crawl are simply dropped.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use plumb_core::{now_unix, read_jsonl, RecordSet, SiteRecord};
use plumb_crawl::{crawl_homepages, CrawlConfig, CrawlOutcome, CrawlResult};
use plumb_index::build_index;
use plumb_ingest::{
    attach_facts, download, facts, load_cc_domain_ranks, load_site_facts, load_tranco,
    load_wikidata_official_sites, Builder,
};
use tokio::runtime::Handle;
use tracing::{info, warn};

use super::store::{self, Paths, SavedState};
use super::{Inner, NodeConfig, ServingIndex, Step, Stopped};
use crate::crawl::{crawl_in_batches, select_targets, CRAWL_BATCH_SIZE, SECONDS_PER_DAY};
use crate::web::{duration_words, group_thousands};
use crate::write_records_atomically;

/// A homepage fetched or tried this recently is not due for a crawl, as
/// with `plumb crawl --skip-crawled-within-days 30`.
const RECRAWL_AFTER_DAYS: u64 = 30;

/// Seed downloads younger than this are reused when setup is tried again.
const SEED_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How often a wait looks at the clock and at old indexes to delete.
const TICK: Duration = Duration::from_secs(60);

/// A batch of at least this many homepages of which not one answered means
/// the network is down (or blocked), not the sites.
const OFFLINE_MIN_BATCH: usize = 20;

/// Does the node's background work until it stops.
pub(super) async fn run(inner: Arc<Inner>) {
    let mut backoff = Backoff::new(inner.config.retry_wait, inner.config.max_retry_wait);
    while !inner.stopping() {
        match step(&inner).await {
            Ok(Next::Continue) => {
                backoff.reset();
                inner.clear_error();
            }
            Ok(Next::IdleUntil(until)) => {
                inner.set_step(Step::Idle, idle_detail(&inner.config));
                wait(&inner, Deadline::Wall(until)).await;
            }
            Err(err) if err.is::<Stopped>() => break,
            Err(err) => {
                let delay = backoff.next_delay();
                warn!(
                    "{err:#}; trying again in {}",
                    duration_words(delay.as_secs())
                );
                inner.set_error(&err, now_unix().saturating_add(delay.as_secs()));
                wait(&inner, Deadline::After(Instant::now() + delay)).await;
                // A refresh request that cut the wait short is answered by the retry.
                inner.refresh_requested.store(false, Ordering::SeqCst);
            }
        }
    }
    inner.set_step(Step::Stopping, "Stopped");
}

/// What to do after a [`step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// Look for more work right away.
    Continue,
    /// Nothing to do until this Unix time (forever when `None`), unless a
    /// refresh is asked for.
    IdleUntil(Option<u64>),
}

/// Does the next piece of work.
async fn step(inner: &Arc<Inner>) -> Result<Next> {
    if !inner.paths.records.is_file() {
        set_up(inner).await?;
        return Ok(Next::Continue);
    }
    let saved = inner.saved();
    if inner.current().is_none() || (saved.index_stale && saved.crawl_left == 0) {
        rebuild(inner).await?;
        return Ok(Next::Continue);
    }
    if saved.crawl_left > 0 {
        crawl(inner).await?;
        return Ok(Next::Continue);
    }
    let requested = inner.refresh_requested.swap(false, Ordering::SeqCst);
    let due = match (inner.config.refresh_every, saved.last_refresh) {
        (None, _) => None,
        (Some(every), Some(last)) => Some(last.saturating_add(every.as_secs())),
        // No refresh on record: one is due now.
        (Some(_), None) => Some(0),
    };
    if requested || due.is_some_and(|due| due <= now_unix()) {
        start_round(inner, requested)?;
        return Ok(Next::Continue);
    }
    Ok(Next::IdleUntil(due))
}

fn idle_detail(config: &NodeConfig) -> &'static str {
    if config.refresh_every.is_some() {
        "Up to date until the next refresh"
    } else {
        "Up to date; refreshing is off"
    }
}

/// First start: downloads the seed data, keeps the best sites in a new
/// records file and puts the first index of them in service.
async fn set_up(inner: &Arc<Inner>) -> Result<()> {
    info!(
        "no records in {} yet: setting up from the seed data",
        inner.paths.data.display()
    );
    let files = tokio::select! {
        files = download_seed(inner) => files?,
        () = inner.stopped() => return Err(Stopped.into()),
    };
    let built = blocking(inner, move |inner| {
        let records = ingest(inner, &files)?;
        let fresh = SavedState::fresh(inner.config.initial_crawl);
        inner.update_saved(|saved| *saved = fresh)?;
        inner.check_stop()?;
        build(inner, records)
    })
    .await?;
    put_in_service(inner, built).await
}

/// The files a setup ingests.
#[derive(Debug)]
struct SeedFiles {
    tranco: PathBuf,
    wikidata: PathBuf,
    /// Countries and kinds of the official websites' organizations; used
    /// when the file exists, since setup goes on without it.
    facts: PathBuf,
    cc_ranks: Option<PathBuf>,
}

/// Downloads the seed data into `seed/`, keeping files that an earlier try
/// saved in the last week. Every source is tried before failing, so the ones
/// that worked are not fetched again next time.
async fn download_seed(inner: &Inner) -> Result<SeedFiles> {
    let config = &inner.config;
    let sources = &config.sources;
    let seed = &inner.paths.seed;
    let client = download::http_client()?;
    let cc_url = config.cc_ranks_url();
    let total = 2 + usize::from(cc_url.is_some());
    let mut failures = Vec::new();

    let tranco = seed.join(download::TRANCO_FILE_NAME);
    inner.set_step(
        Step::Downloading,
        "Downloading the Tranco list of popular sites",
    );
    inner.set_progress(0, total, "files");
    if !is_recent(&tranco) {
        if let Err(err) = download::download_to_file(&client, &sources.tranco_url, &tranco).await {
            failures.push(format!("the Tranco list: {err:#}"));
        }
    }

    let wikidata = seed.join(download::WIKIDATA_FILE_NAME);
    inner.set_step(Step::Downloading, "Asking Wikidata for official websites");
    inner.set_progress(1, total, "files");
    if !is_recent(&wikidata) {
        let downloaded = download::download_wikidata_official_sites_from(
            &client,
            &sources.wikidata_sparql_url,
            seed,
            sources.wikidata_min_sitelinks,
        )
        .await;
        if let Err(err) = downloaded {
            failures.push(format!("Wikidata's official websites: {err:#}"));
        }
    }

    let facts = seed.join(facts::FACTS_FILE_NAME);
    if !is_recent(&facts) {
        inner.set_step(
            Step::Downloading,
            "Asking Wikidata for the countries and kinds of those sites",
        );
        let downloaded = facts::download_site_facts(
            &client,
            &sources.wikidata_sparql_url,
            seed,
            sources.wikidata_min_sitelinks,
        )
        .await;
        if let Err(err) = downloaded {
            // Only the country and kind ranking need them: carry on.
            warn!("could not get Wikidata's countries and kinds, going on without them: {err:#}");
        }
    }

    let mut cc_ranks = None;
    if let Some(url) = &cc_url {
        let path = seed.join(download::cc_domain_ranks_top_file_name(url, config.sites)?);
        inner.set_step(
            Step::Downloading,
            format!(
                "Downloading the top {} Common Crawl domain ranks",
                group_thousands(config.sites as u64)
            ),
        );
        inner.set_progress(2, total, "files");
        if !is_recent(&path) {
            let downloaded =
                download::download_cc_domain_ranks_top(&client, url, seed, config.sites).await;
            if let Err(err) = downloaded {
                failures.push(format!("Common Crawl's domain ranks: {err:#}"));
            }
        }
        cc_ranks = Some(path);
    }

    inner.set_progress(total, total, "files");
    if !failures.is_empty() {
        bail!("could not download the seed data:\n{}", failures.join("\n"));
    }
    Ok(SeedFiles {
        tranco,
        wikidata,
        facts,
        cc_ranks,
    })
}

/// True for a file saved within [`SEED_MAX_AGE`] (or dated in the future).
fn is_recent(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    meta.is_file()
        && SystemTime::now()
            .duration_since(modified)
            .map_or(true, |age| age < SEED_MAX_AGE)
}

/// Folds the seed files into site records, keeps the best
/// [`NodeConfig::sites`] and saves them as the records file.
fn ingest(inner: &Inner, files: &SeedFiles) -> Result<Vec<SiteRecord>> {
    let sources = 2 + usize::from(files.cc_ranks.is_some());
    let mut builder = Builder::new();

    inner.set_step(Step::Ingesting, "Reading the Tranco list");
    inner.set_progress(0, sources, "files");
    let entries = load_tranco(&files.tranco, None)
        .with_context(|| format!("loading the Tranco list {}", files.tranco.display()))?;
    builder.add_tranco(&entries);
    drop(entries);
    inner.check_stop()?;

    if let Some(path) = &files.cc_ranks {
        inner.set_step(Step::Ingesting, "Reading the Common Crawl domain ranks");
        inner.set_progress(1, sources, "files");
        let ranks = load_cc_domain_ranks(path, None)
            .with_context(|| format!("loading Common Crawl ranks {}", path.display()))?;
        builder.add_cc_ranks(&ranks);
        drop(ranks);
        inner.check_stop()?;
    }

    inner.set_step(Step::Ingesting, "Reading Wikidata's official websites");
    inner.set_progress(sources - 1, sources, "files");
    let mut sites = load_wikidata_official_sites(&files.wikidata)
        .with_context(|| format!("loading Wikidata sites {}", files.wikidata.display()))?;
    if files.facts.is_file() {
        match load_site_facts(&files.facts) {
            Ok(facts) => attach_facts(&mut sites, &facts),
            Err(err) => warn!("going on without Wikidata's countries and kinds: {err:#}"),
        }
    }
    builder.add_official_sites(&sites);
    drop(sites);
    inner.check_stop()?;

    let found = builder.len();
    let records = builder.finish(Some(inner.config.sites));
    inner.set_step(
        Step::Ingesting,
        format!(
            "Saving {} site records",
            group_thousands(records.len() as u64)
        ),
    );
    write_records_atomically(&inner.paths.records, &records)?;
    info!(
        "kept the best {} of {found} sites from the seed data in {}",
        records.len(),
        inner.paths.records.display()
    );
    Ok(records)
}

/// Builds a new index of the records file and puts it in service.
async fn rebuild(inner: &Arc<Inner>) -> Result<()> {
    let built = blocking(inner, |inner| {
        inner.set_step(Step::Indexing, "Reading the site records");
        let records = read_records(&inner.paths)?.into_sorted_vec();
        inner.check_stop()?;
        build(inner, records)
    })
    .await?;
    put_in_service(inner, built).await
}

/// Starts a refresh: [`NodeConfig::crawl_per_refresh`] homepages to crawl.
/// One that was asked for rebuilds the index even when nothing is crawled.
fn start_round(inner: &Inner, requested: bool) -> Result<()> {
    let homepages = inner.config.crawl_per_refresh;
    info!(
        "{}: crawling {homepages} homepages",
        if requested {
            "refresh asked for"
        } else {
            "refresh due"
        }
    );
    let now = now_unix();
    inner.update_saved(|saved| {
        saved.crawl_left = homepages;
        if requested {
            saved.index_stale = true;
        } else if homepages == 0 {
            saved.last_refresh = Some(now);
        }
    })
}

/// Crawls the rest of the round under way, then puts an index of the result
/// in service.
async fn crawl(inner: &Arc<Inner>) -> Result<()> {
    let handle = Handle::current();
    let built = blocking(inner, move |inner| crawl_and_build(inner, &handle)).await?;
    match built {
        Some(built) => put_in_service(inner, built).await,
        None => Ok(()),
    }
}

/// Crawls the homepages left in the round, saving the records file and the
/// state after every batch, then builds an index. `None` when there turned
/// out to be nothing to crawl and nothing new to index.
fn crawl_and_build(inner: &Inner, handle: &Handle) -> Result<Option<ServingIndex>> {
    let left = inner.saved().crawl_left;
    inner.set_step(Step::Crawling, "Reading the site records");
    let mut set = read_records(&inner.paths)?;
    inner.check_stop()?;

    let cutoff = now_unix().saturating_sub(RECRAWL_AFTER_DAYS * SECONDS_PER_DAY);
    let targets = select_targets(set.iter(), left, cutoff);
    if targets.is_empty() {
        info!("no homepage is due for a crawl");
    } else {
        info!(
            "crawling {} homepages, {CRAWL_BATCH_SIZE} between saves",
            targets.len()
        );
        inner.set_step(Step::Crawling, "Crawling homepages");
        inner.set_progress(0, targets.len(), "homepages");
        let cfg = CrawlConfig::default();
        let mut offline = false;
        let totals = crawl_in_batches(
            &mut set,
            &targets,
            CRAWL_BATCH_SIZE,
            &inner.paths.records,
            |batch| {
                let size = batch.len();
                let results = handle.block_on(async {
                    tokio::select! {
                        results = crawl_homepages(batch, &cfg) => Some(results),
                        () = inner.stopped() => None,
                    }
                })?;
                if seems_offline(size, &results) {
                    offline = true;
                    return None;
                }
                Some(results)
            },
            |totals| {
                inner.set_progress(totals.attempted, targets.len(), "homepages");
                inner.update_saved(|saved| {
                    saved.crawl_left = left.saturating_sub(totals.attempted);
                    saved.index_stale = true;
                })
            },
        )?;
        if offline {
            bail!(
                "no homepage in a batch of {} answered, so the network seems to be down \
                 (that batch was not saved)",
                CRAWL_BATCH_SIZE.min(targets.len() - totals.attempted)
            );
        }
        inner.check_stop()?;
        let o = &totals.outcomes;
        info!(
            "crawled {} homepages: {} fetched, {} blocked by robots.txt, {} errors; \
             {} new domains",
            totals.attempted,
            o.fetched,
            o.robots_disallowed,
            o.errors(),
            totals.discovered
        );
    }

    inner.update_saved(|saved| saved.crawl_left = 0)?;
    if !inner.saved().index_stale {
        let now = now_unix();
        inner.update_saved(|saved| saved.last_refresh = Some(now))?;
        inner.refresh_requested.store(false, Ordering::SeqCst);
        return Ok(None);
    }
    inner.check_stop()?;
    build(inner, set.into_sorted_vec()).map(Some)
}

/// True when a whole batch failed before any server answered: no page, no
/// status code, not even robots.txt rules. With enough homepages in the
/// batch, that is the network (or a firewall), not the sites, so the batch
/// is not saved: saving would mark every one of them as tried for 30 days.
fn seems_offline(batch: usize, results: &[CrawlResult]) -> bool {
    batch >= OFFLINE_MIN_BATCH
        && results
            .iter()
            .all(|result| matches!(result.outcome, CrawlOutcome::Failed { .. }))
}

/// Builds an index of `records` in a new numbered directory and opens it.
fn build(inner: &Inner, records: Vec<SiteRecord>) -> Result<ServingIndex> {
    let id = store::next_index_id(&inner.paths);
    let dir = inner.paths.index(id);
    inner.set_step(
        Step::Indexing,
        format!(
            "Building the search index of {} sites",
            group_thousands(records.len() as u64)
        ),
    );
    let started = Instant::now();
    let stats = build_index(&dir, &records)
        .with_context(|| format!("building the index in {}", dir.display()))?;
    drop(records);
    let index = match ServingIndex::open(id, &dir, inner.rank) {
        Ok(index) => index,
        Err(err) => {
            let _ = store::remove_index(&dir);
            return Err(err.context(format!("opening the new index in {}", dir.display())));
        }
    };
    info!(
        "built the index in {} ({} sites) in {:.1} s",
        dir.display(),
        stats.docs,
        started.elapsed().as_secs_f64()
    );
    Ok(index)
}

/// Swaps in a freshly built index and notes that the records file holds no
/// changes it lacks. A build with no homepages left to crawl ends the round.
async fn put_in_service(inner: &Arc<Inner>, built: ServingIndex) -> Result<()> {
    inner.install(built);
    let now = now_unix();
    inner.update_saved(|saved| {
        saved.index_stale = false;
        if saved.crawl_left == 0 {
            saved.last_refresh = Some(now);
        }
    })?;
    if inner.saved().crawl_left == 0 {
        // Requests made during the round are answered by it.
        inner.refresh_requested.store(false, Ordering::SeqCst);
    }
    sweep(inner).await;
    Ok(())
}

/// Reads the records file into a set, merging records for the same domain
/// (a file put there by hand may have some).
fn read_records(paths: &Paths) -> Result<RecordSet> {
    let records: Vec<SiteRecord> = read_jsonl(&paths.records)
        .with_context(|| format!("loading records {}", paths.records.display()))?;
    Ok(records.into_iter().collect())
}

/// Runs `work` on a blocking thread and waits for it to end, even when the
/// node is stopping: the work checks for that itself, between steps where
/// stopping leaves nothing half-done. A panic becomes an error.
async fn blocking<T, F>(inner: &Arc<Inner>, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Inner) -> Result<T> + Send + 'static,
{
    let inner = Arc::clone(inner);
    tokio::task::spawn_blocking(move || work(&inner))
        .await
        .context("background work crashed")?
}

/// Deletes the directories of replaced indexes that no search has open any
/// more.
async fn sweep(inner: &Arc<Inner>) {
    if !inner.has_retired() {
        return;
    }
    let inner = Arc::clone(inner);
    let _ = tokio::task::spawn_blocking(move || inner.sweep()).await;
}

/// When a [`wait`] ends.
#[derive(Debug, Clone, Copy)]
enum Deadline {
    /// At this Unix time, or never. The wall clock keeps counting while a
    /// computer sleeps, so a refresh due during a night in standby happens
    /// soon after waking up.
    Wall(Option<u64>),
    /// Once this much time has passed.
    After(Instant),
}

/// Waits for the deadline, a refresh request or shutdown, whichever comes
/// first, deleting replaced indexes meanwhile once their last search ends.
async fn wait(inner: &Arc<Inner>, deadline: Deadline) {
    loop {
        if inner.stopping() || inner.refresh_requested.load(Ordering::SeqCst) {
            return;
        }
        let nap = match deadline {
            Deadline::Wall(None) => TICK,
            Deadline::Wall(Some(at)) => {
                let now = now_unix();
                if now >= at {
                    return;
                }
                Duration::from_secs(at - now).min(TICK)
            }
            Deadline::After(at) => {
                let now = Instant::now();
                if now >= at {
                    return;
                }
                (at - now).min(TICK)
            }
        };
        tokio::select! {
            () = tokio::time::sleep(nap) => {}
            () = inner.wake.notified() => {}
            () = inner.stopped() => return,
        }
        sweep(inner).await;
    }
}

/// Waits between tries of failed work: `first`, then twice as long after
/// each failure in a row, up to `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Backoff {
    first: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    fn new(first: Duration, max: Duration) -> Self {
        Backoff {
            first,
            max: max.max(first),
            next: first,
        }
    }

    /// The wait after one more failure.
    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.max);
        delay
    }

    /// Back to the first wait, after a success.
    fn reset(&mut self) {
        self.next = self.first;
    }
}

#[cfg(test)]
mod tests {
    use plumb_crawl::{CrawledPage, PageMeta};

    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let minutes = |m: u64| Duration::from_secs(m * 60);
        let mut backoff = Backoff::new(minutes(10), minutes(6 * 60));
        let delays: Vec<u64> = (0..8)
            .map(|_| backoff.next_delay().as_secs() / 60)
            .collect();
        assert_eq!(delays, [10, 20, 40, 80, 160, 320, 360, 360]);
        backoff.reset();
        assert_eq!(backoff.next_delay(), minutes(10));
        // A cap below the first wait is raised to it.
        let mut odd = Backoff::new(minutes(10), minutes(1));
        assert_eq!(odd.next_delay(), minutes(10));
        assert_eq!(odd.next_delay(), minutes(10));
    }

    fn result(domain: &str, outcome: CrawlOutcome) -> CrawlResult {
        CrawlResult {
            domain: domain.to_string(),
            outcome,
        }
    }

    #[test]
    fn a_batch_with_no_answer_at_all_means_offline() {
        let failed = |i: usize| {
            result(
                &format!("site{i}.com"),
                CrawlOutcome::Failed {
                    error: "dns error".into(),
                    network: true,
                },
            )
        };
        let all_failed: Vec<CrawlResult> = (0..OFFLINE_MIN_BATCH).map(failed).collect();
        assert!(seems_offline(all_failed.len(), &all_failed));
        // Small batches can fail whole: a few dead sites are not an outage.
        assert!(!seems_offline(3, &all_failed[..3]));
        // One answer, of any kind, shows the network works.
        for answer in [
            CrawlOutcome::RobotsDisallowed,
            CrawlOutcome::HttpStatus { status: 503 },
            CrawlOutcome::Fetched(CrawledPage {
                domain: "up.com".into(),
                final_url: "https://up.com/".into(),
                status: 200,
                fetched_at: 1,
                meta: PageMeta::default(),
            }),
        ] {
            let mut results = all_failed.clone();
            results.push(result("up.com", answer));
            assert!(!seems_offline(results.len(), &results));
        }
    }

    #[test]
    fn only_recent_seed_files_are_reused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tranco-top-1m.csv.zip");
        assert!(!is_recent(&file));
        std::fs::write(&file, "1,example.com\n").unwrap();
        assert!(is_recent(&file));
        let old = SystemTime::now() - SEED_MAX_AGE - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(!is_recent(&file));
        assert!(!is_recent(dir.path()));
    }
}
