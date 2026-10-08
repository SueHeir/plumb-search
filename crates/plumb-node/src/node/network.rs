//! A node in the Plumb network ([`NodeConfig::network`]): what it adds to
//! a long-running node.
//!
//! * Crawling follows the network's daily assignment: a refresh picks its
//!   homepages among the sites this node is assigned today, and each batch
//!   of results is signed and published as it is saved.
//! * Records other nodes publish are appended to `DIR/net/inbox.jsonl` as
//!   they arrive, and folded into the records file between two pieces of
//!   work ([`absorb_inbox`]). Rebuilding the index for every batch would
//!   keep a node busy, so the index is rebuilt once
//!   [`REBUILD_AFTER_RECORDS`] records have come in, at most once every
//!   [`NETWORK_REBUILD_GAP`] (longer after a slow build), or at the next refresh.
//! * Other nodes search by bucket (see `plumb_net::bucket`), never sending
//!   their query. Each index build also writes the index's buckets into
//!   `indexes/NNNNNN/buckets/` ([`build_buckets`]), and bucket requests are
//!   answered from the index being served. An index built before the node
//!   joined has none, and serves no buckets until the next build.
//! * Popularity (see `plumb_net::popularity`): every node in the network
//!   ranks with what the reports it holds say people pick for a query
//!   ([`apply_popularity`]). A node started with
//!   [`NodeConfig::share_popularity`](super::NodeConfig::share_popularity)
//!   also notes which result its web page's users open ([`record_pick`])
//!   and sends a few reports of those picks a day, at random times
//!   ([`report_picks`]).
//! * A node with free space asks the nodes it trusts for their crawled
//!   sites, best-ranked first, until its storage limit leaves no room (see
//!   [`super::fill`]).
//! * A node started with
//!   [`NodeConfig::publish_records`](super::NodeConfig::publish_records)
//!   also shares the homepages crawled into another records file, such as
//!   one a `plumb crawl` is filling ([`publish_records`]): every
//!   [`PUBLISH_RECORDS_EVERY`] it publishes those crawled since last time
//!   (within the last [`PUBLISH_MAX_AGE_DAYS`] days, as other nodes take no
//!   older batches) and folds them into its own records too.
//!
//! ```text
//! DIR/net/
//!   node.key           the node's identity; keep it to keep the same id
//!   batches/           signed batches held (see plumb_net::store)
//!   inbox.jsonl        records from other nodes not yet folded in
//!   inbox.absorbing    the inbox being folded in
//!   reports/           popularity reports of this week and last week
//!   popularity.json    what they say, as last counted
//!   picks.json         results opened here this week (sharing nodes only)
//!   published.json     how far publish_records got in its file
//!   fill.json          how far filling free space got
//! ```

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use std::time::Duration;

use anyhow::{Context, Result};
use plumb_core::{now_unix, SiteRecord};
use plumb_index::Hit;
use plumb_net::popularity::report_epoch;
use plumb_net::{BucketSource, BucketTable, NetHandle, PickLog, PopularityTable, Report};
use tracing::{debug, info, warn};

use super::{Inner, MB};
use crate::icons::{self, IconStore};
use crate::records::{Change, RecordStore};

/// Records from other nodes that make a node rebuild its index before the
/// next refresh.
pub(crate) const REBUILD_AFTER_RECORDS: u64 = 2_000;

/// The least time between two index builds that records from other nodes
/// ask for. A build of a million sites takes a few minutes and over 2 GB of
/// memory, and a busy network can send 2,000 records every few minutes.
pub(crate) const NETWORK_REBUILD_GAP: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Records from the inbox saved to the records journal at a time.
const ABSORB_CHUNK: usize = 1_000;
/// The share of a storage limit, in percent, other crawlers' batches may
/// take. About two days of the network's crawls on an 8 GB limit.
const BATCHES_PERCENT: u64 = 20;

/// Where an index keeps its buckets, inside its directory.
pub(super) const BUCKETS_DIR: &str = "buckets";

/// Results a search ranks before popularity re-orders them, so that a
/// popular site just below the page can still move onto it.
pub(super) const POPULARITY_CANDIDATES: usize = 20;

/// The shortest and longest wait between two tries at sending a report.
/// Random, so the time a report goes out says little about when the pick
/// was made.
const REPORT_GAP_MINUTES: std::ops::Range<u64> = 20..100;

/// How long a node handed a report has to take it.
const REPORT_WAIT: Duration = Duration::from_secs(20);

const PICKS_FILE: &str = "picks.json";

/// How often [`publish_records`] looks for newly crawled homepages.
pub(super) const PUBLISH_RECORDS_EVERY: Duration = Duration::from_secs(30 * 60);

/// Crawls older than this many days are not published: other nodes only
/// take batches of the last week.
pub(super) const PUBLISH_MAX_AGE_DAYS: u64 = 6;

/// Homepages per batch [`publish_records`] publishes.
const PUBLISH_CHUNK: usize = 1_000;

/// Pause between two of those batches, so peers can keep up.
const PUBLISH_PAUSE: Duration = Duration::from_millis(500);

/// Where [`publish_records`] notes how far it got.
const PUBLISHED_FILE: &str = "published.json";

/// How far [`publish_records`] got in a file.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Published {
    file: PathBuf,
    /// The latest crawl time published from it.
    up_to: u64,
}

/// Answers other nodes' bucket requests from the index this node serves.
struct ServedIndex(Arc<Inner>);

impl BucketSource for ServedIndex {
    fn bucket(&self, bucket: u32) -> Option<Vec<String>> {
        let index = self.0.current()?;
        let table = index.buckets.as_ref()?;
        match table.get(bucket) {
            Ok(records) => Some(records),
            Err(err) => {
                warn!("cannot read bucket {bucket}: {err:#}");
                None
            }
        }
    }

    fn ranked(&self, from: usize, count: usize) -> Option<(Vec<String>, usize)> {
        let index = self.0.current()?;
        let table = index.buckets.as_ref()?;
        match table.ranked(from, count) {
            Ok(records) => Some((records, table.len())),
            Err(err) => {
                warn!("cannot read the sites from {from}: {err:#}");
                None
            }
        }
    }
    fn icon(&self, domain: &str) -> Option<String> {
        icons::to_shared(&IconStore::new(&self.0.paths.icons).get(domain)?)
    }

    fn page_set_file(&self, set: &str) -> Option<PathBuf> {
        if let Some(file) = super::shared_vectors::servable(&self.0.paths.data, set) {
            return Some(file);
        }
        if set == super::adult::SHARED_NAME {
            return super::adult::shared_file(&self.0.paths.data);
        }
        crate::pages::SetInfo::find(set)?.servable_file(&self.0.paths.data)
    }

    fn profile(
        &self,
        peer: plumb_net::PeerId,
        request: plumb_net::proto::ProfileRequest,
    ) -> plumb_net::proto::ProfileResponse {
        match profiles_dir(&self.0) {
            Some(dir) => crate::sync::answer(&dir, peer, request),
            None => plumb_net::proto::ProfileResponse::Refused(
                "this node keeps no search profiles".into(),
            ),
        }
    }
}

/// Where the node keeps its searchers' profiles, when it keeps them (see
/// [`crate::history`]).
fn profiles_dir(inner: &Inner) -> Option<PathBuf> {
    inner
        .config
        .search_history
        .then(|| inner.paths.data.join("history"))
}

/// Whether this node's indexes need buckets: it answers other nodes'
/// searches, or serves private search to browsers.
pub(super) fn wants_buckets(inner: &Inner) -> bool {
    inner.config.private_search
        || inner
            .config
            .network
            .as_ref()
            .is_some_and(|n| n.answer_searches)
}

/// Writes the buckets of a new index into its directory, for a node that
/// [`wants_buckets`]. A failure is logged, not raised: the index still
/// works, it just serves no buckets.
pub(super) fn build_buckets<R: std::borrow::Borrow<SiteRecord>>(
    inner: &Inner,
    index_dir: &Path,
    records: &[R],
) {
    if !wants_buckets(inner) {
        return;
    }
    let dir = index_dir.join(BUCKETS_DIR);
    if let Err(err) = BucketTable::build(&dir, records) {
        warn!(
            "cannot write the buckets of {}: {err:#}",
            index_dir.display()
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

/// The name browsers know an index's buckets by (see `GET /api/buckets`):
/// the index id and the start of a hash of its bucket index. `None` when
/// the index has no buckets.
pub(super) fn bucket_table_name(id: u64, index_dir: &Path) -> Option<String> {
    let idx = fs::read(index_dir.join(BUCKETS_DIR).join("buckets.idx")).ok()?;
    let hash = plumb_net::hash::Hash::of(&[&idx]).to_hex();
    Some(format!("{id}-{}", &hash[..16]))
}

/// Joins the network, if the node is configured to, and starts keeping
/// what other nodes send in the inbox.
pub(super) async fn start(inner: &Arc<Inner>) -> Result<()> {
    let Some(mut config) = inner.config.network.clone() else {
        return Ok(());
    };
    config.dir = inner.paths.net.clone();
    // A node with a storage limit keeps the batches it holds for as long as
    // crawls are checked against each other, not the default five weeks:
    // other nodes take none older than a week, and the rest takes room
    // (see super::trim). Other crawlers' batches come at a gigabyte a day
    // or more, so they also get a share of the limit, oldest out first.
    let limit = inner.settings().storage_limit_mb.saturating_mul(MB);
    if limit > 0 {
        if config.keep_batches_days == plumb_net::store::RETAIN_EPOCHS {
            config.keep_batches_days = plumb_net::agree::WINDOW_EPOCHS;
        }
        if config.keep_batches_bytes.is_none() {
            config.keep_batches_bytes = Some(limit / 100 * BATCHES_PERCENT);
        }
    }
    let (handle, mut records) = plumb_net::start(config, Arc::new(ServedIndex(inner.clone())))
        .await
        .context("joining the Plumb network")?;
    info!("joined the Plumb network as {}", handle.peer_id());
    inner.journal.info(format!(
        "Joined the Plumb network as node {}",
        handle.peer_id()
    ));
    let handle = Arc::new(handle);
    let _ = inner.net.set(handle.clone());
    if let Some(dir) = profiles_dir(inner) {
        tokio::spawn(crate::sync::run(dir, handle));
    }
    if inner.config.share_popularity {
        let picks = PickLog::open(&inner.paths.net.join(PICKS_FILE));
        *inner
            .picks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(picks);
        info!("sharing popularity: results opened here are reported anonymously");
        tokio::spawn(report_picks(inner.clone()));
    }
    if let Some(path) = inner.config.publish_records.clone() {
        tokio::spawn(publish_records(inner.clone(), path));
    }
    if inner.config.network.as_ref().is_some_and(|n| n.fill) {
        tokio::spawn(super::fill::fill_space(inner.clone()));
    }
    let receiver = inner.clone();
    tokio::spawn(async move {
        while let Some(mut batch) = records.recv().await {
            // Crawling only, other nodes' crawls are not kept: they would
            // only grow the records this node crawls from.
            if receiver.config.crawl_only {
                continue;
            }
            // Trusted nodes' feed checks go to the headline store, not the
            // records (see plumb_net::start).
            if batch.iter().any(|record| !record.news.is_empty()) {
                let (news, rest) = batch.into_iter().partition(|r| !r.news.is_empty());
                receiver.news.put_shared(news, now_unix());
                batch = rest;
                if batch.is_empty() {
                    continue;
                }
            }
            if !receiver.config.take_new_sites {
                held_only(&receiver, &mut batch);
                if batch.is_empty() {
                    continue;
                }
            }
            let inner = receiver.clone();
            let saved = tokio::task::spawn_blocking(move || {
                let n = batch.len() as u64;
                append_inbox(&inner, &batch).map(|()| {
                    let total = inner.inbox_records.fetch_add(n, Ordering::SeqCst) + n;
                    if total >= REBUILD_AFTER_RECORDS {
                        inner.wake.notify_one();
                    }
                })
            })
            .await;
            match saved {
                Ok(Ok(())) => {}
                Ok(Err(err)) => warn!("cannot keep records from the network: {err:#}"),
                Err(err) => warn!("keeping records from the network failed: {err}"),
            }
        }
    });
    Ok(())
}

/// Stops the network side, if running.
pub(super) async fn stop(inner: &Inner) {
    if let Some(net) = inner.net.get() {
        net.shutdown().await;
    }
}

pub(super) fn handle(inner: &Inner) -> Option<&Arc<NetHandle>> {
    inner.net.get()
}

/// Whether the node notes and reports the results opened on its page.
pub(super) fn shares_popularity(inner: &Inner) -> bool {
    inner
        .picks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
}

/// Notes that `domain` was opened from the results for `query`, when the
/// node shares popularity. Queries that are never reported (see
/// `plumb_net::popularity::pick_query`) are not kept.
pub(super) fn record_pick(inner: &Inner, query: &str, domain: &str) {
    let mut picks = inner
        .picks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(picks) = picks.as_mut() {
        if let Err(err) = picks.record(query, domain, now_unix()) {
            warn!("cannot note a pick: {err:#}");
        }
    }
}

/// Adds each hit's popularity bonus to its score and sorts again. The
/// order of hits with equal scores is kept.
pub(super) fn apply_popularity(table: &PopularityTable, query: &str, hits: &mut [Hit]) {
    if table.is_empty() {
        return;
    }
    let mut changed = false;
    for hit in hits.iter_mut() {
        let bonus = table.bonus(query, &hit.domain);
        if bonus > 0.0 {
            hit.score += bonus;
            changed = true;
        }
    }
    if changed {
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    }
}

/// Sends the reports of this week's picks, one at a time at random
/// intervals, as many a day as `plumb_net::popularity::REPORTS_PER_DAY`
/// allows, until the node stops.
async fn report_picks(inner: Arc<Inner>) {
    loop {
        let minutes = random_in(REPORT_GAP_MINUTES);
        tokio::select! {
            () = inner.stopped() => return,
            () = tokio::time::sleep(Duration::from_secs(minutes * 60)) => {}
        }
        if let Err(err) = report_one(&inner).await {
            debug!("no popularity report sent this time: {err:#}");
        }
    }
}

/// A number in `range`, unpredictable enough to spread reports out.
fn random_in(range: std::ops::Range<u64>) -> u64 {
    use std::hash::BuildHasher;
    let random = std::collections::hash_map::RandomState::new().hash_one(now_unix());
    range.start + random % (range.end - range.start)
}

/// Sends the report of the pick due next, if any.
pub(super) async fn report_one(inner: &Inner) -> Result<bool> {
    let Some(net) = handle(inner) else {
        return Ok(false);
    };
    let now = now_unix();
    let due = inner
        .picks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
        .and_then(|picks| picks.next_due(now));
    let Some((query, domain)) = due else {
        return Ok(false);
    };
    let report = tokio::task::spawn_blocking({
        let (query, domain) = (query.clone(), domain.clone());
        move || Report::new(report_epoch(now), &query, &domain)
    })
    .await
    .context("the report task failed")??;
    net.send_report(&report, REPORT_WAIT).await?;
    if let Some(picks) = inner
        .picks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        picks.sent(&query, &domain, now)?;
    }
    Ok(true)
}

/// Publishes the homepages crawled into the records file at `path`, as
/// the module docs describe, until the node stops.
async fn publish_records(inner: Arc<Inner>, path: PathBuf) {
    loop {
        match publish_new_records(&inner, &path).await {
            Ok(0) => debug!("no new homepages to publish in {}", path.display()),
            Ok(n) => info!("published {n} homepages crawled into {}", path.display()),
            Err(err) => warn!(
                "cannot publish the homepages in {}: {err:#}",
                path.display()
            ),
        }
        tokio::select! {
            () = inner.stopped() => return,
            () = tokio::time::sleep(PUBLISH_RECORDS_EVERY) => {}
        }
    }
}

/// One pass of [`publish_records`]: returns how many homepages it published.
pub(super) async fn publish_new_records(inner: &Arc<Inner>, path: &Path) -> Result<usize> {
    let Some(net) = handle(inner).cloned() else {
        return Ok(0);
    };
    let marker = inner.paths.net.join(PUBLISHED_FILE);
    let since = {
        let path = path.to_path_buf();
        let marker = marker.clone();
        let inner = Arc::clone(inner);
        tokio::task::spawn_blocking(move || {
            let published: Published = fs::read(&marker)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
            let oldest = now_unix().saturating_sub(PUBLISH_MAX_AGE_DAYS * 24 * 60 * 60);
            let after = if published.file == path {
                published.up_to
            } else {
                0
            };
            // Read a record at a time, the journal folded in first.
            let _records = inner.hold_records();
            crate::outline::fold_journal(&path)?;
            let mut crawled: Vec<SiteRecord> = Vec::new();
            crate::outline::for_each_record(&path, |record| {
                if record
                    .crawled_at
                    .is_some_and(|at| at > after && at >= oldest)
                {
                    crawled.push(crawl_facts(&record));
                }
            })?;
            crawled.sort_by_key(|r| r.crawled_at);
            anyhow::Ok(crawled)
        })
        .await
        .context("the publishing task failed")??
    };
    let mut published = 0;
    for chunk in since.chunks(PUBLISH_CHUNK) {
        if inner.stopping() {
            break;
        }
        net.publish(chunk.to_vec()).await?;
        let up_to = chunk.last().and_then(|r| r.crawled_at).unwrap_or(0);
        let n = chunk.len();
        let (inner2, chunk, marker, file) = (
            inner.clone(),
            chunk.to_vec(),
            marker.clone(),
            path.to_path_buf(),
        );
        tokio::task::spawn_blocking(move || {
            append_inbox(&inner2, &chunk)?;
            let n = chunk.len() as u64;
            let total = inner2.inbox_records.fetch_add(n, Ordering::SeqCst) + n;
            if total >= REBUILD_AFTER_RECORDS {
                inner2.wake.notify_one();
            }
            let json = serde_json::to_vec(&Published { file, up_to })?;
            super::store::write_atomically(&marker, &json)
        })
        .await
        .context("the publishing task failed")??;
        published += n;
        tokio::time::sleep(PUBLISH_PAUSE).await;
    }
    Ok(published)
}

/// What a crawl saw of a site's homepage, without the ranks, seed data and
/// bookkeeping of the file it came from.
fn crawl_facts(record: &SiteRecord) -> SiteRecord {
    let mut facts = SiteRecord::new(record.domain.clone());
    facts.url = record.url.clone();
    facts.title = record.title.clone();
    facts.description = record.description.clone();
    facts.aliases = record.aliases.clone();
    facts.headings = record.headings.clone();
    facts.body_text = record.body_text.clone();
    facts.terms = record.terms.clone();
    facts.key_pages = record.key_pages.clone();
    facts.crawled_at = record.crawled_at;
    facts
}

pub(super) fn append_inbox(inner: &Inner, records: &[SiteRecord]) -> Result<()> {
    let path = &inner.paths.inbox;
    let mut lines = Vec::with_capacity(records.len() * 200);
    for record in records {
        serde_json::to_writer(&mut lines, record).context("encoding a record")?;
        lines.push(b'\n');
    }
    let _guard = inner
        .inbox_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A last line a crash cut short gets a line break first, so the next
    // record is not lost with it.
    let mut file = crate::records::open_journal(path)?;
    file.write_all(&lines)
        .and_then(|()| file.sync_data())
        .with_context(|| format!("writing {}", path.display()))
}

/// Folds the records other nodes sent into the records file (through its
/// journal, as a crawl does) and returns how many there were. Runs between
/// two pieces of work, so it never races a crawl that holds the records in
/// memory.
///
/// The inbox is first renamed, so records arriving meanwhile start a new
/// one, and the renamed file is deleted only once its records are saved. A
/// crash in between folds them in again at the next start, which merging
/// makes harmless.
pub(super) fn absorb_inbox(inner: &Inner) -> Result<u64> {
    let paths = &inner.paths;
    if !paths.absorbing.exists() {
        let _guard = inner
            .inbox_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match fs::rename(&paths.inbox, &paths.absorbing) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(err) => {
                return Err(err).with_context(|| format!("moving {}", paths.inbox.display()))
            }
        }
        inner.inbox_records.store(0, Ordering::SeqCst);
    }
    let file = File::open(&paths.absorbing)
        .with_context(|| format!("opening {}", paths.absorbing.display()))?;
    let icons = IconStore::new(&paths.icons);
    let mut store = RecordStore::open(&paths.records);
    // A full node only refreshes the sites it holds, but for those about
    // its topics and official websites (see super::trim).
    let topics = (!super::trim::takes_new_sites(inner)).then(|| inner.keep_topics());
    // Saved a part at a time, so an inbox of many thousand records is never
    // all in memory.
    let mut changes = Vec::with_capacity(ABSORB_CHUNK);
    let mut n = 0;
    read_inbox(BufReader::new(file), |mut record| {
        keep_shared_icon(&icons, &mut record);
        changes.push(match &topics {
            Some(topics) if !super::trim::keeps_new_site(&record, topics) => {
                Change::RefreshShared { record }
            }
            _ => Change::MergeShared { record },
        });
        if changes.len() >= ABSORB_CHUNK {
            store.save(&changes)?;
            n += changes.len() as u64;
            changes.clear();
        }
        Ok(())
    })
    .with_context(|| format!("reading {}", paths.absorbing.display()))?;
    store.save(&changes)?;
    n += changes.len() as u64;
    drop(changes);
    if store.wants_compaction() {
        // Folded a record at a time; a file only a whole set can fold
        // (a site on two lines) keeps its journal for the next build.
        let _records = inner.hold_records();
        store.fold()?;
    }
    fs::remove_file(&paths.absorbing)
        .with_context(|| format!("deleting {}", paths.absorbing.display()))?;
    if n > 0 {
        info!("folded {n} records from the network into the records file");
    }
    Ok(n)
}

/// Calls `each` with every record in the inbox `reader` reads, one a
/// line. A line that is not one (cut short by a crash, or not even UTF-8)
/// is skipped, so it never holds up the records after it.
fn read_inbox(
    mut reader: impl BufRead,
    mut each: impl FnMut(SiteRecord) -> Result<()>,
) -> Result<()> {
    let (mut damaged, mut line) = (0usize, Vec::new());
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<SiteRecord>(&line) {
            Ok(record) => each(record)?,
            Err(_) => damaged += 1,
        }
    }
    if damaged > 0 {
        warn!("the network inbox had {damaged} damaged lines, skipped");
    }
    Ok(())
}

/// Leaves out of `batch`, crawls other nodes published, the sites the
/// index being served does not hold, for a node holding back new sites
/// ([`super::NodeConfig::take_new_sites`] off): they only refresh the sites
/// it has. Filling free space still adds sites (see super::fill).
fn held_only(inner: &Inner, batch: &mut Vec<SiteRecord>) {
    let index = inner.current();
    let backend = index.as_ref().and_then(|index| index.backend.as_ref());
    batch.retain(|record| backend.is_some_and(|backend| backend.has_domain(&record.domain)));
}

/// Most sites [`Inner::kept_found`] remembers before it starts over.
const MAX_KEPT_FOUND: usize = 10_000;

/// Keeps the signed crawls a network search found of sites the index being
/// served holds: they go to the inbox like crawls other nodes publish, so
/// a site this node has without text gets the network's text in its next
/// index, and is then embedded for search by meaning. Only what
/// [`plumb_net::FoundSite::keeps`] gives is passed here, so the trust rules
/// of published batches apply (a crawl is kept only from a trusted crawler
/// or once confirmed, text only from trusted crawlers). Sites this
/// node does not hold are left out: searching never fills its storage.
pub(super) fn keep_found(inner: &Inner, records: Vec<SiteRecord>) {
    let Some(index) = inner.current() else {
        return;
    };
    let Some(backend) = index.backend.as_ref() else {
        return;
    };
    let mut kept = inner
        .kept_found
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let new: Vec<SiteRecord> = records
        .into_iter()
        .filter(|record| {
            let Some(at) = record.crawled_at else {
                return false;
            };
            kept.get(&record.domain).is_none_or(|&had| had < at)
                && backend.has_domain(&record.domain)
        })
        .collect();
    if new.is_empty() {
        return;
    }
    if kept.len() + new.len() > MAX_KEPT_FOUND {
        kept.clear();
    }
    for record in &new {
        kept.insert(record.domain.clone(), record.crawled_at.unwrap_or(0));
    }
    drop(kept);
    let n = new.len() as u64;
    match append_inbox(inner, &new) {
        Ok(()) => {
            debug!("kept {n} signed crawls a network search found");
            let total = inner.inbox_records.fetch_add(n, Ordering::SeqCst) + n;
            if total >= REBUILD_AFTER_RECORDS {
                inner.wake.notify_one();
            }
        }
        Err(err) => warn!("cannot keep records a network search found: {err:#}"),
    }
}

/// Moves the icon a shared crawl carries into the icon store: icons are
/// never kept in the records file.
fn keep_shared_icon(icons: &IconStore, record: &mut SiteRecord) {
    let Some(icon) = record.icon.take().as_deref().and_then(icons::from_shared) else {
        return;
    };
    if let Err(err) = icons.put(&record.domain, Some(&icon)) {
        warn!("cannot save the icon of {}: {err}", record.domain);
    }
}

#[cfg(test)]
mod tests {
    use plumb_net::popularity::{Popular, MAX_POPULARITY_BONUS};

    use super::*;

    #[test]
    fn a_shared_icon_goes_to_the_icon_store_not_the_record() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let icons = IconStore::new(dir.path());
        // A 1x1 PNG is too small to be an icon: dropped, but still not kept.
        let mut tiny = SiteRecord::new("tiny.com");
        tiny.icon = Some("iVBORw0KGgo=".into());
        keep_shared_icon(&icons, &mut tiny);
        assert!(tiny.icon.is_none());
        assert_eq!(icons.get("tiny.com"), None);

        let png = plumb_crawl::normalize_icon(&crate::icons::tests::bmp()).unwrap();
        let mut record = SiteRecord::new("a.com");
        record.icon = Some(base64::engine::general_purpose::STANDARD.encode(&png));
        keep_shared_icon(&icons, &mut record);
        assert!(record.icon.is_none());
        assert!(icons.get("a.com").unwrap().starts_with(b"\x89PNG"));
    }

    #[test]
    fn a_damaged_inbox_line_is_skipped_not_fatal() {
        let line = |domain: &str| {
            let mut line = serde_json::to_vec(&SiteRecord::new(domain)).unwrap();
            line.push(b'\n');
            line
        };
        let mut inbox = line("a.com");
        // Not UTF-8, then a record a crash cut short, given a line break
        // by the next append.
        inbox.extend_from_slice(b"\xff\xfe{\"domain\"\n");
        inbox.extend_from_slice(b"{\"domain\":\"cut.co\n");
        inbox.extend(line("b.com"));
        inbox.extend_from_slice(b"\n");
        let mut domains = Vec::new();
        read_inbox(inbox.as_slice(), |record| {
            domains.push(record.domain);
            Ok(())
        })
        .unwrap();
        assert_eq!(domains, ["a.com", "b.com"]);
    }

    #[test]
    fn appending_to_an_inbox_cut_short_starts_a_new_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.jsonl");
        std::fs::write(&path, b"{\"domain\":\"cut.co").unwrap();
        crate::records::open_journal(&path)
            .unwrap()
            .write_all(b"{}\n")
            .unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{\"domain\":\"cut.co\n{}\n".to_vec()
        );
    }

    fn hit(domain: &str, score: f32) -> Hit {
        Hit {
            demand: None,
            missing_words: false,
            placing_text_score: None,
            domain: domain.to_string(),
            url: format!("https://{domain}/"),
            title: None,
            description: None,
            score,
            text_score: 0.5,
            link_score: 0.5,
            country: None,
            named: false,
            official: false,
            key_pages: Vec::new(),
        }
    }

    #[test]
    fn popular_picks_move_up_a_close_call_but_not_past_a_clear_winner() {
        let table = PopularityTable::new(
            vec![1],
            [Popular {
                query: "delta".into(),
                domain: "delta.com".into(),
                count: 40,
            }],
        );
        let mut hits = vec![
            hit("deltafaucet.com", 1.30),
            hit("delta.com", 1.20),
            hit("delta.org", 1.19),
        ];
        apply_popularity(&table, "Delta", &mut hits);
        let order: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
        assert_eq!(order, ["delta.com", "deltafaucet.com", "delta.org"]);
        assert!((hits[0].score - (1.20 + MAX_POPULARITY_BONUS)).abs() < 1e-6);

        let mut clear = vec![hit("deltafaucet.com", 2.0), hit("delta.com", 1.0)];
        apply_popularity(&table, "delta", &mut clear);
        assert_eq!(clear[0].domain, "deltafaucet.com");

        // Other queries are left alone.
        let mut other = vec![hit("delta.org", 1.0), hit("delta.com", 0.9)];
        apply_popularity(&table, "delta airlines", &mut other);
        assert_eq!(other[0].domain, "delta.org");
        assert_eq!(other[1].score, 0.9);
    }
}
