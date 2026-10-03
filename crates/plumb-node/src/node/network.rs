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
//!   [`REBUILD_AFTER_RECORDS`] records have come in, or at the next refresh.
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
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use std::time::Duration;

use anyhow::{Context, Result};
use plumb_core::{now_unix, SiteRecord};
use plumb_index::Hit;
use plumb_net::popularity::report_epoch;
use plumb_net::{BucketSource, BucketTable, NetHandle, PickLog, PopularityTable, Report};
use tracing::{debug, info, warn};

use super::Inner;
use crate::records::{load_records, Change, RecordStore};

/// Records from other nodes that make a node rebuild its index before the
/// next refresh.
pub(crate) const REBUILD_AFTER_RECORDS: u64 = 2_000;

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
pub(super) fn build_buckets(inner: &Inner, index_dir: &Path, records: &[SiteRecord]) {
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

/// Joins the network now if the node is set up to and its settings say
/// so, then follows the settings: leaves the network when "join the
/// network" is turned off and joins again when it is turned on.
pub(super) async fn follow_settings(inner: &Arc<Inner>) {
    if inner.config.network.is_none() {
        return;
    }
    apply_settings(inner).await;
    let following = inner.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = following.stopped() => return,
                () = following.net_change.notified() => {}
            }
            apply_settings(&following).await;
        }
    });
}

/// Joins or leaves the network, as the settings now say.
async fn apply_settings(inner: &Arc<Inner>) {
    let wanted = inner.settings().join_network;
    let joined = handle(inner).is_some();
    if wanted && !joined {
        if let Err(err) = start(inner).await {
            // The node still searches and crawls on its own.
            warn!("{err:#}");
        }
    } else if !wanted && joined {
        info!("leaving the Plumb network, as the settings say");
        stop(inner).await;
    }
}

/// Joins the network, if the node is configured to, and starts keeping
/// what other nodes send in the inbox.
pub(super) async fn start(inner: &Arc<Inner>) -> Result<()> {
    let Some(mut config) = inner.config.network.clone() else {
        return Ok(());
    };
    config.dir = inner.paths.net.clone();
    let (handle, mut records) = plumb_net::start(config, Arc::new(ServedIndex(inner.clone())))
        .await
        .context("joining the Plumb network")?;
    info!("joined the Plumb network as {}", handle.peer_id());
    *inner.net.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(handle));
    if inner.config.share_popularity {
        let picks = PickLog::open(&inner.paths.net.join(PICKS_FILE));
        *inner
            .picks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(picks);
        info!("sharing popularity: results opened here are reported anonymously");
        tokio::spawn(report_picks(inner.clone()));
    }
    let receiver = inner.clone();
    tokio::spawn(async move {
        while let Some(batch) = records.recv().await {
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
    let net = inner
        .net
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    *inner.picks.lock().unwrap_or_else(PoisonError::into_inner) = None;
    if let Some(net) = net {
        net.shutdown().await;
    }
}

pub(super) fn handle(inner: &Inner) -> Option<Arc<NetHandle>> {
    inner
        .net
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
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

fn append_inbox(inner: &Inner, records: &[SiteRecord]) -> Result<()> {
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
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
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
    let mut changes = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("reading {}", paths.absorbing.display()))?;
        // A crash can cut the last line short.
        if let Ok(record) = serde_json::from_str::<SiteRecord>(&line) {
            changes.push(Change::Merge { record });
        }
    }
    let n = changes.len() as u64;
    let mut store = RecordStore::open(&paths.records);
    store.save(&changes)?;
    if store.wants_compaction() {
        let set = load_records(&paths.records)?;
        store.compact(&set)?;
    }
    fs::remove_file(&paths.absorbing)
        .with_context(|| format!("deleting {}", paths.absorbing.display()))?;
    if n > 0 {
        info!("folded {n} records from the network into the records file");
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use plumb_net::popularity::{Popular, MAX_POPULARITY_BONUS};

    use super::*;

    fn hit(domain: &str, score: f32) -> Hit {
        Hit {
            domain: domain.to_string(),
            url: format!("https://{domain}/"),
            title: None,
            description: None,
            score,
            text_score: 0.5,
            link_score: 0.5,
            country: None,
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
