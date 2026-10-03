//! The background work of a node: setup on first start, crawls, index
//! builds and the refresh schedule, with retries after failures.
//!
//! Each turn of [`run`] looks at what is on disk and in the saved state and
//! does the next piece of work, so the same code resumes after a restart:
//!
//! 1. no records file: download the Tranco list and put a quick first index
//!    of it in service ([`set_up`]);
//! 2. no index, or one older than the records: build one ([`rebuild`]);
//! 3. the rest of the seed data (Wikidata's official websites, Common
//!    Crawl's ranks) missing and a try due: download it and fold it into
//!    the records ([`complete_seed`]);
//! 4. homepages left in a round: crawl them, then rebuild ([`crawl`]);
//! 5. a refresh due or asked for: start a round ([`start_round`]);
//! 6. otherwise wait for the next refresh (or the next try at Wikidata).
//!
//! A node in the network first folds in the records other nodes sent
//! ([`super::network::absorb_inbox`]), and rebuilds its index once enough
//! have come in; its crawls take only the sites it is assigned and publish
//! each batch of results.
//!
//! Slow work runs on Tokio's blocking threads and checks for shutdown only
//! where stopping leaves nothing half-done; downloads and the homepage
//! fetches of a crawl are simply dropped. Crawls pick homepages and save
//! their results as `plumb crawl` does ([`crate::crawl`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use plumb_core::{now_unix, SiteRecord};
use plumb_crawl::{CrawlConfig, CrawlOutcome, CrawlResult, CrawlTarget, HomepageCrawler};
use plumb_index::build_index;
use plumb_ingest::{
    attach_facts, download, facts, kind_sites, load_cc_domain_ranks, load_site_facts, load_tranco,
    load_wikidata_official_sites, Builder,
};
use tokio::runtime::Handle;
use tracing::{info, warn};

use super::network::{self, NETWORK_REBUILD_GAP, REBUILD_AFTER_RECORDS};
use super::store::{self, SavedState};
use super::{Inner, NodeConfig, ServingIndex, Step, Stopped};
use crate::crawl::{
    crawl_rolling, select_targets_with, target_for, Fetcher, Rolling, RunEnd, CRAWL_BATCH_SIZE,
    SECONDS_PER_DAY,
};
use crate::icons::IconStore;
use crate::records::{load_records, replace_records, RecordStore};
use crate::web::{duration_words, group_thousands};

/// A homepage fetched or answered this recently is not due for a crawl, as
/// with `plumb crawl --skip-crawled-within-days 30`. Sites that could not
/// be reached are retried sooner (see [`crate::crawl`]).
const RECRAWL_AFTER_DAYS: u64 = 30;

/// Nodes keep site icons from crawls since about this time (Unix seconds,
/// 2026-10-02). A site crawled before it is due again for its icon.
const ICONS_KEPT_SINCE: u64 = 1_791_000_000;

/// Disputed sites (see `plumb_net::agree`) a crawl round fetches at most,
/// first, out of the round's homepages.
const MAX_RECHECKS_PER_ROUND: usize = 100;

/// A disputed site this node fetched less than this long ago is not
/// fetched again yet: its crawl is on its way to the other nodes, or the
/// site did not answer.
const RECHECK_AGAIN_AFTER_SECS: u64 = SECONDS_PER_DAY;

/// Seed downloads younger than this are reused when setup is tried again.
const SEED_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How often a wait looks at the clock and at old indexes to delete.
const TICK: Duration = Duration::from_secs(60);

/// Does the node's background work until it stops.
pub(super) async fn run(inner: Arc<Inner>) {
    let mut backoff = Backoff::new(inner.config.retry_wait, inner.config.max_retry_wait);
    while !inner.stopping() {
        match step(&inner).await {
            Ok(Next::Continue) => {
                backoff.reset();
                inner.clear_error();
            }
            // A try at Wikidata, which keeps its own waits between tries,
            // says nothing of the work this loop is retrying.
            Ok(Next::Aside) => {}
            Ok(Next::IdleUntil(until)) => {
                let detail = inner
                    .pause_reason()
                    .unwrap_or_else(|| idle_detail(&inner.config).to_owned());
                inner.set_step(Step::Idle, detail);
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
    /// Look for more work right away, after a try at Wikidata.
    Aside,
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
    if inner.paths.inbox.exists() || inner.paths.absorbing.exists() {
        let absorbed = blocking(inner, network::absorb_inbox).await?;
        if absorbed > 0 {
            inner.update_saved(|saved| saved.network_pending += absorbed)?;
        }
    }
    // Enough records from other nodes rebuild the index, but no sooner
    // than NETWORK_REBUILD_GAP after the last build.
    let network_rebuild_at =
        (inner.saved().network_pending >= REBUILD_AFTER_RECORDS).then(|| network_rebuild_at(inner));
    if network_rebuild_at.is_some_and(|at| at <= now_unix()) && !inner.saved().index_stale {
        inner.update_saved(|saved| saved.index_stale = true)?;
    }
    let saved = inner.saved();
    if inner.current().is_none() || (saved.index_stale && saved.crawl_left == 0) {
        rebuild(inner).await?;
        return Ok(Next::Continue);
    }
    if missing_buckets(inner) {
        // Once per start, so a build that cannot write them is not retried
        // in a loop.
        inner.buckets_rebuilt.store(true, Ordering::SeqCst);
        info!("rebuilding the index to add the buckets private search and the network need");
        rebuild(inner).await?;
        return Ok(Next::Continue);
    }
    let wikidata_due = saved.wikidata_missing.then(|| inner.wikidata_retry_at());
    // Crawls and refreshes wait while background updates are off, paused
    // or outside the crawl hours, or a limit is reached; a day's download
    // limit ends with the day, a pause when it says.
    let pause = inner.pause();
    // After a quick start, the first crawl goes ahead of the rest of the
    // seed data: Wikidata takes half an hour or more, and until a node has
    // crawled, it has nothing to share with the network.
    let first_crawl = saved.quick_start && saved.crawl_left > 0 && pause.is_none();
    if wikidata_due.is_some_and(|due| due <= now_unix()) && !first_crawl {
        complete_seed(inner).await?;
        return Ok(Next::Aside);
    }
    if let Some(pause) = pause {
        inner.refresh_requested.store(false, Ordering::SeqCst);
        let until = pause.until.unwrap_or_else(|| store::next_day(now_unix()));
        return Ok(Next::IdleUntil(Some(
            wikidata_due.map_or(until, |due| due.min(until)),
        )));
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
    let until = [due, wikidata_due, network_rebuild_at]
        .into_iter()
        .flatten()
        .min();
    Ok(Next::IdleUntil(until))
}

/// When records from other nodes may next rebuild the index: once
/// [`NETWORK_REBUILD_GAP`] has passed since the last build (right away when
/// none was built since the node started).
fn network_rebuild_at(inner: &Inner) -> u64 {
    match inner.last_build.load(Ordering::SeqCst) {
        0 => 0,
        last => last.saturating_add(NETWORK_REBUILD_GAP.as_secs()),
    }
}

fn idle_detail(config: &NodeConfig) -> &'static str {
    if config.refresh_every.is_some() {
        "Up to date until the next refresh"
    } else {
        "Up to date; refreshing is off"
    }
}

/// First start: downloads the Tranco list alone, keeps its best sites in a
/// new records file and puts a first index of them in service, so search
/// works within a minute or two. The rest of the seed data, whose downloads
/// take many minutes (Wikidata's above all), is folded in right after by
/// [`complete_seed`].
async fn set_up(inner: &Arc<Inner>) -> Result<()> {
    info!(
        "no records in {} yet: setting up from the Tranco list, the rest of the seed data next",
        inner.paths.data.display()
    );
    let before = store::dir_size(&inner.paths.seed);
    let files = tokio::select! {
        files = download_quick_seed(inner) => files?,
        () = inner.stopped() => return Err(Stopped.into()),
    };
    let downloaded = store::dir_size(&inner.paths.seed).saturating_sub(before);
    let built = blocking(inner, move |inner| {
        let records = seed_records(inner, &files)?;
        let mut fresh = SavedState::fresh(inner.config.initial_crawl);
        fresh.wikidata_missing = true;
        fresh.quick_start = true;
        fresh.add_downloaded(downloaded, now_unix());
        // Saved first: records on disk always come with their state.
        inner.update_saved(|saved| *saved = fresh)?;
        save_seed_records(inner, &records)?;
        inner.check_stop()?;
        build(inner, records)
    })
    .await?;
    put_in_service(inner, built).await
}

/// Downloads the rest of the seed data, which setup went on without, and
/// folds it into the records as a full setup would have, then puts an index
/// of the result in service. After a quick start ([`SavedState::quick_start`])
/// the full seed replaces the quick records, keeping what crawls added; when
/// only Wikidata fails, the other files are folded in already.
///
/// A failure of Wikidata only sets the time of the next try: it is shown in
/// the status, but does not hold up other work.
async fn complete_seed(inner: &Arc<Inner>) -> Result<()> {
    let quick = inner.saved().quick_start;
    if quick {
        info!("downloading the rest of the seed data: Wikidata's official websites and more");
    } else {
        info!("asking Wikidata again for the official websites setup went without");
    }
    let before = store::dir_size(&inner.paths.seed);
    let files = tokio::select! {
        files = download_seed(inner) => files,
        () = inner.stopped() => return Err(Stopped.into()),
    };
    inner.add_downloaded(store::dir_size(&inner.paths.seed).saturating_sub(before))?;
    inner.recount_disk();
    let failed = |err: &anyhow::Error| {
        let retry_at = inner.wikidata_failed(err);
        warn!(
            "{err:#}; trying Wikidata again in {}",
            duration_words(retry_at.saturating_sub(now_unix()))
        );
    };
    let files = match files {
        Ok(files) => files,
        Err(err) => {
            failed(&err);
            return Ok(());
        }
    };
    if let (Err(err), false) = (&files.wikidata, quick) {
        failed(err);
        return Ok(());
    }
    // Noted once the other files are in, so that the status and the saved
    // state agree.
    let wikidata_err = files
        .wikidata
        .as_ref()
        .err()
        .map(|err| anyhow::anyhow!("{err:#}"));
    let wikidata_missing = wikidata_err.is_some();
    let built = blocking(inner, move |inner| {
        let seed = seed_records(inner, &files)?;
        inner.set_step(Step::Ingesting, "Reading the site records");
        let mut set = load_records(&inner.paths.records)?;
        let before = set.len();
        if quick {
            // Quick records the full seed leaves out are dropped, unless a
            // crawl reached them or found links to them.
            set.retain(|record| {
                record.crawl_attempted_at.is_some() || !record.link_texts.is_empty()
            });
        }
        set.extend(seed);
        inner.check_stop()?;
        let records = set.into_sorted_vec();
        inner.set_step(
            Step::Ingesting,
            format!(
                "Saving {} site records",
                group_thousands(records.len() as u64)
            ),
        );
        replace_records(&inner.paths.records, &records)?;
        inner.update_saved(|saved| {
            saved.wikidata_missing = wikidata_missing;
            saved.quick_start = false;
            saved.index_stale = true;
        })?;
        info!(
            "folded the seed data into the records: {} sites (there were {before})",
            records.len(),
        );
        inner.check_stop()?;
        build(inner, records)
    })
    .await?;
    // Not put_in_service: this is no refresh, and a round under way goes on.
    inner.install(built);
    inner.update_saved(|saved| saved.index_stale = false)?;
    match &wikidata_err {
        None => inner.wikidata_arrived(),
        Some(err) => failed(err),
    }
    sweep(inner).await;
    Ok(())
}

/// The files a setup ingests.
#[derive(Debug)]
struct SeedFiles {
    tranco: PathBuf,
    /// Setup can go on without Wikidata, so its failure is kept here
    /// rather than failing the download.
    wikidata: Result<PathBuf>,
    /// Countries and kinds of the official websites' organizations; used
    /// when the file exists, since setup goes on without it.
    facts: PathBuf,
    /// Official websites of banks, credit unions and other kinds of
    /// organizations, however few sitelinks; used when the file exists.
    kind_sites: PathBuf,
    cc_ranks: Option<PathBuf>,
}

/// Downloads the seed data into `seed/`, keeping files that an earlier try
/// saved in the last week. Every source is tried before failing, so the ones
/// that worked are not fetched again next time. Only the Tranco list and
/// Common Crawl's ranks are needed; a failure to get Wikidata's official
/// websites is returned in [`SeedFiles::wikidata`].
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

    let mut wikidata = Ok(seed.join(download::WIKIDATA_FILE_NAME));
    inner.set_step(Step::Downloading, "Asking Wikidata for official websites");
    inner.set_progress(1, total, "files");
    if !wikidata.as_ref().is_ok_and(|path| is_recent(path)) {
        let downloaded = download::download_wikidata_official_sites_paced(
            &client,
            &sources.wikidata_sparql_url,
            seed,
            sources.wikidata_min_sitelinks,
            sources.wikidata_pacing,
        )
        .await;
        if let Err(err) = &downloaded {
            failures.push(format!("Wikidata's official websites: {err:#}"));
        }
        wikidata = downloaded.context("could not download Wikidata's official websites");
    }

    let kind_sites = seed.join(kind_sites::KIND_SITES_FILE_NAME);
    if wikidata.is_ok() && !is_recent(&kind_sites) {
        inner.set_step(
            Step::Downloading,
            "Asking Wikidata for the sites of banks, credit unions and other organizations",
        );
        let downloaded = kind_sites::download_kind_sites(
            &client,
            &sources.wikidata_sparql_url,
            seed,
            sources.wikidata_pacing,
        )
        .await;
        if let Err(err) = downloaded {
            // More official sites help, but are not needed: carry on.
            warn!("could not get Wikidata's sites by kind, going on without them: {err:#}");
        }
    }

    let facts = seed.join(facts::FACTS_FILE_NAME);
    if let (Ok(sites), false) = (&wikidata, is_recent(&facts)) {
        inner.set_step(
            Step::Downloading,
            "Asking Wikidata for the countries and kinds of those sites",
        );
        let downloaded = facts::download_site_facts(
            &client,
            &sources.wikidata_sparql_url,
            seed,
            &[sites.clone(), kind_sites.clone()],
            sources.wikidata_pacing,
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
    // The other sources are needed; their failure is reported with
    // Wikidata's, if any, which is tried again then too.
    if failures.len() > usize::from(wikidata.is_err()) {
        bail!("could not download the seed data:\n{}", failures.join("\n"));
    }
    Ok(SeedFiles {
        tranco,
        wikidata,
        facts,
        kind_sites,
        cc_ranks,
    })
}

/// Downloads only the Tranco list into `seed/` (kept when an earlier try
/// saved it in the last week), for a quick start.
async fn download_quick_seed(inner: &Inner) -> Result<SeedFiles> {
    let seed = &inner.paths.seed;
    let tranco = seed.join(download::TRANCO_FILE_NAME);
    inner.set_step(
        Step::Downloading,
        "Downloading the Tranco list of popular sites",
    );
    inner.set_progress(0, 1, "files");
    if !is_recent(&tranco) {
        let client = download::http_client()?;
        download::download_to_file(&client, &inner.config.sources.tranco_url, &tranco)
            .await
            .context("could not download the seed data:\nthe Tranco list")?;
    }
    inner.set_progress(1, 1, "files");
    Ok(SeedFiles {
        tranco,
        wikidata: Err(anyhow::anyhow!("not downloaded yet")),
        facts: seed.join(facts::FACTS_FILE_NAME),
        kind_sites: seed.join(kind_sites::KIND_SITES_FILE_NAME),
        cc_ranks: None,
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

/// Folds the seed files into site records and keeps the best
/// [`NodeConfig::sites`], best first.
fn seed_records(inner: &Inner, files: &SeedFiles) -> Result<Vec<SiteRecord>> {
    let wikidata = files.wikidata.as_ref().ok();
    let sources = 1 + usize::from(wikidata.is_some()) + usize::from(files.cc_ranks.is_some());
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
        // The download kept only the top `sites` rows: read them all, past
        // the default cap of a million.
        let ranks = load_cc_domain_ranks(path, Some(usize::MAX))
            .with_context(|| format!("loading Common Crawl ranks {}", path.display()))?;
        builder.add_cc_ranks(&ranks);
        drop(ranks);
        inner.check_stop()?;
    }

    if let Some(path) = wikidata {
        inner.set_step(Step::Ingesting, "Reading Wikidata's official websites");
        inner.set_progress(sources - 1, sources, "files");
        let mut sites = load_wikidata_official_sites(path)
            .with_context(|| format!("loading Wikidata sites {}", path.display()))?;
        if files.kind_sites.is_file() {
            match load_wikidata_official_sites(&files.kind_sites) {
                Ok(by_kind) => sites.extend(by_kind),
                Err(err) => warn!("going on without Wikidata's sites by kind: {err:#}"),
            }
        }
        if files.facts.is_file() {
            match load_site_facts(&files.facts) {
                Ok(facts) => attach_facts(&mut sites, &facts),
                Err(err) => warn!("going on without Wikidata's countries and kinds: {err:#}"),
            }
        }
        builder.add_official_sites(&sites);
        drop(sites);
        inner.check_stop()?;
    }

    let found = builder.len();
    let records = builder.finish(Some(inner.config.sites));
    info!(
        "kept the best {} of {found} sites from the seed data",
        records.len()
    );
    Ok(records)
}

/// Saves the records of a setup as the records file.
fn save_seed_records(inner: &Inner, records: &[SiteRecord]) -> Result<()> {
    inner.set_step(
        Step::Ingesting,
        format!(
            "Saving {} site records",
            group_thousands(records.len() as u64)
        ),
    );
    replace_records(&inner.paths.records, records)?;
    info!(
        "saved {} site records in {}",
        records.len(),
        inner.paths.records.display()
    );
    Ok(())
}

/// Whether the index being served lacks the buckets this node needs, as an
/// index built before private search or the network was turned on does.
fn missing_buckets(inner: &Inner) -> bool {
    network::wants_buckets(inner)
        && !inner.buckets_rebuilt.load(Ordering::SeqCst)
        && inner.current().is_some_and(|index| index.buckets.is_none())
}

/// Builds a new index of the records file and puts it in service.
async fn rebuild(inner: &Arc<Inner>) -> Result<()> {
    let built = blocking(inner, |inner| {
        inner.set_step(Step::Indexing, "Reading the site records");
        let records = load_records(&inner.paths.records)?.into_sorted_vec();
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
    inner.journal.info(format!(
        "{} a crawl round of {} homepages",
        if requested {
            "Started, as asked,"
        } else {
            "Started"
        },
        group_thousands(homepages as u64)
    ));
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
        if requested || saved.network_pending > 0 {
            saved.index_stale = true;
        } else if homepages == 0 {
            saved.last_refresh = Some(now);
        }
    })
}

/// Fetches homepages for a node's crawl: keeps them in flight across
/// batches, stops on shutdown or when background updates are paused, keeps
/// the sites' icons, and shares each batch's results with the network.
struct NodeFetcher<'a> {
    rolling: Rolling<'a>,
    inner: &'a Inner,
    net: Option<&'a plumb_net::NetHandle>,
    icons: &'a IconStore,
    /// When this part of the crawl started.
    started: Instant,
    /// Batches finished in this part.
    batches: usize,
    /// Stopped to put an index of the crawl so far in service
    /// ([`NodeConfig::index_during_crawl_every`](super::NodeConfig)).
    checkpoint: bool,
}

impl Fetcher for NodeFetcher<'_> {
    fn ahead(&self) -> usize {
        self.rolling.ahead()
    }

    fn start(&mut self, targets: Vec<CrawlTarget>) {
        self.rolling.start(targets);
    }

    fn finished(&mut self, n: usize) -> Option<(Vec<String>, Vec<CrawlResult>)> {
        // Background updates turned off or a limit reached: pause; the
        // homepages in flight are tried again later.
        if self.inner.pause_reason().is_some() {
            return None;
        }
        // Time to index what was crawled so far; the homepages in flight
        // are tried again when the crawl goes on.
        if self.batches > 0 && self.started.elapsed() >= self.inner.config.index_during_crawl_every
        {
            self.checkpoint = true;
            return None;
        }
        self.batches += 1;
        let crawler = &mut self.rolling.crawler;
        let (inner, net, icons) = (self.inner, self.net, self.icons);
        self.rolling.runtime.block_on(async move {
            let results = tokio::select! {
                results = Rolling::next_results(crawler, n) => results,
                () = inner.stopped() => return None,
            };
            save_icons(icons, &results);
            if let Some(net) = net {
                // Shared before it is saved here: a batch the offline
                // check throws away holds few records anyway.
                let records = plumb_crawl::to_records(&results);
                if let Err(err) = net.publish(records).await {
                    warn!("cannot publish crawl results to the network: {err:#}");
                }
            }
            let done = results.iter().map(|result| result.domain.clone()).collect();
            Some((done, results))
        })
    }
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

/// Crawls the homepages left in the round, saving each batch as it goes
/// (see [`crate::crawl`]), then builds an index. `None` when there turned
/// out to be nothing to crawl and nothing new to index.
fn crawl_and_build(inner: &Inner, handle: &Handle) -> Result<Option<ServingIndex>> {
    let left = inner.saved().crawl_left;
    inner.set_step(Step::Crawling, "Reading the site records");
    let mut set = load_records(&inner.paths.records)?;
    let mut store = RecordStore::open(&inner.paths.records);
    inner.check_stop()?;

    let window = RECRAWL_AFTER_DAYS * SECONDS_PER_DAY;
    let net = network::handle(inner).cloned();
    let now = now_unix();
    // In the network, only the sites assigned to this node today, or,
    // crawling any site, this node's slice among its trusted crawlers.
    let group = match &net {
        Some(net) if inner.config.crawl_any_site => Some(net.crawl_group(&inner.config.crawl_with)),
        _ => None,
    };
    if let Some(group) = &group {
        info!(
            "crawling this node's slice of the sites, shared with {} other crawlers",
            group.len() - 1
        );
    }
    let candidates = set.iter().filter(|record| match (&net, &group) {
        (None, _) => true,
        (Some(net), Some(group)) => net.owns_slice(group, &record.domain),
        (Some(net), None) => net.is_assigned(&record.domain, now),
    });
    // Sites whose crawlers disagree are fetched whether assigned or not:
    // this node's own crawl settles the dispute (see plumb_net::agree).
    let rechecks: Vec<CrawlTarget> = match &net {
        Some(net) => handle
            .block_on(net.rechecks(MAX_RECHECKS_PER_ROUND))
            .unwrap_or_default()
            .iter()
            .filter_map(|domain| set.get(domain))
            .filter(|record| {
                record
                    .crawled_at
                    .max(record.crawl_attempted_at)
                    .is_none_or(|last| last + RECHECK_AGAIN_AFTER_SECS <= now)
            })
            .map(target_for)
            .collect(),
        None => Vec::new(),
    };
    let candidates =
        candidates.filter(|record| !rechecks.iter().any(|target| target.domain == record.domain));
    // Sites crawled before nodes kept icons are due again for theirs.
    let icons = IconStore::new(&inner.paths.icons);
    let noted = icons.noted();
    let rest = select_targets_with(
        candidates,
        left.saturating_sub(rechecks.len()),
        now,
        window,
        |record| due_for_icon(record, &noted),
    );
    drop(noted);
    let mut targets = rechecks;
    targets.extend(rest);
    if targets.is_empty() {
        info!("no homepage is due for a crawl");
    } else {
        info!(
            "crawling {} homepages, {CRAWL_BATCH_SIZE} at a time",
            targets.len()
        );
        inner.set_step(Step::Crawling, "Crawling homepages");
        inner.set_progress(0, targets.len(), "homepages");
        let cfg = CrawlConfig {
            use_system_proxy: inner.config.use_system_proxy,
            concurrency: inner
                .settings()
                .workload
                .concurrency_or(inner.config.crawl_concurrency),
            ..CrawlConfig::default()
        };
        // Homepages counted in the saved state so far.
        let counted = std::cell::Cell::new(0);
        let mut fetcher = NodeFetcher {
            rolling: Rolling {
                concurrency: cfg.concurrency,
                crawler: HomepageCrawler::new(cfg.clone()),
                runtime: handle,
            },
            inner,
            net: net.as_deref(),
            icons: &icons,
            started: Instant::now(),
            batches: 0,
            checkpoint: false,
        };
        let totals = crawl_rolling(
            &mut set,
            &targets,
            CRAWL_BATCH_SIZE,
            &mut store,
            &[],
            &mut fetcher,
            |totals| {
                inner.set_progress(totals.attempted, targets.len(), "homepages");
                let visited = totals.attempted - counted.replace(totals.attempted);
                let downloaded = cfg.downloaded.swap(0, Ordering::Relaxed);
                let now = now_unix();
                inner.update_saved(|saved| {
                    saved.crawl_left = left.saturating_sub(totals.attempted);
                    saved.index_stale = true;
                    saved.homepages_visited += visited as u64;
                    saved.add_downloaded(downloaded, now);
                })?;
                inner.recount_disk();
                Ok(())
            },
        )?;
        match totals.end {
            RunEnd::Finished => {}
            RunEnd::Stopped if inner.stopping() => return Err(Stopped.into()),
            RunEnd::Stopped if fetcher.checkpoint => {
                // Index what was crawled so far; the next turn goes on.
                info!(
                    "indexing the {} homepages crawled so far; {} left in this round",
                    totals.attempted,
                    inner.saved().crawl_left
                );
                return build(inner, set.into_sorted_vec()).map(Some);
            }
            RunEnd::Stopped => {
                // Paused by the settings or a limit: index what was crawled
                // so far, and go on from there later.
                let reason = inner.pause_reason().unwrap_or_else(|| "Paused".to_owned());
                info!(
                    "{reason}: pausing the crawl after {} homepages",
                    totals.attempted
                );
                inner.journal.info(format!(
                    "{reason}: the crawl stopped after {} homepages and goes on later",
                    group_thousands(totals.attempted as u64)
                ));
                if totals.attempted == 0 {
                    return Ok(None);
                }
                return build(inner, set.into_sorted_vec()).map(Some);
            }
            RunEnd::Offline(offline) => {
                let proxy = if inner.config.use_system_proxy {
                    ""
                } else {
                    ". If this machine reaches the internet only through a proxy, turn on \
                     use_system_proxy (plumb run --use-system-proxy)"
                };
                bail!("{offline}{proxy}");
            }
        }
        inner.check_stop()?;
        let o = &totals.outcomes;
        inner.journal.info(format!(
            "Visited {} homepages: {} fetched, {} turned Plumb away (robots.txt), {} \
             failed, {} new sites found",
            group_thousands(totals.attempted as u64),
            group_thousands(o.fetched as u64),
            group_thousands(o.robots_disallowed as u64),
            group_thousands(o.errors() as u64),
            group_thousands(totals.discovered as u64)
        ));
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

/// Whether the last try at `record`'s homepage fetched it.
/// Whether a site last crawled before nodes kept icons is due again for
/// its icon. A newer crawl without an icon here came from the network
/// (icons aren't shared yet); fetching it again would undo the point of
/// sharing crawls, so its icon waits for its next regular crawl.
fn due_for_icon(record: &SiteRecord, noted: &HashSet<String>) -> bool {
    last_crawl_answered(record)
        && record.crawled_at.is_some_and(|at| at < ICONS_KEPT_SINCE)
        && !noted.contains(&record.domain)
}

fn last_crawl_answered(record: &SiteRecord) -> bool {
    record
        .crawled_at
        .is_some_and(|at| at >= record.crawl_attempted_at.unwrap_or(0))
}

/// Notes the icon, or the lack of one, of every homepage fetched. A site
/// whose homepage was not fetched keeps what an earlier crawl found.
fn save_icons(icons: &IconStore, results: &[CrawlResult]) {
    for result in results {
        if let CrawlOutcome::Fetched(page) = &result.outcome {
            if let Err(err) = icons.put(&result.domain, page.icon.as_deref()) {
                warn!("cannot save the icon of {}: {err}", result.domain);
                return;
            }
        }
    }
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
    let buckets = network::wants_buckets(inner);
    let steps = if buckets { 2 } else { 1 };
    inner.set_progress(0, steps, "steps");
    let started = Instant::now();
    let stats = build_index(&dir, &records)
        .with_context(|| format!("building the index in {}", dir.display()))?;
    if buckets {
        inner.set_step(Step::Indexing, "Writing the buckets other nodes search");
        inner.set_progress(1, steps, "steps");
    }
    network::build_buckets(inner, &dir, &records);
    drop(records);
    let index = match ServingIndex::open(id, &dir, inner.rank) {
        Ok(index) => index,
        Err(err) => {
            let _ = store::remove_index(&dir);
            return Err(err.context(format!("opening the new index in {}", dir.display())));
        }
    };
    inner.journal.info(format!(
        "Search index rebuilt: {} sites in {}",
        group_thousands(stats.docs),
        duration_words(started.elapsed().as_secs().max(1))
    ));
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
    inner.last_build.store(now, Ordering::SeqCst);
    inner.update_saved(|saved| {
        saved.index_stale = false;
        saved.network_pending = 0;
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

/// Runs `work` on a blocking thread and waits for it to end, even when the
/// node is stopping: the work checks for that itself, between steps where
/// stopping leaves nothing half-done. A panic becomes an error. The memory
/// the work freed (records, index buffers) is then handed back to the
/// system.
async fn blocking<T, F>(inner: &Arc<Inner>, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Inner) -> Result<T> + Send + 'static,
{
    let inner = Arc::clone(inner);
    let done = tokio::task::spawn_blocking(move || work(&inner))
        .await
        .context("background work crashed");
    // Not on an async thread: handing back a few hundred megabytes takes
    // tens of milliseconds.
    let _ = tokio::task::spawn_blocking(crate::release_freed_memory).await;
    done?
}

/// Deletes the directories of replaced indexes that no search has open any
/// more.
async fn sweep(inner: &Arc<Inner>) {
    inner.recount_disk();
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
        // Records from other nodes are folded in as they pile up; whether
        // they rebuild the index is up to step().
        if inner.inbox_records.load(Ordering::SeqCst) >= REBUILD_AFTER_RECORDS {
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
            // A refresh request or new settings: look again at what to do.
            () = inner.wake.notified() => return,
            () = inner.stopped() => return,
        }
        sweep(inner).await;
    }
}

/// Waits between tries of failed work: `first`, then twice as long after
/// each failure in a row, up to `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Backoff {
    first: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub(super) fn new(first: Duration, max: Duration) -> Self {
        Backoff {
            first,
            max: max.max(first),
            next: first,
        }
    }

    /// The wait after one more failure.
    pub(super) fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.max);
        delay
    }

    /// Back to the first wait, after a success.
    pub(super) fn reset(&mut self) {
        self.next = self.first;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_sites_whose_last_crawl_answered_wait_for_an_icon() {
        let mut record = SiteRecord::new("a.com");
        assert!(!last_crawl_answered(&record), "never crawled");
        record.crawled_at = Some(10);
        assert!(last_crawl_answered(&record));
        record.crawl_attempted_at = Some(10);
        assert!(last_crawl_answered(&record));
        record.crawl_attempted_at = Some(20);
        assert!(!last_crawl_answered(&record), "the last try failed");
    }

    #[test]
    fn fetched_homepages_note_their_icon_or_its_lack() {
        use plumb_crawl::{CrawledPage, PageMeta};
        let dir = tempfile::tempdir().unwrap();
        let icons = IconStore::new(dir.path());
        let page = |domain: &str, icon: Option<Vec<u8>>| CrawlResult {
            domain: domain.into(),
            outcome: CrawlOutcome::Fetched(CrawledPage {
                domain: domain.into(),
                final_url: format!("https://{domain}/"),
                status: 200,
                fetched_at: 1,
                meta: PageMeta::default(),
                icon,
            }),
        };
        let results = [
            page("a.com", Some(b"png".to_vec())),
            page("b.com", None),
            CrawlResult {
                domain: "c.com".into(),
                outcome: CrawlOutcome::RobotsDisallowed,
            },
        ];
        save_icons(&icons, &results);
        assert_eq!(icons.get("a.com").as_deref(), Some(&b"png"[..]));
        let noted = icons.noted();
        assert!(noted.contains("a.com") && noted.contains("b.com"));
        assert!(!noted.contains("c.com"));
    }

    #[test]
    fn only_sites_crawled_before_icons_are_due_for_one() {
        let crawled = |domain: &str, at: u64| {
            let mut record = SiteRecord::new(domain);
            record.crawled_at = Some(at);
            record
        };
        let noted: HashSet<String> = ["noted.com".to_string()].into();
        let old = ICONS_KEPT_SINCE - 1;
        assert!(due_for_icon(&crawled("old.com", old), &noted));
        assert!(!due_for_icon(&crawled("noted.com", old), &noted));
        // Crawled since, by another node: waits for its regular recrawl.
        assert!(!due_for_icon(
            &crawled("network.com", ICONS_KEPT_SINCE + 60),
            &noted
        ));
        assert!(!due_for_icon(&SiteRecord::new("never.com"), &noted));
    }

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
