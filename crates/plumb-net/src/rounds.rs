//! One steady stream of bucket requests, so that a node's searches look
//! the same on the network as the rest of its traffic (Liz, 2026-10-04:
//! "no one should know the difference between search and updates").
//!
//! A network search fetches [`BUCKETS_PER_SEARCH`] buckets, each from
//! [`crate::search::NODES_PER_BUCKET`] nodes, under throwaway identities,
//! sealed through relays (see [`crate::search`]). On its own, that burst of
//! requests would show whoever watches a node's traffic (its relays, its
//! internet provider) the moment it searches, even though nobody can see
//! what for. So every node also fetches **rounds** of buckets in the
//! background, at random times, [`NetConfig::round_every`] apart on
//! average, built exactly like a search's: the same number of buckets, the
//! same number of nodes per bucket, the same throwaway identities and
//! relays, the same sizes, the same retry with a token when a node is busy.
//!
//! * **A search is a round.** Its own buckets go in, and the rest of the
//!   round is filled with buckets this node has not fetched lately, as a
//!   background round would be. A search does not wait for the next round:
//!   it goes at once and the next background round is skipped in its place
//!   ([`Pace::searched`]), so a node sends the same number of rounds an hour
//!   whether it searches or not (up to [`MAX_OWED`] searches ahead).
//! * **A background round is useful.** Its answers are checked like a
//!   search's and kept in the bucket cache ([`crate::cache`]), so a later
//!   search that needs those buckets is answered on the node, with no
//!   request at all.
//! * Nodes answering, and relays passing requests on, see the same thing
//!   either way: a bucket number from a throwaway identity, or a sealed
//!   request of one size.
//!
//! What still differs: a node that searches far more often than its rounds
//! come (more than [`MAX_OWED`] searches within that many rounds) sends
//! more rounds than usual for a while, which shows it is busier, never
//! which rounds are its searches. Crawl batches (`/plumb/batch/1` and the
//! gossip topic) are not part of this: every node fetches every batch,
//! whatever it searches, so they say nothing about searches.
//!
//! [`NetConfig::round_every`]: crate::NetConfig::round_every

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rand_core::RngCore;
use serde::{Deserialize, Serialize};

use crate::bucket::BUCKETS;
pub use crate::bucket::BUCKETS_PER_SEARCH;
use crate::cache::BucketCache;

/// Average time between two rounds unless changed: 10 minutes, about
/// 150 rounds a day.
pub const ROUND_EVERY: Duration = Duration::from_secs(10 * 60);
/// For the desktop app, which runs on home internet: 30 minutes, about 50
/// rounds a day.
pub const DESKTOP_ROUND_EVERY: Duration = Duration::from_secs(30 * 60);
/// Searches that may each take the place of a later background round.
/// Beyond that, a search still goes at once, as an extra round.
pub const MAX_OWED: u32 = 6;
/// Tries at finding a bucket not fetched lately before taking any.
const FILL_TRIES: usize = 64;

/// When rounds go, and the background rounds owed to searches.
#[derive(Debug)]
pub struct Pace {
    every: Option<Duration>,
    owed: AtomicU32,
}

impl Pace {
    /// Rounds `every` apart on average; `None` sends no background rounds.
    pub fn new(every: Option<Duration>) -> Pace {
        Pace {
            every: every.filter(|e| !e.is_zero()),
            owed: AtomicU32::new(0),
        }
    }

    pub fn every(&self) -> Option<Duration> {
        self.every
    }

    /// A search just sent a round: the next background round is skipped
    /// in its place.
    pub fn searched(&self) {
        if self.every.is_some() {
            let _ = update(&self.owed, |n| (n < MAX_OWED).then_some(n + 1));
        }
    }

    /// Whether the round due now was already sent by a search, and so is
    /// skipped.
    pub fn skip(&self) -> bool {
        update(&self.owed, |n| n.checked_sub(1)).is_some()
    }

    /// How long until the next round: random, `every` on average, as the
    /// times between events that happen at random (exponential), so that
    /// a search, which comes whenever someone types, could have been any
    /// round. Capped at 8 times `every`.
    pub fn next_wait(&self) -> Option<Duration> {
        let every = self.every?;
        // In (0, 1], so the logarithm is finite.
        let u = ((rand_core::OsRng.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64;
        Some(every.mul_f64((-u.ln()).min(8.0)))
    }
}

/// What this node's rounds did, its searches' included, for
/// `GET /api/status`. Searches are not counted apart: this page is public
/// on a public node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundStatus {
    /// Average seconds between rounds; `None` when background rounds are
    /// off.
    pub every_secs: Option<u64>,
    /// Rounds sent since the node started.
    pub sent: u64,
    /// Bucket answers fetched by them, and the size of their records in
    /// bytes (before padding, which adds up to half).
    pub answers: u64,
    pub bytes_fetched: u64,
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

/// Sets `value` to what `f` makes of it, unless `f` says `None`; returns
/// the old value when it changed. `fetch_update` written out, as Rust
/// renamed it (`try_update`) in a release older toolchains don't have.
fn update(value: &AtomicU32, f: impl Fn(u32) -> Option<u32>) -> Option<u32> {
    let mut old = value.load(Ordering::Relaxed);
    loop {
        let new = f(old)?;
        match value.compare_exchange_weak(old, new, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(old) => return Some(old),
            Err(now) => old = now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_takes_the_place_of_a_later_round() {
        let pace = Pace::new(Some(Duration::from_secs(60)));
        assert!(!pace.skip());
        pace.searched();
        pace.searched();
        assert!(pace.skip());
        assert!(pace.skip());
        assert!(!pace.skip());
        // Only so many ahead.
        for _ in 0..MAX_OWED + 5 {
            pace.searched();
        }
        let skipped = std::iter::from_fn(|| pace.skip().then_some(())).count();
        assert_eq!(skipped, MAX_OWED as usize);
    }

    #[test]
    fn no_rounds_when_off() {
        let pace = Pace::new(None);
        pace.searched();
        assert!(!pace.skip());
        assert_eq!(pace.next_wait(), None);
        assert_eq!(Pace::new(Some(Duration::ZERO)).every(), None);
    }

    #[test]
    fn waits_average_out_to_the_pace() {
        let pace = Pace::new(Some(Duration::from_secs(100)));
        let n = 20_000;
        let total: f64 = (0..n)
            .map(|_| pace.next_wait().unwrap().as_secs_f64())
            .sum();
        let mean = total / f64::from(n);
        assert!((95.0..105.0).contains(&mean), "{mean}");
        assert!((0..1000).all(|_| pace.next_wait().unwrap() <= Duration::from_secs(800)));
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
