//! How often people look at each place on a results page, so that a click
//! counts for what it says about the result and not for where the result
//! was ("Position Bias Estimation for Unbiased Learning to Rank in Personal
//! Search", Wang et al. 2018; "Learning to Rank with Selection Bias in
//! Personal Search", Wang et al. 2016).
//!
//! The first result is opened far more often than the fifth even when the
//! fifth is as good, because more people read that far. A click is taken
//! to need two things: the searcher looked at the result (with a chance
//! that depends only on its place, `examine[k]`), and the result was what
//! they wanted (with a chance that depends only on the search and the
//! site). The same site shown at different places for the same search, as
//! it is when results move with what each browser learned, tells the two
//! apart, and [`estimate`] finds both by expectation-maximization (the
//! paper's EM, without its regression step, which needs features this
//! node does not keep).
//!
//! A click at place `k` then counts `1 / examine[k]` times, the inverse
//! propensity weight, at most [`MAX_WEIGHT`]: a site opened from far down
//! the page says more than one opened from the top. The per-browser
//! learning uses it (see [`crate::history::History::bonus`]).
//!
//! Only browsers that learn from clicks count, and what the node keeps is
//! counts and nothing else: for each search and site a salted fingerprint
//! (not the search, not the site) with the times it was shown and opened at
//! each place, in `DIR/history/positions.json`. No browser's profile is in
//! it, nothing of it leaves the node, and fingerprints not seen for
//! [`KEEP_DAYS`] days are dropped. Until [`MIN_CLICKS`] clicks are counted
//! the node goes by [`PositionBias::prior`].

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Places on the page counted: this node's first results.
pub const PLACES: usize = 10;
/// Most a click counts, however far down the page: a weight this big is
/// mostly noise (Wang et al. clip it too).
pub const MAX_WEIGHT: f32 = 5.0;
/// Clicks on searches seen at two places or more before the estimate is
/// used instead of the prior.
pub const MIN_CLICKS: u32 = 100;
/// Clicks between two estimates.
const REFIT_EVERY: u32 = 25;
/// Fingerprints not seen for this long are dropped.
pub const KEEP_DAYS: u64 = 90;
/// Most fingerprints kept; the ones seen longest ago go first.
const MAX_GROUPS: usize = 5_000;
/// Rounds of EM, at most.
const EM_ROUNDS: usize = 200;
/// The least chance of being looked at an estimate gives a place.
const MIN_EXAMINE: f32 = 0.05;

/// One write at a time.
static WRITING: Mutex<()> = Mutex::new(());

/// How likely each place of the page is to be looked at, the first being 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionBias {
    pub examine: Vec<f32>,
}

impl PositionBias {
    /// What the node goes by before it has counted enough: `1 / (k + 1)`,
    /// the curve unbiased learning to rank is usually tried with.
    pub fn prior() -> Self {
        PositionBias {
            examine: (0..PLACES).map(|k| 1.0 / (k + 1) as f32).collect(),
        }
    }

    /// How much a click at place `k` (0 for the first) counts.
    pub fn weight(&self, k: usize) -> f32 {
        let examine = self
            .examine
            .get(k)
            .or(self.examine.last())
            .copied()
            .unwrap_or(1.0);
        (1.0 / examine.max(f32::MIN_POSITIVE)).clamp(1.0, MAX_WEIGHT)
    }
}

/// Times a search and site were shown and opened at each place.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Counts {
    /// `[shown, opened]` for each place, first place first.
    pub at: Vec<[u32; 2]>,
    /// The unix day it was last shown.
    pub day: u64,
}

/// What `positions.json` holds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Positions {
    /// Mixed into each fingerprint, so the file names no search or site
    /// by itself; made the first time.
    salt: String,
    groups: BTreeMap<u64, Counts>,
    /// Clicks counted since the last estimate.
    since_fit: u32,
    /// The latest estimate, once there are enough clicks.
    fitted: Option<PositionBias>,
}

impl Positions {
    /// The place bias the node goes by now.
    pub fn bias(&self) -> PositionBias {
        self.fitted.clone().unwrap_or_else(PositionBias::prior)
    }

    fn key(&self, query: &str, domain: &str) -> u64 {
        let mut hash = Sha256::new();
        hash.update(self.salt.as_bytes());
        hash.update([0]);
        hash.update(crate::learn::query_key(query).as_bytes());
        hash.update([0]);
        hash.update(domain.as_bytes());
        let digest = hash.finalize();
        u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"))
    }

    fn counts(&mut self, query: &str, domain: &str, day: u64) -> &mut Counts {
        let key = self.key(query, domain);
        let counts = self.groups.entry(key).or_default();
        counts.day = counts.day.max(day);
        if counts.at.len() < PLACES {
            counts.at.resize(PLACES, [0, 0]);
        }
        counts
    }

    /// Notes a results page for `query`: its first sites, best first.
    pub fn note_shown(&mut self, query: &str, sites: &[String], at: u64) {
        if crate::learn::query_key(query).is_empty() {
            return;
        }
        let day = at / 86_400;
        for (k, domain) in sites.iter().take(PLACES).enumerate() {
            let counts = self.counts(query, domain, day);
            counts.at[k][0] = counts.at[k][0].saturating_add(1);
        }
        self.trim(day);
    }

    /// Notes that `domain` was opened from place `k` of a page for `query`
    /// noted with [`Positions::note_shown`], and returns what the click
    /// counts for ([`PositionBias::weight`]).
    pub fn note_opened(&mut self, query: &str, domain: &str, k: usize, at: u64) -> f32 {
        let weight = self.bias().weight(k);
        if k >= PLACES {
            return weight;
        }
        let counts = self.counts(query, domain, at / 86_400);
        // Never more opened than shown, whatever the order of the writes.
        let [shown, opened] = &mut counts.at[k];
        *opened = opened.saturating_add(1);
        *shown = (*shown).max(*opened);
        self.since_fit += 1;
        if self.since_fit >= REFIT_EVERY || self.fitted.is_none() {
            self.since_fit = 0;
            self.fitted = estimate(self.groups.values().map(|c| c.at.as_slice()));
        }
        weight
    }

    fn trim(&mut self, today: u64) {
        self.groups
            .retain(|_, counts| today.saturating_sub(counts.day) <= KEEP_DAYS);
        if self.groups.len() > MAX_GROUPS {
            let mut days: Vec<u64> = self.groups.values().map(|c| c.day).collect();
            days.sort_unstable_by(|a, b| b.cmp(a));
            let cut = days[MAX_GROUPS - 1];
            self.groups.retain(|_, counts| counts.day >= cut);
            // Ties on the cut day may leave a few too many.
            while self.groups.len() > MAX_GROUPS {
                let Some((&key, _)) = self.groups.iter().min_by_key(|(_, c)| c.day) else {
                    break;
                };
                self.groups.remove(&key);
            }
        }
    }
}

/// The place bias in counts of each search and site at each place
/// (`[shown, opened]`, first place first), by EM; `None` until searches
/// seen at two places or more have [`MIN_CLICKS`] clicks.
///
/// Each round, every time a result was shown and not opened is shared out
/// between "not looked at" and "looked at but not wanted" in proportion to
/// how likely each is under the current estimate; then each place's chance
/// of being looked at, and each search and site's chance of being wanted,
/// are counted anew from that.
pub fn estimate<'a>(groups: impl IntoIterator<Item = &'a [[u32; 2]]>) -> Option<PositionBias> {
    // Only a search and site seen at two places tells the place apart from
    // the result; the others would fit any curve.
    let groups: Vec<&[[u32; 2]]> = groups
        .into_iter()
        .filter(|at| at.iter().filter(|[shown, _]| *shown > 0).count() >= 2)
        .collect();
    let clicks: u64 = groups
        .iter()
        .flat_map(|at| at.iter())
        .map(|[_, opened]| u64::from(*opened))
        .sum();
    if clicks < u64::from(MIN_CLICKS) {
        return None;
    }
    let places = groups
        .iter()
        .map(|at| at.len())
        .max()
        .unwrap_or(0)
        .min(PLACES);
    let mut examine: Vec<f64> = PositionBias::prior()
        .examine
        .iter()
        .take(places)
        .map(|&e| f64::from(e))
        .collect();
    let mut wanted: Vec<f64> = vec![0.5; groups.len()];
    for _ in 0..EM_ROUNDS {
        let mut looked = vec![0.0f64; places];
        let mut shown_at = vec![0.0f64; places];
        let mut change = 0.0f64;
        for (g, at) in groups.iter().enumerate() {
            let w = wanted[g];
            let (mut want, mut shown_here) = (0.0f64, 0.0f64);
            for (k, &[shown, opened]) in at.iter().enumerate().take(places) {
                if shown == 0 {
                    continue;
                }
                let (shown, opened) = (f64::from(shown), f64::from(opened.min(shown)));
                let e = examine[k];
                let missed = shown - opened;
                let none = (1.0 - e * w).max(1e-9);
                // Shown and not opened: looked at but not wanted...
                looked[k] += opened + missed * e * (1.0 - w) / none;
                shown_at[k] += shown;
                // ...or wanted but not looked at.
                want += opened + missed * (1.0 - e) * w / none;
                shown_here += shown;
            }
            // A little smoothing, so one click is not "always wanted".
            let next = (want + 0.5) / (shown_here + 1.0);
            change = change.max((next - w).abs());
            wanted[g] = next;
        }
        for k in 0..places {
            if shown_at[k] > 0.0 {
                let next = looked[k] / shown_at[k];
                change = change.max((next - examine[k]).abs());
                examine[k] = next;
            }
        }
        if change < 1e-6 {
            break;
        }
    }
    // Only the ratios mean anything: looked at half as often, or twice as
    // much wanted, give the same clicks.
    let first = examine.first().copied().filter(|e| *e > 0.0)?;
    let mut out: Vec<f32> = examine
        .iter()
        .map(|e| ((e / first) as f32).clamp(MIN_EXAMINE, 1.0))
        .collect();
    out.resize(PLACES, *out.last().unwrap_or(&MIN_EXAMINE));
    Some(PositionBias { examine: out })
}

/// The counts of a node, in its history folder.
#[derive(Debug, Clone)]
pub struct PositionStore {
    path: PathBuf,
}

impl PositionStore {
    /// The counts in `dir` (the node's `history` folder).
    pub fn in_dir(dir: &Path) -> Self {
        PositionStore {
            path: dir.join("positions.json"),
        }
    }

    pub fn load(&self) -> Positions {
        fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Changes the counts with `change` and saves them.
    pub fn update<T>(&self, change: impl FnOnce(&mut Positions) -> T) -> Result<T> {
        let _writing = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
        let mut positions = self.load();
        if positions.salt.is_empty() {
            positions.salt = crate::history::new_profile()?;
        }
        let out = change(&mut positions);
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        crate::node::store::write_atomically(&self.path, &serde_json::to_vec(&positions)?)?;
        Ok(out)
    }
}

#[cfg(test)]
pub(crate) mod sim {
    //! Simulated searchers, for testing what is learned from clicks
    //! without anyone's clicks.

    /// A small, seeded random number generator (xorshift64*).
    pub struct Rng(u64);

    impl Rng {
        pub fn new(seed: u64) -> Self {
            Rng(seed.max(1))
        }

        pub fn next_u64(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        /// Uniform in `[0, 1)`.
        pub fn unit(&mut self) -> f64 {
            (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
        }

        pub fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
    }

    /// How likely a simulated searcher is to look at each place: steeper
    /// than [`super::PositionBias::prior`], so a test can tell which one
    /// was found.
    pub const LOOKS: [f64; 10] = [1.0, 0.7, 0.5, 0.38, 0.3, 0.25, 0.2, 0.17, 0.14, 0.12];

    /// Whether a searcher who wants a result with chance `wanted` opens it
    /// at place `k`.
    pub fn clicks(rng: &mut Rng, k: usize, wanted: f64) -> bool {
        rng.unit() < LOOKS[k.min(LOOKS.len() - 1)] * wanted
    }
}

#[cfg(test)]
mod tests {
    use super::sim::{clicks, Rng, LOOKS};
    use super::*;

    /// Searches with ten sites each, how much each is wanted drawn at
    /// random, shown in an order that is right only on average (as a
    /// ranking that moves with each browser's learning would be).
    fn simulate(rng: &mut Rng, searches: usize, pages: usize) -> Positions {
        let mut positions = Positions {
            salt: "test".into(),
            ..Positions::default()
        };
        for s in 0..searches {
            let query = format!("search {s}");
            let wanted: Vec<f64> = (0..PLACES).map(|_| rng.unit() * 0.8).collect();
            for _ in 0..pages {
                let mut order: Vec<usize> = (0..PLACES).collect();
                // Sorted by how much wanted, plus noise.
                let noise: Vec<f64> = (0..PLACES).map(|_| rng.unit() * 0.6).collect();
                order.sort_by(|&a, &b| (wanted[b] + noise[b]).total_cmp(&(wanted[a] + noise[a])));
                let sites: Vec<String> = order.iter().map(|i| format!("site{i}.example")).collect();
                positions.note_shown(&query, &sites, 0);
                for (k, &i) in order.iter().enumerate() {
                    if clicks(rng, k, wanted[i]) {
                        positions.note_opened(&query, &sites[k], k, 0);
                    }
                }
            }
        }
        positions
    }

    #[test]
    fn em_finds_how_often_each_place_is_looked_at() {
        let mut rng = Rng::new(7);
        let positions = simulate(&mut rng, 300, 20);
        let found =
            estimate(positions.groups.values().map(|c| c.at.as_slice())).expect("enough clicks");
        for (k, (&got, &truth)) in found.examine.iter().zip(LOOKS.iter()).enumerate() {
            assert!(
                (f64::from(got) - truth).abs() < 0.08,
                "place {k}: found {got}, truth {truth} (all: {:?})",
                found.examine
            );
        }
        // And the node goes by it once it has enough.
        assert_eq!(positions.bias().examine.len(), PLACES);
        assert!(positions.bias().examine[5] < 0.4);
    }

    #[test]
    fn a_few_clicks_keep_the_prior() {
        let mut positions = Positions::default();
        let sites: Vec<String> = (0..3).map(|i| format!("s{i}.example")).collect();
        positions.note_shown("q", &sites, 0);
        positions.note_opened("q", "s1.example", 1, 0);
        assert_eq!(positions.bias(), PositionBias::prior());
        assert!(estimate(positions.groups.values().map(|c| c.at.as_slice())).is_none());
    }

    #[test]
    fn clicks_far_down_count_more_but_not_without_bound() {
        let prior = PositionBias::prior();
        assert_eq!(prior.weight(0), 1.0);
        assert!((prior.weight(1) - 2.0).abs() < 1e-6);
        assert_eq!(prior.weight(9), MAX_WEIGHT);
        assert_eq!(prior.weight(50), MAX_WEIGHT);
    }

    /// The point of it: a site that is wanted more but shown lower gets
    /// fewer raw clicks than the one above it, and more weighted ones.
    #[test]
    fn weighted_clicks_put_the_better_site_first_where_raw_clicks_do_not() {
        let mut rng = Rng::new(11);
        let bias = PositionBias {
            examine: LOOKS.iter().map(|&l| l as f32).collect(),
        };
        // `top` is always shown first and wanted 30% of the time;
        // `better`, wanted 50%, always third.
        let (mut top, mut better) = ((0u32, 0.0f32), (0u32, 0.0f32));
        for _ in 0..5_000 {
            if clicks(&mut rng, 0, 0.3) {
                top.0 += 1;
                top.1 += bias.weight(0);
            }
            if clicks(&mut rng, 2, 0.5) {
                better.0 += 1;
                better.1 += bias.weight(2);
            }
        }
        assert!(top.0 > better.0, "raw: {top:?} vs {better:?}");
        assert!(better.1 > top.1, "weighted: {top:?} vs {better:?}");
    }

    #[test]
    fn the_file_names_no_search_and_no_site() {
        let dir = tempfile::tempdir().unwrap();
        let store = PositionStore::in_dir(dir.path());
        let sites = vec!["usbank.com".to_owned(), "chase.com".to_owned()];
        store
            .update(|p| p.note_shown("us bank", &sites, 0))
            .unwrap();
        let weight = store
            .update(|p| p.note_opened("us bank", "chase.com", 1, 0))
            .unwrap();
        assert!((weight - 2.0).abs() < 1e-6);
        let text = fs::read_to_string(dir.path().join("positions.json")).unwrap();
        for secret in ["us bank", "usbank", "chase"] {
            assert!(!text.contains(secret), "{text}");
        }
        let groups = store.load().groups;
        assert_eq!(groups.len(), 2);
        assert!(groups.values().any(|c| c.at[1] == [1, 1]));
    }

    #[test]
    fn old_and_too_many_fingerprints_are_dropped() {
        let mut positions = Positions::default();
        let day = 86_400;
        positions.note_shown("old", &["a.example".to_owned()], 0);
        positions.note_shown("new", &["a.example".to_owned()], (KEEP_DAYS + 1) * day);
        assert_eq!(positions.groups.len(), 1);
        let sites: Vec<String> = (0..PLACES).map(|i| format!("s{i}.example")).collect();
        for q in 0..(MAX_GROUPS / PLACES + 10) {
            positions.note_shown(&format!("q{q}"), &sites, (KEEP_DAYS + 2) * day);
        }
        assert_eq!(positions.groups.len(), MAX_GROUPS);
    }
}
