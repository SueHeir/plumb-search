//! Building the site index from site records: one document a site,
//! records for the same domain merged and redirecting domains folded into
//! the site they redirect to.

use std::borrow::Borrow;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use plumb_core::{canonical_domain, AdultLevel, SiteRecord};
use tantivy::directory::Directory;
use tantivy::merge_policy::NoMergePolicy;
use tantivy::{Index, IndexWriter};

use crate::replace::Staging;
use crate::schema::Fields;
use crate::{analysis, schema, spell, spell_model, storage, IndexStats};

/// Memory budget of the index writer, shared by its threads. Enough for a
/// million records without flushing tiny segments.
const WRITER_HEAP_BYTES: usize = 200_000_000;

/// Builds a fresh index of `records` in `dir`, replacing any index already
/// there.
///
/// Each record's domain is made canonical with
/// [`plumb_core::canonical_domain`] (`Example.com` -> `example.com`,
/// `münchen.de` -> `xn--mnchen-3ya.de`, `www.example.com` ->
/// `example.com`), and records that end up with the same domain are merged
/// into one document with [`SiteRecord::merge`], as
/// [`plumb_core::RecordSet`] would. Records whose domain is not a valid
/// registrable domain are skipped. The returned [`IndexStats`] counts both.
///
/// The index is built in a hidden directory next to `dir` and swapped in
/// only once it is complete, so a failed build leaves the previous index in
/// place and removes its own leftovers. A `Searcher` opened before keeps
/// serving the previous index (on Unix; elsewhere the swap may fail while
/// it is open).
///
/// Replacing deletes everything in `dir`, so to protect other data `dir`
/// must be missing, empty or an index: every index built here holds a
/// marker file, `.plumb-index`, and a directory without one is replaced
/// only when it holds a Tantivy index and nothing else (as indexes built
/// before the marker do). Anything else is refused with an error naming
/// `dir`. If `dir` is a symlink, the directory it points to is replaced.
/// Its parent must be writable, and `dir` itself cannot be a mount point
/// (mount the parent).
///
/// `records` may be the records themselves or references to them, so a
/// caller holding a [`plumb_core::RecordSet`] need not copy it into a list.
pub fn build_index<R: Borrow<SiteRecord>>(dir: &Path, records: &[R]) -> Result<IndexStats> {
    build_index_with_budget(dir, records, None)
}

pub fn build_index_with_budget<R: Borrow<SiteRecord>>(
    dir: &Path,
    records: &[R],
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
) -> Result<IndexStats> {
    let (sites, mut stats) = merge_by_domain(records);
    let (sites, redirect_names) = fold_redirects(sites);
    stats.redirected = stats.docs - sites.len() as u64;
    stats.docs = sites.len() as u64;
    let mut build = IndexBuild::new_with_budget(dir, budget)?;
    for (i, site) in sites.iter().enumerate() {
        let names = redirect_names.get(&i).map_or(&[][..], Vec::as_slice);
        build.add(site, names)?;
    }
    build.finish()?;
    Ok(stats)
}

/// An index built one site at a time, for a caller that cannot hold every
/// record at once: a node reads its records file a record at a time
/// instead. [`build_index`] does the rest for records in memory: merging
/// records of one domain and folding redirects ([`redirect_targets`]), which
/// a caller of this does itself.
///
/// The index is built in a hidden directory next to `dir`, and replaces
/// what is there only on [`IndexBuild::finish`], as [`build_index`] says.
pub struct IndexBuild {
    writer: IndexWriter,
    index: Index,
    fields: Fields,
    spelling: spell_model::ModelBuilder,
    // Close indexing workers before deleting and releasing a failed stage.
    staging: Staging,
}

impl IndexBuild {
    /// Starts a build that replaces the index in `dir` when finished,
    /// counting what it writes against `budget`.
    pub fn new_with_budget(
        dir: &Path,
        budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
    ) -> Result<IndexBuild> {
        let staging = Staging::new_with_budget(dir, budget.clone())?;
        let schema = schema::schema();
        let fields = Fields::new(&schema)?;
        let index = storage::create_index(staging.path(), schema, budget, staging.lifecycle())
            .with_context(|| format!("creating index in {}", staging.path().display()))?;
        analysis::register(index.tokenizers());
        let writer: IndexWriter = index
            .writer(WRITER_HEAP_BYTES)
            .context("opening index writer")?;
        // Merge once at the end instead of while indexing.
        writer.set_merge_policy(Box::new(NoMergePolicy));
        Ok(IndexBuild {
            staging,
            index,
            writer,
            fields,
            spelling: spell_model::ModelBuilder::new(analysis::words_analyzer()),
        })
    }

    /// Adds `site`, whose domain is canonical and not added before, with
    /// the names other sites give it by redirecting to it
    /// ([`redirect_name`]).
    pub fn add(&mut self, site: &SiteRecord, redirect_names: &[String]) -> Result<()> {
        self.writer
            .add_document(schema::document(&self.fields, site, redirect_names))?;
        // Adult sites' words are never offered as completions.
        if plumb_core::record_adult_level(site) == AdultLevel::None {
            let texts = schema::spelling_texts(site, redirect_names);
            self.spelling.add(texts.iter().map(String::as_str));
        }
        Ok(())
    }

    /// Writes the index: one commit, then a merge into a single segment,
    /// and puts it in place.
    pub fn finish(mut self) -> Result<()> {
        self.writer.commit().context("committing index")?;
        fail_point("after_commit")?;
        // The index is read-only from here on, and one segment searches fastest.
        let segments = self.index.searchable_segment_ids()?;
        if segments.len() > 1 {
            self.writer
                .merge(&segments)
                .wait()
                .context("merging index segments")?;
        }
        self.writer
            .wait_merging_threads()
            .context("finishing index merges")?;
        // The spelling model learns from the words just indexed.
        let reader = self.index.reader().context("reading the new index")?;
        let model = self
            .spelling
            .finish(&reader.searcher(), &spell::word_fields(&self.fields))?;
        drop(reader);
        if self.staging.budget().is_some() {
            let mut out = self
                .index
                .directory()
                .open_write(Path::new(spell_model::MODEL_FILE))?;
            model.write(&mut out)?;
            tantivy::directory::TerminatingWrite::terminate(out)?;
        } else {
            model.save(&self.staging.path().join(spell_model::MODEL_FILE))?;
        }
        self.staging.install()
    }
}

/// The name a site whose homepage redirects to another site gives that
/// site: its domain label, as "pnc bank" for `pncbank.com`.
pub fn redirect_name(domain: &str) -> String {
    schema::label_text(domain)
}

/// For each of `sites`, by position, the site it is folded into: the one
/// its homepage redirects to, when that is another of `sites` and does not
/// redirect itself (one step only). `domain` gives a site's canonical
/// domain and `redirect_to` the canonical domain its homepage redirects to
/// (an empty one when it redirects to no valid domain: no target, but it
/// still redirects).
/// A site folded into another is left out of the index, and its
/// [`redirect_name`] names the other.
pub fn redirect_targets<T>(
    sites: &[T],
    domain: impl Fn(&T) -> &str,
    redirect_to: impl Fn(&T) -> Option<String>,
) -> Vec<Option<usize>> {
    let positions: HashMap<&str, usize> = sites
        .iter()
        .enumerate()
        .map(|(i, site)| (domain(site), i))
        .collect();
    sites
        .iter()
        .map(|site| {
            let to = redirect_to(site).filter(|to| !to.is_empty())?;
            let &target = positions.get(to.as_str())?;
            (redirect_to(&sites[target]).is_none() && domain(&sites[target]) != domain(site))
                .then_some(target)
        })
        .collect()
}

/// A site to index: one of the records given, or a copy where records had
/// to be merged or their domain changed. Two words, where a
/// `Cow<SiteRecord>` would take the size of a whole record for each of a
/// million sites even when it only borrows.
enum Site<'a> {
    Borrowed(&'a SiteRecord),
    Owned(Box<SiteRecord>),
}

impl Site<'_> {
    fn to_mut(&mut self) -> &mut SiteRecord {
        if let Site::Borrowed(record) = *self {
            *self = Site::Owned(Box::new(record.clone()));
        }
        match self {
            Site::Owned(record) => record,
            Site::Borrowed(_) => unreachable!("made owned above"),
        }
    }
}

impl std::ops::Deref for Site<'_> {
    type Target = SiteRecord;

    fn deref(&self) -> &SiteRecord {
        match self {
            Site::Borrowed(record) => record,
            Site::Owned(record) => record,
        }
    }
}

/// One record per canonical domain ([`canonical_domain`]), in the order the
/// domains first appear: records for the same domain are merged
/// ([`SiteRecord::merge`]) and records without a valid domain are skipped.
/// Records that are already canonical and unique are not copied.
fn merge_by_domain<R: Borrow<SiteRecord>>(records: &[R]) -> (Vec<Site<'_>>, IndexStats) {
    let mut sites: Vec<Site> = Vec::with_capacity(records.len());
    let mut positions: HashMap<String, usize> = HashMap::with_capacity(records.len());
    let mut stats = IndexStats::default();
    for record in records {
        let record: &SiteRecord = record.borrow();
        let Some(domain) = canonical_domain(&record.domain).filter(|_| record.gone_at.is_none())
        else {
            stats.skipped += 1;
            continue;
        };
        let with_domain = |domain: &String| {
            let mut record = record.clone();
            record.domain.clone_from(domain);
            record
        };
        match positions.entry(domain) {
            Entry::Occupied(entry) => {
                let other = with_domain(entry.key());
                sites[*entry.get()].to_mut().merge(other);
                stats.merged += 1;
            }
            Entry::Vacant(entry) => {
                let site = if record.domain == *entry.key() {
                    Site::Borrowed(record)
                } else {
                    Site::Owned(Box::new(with_domain(entry.key())))
                };
                entry.insert(sites.len());
                sites.push(site);
            }
        }
    }
    stats.docs = sites.len() as u64;
    (sites, stats)
}

/// Leaves out the sites whose homepage redirects to another site in
/// `sites` (`pncbank.com` -> `pnc.com`): they are that site under another
/// name. Their domain labels become names of the site they redirect to,
/// returned by its position in the sites kept, so "pnc bank" names pnc.com.
fn fold_redirects(sites: Vec<Site<'_>>) -> (Vec<Site<'_>>, HashMap<usize, Vec<String>>) {
    let targets = redirect_targets(
        &sites,
        |site| site.domain.as_str(),
        |site| {
            let to = &site.redirect.as_ref()?.to;
            Some(canonical_domain(to).unwrap_or_default())
        },
    );
    let mut kept_at = vec![None; sites.len()];
    let mut kept = 0;
    for (i, target) in targets.iter().enumerate() {
        if target.is_none() {
            kept_at[i] = Some(kept);
            kept += 1;
        }
    }
    let mut names: HashMap<usize, Vec<String>> = HashMap::new();
    for (i, target) in targets.iter().enumerate() {
        if let Some(target) = target.and_then(|t| kept_at[t]) {
            names
                .entry(target)
                .or_default()
                .push(redirect_name(&sites[i].domain));
        }
    }
    let sites = sites
        .into_iter()
        .zip(&targets)
        .filter(|(_, target)| target.is_none())
        .map(|(site, _)| site)
        .collect();
    (sites, names)
}

#[cfg(test)]
thread_local! {
    /// The step at which [`fail_point`] makes the build fail on this thread.
    pub(crate) static FAIL_AT: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// Fails at `step` when a test asked for it, so tests can break a build
/// halfway. Does nothing outside tests.
#[cfg(test)]
fn fail_point(step: &str) -> Result<()> {
    if FAIL_AT.get() == Some(step) {
        anyhow::bail!("injected failure at {step}");
    }
    Ok(())
}

#[cfg(not(test))]
fn fail_point(_step: &str) -> Result<()> {
    Ok(())
}
