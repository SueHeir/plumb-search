//! Search history, kept on the node for each browser that searches it
//! (`plumb run --search-history`, on by default for the desktop app).
//!
//! Several people can use one node, a homelab's or a family computer's, so
//! each browser gets a profile of its own: a random id in a cookie, and a
//! file `DIR/history/<id>.json` holding that browser's searches and the
//! sites it opened from them. Nobody sees another browser's history, and
//! nothing of it leaves the node.
//!
//! The searcher chooses on the search page's settings whether their past
//! searches are shown (and sites opened before are labelled), and whether
//! the sites they opened before are ranked higher. With both off nothing is
//! noted.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result};
use plumb_core::collapse_whitespace;
use serde::{Deserialize, Serialize};

use crate::learn::Learned;

/// Most past searches kept per profile.
pub const MAX_SEARCHES: usize = 200;
/// Most opened sites kept per profile, one per search and site.
pub const MAX_OPENED: usize = 500;
/// Longest search kept, in characters.
const MAX_QUERY_CHARS: usize = 200;
/// Characters of a profile id: 128 random bits in hex.
const PROFILE_ID_CHARS: usize = 32;

/// Score added to a site opened before for the same search. Most of a
/// result's score, so the site you went to last time comes back first.
pub const OPENED_FOR_QUERY_BONUS: f32 = 0.3;
/// Score added to a site opened before for another search. A search that
/// shares words with it gets up to [`OPENED_FOR_QUERY_BONUS`] times the
/// share of words in common: "us bank login" lifts the site opened for
/// "us bank" more than "pizza" does.
pub const OPENED_BONUS: f32 = 0.05;

/// One write at a time, for every profile: history files are small.
static WRITING: Mutex<()> = Mutex::new(());

/// A past search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PastSearch {
    pub query: String,
    /// Unix time of the latest time it was searched.
    pub at: u64,
}

/// A site opened from the results of a search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opened {
    pub query: String,
    pub domain: String,
    /// Unix time of the latest time it was opened for the query.
    pub at: u64,
    /// How many times it was opened for the query.
    pub times: u32,
}

/// One profile's history, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    pub searches: Vec<PastSearch>,
    pub opened: Vec<Opened>,
    /// What the browser's clicks taught the node.
    pub learned: Learned,
}

impl History {
    /// Notes `query`, moving it to the front if it was searched before.
    pub fn add_search(&mut self, query: &str, at: u64) {
        let Some(query) = clean(query) else {
            return;
        };
        let wanted = key(&query);
        self.searches.retain(|s| key(&s.query) != wanted);
        self.searches.insert(0, PastSearch { query, at });
        self.searches.truncate(MAX_SEARCHES);
    }

    /// Notes that `domain` was opened for `query`.
    pub fn add_opened(&mut self, query: &str, domain: &str, at: u64) {
        let Some(query) = clean(query) else {
            return;
        };
        let wanted = key(&query);
        let times = match self
            .opened
            .iter()
            .position(|o| o.domain == domain && key(&o.query) == wanted)
        {
            Some(i) => self.opened.remove(i).times.saturating_add(1),
            None => 1,
        };
        self.opened.insert(
            0,
            Opened {
                query,
                domain: domain.to_owned(),
                at,
                times,
            },
        );
        self.opened.truncate(MAX_OPENED);
    }

    /// Whether `domain` was opened from any search.
    pub fn was_opened(&self, domain: &str) -> bool {
        self.opened.iter().any(|o| o.domain == domain)
    }

    /// The score a site gets for having been opened before: more for the
    /// same search than for another one, and more for a search sharing
    /// words with this one.
    pub fn bonus(&self, query: &str, domain: &str) -> f32 {
        let wanted = key(query);
        let words = word_set(query);
        let mut bonus = 0.0f32;
        for opened in self.opened.iter().filter(|o| o.domain == domain) {
            let this = if key(&opened.query) == wanted {
                OPENED_FOR_QUERY_BONUS
            } else {
                let theirs = word_set(&opened.query);
                let shared = words.intersection(&theirs).count();
                let all = words.union(&theirs).count();
                let alike = if all == 0 {
                    0.0
                } else {
                    shared as f32 / all as f32
                };
                OPENED_BONUS.max(OPENED_FOR_QUERY_BONUS * alike)
            };
            bonus = bonus.max(this);
        }
        bonus
    }
}

/// `query` as kept: whitespace collapsed and cut short; `None` when empty.
fn clean(query: &str) -> Option<String> {
    let query = collapse_whitespace(query);
    let query: String = query.chars().take(MAX_QUERY_CHARS).collect();
    (!query.trim().is_empty()).then(|| query.trim().to_owned())
}

/// What makes two searches the same: case and spacing do not count.
fn key(query: &str) -> String {
    collapse_whitespace(query).trim().to_lowercase()
}

/// The distinct words of a query, as [`crate::learn::query_key`] spells
/// them.
fn word_set(query: &str) -> std::collections::HashSet<String> {
    crate::learn::query_key(query)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether `id` looks like a profile id this module made.
pub fn valid_profile(id: &str) -> bool {
    id.len() == PROFILE_ID_CHARS && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A new random profile id.
pub fn new_profile() -> Result<String> {
    let mut bytes = [0u8; PROFILE_ID_CHARS / 2];
    getrandom::fill(&mut bytes).map_err(|err| anyhow::anyhow!("no randomness: {err}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The history files of a node, in its `history` folder.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    dir: PathBuf,
}

impl HistoryStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        HistoryStore { dir: dir.into() }
    }

    /// The folder the history files are in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, profile: &str) -> Option<PathBuf> {
        valid_profile(profile).then(|| self.dir.join(format!("{profile}.json")))
    }

    /// The history of `profile`; empty when it has none or cannot be read.
    pub fn load(&self, profile: &str) -> History {
        let Some(path) = self.path(profile) else {
            return History::default();
        };
        read(&path).unwrap_or_default()
    }

    /// Changes the history of `profile` with `change` and saves it.
    pub fn update(&self, profile: &str, change: impl FnOnce(&mut History)) -> Result<()> {
        let Some(path) = self.path(profile) else {
            anyhow::bail!("not a profile id: {profile:?}");
        };
        let _writing = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
        let mut history = read(&path).unwrap_or_default();
        change(&mut history);
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        crate::node::store::write_atomically(&path, &serde_json::to_vec(&history)?)
    }

    /// Deletes the history of `profile`.
    pub fn clear(&self, profile: &str) -> Result<()> {
        let Some(path) = self.path(profile) else {
            return Ok(());
        };
        let _writing = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
        match fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                Err(err).with_context(|| format!("deleting {}", path.display()))
            }
            _ => Ok(()),
        }
    }
}

/// Every site opened from a search in the history files in `dir`.
pub fn all_opened(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut opened = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(profile) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if !valid_profile(profile) {
            continue;
        }
        if let Some(history) = read(&entry.path()) {
            opened.extend(history.opened.into_iter().map(|o| o.domain));
        }
    }
    opened
}

fn read(path: &Path) -> Option<History> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searches_are_kept_newest_first_once_each() {
        let mut history = History::default();
        history.add_search("us bank", 1);
        history.add_search("github", 2);
        history.add_search("  US   Bank ", 3);
        history.add_search("   ", 4);
        let queries: Vec<&str> = history.searches.iter().map(|s| s.query.as_str()).collect();
        assert_eq!(queries, ["US Bank", "github"]);
        assert_eq!(history.searches[0].at, 3);
    }

    #[test]
    fn history_is_capped() {
        let mut history = History::default();
        for i in 0..MAX_SEARCHES + 10 {
            history.add_search(&format!("q{i}"), i as u64);
            history.add_opened(&format!("q{i}"), "example.com", i as u64);
        }
        assert_eq!(history.searches.len(), MAX_SEARCHES);
        assert_eq!(history.searches[0].query, format!("q{}", MAX_SEARCHES + 9));
        assert!(history.opened.len() <= MAX_OPENED);
    }

    #[test]
    fn opened_sites_get_more_for_the_same_search() {
        let mut history = History::default();
        history.add_opened("bank", "usbank.com", 1);
        history.add_opened("Bank", "usbank.com", 2);
        history.add_opened("chase", "chase.com", 3);
        assert_eq!(history.opened.len(), 2);
        assert_eq!(history.opened[1].times, 2);
        assert_eq!(history.bonus("bank", "usbank.com"), OPENED_FOR_QUERY_BONUS);
        assert_eq!(history.bonus("bank", "chase.com"), OPENED_BONUS);
        assert_eq!(history.bonus("bank", "wellsfargo.com"), 0.0);
        assert!(history.was_opened("chase.com"));
        assert!(!history.was_opened("wellsfargo.com"));
        // A search sharing words lifts it more than another search does.
        history.add_opened("us bank", "usbank.com", 4);
        let alike = history.bonus("us bank login", "usbank.com");
        assert!(
            alike > OPENED_BONUS && alike < OPENED_FOR_QUERY_BONUS,
            "{alike}"
        );
    }

    #[test]
    fn profiles_are_kept_apart_and_can_be_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path().join("history"));
        let (a, b) = (new_profile().unwrap(), new_profile().unwrap());
        assert!(valid_profile(&a) && a != b);
        store.update(&a, |h| h.add_search("github", 1)).unwrap();
        store.update(&b, |h| h.add_search("chase", 1)).unwrap();
        assert_eq!(store.load(&a).searches[0].query, "github");
        assert_eq!(store.load(&b).searches[0].query, "chase");
        store.clear(&a).unwrap();
        assert!(store.load(&a).searches.is_empty());
        assert_eq!(store.load(&b).searches.len(), 1);
        store.clear(&a).unwrap();
    }

    #[test]
    fn profile_ids_cannot_name_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        for bad in [
            "../settings",
            "",
            "ABCDEF0123456789ABCDEF0123456789",
            "x".repeat(32).as_str(),
        ] {
            assert!(!valid_profile(bad));
            assert!(store.update(bad, |h| h.add_search("x", 1)).is_err());
            assert!(store.load(bad).searches.is_empty());
        }
    }
}
