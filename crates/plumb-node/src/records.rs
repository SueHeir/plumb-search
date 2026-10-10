//! Records files that crawls keep up to date: the whole file, rewritten now
//! and then, plus a journal of the changes made since.
//!
//! Once a records file holds a million sites, rewriting it takes seconds and
//! writes hundreds of megabytes, far too much to do after every batch of
//! homepages. So a crawl appends each batch's [`Change`]s to a journal next
//! to the file (`records.jsonl` -> `records.jsonl.journal`), flushed to disk
//! before the crawl goes on, and folds the journal into the file
//! ([`RecordStore::compact`]) only once the journal has grown to a quarter of
//! the file's size (at least [`MIN_COMPACT_BYTES`], at most
//! [`MAX_JOURNAL_BYTES`]), and when `plumb crawl` ends. [`load_records`] replays a journal it finds, so whatever an
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

/// A journal this big is folded into its file however big the file:
/// folding it a record at a time ([`RecordStore::fold`]) holds the
/// journal's changes in memory, about twice its size.
pub(crate) const MAX_JOURNAL_BYTES: u64 = 128 << 20;

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
    /// Cuts a site judged dead down to its crawl marks and ranks
    /// ([`SiteRecord::make_gone`]), gone since `at`; see [`crate::dead`].
    Gone { domain: String, at: u64 },
    /// Takes a misread official website claim back off `domain`'s record
    /// ([`RecordSet::take_back_official_site`]).
    TakeBack { domain: String, names: Vec<String> },
    /// Adds a site on a subdomain ([`plumb_core::parent_domain`]) where its
    /// parent domain has a record: it takes the parent's ranks it lacks,
    /// and its names come off the parent
    /// ([`RecordSet::split_subdomain_sites`]). Ignored without a parent.
    SubdomainSite { record: SiteRecord },
}

impl Change {
    /// The domain of the record a change may add to a set: a merge's.
    pub(crate) fn adds(&self) -> Option<&str> {
        match self {
            Change::Merge { record }
            | Change::MergeShared { record }
            | Change::SubdomainSite { record } => Some(&record.domain),
            _ => None,
        }
    }

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
            Change::Gone { domain, at } => {
                if set.get(&domain).is_some() {
                    set.entry(&domain).make_gone(at);
                }
            }
            Change::TakeBack { domain, names } => {
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                set.take_back_official_site(&domain, &names);
            }
            Change::SubdomainSite { mut record } => {
                let Some(parent) = plumb_core::parent_domain(&record.domain)
                    .and_then(|parent| set.get(parent))
                    .map(|parent| parent.signals.clone())
                else {
                    return;
                };
                let signals = &mut record.signals;
                signals.tranco_rank = signals.tranco_rank.or(parent.tranco_rank);
                signals.harmonic_rank = signals.harmonic_rank.or(parent.harmonic_rank);
                signals.pagerank_rank = signals.pagerank_rank.or(parent.pagerank_rank);
                set.split_subdomain_sites(std::slice::from_ref(&record));
                set.upsert(record);
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
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
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
            budget: None,
        }
    }

    pub(crate) fn with_budget(
        mut self,
        budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
    ) -> Self {
        self.budget = budget;
        self
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
        if let Some(budget) = self.budget.clone() {
            use plumb_core::storage::{allocation_for, file_bytes};
            anyhow::ensure!(
                records_budget(&self.path, Some(&budget))?.is_some(),
                "records are outside the node storage root"
            );
            let _mutation = budget.mutation();
            // Appends follow a final symlink; charge its resolved file and
            // directory rather than the alias inode excluded by the census.
            let accounted = canonical_file_path(&self.journal_path)?;
            let old = plumb_core::storage::existing_file_bytes(&accounted)?;
            let len = file_len(&self.journal_path);
            let mut reserved = budget.reserve(
                allocation_for(len.saturating_add(lines.len() as u64).saturating_add(1))
                    .saturating_sub(old),
                false,
            )?;
            let before_dir = accounted.parent().map(file_bytes).transpose()?.unwrap_or(0);
            let written = (|| -> Result<()> {
                if self.journal.is_none() {
                    self.journal = Some(open_journal(&self.journal_path)?);
                }
                let journal = self.journal.as_mut().unwrap();
                let written = journal.write_all(&lines).and_then(|()| journal.sync_data());
                if written.is_err() {
                    journal.set_len(len)?;
                    journal.sync_data()?;
                }
                written?;
                Ok(())
            })();
            let new = match (
                file_bytes(&accounted),
                accounted.parent().map(file_bytes).transpose(),
            ) {
                (Ok(bytes), Ok(parent)) => {
                    bytes.saturating_add(parent.unwrap_or(0).saturating_sub(before_dir))
                }
                _ => old.saturating_add(reserved.bytes()),
            };
            reserved.commit(reserved.bytes(), old, new);
            #[cfg(test)]
            note_fold_peak(&self.path, &budget);
            written?;
            self.journal_bytes = file_len(&self.journal_path);
            return Ok(());
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
            && self.journal_bytes
                >= self
                    .min_compact_bytes
                    .max((self.file_bytes / 4).min(MAX_JOURNAL_BYTES))
    }

    /// Writes every record of `set` to the file, best link score first,
    /// then deletes the journal. Returns how many records were written.
    pub(crate) fn compact(&mut self, set: &RecordSet) -> Result<usize> {
        // Closed first: Windows cannot delete a file that is open.
        self.journal = None;
        let written = if let Some(budget) = self.budget.clone() {
            let mut size = plumb_core::storage::ByteCount {
                bytes: 0,
                limit: u64::MAX,
            };
            for record in set.iter() {
                serde_json::to_writer(&mut size, record)?;
                size.bytes = size.bytes.saturating_add(1);
            }
            self.admitted_rewrite(&budget, size.bytes, || {
                replace_records(&self.path, sorted_by_link_score(set))
            })?
        } else {
            replace_records(&self.path, sorted_by_link_score(set))?
        };
        self.journal_bytes = 0;
        self.file_bytes = file_len(&self.path);
        Ok(written)
    }

    /// Folds the journal into the file a record at a time, without the set
    /// ([`crate::outline::fold_journal`]).
    pub(crate) fn fold(&mut self) -> Result<crate::outline::Folded> {
        self.fold_with_check(|| Ok(()))
    }

    pub(crate) fn fold_with_check(
        &mut self,
        mut check: impl FnMut() -> Result<()>,
    ) -> Result<crate::outline::Folded> {
        check()?;
        // Closed first: Windows cannot delete a file that is open.
        #[cfg(test)]
        if let Some(budget) = &self.budget {
            if fold_evidence().lock().unwrap().contains_key(&self.path) {
                let _mutation = budget.mutation();
                note_fold_phase(&self.path, budget, "before-close", 0);
            }
        }
        self.journal = None;
        if file_len(&self.journal_path) == 0 {
            return Ok(crate::outline::Folded::Nothing);
        }
        let written = if let Some(budget) = self.budget.clone() {
            self.admitted_fold(&budget, &mut check)?
        } else {
            crate::outline::fold_journal(&self.path, u64::MAX, || {}, &mut check)?
        };
        self.journal_bytes = file_len(&self.journal_path);
        self.file_bytes = file_len(&self.path);
        Ok(written)
    }

    fn admitted_fold(
        &self,
        budget: &std::sync::Arc<plumb_core::storage::StorageBudget>,
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<crate::outline::Folded> {
        use crate::outline::{FoldMeasure, Folded};
        anyhow::ensure!(
            records_budget(&self.path, Some(budget))?.is_some(),
            "records are outside the node storage root"
        );
        let _mutation = budget.mutation();
        #[cfg(test)]
        note_fold_phase(&self.path, budget, "before-count", 0);
        let raw = FoldInput::read(&self.path)?;
        let journal = FoldInput::read(&self.journal_path)?;
        let mut checked = || {
            #[cfg(test)]
            note_fold_peak(&self.path, budget);
            check()?;
            anyhow::ensure!(
                raw == FoldInput::read(&self.path)?
                    && journal == FoldInput::read(&self.journal_path)?,
                "records or journal changed during the admitted fold"
            );
            Ok(())
        };
        let max = file_len(&self.path)
            .saturating_add(file_len(&self.journal_path))
            .saturating_mul(2);
        let (reserved, limit) = match budget
            .reserve(plumb_core::storage::allocation_for(max), false)
        {
            Ok(reserved) => (reserved, max),
            Err(_) => {
                // The merge can preserve most of a large raw file. Only an
                // admission refusal pays for the second traversal, using the
                // writer's serializer rather than a smaller guessed bound.
                let bytes = match crate::outline::measure_fold_journal(&self.path, &mut checked)? {
                    FoldMeasure::Nothing => return Ok(Folded::Nothing),
                    FoldMeasure::NeedsSet => return Ok(Folded::NeedsSet),
                    FoldMeasure::Bytes(bytes) => bytes,
                };
                (
                    budget.reserve(plumb_core::storage::allocation_for(bytes), false)?,
                    bytes,
                )
            }
        };
        #[cfg(test)]
        note_fold_phase(&self.path, budget, "before-write", reserved.bytes());
        let installed = std::cell::Cell::new(false);
        self.rewrite_reserved(budget, reserved, Some(&installed), || {
            crate::outline::fold_journal(&self.path, limit, || installed.set(true), &mut checked)
        })
    }

    fn admitted_rewrite<T>(
        &self,
        budget: &std::sync::Arc<plumb_core::storage::StorageBudget>,
        max: u64,
        rewrite: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        anyhow::ensure!(
            records_budget(&self.path, Some(budget))?.is_some(),
            "records are outside the node storage root"
        );
        let _mutation = budget.mutation();
        let reserved = budget.reserve(plumb_core::storage::allocation_for(max), false)?;
        self.rewrite_reserved(budget, reserved, None, rewrite)
    }

    /// The caller holds mutation across measuring, admission and replacement.
    fn rewrite_reserved<T>(
        &self,
        budget: &std::sync::Arc<plumb_core::storage::StorageBudget>,
        mut reserved: plumb_core::storage::Reservation,
        installed: Option<&std::cell::Cell<bool>>,
        rewrite: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        // Raw compaction uses one temporary file beside records, not a corpus
        // tree walk. Include failure leftovers before releasing the reservation.
        let parent = fs::canonicalize(
            self.path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?;
        let before = shallow_bytes(&parent)?;
        let old_raw = plumb_core::storage::existing_file_bytes(&self.path)?;
        let result = rewrite();
        let after =
            shallow_bytes(&parent).unwrap_or_else(|_| before.saturating_add(reserved.bytes()));
        let before = if installed.is_some_and(std::cell::Cell::get) && old_raw > 0 {
            // Retire while output room is still reserved. A held reader keeps
            // its charge throughout; admission must never see a credit for
            // the old pathname between committing output and retiring it.
            budget.retire_file(&self.path, old_raw);
            before.saturating_sub(old_raw)
        } else {
            before
        };
        reserved.commit(reserved.bytes(), before, after);
        #[cfg(test)]
        note_fold_phase(&self.path, budget, "after-write", 0);
        result
    }

    #[cfg(test)]
    pub(crate) fn set_min_compact_bytes(&mut self, bytes: u64) {
        self.min_compact_bytes = bytes;
    }
}

// Test-only evidence for physical peaks and same-generation allocation
// shrinkage. Enabled by the bounded queue fixture; no production polling.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct FoldInputAllocation {
    pub logical: u64,
    pub allocated: u64,
    generation: Option<FoldInput>,
}
#[cfg(test)]
impl FoldInputAllocation {
    pub fn read(path: &Path) -> Self {
        Self {
            logical: file_len(path),
            allocated: plumb_core::storage::existing_file_bytes(path).unwrap(),
            generation: FoldInput::read(path).unwrap(),
        }
    }
    pub fn same_generation(&self, other: &Self) -> bool {
        self.generation == other.generation
    }
    pub fn dense_slack(&self) -> u64 {
        // This fixture writes dense JSON. file_bytes includes 4 KiB per-file
        // metadata; physical block rounding makes this a conservative floor.
        self.allocated
            .saturating_sub(self.logical.saturating_add(4096))
    }
}
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FoldAllocationPhase {
    pub phase: &'static str,
    pub inputs: [FoldInputAllocation; 2],
}
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FoldAllocationEvidence {
    pub phases: Vec<FoldAllocationPhase>,
    pub physical_plus_queued_peak: u64,
    pub charged_plus_reserved_peak: u64,
    pub uncovered_peak: u64,
    stage_room: u64,
    samples: usize,
}
#[cfg(test)]
fn fold_evidence(
) -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, FoldAllocationEvidence>> {
    static TRACE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, FoldAllocationEvidence>>,
    > = std::sync::OnceLock::new();
    TRACE.get_or_init(Default::default)
}
#[cfg(test)]
pub(crate) fn start_fold_allocation_evidence(path: &Path) {
    fold_evidence()
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), FoldAllocationEvidence::default());
}
#[cfg(test)]
pub(crate) fn take_fold_allocation_evidence(path: &Path) -> FoldAllocationEvidence {
    fold_evidence().lock().unwrap().remove(path).unwrap()
}
#[cfg(test)]
fn sample_fold_peak(
    evidence: &mut FoldAllocationEvidence,
    budget: &plumb_core::storage::StorageBudget,
) {
    let linked = plumb_core::storage::directory_bytes(budget.root()).unwrap();
    let status = budget.status();
    let physical = linked.saturating_add(status.reader_held_bytes);
    let queued = status.reserved_bytes.saturating_sub(evidence.stage_room);
    evidence.physical_plus_queued_peak = evidence
        .physical_plus_queued_peak
        .max(physical.saturating_add(queued));
    evidence.charged_plus_reserved_peak = evidence
        .charged_plus_reserved_peak
        .max(status.used_bytes.saturating_add(status.reserved_bytes));
    evidence.uncovered_peak = evidence
        .uncovered_peak
        .max(physical.saturating_sub(status.used_bytes.saturating_add(evidence.stage_room)));
}
#[cfg(test)]
fn note_fold_phase(
    path: &Path,
    budget: &plumb_core::storage::StorageBudget,
    phase: &'static str,
    stage_room: u64,
) {
    let mut traces = fold_evidence().lock().unwrap();
    if let Some(evidence) = traces.get_mut(path) {
        evidence.stage_room = stage_room;
        evidence.phases.push(FoldAllocationPhase {
            phase,
            inputs: [
                FoldInputAllocation::read(path),
                FoldInputAllocation::read(&journal_path(path)),
            ],
        });
        sample_fold_peak(evidence, budget);
    }
}
#[cfg(test)]
fn note_fold_peak(path: &Path, budget: &plumb_core::storage::StorageBudget) {
    let mut traces = fold_evidence().lock().unwrap();
    if let Some(evidence) = traces.get_mut(path) {
        evidence.samples += 1;
        if evidence.samples.is_multiple_of(128) {
            sample_fold_peak(evidence, budget);
        }
    }
}

/// Mutation keeps managed writers out; refuse to overwrite an input changed
/// by an outside owner during either pass without claiming admission for it.
#[cfg_attr(test, derive(Debug, Clone))]
#[derive(PartialEq, Eq)]
struct FoldInput {
    len: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl FoldInput {
    fn read(path: &Path) -> Result<Option<Self>> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).context("reading fold input generation"),
        };
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ),
        }))
    }
}

/// An explicitly external corpus belongs to its owner. Mixed scopes (for
/// example an internal symlink to external records) cannot safely rewrite
/// beside the input under either owner's ledger.
pub(crate) fn records_budget(
    path: &Path,
    budget: Option<&std::sync::Arc<plumb_core::storage::StorageBudget>>,
) -> Result<Option<std::sync::Arc<plumb_core::storage::StorageBudget>>> {
    let Some(budget) = budget else {
        return Ok(None);
    };
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staging_inside = fs::canonicalize(parent)?.starts_with(budget.root());
    let records_inside = budget.contains_file(path)?;
    let journal_inside = budget.contains_file(&journal_path(path))?;
    anyhow::ensure!(
        staging_inside == records_inside && records_inside == journal_inside,
        "records, journal and replacement directory span the node storage boundary: {}",
        path.display()
    );
    Ok(staging_inside.then(|| budget.clone()))
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

/// Node-managed full replacements hold peak room before creating their temp file.
pub(crate) fn replace_records_with_budget<'a, I>(
    path: &Path,
    records: I,
    budget: Option<&std::sync::Arc<plumb_core::storage::StorageBudget>>,
) -> Result<usize>
where
    I: IntoIterator<Item = &'a SiteRecord>,
    I::IntoIter: Clone,
{
    let records = records.into_iter();
    let Some(budget) = budget else {
        return replace_records(path, records);
    };
    let mut size = plumb_core::storage::ByteCount {
        bytes: 0,
        limit: u64::MAX,
    };
    for record in records.clone() {
        serde_json::to_writer(&mut size, record)?;
        size.bytes = size.bytes.saturating_add(1);
    }
    RecordStore::open(path).admitted_rewrite(budget, size.bytes, || replace_records(path, records))
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

fn shallow_bytes(path: &Path) -> Result<u64> {
    let mut bytes = plumb_core::storage::file_bytes(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            bytes = bytes.saturating_add(plumb_core::storage::file_bytes(&entry.path())?);
        }
    }
    Ok(bytes)
}

fn canonical_file_path(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok(
                fs::canonicalize(parent)?.join(path.file_name().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "journal needs a file name")
                })?),
            )
        }
        Err(err) => Err(err),
    }
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
    fn a_fold_that_fits_actual_output_makes_progress_with_inbox_and_queued_reservations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let mut record = SiteRecord::new("held.example");
        record.description = Some("x".repeat(96 * 1024));
        write_jsonl(&path, &[record]).unwrap();
        let mut store = RecordStore::open(&path);
        store.save(&[mark("held.example", Some(100), 1)]).unwrap();
        let expected = sorted(load_records(&path).unwrap());
        let inbox = dir.path().join("inbox.jsonl");
        write_jsonl(&inbox, &[SiteRecord::new("pending.example")]).unwrap();
        let protected_inbox = fs::read(&inbox).unwrap();
        let used = plumb_core::storage::directory_bytes(dir.path()).unwrap();
        let budget =
            plumb_core::storage::StorageBudget::open(dir.path(), used + 192 * 1024).unwrap();
        let queued = budget.reserve(64 * 1024, false).unwrap();
        let mut store = store.with_budget(Some(budget.clone()));

        assert!(matches!(
            store
                .fold()
                .expect("the complete folded output fits alongside receiving reservations"),
            crate::outline::Folded::Records(1)
        ));
        assert_eq!(sorted(load_records(&path).unwrap()), expected);
        assert!(!journal_path(&path).exists());
        assert_eq!(fs::read(&inbox).unwrap(), protected_inbox);
        assert_eq!(budget.status().reserved_bytes, 64 * 1024);
        assert!(budget.status().used_bytes + budget.status().reserved_bytes <= used + 192 * 1024);
        assert_eq!(
            budget.status().used_bytes,
            plumb_core::storage::directory_bytes(dir.path()).unwrap()
        );
        drop(queued);
        assert_eq!(budget.status().reserved_bytes, 0);
    }

    fn pressured_fold() -> (
        tempfile::TempDir,
        RecordStore,
        std::sync::Arc<plumb_core::storage::StorageBudget>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let mut record = SiteRecord::new("held.example");
        record.description = Some("x".repeat(96 * 1024));
        write_jsonl(&path, &[record]).unwrap();
        let mut store = RecordStore::open(&path);
        store.save(&[mark("held.example", Some(100), 1)]).unwrap();
        write_jsonl(
            &dir.path().join("inbox.absorbing"),
            &[SiteRecord::new("pending.example")],
        )
        .unwrap();
        let used = plumb_core::storage::directory_bytes(dir.path()).unwrap();
        let budget =
            plumb_core::storage::StorageBudget::open(dir.path(), used + 128 * 1024).unwrap();
        (dir, store.with_budget(Some(budget.clone())), budget)
    }

    fn assert_fold_ledger(budget: &plumb_core::storage::StorageBudget) {
        let status = budget.status();
        assert_eq!(status.reserved_bytes, 0);
        assert_eq!(
            status.used_bytes,
            plumb_core::storage::directory_bytes(budget.root()).unwrap() + status.reader_held_bytes
        );
        assert!(status.used_bytes <= status.limit_bytes);
    }

    fn folded_temp(dir: &Path) -> Option<PathBuf> {
        fs::read_dir(dir).unwrap().find_map(|entry| {
            let path = entry.unwrap().path();
            let name = path.file_name()?.to_str()?;
            (name.starts_with(".records.jsonl.") && name.ends_with(".tmp")).then_some(path)
        })
    }

    #[test]
    fn queued_receiving_can_block_the_exact_output_until_its_reservation_is_released() {
        let (dir, mut store, budget) = pressured_fold();
        let raw = fs::read(&store.path).unwrap();
        let journal = fs::read(&store.journal_path).unwrap();
        let inbox = fs::read(dir.path().join("inbox.absorbing")).unwrap();
        let initial = names(dir.path());
        let queued = budget.reserve(64 * 1024, false).unwrap();
        assert!(store
            .fold()
            .unwrap_err()
            .to_string()
            .contains("backpressure"));
        assert_eq!(fs::read(&store.path).unwrap(), raw);
        assert_eq!(fs::read(&store.journal_path).unwrap(), journal);
        assert_eq!(fs::read(dir.path().join("inbox.absorbing")).unwrap(), inbox);
        assert_eq!(names(dir.path()), initial);
        assert_eq!(budget.status().reserved_bytes, 64 * 1024);
        drop(queued);
        assert_fold_ledger(&budget);
        // Retry at the same cap, after the receiving owner releases its token.
        assert_eq!(store.fold().unwrap(), crate::outline::Folded::Records(1));
        assert_fold_ledger(&budget);
    }

    #[test]
    fn genuinely_insufficient_exact_output_preserves_every_input_for_restart() {
        let (dir, mut store, budget) = pressured_fold();
        budget.set_limit(budget.status().used_bytes + 64 * 1024);
        let raw = fs::read(&store.path).unwrap();
        let journal = fs::read(&store.journal_path).unwrap();
        let initial = names(dir.path());
        for _ in 0..2 {
            let mut restarted = RecordStore::open(&store.path).with_budget(Some(budget.clone()));
            assert!(restarted.fold().is_err());
            assert_eq!(fs::read(&store.path).unwrap(), raw);
            assert_eq!(fs::read(&store.journal_path).unwrap(), journal);
            assert_eq!(names(dir.path()), initial);
            assert_fold_ledger(&budget);
        }
        // Even an uncapped owner still uses the ordinary journal semantics.
        store.budget = None;
        assert_eq!(store.fold().unwrap(), crate::outline::Folded::Records(1));
        assert_eq!(load_records(&store.path).unwrap().len(), 1);
    }

    #[test]
    fn exact_fold_keeps_the_retired_raw_reader_charged_until_its_lease_drops() {
        let (_dir, mut store, budget) = pressured_fold();
        let (mut old_reader, lease) = {
            let _mutation = budget.mutation();
            (
                File::open(&store.path).unwrap(),
                budget.read_lease(&store.path),
            )
        };
        let old_bytes = plumb_core::storage::file_bytes(&store.path).unwrap();
        let old_text = fs::read(&store.path).unwrap();
        assert_eq!(store.fold().unwrap(), crate::outline::Folded::Records(1));
        assert_eq!(budget.status().reader_held_bytes, old_bytes);
        assert_fold_ledger(&budget);
        budget.recount(budget.root()).unwrap();
        assert_fold_ledger(&budget);
        let mut retained = Vec::new();
        old_reader.read_to_end(&mut retained).unwrap();
        assert_eq!(retained, old_text);
        drop(old_reader);
        drop(lease);
        assert_eq!(budget.status().reader_held_bytes, 0);
        assert_fold_ledger(&budget);
    }

    #[test]
    fn cancelled_exact_fold_cleans_output_and_retries_from_unchanged_inputs() {
        for cancel_after_writing in [false, true] {
            let (dir, mut store, budget) = pressured_fold();
            let raw = fs::read(&store.path).unwrap();
            let journal = fs::read(&store.journal_path).unwrap();
            let initial = names(dir.path());
            let mut checks = 0;
            let error = store
                .fold_with_check(|| {
                    checks += 1;
                    let written = folded_temp(dir.path()).is_some_and(|temp| {
                        fs::read(temp)
                            .is_ok_and(|bytes| serde_json::from_slice::<SiteRecord>(&bytes).is_ok())
                    });
                    if (cancel_after_writing && written) || (!cancel_after_writing && checks == 4) {
                        anyhow::bail!("injected stop request");
                    }
                    Ok(())
                })
                .unwrap_err();
            assert!(format!("{error:#}").contains("injected stop request"));
            assert_eq!(fs::read(&store.path).unwrap(), raw);
            assert_eq!(fs::read(&store.journal_path).unwrap(), journal);
            assert_eq!(names(dir.path()), initial);
            assert_fold_ledger(&budget);
            assert_eq!(store.fold().unwrap(), crate::outline::Folded::Records(1));
            assert_fold_ledger(&budget);
        }
    }

    #[test]
    fn an_outside_raw_replacement_during_count_is_not_overwritten() {
        let (dir, mut store, budget) = pressured_fold();
        let path = store.path.clone();
        let journal = fs::read(&store.journal_path).unwrap();
        let mut checks = 0;
        let error = store
            .fold_with_check(|| {
                checks += 1;
                if checks == 4 {
                    let next = dir.path().join("outside-generation");
                    write_jsonl(&next, &[SiteRecord::new("outside.example")])?;
                    fs::rename(next, &path)?;
                }
                Ok(())
            })
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("changed during the admitted fold"));
        assert_eq!(
            load_records(&path).unwrap().into_sorted_vec()[0].domain,
            "outside.example"
        );
        assert_eq!(fs::read(&store.journal_path).unwrap(), journal);
        assert!(folded_temp(dir.path()).is_none());
        assert_eq!(budget.status().reserved_bytes, 0);
        // The external writer did not participate in this owner's accounting.
        budget.recount(dir.path()).unwrap();
        assert_fold_ledger(&budget);
    }

    #[test]
    fn unexpected_stage_allocation_refuses_installation_and_keeps_the_reader_for_retry() {
        use std::io::Read;
        let (dir, mut store, budget) = pressured_fold();
        let raw = fs::read(&store.path).unwrap();
        let journal = fs::read(&store.journal_path).unwrap();
        let (mut reader, lease) = {
            let _mutation = budget.mutation();
            (
                File::open(&store.path).unwrap(),
                budget.read_lease(&store.path),
            )
        };
        let mut injected = false;
        let error = store
            .fold_with_check(|| {
                if !injected {
                    if let Some(temp) = folded_temp(dir.path()) {
                        // An outside writer can escape the filesystem envelope.
                        // Inject real allocated bytes, not a mocked counter result.
                        let mut file = OpenOptions::new().append(true).open(temp)?;
                        file.write_all(&[1; 256 * 1024])?;
                        file.sync_all()?;
                        injected = true;
                    }
                }
                Ok(())
            })
            .unwrap_err();
        assert!(injected);
        assert!(format!("{error:#}").contains("exceeds its admitted allocation"));
        assert_eq!(fs::read(&store.path).unwrap(), raw);
        assert_eq!(fs::read(&store.journal_path).unwrap(), journal);
        assert!(folded_temp(dir.path()).is_none());
        assert_eq!(budget.status().reader_held_bytes, 0);
        assert_fold_ledger(&budget);
        store.fold().unwrap();
        assert!(budget.status().reader_held_bytes > 0);
        let mut old = Vec::new();
        reader.read_to_end(&mut old).unwrap();
        assert_eq!(old, raw);
        drop(reader);
        drop(lease);
        assert_fold_ledger(&budget);
    }

    #[cfg(unix)]
    #[test]
    fn exact_fold_rename_failure_preserves_inputs_and_charges_leftover_output() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, mut store, budget) = pressured_fold();
        let raw = fs::read(&store.path).unwrap();
        let journal = fs::read(&store.journal_path).unwrap();
        let permissions = fs::metadata(dir.path()).unwrap().permissions();
        let mut blocked = false;
        let result = store.fold_with_check(|| {
            if !blocked && folded_temp(dir.path()).is_some() {
                // The stage already exists and its fd can finish writing, but
                // rename and cleanup cannot mutate the read-only directory.
                fs::set_permissions(
                    dir.path(),
                    fs::Permissions::from_mode(permissions.mode() & !0o222),
                )?;
                blocked = true;
            }
            Ok(())
        });
        fs::set_permissions(dir.path(), permissions).unwrap();
        let error = result.unwrap_err();
        assert!(blocked);
        assert!(error.to_string().contains("moving"));
        assert_eq!(fs::read(&store.path).unwrap(), raw);
        assert_eq!(fs::read(&store.journal_path).unwrap(), journal);
        let leftover = folded_temp(dir.path()).unwrap();
        assert!(leftover.is_file());
        assert_fold_ledger(&budget);
        plumb_core::storage::remove_file(&leftover, Some(&budget)).unwrap();
        store.fold().unwrap();
        assert_fold_ledger(&budget);
    }

    #[test]
    fn a_journal_delete_failure_after_install_keeps_the_old_reader_charged_and_replays_safely() {
        let (dir, store, budget) = pressured_fold();
        let fault_dir = dir.path().join("journal-delete-fault");
        fs::create_dir(&fault_dir).unwrap();
        budget.recount(dir.path()).unwrap();
        let expected = sorted(load_records(&store.path).unwrap());
        let saved_journal = dir.path().join("saved-journal");
        let (old_reader, lease) = {
            let _mutation = budget.mutation();
            (
                File::open(&store.path).unwrap(),
                budget.read_lease(&store.path),
            )
        };
        let old_bytes = plumb_core::storage::file_bytes(&store.path).unwrap();
        let installed = std::cell::Cell::new(false);
        let error = {
            let _mutation = budget.mutation();
            let crate::outline::FoldMeasure::Bytes(bytes) =
                crate::outline::measure_fold_journal(&store.path, &mut || Ok(())).unwrap()
            else {
                panic!("expected a measurable fold")
            };
            let reserved = budget
                .reserve(plumb_core::storage::allocation_for(bytes), false)
                .unwrap();
            store
                .rewrite_reserved(&budget, reserved, Some(&installed), || {
                    crate::outline::fold_journal(
                        &store.path,
                        bytes,
                        || {
                            installed.set(true);
                            // Preserve the journal's bytes, but make its unlink fail.
                            fs::rename(&store.journal_path, &saved_journal).unwrap();
                            fs::rename(&fault_dir, &store.journal_path).unwrap();
                        },
                        &mut || Ok(()),
                    )
                })
                .unwrap_err()
        };
        assert!(error.to_string().contains("deleting"));
        assert!(installed.get());
        assert_eq!(budget.status().reader_held_bytes, old_bytes);
        assert_fold_ledger(&budget);
        assert_eq!(read_jsonl::<SiteRecord>(&store.path).unwrap(), expected);
        drop(old_reader);
        drop(lease);
        assert_fold_ledger(&budget);
        fs::rename(&store.journal_path, &fault_dir).unwrap();
        fs::rename(&saved_journal, &store.journal_path).unwrap();
        let mut restarted = RecordStore::open(&store.path).with_budget(Some(budget.clone()));
        assert_eq!(
            restarted.fold().unwrap(),
            crate::outline::Folded::Records(1)
        );
        assert_eq!(sorted(load_records(&store.path).unwrap()), expected);
        assert!(!store.journal_path.exists());
        assert_fold_ledger(&budget);
    }

    #[test]
    fn exact_admission_keeps_duplicates_for_the_whole_set_without_creating_output() {
        let (dir, mut store, budget) = pressured_fold();
        let raw = fs::read(&store.path).unwrap();
        let mut doubled = raw.clone();
        doubled.extend_from_slice(&raw);
        fs::write(&store.path, &doubled).unwrap();
        budget.recount(dir.path()).unwrap();
        budget.set_limit(budget.status().used_bytes + 128 * 1024);
        let journal = fs::read(&store.journal_path).unwrap();
        let initial = names(dir.path());
        assert_eq!(store.fold().unwrap(), crate::outline::Folded::NeedsSet);
        assert_eq!(fs::read(&store.path).unwrap(), doubled);
        assert_eq!(fs::read(&store.journal_path).unwrap(), journal);
        assert_eq!(names(dir.path()), initial);
        assert_fold_ledger(&budget);
    }

    #[test]
    fn external_corpus_fold_cannot_credit_or_reserve_the_node_ledger() {
        let node = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        fs::write(node.path().join("protected"), vec![7; 64 * 1024]).unwrap();
        let budget = plumb_core::storage::StorageBudget::open(node.path(), 1).unwrap();
        let before = budget.status();
        let path = external.path().join("records.jsonl");
        write_jsonl(&path, &[SiteRecord::new("outside.example")]).unwrap();
        let mut journal = RecordStore::open(&path);
        for _ in 0..100 {
            journal
                .save(&[mark("outside.example", Some(100), 1)])
                .unwrap();
        }
        drop(journal);
        let selected = records_budget(&path, Some(&budget)).unwrap();
        assert!(selected.is_none());
        RecordStore::open(&path)
            .with_budget(selected)
            .fold()
            .unwrap();
        assert!(!journal_path(&path).exists());
        assert_eq!(budget.status(), before);
        assert_eq!(
            fs::read(node.path().join("protected")).unwrap(),
            vec![7; 64 * 1024]
        );
    }

    #[cfg(unix)]
    #[test]
    fn internal_journal_and_root_aliases_charge_the_resolved_disk_allocation() {
        use std::os::unix::fs::symlink;
        let owner = tempfile::tempdir().unwrap();
        let aliases = tempfile::tempdir().unwrap();
        let root_alias = aliases.path().join("node");
        symlink(owner.path(), &root_alias).unwrap();
        let path = root_alias.join("records.jsonl");
        write_jsonl(&path, &[SiteRecord::new("alias-journal.example")]).unwrap();
        let target_dir = owner.path().join("journal-targets");
        fs::create_dir(&target_dir).unwrap();
        let target = target_dir.join("held.journal");
        fs::write(&target, b"\n").unwrap();
        let journal_alias = journal_path(&path);
        symlink(&target, &journal_alias).unwrap();
        let budget = plumb_core::storage::StorageBudget::open(&root_alias, u64::MAX).unwrap();
        let protected = fs::read(&path).unwrap();
        let mut changed = SiteRecord::new("alias-journal.example");
        changed.description = Some("resolved allocation ".repeat(8_000));
        let mut store = RecordStore::open(&path).with_budget(Some(budget.clone()));
        store.save(&[Change::Merge { record: changed }]).unwrap();
        assert!(fs::metadata(&target).unwrap().len() > 100_000);
        assert_eq!(fs::read(&path).unwrap(), protected);
        assert_eq!(budget.status().reserved_bytes, 0);
        assert_eq!(
            budget.status().used_bytes,
            plumb_core::storage::directory_bytes(owner.path()).unwrap()
        );
        let retained_target = fs::read(&target).unwrap();
        store.fold().unwrap();
        assert!(!journal_alias.exists());
        assert_eq!(fs::read(&target).unwrap(), retained_target);
        assert!(fs::read(&path).unwrap().len() > 100_000);
        assert_eq!(budget.status().reserved_bytes, 0);
        assert_eq!(
            budget.status().used_bytes,
            plumb_core::storage::directory_bytes(owner.path()).unwrap()
        );
        budget.recount(&root_alias).unwrap();
        assert_eq!(
            budget.status().used_bytes,
            plumb_core::storage::directory_bytes(owner.path()).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn corpus_scope_resolves_root_parent_and_file_symlinks_before_rewriting() {
        use std::os::unix::fs::symlink;
        let owner = tempfile::tempdir().unwrap();
        let aliases = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let root_alias = aliases.path().join("node");
        symlink(owner.path(), &root_alias).unwrap();
        let budget = plumb_core::storage::StorageBudget::open(&root_alias, u64::MAX).unwrap();
        assert_eq!(budget.root(), fs::canonicalize(owner.path()).unwrap());
        let managed = root_alias.join("records.jsonl");
        write_jsonl(&managed, &[SiteRecord::new("inside.example")]).unwrap();
        assert!(records_budget(&managed, Some(&budget)).unwrap().is_some());
        budget.recount(&root_alias).unwrap();
        assert_eq!(
            budget.status().used_bytes,
            plumb_core::storage::directory_bytes(owner.path()).unwrap()
        );
        let outside = external.path().join("records.jsonl");
        write_jsonl(&outside, &[SiteRecord::new("outside.example")]).unwrap();
        let escape = owner.path().join("external-directory");
        symlink(external.path(), &escape).unwrap();
        assert!(records_budget(&escape.join("records.jsonl"), Some(&budget))
            .unwrap()
            .is_none());
        let mixed_input = owner.path().join("linked-records.jsonl");
        symlink(&outside, &mixed_input).unwrap();
        assert!(records_budget(&mixed_input, Some(&budget)).is_err());
        let external_alias = external.path().join("linked-records.jsonl");
        symlink(&managed, &external_alias).unwrap();
        assert!(records_budget(&external_alias, Some(&budget)).is_err());
        symlink(journal_path(&outside), journal_path(&managed)).unwrap();
        fs::write(journal_path(&outside), b"\n").unwrap();
        assert!(records_budget(&managed, Some(&budget)).is_err());
        assert_eq!(fs::read(&outside).unwrap(), fs::read(&mixed_input).unwrap());
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
        // Gone cuts a held site down, and creates none.
        let gone = |domain: &str| Change::Gone {
            domain: domain.into(),
            at: 9,
        };
        let line = serde_json::to_string(&gone("new.com")).unwrap();
        assert_eq!(line, r#"{"op":"gone","domain":"new.com","at":9}"#);
        serde_json::from_str::<Change>(&line)
            .unwrap()
            .apply(&mut set);
        gone("nowhere.com").apply(&mut set);
        let new = set.get("new.com").unwrap();
        assert_eq!((new.gone_at, new.title.as_deref()), (Some(9), None));
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
        // A quarter of a big file is more than a journal may grow to.
        store.file_bytes = 8 * MAX_JOURNAL_BYTES;
        store.journal_bytes = MAX_JOURNAL_BYTES - 1;
        assert!(!store.wants_compaction());
        store.journal_bytes = MAX_JOURNAL_BYTES;
        assert!(store.wants_compaction());
        store.journal_bytes = saved;

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
