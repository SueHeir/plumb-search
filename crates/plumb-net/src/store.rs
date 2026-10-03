//! The batches a node holds: its own and the ones it accepted from others,
//! kept on disk so it can hand them on and prove its search answers.
//!
//! ```text
//! DIR/net/batches/<id>.json   one signed batch each
//! ```
//!
//! For every crawled homepage the store remembers the newest batch holding
//! it, so a search answer can carry a [`RecordProof`] for each hit. Batches
//! older than [`RETAIN_EPOCHS`] are deleted.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use plumb_core::SiteRecord;
use tracing::{debug, warn};

use crate::assign::epoch_of;
use crate::batch::{Batch, RecordProof, SignedHeader};
use crate::hash::Hash;

/// Batches are kept for this many epochs.
pub const RETAIN_EPOCHS: u64 = 35;

/// Where a crawled homepage's newest record is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Holding {
    batch: Hash,
    index: usize,
    created_at: u64,
}

#[derive(Debug)]
pub struct BatchStore {
    dir: PathBuf,
    ids: HashSet<Hash>,
    headers: HashMap<Hash, SignedHeader>,
    newest: HashMap<String, Holding>,
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
            newest: HashMap::new(),
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
    pub fn proof(&self, domain: &str) -> Result<Option<RecordProof>> {
        let Some(holding) = self.newest.get(domain) else {
            return Ok(None);
        };
        Ok(self
            .get(&holding.batch)?
            .map(|batch| batch.proof(holding.index)))
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
        self.newest
            .retain(|_, holding| !old.contains(&holding.batch));
    }

    fn path(&self, id: &Hash) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn note(&mut self, batch: &Batch) {
        let id = batch.id();
        let created_at = batch.header.header.created_at;
        for (index, line) in batch.records.iter().enumerate() {
            let Ok(record) = serde_json::from_str::<SiteRecord>(line) else {
                continue;
            };
            if record.crawled_at.is_none() {
                continue;
            }
            let holding = Holding {
                batch: id,
                index,
                created_at,
            };
            self.newest
                .entry(record.domain)
                .and_modify(|held| {
                    if held.created_at <= created_at {
                        *held = holding;
                    }
                })
                .or_insert(holding);
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
        let proof = store.proof(&domain).unwrap().unwrap();
        assert_eq!(proof.verify(now).unwrap().0.title.as_deref(), Some("Hello"));

        store.prune(now + (RETAIN_EPOCHS + 1) * EPOCH_SECS);
        assert!(store.is_empty());
        assert!(store.proof(&domain).unwrap().is_none());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
