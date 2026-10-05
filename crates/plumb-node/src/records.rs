//! Records files that crawls keep up to date: the whole file, rewritten now
//! and then, plus a journal of the changes made since.
//!
//! Once a records file holds a million sites, rewriting it takes seconds and
//! writes hundreds of megabytes, far too much to do after every batch of
//! homepages. So a crawl appends each batch's [`Change`]s to a journal next
//! to the file (`records.jsonl` -> `records.jsonl.journal`), flushed to disk
//! before the crawl goes on, and folds the journal into the file
//! ([`RecordStore::compact`]) only once the journal has grown to a quarter of
//! the file's size (and at least [`MIN_COMPACT_BYTES`]), and when `plumb
//! crawl` ends. [`load_records`] replays a journal it finds, so whatever an
//! interrupted crawl saved is kept.
//!
//! The file is always replaced whole and atomically, by way of a temporary
//! file flushed to disk before the rename (see
//! [`crate::write_records_atomically`]), and its journal is deleted only once
//! the new file is on disk. Replaying a journal into a file that already
//! holds its changes leaves the file as it is, so a crash between the two
//! steps loses nothing either.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use plumb_core::{RecordSet, SiteRecord};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::{sync_parent_dir, write_records_atomically};

/// A journal smaller than this is never folded into its file, however
/// small the file.
pub(crate) const MIN_COMPACT_BYTES: u64 = 64 << 20;

/// One change a crawl makes to a set of records, as saved in a journal (one
/// JSON object per line, tagged by `op`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
// Most changes are merges, so boxing the record would only add allocations.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Change {
    /// Merges a record into the set, as [`RecordSet::upsert`] does.
    Merge { record: SiteRecord },
    /// Merges a crawl another node shared, as [`RecordSet::upsert_shared`]
    /// does: what it leaves out (search box, page text) is kept, not cleared.
    MergeShared { record: SiteRecord },
    /// Merges a crawl another node shared into a site the set already
    /// holds; a site it does not hold is left out. For a node with no room
    /// for more sites (see `crate::node::trim`).
    RefreshShared { record: SiteRecord },
    /// Sets when a site's homepage was last tried and how many tries in a
    /// row failed to reach it ([`SiteRecord::crawl_failures`]).
    Mark {
        domain: String,
        attempted_at: Option<u64>,
        failures: u32,
    },
}

impl Change {
    /// Makes the change to `set`. A mark for a domain not in the set is
    /// ignored.
    pub(crate) fn apply(self, set: &mut RecordSet) {
        match self {
            Change::Merge { record } => {
                set.upsert(record);
            }
            Change::MergeShared { record } => {
                set.upsert_shared(record);
            }
            Change::RefreshShared { record } => {
                let held = plumb_core::canonical_domain(&record.domain)
                    .is_some_and(|domain| set.get(&domain).is_some());
                if held {
                    set.upsert_shared(record);
                }
            }
            Change::Mark {
                domain,
                attempted_at,
                failures,
            } => {
                if set.get(&domain).is_some() {
                    let record = set.entry(&domain);
                    record.crawl_attempted_at = attempted_at;
                    record.crawl_failures = failures;
                }
            }
        }
    }
}

/// `records.jsonl` -> `records.jsonl.journal`.
pub(crate) fn journal_path(records: &Path) -> PathBuf {
    let mut name = records
        .file_name()
        .map_or_else(|| "records".into(), |name| name.to_os_string());
    name.push(".journal");
    records.with_file_name(name)
}

/// Reads a records file into a set, then replays the journal an interrupted
/// crawl may have left next to it. Records for the same domain are merged,
/// and ones without a valid domain are dropped.
pub(crate) fn load_records(path: &Path) -> Result<RecordSet> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut set = RecordSet::new();
    let (mut read, mut dropped, mut line_no) = (0usize, 0usize, 0usize);
    let mut bot_checks = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        line_no += 1;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let record: SiteRecord = serde_json::from_str(text)
            .with_context(|| format!("{}:{line_no}: invalid JSON line", path.display()))?;
        read += 1;
        bot_checks += usize::from(record.is_bot_check());
        if !set.upsert(record) {
            dropped += 1;
        }
    }
    let merged = read - dropped - set.len();
    if merged > 0 {
        info!(
            "merged {merged} duplicate records for the same domain in {}",
            path.display()
        );
    }
    if bot_checks > 0 {
        info!(
            "left out the page text of {bot_checks} records of {} read off bot checks \
             rather than homepages",
            path.display()
        );
    }
    if dropped > 0 {
        warn!(
            "left out {dropped} records of {} whose domain is not a valid registrable domain",
            path.display()
        );
    }
    replay_journal(&journal_path(path), &mut set)?;
    Ok(set)
}

/// Applies the changes saved in the journal at `path` to `set`. A missing
/// journal is fine, and damaged lines are skipped: a crash can cut the last
/// one short.
fn replay_journal(path: &Path, set: &mut RecordSet) -> Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("opening {}", path.display())),
    };
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let (mut applied, mut damaged, mut line_no) = (0usize, 0usize, 0usize);
    let mut first_damaged = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        line_no += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Change>(&line) {
            Ok(change) => {
                change.apply(set);
                applied += 1;
            }
            Err(err) => {
                damaged += 1;
                first_damaged.get_or_insert((line_no, err));
            }
        }
    }
    if let Some((line_no, err)) = first_damaged {
        warn!(
            "{}: skipped {damaged} damaged lines, the first at line {line_no} ({err})",
            path.display()
        );
    }
    // The journal is replayed on every read until it is folded in, so this
    // is routine, not a sign of an interrupted crawl.
    debug!(
        "replayed {applied} changes from {} not yet folded into the records file",
        path.display()
    );
    Ok(())
}

/// Saves the changes a crawl makes to a records file: appends them to the
/// file's journal, and rewrites the file once the journal has grown enough.
#[derive(Debug)]
pub(crate) struct RecordStore {
    path: PathBuf,
    journal_path: PathBuf,
    /// Opened for appending when the first change is saved.
    journal: Option<File>,
    journal_bytes: u64,
    /// The size of the file when opened or last rewritten.
    file_bytes: u64,
    /// The journal is folded in once it reaches this size, or a quarter of
    /// the file when that is more.
    min_compact_bytes: u64,
}

impl RecordStore {
    /// A store for the records file at `path`. A journal already there must
    /// have been replayed into the set the crawl changes, as
    /// [`load_records`] does: new changes are added to it.
    pub(crate) fn open(path: &Path) -> Self {
        let journal_path = journal_path(path);
        RecordStore {
            path: path.to_path_buf(),
            journal_bytes: file_len(&journal_path),
            file_bytes: file_len(path),
            journal_path,
            journal: None,
            min_compact_bytes: MIN_COMPACT_BYTES,
        }
    }

    /// The records file.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `changes` to the journal and flushes them to disk.
    pub(crate) fn save(&mut self, changes: &[Change]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let mut lines = Vec::with_capacity(changes.len() * 160);
        for change in changes {
            serde_json::to_writer(&mut lines, change).context("encoding a record change")?;
            lines.push(b'\n');
        }
        if self.journal.is_none() {
            let journal = open_journal(&self.journal_path)?;
            self.journal_bytes = journal.metadata().map_or(0, |meta| meta.len());
            self.journal = Some(journal);
        }
        let journal = self.journal.as_mut().expect("the journal was opened above");
        journal
            .write_all(&lines)
            .and_then(|()| journal.sync_data())
            .with_context(|| format!("writing {}", self.journal_path.display()))?;
        self.journal_bytes += lines.len() as u64;
        Ok(())
    }

    /// Whether the journal has grown enough to be folded into the file.
    pub(crate) fn wants_compaction(&self) -> bool {
        self.journal_bytes > 0
            && self.journal_bytes >= self.min_compact_bytes.max(self.file_bytes / 4)
    }

    /// Writes every record of `set` to the file, best link score first,
    /// then deletes the journal. Returns how many records were written.
    pub(crate) fn compact(&mut self, set: &RecordSet) -> Result<usize> {
        // Closed first: Windows cannot delete a file that is open.
        self.journal = None;
        let written = replace_records(&self.path, sorted_by_link_score(set))?;
        self.journal_bytes = 0;
        self.file_bytes = file_len(&self.path);
        Ok(written)
    }

    #[cfg(test)]
    pub(crate) fn set_min_compact_bytes(&mut self, bytes: u64) {
        self.min_compact_bytes = bytes;
    }
}

/// Replaces the records file at `path` with `records`, which hold every
/// change made so far, and deletes the file's journal. Returns how many
/// records were written.
pub(crate) fn replace_records<'a, I>(path: &Path, records: I) -> Result<usize>
where
    I: IntoIterator<Item = &'a SiteRecord>,
{
    let written = write_records_atomically(path, records)?;
    let journal = journal_path(path);
    match fs::remove_file(&journal) {
        Ok(()) => sync_parent_dir(&journal),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("deleting {}", journal.display()));
        }
    }
    Ok(written)
}

/// Opens a journal for appending. The directory entry of a new journal is
/// flushed to disk; an old journal whose last line a crash cut short gets a
/// line break, so that the next change starts on a line of its own. Also
/// opens the network inbox, a journal of the same kind.
pub(crate) fn open_journal(path: &Path) -> Result<File> {
    let open = || -> io::Result<File> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            sync_parent_dir(path);
        } else {
            let mut last = [0u8];
            file.seek(SeekFrom::Start(len - 1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        Ok(file)
    };
    open().with_context(|| format!("opening {}", path.display()))
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |meta| meta.len())
}

/// All records in the order [`RecordSet::into_sorted_vec`] gives (best link
/// score first, ties by domain), without copying them.
pub(crate) fn sorted_by_link_score(set: &RecordSet) -> Vec<&SiteRecord> {
    let mut scored: Vec<(f32, &SiteRecord)> = set.iter().map(|r| (r.link_score(), r)).collect();
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.domain.cmp(&b.1.domain))
    });
    scored.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use plumb_core::{read_jsonl, write_jsonl};

    use super::*;

    fn record(domain: &str, tranco: u32) -> SiteRecord {
        let mut r = SiteRecord::new(domain);
        r.signals.tranco_rank = Some(tranco);
        r
    }

    fn mark(domain: &str, attempted_at: Option<u64>, failures: u32) -> Change {
        Change::Mark {
            domain: domain.to_string(),
            attempted_at,
            failures,
        }
    }

    fn titled(domain: &str, title: &str) -> Change {
        let mut record = SiteRecord::new(domain);
        record.title = Some(title.to_string());
        record.crawled_at = Some(100);
        Change::Merge { record }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn sorted(set: RecordSet) -> Vec<SiteRecord> {
        set.into_sorted_vec()
    }

    #[test]
    fn a_refresh_updates_held_sites_and_adds_none() {
        let mut set = RecordSet::new();
        set.upsert(record("held.com", 1));
        let shared = |domain: &str| {
            let mut record = SiteRecord::new(domain);
            record.title = Some("Fresh".into());
            Change::RefreshShared { record }
        };
        shared("HELD.com").apply(&mut set);
        shared("new.com").apply(&mut set);
        assert_eq!(set.len(), 1);
        assert_eq!(set.get("held.com").unwrap().title.as_deref(), Some("Fresh"));
        assert_eq!(
            serde_json::to_string(&shared("a.com")).unwrap(),
            r#"{"op":"refresh_shared","record":{"domain":"a.com","title":"Fresh","signals":{}}}"#
        );
    }

    #[test]
    fn journals_sit_next_to_their_file() {
        assert_eq!(
            journal_path(Path::new("data/records.jsonl")),
            Path::new("data/records.jsonl.journal")
        );
        assert_eq!(
            journal_path(Path::new("out.jsonl")),
            Path::new("out.jsonl.journal")
        );
    }

    #[test]
    fn changes_are_saved_as_tagged_json_lines() {
        assert_eq!(
            serde_json::to_string(&mark("a.com", Some(5), 2)).unwrap(),
            r#"{"op":"mark","domain":"a.com","attempted_at":5,"failures":2}"#
        );
        assert_eq!(
            serde_json::to_string(&mark("a.com", None, 0)).unwrap(),
            r#"{"op":"mark","domain":"a.com","attempted_at":null,"failures":0}"#
        );
        assert_eq!(
            serde_json::to_string(&Change::Merge {
                record: SiteRecord::new("a.com")
            })
            .unwrap(),
            r#"{"op":"merge","record":{"domain":"a.com","signals":{}}}"#
        );
    }

    #[test]
    fn applying_changes() {
        let mut set: RecordSet = [record("a.com", 10)].into_iter().collect();
        mark("a.com", Some(7), 3).apply(&mut set);
        let a = set.get("a.com").unwrap();
        assert_eq!((a.crawl_attempted_at, a.crawl_failures), (Some(7), 3));
        // Marks set, so they can also put back what was there.
        mark("a.com", None, 0).apply(&mut set);
        let a = set.get("a.com").unwrap();
        assert_eq!((a.crawl_attempted_at, a.crawl_failures), (None, 0));
        // Marks do not create records; merges do, with a canonical domain.
        mark("gone.com", Some(7), 1).apply(&mut set);
        assert!(set.get("gone.com").is_none());
        titled("WWW.New.COM", "New").apply(&mut set);
        assert_eq!(set.get("new.com").unwrap().title.as_deref(), Some("New"));
        titled("not a domain", "x").apply(&mut set);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn loading_merges_duplicates_and_drops_bad_domains() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let mut upper = record("Example.COM", 50);
        upper.title = Some("Example".into());
        write_jsonl(
            &path,
            &[record("example.com", 10), upper, record("not a domain", 1)],
        )
        .unwrap();
        let set = load_records(&path).unwrap();
        assert_eq!(set.len(), 1);
        let example = set.get("example.com").unwrap();
        assert_eq!(example.signals.tranco_rank, Some(10));
        assert_eq!(example.title.as_deref(), Some("Example"));

        let err = load_records(&dir.path().join("missing.jsonl")).unwrap_err();
        assert!(format!("{err:#}").contains("missing.jsonl"), "{err:#}");
        fs::write(&path, "{\"domain\":\"a.com\"}\nnot json\n").unwrap();
        let err = load_records(&path).unwrap_err();
        assert!(format!("{err:#}").contains("records.jsonl:2"), "{err:#}");
    }

    #[test]
    fn saved_changes_are_replayed_when_loading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        write_jsonl(&path, &[record("a.com", 10), record("b.com", 20)]).unwrap();
        let mut store = RecordStore::open(&path);
        store
            .save(&[titled("c.com", "C"), mark("a.com", Some(5), 2)])
            .unwrap();
        store.save(&[]).unwrap();
        store.save(&[mark("a.com", Some(6), 0)]).unwrap();
        assert_eq!(
            names(dir.path()),
            ["records.jsonl", "records.jsonl.journal"]
        );

        let set = load_records(&path).unwrap();
        assert_eq!(set.len(), 3);
        assert_eq!(set.get("c.com").unwrap().title.as_deref(), Some("C"));
        let a = set.get("a.com").unwrap();
        assert_eq!((a.crawl_attempted_at, a.crawl_failures), (Some(6), 0));
        // The file itself is untouched until the journal is folded in.
        let file: Vec<SiteRecord> = read_jsonl(&path).unwrap();
        assert_eq!(file, vec![record("a.com", 10), record("b.com", 20)]);
    }

    #[test]
    fn damaged_journal_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        write_jsonl(&path, &[record("a.com", 10), record("b.com", 20)]).unwrap();
        let journal = journal_path(&path);
        // A good line, a damaged one, a good one, and a last line that a
        // crash cut short.
        let lines = [
            serde_json::to_string(&mark("a.com", Some(1), 1)).unwrap(),
            "{\"op\":\"teleport\"}".to_string(),
            serde_json::to_string(&mark("b.com", Some(2), 1)).unwrap(),
            "{\"op\":\"mark\",\"domain\":\"a.c".to_string(),
        ];
        fs::write(&journal, lines.join("\n")).unwrap();
        let set = load_records(&path).unwrap();
        assert_eq!(set.get("a.com").unwrap().crawl_attempted_at, Some(1));
        assert_eq!(set.get("b.com").unwrap().crawl_attempted_at, Some(2));

        // The next change starts on a line of its own.
        let mut store = RecordStore::open(&path);
        store.save(&[mark("b.com", Some(3), 0)]).unwrap();
        let set = load_records(&path).unwrap();
        let b = set.get("b.com").unwrap();
        assert_eq!((b.crawl_attempted_at, b.crawl_failures), (Some(3), 0));
        assert!(fs::read_to_string(&journal).unwrap().ends_with("}\n"));
    }

    #[test]
    fn compacting_writes_everything_and_deletes_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        write_jsonl(&path, &[record("b.com", 20), record("a.com", 10)]).unwrap();
        let mut set = load_records(&path).unwrap();
        let mut store = RecordStore::open(&path);
        let changes = vec![titled("c.com", "C"), mark("a.com", Some(5), 1)];
        store.save(&changes).unwrap();
        for change in changes {
            change.apply(&mut set);
        }

        assert_eq!(store.compact(&set).unwrap(), 3);
        assert_eq!(names(dir.path()), ["records.jsonl"]);
        let file: Vec<SiteRecord> = read_jsonl(&path).unwrap();
        assert_eq!(file, sorted(set.clone()));
        assert_eq!(
            file.iter().map(|r| r.domain.as_str()).collect::<Vec<_>>(),
            ["a.com", "b.com", "c.com"]
        );
        assert_eq!(sorted(load_records(&path).unwrap()), file);

        // The store goes on with a new journal.
        store.save(&[mark("b.com", Some(9), 0)]).unwrap();
        assert_eq!(
            load_records(&path)
                .unwrap()
                .get("b.com")
                .unwrap()
                .crawl_attempted_at,
            Some(9)
        );
        store.compact(&load_records(&path).unwrap()).unwrap();
        assert_eq!(names(dir.path()), ["records.jsonl"]);
    }

    #[test]
    fn a_journal_is_folded_in_once_it_is_big_enough() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let records: Vec<SiteRecord> = (0..100)
            .map(|i| record(&format!("site{i}.com"), i + 1))
            .collect();
        write_jsonl(&path, &records).unwrap();
        let file_bytes = fs::metadata(&path).unwrap().len();

        let mut store = RecordStore::open(&path);
        assert!(!store.wants_compaction(), "no journal yet");
        store.set_min_compact_bytes(0);
        assert!(!store.wants_compaction(), "no journal yet");
        // A quarter of the file is the bar.
        let mut saved = 0;
        while saved < file_bytes / 4 {
            assert!(!store.wants_compaction(), "{saved} of {file_bytes}");
            store.save(&[mark("site1.com", Some(saved), 1)]).unwrap();
            saved = fs::metadata(journal_path(&path)).unwrap().len();
        }
        assert!(store.wants_compaction());
        // Also when that quarter is less than the minimum.
        store.set_min_compact_bytes(MIN_COMPACT_BYTES);
        assert!(!store.wants_compaction());

        // A journal from before counts too.
        let store = RecordStore::open(&path);
        assert_eq!(store.journal_bytes, saved);
    }

    #[test]
    fn replacing_a_file_drops_a_journal_left_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        fs::write(
            journal_path(&path),
            serde_json::to_string(&titled("old.com", "Old")).unwrap() + "\n",
        )
        .unwrap();
        let fresh = [record("new.com", 1)];
        assert_eq!(replace_records(&path, &fresh).unwrap(), 1);
        assert_eq!(names(dir.path()), ["records.jsonl"]);
        assert_eq!(sorted(load_records(&path).unwrap()), fresh.to_vec());
    }

    #[test]
    fn sorts_like_record_set() {
        let set: RecordSet = [
            record("b.com", 50),
            record("a.com", 50),
            record("c.com", 5),
            SiteRecord::new("z.com"),
        ]
        .into_iter()
        .collect();
        let by_ref: Vec<&str> = sorted_by_link_score(&set)
            .iter()
            .map(|r| r.domain.as_str())
            .collect();
        let owned: Vec<String> = set
            .clone()
            .into_sorted_vec()
            .into_iter()
            .map(|r| r.domain)
            .collect();
        assert_eq!(by_ref, owned);
        assert_eq!(by_ref, ["c.com", "a.com", "b.com", "z.com"]);
    }
}
