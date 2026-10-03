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
//! [`crate::agree`]). Batches older than [`RETAIN_EPOCHS`] are deleted.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use libp2p::identity::PublicKey;
use plumb_core::SiteRecord;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::agree::agree;
use crate::assign::{epoch_of, is_assigned};
use crate::batch::{Batch, RecordProof, SignedHeader};
use crate::hash::Hash;

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

/// Where one crawler's newest record of a homepage is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Holding {
    batch: Hash,
    index: usize,
    created_at: u64,
    /// The crawler's public key, as in the batch header.
    crawler: Vec<u8>,
}

#[derive(Debug)]
pub struct BatchStore {
    dir: PathBuf,
    ids: HashSet<Hash>,
    headers: HashMap<Hash, SignedHeader>,
    /// Per crawled homepage, the newest record from each crawler.
    crawls: HashMap<String, Vec<Holding>>,
}

impl BatchStore {
    /// Opens the store in `dir`, creating it, and indexes the batches there.
    /// Unreadable files are deleted.
    pub fn open(dir: &Path) -> Result<BatchStore> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut store = BatchStore {
            dir: dir.to_path_buf(),
            ids: HashSet::new(),
            headers: HashMap::new(),
            crawls: HashMap::new(),
        };
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                if path.extension().and_then(|e| e.to_str()) == Some("tmp") {
                    let _ = fs::remove_file(&path);
                }
                continue;
            }
            match read_batch(&path) {
                Ok(batch) => store.note(&batch),
                Err(err) => {
                    warn!("deleting the unreadable batch {}: {err:#}", path.display());
                    let _ = fs::remove_file(&path);
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
        let id = batch.id();
        if self.ids.contains(&id) {
            return Ok(());
        }
        let path = self.path(&id);
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_vec(batch).context("encoding a batch")?;
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(&json)
            .and_then(|()| file.sync_data())
            .with_context(|| format!("writing {}", tmp.display()))?;
        drop(file);
        fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
        self.note(batch);
        Ok(())
    }

    /// The batch with id `id`, if held.
    pub fn get(&self, id: &Hash) -> Result<Option<Batch>> {
        if !self.ids.contains(id) {
            return Ok(None);
        }
        match read_batch(&self.path(id)) {
            Ok(batch) => Ok(Some(batch)),
            Err(err) if is_not_found(&err) => Ok(None),
            Err(err) => Err(err),
        }
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
        let Some(held) = self.crawls.get(domain) else {
            return Ok(Vec::new());
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
        let mut crawls: Vec<(SiteRecord, RecordProof)> = Vec::new();
        for holding in held {
            let Some(batch) = self.get(&holding.batch)? else {
                continue;
            };
            let proof = batch.proof(holding.index);
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
        let oldest = epoch_of(now).saturating_sub(RETAIN_EPOCHS);
        let old: Vec<Hash> = self
            .headers
            .iter()
            .filter(|(_, header)| header.header.epoch < oldest)
            .map(|(id, _)| *id)
            .collect();
        if old.is_empty() {
            return;
        }
        for id in &old {
            match fs::remove_file(self.path(id)) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    warn!("cannot delete an old batch: {err}");
                    continue;
                }
            }
            self.ids.remove(id);
            self.headers.remove(id);
        }
        self.crawls.retain(|_, held| {
            held.retain(|holding| !old.contains(&holding.batch));
            !held.is_empty()
        });
    }

    fn path(&self, id: &Hash) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn note(&mut self, batch: &Batch) {
        let id = batch.id();
        let h = &batch.header.header;
        let created_at = h.created_at;
        let crawler = libp2p::identity::PublicKey::try_decode_protobuf(&h.crawler)
            .map(|key| key.to_peer_id())
            .ok();
        for (index, line) in batch.records.iter().enumerate() {
            let Ok(record) = serde_json::from_str::<SiteRecord>(line) else {
                continue;
            };
            // Only crawls others can check: a node's own fetches of sites
            // it was not assigned (to settle a dispute) prove nothing to
            // anyone else.
            let assigned =
                crawler.is_some_and(|c| is_assigned(h.epoch, &c, &record.domain, h.share_ppm));
            if record.crawled_at.is_none() || !assigned {
                continue;
            }
            let holding = Holding {
                batch: id,
                index,
                created_at,
                crawler: batch.header.header.crawler.clone(),
            };
            let held = self.crawls.entry(record.domain).or_default();
            match held.iter_mut().find(|h| h.crawler == holding.crawler) {
                Some(old) if old.created_at > created_at => {}
                Some(old) => *old = holding,
                None => held.push(holding),
            }
        }
        self.ids.insert(id);
        self.headers.insert(id, batch.header.clone());
    }
}

fn read_batch(path: &Path) -> Result<Batch> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
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
