//! Buckets this node fetched, for its own network searches or in its
//! background rounds (see [`crate::rounds`]), kept so that a search that
//! needs the same buckets is answered here instead of asking the network
//! again.
//!
//! What is kept is whole buckets, never the query or its results: a bucket
//! holds the sites of many unrelated keys, and the buckets that filled out
//! a search's round are kept just like its own, as are those of background
//! rounds. The retained bucket set can still reveal interests, so the cache
//! directory and its files are private to the local user on Unix.
//!
//! Callers validate answers before storing and after retrieving them: this
//! cache does not verify or extend cryptographic proofs. Buckets become stale
//! after [`FRESH_FOR`] seconds but remain available across restarts while a
//! separate background round refreshes them. Both positive and empty answered
//! buckets are retained, subject to count and byte limits, oldest fetch first.
//!
//! ```text
//! <dir>/<bucket>.json   {"fetched_at": .., "answers": [[BucketRecord, ..], ..]}
//! ```

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::proto::BucketRecord;
use crate::storage::{allocation_for, file_bytes, StorageBudget};

/// How long a fetched bucket is used before it is asked for again: as long
/// as the crawls that fill it take to come round (about twice a day).
pub const FRESH_FOR: u64 = 12 * 60 * 60;
/// Most buckets kept, a quarter of them all.
pub const MAX_BUCKETS: usize = 4096;
/// Largest serialized bucket retained; oversized answers leave prior data intact.
pub const MAX_BUCKET_BYTES: usize = 16 * 1024 * 1024;
/// Total serialized data retained, including stale buckets.
pub const MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024;

/// Cached answers with their fetch time and freshness; proof validity is separate.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedBucket {
    pub fetched_at: u64,
    pub answers: Vec<Vec<BucketRecord>>,
    pub stale: bool,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    fetched_at: u64,
    bytes: u64,
}

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
    available: bool,
    fetched: Mutex<HashMap<u32, Entry>>,
    storage: Option<Arc<StorageBudget>>,
}

impl BucketCache {
    /// Opens the cache, retaining stale answers and discarding corrupt files.
    /// If private storage cannot be prepared, the cache stays disabled.
    pub fn open(dir: &Path, _now: u64) -> BucketCache {
        Self::open_with_budget(dir, _now, None)
    }

    pub(crate) fn open_with_budget(
        dir: &Path,
        _now: u64,
        storage: Option<Arc<StorageBudget>>,
    ) -> BucketCache {
        let _mutation = storage.as_ref().map(|budget| budget.mutation());
        let existed = dir.exists();
        let available = match secure_dir(dir) {
            Ok(()) => true,
            Err(err) => {
                debug!("bucket cache {}: {err}", dir.display());
                false
            }
        };
        if available && !existed {
            if let Some(budget) = &storage {
                budget.added(file_bytes(dir).unwrap_or(0));
            }
        }
        let mut fetched = HashMap::new();
        for entry in available
            .then(|| fs::read_dir(dir))
            .into_iter()
            .flatten()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            let bucket = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|&b| {
                    b < plumb_core::keys::BUCKETS
                        && path
                            .file_name()
                            .is_some_and(|name| name == format!("{b}.json").as_str())
                });
            let saved = bucket.and_then(|b| {
                secure_file(&path).ok()?;
                read(&path).map(|(s, bytes)| {
                    (
                        b,
                        Entry {
                            fetched_at: s.fetched_at,
                            bytes: file_bytes(&path).unwrap_or(bytes),
                        },
                    )
                })
            });
            match saved {
                Some((bucket, entry)) => {
                    fetched.insert(bucket, entry);
                }
                _ => {
                    let size = file_bytes(&path).unwrap_or(0);
                    if fs::remove_file(&path).is_ok() {
                        if let Some(budget) = &storage {
                            budget.removed(size);
                        }
                    }
                }
            }
        }
        let cache = BucketCache {
            dir: dir.to_path_buf(),
            available,
            fetched: Mutex::new(fetched),
            storage: storage.clone(),
        };
        cache.trim(&mut cache.lock());
        cache.trim_for_room(&mut cache.lock(), 0);
        cache
    }

    /// The answers kept for `bucket`, while they are fresh at `now`.
    pub fn get(&self, bucket: u32, now: u64) -> Option<Vec<Vec<BucketRecord>>> {
        self.get_retained(bucket, now)
            .filter(|saved| !saved.stale)
            .map(|saved| saved.answers)
    }

    /// Retained answers, including stale ones. Never refreshes or contacts peers.
    /// Callers must revalidate proofs at `now`, even for fresh cached data.
    pub fn get_retained(&self, bucket: u32, now: u64) -> Option<CachedBucket> {
        let mut fetched = self.lock();
        fetched.get(&bucket)?;
        if let Some((saved, bytes)) = read(&self.path(bucket)) {
            fetched.insert(
                bucket,
                Entry {
                    fetched_at: saved.fetched_at,
                    bytes: file_bytes(&self.path(bucket)).unwrap_or(bytes),
                },
            );
            return Some(CachedBucket {
                fetched_at: saved.fetched_at,
                stale: !fresh(saved.fetched_at, now),
                answers: saved.answers,
            });
        }
        let _mutation = self.storage.as_ref().map(|budget| budget.mutation());
        self.remove(&mut fetched, bucket);
        None
    }

    /// Whether `bucket` was fetched lately enough to be used at `now`.
    pub fn is_fresh(&self, bucket: u32, now: u64) -> bool {
        self.lock()
            .get(&bucket)
            .is_some_and(|entry| fresh(entry.fetched_at, now))
    }

    /// Keeps `answers` as `bucket`'s, fetched at `now`. Nothing is kept
    /// when no node answered.
    pub fn put(&self, bucket: u32, answers: Vec<Vec<BucketRecord>>, now: u64) {
        if !self.available || bucket >= plumb_core::keys::BUCKETS || answers.is_empty() {
            return;
        }
        let saved = Saved {
            fetched_at: now,
            answers,
        };
        // Serialize filesystem mutation and index changes, including clear/trim.
        let mut fetched = self.lock();
        let mut encoded = LimitedBytes(Vec::new());
        if serde_json::to_writer(&mut encoded, &saved).is_err() {
            debug!("could not encode a bucket within the cache byte limit");
            return;
        }
        let _mutation = self.storage.as_ref().map(|budget| budget.mutation());
        let needed = allocation_for(encoded.0.len() as u64);
        // Reserve the full new file while the old replacement is still on disk.
        if needed > MAX_CACHE_BYTES
            || self
                .storage
                .as_ref()
                .is_some_and(|budget| needed > budget.status().limit_bytes)
        {
            return;
        }
        self.trim_to(&mut fetched, MAX_CACHE_BYTES.saturating_sub(needed));
        self.trim_for_room(&mut fetched, needed);
        let directory_before = file_bytes(&self.dir).unwrap_or(0);
        let old = file_bytes(&self.path(bucket)).unwrap_or(0);
        let mut reservation = match self
            .storage
            .as_ref()
            .map(|budget| budget.reserve(needed, false))
            .transpose()
        {
            Ok(reserved) => reserved,
            Err(err) => {
                debug!("bucket cache: {err:#}");
                return;
            }
        };
        if let Err(err) = self.write(bucket, &encoded.0, &mut reservation) {
            if let Some(budget) = &self.storage {
                budget.added(
                    file_bytes(&self.dir)
                        .unwrap_or(directory_before + 4096)
                        .saturating_sub(directory_before),
                );
            }
            debug!("could not retain a bucket: {err}");
            return;
        }
        let bytes = file_bytes(&self.path(bucket)).unwrap_or(needed);
        if let Some(reserved) = &mut reservation {
            reserved.commit(
                needed,
                old,
                bytes.saturating_add(
                    file_bytes(&self.dir)
                        .unwrap_or(directory_before + 4096)
                        .saturating_sub(directory_before),
                ),
            );
        }
        fetched.insert(
            bucket,
            Entry {
                fetched_at: now,
                bytes,
            },
        );
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
        let _mutation = self.storage.as_ref().map(|budget| budget.mutation());
        for bucket in fetched.keys().copied().collect::<Vec<_>>() {
            self.remove(&mut fetched, bucket);
        }
    }

    fn trim(&self, fetched: &mut HashMap<u32, Entry>) {
        self.trim_to(fetched, MAX_CACHE_BYTES);
    }

    fn trim_to(&self, fetched: &mut HashMap<u32, Entry>, byte_limit: u64) {
        let mut bytes: u64 = fetched.values().map(|entry| entry.bytes).sum();
        let mut by_age: Vec<_> = fetched
            .iter()
            .map(|(&b, entry)| (entry.fetched_at, b))
            .collect();
        by_age.sort_unstable();
        for (_, bucket) in by_age {
            if fetched.len() <= MAX_BUCKETS && bytes <= byte_limit {
                break;
            }
            if self.remove(fetched, bucket) {
                bytes = fetched.values().map(|entry| entry.bytes).sum();
            }
        }
    }

    fn trim_for_room(&self, fetched: &mut HashMap<u32, Entry>, needed: u64) {
        let Some(budget) = &self.storage else {
            return;
        };
        let mut by_age: Vec<_> = fetched
            .iter()
            .map(|(&b, entry)| (entry.fetched_at, b))
            .collect();
        by_age.sort_unstable();
        for (_, bucket) in by_age {
            let status = budget.status();
            if status
                .used_bytes
                .saturating_add(status.reserved_bytes)
                .saturating_add(needed)
                <= status.limit_bytes
            {
                break;
            }
            self.remove(fetched, bucket);
        }
    }

    fn remove(&self, fetched: &mut HashMap<u32, Entry>, bucket: u32) -> bool {
        match fs::remove_file(self.path(bucket)) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                debug!("cannot remove cached bucket: {err}");
                return false;
            }
        }
        if let Some(entry) = fetched.remove(&bucket) {
            if let Some(budget) = &self.storage {
                budget.removed(entry.bytes);
            }
        }
        true
    }

    fn path(&self, bucket: u32) -> PathBuf {
        self.dir.join(format!("{bucket}.json"))
    }

    fn write(
        &self,
        bucket: u32,
        bytes: &[u8],
        reservation: &mut Option<crate::storage::Reservation>,
    ) -> io::Result<()> {
        let mut random = [0u8; 16];
        OsRng
            .try_fill_bytes(&mut random)
            .map_err(io::Error::other)?;
        let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let staging = self.dir.join(format!(".{name}.staging"));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // Do not remove a preexisting file if create_new rejects a collision.
        let mut file = options.open(&staging)?;
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&staging, self.path(bucket))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&staging);
            let retained = plumb_core::storage::existing_file_bytes(&staging)
                .unwrap_or_else(|_| allocation_for(bytes.len() as u64));
            if let Some(reserved) = reservation {
                reserved.commit(allocation_for(bytes.len() as u64), 0, retained);
            }
        }
        result
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Entry>> {
        self.fetched.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn fresh(fetched_at: u64, now: u64) -> bool {
    fetched_at <= now.saturating_add(60) && now.saturating_sub(fetched_at) < FRESH_FOR
}

fn read(path: &Path) -> Option<(Saved, u64)> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_BUCKET_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_BUCKET_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_BUCKET_BYTES {
        return None;
    }
    let saved: Saved = serde_json::from_slice(&bytes).ok()?;
    if saved.answers.is_empty() {
        return None;
    }
    Some((saved, bytes.len() as u64))
}

fn secure_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;
    if !fs::symlink_metadata(dir)?.is_dir() {
        return Err(io::Error::other("cache path is not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn secure_file(path: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other("cache entry is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

struct LimitedBytes(Vec<u8>);

impl Write for LimitedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_BUCKET_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("bucket exceeds cache byte limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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
    fn cache_admits_staging_allocation_and_trims_at_startup_and_each_put() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let cache = BucketCache::open(&cache_dir, 100);
        for bucket in 0..20 {
            cache.put(bucket, vec![answer("small")], 100 + bucket as u64);
        }
        drop(cache);
        fs::write(dir.path().join("records.jsonl"), b"raw stays").unwrap();
        let budget = StorageBudget::open(dir.path(), 48 * 1024).unwrap();
        let cache = BucketCache::open_with_budget(&cache_dir, 100, Some(budget.clone()));
        assert!(crate::storage::directory_bytes(dir.path()).unwrap() <= 48 * 1024);
        for bucket in 20..60 {
            cache.put(bucket, vec![answer("new")], 100 + bucket as u64);
            assert!(crate::storage::directory_bytes(dir.path()).unwrap() <= 48 * 1024);
            assert_eq!(budget.status().reserved_bytes, 0);
        }
        assert_eq!(
            fs::read(dir.path().join("records.jsonl")).unwrap(),
            b"raw stays"
        );
        assert!(cache.get_retained(59, 200).is_some());
    }

    #[test]
    fn retains_stale_answers_across_restarts_with_freshness_metadata() {
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
        let retained = reopened.get_retained(7, now + 3600).unwrap();
        assert!(!retained.stale);
        assert_eq!(retained.fetched_at, now);
        assert!(reopened.get(7, now + FRESH_FOR).is_none());
        assert!(!reopened.is_fresh(7, now + FRESH_FOR));
        assert!(dir.path().join("7.json").exists());
        drop(reopened);
        drop(cache);
        let stale = BucketCache::open(dir.path(), now + FRESH_FOR + 1);
        assert_eq!(stale.len(), 1);
        let retained = stale.get_retained(7, now + FRESH_FOR + 1).unwrap();
        assert!(retained.stale);
        assert_eq!(retained.answers, vec![answer("a"), answer("b")]);
        stale.put(7, vec![answer("new")], now + FRESH_FOR + 1);
        assert!(stale.is_fresh(7, now + FRESH_FOR + 1));
        assert_eq!(stale.get(7, now + FRESH_FOR + 1), Some(vec![answer("new")]));
    }

    #[test]
    fn retains_empty_answered_buckets_but_not_missing_answers() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_800_000_000;
        let cache = BucketCache::open(dir.path(), now);
        cache.put(7, vec![Vec::new()], now);
        cache.put(8, Vec::new(), now);
        cache.put(7, Vec::new(), now + 60);
        drop(cache);
        let reopened = BucketCache::open(dir.path(), now + FRESH_FOR);
        let saved = reopened.get_retained(7, now + FRESH_FOR).unwrap();
        assert!(saved.stale);
        assert_eq!(saved.fetched_at, now);
        assert_eq!(saved.answers, vec![Vec::<BucketRecord>::new()]);
        assert!(reopened.get_retained(8, now + FRESH_FOR).is_none());
    }

    #[test]
    fn rejects_corrupt_oversized_and_invalid_files_without_following_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_800_000_000;
        let cache = BucketCache::open(dir.path(), now);
        cache.put(7, vec![answer("valid")], now);
        fs::write(dir.path().join("8.json"), b"broken json").unwrap();
        fs::write(
            dir.path().join("09.json"),
            fs::read(dir.path().join("7.json")).unwrap(),
        )
        .unwrap();
        fs::write(dir.path().join("16384.json"), b"{}").unwrap();
        fs::write(
            dir.path().join("10.json"),
            br#"{"fetched_at":0,"answers":[]}"#,
        )
        .unwrap();
        fs::write(dir.path().join(".abandoned.staging"), b"partial").unwrap();
        let oversized = fs::File::create(dir.path().join("11.json")).unwrap();
        oversized.set_len(MAX_BUCKET_BYTES as u64 + 1).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("7.json"), dir.path().join("12.json"))
                .unwrap();
        }
        drop(cache);
        let reopened = BucketCache::open(dir.path(), now);
        assert_eq!(reopened.len(), 1);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        assert_eq!(reopened.get(7, now), Some(vec![answer("valid")]));
        fs::write(dir.path().join("7.json"), b"corrupted after opening").unwrap();
        assert!(reopened.get_retained(7, now).is_none());
        assert!(reopened.is_empty());
        assert!(!dir.path().join("7.json").exists());
    }

    #[test]
    fn oversized_answers_preserve_existing_cache_and_clock_skew_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_800_000_000;
        let cache = BucketCache::open(dir.path(), now);
        cache.put(7, vec![answer("valid")], now);
        cache.put(7, vec![answer(&"x".repeat(MAX_BUCKET_BYTES))], now + 1);
        assert_eq!(cache.get(7, now + 1), Some(vec![answer("valid")]));
        assert!(!cache.get_retained(7, now - 60).unwrap().stale);
        assert!(cache.get_retained(7, now - 61).unwrap().stale);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn byte_budget_evicts_oldest_bucket_and_keeps_files_in_sync() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_800_000_000;
        let cache = BucketCache::open(dir.path(), now);
        cache.put(7, vec![answer("old")], now);
        cache.put(8, vec![answer("new")], now + 1);
        let newest_size = file_bytes(&dir.path().join("8.json")).unwrap();
        cache.trim_to(&mut cache.lock(), newest_size);
        assert_eq!(cache.len(), 1);
        assert!(!dir.path().join("7.json").exists());
        assert!(cache.get_retained(8, now + FRESH_FOR + 1).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn creates_and_tightens_user_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("cache");
        let cache = BucketCache::open(&dir, 1_000);
        cache.put(7, vec![answer("valid")], 1_000);
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(dir.join("7.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(dir.join("7.json"), fs::Permissions::from_mode(0o644)).unwrap();
        drop(cache);
        let reopened = BucketCache::open(&dir, 100_000);
        assert_eq!(reopened.len(), 1);
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(dir.join("7.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let alias = base.path().join("alias");
        std::os::unix::fs::symlink(&dir, &alias).unwrap();
        let disabled = BucketCache::open(&alias, 100_000);
        disabled.put(8, vec![answer("must not write")], 100_000);
        assert!(disabled.is_empty());
        assert!(!dir.join("8.json").exists());
    }

    #[test]
    fn concurrent_writes_and_clear_remain_complete_and_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BucketCache::open(dir.path(), 1_000);
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let cache = &cache;
                scope.spawn(move || {
                    for round in 0..8 {
                        if worker == 0 {
                            cache.clear();
                        } else {
                            cache.put(7, vec![answer(&format!("{worker}:{round}"))], 1_000 + round);
                            cache.get_retained(7, 2_000);
                        }
                    }
                });
            }
        });
        let count = cache.len();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), count);
        drop(cache);
        let reopened = BucketCache::open(dir.path(), 100_000);
        assert_eq!(reopened.len(), count);
        if count == 1 {
            assert!(reopened.get_retained(7, 100_000).is_some());
        }
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
        // Startup enforces the same limit even if extra files were left behind.
        fs::write(
            dir.path().join("0.json"),
            serde_json::to_vec(&Saved {
                fetched_at: now,
                answers: vec![answer("oldest")],
            })
            .unwrap(),
        )
        .unwrap();
        drop(cache);
        let cache = BucketCache::open(dir.path(), now + FRESH_FOR + 5000);
        assert_eq!(cache.len(), MAX_BUCKETS);
        assert!(cache.get_retained(0, now + FRESH_FOR + 5000).is_none());
        assert!(cache.get_retained(1, now + FRESH_FOR + 5000).unwrap().stale);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
