//! Buckets this node fetched for its own network searches, kept so that
//! the same search, or another one that needs the same buckets, is answered
//! here instead of asking the network again.
//!
//! What is kept is whole buckets, padding included, never the query or its
//! results: a bucket holds the sites of many unrelated keys, and the random
//! padding buckets of a search are kept just like the real ones, so what is
//! on disk does not say what was searched any more than the requests did.
//!
//! Only answers whose proofs checked out are kept, and they are checked
//! again when read (a proof can expire). A bucket is kept for
//! [`FRESH_FOR`] seconds, then asked of the network again; at most
//! [`MAX_BUCKETS`] are kept, the oldest going first.
//!
//! ```text
//! <dir>/<bucket>.json   {"fetched_at": .., "answers": [[BucketRecord, ..], ..]}
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::proto::BucketRecord;

/// How long a fetched bucket is used before it is asked for again: as long
/// as the crawls that fill it take to come round (about twice a day).
pub const FRESH_FOR: u64 = 12 * 60 * 60;
/// Most buckets kept, a quarter of them all.
pub const MAX_BUCKETS: usize = 4096;

/// One bucket as fetched: the records of each node that answered for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Saved {
    fetched_at: u64,
    answers: Vec<Vec<BucketRecord>>,
}

/// Fetched buckets, on disk, with when each was fetched in memory.
#[derive(Debug)]
pub struct BucketCache {
    dir: PathBuf,
    fetched: Mutex<HashMap<u32, u64>>,
}

impl BucketCache {
    /// Opens the cache in `dir`, creating it, and forgets the buckets that
    /// are no longer fresh at `now`.
    pub fn open(dir: &Path, now: u64) -> BucketCache {
        if let Err(err) = fs::create_dir_all(dir) {
            debug!("bucket cache {}: {err}", dir.display());
        }
        let mut fetched = HashMap::new();
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let bucket = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|_| path.extension().is_some_and(|e| e == "json"));
            let saved = bucket.and_then(|b| read(&path).map(|s| (b, s.fetched_at)));
            match saved {
                Some((bucket, at)) if fresh(at, now) => {
                    fetched.insert(bucket, at);
                }
                _ => {
                    let _ = fs::remove_file(&path);
                }
            }
        }
        let cache = BucketCache {
            dir: dir.to_path_buf(),
            fetched: Mutex::new(fetched),
        };
        cache.trim(&mut cache.lock());
        cache
    }

    /// The answers kept for `bucket`, while they are fresh at `now`.
    pub fn get(&self, bucket: u32, now: u64) -> Option<Vec<Vec<BucketRecord>>> {
        let mut fetched = self.lock();
        let at = *fetched.get(&bucket)?;
        if fresh(at, now) {
            if let Some(saved) = read(&self.path(bucket)) {
                return Some(saved.answers);
            }
        }
        fetched.remove(&bucket);
        let _ = fs::remove_file(self.path(bucket));
        None
    }

    /// Keeps `answers` as `bucket`'s, fetched at `now`. Nothing is kept
    /// when no node answered.
    pub fn put(&self, bucket: u32, answers: Vec<Vec<BucketRecord>>, now: u64) {
        if answers.is_empty() {
            return;
        }
        let saved = Saved {
            fetched_at: now,
            answers,
        };
        let path = self.path(bucket);
        let staging = path.with_extension("staging");
        let written = serde_json::to_vec(&saved)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(fs::write(&staging, bytes)?))
            .and_then(|()| Ok(fs::rename(&staging, &path)?));
        if let Err(err) = written {
            debug!("keeping bucket {bucket}: {err:#}");
            let _ = fs::remove_file(&staging);
            return;
        }
        let mut fetched = self.lock();
        fetched.insert(bucket, now);
        self.trim(&mut fetched);
    }

    /// Buckets kept.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forgets every bucket.
    pub fn clear(&self) {
        let mut fetched = self.lock();
        for bucket in fetched.drain().map(|(b, _)| b) {
            let _ = fs::remove_file(self.path(bucket));
        }
    }

    /// Drops the oldest buckets beyond [`MAX_BUCKETS`].
    fn trim(&self, fetched: &mut HashMap<u32, u64>) {
        if fetched.len() <= MAX_BUCKETS {
            return;
        }
        let mut by_age: Vec<(u64, u32)> = fetched.iter().map(|(&b, &at)| (at, b)).collect();
        by_age.sort_unstable();
        for &(_, bucket) in &by_age[..fetched.len() - MAX_BUCKETS] {
            fetched.remove(&bucket);
            let _ = fs::remove_file(self.path(bucket));
        }
    }

    fn path(&self, bucket: u32) -> PathBuf {
        self.dir.join(format!("{bucket}.json"))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u32, u64>> {
        self.fetched.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn fresh(fetched_at: u64, now: u64) -> bool {
    fetched_at <= now.saturating_add(60) && now.saturating_sub(fetched_at) < FRESH_FOR
}

fn read(path: &Path) -> Option<Saved> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(record: &str) -> Vec<BucketRecord> {
        vec![BucketRecord {
            record: record.to_string(),
            proof: None,
            also: Vec::new(),
        }]
    }

    #[test]
    fn keeps_a_bucket_across_restarts_until_it_goes_stale() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_800_000_000;
        let cache = BucketCache::open(dir.path(), now);
        assert!(cache.get(7, now).is_none());
        cache.put(7, vec![answer("a"), answer("b")], now);
        cache.put(8, Vec::new(), now);
        assert_eq!(cache.get(7, now + 60).unwrap().len(), 2);
        assert!(cache.get(8, now).is_none());

        let reopened = BucketCache::open(dir.path(), now + 3600);
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened.get(7, now + 3600).unwrap()[1], answer("b"));
        assert!(reopened.get(7, now + FRESH_FOR).is_none());
        assert!(reopened.is_empty());
        assert!(!dir.path().join("7.json").exists());

        cache.put(9, vec![answer("c")], now);
        let stale = BucketCache::open(dir.path(), now + FRESH_FOR + 1);
        assert!(stale.is_empty());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn keeps_at_most_max_buckets_dropping_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_800_000_000;
        let cache = BucketCache::open(dir.path(), now);
        for bucket in 0..=MAX_BUCKETS as u32 {
            cache.put(bucket, vec![answer("x")], now + u64::from(bucket));
        }
        assert_eq!(cache.len(), MAX_BUCKETS);
        assert!(cache.get(0, now + 5000).is_none());
        assert!(cache.get(1, now + 5000).is_some());
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
