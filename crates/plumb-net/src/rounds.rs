//! Background bucket requests at fixed deadlines, independent of searches.
//!
//! When rounds are enabled, a native search reads retained local buckets and
//! queues only missing/stale real bucket IDs. It never sends a request, changes
//! the next deadline, or spends a future slot. At each scheduled slot, at most
//! four queued IDs plus random fillers form the existing padded transport round.
//! Busy or failed refreshes return to the queue tail for a later slot.
//!
//! Fixed cadence intentionally replaces the old exponential waits and
//! search-triggered substitution. The scheduler skips slots while an earlier
//! round is still running; it never sends catch-up bursts. Query arrivals and
//! round completion do not reset its deadlines. Disabling rounds retains legacy
//! immediate-fetch behavior and therefore does not provide this timing policy.
//!
//! This does not make searches fully unobservable: answering nodes still learn
//! bucket IDs, response size classes and peer availability vary, and query-driven
//! bucket selection can affect the contents of future scheduled requests.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use rand_core::RngCore;
use serde::{Deserialize, Serialize};

use crate::bucket::BUCKETS;
pub use crate::bucket::BUCKETS_PER_SEARCH;
use crate::cache::BucketCache;

/// Fixed interval between rounds unless changed: 10 minutes, about
/// 150 rounds a day.
pub const ROUND_EVERY: Duration = Duration::from_secs(10 * 60);
/// For the desktop app, which runs on home internet: 30 minutes, about 50
/// rounds a day.
pub const DESKTOP_ROUND_EVERY: Duration = Duration::from_secs(30 * 60);
/// Maximum real bucket IDs waiting or being fetched. No queries are stored.
pub(crate) const MAX_PENDING: usize = 4096;
/// Tries at finding a bucket not fetched lately before taking any.
const FILL_TRIES: usize = 64;

/// The configured fixed background cadence. Searches cannot modify it.
#[derive(Debug)]
pub struct Pace {
    every: Option<Duration>,
}

impl Pace {
    pub fn new(every: Option<Duration>) -> Pace {
        Pace {
            every: every.filter(|e| !e.is_zero()),
        }
    }

    pub fn every(&self) -> Option<Duration> {
        self.every
    }
}

/// Deduplicates waiting and in-flight IDs under the same bounded budget.
#[derive(Debug, Default)]
pub(crate) struct PendingBuckets {
    waiting: VecDeque<u32>,
    known: HashSet<u32>,
    generation: u64,
}

impl PendingBuckets {
    pub(crate) fn queue(&mut self, buckets: impl IntoIterator<Item = u32>) {
        for bucket in buckets {
            if bucket < BUCKETS && self.known.len() < MAX_PENDING && self.known.insert(bucket) {
                self.waiting.push_back(bucket);
            }
        }
    }

    pub(crate) fn take_round(&mut self) -> (u64, Vec<u32>) {
        let count = self.waiting.len().min(BUCKETS_PER_SEARCH);
        (self.generation, self.waiting.drain(..count).collect())
    }

    /// Release in-flight IDs and put failed refreshes behind current waiters.
    pub(crate) fn finish(&mut self, generation: u64, taken: &[u32], retry: Vec<u32>) {
        if generation != self.generation {
            return; // Clear must not resurrect IDs from an older in-flight round.
        }
        for bucket in taken {
            self.known.remove(bucket);
        }
        self.queue(retry);
    }

    pub(crate) fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.waiting.clear();
        self.known.clear();
    }
}

/// What this node's scheduled rounds did, for
/// `GET /api/status`. Searches are not counted apart: this page is public
/// on a public node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundStatus {
    /// Configured fixed seconds between slots; `None` when background rounds are
    /// off.
    pub every_secs: Option<u64>,
    /// Rounds sent since the node started.
    pub sent: u64,
    /// Bucket answers fetched by them. Their sizes are left out: on a
    /// public status page, the record bytes of each round would hint at
    /// which buckets it fetched.
    pub answers: u64,
}

/// `real` (a search's own buckets, at most [`BUCKETS_PER_SEARCH`]) and
/// buckets this node has not fetched lately to make up a round, in random
/// order. Without a cache, the others are just random.
pub fn fill_round(mut real: Vec<u32>, cache: Option<&BucketCache>, now: u64) -> Vec<u32> {
    let mut rng = rand_core::OsRng;
    real.truncate(BUCKETS_PER_SEARCH);
    let mut tries = 0;
    while real.len() < BUCKETS_PER_SEARCH {
        let bucket = (rng.next_u64() % u64::from(BUCKETS)) as u32;
        if real.contains(&bucket) {
            continue;
        }
        tries += 1;
        let fresh = cache.is_some_and(|c| c.is_fresh(bucket, now));
        if fresh && tries < FILL_TRIES {
            continue;
        }
        real.push(bucket);
    }
    crate::search::shuffle(&mut real);
    real
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_ids_are_bounded_deduplicated_and_retries_go_to_tail() {
        let mut pending = PendingBuckets::default();
        pending.queue([1, 2, 2, 3, 4, 5, BUCKETS]);
        assert_eq!(pending.known.len(), 5);
        let (generation, first) = pending.take_round();
        assert_eq!(first, [1, 2, 3, 4]);
        pending.queue([1, 5, 6]); // In-flight IDs remain deduplicated.
        pending.finish(generation, &first, vec![1, 3]);
        assert_eq!(pending.take_round().1, [5, 6, 1, 3]);
        pending.queue(0..BUCKETS);
        assert_eq!(pending.known.len(), MAX_PENDING);
        let before = pending.waiting.len();
        pending.queue(0..BUCKETS);
        assert_eq!(pending.waiting.len(), before);
    }

    #[test]
    fn clearing_queued_ids_prevents_inflight_retry_resurrection() {
        let mut pending = PendingBuckets::default();
        pending.queue([1, 2, 3]);
        let (generation, taken) = pending.take_round();
        pending.clear();
        pending.queue([1, 4]);
        pending.finish(generation, &taken, vec![1, 2, 3]);
        assert_eq!(pending.take_round().1, [1, 4]);
    }

    #[test]
    fn cadence_has_no_search_dependent_state() {
        assert_eq!(Pace::new(None).every(), None);
        assert_eq!(Pace::new(Some(Duration::ZERO)).every(), None);
        assert_eq!(
            Pace::new(Some(Duration::from_secs(60))).every(),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn a_round_always_has_as_many_buckets_as_a_search() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BucketCache::open(dir.path(), 1_000);
        for real in [vec![], vec![7], vec![1, 2, 3, 4], vec![1, 2, 3, 4, 5, 6]] {
            let round = fill_round(real.clone(), Some(&cache), 1_000);
            assert_eq!(round.len(), BUCKETS_PER_SEARCH);
            let mut unique = round.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), BUCKETS_PER_SEARCH);
            for bucket in real.iter().take(BUCKETS_PER_SEARCH) {
                assert!(round.contains(bucket));
            }
            assert!(round.iter().all(|b| *b < BUCKETS));
        }
    }

    #[test]
    fn rounds_fill_with_buckets_not_fetched_lately() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_800_000_000;
        let cache = BucketCache::open(dir.path(), now);
        let record = crate::proto::BucketRecord {
            record: "{}".into(),
            proof: None,
            also: Vec::new(),
        };
        // A quarter of all buckets fetched lately: the rest fill rounds.
        let fetched = BUCKETS / 4;
        for bucket in 0..fetched {
            cache.put(bucket, vec![vec![record.clone()]], now);
        }
        for _ in 0..20 {
            let round = fill_round(vec![1], Some(&cache), now);
            assert!(round.contains(&1));
            assert!(
                round.iter().filter(|&&b| b != 1).all(|&b| b >= fetched),
                "{round:?}"
            );
        }
    }
}
