//! The background work of a node: setup on first start, crawls, index
//! builds and the refresh schedule, with retries after failures.
//!
//! Each turn of [`run`] looks at what is on disk and in the saved state and
//! does the next piece of work, so the same code resumes after a restart:
//!
//! 1. no records file: take the best sites of a trusted node in the
//!    network, or else download the Tranco list, and put a quick first
//!    index of them in service ([`set_up`]);
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

use std::borrow::Borrow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use plumb_core::{
    now_unix, parent_domain, RecordSet, SiteRecord, SITES_VERSION, SUBDOMAIN_SITE_NAMES,
};
use plumb_crawl::{CrawlConfig, CrawlOutcome, CrawlResult, CrawlTarget, HomepageCrawler};
use plumb_index::build_index;
use plumb_ingest::{
    attach_facts, attach_intros, download, facts, intros, kind_sites, load_cc_domain_ranks,
    load_intros, load_misread_official_sites, load_site_facts, load_tranco,
    load_wikidata_official_sites, Builder,
};
use tokio::runtime::Handle;
use tracing::{info, warn};

use super::network::{self, NETWORK_REBUILD_GAP, REBUILD_AFTER_RECORDS};
use super::round::{RoundSite, RoundSites};
use super::store::{self, SavedState};
use super::trim::Keep;
use super::{Inner, NodeConfig, ServingIndex, Step, Stopped};
use crate::crawl::{
    crawl_rolling, due_at, select_targets, select_targets_with, target_for, CrawlSet, CrawlSite,
    Fetcher, Rolling, RunEnd, CRAWL_BATCH_SIZE, SECONDS_PER_DAY,
};
use crate::icons::IconStore;
use crate::records::{load_records, replace_records, sorted_by_link_score, Change, RecordStore};
use crate::web::{duration_words, group_thousands};

/// A homepage fetched or answered this recently is not due for a crawl, as
/// with `plumb crawl --skip-crawled-within-days 30`. Sites that could not
/// be reached are retried sooner (see [`crate::crawl`]).
const RECRAWL_AFTER_DAYS: u64 = 30;

/// Sites about a node's focus topics get at most one in this many of a
/// round's homepages...
const FOCUS_SHARE_OF_ROUND: usize = 2;
/// ...and are due again this many times as soon as other sites.
const FOCUS_RECRAWL_FASTER: u64 = 2;

/// Nodes keep site icons from crawls since about this time (Unix seconds,
/// 2026-10-02). A site crawled before it is due again for its icon.
const ICONS_KEPT_SINCE: u64 = 1_791_000_000;

/// Nodes keep sites' key pages (sitelinks) from crawls since about this
/// time (Unix seconds, 2026-10-05 08:46 UTC). A site crawled before it,
/// with none, is due again for them, best-known sites first.
const KEY_PAGES_KEPT_SINCE: u64 = 1_791_190_000;

/// The best-known sites due for key pages that a round crawls whether or not
/// they are this node's to crawl today: a site outside its share would
/// otherwise keep no sitelinks here until another node's regular recrawl.
const KEY_PAGES_CATCH_UP_PER_ROUND: usize = 200;

/// Well-known sites no crawl has fetched yet (this node's or a trusted
/// crawler's) that a round crawls whether or not they are this node's to
/// crawl today, at most this many, best-known first. A popular site outside
/// the node's daily share can otherwise wait weeks for a first read, and
/// until then search knows nothing of what it is. They come out of the
/// round's budget, not on top of it. About 180,000 such sites were waiting
/// in October 2026; an hpc test crawl of 60,000 of them fixed three test
/// searches (weather.com, stability.ai) and broke one.
const FIRST_FETCH_CATCH_UP_PER_ROUND: usize = 500;

/// Link score a site needs for [`first_fetch_catch_up`]: about the top
/// 60,000 of the Tranco list, or any official site.
const FIRST_FETCH_MIN_SCORE: f32 = 0.3;

/// Sites without an icon here that a crawl round fetches just the icon of,
/// most linked first: crawls that came from the network before nodes shared
/// icons, or through filling, carry none, and those sites would otherwise
/// show a letter until their next crawl.
const ICON_CATCH_UP_PER_ROUND: usize = 1_000;

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
    // New topics: the sites kept for the old ones go, and the index is
    // built without them, before filling takes sites for the new ones.
    // Only under a storage limit: a node with none never loses sites.
    if inner.fill_state().prune
        && !inner.config.crawl_only
        && inner.settings().storage_limit_mb > 0
        && inner.saved().crawl_left == 0
    {
        let built = blocking(inner, |inner| {
            let _records = inner.hold_records();
            inner.set_step(Step::Indexing, "Making room for sites about new topics");
            let mut set = load_records(&inner.paths.records)?;
            inner.check_stop()?;
            if inner.prune_for_new_topics(&mut set)? > 0 {
                replace_records(&inner.paths.records, sorted_by_link_score(&set))?;
            }
            build(inner, &sorted_by_link_score(&set))
        })
        .await?;
        put_in_service(inner, Some(built), false).await?;
        return Ok(Next::Continue);
    }
    // Over the storage limit: the least useful sites go, before the limit
    // pauses crawling (see super::trim).
    if super::trim::due(inner) {
        if let Some(built) = blocking(inner, super::trim::trim).await? {
            put_in_service(inner, Some(built), false).await?;
        }
        return Ok(Next::Continue);
    }
    // Enough records from other nodes rebuild the index, but no sooner
    // than NETWORK_REBUILD_GAP (or a few times the last build) after it.
    let network_rebuild_at = (inner.saved().network_pending >= REBUILD_AFTER_RECORDS
        && !inner.config.crawl_only)
        .then(|| network_rebuild_at(inner));
    if network_rebuild_at.is_some_and(|at| at <= now_unix()) && !inner.saved().index_stale {
        if inner.saved().crawl_left > 0 {
            // The round under way builds them in when it ends.
            inner.update_saved(|saved| saved.index_stale = true)?;
        } else {
            // Not a refresh: the next one stays due when it was, or
            // records arriving every half hour would put it off forever.
            rebuild(inner, false).await?;
            return Ok(Next::Continue);
        }
    }
    let saved = inner.saved();
    if !inner.config.crawl_only
        && (inner.current().is_none() || (saved.index_stale && saved.crawl_left == 0))
    {
        rebuild(inner, true).await?;
        return Ok(Next::Continue);
    }
    if saved.sites_version < SITES_VERSION && !saved.quick_start {
        refold_seed(inner).await?;
        return Ok(Next::Continue);
    }
    if missing_buckets(inner) {
        // Once per start, so a build that cannot write them is not retried
        // in a loop.
        inner.buckets_rebuilt.store(true, Ordering::SeqCst);
        info!("rebuilding the index to add the buckets private search and the network need");
        rebuild(inner, false).await?;
        return Ok(Next::Continue);
    }
    // Crawling only, the sites set up with (from a trusted node, or the
    // Tranco list) and those filling brings are crawled without the rest of
    // the seed data, whose fold holds every record.
    let wikidata_due =
        (saved.wikidata_missing && !inner.config.crawl_only).then(|| inner.wikidata_retry_at());
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
        // Over the storage limit: look again when trimming is due.
        let until = super::trim::next_due(inner).map_or(until, |due| due.min(until));
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
/// [`NETWORK_REBUILD_GAP`], or [`BUILD_GAP_PER_BUILD`] times as long as the
/// last build took when that is longer, has passed since the last build
/// (right away when none was built since the node started).
fn network_rebuild_at(inner: &Inner) -> u64 {
    match inner.last_build.load(Ordering::SeqCst) {
        0 => 0,
        last => last.saturating_add(network_rebuild_gap(
            inner.last_build_took.load(Ordering::SeqCst),
        )),
    }
}

/// Seconds between index builds that records from other nodes ask for,
/// after a build that took `took` seconds: a build of millions of sites on
/// a small server takes most of half an hour, and one every half hour
/// would leave it building more often than not, with searches waiting on
/// the memory and disk the builds take.
fn network_rebuild_gap(took: u64) -> u64 {
    NETWORK_REBUILD_GAP
        .as_secs()
        .max(took.saturating_mul(BUILD_GAP_PER_BUILD))
}

/// How many times as long as a build takes the node waits before another
/// that records from other nodes ask for: building at most a quarter of
/// the time.
const BUILD_GAP_PER_BUILD: u64 = 3;

fn idle_detail(config: &NodeConfig) -> &'static str {
    if config.crawl_only {
        "Crawling only; up to date until the next crawl round"
    } else if config.refresh_every.is_some() {
        "Up to date until the next refresh"
    } else {
        "Up to date; refreshing is off"
    }
}

/// First start: in the network, takes the best sites of a node it trusts
/// (see [`super::fill::seed_from_network`]), which carry everything the
/// seed downloads give, so nothing is downloaded from outside. Otherwise,
/// or when no trusted node answers, downloads the Tranco list alone, keeps
/// its best sites in a new records file and puts a first index of them in
/// service, so search works within a minute or two. The rest of the seed
/// data, whose downloads take many minutes (Wikidata's above all), is
/// folded in right after by [`complete_seed`].
async fn set_up(inner: &Arc<Inner>) -> Result<()> {
    let from_network = tokio::select! {
        records = super::fill::seed_from_network(inner) => records?,
        () = inner.stopped() => return Err(Stopped.into()),
    };
    if let Some((records, downloaded)) = from_network {
        let built = blocking(inner, move |inner| {
            let mut fresh = SavedState::fresh(inner.config.initial_crawl);
            fresh.add_downloaded(downloaded, now_unix());
            inner.update_saved(|saved| *saved = fresh)?;
            save_seed_records(inner, &records)?;
            inner.check_stop()?;
            build_unless_crawling_only(inner, &records)
        })
        .await?;
        return put_in_service(inner, built, true).await;
    }
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
        build_unless_crawling_only(inner, &records)
    })
    .await?;
    put_in_service(inner, built, true).await
}

/// [`build`], or crawling only ([`NodeConfig::crawl_only`]), no index:
/// `None`, noting how many sites the records hold.
fn build_unless_crawling_only<R: Borrow<SiteRecord>>(
    inner: &Inner,
    records: &[R],
) -> Result<Option<ServingIndex>> {
    if inner.config.crawl_only {
        inner
            .round_sites
            .store(records.len() as u64, Ordering::SeqCst);
        return Ok(None);
    }
    build(inner, records).map(Some)
}

/// [`build_from_file`], or crawling only ([`NodeConfig::crawl_only`]),
/// no index: `None`. The records are saved already, and the next round
/// folds their journal in as it reads them.
fn build_from_file_unless_crawling_only(inner: &Inner) -> Result<Option<ServingIndex>> {
    if inner.config.crawl_only {
        return Ok(None);
    }
    build_from_file(inner).map(Some)
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
    let from_network = tokio::select! {
        done = complete_seed_from_network(inner) => done?,
        () = inner.stopped() => return Err(Stopped.into()),
    };
    if from_network {
        return Ok(());
    }
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
        let _records = inner.hold_records();
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
        take_back_misread(&files, &mut set);
        set.split_subdomain_sites(&seed);
        set.extend(seed);
        let titled = set
            .get(plumb_core::HOME_SITE)
            .is_some_and(|home| home.title.is_some());
        if let Some(change) = home_site_change(inner, titled) {
            change.apply(&mut set);
        }
        inner.check_stop()?;
        let records = sorted_by_link_score(&set);
        inner.set_step(
            Step::Ingesting,
            format!(
                "Saving {} site records",
                group_thousands(records.len() as u64)
            ),
        );
        replace_records(&inner.paths.records, records.iter().copied())?;
        inner.update_saved(|saved| {
            saved.wikidata_missing = wikidata_missing;
            saved.quick_start = false;
            saved.index_stale = true;
            saved.sites_version = SITES_VERSION;
        })?;
        info!(
            "folded the seed data into the records: {} sites (there were {before})",
            records.len(),
        );
        inner.check_stop()?;
        build(inner, &records)
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

/// The rest of the seed data from the network rather than Wikidata: a
/// node that set up from the Tranco list because no trusted node answered
/// in time asks again before it goes to Wikidata and Wikipedia, and takes
/// the best sites of a trusted node's list, which carry the official
/// sites, facts and intros, as filling does. `false` when no trusted node
/// answers now either.
async fn complete_seed_from_network(inner: &Arc<Inner>) -> Result<bool> {
    let Some((records, downloaded)) = super::fill::seed_from_network(inner).await? else {
        return Ok(false);
    };
    let n = records.len() as u64;
    blocking(inner, move |inner| {
        super::network::append_inbox(inner, &records)?;
        inner.update_saved(|saved| {
            saved.wikidata_missing = false;
            saved.quick_start = false;
            saved.sites_version = SITES_VERSION;
        })
    })
    .await?;
    inner.add_downloaded(downloaded)?;
    inner.inbox_records.fetch_add(n, Ordering::SeqCst);
    inner.wake.notify_one();
    info!("took {n} sites from a trusted node instead of asking Wikidata");
    inner.journal.info(format!(
        "Took the rest of the seed data from a trusted node in the network ({n} sites) instead of Wikidata"
    ));
    Ok(true)
}

/// Brings records made before the lists of sites on subdomains, or before
/// email addresses were refused as websites ([`SITES_VERSION`]), up to
/// date from the seed files already on disk, without downloading anything:
/// sites such as scholar.google.com, which were part of their parent
/// domain, get records of their own with their official names, and claims
/// misread from email addresses are taken back. Only the few records that
/// change are read from the seed files; the changes go into the records'
/// journal ([`subdomain_changes`]) and the index is rebuilt as after a
/// crawl, so this needs no more memory than a rebuild. A node stopped part
/// way does it again on its next start: the changes come out the same.
async fn refold_seed(inner: &Arc<Inner>) -> Result<()> {
    info!("the lists of sites on subdomains changed: updating the records from the seed data");
    blocking(inner, |inner| {
        let changes = subdomain_changes(&inner.paths.seed)?;
        let n = changes.len();
        inner.check_stop()?;
        {
            let _records = inner.hold_records();
            RecordStore::open(&inner.paths.records).save(&changes)?;
        }
        inner.update_saved(|saved| {
            saved.sites_version = SITES_VERSION;
            saved.index_stale |= n > 0;
        })?;
        info!("updated the records from the seed data: {n} changes");
        Ok(())
    })
    .await
}

/// The changes [`refold_seed`] makes, from the Wikidata files in `seed`:
/// for each claim misread from an email address
/// ([`plumb_ingest::OfficialSite::misread`]), taking it back and merging
/// the domain's own claims again; for each subdomain site with a claim or
/// a built-in name ([`SUBDOMAIN_SITE_NAMES`]), its record, made where its
/// parent domain has one ([`Change::SubdomainSite`]).
fn subdomain_changes(seed: &Path) -> Result<Vec<Change>> {
    let files = [
        seed.join(download::WIKIDATA_FILE_NAME),
        seed.join(kind_sites::KIND_SITES_FILE_NAME),
    ];
    let files: Vec<&PathBuf> = files.iter().filter(|path| path.is_file()).collect();
    let mut misread = Vec::new();
    for path in &files {
        misread.extend(
            load_misread_official_sites(path)?
                .into_iter()
                .filter(|site| site.is_root_homepage()),
        );
    }
    let misread_domains: HashSet<&str> = misread.iter().map(|site| site.domain.as_str()).collect();
    let mut sites = Vec::new();
    for path in &files {
        sites.extend(
            load_wikidata_official_sites(path)?
                .into_iter()
                .filter(|site| {
                    parent_domain(&site.domain).is_some()
                        || misread_domains.contains(site.domain.as_str())
                }),
        );
    }
    let facts = seed.join(facts::FACTS_FILE_NAME);
    if facts.is_file() && !(sites.is_empty() && misread.is_empty()) {
        let facts = load_site_facts(&facts)?;
        attach_facts(&mut sites, &facts);
        attach_facts(&mut misread, &facts);
    }
    let intros = seed.join(intros::INTROS_FILE_NAME);
    if intros.is_file() && !sites.is_empty() {
        attach_intros(&mut sites, &load_intros(&intros)?);
    }
    let mut changes: Vec<Change> = misread
        .iter()
        .map(|site| Change::TakeBack {
            domain: site.domain.clone(),
            names: std::iter::once(site.label.trim().to_string())
                .chain(site.names.iter().cloned())
                .collect(),
        })
        .collect();
    let mut builder = Builder::new();
    builder.add_official_sites(&sites);
    for record in builder.finish(None) {
        changes.push(if parent_domain(&record.domain).is_some() {
            Change::SubdomainSite { record }
        } else {
            Change::Merge { record }
        });
    }
    for (site, name) in SUBDOMAIN_SITE_NAMES {
        let mut record = SiteRecord::new(*site);
        record.signals.official_site = true;
        record.add_alias(name);
        changes.push(Change::SubdomainSite { record });
    }
    Ok(changes)
}

/// Takes back what Wikidata claims that earlier versions misread gave the
/// records ([`plumb_ingest::OfficialSite::misread`]), for a full fold of
/// the seed: the domain's own seed record adds its facts back after.
fn take_back_misread(files: &SeedFiles, set: &mut RecordSet) {
    let Ok(wikidata) = &files.wikidata else {
        return;
    };
    for path in [wikidata, &files.kind_sites] {
        if !path.is_file() {
            continue;
        }
        match load_misread_official_sites(path) {
            Ok(sites) => {
                for site in sites.iter().filter(|site| site.is_root_homepage()) {
                    set.take_back_official_site(&site.domain, &[site.label.trim()]);
                }
            }
            Err(err) => warn!("could not read {} again: {err:#}", path.display()),
        }
    }
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
    /// Wikipedia's first sentences about the best-known of them; used when
    /// the file exists.
    intros: PathBuf,
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

    let intros = seed.join(intros::INTROS_FILE_NAME);
    if facts.is_file() && !is_recent(&intros) {
        inner.set_step(
            Step::Downloading,
            "Asking Wikipedia what the best-known of those sites are",
        );
        let downloaded = intros::download_wikipedia_intros(
            &client,
            &sources.wikidata_sparql_url,
            &sources.wikipedia_api_url,
            seed,
            &facts,
            sources.wikidata_pacing,
        )
        .await;
        if let Err(err) = downloaded {
            // They help search by meaning, but are not needed: carry on.
            warn!("could not get Wikipedia's intros, going on without them: {err:#}");
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
        intros,
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
        intros: seed.join(intros::INTROS_FILE_NAME),
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
        if files.intros.is_file() {
            match load_intros(&files.intros) {
                Ok(intros) => attach_intros(&mut sites, &intros),
                Err(err) => warn!("going on without Wikipedia's intros: {err:#}"),
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

/// Builds a new index of the records file and puts it in service; see
/// [`put_in_service`] for `ends_round`.
async fn rebuild(inner: &Arc<Inner>, ends_round: bool) -> Result<()> {
    let started = Instant::now();
    let built = blocking(inner, |inner| {
        let _records = inner.hold_records();
        build_from_file(inner)
    })
    .await?;
    inner
        .last_build_took
        .store(started.elapsed().as_secs(), Ordering::SeqCst);
    put_in_service(inner, Some(built), ends_round).await
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
        // Crawling only, there is no index to put the crawl in.
        if self.batches > 0
            && !self.inner.config.crawl_only
            && self.started.elapsed() >= self.inner.config.index_during_crawl_every
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
            inner.news.note_feeds(&results);
            if let Some(net) = net {
                // Shared before it is saved here: a batch the offline
                // check throws away holds few records anyway.
                let mut records = plumb_crawl::to_records(&results);
                crate::icons::attach(&mut records, &results);
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
    match blocking(inner, move |inner| crawl_and_build(inner, &handle)).await? {
        Done::Built(built) => put_in_service(inner, built, true).await,
        Done::Nothing => Ok(()),
    }
}

/// The change that gives the records the network's own site
/// ([`plumb_core::home_site_record`]) while they have no title for it
/// (`titled` false): never crawled, or only through a bot check. `None`
/// unless [`super::NodeConfig::crawl_home_site`].
fn home_site_change(inner: &Inner, titled: bool) -> Option<Change> {
    if !inner.config.crawl_home_site {
        return None;
    }
    (!titled).then(|| Change::Merge {
        record: plumb_core::home_site_record(),
    })
}

/// What [`crawl_and_build`] did. Made once a round, so its size does not
/// matter.
#[allow(clippy::large_enum_variant)]
enum Done {
    /// Crawled, and built an index of the records (none when crawling
    /// only), for [`put_in_service`].
    Built(Option<ServingIndex>),
    /// Nothing to crawl and nothing new to index.
    Nothing,
}

/// Crawls the homepages left in the round, saving each batch as it goes
/// (see [`crate::crawl`]), then builds an index.
fn crawl_and_build(inner: &Inner, handle: &Handle) -> Result<Done> {
    let left = inner.saved().crawl_left;
    let _records = inner.hold_records();
    inner.set_step(Step::Crawling, "Reading the site records");
    let topics = inner.focus_topics();
    let mut set = RoundSites::load(&inner.paths.records, topics.clone(), Keep::of(inner))?;
    set.hold_new_sites(!inner.config.take_new_sites);
    inner
        .round_sites
        .store(set.iter().len() as u64, Ordering::SeqCst);
    let mut store = RecordStore::open(&inner.paths.records);
    inner.check_stop()?;
    let titled = set
        .get(plumb_core::HOME_SITE)
        .is_some_and(RoundSite::titled);
    if let Some(change) = home_site_change(inner, titled) {
        store.save(std::slice::from_ref(&change))?;
        set.apply(change);
    }

    let window = RECRAWL_AFTER_DAYS * SECONDS_PER_DAY;
    let net = network::handle(inner).cloned();
    let now = now_unix();
    drop_dead_sites(inner, &mut set, &mut store, now)?;
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
        (Some(net), Some(group)) => net.owns_slice(group, record.domain()),
        (Some(net), None) => net.is_assigned(record.domain(), now),
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
                    .crawled_at()
                    .max(record.crawl_attempted_at())
                    .is_none_or(|last| last + RECHECK_AGAIN_AFTER_SECS <= now)
            })
            .map(target_for)
            .collect(),
        None => Vec::new(),
    };
    let mut rechecks = rechecks;
    // The network's own site, on every node whatever is assigned, so it is
    // crawled even where its own server cannot reach itself.
    let home = set
        .get(plumb_core::HOME_SITE)
        .filter(|_| inner.config.crawl_home_site);
    if let Some(home) = home {
        if due_at(home, window).is_none_or(|due| due <= now)
            && !rechecks.iter().any(|target| target.domain == home.domain())
        {
            rechecks.push(target_for(home));
        }
    }
    for record in key_page_catch_up(&set)
        .into_iter()
        .chain(first_fetch_catch_up(&set, now, window))
    {
        if !rechecks
            .iter()
            .any(|target| target.domain == record.domain())
        {
            rechecks.push(target_for(record));
        }
    }
    let candidates: Vec<&RoundSite> = candidates
        .filter(|record| {
            !rechecks
                .iter()
                .any(|target| target.domain == record.domain())
        })
        .collect();
    let budget = left.saturating_sub(rechecks.len());
    // Sites about the node's focus topics come first, up to half the
    // round, and are due again twice as soon.
    let focused = if topics.is_empty() {
        Vec::new()
    } else {
        select_targets(
            candidates.iter().copied().filter(|r| r.focused()),
            budget / FOCUS_SHARE_OF_ROUND,
            now,
            window / FOCUS_RECRAWL_FASTER,
        )
    };
    if !focused.is_empty() {
        info!(
            "{} homepages are about this node's focus topics",
            focused.len()
        );
    }
    let focused_domains: HashSet<&str> = focused.iter().map(|t| t.domain.as_str()).collect();
    // Sites crawled before nodes kept icons or key pages are due again for
    // them, and so are sites read by an older crawler (see
    // plumb_crawl::CRAWL_VERSION).
    let icons = IconStore::new(&inner.paths.icons);
    let noted = icons.noted();
    let rest = select_targets_with(
        candidates
            .iter()
            .copied()
            .filter(|r| !focused_domains.contains(r.domain())),
        budget - focused.len(),
        now,
        window,
        |record| {
            due_for_icon(record, &noted) || due_for_key_pages(record) || due_for_rereading(record)
        },
    );
    drop(noted);
    drop(focused_domains);
    let mut targets = rechecks;
    targets.extend(focused);
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
                drop((set, store));
                return build_from_file_unless_crawling_only(inner).map(Done::Built);
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
                    return Ok(Done::Nothing);
                }
                drop((set, store));
                return build_from_file_unless_crawling_only(inner).map(Done::Built);
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

    catch_up_icons(inner, handle, &set, &icons)?;
    inner.update_saved(|saved| saved.crawl_left = 0)?;
    if !inner.saved().index_stale {
        let now = now_unix();
        inner.update_saved(|saved| saved.last_refresh = Some(now))?;
        inner.refresh_requested.store(false, Ordering::SeqCst);
        return Ok(Done::Nothing);
    }
    inner.check_stop()?;
    // The records are read again a record at a time, rather than built
    // from the set held, so the build never holds every record.
    drop((set, store));
    build_from_file_unless_crawling_only(inner).map(Done::Built)
}

/// Whether the last try at `record`'s homepage fetched it.
/// Whether a site last crawled before nodes kept icons is due again for
/// its icon. A newer crawl without an icon here came from the network
/// without one; fetching it again would undo the point of sharing crawls,
/// so its icon is fetched on its own ([`catch_up_icons`]).
fn due_for_icon(record: &RoundSite, noted: &HashSet<String>) -> bool {
    last_crawl_answered(record)
        && record.crawled_at().is_some_and(|at| at < ICONS_KEPT_SINCE)
        && !noted.contains(record.domain())
}

/// Whether a site last crawled before nodes kept key pages, and without
/// any, is due again for them.
fn due_for_key_pages(record: &RoundSite) -> bool {
    last_crawl_answered(record)
        && !record.has_key_pages()
        && record
            .crawled_at()
            .is_some_and(|at| at < KEY_PAGES_KEPT_SINCE)
}

/// The best-linked sites, up to [`KEY_PAGES_CATCH_UP_PER_ROUND`], due for
/// key pages ([`due_for_key_pages`]), assigned to this node or not.
fn key_page_catch_up(set: &RoundSites) -> Vec<&RoundSite> {
    set.sorted_by_link_score()
        .into_iter()
        .filter(|record| due_for_key_pages(record))
        .take(KEY_PAGES_CATCH_UP_PER_ROUND)
        .collect()
}

/// The best-known sites, up to [`FIRST_FETCH_CATCH_UP_PER_ROUND`], that no
/// crawl has fetched yet and are due a try ([`due_at`]: never tried, or
/// waited out the pause after their last failed try), assigned to this node
/// or not.
fn first_fetch_catch_up(set: &RoundSites, now: u64, window: u64) -> Vec<&RoundSite> {
    let mut due: Vec<&RoundSite> = set
        .iter()
        .filter(|record| {
            record.crawled_at().is_none()
                && record.gone_at().is_none()
                && record.link_score() >= FIRST_FETCH_MIN_SCORE
                && due_at(*record, window).is_none_or(|due| due <= now)
        })
        .collect();
    due.sort_by(|a, b| {
        b.link_score()
            .total_cmp(&a.link_score())
            .then_with(|| a.domain().cmp(b.domain()))
    });
    due.truncate(FIRST_FETCH_CATCH_UP_PER_ROUND);
    due
}

/// Whether a site was last read by an older crawler than this one
/// ([`plumb_crawl::CRAWL_VERSION`]), so it is due again to be read anew.
// Never, while the version is still the first one.
#[allow(clippy::absurd_extreme_comparisons)]
fn due_for_rereading(record: &RoundSite) -> bool {
    last_crawl_answered(record) && record.crawl_version() < plumb_crawl::CRAWL_VERSION
}

/// Counts the sites that look dead ([`crate::dead`]) and, with
/// [`NodeConfig::drop_dead_sites`], takes them out: each is cut down to its
/// crawl marks and ranks, saved to the records' journal, and the index is
/// built again without them.
fn drop_dead_sites(
    inner: &Inner,
    set: &mut RoundSites,
    store: &mut RecordStore,
    now: u64,
) -> Result<()> {
    if !set
        .iter()
        .any(|record| crate::dead::looks_dead(record, now))
    {
        return Ok(());
    }
    let dead: Vec<String> = crate::dead::find_dead(|| set.iter(), now, RoundSite::kept)
        .into_iter()
        .map(|record| record.domain().to_owned())
        .collect();
    if dead.is_empty() {
        return Ok(());
    }
    if !inner.config.drop_dead_sites {
        info!(
            "{} sites look dead (no answer for weeks; the best known: {}); kept, since \
             --drop-dead-sites is off",
            dead.len(),
            dead.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
        );
        return Ok(());
    }
    let changes: Vec<Change> = dead
        .into_iter()
        .map(|domain| Change::Gone { domain, at: now })
        .collect();
    store.save(&changes)?;
    let count = changes.len();
    for change in changes {
        set.apply(change);
    }
    inner.update_saved(|saved| saved.index_stale = true)?;
    info!("took {count} dead sites out of the index");
    inner.journal.info(format!(
        "Took {} sites out of the index that no crawl has reached for weeks; any that \
         answer again come back",
        group_thousands(count as u64)
    ));
    Ok(())
}

fn last_crawl_answered(record: &impl CrawlSite) -> bool {
    record
        .crawled_at()
        .is_some_and(|at| at >= record.crawl_attempted_at().unwrap_or(0))
}

/// The best-linked sites, up to [`ICON_CATCH_UP_PER_ROUND`], whose last
/// crawl answered but which have no icon noted here: their icons are
/// fetched on their own ([`catch_up_icons`]).
fn icon_catch_up_targets(set: &RoundSites, noted: &HashSet<String>) -> Vec<CrawlTarget> {
    set.sorted_by_link_score()
        .into_iter()
        .filter(|record| last_crawl_answered(*record) && !noted.contains(record.domain()))
        .take(ICON_CATCH_UP_PER_ROUND)
        .map(|record| CrawlTarget {
            url: record.url().unwrap_or_default().to_owned(),
            known_url: None,
            domain: record.domain().to_owned(),
        })
        .collect()
}

/// Fetches just the icons of [`icon_catch_up_targets`] and notes what was
/// found, an empty file for none, so a site is not asked again before its
/// next crawl. Not a crawl: no record changes and nothing is published.
fn catch_up_icons(
    inner: &Inner,
    handle: &Handle,
    set: &RoundSites,
    icons: &IconStore,
) -> Result<()> {
    if inner.pause_reason().is_some() {
        return Ok(());
    }
    let targets = icon_catch_up_targets(set, &icons.noted());
    if targets.is_empty() {
        return Ok(());
    }
    info!(
        "fetching the icons of {} sites that have none here",
        targets.len()
    );
    inner.set_step(Step::Crawling, "Fetching site icons");
    let cfg = CrawlConfig {
        use_system_proxy: inner.config.use_system_proxy,
        concurrency: inner
            .settings()
            .workload
            .concurrency_or(inner.config.crawl_concurrency),
        ..CrawlConfig::default()
    };
    let found = handle.block_on(async {
        tokio::select! {
            found = plumb_crawl::fetch_site_icons(targets, &cfg) => Some(found),
            () = inner.stopped() => None,
        }
    });
    inner.add_downloaded(cfg.downloaded.swap(0, Ordering::Relaxed))?;
    let Some(found) = found else {
        return Err(Stopped.into());
    };
    let mut kept = 0;
    for (domain, icon) in &found {
        if let Err(err) = icons.put(domain, icon.as_deref()) {
            warn!("cannot save the icon of {domain}: {err}");
            return Ok(());
        }
        kept += usize::from(icon.is_some());
    }
    info!("found icons for {kept} of {} sites", found.len());
    Ok(())
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
pub(super) fn build<R: Borrow<SiteRecord>>(inner: &Inner, records: &[R]) -> Result<ServingIndex> {
    // Sites judged dead stay in the records, out of the index and the
    // buckets other nodes take.
    let records: Vec<&SiteRecord> = records
        .iter()
        .map(Borrow::borrow)
        .filter(|record| record.gone_at.is_none())
        .collect();
    let records = records.as_slice();
    let id = store::next_index_id(&inner.paths);
    let dir = inner.paths.index(id);
    inner.set_step(
        Step::Indexing,
        format!(
            "Building the search index of {} sites",
            group_thousands(records.len() as u64)
        ),
    );
    watch_feeds(inner, records);
    let buckets = network::wants_buckets(inner);
    let steps = if buckets { 2 } else { 1 };
    inner.set_progress(0, steps, "steps");
    let started = Instant::now();
    let stats = build_index(&dir, records)
        .with_context(|| format!("building the index in {}", dir.display()))?;
    if buckets {
        inner.set_step(Step::Indexing, "Writing the buckets other nodes search");
        inner.set_progress(1, steps, "steps");
    }
    network::build_buckets(inner, &dir, records);
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

/// Builds an index of the records file in a new numbered directory and
/// opens it, reading the file a record at a time ([`crate::outline`]): the
/// journal is folded into the file first, and no more than an outline of
/// each site is held at once, where loading the file would hold every
/// record. The same index as [`build`] of the whole set. A file that only a
/// whole set can merge (a site on two lines) is loaded and rewritten once.
/// Call it holding the records ([`Inner::hold_records`]).
pub(super) fn build_from_file(inner: &Inner) -> Result<ServingIndex> {
    inner.set_step(Step::Indexing, "Reading the site records");
    let Some(outlines) = crate::outline::outline(&inner.paths.records)? else {
        info!(
            "{} holds a site more than once: reading it whole to merge them",
            inner.paths.records.display()
        );
        let set = load_records(&inner.paths.records)?;
        RecordStore::open(&inner.paths.records).compact(&set)?;
        inner.check_stop()?;
        return build(inner, &sorted_by_link_score(&set));
    };
    inner.check_stop()?;
    let id = store::next_index_id(&inner.paths);
    let dir = inner.paths.index(id);
    // Hidden next to the index until both are done (leftovers of hidden
    // names are removed at start).
    let buckets_dir = inner
        .paths
        .indexes
        .join(format!(".{}-buckets", store::index_name(id)));
    let started = Instant::now();
    let built = crate::outline::build_index(
        &inner.paths.records,
        outlines,
        &dir,
        network::wants_buckets(inner).then_some(buckets_dir.as_path()),
        inner.config.news_feeds,
        &mut |step| {
            match step {
                crate::outline::Step::Started { docs, sites } => {
                    inner.set_step(
                        Step::Indexing,
                        format!(
                            "Building the search index of {} sites",
                            group_thousands(docs as u64)
                        ),
                    );
                    inner.set_progress(0, sites, "sites");
                }
                crate::outline::Step::Read { done, sites } => {
                    inner.set_progress(done, sites, "sites");
                }
                crate::outline::Step::Writing => {
                    inner.set_step(Step::Indexing, "Writing the search index");
                }
            }
            inner.check_stop()
        },
    )
    .with_context(|| format!("building the index in {}", dir.display()));
    let built = match built {
        Ok(built) => built,
        Err(err) => {
            // Each try has a new id: the half-written buckets would
            // otherwise pile up, against the storage limit, until restart.
            let _ = std::fs::remove_dir_all(&buckets_dir);
            return Err(err);
        }
    };
    if inner.config.news_feeds > 0 {
        inner.news.watch(built.feeds);
    }
    if built.buckets {
        let into = dir.join(network::BUCKETS_DIR);
        if let Err(err) = std::fs::rename(&buckets_dir, &into) {
            warn!("cannot move the buckets to {}: {err}", into.display());
            let _ = std::fs::remove_dir_all(&buckets_dir);
        }
    }
    let index = match ServingIndex::open(id, &dir, inner.rank) {
        Ok(index) => index,
        Err(err) => {
            let _ = store::remove_index(&dir);
            return Err(err.context(format!("opening the new index in {}", dir.display())));
        }
    };
    inner.journal.info(format!(
        "Search index rebuilt: {} sites in {}",
        group_thousands(built.docs as u64),
        duration_words(started.elapsed().as_secs().max(1))
    ));
    info!(
        "built the index in {} ({} sites, read a record at a time) in {:.1} s",
        dir.display(),
        built.docs,
        started.elapsed().as_secs_f64()
    );
    Ok(index)
}

/// Has the node watch the feeds of its best-ranked sites, the first of
/// `records` (see [`NodeConfig::news_feeds`](super::NodeConfig)): sites
/// that answered their last crawl and redirect nowhere.
fn watch_feeds<R: Borrow<SiteRecord>>(inner: &Inner, records: &[R]) {
    let wanted = inner.config.news_feeds;
    if wanted == 0 {
        return;
    }
    let sites = records
        .iter()
        .map(Borrow::borrow)
        .filter(|r| crate::outline::wants_feed(r))
        .take(wanted)
        .map(crate::outline::feed_of)
        .collect();
    inner.news.watch(sites);
}

/// Swaps in a freshly built index and notes that the records file holds no
/// changes it lacks. A build that `ends_round`, with no homepages left to
/// crawl, ends the round: the next refresh is due a refresh interval later.
/// `None` when crawling only ([`NodeConfig::crawl_only`]): no index, and
/// the records stay marked newer than the last one, for when the node
/// searches again.
async fn put_in_service(
    inner: &Arc<Inner>,
    built: Option<ServingIndex>,
    ends_round: bool,
) -> Result<()> {
    let indexed = built.is_some();
    if let Some(built) = built {
        inner.install(built);
    }
    let now = now_unix();
    inner.last_build.store(now, Ordering::SeqCst);
    inner.update_saved(|saved| {
        saved.index_stale = !indexed;
        saved.network_pending = 0;
        if ends_round && saved.crawl_left == 0 {
            saved.last_refresh = Some(now);
        }
    })?;
    if ends_round && inner.saved().crawl_left == 0 {
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
    // On a thread of its own, at a lower priority than searches: a thread
    // of the blocking pool keeps its priority, and searches run there too.
    let runtime = Handle::current();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("plumb-background".to_string())
        .spawn(move || {
            let _runtime = runtime.enter();
            crate::lower_thread_priority();
            let _ = sender.send(work(&inner));
        })
        .context("cannot start the background work")?;
    let done = receiver.await.context("background work crashed");
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
        // they rebuild the index is up to step(). Not while backing off
        // after an error: folding them in may be what failed.
        if !matches!(deadline, Deadline::After(_))
            && inner.inbox_records.load(Ordering::SeqCst) >= REBUILD_AFTER_RECORDS
        {
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
    fn slow_builds_wait_longer_for_the_next() {
        let gap = NETWORK_REBUILD_GAP.as_secs();
        assert_eq!(network_rebuild_gap(0), gap, "none built yet");
        assert_eq!(network_rebuild_gap(120), gap, "a quick build");
        // 2.5 million sites on a busy 4-core server: 25 minutes.
        assert_eq!(network_rebuild_gap(1_500), 4_500);
    }

    /// What a round keeps of `record`.
    fn round(record: SiteRecord) -> RoundSite {
        RoundSite::of(&record, &crate::about::Topics::default(), &Keep::default())
    }

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
        assert!(due_for_icon(&round(crawled("old.com", old)), &noted));
        assert!(!due_for_icon(&round(crawled("noted.com", old)), &noted));
        // Crawled since, by another node: waits for its regular recrawl.
        assert!(!due_for_icon(
            &round(crawled("network.com", ICONS_KEPT_SINCE + 60)),
            &noted
        ));
        assert!(!due_for_icon(&round(SiteRecord::new("never.com")), &noted));
    }

    #[test]
    fn sites_crawled_before_key_pages_are_due_for_them_once() {
        let crawled = |at: u64, pages: bool| {
            let mut record = SiteRecord::new("facebook.com");
            record.crawled_at = Some(at);
            if pages {
                record.key_pages = vec![plumb_core::KeyPage {
                    label: "Log in".into(),
                    url: "https://www.facebook.com/".into(),
                }];
            }
            round(record)
        };
        let old = KEY_PAGES_KEPT_SINCE - 1;
        assert!(due_for_key_pages(&crawled(old, false)));
        assert!(!due_for_key_pages(&crawled(old, true)));
        assert!(!due_for_key_pages(&crawled(
            KEY_PAGES_KEPT_SINCE + 60,
            false
        )));
        assert!(!due_for_key_pages(&round(SiteRecord::new("never.com"))));
    }

    #[test]
    fn sites_are_read_anew_only_once_the_crawl_version_goes_up() {
        let mut record = SiteRecord::new("a.com");
        record.crawled_at = Some(10);
        record.crawl_version = plumb_crawl::CRAWL_VERSION;
        assert!(!due_for_rereading(&round(record.clone())));
        if let Some(older) = plumb_crawl::CRAWL_VERSION.checked_sub(1) {
            record.crawl_version = older;
            assert!(due_for_rereading(&round(record.clone())));
            record.crawl_attempted_at = Some(20);
            record.crawl_failures = 1;
            assert!(
                !due_for_rereading(&round(record)),
                "not reached on its last try"
            );
        }
    }

    #[test]
    fn key_pages_are_caught_up_for_the_best_linked_sites_due() {
        let site = |domain: &str, rank: u32, at: u64| {
            let mut record = SiteRecord::new(domain);
            record.signals.tranco_rank = Some(rank);
            record.crawled_at = Some(at);
            record
        };
        let old = KEY_PAGES_KEPT_SINCE - 1;
        let set = RoundSites::of([
            site("paypal.com", 20, old),
            site("facebook.com", 1, old),
            site("recent.com", 2, KEY_PAGES_KEPT_SINCE + 60),
        ]);
        let domains: Vec<&str> = key_page_catch_up(&set).iter().map(|r| r.domain()).collect();
        assert_eq!(domains, ["facebook.com", "paypal.com"]);
    }

    #[test]
    fn well_known_sites_never_fetched_are_caught_up_once_due() {
        const NOW: u64 = 100 * SECONDS_PER_DAY;
        const WINDOW: u64 = 30 * SECONDS_PER_DAY;
        let site = |domain: &str, rank: u32| {
            let mut record = SiteRecord::new(domain);
            record.signals.tranco_rank = Some(rank);
            record
        };
        let mut crawled = site("crawled.com", 1);
        crawled.crawled_at = Some(NOW - SECONDS_PER_DAY);
        let mut failed_lately = site("failed-lately.com", 2);
        failed_lately.crawl_attempted_at = Some(NOW - 60);
        failed_lately.crawl_failures = 1;
        let mut failed_long_ago = site("failed-long-ago.com", 3);
        failed_long_ago.crawl_attempted_at = Some(NOW - WINDOW);
        failed_long_ago.crawl_failures = 1;
        let mut gone = site("gone.com", 4);
        gone.gone_at = Some(NOW - SECONDS_PER_DAY);
        let mut official = SiteRecord::new("official.com");
        official.signals.official_site = true;
        official.signals.tranco_rank = Some(500_000);
        let set = RoundSites::of([
            site("obscure.com", 5_000_000),
            site("instacart.com", 2_339),
            crawled,
            failed_lately,
            failed_long_ago,
            gone,
            official,
            site("google.com", 1),
        ]);
        let domains: Vec<&str> = first_fetch_catch_up(&set, NOW, WINDOW)
            .iter()
            .map(|r| r.domain())
            .collect();
        assert_eq!(
            domains,
            [
                "google.com",
                "failed-long-ago.com",
                "instacart.com",
                "official.com"
            ]
        );
    }

    #[test]
    fn icons_are_caught_up_for_the_best_linked_crawled_sites_not_noted() {
        let site = |domain: &str, rank: u32, crawled: bool| {
            let mut record = SiteRecord::new(domain);
            record.signals.tranco_rank = Some(rank);
            if crawled {
                record.crawled_at = Some(ICONS_KEPT_SINCE + 60);
                record.url = Some(format!("https://www.{domain}/"));
            }
            record
        };
        let set = RoundSites::of([
            site("third.com", 30, true),
            site("first.com", 1, true),
            site("noted.com", 2, true),
            site("never.com", 3, false),
            site("second.com", 20, true),
        ]);
        let noted: HashSet<String> = ["noted.com".to_string()].into();
        let targets = icon_catch_up_targets(&set, &noted);
        let domains: Vec<&str> = targets.iter().map(|t| t.domain.as_str()).collect();
        assert_eq!(domains, ["first.com", "second.com", "third.com"]);
        assert_eq!(targets[0].url, "https://www.first.com/");
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
