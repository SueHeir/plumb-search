//! Records files read a record at a time, for nodes that cannot hold every
//! record in memory: a million sites take over a gigabyte as a
//! [`RecordSet`], about twice the size of the file.
//!
//! [`outline`] folds the journal into the records file the way
//! [`crate::records::load_records`] replays it, one record at a time, and
//! returns an [`Outline`] of each site: where its line is in the file and
//! the little an index build needs to put the sites in order and fold
//! redirects. [`RecordReader`] then reads the sites back one at a time.
//!
//! Only the journal's changes are held in memory meanwhile, grouped by the
//! site they change. A change to a site on a subdomain
//! ([`Change::SubdomainSite`]) takes its parent's ranks: the parent's as the
//! journal leaves it, where replaying the journal into a whole set takes them
//! as they are when the change comes (ranks come from seed data, which a
//! journal rarely changes twice).
//!
//! A file that holds a site on two lines, or under a domain that is not
//! canonical, is something only a whole set can merge: [`outline`] returns
//! `None` for it. The index build, trimming and making room for new topics
//! get the outlines of the file from [`outline_merged`], which loads such a
//! file as a set once, writes it again with each site on one line, and
//! outlines that; they then read the file a record at a time like any
//! other. A crawl round, which reads whole records, merges it the same way
//! when it finds a site twice as it reads them.
//!
//! Every search index a node builds, and the buckets other nodes take with
//! it, is built from the file this way ([`build_index`]), whatever wrote
//! the file last: setup, the full seed, a crawl, trimming. The build reads
//! each line as the site's stored item ([`RecordReader::read_item`],
//! [`plumb_core::site`]), and the index, the buckets, the feeds watched and
//! the node's denylist all work on that item; what the denylist denies is
//! left out as each site is read. The fold still reads records, until the
//! records file itself moves onto items.
//!
//! The steps that drop sites from the file (trimming to the storage limit,
//! making room for new topics) work the same way. They pick the sites to
//! drop from the outlines, with the little else the choice needs read off
//! each site's stored item once ([`domains_where`]), and write the file
//! again without them by copying the lines kept as the file holds them
//! ([`replace_lines`]), never reading those as records.
//!
//! A node's crawl round reads each line as the site's stored item too
//! ([`for_each_site`]), and so do the tools that read a file a site at a
//! time, from a copy with the journal folded in ([`outline_copy`]) when
//! there is one.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use plumb_core::{canonical_domain, parent_domain, site, stored, RecordSet, Signals, SiteRecord};
use plumb_format::Item;
use tracing::{info, warn};

use crate::records::{journal_path, Change};
use crate::{sync_parent_dir, temp_path_for};

/// What a build needs to know of a site before it reads its record.
#[derive(Debug, Clone, PartialEq)]
pub struct Outline {
    /// Where the record's line starts in the records file.
    pub(crate) offset: u64,
    /// The length of the line, without its line break.
    pub(crate) len: u32,
    /// [`SiteRecord::link_score`].
    pub(crate) score: f32,
    /// The canonical domain.
    pub(crate) domain: Box<str>,
    /// The canonical domain of the site it is under another name
    /// ([`plumb_core::Redirect::folds_into`]); empty when it redirects to no valid
    /// domain, as [`plumb_index::redirect_targets`] takes it.
    pub(crate) redirect_to: Option<Box<str>>,
    /// Judged dead ([`SiteRecord::gone_at`]).
    pub(crate) gone: bool,
}

impl Outline {
    fn of(record: &SiteRecord, offset: u64, len: u32) -> Outline {
        Outline {
            offset,
            len,
            score: record.link_score(),
            domain: record.domain.as_str().into(),
            redirect_to: record
                .redirect
                .as_ref()
                .and_then(|redirect| redirect.folds_into(&record.domain))
                .map(Into::into),
            gone: record.gone_at.is_some(),
        }
    }
}

/// Sorts outlines the way [`crate::records::sorted_by_link_score`] sorts
/// records: best link score first, ties by domain.
pub(crate) fn sort(outlines: &mut [Outline]) {
    outlines.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.domain.cmp(&b.domain))
    });
}

/// A change from the journal, as [`outline`] makes it to one site.
enum Op {
    Change(Change),
    /// [`Change::SubdomainSite`], on the site itself: merged with its
    /// parent's ranks.
    Subsite {
        record: SiteRecord,
        parent: String,
    },
    /// [`Change::SubdomainSite`], on its parent: what the parent got from
    /// the subdomain site comes off ([`RecordSet::split_subdomain_sites`]).
    Split(SiteRecord),
}

/// The journal's changes, by the site each changes, in journal order.
#[derive(Default)]
struct Pending {
    ops: HashMap<String, Vec<Op>>,
    /// Parents whose ranks a [`Op::Subsite`] takes.
    parents: HashSet<String>,
}

impl Pending {
    fn read(path: &Path) -> Result<Option<Pending>> {
        Self::read_checked(path, &mut || Ok(()))
    }

    fn read_checked(path: &Path, check: &mut dyn FnMut() -> Result<()>) -> Result<Option<Pending>> {
        check()?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("opening {}", path.display())),
        };
        let mut pending = Pending::default();
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut line = Vec::new();
        let (mut damaged, mut changes, mut lines) = (0usize, 0usize, 0usize);
        loop {
            line.clear();
            if reader
                .read_until(b'\n', &mut line)
                .with_context(|| format!("reading {}", path.display()))?
                == 0
            {
                break;
            }
            lines += 1;
            if lines.is_multiple_of(1_000) {
                check()?;
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            // Damaged lines are skipped, as replaying the journal does.
            match serde_json::from_slice::<Change>(&line) {
                Ok(change) => {
                    pending.add(change);
                    changes += 1;
                }
                Err(_) => damaged += 1,
            }
        }
        check()?;
        if damaged > 0 {
            warn!("{}: skipped {damaged} damaged lines", path.display());
        }
        if changes == 0 {
            return Ok(None);
        }
        Ok(Some(pending))
    }

    fn push(&mut self, site: String, op: Op) {
        self.ops.entry(site).or_default().push(op);
    }

    fn add(&mut self, change: Change) {
        match change {
            Change::Merge { ref record }
            | Change::MergeShared { ref record }
            | Change::RefreshShared { ref record } => {
                if let Some(site) = canonical_domain(&record.domain) {
                    self.push(site, Op::Change(change));
                }
            }
            Change::Mark { ref domain, .. }
            | Change::Gone { ref domain, .. }
            | Change::TakeBack { ref domain, .. } => {
                let site = domain.clone();
                self.push(site, Op::Change(change));
            }
            Change::SubdomainSite { record } => {
                let (Some(parent), Some(site)) = (
                    parent_domain(&record.domain).map(str::to_owned),
                    canonical_domain(&record.domain),
                ) else {
                    return;
                };
                self.parents.insert(parent.clone());
                self.push(parent.clone(), Op::Split(record.clone()));
                self.push(site, Op::Subsite { record, parent });
            }
        }
    }

    /// The record of `site` once its changes are made: `record` as the
    /// file holds it (`None` when it does not), changed and left out of
    /// the pending changes. `ranks` holds the parents' ranks for
    /// [`Op::Subsite`]; a site whose parent has none is left as it was.
    fn apply(
        &mut self,
        site: &str,
        record: Option<SiteRecord>,
        ranks: &HashMap<String, Signals>,
    ) -> Option<SiteRecord> {
        let mut set = RecordSet::new();
        if let Some(record) = record {
            set.upsert(record);
        }
        for op in self.ops.remove(site).unwrap_or_default() {
            match op {
                Op::Change(change) => change.apply(&mut set),
                Op::Subsite { mut record, parent } => {
                    let Some(parent) = ranks.get(&parent) else {
                        continue;
                    };
                    let signals = &mut record.signals;
                    signals.tranco_rank = signals.tranco_rank.or(parent.tranco_rank);
                    signals.harmonic_rank = signals.harmonic_rank.or(parent.harmonic_rank);
                    signals.pagerank_rank = signals.pagerank_rank.or(parent.pagerank_rank);
                    set.upsert(record);
                }
                Op::Split(record) => set.split_subdomain_sites(std::slice::from_ref(&record)),
            }
        }
        set.into_iter().next()
    }

    /// The ranks of each parent site an [`Op::Subsite`] takes them from,
    /// with every change of the journal made to it but the splits.
    fn parent_ranks(
        &self,
        path: &Path,
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<HashMap<String, Signals>> {
        let mut ranks = HashMap::new();
        if self.parents.is_empty() {
            return Ok(ranks);
        }
        let mut lines = Lines::open(path)?;
        let mut line_no = 0usize;
        while let Some((_, line)) = lines.next()? {
            line_no += 1;
            if line_no.is_multiple_of(1_000) {
                check()?;
            }
            let Ok(head) = stored::head(line) else {
                continue;
            };
            if !self.parents.contains(&head.domain) || ranks.contains_key(&head.domain) {
                continue;
            }
            let record = stored::read(line)
                .with_context(|| format!("{}: invalid JSON line", path.display()))?;
            if let Some(signals) = self.parent_signals(&head.domain, Some(record)) {
                ranks.insert(head.domain, signals);
            }
        }
        for (i, parent) in self.parents.iter().enumerate() {
            if i.is_multiple_of(1_000) {
                check()?;
            }
            if !ranks.contains_key(parent) {
                if let Some(signals) = self.parent_signals(parent, None) {
                    ranks.insert(parent.clone(), signals);
                }
            }
        }
        Ok(ranks)
    }

    fn parent_signals(&self, site: &str, record: Option<SiteRecord>) -> Option<Signals> {
        let mut set = RecordSet::new();
        if let Some(record) = record {
            set.upsert(record);
        }
        for op in self.ops.get(site).into_iter().flatten() {
            if let Op::Change(change) = op {
                change.clone().apply(&mut set);
            }
        }
        set.get(site).map(|record| record.signals.clone())
    }
}

/// The lines of a records file with where each starts, without their line
/// breaks; blank lines are skipped.
struct Lines {
    reader: BufReader<File>,
    line: Vec<u8>,
    at: u64,
}

impl Lines {
    fn open(path: &Path) -> Result<Lines> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        Ok(Lines {
            reader: BufReader::with_capacity(1 << 20, file),
            line: Vec::new(),
            at: 0,
        })
    }

    fn next(&mut self) -> Result<Option<(u64, &[u8])>> {
        loop {
            self.line.clear();
            let n = self.reader.read_until(b'\n', &mut self.line)?;
            if n == 0 {
                return Ok(None);
            }
            let start = self.at;
            self.at += n as u64;
            let text = trim_end(&self.line);
            if !text.iter().all(u8::is_ascii_whitespace) {
                let len = text.len();
                return Ok(Some((start, &self.line[..len])));
            }
        }
    }
}

fn trim_end(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && line[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &line[..end]
}

/// Folds the journal next to the records file at `path` into it, if there
/// is one, and returns an outline of every site in the file, in file
/// order. `None` when the file holds a site twice or under a domain that is
/// not canonical (see the module docs); the file is left as it was then.
/// Records whose domain is not a valid registrable domain are left out, as
/// loading the file leaves them out.
pub(crate) fn outline(path: &Path) -> Result<Option<Vec<Outline>>> {
    let journal = journal_path(path);
    match Pending::read(&journal)? {
        None => scan(path),
        Some(pending) => Ok(
            fold(path, &journal, pending, true, u64::MAX, || {}, &mut || {
                Ok(())
            })?
            .map(|(_, outlines)| outlines),
        ),
    }
}

/// [`outline`], with the journal folded in under the storage `budget` when
/// there is one ([`crate::records::RecordStore::fold`]).
pub(crate) fn outline_with_budget(
    path: &Path,
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
) -> Result<Option<Vec<Outline>>> {
    if let Some(budget) = budget {
        let mut store = crate::records::RecordStore::open(path).with_budget(Some(budget));
        if store.fold()? == Folded::NeedsSet {
            return Ok(None);
        }
        scan(path)
    } else {
        outline(path)
    }
}

/// The outlines of the records file at `path`, with its journal folded in
/// ([`outline_with_budget`]): how an index build, trimming and making room
/// for new topics get them. A file that only a whole set can merge (see the module docs) is first
/// loaded once ([`crate::records::load_records`]) and written again with
/// each site on one line, under the storage `budget` when there is one
/// ([`crate::records::RecordStore::compact`]); the set is dropped before
/// the file is outlined again. Fails, rather than load it again and
/// again, should the file still hold a site twice after that.
pub(crate) fn outline_merged(
    path: &Path,
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
) -> Result<Vec<Outline>> {
    if let Some(outlines) = outline_with_budget(path, budget.clone())? {
        return Ok(outlines);
    }
    info!(
        "{} holds a site more than once: reading it whole to merge them",
        path.display()
    );
    let set = crate::records::load_records(path)?;
    crate::records::RecordStore::open(path)
        .with_budget(budget.clone())
        .compact(&set)?;
    drop(set);
    outline_with_budget(path, budget)?.with_context(|| {
        format!(
            "{} still holds a site more than once after merging",
            path.display()
        )
    })
}

/// What [`fold_journal`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Folded {
    /// There was no change to fold in.
    Nothing,
    /// Folded in; the file holds this many records.
    Records(usize),
    /// Only a whole set can fold the journal into this file (see the module
    /// docs): file and journal are as they were.
    NeedsSet,
}

/// Folds the journal next to the records file at `path` into it, as
/// [`outline`] does, without outlining the sites. Notify the admitted caller
/// once the old raw generation has been replaced, including when deleting the
/// journal afterwards fails. `check` can stop the fold before installation.
pub(crate) fn fold_journal(
    path: &Path,
    limit: u64,
    installed: impl FnOnce(),
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<Folded> {
    let journal = journal_path(path);
    let Some(pending) = Pending::read_checked(&journal, check)? else {
        return Ok(Folded::Nothing);
    };
    Ok(
        match fold(path, &journal, pending, false, limit, installed, check)? {
            Some((count, _)) => Folded::Records(count),
            None => Folded::NeedsSet,
        },
    )
}

pub(crate) enum FoldMeasure {
    Nothing,
    NeedsSet,
    Bytes(u64),
}

/// Count the identical traversal and serializer without creating an output.
/// The caller keeps the managed raw and journal generation stable until the
/// measured output has been admitted and written.
pub(crate) fn measure_fold_journal(
    path: &Path,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<FoldMeasure> {
    let Some(pending) = Pending::read_checked(&journal_path(path), check)? else {
        return Ok(FoldMeasure::Nothing);
    };
    let mut written = Written::new(
        plumb_core::storage::ByteCount {
            bytes: 0,
            limit: u64::MAX,
        },
        false,
    );
    if !write_folded_into(path, pending, &mut written, check)? {
        return Ok(FoldMeasure::NeedsSet);
    }
    let (counter, _, _) = written.finish()?;
    Ok(FoldMeasure::Bytes(counter.bytes))
}

/// Calls `each` with every record of the records file at `path`, in file
/// order, as the file holds it: the journal is not replayed.
pub fn for_each_record(path: &Path, mut each: impl FnMut(SiteRecord)) -> Result<()> {
    let mut lines = Lines::open(path)?;
    let mut line_no = 0usize;
    while let Some((_, line)) = lines.next()? {
        line_no += 1;
        let record = stored::read(line)
            .with_context(|| format!("{}:{line_no}: invalid JSON line", path.display()))?;
        each(record);
    }
    Ok(())
}

/// [`for_each_record`] of the sites' stored items: calls `each` with every
/// site of the records file at `path`, in file order, read as its stored
/// item ([`stored::read_item`]) as the file holds it, not cleaned, and
/// without replaying the journal. Written back with
/// [`stored::write_item`], a site makes the line its record makes, but for
/// a crawl or redirect time past the year 9999, which no item holds.
pub fn for_each_site(path: &Path, mut each: impl FnMut(Item)) -> Result<()> {
    let mut lines = Lines::open(path)?;
    let mut line_no = 0usize;
    while let Some((_, line)) = lines.next()? {
        line_no += 1;
        let site = stored::read_item(line)
            .with_context(|| format!("{}:{line_no}: invalid JSON line", path.display()))?;
        each(site);
    }
    Ok(())
}

/// The domains of the sites of the records file at `path` that `wanted`
/// says yes to, read a site at a time as its stored item
/// ([`for_each_site`]) and cleaned as loading the file cleans them
/// ([`plumb_core::sites::Sites::upsert`]), so that each is judged as a
/// whole set holds it. This is what a step that picks sites to drop holds
/// of them besides their [`Outline`]s. The journal is not replayed: fold
/// it in first ([`outline`]).
pub(crate) fn domains_where(
    path: &Path,
    mut wanted: impl FnMut(&Item) -> bool,
) -> Result<HashSet<Box<str>>> {
    let mut domains = HashSet::new();
    for_each_site(path, |mut site| {
        site::clean(&mut site);
        site::drop_bot_check(&mut site);
        if wanted(&site) {
            domains.insert(site.id.into_boxed_str());
        }
    })?;
    Ok(domains)
}

/// [`outline`] of a file with no journal to fold in.
fn scan(path: &Path) -> Result<Option<Vec<Outline>>> {
    let mut lines = Lines::open(path)?;
    let mut outlines = Vec::new();
    let mut seen = HashSet::new();
    let mut line_no = 0usize;
    while let Some((offset, line)) = lines.next()? {
        line_no += 1;
        let head = stored::head(line)
            .with_context(|| format!("{}:{line_no}: invalid JSON line", path.display()))?;
        let Some(domain) = canonical_domain(&head.domain) else {
            continue;
        };
        if domain != head.domain || !seen.insert(domain) {
            return Ok(None);
        }
        outlines.push(Outline {
            offset,
            len: u32::try_from(line.len()).context("a record line is too long")?,
            score: plumb_core::link_score(&head.signals),
            redirect_to: head
                .redirect
                .as_ref()
                .and_then(|redirect| redirect.folds_into(&head.domain))
                .map(Into::into),
            domain: head.domain.into(),
            gone: head.gone_at.is_some(),
        });
    }
    Ok(Some(outlines))
}

/// [`outline`] of a file with `pending` changes from its journal: writes
/// the changed file next to it, then puts it in place and deletes the
/// journal, as [`crate::records::replace_records`] does.
/// Returns how many records the file holds then, and their outlines when
/// `outlines` (none otherwise).
fn fold(
    path: &Path,
    journal: &Path,
    pending: Pending,
    outlines: bool,
    limit: u64,
    installed: impl FnOnce(),
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<Option<(usize, Vec<Outline>)>> {
    let tmp = temp_path_for(path);
    let written = write_folded_checked(path, pending, &tmp, outlines, limit, check);
    let (count, outlines) = match written {
        Ok(Some(written)) => written,
        Ok(None) => {
            let _ = fs::remove_file(&tmp);
            return Ok(None);
        }
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            return Err(err.context(format!("writing {}", tmp.display())));
        }
    };
    if let Err(err) = check() {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("moving {} to {}", tmp.display(), path.display()));
    }
    installed();
    sync_parent_dir(path);
    match fs::remove_file(journal) {
        Ok(()) => sync_parent_dir(journal),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("deleting {}", journal.display())),
    }
    info!(
        "folded the journal into {} ({count} records)",
        path.display()
    );
    Ok(Some((count, outlines)))
}

/// Writes the records file at `path` with `pending` changes folded in to
/// `out`, leaving `path` as it is. Returns how many records `out` holds and,
/// when `outlines`, their outlines; `None` when only a whole set can fold
/// the changes (see the module docs).
fn write_folded(
    path: &Path,
    pending: Pending,
    out: &Path,
    outlines: bool,
) -> Result<Option<(usize, Vec<Outline>)>> {
    write_folded_checked(path, pending, out, outlines, u64::MAX, &mut || Ok(()))
}

fn write_folded_checked(
    path: &Path,
    pending: Pending,
    out: &Path,
    outlines: bool,
    limit: u64,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<Option<(usize, Vec<Outline>)>> {
    let file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    if limit != u64::MAX {
        // Declare the admitted EOF before writing. Sequential append into an
        // empty file can allocate a speculative tail larger than the payload.
        // Unwritten space remains sparse on filesystems that support holes.
        file.set_len(limit)?;
        check()?;
        anyhow::ensure!(
            plumb_core::storage::file_bytes(out)? <= plumb_core::storage::allocation_for(limit),
            "folded output exceeds its admitted allocation"
        );
    }
    #[cfg(test)]
    let checks = std::cell::RefCell::new(check);
    #[cfg(test)]
    let file = ObservedFoldFile {
        file,
        check: &checks,
    };
    #[cfg(test)]
    let mut checked = || (checks.borrow_mut())();
    #[cfg(not(test))]
    let mut checked = check;
    let mut written = Written::new(file, outlines);
    written.limit = limit;
    if !write_folded_into(path, pending, &mut written, &mut checked)? {
        return Ok(None);
    }
    let bytes = written.at;
    let (file, count, outlines) = written.finish()?;
    #[cfg(test)]
    let file = file.file;
    if limit != u64::MAX {
        // A conservative admission can exceed the actual serialized length.
        // Publish only the completed records, never the sparse trailing range.
        file.set_len(bytes)?;
    }
    checked()?;
    file.sync_all()?;
    checked()?;
    if limit != u64::MAX {
        anyhow::ensure!(
            plumb_core::storage::file_bytes(out)? <= plumb_core::storage::allocation_for(limit),
            "folded output exceeds its admitted allocation"
        );
    }
    drop(file);
    checked()?;
    Ok(Some((count, outlines)))
}

/// Resource regressions observe every underlying write/buffer flush, as well
/// as pre-sizing, truncation, sync and closure. Production retains its bounded
/// traversal checks without adding a filesystem census to each write.
#[cfg(test)]
struct ObservedFoldFile<'a, 'b> {
    file: File,
    check: &'a std::cell::RefCell<&'b mut dyn FnMut() -> Result<()>>,
}

#[cfg(test)]
impl Write for ObservedFoldFile<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.file.write(bytes)?;
        (self.check.borrow_mut())().map_err(io::Error::other)?;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()?;
        (self.check.borrow_mut())().map_err(io::Error::other)
    }
}

fn write_folded_into<W: Write>(
    path: &Path,
    mut pending: Pending,
    written: &mut Written<W>,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<bool> {
    check()?;
    let ranks = pending.parent_ranks(path, check)?;
    let mut lines = Lines::open(path)?;
    let mut seen = HashSet::new();
    let mut line_no = 0usize;
    while let Some((_, line)) = lines.next()? {
        line_no += 1;
        if line_no.is_multiple_of(1_000) {
            check()?;
        }
        let record = stored::read(line)
            .with_context(|| format!("{}:{line_no}: invalid JSON line", path.display()))?;
        let Some(domain) = canonical_domain(&record.domain) else {
            continue;
        };
        if domain != record.domain || !seen.insert(domain.clone()) {
            return Ok(false);
        }
        if let Some(record) = pending.apply(&domain, Some(record), &ranks) {
            written.write(&record)?;
        }
    }
    // Sites the file does not hold yet, in a fixed order.
    let mut rest: Vec<String> = pending.ops.keys().cloned().collect();
    rest.sort_unstable();
    for (i, site) in rest.into_iter().enumerate() {
        if i.is_multiple_of(1_000) {
            check()?;
        }
        if let Some(record) = pending.apply(&site, None, &ranks) {
            // A change may name a site the file holds under its canonical
            // domain only through another change.
            if seen.insert(record.domain.clone()) {
                written.write(&record)?;
            }
        }
    }
    check()?;
    Ok(true)
}

/// [`outline`] for a reader that must not change the records file at
/// `path`, such as `plumb-tools index` next to a running node: the file with its
/// journal folded in is written to `copy` (when there is a journal), and
/// the outlines are of `copy` then. Returns the file to read the records
/// from with them; `None` as [`outline`] does.
pub fn outline_copy<'a>(
    path: &'a Path,
    copy: &'a Path,
) -> Result<Option<(&'a Path, Vec<Outline>)>> {
    match Pending::read(&journal_path(path))? {
        None => Ok(scan(path)?.map(|outlines| (path, outlines))),
        Some(pending) => Ok(write_folded(path, pending, copy, true)
            .with_context(|| format!("writing {}", copy.display()))?
            .map(|(_, outlines)| (copy, outlines))),
    }
}

/// How far [`build_index`] got, for its caller to show (and to stop it,
/// by returning an error).
pub enum Step {
    /// Reading `sites` sites, of which at most `docs` go in the index.
    Started { docs: usize, sites: usize },
    /// `done` of the `sites` read.
    Read { done: usize, sites: usize },
    /// Every site read; the index is being written out.
    Writing,
}

/// What [`build_index`] made.
pub struct Built {
    /// Sites in the index.
    pub docs: usize,
    /// Whether the buckets were written where asked.
    pub buckets: bool,
    /// The best-ranked sites whose feeds to watch ([`wants_feed`]), as many
    /// as asked for.
    pub feeds: Vec<crate::news::Watched>,
}

/// Sites read between calls of [`build_index`]'s `step`.
const STEP_EVERY: usize = 10_000;

/// Builds an index in `index_dir` of the records file at `path`, whose
/// sites `outlines` gives ([`outline`]), reading a site at a time as its
/// stored item ([`RecordReader::read_item`]): what
/// [`plumb_index::build_index`] makes of the whole set, best link score
/// first, without the sites judged dead, and with sites whose homepage
/// redirects to another folded into it. With `buckets`, also writes the
/// buckets of the same sites there ([`plumb_net::BucketWriter`]); a failure
/// there is logged and leaves the index without. `step` is called as the
/// build goes; an error from it stops the build.
///
/// `allowed`, when given, says whether a site goes in, and may take parts
/// off its item first: a node leaves out what its denylist denies. It is
/// asked of each site by its domain alone (an item that holds nothing
/// else, [`site::new`]) before redirects are folded, so that a site left
/// out lends its name to no other and one that redirects to it goes in on
/// its own, as when the site was never there; then of each site as it is
/// read, for the index, the buckets and the feeds alike.
#[allow(clippy::too_many_arguments)]
pub fn build_index(
    path: &Path,
    mut outlines: Vec<Outline>,
    index_dir: &Path,
    buckets: Option<&Path>,
    feeds: usize,
    allowed: Option<&dyn Fn(&mut Item) -> bool>,
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
    step: &mut dyn FnMut(Step) -> Result<()>,
) -> Result<Built> {
    outlines.retain(|outline| {
        !outline.gone && allowed.is_none_or(|allowed| allowed(&mut site::new(&*outline.domain)))
    });
    outlines.shrink_to_fit();
    sort(&mut outlines);
    let folded_into = plumb_index::redirect_targets(
        &outlines,
        |outline| &outline.domain,
        |outline| outline.redirect_to.as_deref().map(str::to_owned),
    );
    let mut redirect_names: HashMap<usize, Vec<String>> = HashMap::new();
    for (i, target) in folded_into.iter().enumerate() {
        if let Some(target) = target {
            redirect_names
                .entry(*target)
                .or_default()
                .push(plumb_index::redirect_name(&outlines[i].domain));
        }
    }
    let docs = folded_into.iter().filter(|target| target.is_none()).count();
    let sites = outlines.len();
    step(Step::Started { docs, sites })?;

    let mut index = plumb_index::IndexBuild::new_with_budget(index_dir, budget.clone())?;
    let mut writer = match buckets {
        None => None,
        Some(dir) => match plumb_net::BucketWriter::new_with_budget(dir, budget.clone()) {
            Ok(writer) => Some(writer),
            Err(err) => {
                drop_buckets(dir, &err);
                None
            }
        },
    };
    let mut watched = Vec::new();
    let mut indexed = 0;
    let mut reader = RecordReader::open(path)?;
    for (i, outline) in outlines.iter().enumerate() {
        if i > 0 && i % STEP_EVERY == 0 {
            step(Step::Read { done: i, sites })?;
        }
        let mut site = reader.read_item(outline)?;
        // Left out by its homepage, which no outline holds, a site takes
        // the sites folded into it out with it.
        if allowed.is_some_and(|allowed| !allowed(&mut site)) {
            continue;
        }
        if let (Some(bucket_writer), Some(dir)) = (writer.as_mut(), buckets) {
            if let Err(err) = bucket_writer.add(&site) {
                writer = None;
                drop_buckets(dir, &err);
            }
        }
        if watched.len() < feeds && wants_feed(&site) {
            watched.push(feed_of(&site));
        }
        if folded_into[i].is_none() {
            let names = redirect_names.get(&i).map_or(&[][..], Vec::as_slice);
            index.add(&site, names)?;
            indexed += 1;
        }
    }
    drop((reader, redirect_names, folded_into, outlines));
    step(Step::Writing)?;
    index.finish()?;
    let mut wrote_buckets = false;
    if let (Some(bucket_writer), Some(dir)) = (writer, buckets) {
        match bucket_writer.finish() {
            Ok(_) => wrote_buckets = true,
            Err(err) => drop_buckets(dir, &err),
        }
    }
    Ok(Built {
        docs: indexed,
        buckets: wrote_buckets,
        feeds: watched,
    })
}

/// Gives up on buckets that could not be written, which leaves the index
/// without: a node goes on without the network's or private search's
/// buckets then.
fn drop_buckets(dir: &Path, err: &anyhow::Error) {
    warn!("cannot write the buckets in {}: {err:#}", dir.display());
    let _ = fs::remove_dir_all(dir);
    let _ = fs::remove_dir_all(dir.with_extension("staging"));
}

/// Whether a node watches the feed of the site of the stored item `site`,
/// among its best-ranked: it answered its last crawl and redirects nowhere.
pub(crate) fn wants_feed(site: &Item) -> bool {
    site::redirect(site).is_none() && site::crawl_marks(site).failures == 0
}

/// The site of the stored item `site` to watch the feed of
/// ([`crate::news`]): its domain, homepage and language, as the index has
/// it, so a search in English leaves out the headlines of a site written in
/// Cyrillic that declares nothing as it leaves out the site.
pub(crate) fn feed_of(site: &Item) -> crate::news::Watched {
    let domain = site::domain(site);
    crate::news::Watched {
        domain: domain.to_string(),
        homepage: site
            .url
            .clone()
            .unwrap_or_else(|| format!("https://{domain}/")),
        language: site::written_language(site),
        ..Default::default()
    }
}

/// A records file being written, with an outline of each record when
/// asked for.
struct Written<W: Write> {
    out: BufWriter<W>,
    at: u64,
    line: Vec<u8>,
    outlines: Vec<Outline>,
    outline: bool,
    count: usize,
    limit: u64,
}

impl<W: Write> Written<W> {
    fn new(out: W, outline: bool) -> Self {
        Self {
            out: BufWriter::with_capacity(1 << 20, out),
            at: 0,
            line: Vec::new(),
            outlines: Vec::new(),
            outline,
            count: 0,
            limit: u64::MAX,
        }
    }

    fn write(&mut self, record: &SiteRecord) -> Result<()> {
        self.line.clear();
        stored::write(&mut self.line, record)?;
        let len = u32::try_from(self.line.len()).context("a record line is too long")?;
        anyhow::ensure!(
            (self.line.len() as u64) < self.limit.saturating_sub(self.at),
            "folded records exceed their admitted output bound"
        );
        self.count += 1;
        if self.outline {
            self.outlines.push(Outline::of(record, self.at, len));
        }
        self.line.push(b'\n');
        self.out.write_all(&self.line)?;
        self.at += self.line.len() as u64;
        Ok(())
    }

    fn finish(self) -> Result<(W, usize, Vec<Outline>)> {
        let out = self.out.into_inner().map_err(|err| err.into_error())?;
        Ok((out, self.count, self.outlines))
    }
}

/// Reads the sites of a records file by their [`Outline`]s.
pub(crate) struct RecordReader {
    file: BufReader<File>,
    /// Where the reader is in the file.
    at: u64,
    line: Vec<u8>,
}

impl RecordReader {
    pub(crate) fn open(path: &Path) -> Result<RecordReader> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        Ok(RecordReader {
            file: BufReader::with_capacity(1 << 16, file),
            at: 0,
            line: Vec::new(),
        })
    }

    /// The line `outline` points at, as the file holds it, without its
    /// line break.
    fn line(&mut self, outline: &Outline) -> Result<&[u8]> {
        if outline.offset != self.at {
            // Relative seeks within the buffer keep it when they can.
            let delta = i64::try_from(outline.offset).unwrap_or(i64::MAX)
                - i64::try_from(self.at).unwrap_or(i64::MAX);
            self.file.seek_relative(delta)?;
        }
        self.line.resize(outline.len as usize, 0);
        self.file.read_exact(&mut self.line)?;
        self.at = outline.offset + u64::from(outline.len);
        Ok(&self.line)
    }

    /// The site `outline` points at, as its stored item
    /// ([`stored::read_item`]), cleaned and with page fields read off a bot
    /// check dropped ([`site::clean`], [`site::drop_bot_check`]), as loading
    /// the file does: the item of what [`RecordReader::read`] reads.
    pub(crate) fn read_item(&mut self, outline: &Outline) -> Result<Item> {
        let mut site = stored::read_item(self.line(outline)?)
            .with_context(|| format!("reading the site {}", outline.domain))?;
        same_site(site::domain(&site), outline)?;
        site::clean(&mut site);
        site::drop_bot_check(&mut site);
        Ok(site)
    }

    /// The record `outline` points at, cleaned and with page fields read
    /// off a bot check dropped, as loading the file does
    /// ([`RecordSet::upsert`]). The fold's tests read the folded file back
    /// with it; an index build reads items ([`RecordReader::read_item`]).
    #[cfg(test)]
    pub(crate) fn read(&mut self, outline: &Outline) -> Result<SiteRecord> {
        let mut record = stored::read(self.line(outline)?)
            .with_context(|| format!("reading the record of {}", outline.domain))?;
        same_site(&record.domain, outline)?;
        record.clean();
        record.drop_bot_check();
        Ok(record)
    }
}

/// Fails unless `domain`, of a line read by its outline, is the outline's:
/// the file changed under the reader.
fn same_site(domain: &str, outline: &Outline) -> Result<()> {
    if domain != &*outline.domain {
        anyhow::bail!(
            "the records file changed while it was read: found {domain} where {} was",
            outline.domain
        );
    }
    Ok(())
}

/// Replaces the records file at `path` with the sites of it that `outlines`
/// gives, in their order, as [`crate::records::replace_records`] replaces it
/// with records: by way of a file next to it flushed to disk before the
/// rename, then deleting the journal, which must be folded in already
/// ([`outline`]). Each line is copied as the file holds it, never read as a
/// record and written again. With a storage `budget`, room for the new
/// file (the lines and their line breaks) is admitted first, as
/// [`crate::records::replace_records_with_budget`] admits it. Returns how
/// many records were written.
pub(crate) fn replace_lines(
    path: &Path,
    outlines: &[Outline],
    budget: Option<&std::sync::Arc<plumb_core::storage::StorageBudget>>,
) -> Result<usize> {
    let Some(budget) = budget else {
        return write_lines(path, outlines);
    };
    let bytes = outlines
        .iter()
        .map(|outline| u64::from(outline.len) + 1)
        .sum();
    crate::records::RecordStore::open(path)
        .admitted_rewrite(budget, bytes, || write_lines(path, outlines))
}

/// [`replace_lines`], with no storage budget to admit the file.
fn write_lines(path: &Path, outlines: &[Outline]) -> Result<usize> {
    let written = crate::write_lines_atomically(path, |out| {
        // Closed before the new file takes its place: Windows cannot
        // rename over a file that is open.
        let mut reader = RecordReader::open(path)?;
        for outline in outlines {
            out.write_all(reader.line(outline)?)?;
            out.write_all(b"\n")?;
        }
        Ok(outlines.len())
    })?;
    crate::records::delete_journal(path)?;
    Ok(written)
}

#[cfg(test)]
pub(crate) mod tests {
    use plumb_core::{write_jsonl, LinkText, Redirect};

    use super::*;
    use crate::records::{load_records, RecordStore};

    fn record(domain: &str, tranco: u32) -> SiteRecord {
        let mut r = SiteRecord::new(domain);
        r.signals.tranco_rank = Some(tranco);
        r
    }

    fn crawled(domain: &str, title: &str, at: u64) -> SiteRecord {
        let mut r = SiteRecord::new(domain);
        r.title = Some(title.into());
        r.body_text = Some(format!("{title} home page text"));
        r.link_texts = vec![LinkText::with_count(title, 2)];
        r.crawled_at = Some(at);
        r
    }

    #[test]
    fn a_feed_is_in_the_language_its_site_is_written_in() {
        let feed = |record: &SiteRecord| feed_of(&record.to_stored_item());
        let mut news = crawled("lenta.example", "Лента новостей России и мира сегодня", 1);
        assert_eq!(feed(&news).language.as_deref(), Some("Cyrl"));
        assert_eq!(feed(&news).homepage, "https://lenta.example/");
        // A declaration still wins.
        news.language = Some("en".into());
        assert_eq!(feed(&news).language.as_deref(), Some("en"));
        let english = crawled("news.example", "World news", 1);
        assert_eq!(feed(&english).language, None);
        // Not of a site that redirects or failed its last crawl.
        assert!(wants_feed(&english.to_stored_item()));
        let mut failed = english.clone();
        failed.crawl_failures = 1;
        assert!(!wants_feed(&failed.to_stored_item()));
        let mut moved = english;
        moved.redirect = Some(Redirect {
            to: "news.example.org".into(),
            at: 2,
            inner: false,
        });
        assert!(!wants_feed(&moved.to_stored_item()));
    }

    /// Every site in the records file at `path`, read a record at a time.
    /// Each is also read as its stored item, as an index build reads it,
    /// which is the record's.
    fn read_all(path: &Path, outlines: &[Outline]) -> Vec<SiteRecord> {
        let mut reader = RecordReader::open(path).unwrap();
        let mut sorted = outlines.to_vec();
        sort(&mut sorted);
        let mut items = RecordReader::open(path).unwrap();
        sorted
            .iter()
            .map(|o| {
                let record = reader.read(o).unwrap();
                assert_eq!(items.read_item(o).unwrap(), record.to_stored_item());
                record
            })
            .collect()
    }

    /// A records file and a journal that make every kind of change.
    pub(crate) fn file_and_journal(path: &Path) -> Vec<Change> {
        let mut parent = record("google.com", 3);
        parent.add_alias("Google Scholar");
        parent.about = Some("search engine".into());
        let mut redirecting = record("pncbank.com", 900);
        redirecting.redirect = Some(Redirect {
            to: "pnc.com".into(),
            at: 40,
            inner: false,
        });
        write_jsonl(
            path,
            &[
                record("a.com", 10),
                crawled("b.org", "B", 50),
                parent,
                record("pnc.com", 800),
                redirecting,
                record("quiet.net", 5000),
            ],
        )
        .unwrap();
        let mut scholar = SiteRecord::new("scholar.google.com");
        scholar.add_alias("Google Scholar");
        scholar.signals.official_site = true;
        let mut shared = crawled("a.com", "A shared", 70);
        shared.body_text = None;
        vec![
            Change::Merge {
                record: crawled("a.com", "A", 60),
            },
            Change::MergeShared { record: shared },
            Change::Mark {
                domain: "b.org".into(),
                attempted_at: Some(80),
                failures: 2,
            },
            Change::Merge {
                record: crawled("WWW.New.COM", "New", 90),
            },
            Change::Mark {
                domain: "never.com".into(),
                attempted_at: Some(1),
                failures: 1,
            },
            Change::RefreshShared {
                record: crawled("unheld.com", "Unheld", 95),
            },
            Change::RefreshShared {
                record: crawled("b.org", "B again", 96),
            },
            Change::Gone {
                domain: "quiet.net".into(),
                at: 100,
            },
            Change::TakeBack {
                domain: "pnc.com".into(),
                names: vec!["Nothing".into()],
            },
            Change::SubdomainSite { record: scholar },
            Change::Merge {
                record: crawled("new.com", "New again", 110),
            },
        ]
    }

    #[test]
    fn folding_the_journal_gives_what_loading_gives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let changes = file_and_journal(&path);
        RecordStore::open(&path).save(&changes).unwrap();
        let loaded = load_records(&path).unwrap().into_sorted_vec();

        let outlines = outline(&path).unwrap().unwrap();
        assert!(!journal_path(&path).exists());
        let read = read_all(&path, &outlines);
        assert_eq!(read, loaded);
        // The subdomain site took its parent's rank, and its name came off
        // the parent.
        let scholar = read
            .iter()
            .find(|r| r.domain == "scholar.google.com")
            .unwrap();
        assert_eq!(scholar.signals.tranco_rank, Some(3));
        let google = read.iter().find(|r| r.domain == "google.com").unwrap();
        assert!(google.aliases.is_empty());
        // Outlines say what an index build needs.
        let by_domain = |d: &str| outlines.iter().find(|o| &*o.domain == d).unwrap();
        assert!(by_domain("quiet.net").gone);
        assert_eq!(
            by_domain("pncbank.com").redirect_to.as_deref(),
            Some("pnc.com")
        );
        let a = read.iter().find(|r| r.domain == "a.com").unwrap();
        assert_eq!(by_domain("a.com").score, a.link_score());
        // Read again with no journal: the same outlines, from the file.
        assert_eq!(outline(&path).unwrap().unwrap(), outlines);
        assert_eq!(load_records(&path).unwrap().into_sorted_vec(), loaded);
    }

    #[test]
    fn exact_admission_preserves_every_merge_kind_and_outline_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let changes = file_and_journal(&path);
        let mut held = SiteRecord::new("held.example");
        held.description = Some("x".repeat(96 * 1024));
        let mut raw = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        serde_json::to_writer(&mut raw, &held).unwrap();
        raw.write_all(b"\n").unwrap();
        drop(raw);
        RecordStore::open(&path).save(&changes).unwrap();
        let loaded = load_records(&path).unwrap().into_sorted_vec();
        let FoldMeasure::Bytes(bytes) = measure_fold_journal(&path, &mut || Ok(())).unwrap() else {
            panic!("expected a measurable fold")
        };
        let conservative =
            fs::metadata(&path).unwrap().len() + fs::metadata(journal_path(&path)).unwrap().len();
        let room = plumb_core::storage::allocation_for(bytes);
        assert!(room < plumb_core::storage::allocation_for(conservative * 2));
        let used = plumb_core::storage::directory_bytes(dir.path()).unwrap();
        let budget = plumb_core::storage::StorageBudget::open(dir.path(), used + room).unwrap();
        let outlines = outline_with_budget(&path, Some(budget.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), bytes);
        assert_eq!(read_all(&path, &outlines), loaded);
        assert!(!journal_path(&path).exists());
        assert_eq!(budget.status().reserved_bytes, 0);
        assert_eq!(
            budget.status().used_bytes,
            plumb_core::storage::directory_bytes(dir.path()).unwrap()
        );
    }

    #[test]
    fn a_fold_write_failure_leaves_the_input_and_journal_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let changes = file_and_journal(&path);
        RecordStore::open(&path).save(&changes).unwrap();
        let raw = fs::read(&path).unwrap();
        let journal = fs::read(journal_path(&path)).unwrap();
        let pending = Pending::read(&journal_path(&path)).unwrap().unwrap();
        // File::create cannot create an output over an existing directory.
        assert!(write_folded(&path, pending, dir.path(), false).is_err());
        assert!(fold_journal(&path, 1, || panic!("must not install"), &mut || Ok(())).is_err());
        assert_eq!(fs::read(&path).unwrap(), raw);
        assert_eq!(fs::read(journal_path(&path)).unwrap(), journal);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
        fold_journal(&path, u64::MAX, || {}, &mut || Ok(())).unwrap();
        assert!(!journal_path(&path).exists());
    }

    #[test]
    fn a_copy_folds_the_journal_and_leaves_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let changes = file_and_journal(&path);
        RecordStore::open(&path).save(&changes).unwrap();
        let file = fs::read(&path).unwrap();
        let journal = fs::read(journal_path(&path)).unwrap();
        let loaded = load_records(&path).unwrap().into_sorted_vec();

        let copy = dir.path().join("copy.jsonl");
        let (read_from, outlines) = outline_copy(&path, &copy).unwrap().unwrap();
        assert_eq!(read_from, copy);
        assert_eq!(read_all(&copy, &outlines), loaded);
        assert_eq!(fs::read(&path).unwrap(), file);
        assert_eq!(fs::read(journal_path(&path)).unwrap(), journal);

        // With no journal, the file itself is read.
        let (read_from, outlines) = outline_copy(&copy, &path).unwrap().unwrap();
        assert_eq!(read_from, copy);
        assert_eq!(read_all(&copy, &outlines), loaded);
        assert_eq!(fs::read(&path).unwrap(), file);
    }

    #[test]
    fn a_site_on_two_lines_needs_the_whole_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        write_jsonl(&path, &[record("a.com", 1), record("A.com", 2)]).unwrap();
        assert_eq!(outline(&path).unwrap(), None);
        write_jsonl(&path, &[record("a.com", 1), record("a.com", 2)]).unwrap();
        assert_eq!(outline(&path).unwrap(), None);
        // With a journal, too, which is left for loading to replay.
        RecordStore::open(&path)
            .save(&[Change::Merge {
                record: record("b.com", 3),
            }])
            .unwrap();
        assert_eq!(outline(&path).unwrap(), None);
        assert!(journal_path(&path).exists());
        assert_eq!(load_records(&path).unwrap().len(), 2);
    }

    #[test]
    fn a_file_only_a_whole_set_can_merge_is_merged_once_and_outlined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        // a.com on two lines, b.com under another form of its domain, and a
        // journal still to fold in.
        write_jsonl(
            &path,
            &[
                record("a.com", 10),
                record("WWW.B.com", 20),
                crawled("a.com", "A", 5),
                record("c.org", 30),
            ],
        )
        .unwrap();
        RecordStore::open(&path)
            .save(&[Change::Merge {
                record: crawled("d.net", "D", 6),
            }])
            .unwrap();
        assert_eq!(outline(&path).unwrap(), None);
        let loaded = load_records(&path).unwrap().into_sorted_vec();
        let budget = plumb_core::storage::StorageBudget::open(dir.path(), u64::MAX).unwrap();

        let outlines = outline_merged(&path, Some(budget.clone())).unwrap();
        let mut domains: Vec<&str> = outlines.iter().map(|o| &*o.domain).collect();
        domains.sort_unstable();
        assert_eq!(domains, ["a.com", "b.com", "c.org", "d.net"]);
        assert_eq!(read_all(&path, &outlines), loaded);
        // Written again with each site on one line and the journal folded
        // in, so it is outlined as it is from now on.
        assert!(!journal_path(&path).exists());
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 4);
        assert_eq!(outline(&path).unwrap().unwrap(), outlines);
        // The new file was admitted and is charged as it is.
        assert_eq!(budget.status().reserved_bytes, 0);
        assert_eq!(
            budget.status().used_bytes,
            plumb_core::storage::directory_bytes(dir.path()).unwrap()
        );

        // A file that holds each site once comes back as it was, byte for
        // byte, whatever the shape of its lines and their order.
        let line = |r: &SiteRecord| serde_json::to_string(r).unwrap();
        let item = String::from_utf8(stored::line(&crawled("b.com", "B", 5)).unwrap()).unwrap();
        let clean = format!(
            "{}\n\n{}\r\n{item}\n",
            line(&record("quiet.net", 5000)),
            line(&record("a.com", 10))
        );
        fs::write(&path, &clean).unwrap();
        let outlines = outline_merged(&path, Some(budget)).unwrap();
        assert_eq!(outlines, outline(&path).unwrap().unwrap());
        assert_eq!(outlines.len(), 3);
        assert_eq!(fs::read_to_string(&path).unwrap(), clean);
    }

    /// Checks `text` against the golden file at `path`, or writes it there
    /// when `PLUMB_WRITE_GOLDEN` is set, as plumb-index's golden test does.
    pub(crate) fn check_golden(path: &Path, text: &str) {
        if std::env::var_os("PLUMB_WRITE_GOLDEN").is_some() {
            fs::write(path, text).unwrap();
            return;
        }
        let golden = fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        let (mut want, mut got) = (golden.lines(), text.lines());
        for n in 1.. {
            match (want.next(), got.next()) {
                (None, None) => break,
                (want, got) if want == got => continue,
                (want, got) => panic!(
                    "{}:{n} differs\n  golden: {}\n  now:    {}",
                    path.display(),
                    want.unwrap_or("(the end)"),
                    got.unwrap_or("(the end)")
                ),
            }
        }
    }

    /// What a node's build of the sites of plumb-index's golden file makes
    /// beside the index, against `outline_golden.txt` next to this file:
    /// the buckets other nodes take, byte for byte, and the feeds watched;
    /// and a bucket table of the same sites built in memory
    /// ([`plumb_net::BucketTable::build`]). Written by the build that read
    /// records, before it moved onto items, and again when the bucket
    /// table's lines became stored items: only `records.dat` and
    /// `records.idx` changed then, each line reading back as the record it
    /// was. Never written again to make a test pass (see plumb-index's
    /// `tests/golden.rs`).
    #[test]
    fn a_build_of_the_golden_sites_makes_the_golden_buckets_and_feeds() {
        use sha2::{Digest, Sha256};
        use std::fmt::Write as _;

        let golden = include_str!("../../plumb-index/src/tests/golden.txt");
        let lines: Vec<&str> = golden
            .lines()
            .filter_map(|line| line.strip_prefix("line "))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        let outlines = outline(&path).unwrap().unwrap();
        assert_eq!(outlines.len(), lines.len());
        let buckets = dir.path().join("buckets");
        let built = build_index(
            &path,
            outlines,
            &dir.path().join("index"),
            Some(&buckets),
            1_000,
            None,
            None,
            &mut |_| Ok(()),
        )
        .unwrap();
        assert!(built.buckets);
        let table = dir.path().join("table");
        let records: Vec<SiteRecord> = lines
            .iter()
            .map(|line| stored::read(line.as_bytes()).unwrap())
            .collect();
        plumb_net::BucketTable::build(&table, &records).unwrap();

        let mut text = String::from(
            "# What a node's build makes of the sites of plumb-index's golden file\n\
             # (crates/plumb-index/src/tests/golden.txt) beside the index: the size\n\
             # and SHA-256 of each file of its buckets and of a bucket table of the\n\
             # same sites built in memory, and the feeds it watches. Written by\n\
             # `PLUMB_WRITE_GOLDEN=1 cargo test -p plumb-node --lib golden`; see\n\
             # plumb-index's tests/golden.rs before writing it again.\n",
        );
        writeln!(text, "docs {}", built.docs).unwrap();
        for (name, dir) in [("buckets", &buckets), ("table", &table)] {
            for file in ["records.dat", "records.idx", "buckets.idx", "buckets.dat"] {
                let bytes = fs::read(dir.join(file)).unwrap();
                let hash: String = Sha256::digest(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                writeln!(text, "{name} {file} {} bytes {hash}", bytes.len()).unwrap();
            }
        }
        for feed in &built.feeds {
            writeln!(text, "feed {}", serde_json::to_string(feed).unwrap()).unwrap();
        }
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/outline_golden.txt");
        check_golden(&path, &text);
    }

    #[test]
    fn a_build_reads_records_clean_as_loading_makes_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        // Decomposed é, and a URL ending in a bare `?`.
        let mut cafe = crawled("cafe.com", "Cafe\u{301}", 5);
        cafe.url = Some("https://cafe.com/?".into());
        // A homepage read on another site's host, which says nothing of
        // this one.
        let mut elsewhere = crawled("bank.com", "Google", 5);
        elsewhere.url = Some("https://www.google.com/".into());
        write_jsonl(&path, &[cafe, elsewhere]).unwrap();
        let outlines = outline(&path).unwrap().unwrap();
        let read = read_all(&path, &outlines);
        let cafe = read.iter().find(|r| r.domain == "cafe.com").unwrap();
        assert_eq!(cafe.title.as_deref(), Some("Caf\u{e9}"));
        assert_eq!(cafe.url.as_deref(), Some("https://cafe.com/"));
        let bank = read.iter().find(|r| r.domain == "bank.com").unwrap();
        assert_eq!((&bank.url, &bank.title), (&None, &None));
        assert_eq!(read, load_records(&path).unwrap().into_sorted_vec());
    }

    #[test]
    fn invalid_domains_and_blank_lines_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let line = |r: &SiteRecord| serde_json::to_string(r).unwrap();
        fs::write(
            &path,
            format!(
                "{}\n\n{}\r\n{}",
                line(&record("a.com", 1)),
                line(&record("not a domain", 2)),
                line(&crawled("b.com", "B", 5))
            ),
        )
        .unwrap();
        let outlines = outline(&path).unwrap().unwrap();
        let read = read_all(&path, &outlines);
        assert_eq!(read, load_records(&path).unwrap().into_sorted_vec());
    }

    #[test]
    fn a_site_at_a_time_is_each_line_s_record_as_its_stored_item() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let line = |r: &SiteRecord| serde_json::to_string(r).unwrap();
        let item = String::from_utf8(stored::line(&crawled("b.com", "B", 5)).unwrap()).unwrap();
        // Records not yet clean, under a domain not canonical, twice, and
        // under one that is none; a stored item; a blank line.
        let mut untidy = crawled("Cafe.COM", " Cafe\u{301}\u{7} ", 5);
        untidy.url = Some("HTTPS://WWW.Cafe.com/index%2ehtml?".into());
        untidy.aliases = vec![" Cafe".into(), String::new()];
        untidy.crawl_failures = 2;
        untidy.gone_at = Some(9);
        let lines = [
            line(&untidy),
            line(&record("cafe.com", 1)),
            String::new(),
            item,
            line(&record("not a domain", 2)),
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let mut records = Vec::new();
        for_each_record(&path, |record| records.push(record)).unwrap();
        let mut sites = Vec::new();
        for_each_site(&path, |site| sites.push(site)).unwrap();
        assert_eq!(sites.len(), 4);
        for (site, record) in sites.iter().zip(&records) {
            assert_eq!(site::domain(site), record.domain);
            assert_eq!(site::link_score(site), record.link_score());
            assert_eq!(site.title, record.title);
            assert_eq!(site::crawl_marks(site).gone, record.gone_at);
            assert_eq!(
                stored::item_as_line(site).unwrap(),
                stored::line(record).unwrap()
            );
        }
        fs::write(&path, "{\"domain\":\"a.com\"}\nnot json\n").unwrap();
        let err = for_each_site(&path, |_| {}).unwrap_err();
        assert!(format!("{err:#}").contains("records.jsonl:2"), "{err:#}");
    }

    #[test]
    fn the_sites_a_step_keeps_are_the_ones_it_kept_as_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let line = |r: &SiteRecord| serde_json::to_string(r).unwrap();
        // About banks with an untidy title, through a link text or a
        // name, or not at all; an official website; a bank's page read on
        // another site, which counts for nothing; in both shapes.
        let mut untidy = crawled("tidy.com", " Cafe\u{301}\u{7}  Bank ", 5);
        untidy.crawl_failures = 3;
        let mut named = record("named.com", 9);
        named.aliases = vec!["First Bank".into()];
        let mut official = record("official.org", 4);
        official.signals.official_site = true;
        let mut elsewhere = crawled("elsewhere.com", "Bank", 6);
        elsewhere.url = Some("https://other.org/".into());
        elsewhere.link_texts.clear();
        let item = |r: SiteRecord| String::from_utf8(stored::line(&r).unwrap()).unwrap();
        let lines = [
            line(&untidy),
            item(crawled("linked.com", "Savings bank", 7)),
            line(&named),
            item(official),
            line(&elsewhere),
            item(crawled("quiet.net", "Gardening", 8)),
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        // How the steps judged each site when they read records.
        let records_where = |wanted: &dyn Fn(&SiteRecord) -> bool| {
            let mut domains = HashSet::new();
            for_each_record(&path, |mut record| {
                record.clean();
                record.drop_bot_check();
                if wanted(&record) {
                    domains.insert(record.domain.into_boxed_str());
                }
            })
            .unwrap();
            domains
        };
        let topics = crate::about::Topics::new(&["bank".to_string()]);
        let about = domains_where(&path, |site| topics.matches_site(site)).unwrap();
        assert_eq!(about, records_where(&|record| topics.matches(record)));
        let mut sorted: Vec<&str> = about.iter().map(|domain| &**domain).collect();
        sorted.sort_unstable();
        assert_eq!(sorted, ["linked.com", "named.com", "tidy.com"]);
        let official = domains_where(&path, |site| site::signals(site).official_site).unwrap();
        let official_records = records_where(&|record| record.signals.official_site);
        assert_eq!(official, official_records);
        assert_eq!(official.len(), 1);
    }

    #[test]
    fn replacing_lines_copies_them_as_the_file_holds_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let line = |r: &SiteRecord| serde_json::to_string(r).unwrap();
        let item = String::from_utf8(stored::line(&crawled("b.com", "B", 5)).unwrap()).unwrap();
        // A record not yet clean, a stored item, a line break of two bytes
        // and a blank line: copied as they are.
        let raw = line(&crawled("cafe.com", "Cafe\u{301}", 5));
        fs::write(
            &path,
            format!(
                "{}\n\n{raw}\r\n{item}\n{}\n",
                line(&record("a.com", 1)),
                line(&record("quiet.net", 9))
            ),
        )
        .unwrap();
        RecordStore::open(&path)
            .save(&[Change::Mark {
                domain: "a.com".into(),
                attempted_at: None,
                failures: 0,
            }])
            .unwrap();
        // The journal is folded in first, which writes each line clean.
        let outlines = outline(&path).unwrap().unwrap();
        let folded = fs::read_to_string(&path).unwrap();
        let mut kept: Vec<Outline> = outlines
            .into_iter()
            .filter(|o| &*o.domain != "quiet.net")
            .collect();
        kept.reverse();
        let budget = plumb_core::storage::StorageBudget::open(dir.path(), u64::MAX).unwrap();
        assert_eq!(replace_lines(&path, &kept, Some(&budget)).unwrap(), 3);
        let lines: Vec<&str> = folded.lines().collect();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{}\n{}\n{}\n", lines[2], lines[1], lines[0])
        );
        assert_eq!(budget.status().reserved_bytes, 0);
        assert_eq!(
            budget.status().used_bytes,
            plumb_core::storage::directory_bytes(dir.path()).unwrap()
        );

        // With no journal, the lines are copied as they are, whatever their
        // shape or line break, and a journal left meanwhile goes.
        fs::write(
            &path,
            format!("{}\n\n{raw}\r\n{item}\n", line(&record("a.com", 1))),
        )
        .unwrap();
        let mut outlines = outline(&path).unwrap().unwrap();
        fs::write(journal_path(&path), "").unwrap();
        outlines.reverse();
        replace_lines(&path, &outlines, None).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{item}\n{raw}\n{}\n", line(&record("a.com", 1)))
        );
        assert!(!journal_path(&path).exists());
        assert!(!temp_path_for(&path).exists());
    }
}
