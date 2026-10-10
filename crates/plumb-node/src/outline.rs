//! Records files read a record at a time, for nodes that cannot hold every
//! record in memory: a million sites take over a gigabyte as a
//! [`RecordSet`], about twice the size of the file.
//!
//! [`outline`] folds the journal into the records file the way
//! [`crate::records::load_records`] replays it, one record at a time, and
//! returns an [`Outline`] of each site: where its line is in the file and
//! the little an index build needs to put the sites in order and fold
//! redirects. [`RecordReader`] then reads the records back one at a time.
//!
//! Only the journal's changes are held in memory meanwhile, grouped by the
//! site they change. A change to a site on a subdomain
//! ([`Change::SubdomainSite`]) takes its parent's ranks: the parent's as the
//! journal leaves it, where replaying the journal into a whole set takes them
//! as they are when the change comes (ranks come from seed data, which a
//! journal rarely changes twice).
//!
//! A file that two lines hold the same site in, or a domain that is not
//! canonical, is something only a whole set can merge: [`outline`] returns
//! `None` for it, and the caller loads the set once instead.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use plumb_core::{canonical_domain, parent_domain, RecordSet, Redirect, Signals, SiteRecord};
use serde::Deserialize;
use tracing::{info, warn};

use crate::records::{journal_path, Change};
use crate::{sync_parent_dir, temp_path_for};

/// What a build needs to know of a site before it reads its record.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outline {
    /// Where the record's line starts in the records file.
    pub(crate) offset: u64,
    /// The length of the line, without its line break.
    pub(crate) len: u32,
    /// [`SiteRecord::link_score`].
    pub(crate) score: f32,
    /// The canonical domain.
    pub(crate) domain: Box<str>,
    /// The canonical domain the homepage redirects to; empty when it
    /// redirects to no valid domain, as [`plumb_index::redirect_targets`]
    /// takes it.
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
                .map(|redirect| redirect_target(redirect).into()),
            gone: record.gone_at.is_some(),
        }
    }
}

fn redirect_target(redirect: &Redirect) -> String {
    canonical_domain(&redirect.to).unwrap_or_default()
}

/// The fields of a record [`outline`] reads when it has no journal to fold
/// in; the rest of the line is skipped.
#[derive(Deserialize)]
struct Head {
    domain: String,
    #[serde(default)]
    signals: Signals,
    #[serde(default)]
    redirect: Option<Redirect>,
    #[serde(default)]
    gone_at: Option<u64>,
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
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("opening {}", path.display())),
        };
        let mut pending = Pending::default();
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut line = Vec::new();
        let (mut damaged, mut changes) = (0usize, 0usize);
        loop {
            line.clear();
            if reader
                .read_until(b'\n', &mut line)
                .with_context(|| format!("reading {}", path.display()))?
                == 0
            {
                break;
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
    fn parent_ranks(&self, path: &Path) -> Result<HashMap<String, Signals>> {
        let mut ranks = HashMap::new();
        if self.parents.is_empty() {
            return Ok(ranks);
        }
        let mut lines = Lines::open(path)?;
        while let Some((_, line)) = lines.next()? {
            let Ok(head) = serde_json::from_slice::<Head>(line) else {
                continue;
            };
            if !self.parents.contains(&head.domain) || ranks.contains_key(&head.domain) {
                continue;
            }
            let record: SiteRecord = serde_json::from_slice(line)
                .with_context(|| format!("{}: invalid JSON line", path.display()))?;
            if let Some(signals) = self.parent_signals(&head.domain, Some(record)) {
                ranks.insert(head.domain, signals);
            }
        }
        for parent in &self.parents {
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
        Some(pending) => Ok(fold(path, &journal, pending, true)?.map(|(_, outlines)| outlines)),
    }
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
/// [`outline`] does, without outlining the sites.
pub(crate) fn fold_journal(path: &Path) -> Result<Folded> {
    let journal = journal_path(path);
    let Some(pending) = Pending::read(&journal)? else {
        return Ok(Folded::Nothing);
    };
    Ok(match fold(path, &journal, pending, false)? {
        Some((count, _)) => Folded::Records(count),
        None => Folded::NeedsSet,
    })
}

/// Calls `each` with every record of the records file at `path`, in file
/// order, as the file holds it: the journal is not replayed.
pub(crate) fn for_each_record(path: &Path, mut each: impl FnMut(SiteRecord)) -> Result<()> {
    let mut lines = Lines::open(path)?;
    let mut line_no = 0usize;
    while let Some((_, line)) = lines.next()? {
        line_no += 1;
        let record: SiteRecord = serde_json::from_slice(line)
            .with_context(|| format!("{}:{line_no}: invalid JSON line", path.display()))?;
        each(record);
    }
    Ok(())
}

/// [`outline`] of a file with no journal to fold in.
fn scan(path: &Path) -> Result<Option<Vec<Outline>>> {
    let mut lines = Lines::open(path)?;
    let mut outlines = Vec::new();
    let mut seen = HashSet::new();
    let mut line_no = 0usize;
    while let Some((offset, line)) = lines.next()? {
        line_no += 1;
        let head: Head = serde_json::from_slice(line)
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
                .map(|redirect| redirect_target(redirect).into()),
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
) -> Result<Option<(usize, Vec<Outline>)>> {
    let tmp = temp_path_for(path);
    let written = write_folded(path, pending, &tmp, outlines);
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
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("moving {} to {}", tmp.display(), path.display()));
    }
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
    mut pending: Pending,
    out: &Path,
    outlines: bool,
) -> Result<Option<(usize, Vec<Outline>)>> {
    let ranks = pending.parent_ranks(path)?;
    let mut written = Written::create(out, outlines)?;
    let mut lines = Lines::open(path)?;
    let mut seen = HashSet::new();
    let mut line_no = 0usize;
    while let Some((_, line)) = lines.next()? {
        line_no += 1;
        let record: SiteRecord = serde_json::from_slice(line)
            .with_context(|| format!("{}:{line_no}: invalid JSON line", path.display()))?;
        let Some(domain) = canonical_domain(&record.domain) else {
            continue;
        };
        if domain != record.domain || !seen.insert(domain.clone()) {
            return Ok(None);
        }
        if let Some(record) = pending.apply(&domain, Some(record), &ranks) {
            written.write(&record)?;
        }
    }
    // Sites the file does not hold yet, in a fixed order.
    let mut rest: Vec<String> = pending.ops.keys().cloned().collect();
    rest.sort_unstable();
    for site in rest {
        if let Some(record) = pending.apply(&site, None, &ranks) {
            // A change may name a site the file holds under its canonical
            // domain only through another change.
            if seen.insert(record.domain.clone()) {
                written.write(&record)?;
            }
        }
    }
    written.finish().map(Some)
}

/// [`outline`] for a reader that must not change the records file at
/// `path`, such as `plumb index` next to a running node: the file with its
/// journal folded in is written to `copy` (when there is a journal), and
/// the outlines are of `copy` then. Returns the file to read the records
/// from with them; `None` as [`outline`] does.
pub(crate) fn outline_copy<'a>(
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
pub(crate) enum Step {
    /// Reading `sites` records, of which `docs` go in the index.
    Started { docs: usize, sites: usize },
    /// `done` of the `sites` read.
    Read { done: usize, sites: usize },
    /// Every record read; the index is being written out.
    Writing,
}

/// What [`build_index`] made.
pub(crate) struct Built {
    /// Sites in the index.
    pub(crate) docs: usize,
    /// Whether the buckets were written where asked.
    pub(crate) buckets: bool,
    /// The domain and homepage of the best-ranked sites whose feeds to
    /// watch ([`wants_feed`]), as many as asked for.
    pub(crate) feeds: Vec<(String, String)>,
}

/// Records read between calls of [`build_index`]'s `step`.
const STEP_EVERY: usize = 10_000;

/// Builds an index in `index_dir` of the records file at `path`, whose
/// sites `outlines` gives ([`outline`]), reading a record at a time: what
/// [`plumb_index::build_index`] makes of the whole set, best link score
/// first, without the sites judged dead, and with sites whose homepage
/// redirects to another folded into it. With `buckets`, also writes the
/// buckets of the same sites there ([`plumb_net::BucketWriter`]); a failure
/// there is logged and leaves the index without. `step` is called as the
/// build goes; an error from it stops the build.
pub(crate) fn build_index(
    path: &Path,
    mut outlines: Vec<Outline>,
    index_dir: &Path,
    buckets: Option<&Path>,
    feeds: usize,
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
    step: &mut dyn FnMut(Step) -> Result<()>,
) -> Result<Built> {
    outlines.retain(|outline| !outline.gone);
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
    let mut reader = RecordReader::open(path)?;
    for (i, outline) in outlines.iter().enumerate() {
        if i > 0 && i % STEP_EVERY == 0 {
            step(Step::Read { done: i, sites })?;
        }
        let record = reader.read(outline)?;
        if let (Some(bucket_writer), Some(dir)) = (writer.as_mut(), buckets) {
            if let Err(err) = bucket_writer.add(&record) {
                writer = None;
                drop_buckets(dir, &err);
            }
        }
        if watched.len() < feeds && wants_feed(&record) {
            watched.push(feed_of(&record));
        }
        if folded_into[i].is_none() {
            let names = redirect_names.get(&i).map_or(&[][..], Vec::as_slice);
            index.add(&record, names)?;
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
        docs,
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

/// Whether a node watches the feed of `record`'s site, among its
/// best-ranked: it answered its last crawl and redirects nowhere.
pub(crate) fn wants_feed(record: &SiteRecord) -> bool {
    record.redirect.is_none() && record.crawl_failures == 0
}

/// The domain and homepage of `record`, for [`crate::news`].
pub(crate) fn feed_of(record: &SiteRecord) -> (String, String) {
    let homepage = record
        .url
        .clone()
        .unwrap_or_else(|| format!("https://{}/", record.domain));
    (record.domain.clone(), homepage)
}

/// A records file being written, with an outline of each record when
/// asked for.
struct Written {
    out: BufWriter<File>,
    at: u64,
    line: Vec<u8>,
    outlines: Vec<Outline>,
    outline: bool,
    count: usize,
}

impl Written {
    fn create(path: &Path, outline: bool) -> Result<Written> {
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Written {
            out: BufWriter::with_capacity(1 << 20, file),
            at: 0,
            line: Vec::new(),
            outlines: Vec::new(),
            outline,
            count: 0,
        })
    }

    fn write(&mut self, record: &SiteRecord) -> Result<()> {
        self.line.clear();
        serde_json::to_writer(&mut self.line, record)?;
        let len = u32::try_from(self.line.len()).context("a record line is too long")?;
        self.count += 1;
        if self.outline {
            self.outlines.push(Outline::of(record, self.at, len));
        }
        self.line.push(b'\n');
        self.out.write_all(&self.line)?;
        self.at += self.line.len() as u64;
        Ok(())
    }

    fn finish(self) -> Result<(usize, Vec<Outline>)> {
        let file = self.out.into_inner().map_err(|err| err.into_error())?;
        file.sync_all()?;
        Ok((self.count, self.outlines))
    }
}

/// Reads records of a file by their [`Outline`]s.
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

    /// The record `outline` points at, with page fields read off a bot
    /// check dropped, as loading the file drops them.
    pub(crate) fn read(&mut self, outline: &Outline) -> Result<SiteRecord> {
        if outline.offset != self.at {
            // Relative seeks within the buffer keep it when they can.
            let delta = i64::try_from(outline.offset).unwrap_or(i64::MAX)
                - i64::try_from(self.at).unwrap_or(i64::MAX);
            self.file.seek_relative(delta)?;
        }
        self.line.resize(outline.len as usize, 0);
        self.file.read_exact(&mut self.line)?;
        self.at = outline.offset + u64::from(outline.len);
        let mut record: SiteRecord = serde_json::from_slice(&self.line)
            .with_context(|| format!("reading the record of {}", outline.domain))?;
        if record.domain.as_str() != &*outline.domain {
            anyhow::bail!(
                "the records file changed while it was read: found {} where {} was",
                record.domain,
                outline.domain
            );
        }
        record.drop_bot_check();
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use plumb_core::{write_jsonl, LinkText};

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

    /// Every site in the records file at `path`, read a record at a time.
    fn read_all(path: &Path, outlines: &[Outline]) -> Vec<SiteRecord> {
        let mut reader = RecordReader::open(path).unwrap();
        let mut sorted = outlines.to_vec();
        sort(&mut sorted);
        sorted.iter().map(|o| reader.read(o).unwrap()).collect()
    }

    /// A records file and a journal that make every kind of change.
    fn file_and_journal(path: &Path) -> Vec<Change> {
        let mut parent = record("google.com", 3);
        parent.add_alias("Google Scholar");
        parent.about = Some("search engine".into());
        let mut redirecting = record("pncbank.com", 900);
        redirecting.redirect = Some(Redirect {
            to: "pnc.com".into(),
            at: 40,
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
}
