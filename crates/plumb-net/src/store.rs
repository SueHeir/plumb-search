//! The batches a node holds: its own and the ones it accepted from others,
//! kept on disk so it can hand them on and prove its search answers.
//!
//! ```text
//! DIR/net/batches/<id>.json   one signed batch each
//! ```
//!
//! For every crawled homepage the store remembers the newest batch holding
//! it from each crawler, so a search answer can carry a [`RecordProof`] for
//! each hit, and a second one from another crawler that agrees (see
//! [`crate::agree`]). Batches older than [`RETAIN_EPOCHS`] are deleted, and
//! a node with little room deletes other crawlers' oldest batches sooner
//! ([`BatchStore::prune_to_bytes`]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context, Result};
use libp2p::identity::PublicKey;
use plumb_core::SiteRecord;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::agree::agree;
use crate::assign::{epoch_of, is_assigned};
use crate::batch::{Batch, RecordProof, SignedHeader};
use crate::hash::Hash;
use crate::storage::{allocation_for, file_bytes, LimitedBytes, Reservation, StorageBudget};

// Signed record strings can double in size when escaped in the batch JSON.
const MAX_STORED_BATCH_BYTES: usize = 2 * crate::batch::MAX_BATCH_BYTES + 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct StoreLimits {
    pub own: Vec<u8>,
    pub bytes: Option<u64>,
    pub now: u64,
    pub epochs: u64,
    pub storage: Option<Arc<StorageBudget>>,
}

/// Batches are kept for this many epochs.
pub const RETAIN_EPOCHS: u64 = 35;

/// How much one crawler sent, from the batches a node holds: whether a node
/// is contributing, and how often. In `GET /api/status` as
/// `network.crawlers`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlerView {
    pub peer_id: String,
    /// This node itself.
    #[serde(default)]
    pub me: bool,
    /// On this node's trust list ([`crate::NetConfig::trusted_peers`]).
    #[serde(default)]
    pub trusted: bool,
    /// Homepages in its batches made in the last 24 hours.
    pub homepages_last_day: u64,
    /// Homepages in its batches made in the last 7 days.
    pub homepages_last_week: u64,
    /// Homepages in all its batches held.
    pub homepages_held: u64,
    pub batches_held: u64,
    /// When its newest batch held was made, in Unix seconds.
    pub last_batch_at: u64,
}

/// Where one crawler's newest record of a homepage is. A store holds one
/// for each homepage each crawler sent in the batches held, millions on a
/// node keeping a month of the network's crawls, so it is kept small: 44
/// bytes, with the crawler's key held once in [`BatchStore::crawler_keys`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Holding {
    batch: Hash,
    index: u32,
    /// Unix seconds, as in the batch header.
    created_at: u32,
    /// Where the crawler's key is in [`BatchStore::crawler_keys`].
    crawler: u32,
}

/// Reads batches of a [`BatchStore`] to prove records, keeping the last
/// few read with their leaf hashes: the sites of one bucket often come from
/// the same batches. A batch deleted meanwhile is just not there.
#[derive(Debug)]
pub struct BatchReader {
    dir: PathBuf,
    cache: HashMap<Hash, Option<(Batch, Vec<Hash>)>>,
    budget: Option<Arc<StorageBudget>>,
}

/// Batches a [`BatchReader`] keeps read at once.
const READER_CACHE: usize = 16;

impl BatchReader {
    /// The proof of record `index` of batch `id`, if held.
    pub fn proof(&mut self, id: &Hash, index: usize) -> Result<Option<RecordProof>> {
        if !self.cache.contains_key(id) {
            if self.cache.len() >= READER_CACHE {
                self.cache.clear();
            }
            let _mutation = self.budget.as_ref().map(|budget| budget.mutation());
            let read = match read_batch(&self.dir.join(format!("{id}.json"))) {
                Ok(batch) => {
                    let leaves = batch.leaf_hashes();
                    Some((batch, leaves))
                }
                Err(err) if is_not_found(&err) => None,
                Err(err) => return Err(err),
            };
            self.cache.insert(*id, read);
        }
        Ok(self.cache[id]
            .as_ref()
            .filter(|(batch, _)| index < batch.records.len())
            .map(|(batch, leaves)| batch.proof_with(index, leaves)))
    }

    /// [`BatchStore::proofs`] from the `sources` [`BatchStore::proof_sources`]
    /// gave.
    pub fn proofs(&mut self, sources: &[(Hash, usize)], max: usize) -> Result<Vec<RecordProof>> {
        let mut crawls: Vec<(SiteRecord, RecordProof)> = Vec::new();
        for (id, index) in sources {
            let Some(proof) = self.proof(id, *index)? else {
                continue;
            };
            if let Ok(record) = serde_json::from_str::<SiteRecord>(&proof.record) {
                crawls.push((record, proof));
            }
        }
        let group = |i: usize| -> Vec<usize> {
            std::iter::once(i)
                .chain((0..crawls.len()).filter(|&j| j != i && agree(&crawls[i].0, &crawls[j].0)))
                .take(max)
                .collect()
        };
        let best = (0..crawls.len())
            .map(group)
            .find(|g| g.len() >= max.min(2))
            .or_else(|| (!crawls.is_empty()).then(|| vec![0]));
        let Some(best) = best else {
            return Ok(Vec::new());
        };
        Ok(best
            .into_iter()
            .map(|i| crawls[i].1.clone())
            .take(max)
            .collect())
    }
}

#[derive(Debug)]
pub struct BatchStore {
    dir: PathBuf,
    ids: HashSet<Hash>,
    headers: HashMap<Hash, SignedHeader>,
    /// Per crawled homepage, the newest record from each crawler.
    crawls: HashMap<Box<str>, Box<[Holding]>>,
    /// The public keys of the crawlers in [`BatchStore::crawls`], as in
    /// the batch headers, each once.
    crawler_keys: Vec<Vec<u8>>,
    /// Where each key is in `crawler_keys`.
    crawler_ids: HashMap<Vec<u8>, u32>,
    /// Whether `crawls` is kept ([`crate::NetConfig::follow_crawls`]).
    following: bool,
    sizes: HashMap<Hash, u64>,
    oldest: BTreeSet<(u64, Hash)>,
    foreign_oldest: BTreeSet<(u64, Hash)>,
    allocated: u64,
    foreign_allocated: u64,
    limits: Option<StoreLimits>,
}

impl BatchStore {
    /// Opens the store in `dir`, creating it, and indexes the batches there.
    /// Unreadable files are deleted.
    pub fn open(dir: &Path) -> Result<BatchStore> {
        BatchStore::open_following(dir, true)
    }

    /// [`BatchStore::open`], noting where each crawl is for proofs only
    /// when `following` ([`crate::NetConfig::follow_crawls`]): a store that
    /// does not has none to give.
    pub fn open_following(dir: &Path, following: bool) -> Result<BatchStore> {
        Self::open_inner(dir, following, None)
    }

    /// Apply retention before building per-homepage proof indexes or replaying
    /// agreement. Only one bounded batch is read into memory at a time.
    pub fn open_retained(dir: &Path, following: bool, limits: StoreLimits) -> Result<Self> {
        Self::open_inner(dir, following, Some(limits))
    }

    fn open_inner(dir: &Path, following: bool, limits: Option<StoreLimits>) -> Result<Self> {
        let storage = limits.as_ref().and_then(|limits| limits.storage.as_ref());
        {
            let _mutation = storage.map(|budget| budget.mutation());
            let existed = dir.exists();
            let parent_before = dir.parent().map(file_bytes).transpose()?.unwrap_or(0);
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            if !existed {
                if let Some(budget) = storage {
                    budget.added(
                        file_bytes(dir)?.saturating_add(
                            dir.parent()
                                .map(file_bytes)
                                .transpose()?
                                .unwrap_or(0)
                                .saturating_sub(parent_before),
                        ),
                    );
                }
            }
        }
        let mut store = BatchStore {
            dir: dir.to_path_buf(),
            ids: HashSet::new(),
            headers: HashMap::new(),
            crawls: HashMap::new(),
            crawler_keys: Vec::new(),
            crawler_ids: HashMap::new(),
            following: following && limits.is_none(),
            sizes: HashMap::new(),
            oldest: BTreeSet::new(),
            foreign_oldest: BTreeSet::new(),
            allocated: 0,
            foreign_allocated: 0,
            limits: None,
        };
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                if path.extension().and_then(|e| e.to_str()) == Some("tmp") {
                    let _ = plumb_core::storage::remove_file(&path, storage.map(AsRef::as_ref));
                }
                continue;
            }
            match read_batch(&path) {
                Ok(batch) => {
                    let size = file_bytes(&path)?;
                    store.allocated = store.allocated.saturating_add(size);
                    store.sizes.insert(batch.id(), size);
                    store.note(&batch);
                }
                Err(err) => {
                    warn!("deleting the unreadable batch {}: {err:#}", path.display());
                    let _ = plumb_core::storage::remove_file(&path, storage.map(AsRef::as_ref));
                }
            }
        }
        if let Some(limits) = limits {
            store.limits = Some(limits.clone());
            store.foreign_oldest = store
                .oldest
                .iter()
                .filter(|(_, id)| store.headers[id].header.crawler != limits.own)
                .copied()
                .collect();
            store.foreign_allocated = store
                .foreign_oldest
                .iter()
                .map(|(_, id)| store.sizes[id])
                .sum();
            store.prune_keeping(limits.now, limits.epochs);
            if let Some(bytes) = limits.bytes {
                store.prune_to_bytes(bytes, &limits.own);
            }
            store.make_room(0);
            store.following = following;
            if following {
                for id in store.ids_oldest_first() {
                    if let Some(batch) = store.get(&id)? {
                        store.note(&batch);
                    }
                }
            }
        }
        debug!("holding {} batches in {}", store.ids.len(), dir.display());
        Ok(store)
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn contains(&self, id: &Hash) -> bool {
        self.ids.contains(id)
    }

    /// Saves a batch that has been checked. Saving one already held does
    /// nothing.
    pub fn insert(&mut self, batch: &Batch) -> Result<()> {
        self.insert_for_delivery(batch, 0).map(|_| ())
    }

    /// Save and reserve the bounded downstream inbox before exposing this
    /// batch to agreement/credits. A duplicate neither prunes nor reserves.
    pub(crate) fn insert_for_delivery(
        &mut self,
        batch: &Batch,
        delivery_bytes: u64,
    ) -> Result<(bool, Option<Reservation>)> {
        let id = batch.id();
        if self.ids.contains(&id) {
            return Ok((false, None));
        }
        let mut json = LimitedBytes {
            bytes: Vec::new(),
            limit: MAX_STORED_BATCH_BYTES,
        };
        serde_json::to_writer(&mut json, batch).context("encoding a bounded batch")?;
        let staging = allocation_for(json.bytes.len() as u64);
        let own = self
            .limits
            .as_ref()
            .is_some_and(|limits| limits.own == batch.header.header.crawler);
        if !own {
            if let Some(limit) = self.limits.as_ref().and_then(|limits| limits.bytes) {
                ensure!(
                    self.allocated
                        .saturating_sub(self.foreign_allocated)
                        .saturating_add(staging)
                        <= limit,
                    "batch and protected own bytes exceed storage quota"
                );
            }
            if let Some(budget) = self.storage() {
                ensure!(
                    staging.saturating_add(delivery_bytes) <= budget.status().limit_bytes,
                    "batch and inbox exceed storage quota"
                );
            }
            self.make_room(staging.saturating_add(delivery_bytes));
            if let Some(limit) = self.limits.as_ref().and_then(|limits| limits.bytes) {
                ensure!(
                    self.allocated.saturating_add(staging) <= limit,
                    "batch storage backpressure: own or retained bytes leave no room"
                );
            }
        }
        let budget = self.storage().cloned();
        let _mutation = budget.as_ref().map(|budget| budget.mutation());
        let mut reservation = budget
            .as_ref()
            .map(|budget| budget.reserve(staging.saturating_add(delivery_bytes), own))
            .transpose()?;
        let directory_before = file_bytes(&self.dir)?;
        let path = self.path(&id);
        let tmp = path.with_extension("tmp");
        // A collision is not permission to overwrite someone else's file.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        let result = (|| -> Result<()> {
            file.write_all(&json.bytes)
                .and_then(|()| file.sync_data())
                .with_context(|| format!("writing {}", tmp.display()))?;
            drop(file);
            fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
            Ok(())
        })();
        if let Err(err) = result {
            let _ = fs::remove_file(&tmp);
            let retained = match file_bytes(&tmp) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
                Err(_) => staging,
            };
            let growth = file_bytes(&self.dir)
                .unwrap_or(directory_before + 4096)
                .saturating_sub(directory_before);
            if let Some(reserved) = &mut reservation {
                reserved.commit(staging, 0, retained.saturating_add(growth));
            }
            return Err(err);
        }
        let size = file_bytes(&path).unwrap_or(staging);
        if let Some(reserved) = &mut reservation {
            reserved.commit(
                staging,
                0,
                size.saturating_add(
                    file_bytes(&self.dir)
                        .unwrap_or(directory_before + 4096)
                        .saturating_sub(directory_before),
                ),
            );
        }
        self.allocated = self.allocated.saturating_add(size);
        self.sizes.insert(id, size);
        if !own && self.limits.is_some() {
            self.foreign_allocated = self.foreign_allocated.saturating_add(size);
        }
        self.note(batch);
        Ok((true, reservation))
    }

    pub fn allocated_bytes(&self) -> u64 {
        self.allocated
    }

    pub(crate) fn storage(&self) -> Option<&Arc<StorageBudget>> {
        self.limits
            .as_ref()
            .and_then(|limits| limits.storage.as_ref())
    }

    fn make_room(&mut self, needed: u64) {
        let Some(limits) = self.limits.clone() else {
            return;
        };
        let local_target = limits.bytes.map(|limit| limit.saturating_sub(needed));
        self.prune_until(local_target, &limits.own, needed);
    }

    /// The batch with id `id`, if held.
    pub fn get(&self, id: &Hash) -> Result<Option<Batch>> {
        self.located(id).map_or(Ok(None), |path| read_held(&path))
    }

    /// Where batch `id` is, if held: for [`read_held`] once the store's
    /// lock is let go.
    pub fn located(&self, id: &Hash) -> Option<PathBuf> {
        self.ids.contains(id).then(|| self.path(id))
    }

    /// The proof of the newest crawled record held for `domain`.
    pub fn proof(&self, domain: &str, now: u64) -> Result<Option<RecordProof>> {
        Ok(self.proofs(domain, 1, now)?.into_iter().next())
    }

    /// Up to `max` proofs of crawls of `domain` from different crawlers
    /// that agree with each other ([`agree`]): the newest crawl that others
    /// agree with, then those others, newest first. When no two agree, the
    /// newest crawl alone. Crawls from batches too old to check out at
    /// `now` ([`SignedHeader::expired`]) are left out: one such proof would
    /// make the searcher throw the whole answer away.
    pub fn proofs(&self, domain: &str, max: usize, now: u64) -> Result<Vec<RecordProof>> {
        self.reader().proofs(&self.proof_sources(domain, now), max)
    }

    /// Where the crawls of `domain` that [`BatchStore::proofs`] picks from
    /// are: (batch, record index), newest first. Cheap, from memory; the
    /// batches are read with a [`BatchReader`], which needs no lock on the
    /// store.
    pub fn proof_sources(&self, domain: &str, now: u64) -> Vec<(Hash, usize)> {
        let Some(held) = self.crawls.get(domain) else {
            return Vec::new();
        };
        let mut held: Vec<&Holding> = held
            .iter()
            .filter(|h| {
                self.headers
                    .get(&h.batch)
                    .is_some_and(|header| !header.expired(now))
            })
            .collect();
        held.sort_by_key(|h| std::cmp::Reverse(h.created_at));
        held.into_iter()
            .map(|h| (h.batch, h.index as usize))
            .collect()
    }

    /// Reads held batches from disk for proofs.
    pub fn reader(&self) -> BatchReader {
        BatchReader {
            dir: self.dir.clone(),
            cache: HashMap::new(),
            budget: self.storage().cloned(),
        }
    }

    /// The headers of the batches held from `epoch` on, newest first, at
    /// most `max`.
    pub fn headers_since(&self, epoch: u64, max: usize) -> Vec<SignedHeader> {
        let mut headers: Vec<SignedHeader> = self
            .headers
            .values()
            .filter(|h| h.header.epoch >= epoch)
            .cloned()
            .collect();
        headers.sort_by_key(|h| std::cmp::Reverse(h.header.created_at));
        headers.truncate(max);
        headers
    }

    /// What each crawler sent, from the headers of the batches held, the
    /// most homepages in the last day first. `me` and `trusted` are left
    /// for the caller.
    pub fn crawlers(&self, now: u64) -> Vec<CrawlerView> {
        const DAY: u64 = 24 * 60 * 60;
        let mut by_key: HashMap<&[u8], CrawlerView> = HashMap::new();
        for signed in self.headers.values() {
            let h = &signed.header;
            let view = by_key.entry(h.crawler.as_slice()).or_default();
            let count = u64::from(h.count);
            let age = now.saturating_sub(h.created_at);
            if age < DAY {
                view.homepages_last_day += count;
            }
            if age < 7 * DAY {
                view.homepages_last_week += count;
            }
            view.homepages_held += count;
            view.batches_held += 1;
            view.last_batch_at = view.last_batch_at.max(h.created_at);
        }
        // Keys are decoded once per crawler, not once per batch.
        let mut crawlers: Vec<CrawlerView> = by_key
            .into_iter()
            .filter_map(|(key, view)| {
                let peer = PublicKey::try_decode_protobuf(key).ok()?.to_peer_id();
                Some(CrawlerView {
                    peer_id: peer.to_string(),
                    ..view
                })
            })
            .collect();
        crawlers.sort_by(|a, b| {
            (b.homepages_last_day, b.homepages_last_week, b.last_batch_at)
                .cmp(&(a.homepages_last_day, a.homepages_last_week, a.last_batch_at))
                .then_with(|| a.peer_id.cmp(&b.peer_id))
        });
        crawlers
    }

    /// The ids of the batches held, oldest first.
    pub fn ids_oldest_first(&self) -> Vec<Hash> {
        let mut ids: Vec<(u64, Hash)> = self
            .headers
            .iter()
            .map(|(id, h)| (h.header.created_at, *id))
            .collect();
        ids.sort();
        ids.into_iter().map(|(_, id)| id).collect()
    }

    /// Deletes batches older than [`RETAIN_EPOCHS`] before `now`.
    pub fn prune(&mut self, now: u64) {
        self.prune_keeping(now, RETAIN_EPOCHS);
    }

    /// Deletes batches older than `epochs` days before `now`.
    pub fn prune_keeping(&mut self, now: u64, epochs: u64) {
        let oldest = epoch_of(now).saturating_sub(epochs);
        let old: HashSet<Hash> = self
            .headers
            .iter()
            .filter(|(_, header)| header.header.epoch < oldest)
            .map(|(id, _)| *id)
            .collect();
        self.remove(old);
    }

    /// Deletes the oldest batches other crawlers made until the batches held
    /// take no more than `bytes` on disk. The batches crawled with `own`, this
    /// node's key, stay: no other node may hold them yet.
    pub fn prune_to_bytes(&mut self, bytes: u64, own: &[u8]) {
        self.prune_until(Some(bytes), own, 0);
    }

    fn prune_until(&mut self, target: Option<u64>, own: &[u8], needed: u64) {
        // The ordered metadata is maintained on insert/remove: no directory
        // scan or sort for each batch in a catch-up burst.
        let configured = self.limits.as_ref().is_some_and(|limits| limits.own == own);
        let fallback: BTreeSet<_> = if configured {
            BTreeSet::new()
        } else {
            self.oldest
                .iter()
                .filter(|(_, id)| self.headers[id].header.crawler != own)
                .copied()
                .collect()
        };
        let mut failed = BTreeSet::new();
        loop {
            let fits_local = target.is_none_or(|bytes| self.allocated <= bytes);
            let fits_total = self.storage().is_none_or(|budget| {
                let status = budget.status();
                status
                    .used_bytes
                    .saturating_add(status.reserved_bytes)
                    .saturating_add(needed)
                    <= status.limit_bytes
            });
            if fits_local && fits_total {
                break;
            }
            let candidates = if configured {
                &self.foreign_oldest
            } else {
                &fallback
            };
            let Some((_, id)) = candidates
                .iter()
                .find(|(_, id)| self.contains(id) && !failed.contains(id))
                .copied()
            else {
                break;
            };
            self.remove(HashSet::from([id]));
            if self.contains(&id) {
                failed.insert(id);
            }
        }
    }

    /// Deletes only through retention; failed deletions retain their metadata
    /// and allocation. Proof cleanup visits this batch's records, not all sites.
    fn remove(&mut self, old: HashSet<Hash>) {
        let budget = self.storage().cloned();
        let _mutation = budget.as_ref().map(|budget| budget.mutation());
        for id in old {
            let domains: Vec<String> = if self.following {
                self.get(&id)
                    .ok()
                    .flatten()
                    .into_iter()
                    .flat_map(|batch| batch.records)
                    .filter_map(|line| serde_json::from_str::<SiteRecord>(&line).ok())
                    .map(|record| record.domain)
                    .collect()
            } else {
                Vec::new()
            };
            match fs::remove_file(self.path(&id)) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    warn!("cannot delete an old batch: {err}");
                    continue;
                }
            }
            let bytes = self.sizes.remove(&id).unwrap_or(0);
            self.allocated = self.allocated.saturating_sub(bytes);
            if let Some(budget) = &budget {
                budget.removed(bytes);
            }
            self.ids.remove(&id);
            if let Some(header) = self.headers.remove(&id) {
                self.oldest.remove(&(header.header.created_at, id));
                if self.foreign_oldest.remove(&(header.header.created_at, id)) {
                    self.foreign_allocated = self.foreign_allocated.saturating_sub(bytes);
                }
            }
            for domain in domains {
                if let Some(held) = self.crawls.get_mut(domain.as_str()) {
                    *held = held
                        .iter()
                        .filter(|holding| holding.batch != id)
                        .copied()
                        .collect();
                    if held.is_empty() {
                        self.crawls.remove(domain.as_str());
                    }
                }
            }
        }
    }

    /// Where `key` is in [`BatchStore::crawler_keys`], added if new.
    fn crawler_id(&mut self, key: &[u8]) -> u32 {
        if let Some(&id) = self.crawler_ids.get(key) {
            return id;
        }
        let id = u32::try_from(self.crawler_keys.len()).unwrap_or(u32::MAX);
        self.crawler_keys.push(key.to_vec());
        self.crawler_ids.insert(key.to_vec(), id);
        id
    }

    fn path(&self, id: &Hash) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn note(&mut self, batch: &Batch) {
        let id = batch.id();
        self.ids.insert(id);
        self.headers.insert(id, batch.header.clone());
        self.oldest.insert((batch.header.header.created_at, id));
        if self
            .limits
            .as_ref()
            .is_some_and(|limits| limits.own != batch.header.header.crawler)
        {
            self.foreign_oldest
                .insert((batch.header.header.created_at, id));
        }
        if !self.following {
            return;
        }
        let h = &batch.header.header;
        let created_at = h.created_at;
        let crawler = libp2p::identity::PublicKey::try_decode_protobuf(&h.crawler)
            .map(|key| key.to_peer_id())
            .ok();
        let key = self.crawler_id(&h.crawler);
        let created = u32::try_from(created_at).unwrap_or(u32::MAX);
        for (index, line) in batch.records.iter().enumerate() {
            let Ok(record) = serde_json::from_str::<SiteRecord>(line) else {
                continue;
            };
            // Only crawls others can check: a node's own fetches of sites
            // it was not assigned (to settle a dispute) prove nothing to
            // anyone else.
            // And only those a searcher counts: a crawl from outside its
            // batch's epoch would make the searcher distrust the answer.
            let assigned =
                crawler.is_some_and(|c| is_assigned(h.epoch, &c, &record.domain, h.share_ppm));
            let in_window = record
                .crawled_at
                .is_some_and(|at| crate::batch::in_crawl_window(at, h.epoch));
            if !in_window || !assigned {
                continue;
            }
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            let holding = Holding {
                batch: id,
                index,
                created_at: created,
                crawler: key,
            };
            match self.crawls.get_mut(record.domain.as_str()) {
                Some(held) => match held.iter_mut().find(|h| h.crawler == key) {
                    Some(old) if old.created_at > created => {}
                    Some(old) => *old = holding,
                    None => {
                        let mut more = held.to_vec();
                        more.push(holding);
                        *held = more.into_boxed_slice();
                    }
                },
                None => {
                    self.crawls
                        .insert(record.domain.into_boxed_str(), Box::new([holding]));
                }
            }
        }
    }
}

/// The batch at a path [`BatchStore::located`] gave; `None` when it was
/// deleted meanwhile.
pub fn read_held(path: &Path) -> Result<Option<Batch>> {
    read_held_with_budget(path, None)
}

pub fn read_held_with_budget(path: &Path, budget: Option<&StorageBudget>) -> Result<Option<Batch>> {
    let _mutation = budget.map(StorageBudget::mutation);
    match read_batch(path) {
        Ok(batch) => Ok(Some(batch)),
        Err(err) if is_not_found(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

fn read_batch(path: &Path) -> Result<Batch> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_STORED_BATCH_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        bytes.len() <= MAX_STORED_BATCH_BYTES,
        "oversized stored batch"
    );
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|io| io.kind() == io::ErrorKind::NotFound)
    })
}

#[cfg(test)]
mod tests {
    use libp2p::identity::Keypair;

    use super::*;
    use crate::assign::{is_assigned, EPOCH_SECS, MAX_SHARE_PPM};
    use crate::batch::MAX_BATCH_AGE_EPOCHS;

    fn quota_batch(key: &Keypair, now: u64, serial: u64) -> Batch {
        let crawler = key.public().to_peer_id();
        let domain = (0..)
            .map(|i| format!("q{serial}-{i}.example"))
            .find(|domain| is_assigned(epoch_of(now), &crawler, domain, MAX_SHARE_PPM))
            .unwrap();
        let mut record = SiteRecord::new(domain);
        record.crawled_at = Some(now);
        record.body_text = Some("Signed crawl retained verbatim".into());
        Batch::sign(key, &[record], epoch_of(now), MAX_SHARE_PPM, now)
            .unwrap()
            .unwrap()
    }

    fn limits(
        own: &Keypair,
        bytes: u64,
        now: u64,
        storage: Option<Arc<StorageBudget>>,
    ) -> StoreLimits {
        StoreLimits {
            own: own.public().encode_protobuf(),
            bytes: Some(bytes),
            now,
            epochs: RETAIN_EPOCHS,
            storage,
        }
    }

    #[test]
    fn over_quota_startup_prunes_before_proof_indexing_and_preserves_raw_credits_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let batches = dir.path().join("batches");
        let now = 1_790_000_000;
        let own = Keypair::generate_ed25519();
        let foreign = Keypair::generate_ed25519();
        let mine = quota_batch(&own, now - 200, 0);
        let mut store = BatchStore::open(&batches).unwrap();
        store.insert(&mine).unwrap();
        for i in 1..80 {
            store
                .insert(&quota_batch(&foreign, now - 100 + i, i))
                .unwrap();
        }
        let size = file_bytes(&store.path(&mine.id())).unwrap();
        drop(store);
        for name in [
            "records.jsonl",
            "inbox.jsonl",
            "inbox.absorbing",
            "credits.json",
        ] {
            fs::write(dir.path().join(name), b"protected bytes").unwrap();
        }
        let budget = StorageBudget::open(dir.path(), 128 * 1024).unwrap();
        let store = BatchStore::open_retained(
            &batches,
            true,
            limits(&own, 3 * size, now, Some(budget.clone())),
        )
        .unwrap();
        assert_eq!(store.len(), 3);
        assert_eq!(
            store.crawls.len(),
            3,
            "discarded batches must never enter the proof index"
        );
        assert_eq!(store.get(&mine.id()).unwrap().unwrap(), mine);
        assert!(store.allocated_bytes() <= 3 * size);
        assert!(
            crate::storage::directory_bytes(dir.path()).unwrap() <= budget.status().limit_bytes
        );
        for name in [
            "records.jsonl",
            "inbox.jsonl",
            "inbox.absorbing",
            "credits.json",
        ] {
            assert_eq!(fs::read(dir.path().join(name)).unwrap(), b"protected bytes");
        }
    }

    #[test]
    fn catchup_admission_bounds_each_write_without_a_maintenance_tick_and_duplicates_are_noops() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_790_000_000;
        let own = Keypair::generate_ed25519();
        let foreign = Keypair::generate_ed25519();
        let budget = StorageBudget::open(dir.path(), 128 * 1024).unwrap();
        let mut store = BatchStore::open_retained(
            &dir.path().join("batches"),
            true,
            limits(&own, 32 * 1024, now, Some(budget.clone())),
        )
        .unwrap();
        let mine = quota_batch(&own, now - 200, 0);
        store.insert(&mine).unwrap();
        let mut last = mine.clone();
        for i in 1..100 {
            last = quota_batch(&foreign, now - 100 + i, i);
            store.insert(&last).unwrap();
            assert!(store.allocated_bytes() <= 32 * 1024);
            assert!(store.len() <= 4 && store.crawls.len() <= 4);
            assert!(store.contains(&mine.id()));
            assert!(crate::storage::directory_bytes(dir.path()).unwrap() <= 128 * 1024);
        }
        let held = store.ids.clone();
        let used = budget.status().used_bytes;
        assert!(!store.insert_for_delivery(&last, u64::MAX).unwrap().0);
        assert_eq!(store.ids, held);
        assert_eq!(budget.status().used_bytes, used);
        assert_eq!(budget.status().reserved_bytes, 0);
    }

    #[test]
    fn own_only_overage_survives_restart_and_blocks_foreign_writes() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_790_000_000;
        let own = Keypair::generate_ed25519();
        let foreign = Keypair::generate_ed25519();
        let mut store = BatchStore::open(dir.path()).unwrap();
        for i in 0..4 {
            store.insert(&quota_batch(&own, now - 20 + i, i)).unwrap();
        }
        drop(store);
        let budget = StorageBudget::open(dir.path(), 16 * 1024).unwrap();
        let mut store = BatchStore::open_retained(
            dir.path(),
            true,
            limits(&own, 8192, now, Some(budget.clone())),
        )
        .unwrap();
        assert_eq!(store.len(), 4);
        assert!(store.insert(&quota_batch(&foreign, now, 10)).is_err());
        store.insert(&quota_batch(&own, now, 11)).unwrap();
        assert_eq!(store.len(), 5);
        assert!(budget.status().backpressure);
        assert_eq!(budget.status().reserved_bytes, 0);
    }

    #[test]
    fn oversized_and_failed_atomic_writes_do_not_leave_files_or_reservations() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_790_000_000;
        let own = Keypair::generate_ed25519();
        let foreign = Keypair::generate_ed25519();
        let budget = StorageBudget::open(dir.path(), 128 * 1024).unwrap();
        let mut store = BatchStore::open_retained(
            dir.path(),
            true,
            limits(&own, 32 * 1024, now, Some(budget.clone())),
        )
        .unwrap();
        let kept = quota_batch(&foreign, now - 1, 0);
        store.insert(&kept).unwrap();
        let mut oversized = quota_batch(&foreign, now, 1);
        oversized.records = vec!["x".repeat(MAX_STORED_BATCH_BYTES + 1)];
        assert!(store.insert(&oversized).is_err());
        assert_eq!(store.len(), 1);
        assert!(store.contains(&kept.id()));
        let failed = quota_batch(&foreign, now, 2);
        fs::create_dir(store.path(&failed.id())).unwrap(); // force rename failure after the write
        let used = budget.status().used_bytes;
        assert!(store.insert(&failed).is_err());
        assert!(!store.contains(&failed.id()));
        assert!(!store.path(&failed.id()).with_extension("tmp").exists());
        assert_eq!(budget.status().used_bytes, used);
        assert_eq!(budget.status().reserved_bytes, 0);
    }

    #[test]
    fn batches_are_kept_across_reopening_proven_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_790_000_000;
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let domain = (0..)
            .map(|i| format!("s{i}.com"))
            .find(|d| is_assigned(epoch_of(now), &peer, d, MAX_SHARE_PPM))
            .unwrap();
        let mut record = SiteRecord::new(domain.as_str());
        record.title = Some("Hello".into());
        record.crawled_at = Some(now - 5);
        let batch = Batch::sign(&key, &[record], epoch_of(now), MAX_SHARE_PPM, now)
            .unwrap()
            .unwrap();

        let mut store = BatchStore::open(dir.path()).unwrap();
        store.insert(&batch).unwrap();
        drop(store);
        let mut store = BatchStore::open(dir.path()).unwrap();
        assert!(store.contains(&batch.id()));
        let proof = store.proof(&domain, now).unwrap().unwrap();
        assert_eq!(proof.verify(now).unwrap().0.title.as_deref(), Some("Hello"));

        store.prune(now + (RETAIN_EPOCHS + 1) * EPOCH_SECS);
        assert!(store.is_empty());
        assert!(store.proof(&domain, now).unwrap().is_none());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_full_store_deletes_other_crawlers_oldest_batches_first() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_790_000_000;
        let me = Keypair::generate_ed25519();
        let other = Keypair::generate_ed25519();
        let sign = |key: &Keypair, i: u64| {
            let mut record = SiteRecord::new(format!("s{i}.com"));
            record.crawled_at = Some(now - 5);
            Batch::sign(key, &[record], epoch_of(now), MAX_SHARE_PPM, now + i)
                .unwrap()
                .unwrap()
        };
        let mine = sign(&me, 0);
        let older = sign(&other, 1);
        let newer = sign(&other, 2);
        let mut store = BatchStore::open(dir.path()).unwrap();
        for batch in [&mine, &older, &newer] {
            store.insert(batch).unwrap();
        }
        let size = |batch: &Batch| file_bytes(&store.path(&batch.id())).unwrap();
        let all = size(&mine) + size(&older) + size(&newer);
        let own = me.public().encode_protobuf();

        store.prune_to_bytes(all, &own);
        assert_eq!(store.len(), 3);
        // One byte too many: the oldest of the other crawler's goes.
        store.prune_to_bytes(all - 1, &own);
        assert!(!store.contains(&older.id()));
        assert!(store.contains(&newer.id()) && store.contains(&mine.id()));
        // However little room is left, this node's own batch stays.
        store.prune_to_bytes(0, &own);
        assert!(!store.contains(&newer.id()));
        assert!(store.contains(&mine.id()));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_store_not_following_crawls_holds_batches_without_proofs() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_790_000_000;
        let key = Keypair::generate_ed25519();
        let crawler = key.public().to_peer_id();
        let domain = (0..)
            .map(|i| format!("site{i}.example"))
            .find(|d| is_assigned(epoch_of(now), &crawler, d, MAX_SHARE_PPM))
            .unwrap();
        let mut record = SiteRecord::new(domain.as_str());
        record.crawled_at = Some(now - 5);
        let batch = Batch::sign(&key, &[record], epoch_of(now), MAX_SHARE_PPM, now)
            .unwrap()
            .unwrap();
        let mut store = BatchStore::open_following(dir.path(), false).unwrap();
        store.insert(&batch).unwrap();
        assert!(store.contains(&batch.id()));
        assert!(store.get(&batch.id()).unwrap().is_some(), "still served");
        assert!(store.proof(&domain, now).unwrap().is_none());
        assert_eq!(store.crawlers(now).len(), 1);
        drop(store);
        // Following again, the crawls held are noted.
        let store = BatchStore::open(dir.path()).unwrap();
        assert!(store.proof(&domain, now).unwrap().is_some());
    }

    #[test]
    fn crawlers_are_counted_by_day_and_week() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_790_000_000;
        let busy = Keypair::generate_ed25519();
        let quiet = Keypair::generate_ed25519();
        let batch = |key: &Keypair, n: usize, at: u64| {
            let records: Vec<SiteRecord> = (0..n)
                .map(|i| {
                    let mut record = SiteRecord::new(format!("s{i}-{at}.com").as_str());
                    record.crawled_at = Some(at);
                    record
                })
                .collect();
            Batch::sign(key, &records, epoch_of(at), MAX_SHARE_PPM, at)
                .unwrap()
                .unwrap()
        };
        let mut store = BatchStore::open(dir.path()).unwrap();
        store.insert(&batch(&busy, 3, now - 60)).unwrap();
        store
            .insert(&batch(&busy, 2, now - 3 * EPOCH_SECS))
            .unwrap();
        store
            .insert(&batch(&quiet, 4, now - 10 * EPOCH_SECS))
            .unwrap();

        let crawlers = store.crawlers(now);
        assert_eq!(crawlers.len(), 2);
        let first = &crawlers[0];
        assert_eq!(first.peer_id, busy.public().to_peer_id().to_string());
        assert_eq!(
            (
                first.homepages_last_day,
                first.homepages_last_week,
                first.homepages_held,
                first.batches_held,
                first.last_batch_at
            ),
            (3, 5, 5, 2, now - 60)
        );
        let second = &crawlers[1];
        assert_eq!(second.peer_id, quiet.public().to_peer_id().to_string());
        assert_eq!(
            (
                second.homepages_last_day,
                second.homepages_last_week,
                second.homepages_held
            ),
            (0, 0, 4)
        );
        assert!(!first.me && !first.trusted);
    }

    #[test]
    fn a_crawl_of_a_site_not_assigned_is_never_a_proof() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_790_000_000;
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let domain = (0..)
            .map(|i| format!("s{i}.com"))
            .find(|d| !is_assigned(epoch_of(now), &peer, d, MAX_SHARE_PPM))
            .unwrap();
        let mut record = SiteRecord::new(domain.as_str());
        record.crawled_at = Some(now - 5);
        let batch = Batch::sign(&key, &[record], epoch_of(now), MAX_SHARE_PPM, now)
            .unwrap()
            .unwrap();
        let mut store = BatchStore::open(dir.path()).unwrap();
        store.insert(&batch).unwrap();
        assert!(store.contains(&batch.id()));
        assert!(store.proof(&domain, now).unwrap().is_none());
    }

    #[test]
    fn a_crawl_from_outside_its_batchs_epoch_is_never_a_proof() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_790_000_000;
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let domain = (0..)
            .map(|i| format!("s{i}.com"))
            .find(|d| is_assigned(epoch_of(now), &peer, d, MAX_SHARE_PPM))
            .unwrap();
        let mut record = SiteRecord::new(domain.as_str());
        // Crawled three days before the epoch the batch is signed in.
        record.crawled_at = Some(now - 3 * EPOCH_SECS);
        let batch = Batch::sign(&key, &[record], epoch_of(now), MAX_SHARE_PPM, now)
            .unwrap()
            .unwrap();
        assert!(
            batch.proof(0).verify(now).is_err(),
            "a searcher would refuse it"
        );
        let mut store = BatchStore::open(dir.path()).unwrap();
        store.insert(&batch).unwrap();
        assert!(store.proof(&domain, now).unwrap().is_none());
    }

    #[test]
    fn proofs_come_from_crawlers_that_agree() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_790_000_000;
        let keys: Vec<Keypair> = (0..3).map(|_| Keypair::generate_ed25519()).collect();
        let domain = (0..)
            .map(|i| format!("s{i}.com"))
            .find(|d| {
                keys.iter()
                    .all(|k| is_assigned(epoch_of(now), &k.public().to_peer_id(), d, MAX_SHARE_PPM))
            })
            .unwrap();
        let mut store = BatchStore::open(dir.path()).unwrap();
        // Two crawlers agree; a third, newer one does not.
        for (i, (key, title)) in keys
            .iter()
            .zip(["Hello there", "Hello there!", "Buy pills"])
            .enumerate()
        {
            let mut record = SiteRecord::new(domain.as_str());
            record.title = Some(title.into());
            record.crawled_at = Some(now - 5);
            let made = now - 100 + i as u64;
            let batch = Batch::sign(key, &[record], epoch_of(now), MAX_SHARE_PPM, made)
                .unwrap()
                .unwrap();
            store.insert(&batch).unwrap();
        }
        let proofs = store.proofs(&domain, 2, now).unwrap();
        let titles: Vec<String> = proofs
            .iter()
            .map(|p| p.verify(now).unwrap().0.title.unwrap())
            .collect();
        assert_eq!(titles, ["Hello there!", "Hello there"]);
        // Just one asked for: the newest crawl.
        let one = store.proof(&domain, now).unwrap().unwrap();
        assert_eq!(
            one.verify(now).unwrap().0.title.as_deref(),
            Some("Buy pills")
        );
    }

    #[test]
    fn crawls_too_old_to_prove_are_not_offered_as_proofs() {
        let dir = tempfile::tempdir().unwrap();
        let made = 1_790_000_000;
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let domain = (0..)
            .map(|i| format!("s{i}.com"))
            .find(|d| is_assigned(epoch_of(made), &peer, d, MAX_SHARE_PPM))
            .unwrap();
        let mut record = SiteRecord::new(domain.as_str());
        record.crawled_at = Some(made - 5);
        let batch = Batch::sign(&key, &[record], epoch_of(made), MAX_SHARE_PPM, made)
            .unwrap()
            .unwrap();
        let mut store = BatchStore::open(dir.path()).unwrap();
        store.insert(&batch).unwrap();
        let week = made + MAX_BATCH_AGE_EPOCHS * EPOCH_SECS;
        let proof = store.proof(&domain, week).unwrap().unwrap();
        assert!(proof.verify(week).is_ok());
        let later = week + EPOCH_SECS;
        assert!(store.proof(&domain, later).unwrap().is_none());
        assert!(store.contains(&batch.id()));
    }
}
