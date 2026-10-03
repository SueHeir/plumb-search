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
//! * Other nodes' searches are answered from the index being served.
//!
//! ```text
//! DIR/net/
//!   node.key           the node's identity; keep it to keep the same id
//!   batches/           signed batches held (see plumb_net::store)
//!   inbox.jsonl        records from other nodes not yet folded in
//!   inbox.absorbing    the inbox being folded in
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{Context, Result};
use plumb_core::SiteRecord;
use plumb_net::{LocalHit, LocalSearch, NetHandle};
use tracing::{info, warn};

use super::Inner;
use crate::records::{load_records, Change, RecordStore};
use crate::web::SearchBackend;

/// Records from other nodes that make a node rebuild its index before the
/// next refresh.
pub(crate) const REBUILD_AFTER_RECORDS: u64 = 2_000;

/// Answers other nodes' searches from the index this node serves.
struct ServedIndex(Arc<Inner>);

impl LocalSearch for ServedIndex {
    fn search(&self, query: &str, limit: usize) -> Vec<LocalHit> {
        match SearchBackend::search(&*self.0, query, limit) {
            Ok(hits) => hits
                .into_iter()
                .map(|hit| LocalHit {
                    domain: hit.domain,
                    url: hit.url,
                    title: hit.title,
                    description: hit.description,
                    score: hit.score,
                })
                .collect(),
            // Not ready yet: nothing to offer.
            Err(_) => Vec::new(),
        }
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
    let _ = inner.net.set(Arc::new(handle));
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
    if let Some(net) = inner.net.get() {
        net.shutdown().await;
    }
}

pub(super) fn handle(inner: &Inner) -> Option<&Arc<NetHandle>> {
    inner.net.get()
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
