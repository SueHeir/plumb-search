//! Building the site index from sites as a node stores them, their stored
//! items ([`plumb_core::site`]): one document a site, items for the same
//! domain merged and redirecting domains folded into the site they
//! redirect to.

use std::borrow::Borrow;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use plumb_core::{canonical_domain, site, AdultLevel};
use plumb_format::Item;
use tantivy::directory::Directory;
use tantivy::merge_policy::NoMergePolicy;
use tantivy::{Index, IndexWriter};

use crate::replace::Staging;
use crate::schema::Fields;
use crate::{analysis, schema, spell, spell_model, storage, IndexStats};

/// Memory budget of the index writer, shared by its threads. Enough for a
/// million sites without flushing tiny segments.
const WRITER_HEAP_BYTES: usize = 200_000_000;

/// Builds a fresh index of `sites`, stored items of sites
/// ([`plumb_core::site`]), in `dir`, replacing any index already there.
///
/// Each site's domain is made canonical with
/// [`plumb_core::canonical_domain`] (`Example.com` -> `example.com`,
/// `münchen.de` -> `xn--mnchen-3ya.de`, `www.example.com` ->
/// `example.com`), and sites that end up with the same domain are merged
/// into one document with [`site::merge`], as
/// [`plumb_core::sites::Sites`] would. Sites whose domain is not a valid
/// registrable domain, and sites judged gone, are skipped. The returned
/// [`IndexStats`] counts both.
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
/// `sites` may be the items themselves or references to them, so a caller
/// holding them elsewhere need not copy them into a list.
pub fn build_index<S: Borrow<Item>>(dir: &Path, sites: &[S]) -> Result<IndexStats> {
    build_index_with_budget(dir, sites, None)
}

pub fn build_index_with_budget<S: Borrow<Item>>(
    dir: &Path,
    sites: &[S],
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
) -> Result<IndexStats> {
    let (sites, mut stats) = merge_by_domain(sites);
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
/// site at once: a node reads its records file a site at a time instead.
/// [`build_index`] does the rest for sites in memory: merging sites of one
/// domain and folding redirects ([`redirect_targets`]), which a caller of
/// this does itself.
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
        analysis::register_sites(index.tokenizers());
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

    /// Adds the site of the stored item `site` ([`plumb_core::site`]),
    /// whose domain is canonical and not added before, with the names other
    /// sites give it by redirecting to it ([`redirect_name`]).
    pub fn add(&mut self, site: &Item, redirect_names: &[String]) -> Result<()> {
        self.writer
            .add_document(schema::document(&self.fields, site, redirect_names))?;
        // Adult sites' words are never offered as completions.
        if site::adult_level(site) == AdultLevel::None {
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
/// domain and `redirect_to` the canonical domain of the site it is under
/// another name ([`plumb_core::Redirect::folds_into`]; an empty one when
/// it redirects to no valid domain: no target, but it still redirects).
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

/// A site to index: one of the items given, or a copy where items had to
/// be merged or their domain changed. Two words, where a `Cow<Item>` would
/// take the size of a whole item for each of a million sites even when it
/// only borrows.
enum Site<'a> {
    Borrowed(&'a Item),
    Owned(Box<Item>),
}

impl Site<'_> {
    fn to_mut(&mut self) -> &mut Item {
        if let Site::Borrowed(item) = *self {
            *self = Site::Owned(Box::new(item.clone()));
        }
        match self {
            Site::Owned(item) => item,
            Site::Borrowed(_) => unreachable!("made owned above"),
        }
    }
}

impl std::ops::Deref for Site<'_> {
    type Target = Item;

    fn deref(&self) -> &Item {
        match self {
            Site::Borrowed(item) => item,
            Site::Owned(item) => item,
        }
    }
}

/// One site per canonical domain ([`canonical_domain`]), in the order the
/// domains first appear: items for the same domain are merged
/// ([`site::merge`]), and items without a valid domain or of a site judged
/// gone are skipped. Items that are already canonical and unique are not
/// copied.
fn merge_by_domain<S: Borrow<Item>>(items: &[S]) -> (Vec<Site<'_>>, IndexStats) {
    let mut sites: Vec<Site> = Vec::with_capacity(items.len());
    let mut positions: HashMap<String, usize> = HashMap::with_capacity(items.len());
    let mut stats = IndexStats::default();
    for item in items {
        let item: &Item = item.borrow();
        let gone = site::crawl_marks(item).gone.is_some();
        let Some(domain) = canonical_domain(site::domain(item)).filter(|_| !gone) else {
            stats.skipped += 1;
            continue;
        };
        let with_domain = |domain: &String| {
            let mut item = item.clone();
            item.id.clone_from(domain);
            item
        };
        match positions.entry(domain) {
            Entry::Occupied(entry) => {
                let other = with_domain(entry.key());
                site::merge(sites[*entry.get()].to_mut(), other);
                stats.merged += 1;
            }
            Entry::Vacant(entry) => {
                let kept = if item.id == *entry.key() {
                    Site::Borrowed(item)
                } else {
                    Site::Owned(Box::new(with_domain(entry.key())))
                };
                entry.insert(sites.len());
                sites.push(kept);
            }
        }
    }
    stats.docs = sites.len() as u64;
    (sites, stats)
}

/// Leaves out the sites whose homepage redirects to another site in
/// `sites` that they are under another name (`pncbank.com` -> `pnc.com`;
/// see [`plumb_core::Redirect::folds_into`]). Their domain labels become
/// names of the site they redirect to, returned by its position in the
/// sites kept, so "pnc bank" names pnc.com.
fn fold_redirects(sites: Vec<Site<'_>>) -> (Vec<Site<'_>>, HashMap<usize, Vec<String>>) {
    let targets = redirect_targets(
        &sites,
        |kept| site::domain(kept),
        |kept| site::redirect(kept)?.folds_into(site::domain(kept)),
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
                .push(redirect_name(site::domain(&sites[i])));
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
