//! The node's data directory: its paths, the lock that keeps a second node
//! out, the saved state, and the numbered index directories.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::NodeSettings;

/// Held locked while a node runs.
const LOCK_FILE: &str = "node.lock";
/// Progress that survives restarts ([`SavedState`]).
const STATE_FILE: &str = "state.json";
/// What the user chose ([`NodeSettings`]).
const SETTINGS_FILE: &str = "settings.json";
/// Every known site, one JSON line each.
const RECORDS_FILE: &str = "records.jsonl";
/// First-start downloads.
const SEED_DIR: &str = "seed";
/// One numbered directory per index build.
const INDEXES_DIR: &str = "indexes";
/// The network side's files (see [`super::network`]).
const NET_DIR: &str = "net";
/// Site icons for results pages (see [`crate::icons`]).
const ICONS_DIR: &str = "icons";
/// Recent headlines and watched feeds (see [`crate::news`]).
const NEWS_DIR: &str = "news";

/// The files and directories of a data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Paths {
    pub(super) data: PathBuf,
    pub(super) records: PathBuf,
    pub(super) state: PathBuf,
    pub(super) settings: PathBuf,
    pub(super) seed: PathBuf,
    pub(super) indexes: PathBuf,
    pub(super) net: PathBuf,
    pub(super) icons: PathBuf,
    /// Recent headlines and watched feeds (see [`crate::news`]).
    pub(super) news: PathBuf,
    /// Records from other nodes, not yet folded in.
    pub(super) inbox: PathBuf,
    /// The inbox while it is being folded in.
    pub(super) absorbing: PathBuf,
}

impl Paths {
    pub(super) fn new(data: &Path) -> Self {
        Paths {
            data: data.to_path_buf(),
            records: data.join(RECORDS_FILE),
            state: data.join(STATE_FILE),
            settings: data.join(SETTINGS_FILE),
            seed: data.join(SEED_DIR),
            indexes: data.join(INDEXES_DIR),
            net: data.join(NET_DIR),
            icons: data.join(ICONS_DIR),
            news: data.join(NEWS_DIR),
            inbox: data.join(NET_DIR).join("inbox.jsonl"),
            absorbing: data.join(NET_DIR).join("inbox.absorbing"),
        }
    }

    /// `indexes/000007` for id 7.
    pub(super) fn index(&self, id: u64) -> PathBuf {
        self.indexes.join(index_name(id))
    }
}

/// The directory name of an index: `7` -> `000007`. The padding keeps a
/// plain listing in build order; ids are compared as numbers.
pub(super) fn index_name(id: u64) -> String {
    format!("{id:06}")
}

/// The id in an index directory name: `000007` -> 7. Hidden names (staging
/// directories) and anything else that is not all digits give `None`.
pub(super) fn index_id(name: &str) -> Option<u64> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

/// Keeps `DIR/node.lock` locked until dropped, so that two nodes never work
/// on one data directory. The lock belongs to the open file, so the system
/// releases it when the process ends, however it ends.
#[derive(Debug)]
pub(super) struct DirLock {
    _file: File,
}

/// Locks the data directory. Fails when another node holds it; where the
/// file system cannot lock files at all, warns and carries on unlocked.
pub(super) fn lock(paths: &Paths) -> Result<Option<DirLock>> {
    let path = paths.data.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(DirLock { _file: file })),
        Err(TryLockError::WouldBlock) => bail!(
            "{} is in use by another Plumb node; stop that node first, \
             or give this one a data directory of its own",
            paths.data.display()
        ),
        Err(TryLockError::Error(err)) => {
            warn!(
                "cannot lock {} ({err}); make sure no other Plumb node uses {}",
                path.display(),
                paths.data.display()
            );
            Ok(None)
        }
    }
}

/// Removes what interrupted work left behind, best effort: hidden staging
/// directories in `indexes/`, temporary records, state and vectors files,
/// and partial downloads of the seed data and the model. Only call it while holding the [`DirLock`].
pub(super) fn remove_leftovers(paths: &Paths) {
    let temp_prefixes = [
        format!(".{RECORDS_FILE}."),
        format!(".{STATE_FILE}."),
        format!(".{SETTINGS_FILE}."),
    ];
    for name in file_names(&paths.data) {
        let partial_vectors = name == format!("{}.part", plumb_embed::VECTORS_FILE_NAME);
        if partial_vectors
            || name.ends_with(".tmp") && temp_prefixes.iter().any(|p| name.starts_with(p))
        {
            remove_leftover(&paths.data.join(name));
        }
    }
    for dir in [&paths.seed, &paths.data.join(super::embedding::MODEL_DIR)] {
        for name in file_names(dir) {
            if name.ends_with(".part") {
                remove_leftover(&dir.join(name));
            }
        }
    }
    for name in file_names(&paths.indexes) {
        if name.starts_with('.') {
            remove_leftover(&paths.indexes.join(name));
        }
    }
}

fn remove_leftover(path: &Path) {
    let removed = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(err) => Err(err),
    };
    match removed {
        Ok(()) => info!("removed {}, left over from an earlier run", path.display()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => warn!("cannot remove {}: {err}", path.display()),
    }
}

/// The names in `dir` that are valid Unicode; nothing when it cannot be read.
fn file_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect()
}

/// The ids of the numbered directories in `indexes/`, newest (highest) first.
pub(super) fn index_ids(paths: &Paths) -> Vec<u64> {
    let mut ids: Vec<u64> = file_names(&paths.indexes)
        .iter()
        .filter_map(|name| index_id(name))
        .filter(|&id| paths.index(id).is_dir())
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// The id for the next build: above every numbered name in `indexes/`, so a
/// new index is always the newest and never lands on an old directory.
pub(super) fn next_index_id(paths: &Paths) -> u64 {
    file_names(&paths.indexes)
        .iter()
        .filter_map(|name| index_id(name))
        .max()
        .map_or(1, |id| id + 1)
}

/// Deletes an index directory that nothing has open. `meta.json` goes first,
/// so a directory that is only partly deleted (Windows refuses to delete a
/// file another program has open) never passes for a complete index.
pub(super) fn remove_index(dir: &Path) -> io::Result<()> {
    match fs::remove_file(dir.join("meta.json")) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    match fs::remove_dir_all(dir) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Progress that survives restarts, kept in `DIR/state.json`. Missing fields
/// read as their defaults, so older files keep working.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct SavedState {
    /// Homepages still to crawl in the round under way: the initial crawl or
    /// a refresh. 0 when no round is under way.
    pub(super) crawl_left: usize,
    /// The records file has changes that no index holds yet.
    pub(super) index_stale: bool,
    /// When the last round of crawling ended, in Unix seconds.
    pub(super) last_refresh: Option<u64>,
    /// Setup went ahead without Wikidata's official websites, which could
    /// not be downloaded; the node adds them once it can.
    pub(super) wikidata_missing: bool,
    /// Records from other nodes folded into the records file since the
    /// index was last built.
    pub(super) network_pending: u64,
    /// The records are a quick start from the Tranco list alone, put in
    /// service while the rest of the seed data downloads. The full seed
    /// replaces them, keeping only what crawls added.
    pub(super) quick_start: bool,
    /// The day (UTC, in days since 1970) of `downloaded_on_day`.
    pub(super) download_day: u64,
    /// Bytes downloaded on `download_day`.
    pub(super) downloaded_on_day: u64,
    /// Bytes downloaded since setup.
    pub(super) downloaded_total: u64,
    /// Homepages visited since setup.
    pub(super) homepages_visited: u64,
}

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

impl SavedState {
    /// Bytes downloaded on the day of `now` (Unix seconds).
    pub(super) fn downloaded_today(&self, now: u64) -> u64 {
        if self.download_day == now / SECONDS_PER_DAY {
            self.downloaded_on_day
        } else {
            0
        }
    }

    pub(super) fn add_downloaded(&mut self, bytes: u64, now: u64) {
        let today = now / SECONDS_PER_DAY;
        if self.download_day != today {
            self.download_day = today;
            self.downloaded_on_day = 0;
        }
        self.downloaded_on_day = self.downloaded_on_day.saturating_add(bytes);
        self.downloaded_total = self.downloaded_total.saturating_add(bytes);
    }
}

/// The Unix time the day of `now` (UTC) ends.
pub(super) fn next_day(now: u64) -> u64 {
    (now / SECONDS_PER_DAY + 1) * SECONDS_PER_DAY
}

impl SavedState {
    /// The state for a records file the node knows nothing about, fresh from
    /// setup or put there by hand: it needs an index and the initial crawl.
    pub(super) fn fresh(initial_crawl: usize) -> Self {
        SavedState {
            crawl_left: initial_crawl,
            index_stale: true,
            last_refresh: None,
            wikidata_missing: false,
            network_pending: 0,
            quick_start: false,
            download_day: 0,
            downloaded_on_day: 0,
            downloaded_total: 0,
            homepages_visited: 0,
        }
    }
}

/// Reads the saved state. `None` when there is none, or when it cannot be
/// read (that is logged; the node then starts over from what is on disk).
pub(super) fn load_state(paths: &Paths) -> Option<SavedState> {
    load_json(&paths.state)
}

/// Reads the settings. `None` when there are none, or when they cannot be
/// read (that is logged).
pub(super) fn load_settings(paths: &Paths) -> Option<NodeSettings> {
    load_json(&paths.settings)
}

/// The bytes the files under `dir` take, counting what can be read.
pub(super) fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_size(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |meta| meta.len()),
            _ => 0,
        })
        .sum()
}

/// Saves the settings atomically.
pub(super) fn save_settings(paths: &Paths, settings: &NodeSettings) -> Result<()> {
    let mut json = serde_json::to_vec_pretty(settings).context("encoding the settings")?;
    json.push(b'\n');
    write_atomically(&paths.settings, &json)
}

/// Reads a JSON file. `None` when there is none, or when it cannot be read
/// (that is logged).
fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!("cannot read {}: {err}", path.display());
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(state) => Some(state),
        Err(err) => {
            warn!("ignoring {}, which is not valid: {err}", path.display());
            None
        }
    }
}

/// Saves the state atomically.
pub(super) fn save_state(paths: &Paths, state: &SavedState) -> Result<()> {
    let mut json = serde_json::to_vec_pretty(state).context("encoding the node state")?;
    json.push(b'\n');
    write_atomically(&paths.state, &json)
}

/// Writes `bytes` to a temporary file next to `path`, flushes it to disk,
/// renames it over `path` and flushes the directory (on Unix): readers see
/// the old file or the new one, never a mix, even after a power cut.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = crate::temp_path_for(path);
    let written = File::create(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(err) = written {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("writing {}", tmp.display()));
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("moving {} to {}", tmp.display(), path.display()));
    }
    crate::sync_parent_dir(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut names = file_names(dir);
        names.sort();
        names
    }

    #[test]
    fn index_names_and_ids() {
        assert_eq!(index_name(7), "000007");
        assert_eq!(index_name(1_234_567), "1234567");
        assert_eq!(index_id("000007"), Some(7));
        assert_eq!(index_id("1234567"), Some(1_234_567));
        for name in ["", ".000007.new-12-0", "7a", "-7", "+7", "000007 ", "notes"] {
            assert_eq!(index_id(name), None, "{name:?}");
        }
    }

    #[test]
    fn lists_numbered_index_directories_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        assert_eq!(index_ids(&paths), Vec::<u64>::new());
        assert_eq!(next_index_id(&paths), 1);
        for name in ["000002", "000010", "000009", ".000011.new-1-0", "notes"] {
            fs::create_dir_all(paths.indexes.join(name)).unwrap();
        }
        // A file with a number for a name is not an index, but its number is taken.
        fs::write(paths.indexes.join("000012"), "").unwrap();
        assert_eq!(index_ids(&paths), [10, 9, 2]);
        assert_eq!(next_index_id(&paths), 13);
    }

    #[test]
    fn removes_leftovers_only() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        fs::create_dir_all(paths.indexes.join(".000003.new-99-0")).unwrap();
        fs::write(paths.indexes.join(".000003.new-99-0/meta.json"), "{}").unwrap();
        fs::create_dir_all(paths.indexes.join("000002")).unwrap();
        fs::create_dir_all(&paths.seed).unwrap();
        fs::create_dir_all(dir.path().join("model")).unwrap();
        for file in [
            "seed/tranco-top-1m.csv.zip",
            "seed/wikidata-official-sites.tsv.part",
            "vectors.bin.part",
            "model/model.safetensors.part",
            "model/config.json",
            "records.jsonl",
            ".records.jsonl.99.tmp",
            "state.json",
            ".state.json.99.tmp",
            ".hidden-by-the-user",
        ] {
            fs::write(dir.path().join(file), "x").unwrap();
        }
        remove_leftovers(&paths);
        assert_eq!(names(&paths.indexes), ["000002"]);
        assert_eq!(names(&paths.seed), ["tranco-top-1m.csv.zip"]);
        assert_eq!(names(&dir.path().join("model")), ["config.json"]);
        assert_eq!(
            names(dir.path()),
            [
                ".hidden-by-the-user",
                "indexes",
                "model",
                "records.jsonl",
                "seed",
                "state.json"
            ]
        );
        // A data directory with nothing in it yet is fine too.
        remove_leftovers(&Paths::new(&dir.path().join("missing")));
    }

    #[test]
    fn removes_an_index_meta_file_first() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("000001");
        fs::create_dir_all(index.join("sub")).unwrap();
        fs::write(index.join("meta.json"), "{}").unwrap();
        fs::write(index.join("sub/segment"), "x").unwrap();
        remove_index(&index).unwrap();
        assert!(!index.exists());
        // Already gone is fine.
        remove_index(&index).unwrap();
    }

    #[test]
    fn state_round_trips_and_tolerates_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        assert_eq!(load_state(&paths), None);
        let state = SavedState {
            crawl_left: 1_500,
            index_stale: true,
            last_refresh: Some(1_700_000_000),
            wikidata_missing: true,
            network_pending: 0,
            quick_start: true,
            download_day: 20_000,
            downloaded_on_day: 1_234,
            downloaded_total: 5_678,
            homepages_visited: 90,
        };
        save_state(&paths, &state).unwrap();
        assert_eq!(load_state(&paths), Some(state));
        assert_eq!(names(dir.path()), ["state.json"]);

        fs::write(&paths.state, "{\"crawl_left\": 3, \"future_field\": 1}").unwrap();
        assert_eq!(
            load_state(&paths),
            Some(SavedState {
                crawl_left: 3,
                ..SavedState::default()
            })
        );
        fs::write(&paths.state, "not json").unwrap();
        assert_eq!(load_state(&paths), None);
    }

    #[test]
    fn one_lock_per_directory() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let held = lock(&paths).unwrap();
        assert!(held.is_some());
        let err = lock(&paths).unwrap_err().to_string();
        assert!(err.contains("in use by another Plumb node"), "{err}");
        drop(held);
        assert!(lock(&paths).unwrap().is_some());
    }
}
