//! Replacing an index directory without ever leaving it missing or half built.
//!
//! A new index is written to a hidden staging directory next to the target
//! (same parent, so renames stay on one file system). Once it is complete,
//! it gets a marker file ([`MARKER`]), the old directory is renamed aside,
//! the new one renamed into place and the old one deleted. A failed build or
//! swap deletes the staging directory and leaves the old index where it was.
//!
//! Replacing deletes the old directory with everything in it, so only a
//! directory that is missing, empty or holds an index is replaced: one with
//! the marker, or one that opens as a Tantivy index and holds nothing but
//! Tantivy's own files, as indexes built before the marker do. Anything
//! else, such as a project folder that happens to contain a `meta.json`, is
//! refused.
//!
//! The swap is two renames, so a crash between them leaves both copies on
//! disk under their hidden names and nothing at the target. The next build
//! puts the old copy back first, and deletes what other crashed builds left
//! behind ([`tidy`]). That only reaches the leftovers of the directory being
//! built, so a program that is the only one to build in a directory also
//! calls [`remove_build_leftovers`] when it starts, for the leftovers of
//! directories it no longer builds. Renaming also means the target cannot be
//! a mount point; mount its parent instead.
//!
//! The hidden names carry the id of the process that made them, but an id
//! alone does not say whose a directory is: a node in a container has the
//! same id every time it starts. So a process keeps a list of the names it
//! uses ([`Claim`]), and only those count as its own.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tantivy::directory::{INDEX_WRITER_LOCK, META_LOCK};
use tantivy::Index;

/// The file that marks a directory as an index built by Plumb. Every
/// directory [`Staging::install`] puts in place has one.
pub(crate) const MARKER: &str = ".plumb-index";

/// The marker's first line, which identifies it. Later versions may add
/// lines after it but must keep it.
const MARKER_SIGNATURE: &str = "plumb-index";
/// The marker's contents: the signature, then a note for people who come
/// across the file.
const MARKER_TEXT: &str = "plumb-index\n\
    This directory holds a Plumb Search index. Rebuilding the index replaces\n\
    the whole directory, so do not keep other files in it.\n";

/// How many unexpected entries an error message names.
const STRAYS_SHOWN: usize = 3;

/// A directory being built next to the directory it will replace. It is
/// deleted when dropped, unless [`Staging::install`] moved it into place.
pub(crate) struct Staging {
    /// The directory being built.
    path: PathBuf,
    /// Where it goes once complete.
    target: PathBuf,
    installed: bool,
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
    lifecycle: Option<std::sync::Arc<plumb_core::storage::StageLifecycle>>,
    /// Keeps tidying from taking it for a leftover. Dropped after the
    /// directory is deleted or installed.
    _claim: Claim,
}

impl Staging {
    /// Checks that `dir` may be replaced, then creates an empty staging
    /// directory next to it (creating `dir`'s parents if needed).
    ///
    /// `dir` may be replaced when it is missing, empty or holds an index
    /// (see the module docs); a symlink is followed, so the directory it
    /// points to gets replaced.
    #[cfg(test)]
    pub(crate) fn new(dir: &Path) -> Result<Staging> {
        Self::new_with_budget(dir, None)
    }

    pub(crate) fn new_with_budget(
        dir: &Path,
        budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
    ) -> Result<Staging> {
        let held = budget.clone();
        let _mutation = held.as_ref().map(|budget| budget.mutation());
        let target = resolve(dir)?;
        dir_name(&target)?;
        tidy_with_budget(&target, budget.as_deref())?;
        check_replaceable(&target)?;
        let parent = parent_of(&target);
        plumb_core::storage::create_directory_locked(parent, budget.as_ref())
            .with_context(|| format!("creating {}", parent.display()))?;
        let mut reservation = budget
            .as_ref()
            .map(|budget| budget.reserve(plumb_core::storage::allocation_for(4096), false))
            .transpose()?;
        let parent_before = plumb_core::storage::file_bytes(parent)?;
        loop {
            let path = sibling(&target, "new")?;
            let claim = Claim::new(&path);
            match fs::create_dir(&path) {
                Ok(()) => {
                    if let Some(reserved) = &mut reservation {
                        reserved.commit(
                            reserved.bytes(),
                            0,
                            plumb_core::storage::file_bytes(&path)
                                .unwrap_or(plumb_core::storage::allocation_for(0))
                                .saturating_add(
                                    plumb_core::storage::file_bytes(parent)
                                        .unwrap_or(parent_before + 4096)
                                        .saturating_sub(parent_before),
                                ),
                        );
                    }
                    return Ok(Staging {
                        path,
                        target,
                        installed: false,
                        lifecycle: budget
                            .as_ref()
                            .map(|budget| plumb_core::storage::StageLifecycle::new(budget.clone())),
                        budget,
                        _claim: claim,
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| format!("creating {}", path.display()))
                }
            }
        }
    }

    /// The directory to build in.
    pub(crate) fn budget(&self) -> Option<&std::sync::Arc<plumb_core::storage::StorageBudget>> {
        self.budget.as_ref()
    }

    pub(crate) fn lifecycle(&self) -> Option<std::sync::Arc<plumb_core::storage::StageLifecycle>> {
        self.lifecycle.clone()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Marks the staging directory as an index ([`MARKER`]) and moves it
    /// into place, replacing what was there.
    pub(crate) fn install(mut self) -> Result<()> {
        if let Some(budget) = &self.budget {
            let path = self.path.join(MARKER);
            let mut file = plumb_core::storage::BudgetFile::create(&path, Some(budget.clone()))?;
            file.write_all(MARKER_TEXT.as_bytes())?;
            file.sync_all()?;
        } else {
            write_marker(&self.path)?;
        }
        {
            let held = self.budget.clone();
            let _mutation = held.as_ref().map(|budget| budget.mutation());
            let old = match plumb_core::storage::directory_bytes(&self.target) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
                Err(err) => return Err(err.into()),
            };
            if let Some(lifecycle) = &self.lifecycle {
                lifecycle.closed.store(true, Ordering::Release);
            }
            swap(&self.path, &self.target)?;
            if let Some(budget) = &self.budget {
                budget.removed(old);
            }
        }
        self.installed = true;
        Ok(())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.installed {
            // Best effort: a leftover only wastes space, the old index is intact.
            if let Some(lifecycle) = &self.lifecycle {
                lifecycle.remove_stage(&self.path);
            } else {
                let _ = fs::remove_dir_all(&self.path);
            }
        }
    }
}

/// Puts the directory `new` at `dir`: renames the current `dir` (if any)
/// aside, renames `new` in, then deletes the old directory. If `new` cannot
/// be renamed in, the old directory is renamed back.
fn swap(new: &Path, dir: &Path) -> Result<()> {
    let old = match fs::symlink_metadata(dir) {
        Ok(_) => {
            let old = unused_sibling(dir, "old")?;
            let claim = Claim::new(&old);
            if let Err(err) = fs::rename(dir, &old) {
                let busy = err.kind() == io::ErrorKind::ResourceBusy;
                let err =
                    anyhow::Error::new(err).context(format!("moving {} aside", dir.display()));
                if busy {
                    // Linux will not rename a mount point, such as a volume
                    // mounted right at the index directory.
                    return Err(err.context(format!(
                        "{} cannot be replaced by renaming; if it is a mount point, \
                         mount its parent directory instead",
                        dir.display()
                    )));
                }
                return Err(err);
            }
            Some((old, claim))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };
    if let Err(err) = fs::rename(new, dir) {
        let err =
            anyhow::Error::new(err).context(format!("moving the new index into {}", dir.display()));
        if let Some((old, _claim)) = old {
            if let Err(restore) = fs::rename(&old, dir) {
                return Err(err.context(format!(
                    "could not move the old index back from {} ({restore})",
                    old.display()
                )));
            }
        }
        return Err(err);
    }
    // The renames are entries of the parent: sync it before deleting the
    // old index, so a crash cannot bring the old name back without it.
    sync_dir(parent_of(dir));
    if let Some((old, _claim)) = old {
        fs::remove_dir_all(&old).with_context(|| {
            format!(
                "the new index is in {}, but the old one could not be deleted from {}",
                dir.display(),
                old.display()
            )
        })?;
    }
    Ok(())
}

/// How old a leftover of another process must be before it is deleted,
/// where it cannot be told whether that process still runs.
const LEFTOVER_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Cleans up after builds of `dir` that crashed: puts an old index back
/// when a crash between [`swap`]'s renames left nothing at `dir`, then
/// deletes staging and old directories that no running build still uses.
fn tidy_with_budget(dir: &Path, budget: Option<&plumb_core::storage::StorageBudget>) -> Result<()> {
    let name = dir_name(dir)?;
    clean_up(
        parent_of(dir),
        |target, pid, path| target == name && !may_be_in_use(pid, path),
        budget,
    )?;
    Ok(())
}

/// Cleans up after every build in `parent` that did not finish, whatever
/// directory it was building and whichever process made it: puts old
/// indexes back where a crash between [`swap`]'s renames left nothing, then
/// deletes the other staging and old directories. Returns the directories
/// deleted.
///
/// Only the builds of this process are left alone, so call it only where
/// no other process can be building in `parent`, such as a node's own data
/// directory while it holds the directory's lock. Unlike the cleanup each
/// build does, it does not need to tell whether another process still runs,
/// which a process id alone cannot (see the module docs).
pub fn remove_build_leftovers(parent: &Path) -> Result<Vec<PathBuf>> {
    clean_up(parent, |_, _, path| !Claim::held(path), None)
}

/// The staging and old directories in `parent` that `abandoned` picks, given
/// the name of the directory each was for, the id of the process that made
/// it and its path: puts the newest old copy that holds an index back where
/// its directory is missing, and deletes the others. Returns the
/// directories deleted; ones that cannot be deleted are left, as a leftover
/// only wastes space.
fn clean_up(
    parent: &Path,
    abandoned: impl Fn(&OsStr, u32, &Path) -> bool,
    budget: Option<&plumb_core::storage::StorageBudget>,
) -> Result<Vec<PathBuf>> {
    let mut leftovers: BTreeMap<OsString, Vec<(Tag, PathBuf)>> = BTreeMap::new();
    match fs::read_dir(parent) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some((target, tag, pid)) = parse_sibling(&name) else {
                    continue;
                };
                // Never followed: only directories are made with these names.
                if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let path = entry.path();
                if abandoned(target, pid, &path) {
                    let found = (tag, path);
                    leftovers.entry(target.to_owned()).or_default().push(found);
                }
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", parent.display())),
    }
    let mut removed = Vec::new();
    for (target, paths) in leftovers {
        let dir = parent.join(&target);
        if matches!(fs::symlink_metadata(&dir), Err(err) if err.kind() == io::ErrorKind::NotFound) {
            let old = paths
                .iter()
                .filter(|(tag, path)| *tag == Tag::Old && holds_index(path))
                .max_by_key(|(_, path)| fs::metadata(path).and_then(|m| m.modified()).ok());
            if let Some((_, old)) = old {
                fs::rename(old, &dir)
                    .with_context(|| format!("moving the old index back from {}", old.display()))?;
                sync_dir(parent);
            }
        }
        for (_, path) in paths {
            // Best effort, like dropping a [`Staging`].
            if fs::symlink_metadata(&path).is_ok() {
                let before = plumb_core::storage::directory_bytes(&path).ok();
                let result = fs::remove_dir_all(&path);
                if let (Some(budget), Some(before)) = (budget, before) {
                    let after = match plumb_core::storage::directory_bytes(&path) {
                        Ok(bytes) => bytes,
                        Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
                        Err(_) => before,
                    };
                    budget.removed(before.saturating_sub(after));
                }
                if result.is_ok() {
                    removed.push(path);
                }
            }
        }
    }
    Ok(removed)
}

/// Which of the two kinds of hidden directory a [`sibling`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    /// A new index being built ([`Staging`]).
    New,
    /// The index it replaces, moved aside during [`swap`].
    Old,
}

/// The parts of a [`sibling`]'s name: `.index.new-12-0` -> the name of the
/// directory it is for (`index`), its tag and the id of the process that
/// made it (12). `None` for any other name.
fn parse_sibling(name: &OsStr) -> Option<(&OsStr, Tag, u32)> {
    let name = name.to_str()?;
    let (target, rest) = name.strip_prefix('.')?.rsplit_once('.')?;
    let (tag, ids) = rest.split_once('-')?;
    let (pid, n) = ids.split_once('-')?;
    let tag = match tag {
        "new" => Tag::New,
        "old" => Tag::Old,
        _ => return None,
    };
    if target.is_empty() || n.parse::<u64>().is_err() {
        return None;
    }
    Some((OsStr::new(target), tag, pid.parse().ok()?))
}

/// Whether a running build may still use the [`sibling`] at `path`, made
/// by process `pid`: one this process holds a [`Claim`] on, or one made by
/// another process that still runs or, where that cannot be told, that
/// changed within [`LEFTOVER_AGE`].
///
/// A name with this process's id that it holds no claim on was made by an
/// earlier process with the same id, as a node in a container gets the same
/// id every time it starts.
fn may_be_in_use(pid: u32, path: &Path) -> bool {
    if pid == std::process::id() {
        return Claim::held(path);
    }
    if cfg!(target_os = "linux") {
        return is_running(pid);
    }
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map_or(true, |time| {
            time.elapsed().map_or(true, |age| age <= LEFTOVER_AGE)
        })
}

/// Whether a process with id `pid` runs, on Linux. `/proc` also has an
/// entry for every thread, under the thread's own id, so a thread whose id
/// matches does not count. When the entry cannot be read for another
/// reason than its absence, the process may run.
fn is_running(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(status) => status
            .lines()
            .find_map(|line| line.strip_prefix("Tgid:"))
            .and_then(|tgid| tgid.trim().parse::<u32>().ok())
            .is_none_or(|tgid| tgid == pid),
        Err(err) => err.kind() != io::ErrorKind::NotFound,
    }
}

/// The names of the [`sibling`]s this process uses now.
static CLAIMED: Mutex<Vec<OsString>> = Mutex::new(Vec::new());

/// A [`sibling`] this process uses, which tidying leaves alone until the
/// claim is dropped. Taken before the directory is made or renamed to that
/// name, and dropped after it is gone. A name is enough to tell, as
/// [`sibling`] never gives one twice within a process.
struct Claim(OsString);

impl Claim {
    fn new(path: &Path) -> Claim {
        let name = path.file_name().unwrap_or_default().to_owned();
        Claim::names().push(name.clone());
        Claim(name)
    }

    /// Whether this process holds a claim on the name of `path`.
    fn held(path: &Path) -> bool {
        path.file_name()
            .is_some_and(|name| Claim::names().iter().any(|held| held == name))
    }

    fn names() -> std::sync::MutexGuard<'static, Vec<OsString>> {
        CLAIMED.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut names = Claim::names();
        if let Some(at) = names.iter().position(|name| *name == self.0) {
            names.swap_remove(at);
        }
    }
}

/// Whether `dir` holds a complete index: marked, or a bare Tantivy index.
fn holds_index(dir: &Path) -> bool {
    has_marker(dir) || strays(dir).is_ok_and(|strays| strays.is_some_and(|s| s.is_empty()))
}

/// `dir` with symlinks resolved when it exists.
fn resolve(dir: &Path) -> Result<PathBuf> {
    match fs::canonicalize(dir) {
        Ok(real) => Ok(real),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(dir.to_path_buf()),
        Err(err) => Err(err).with_context(|| format!("resolving {}", dir.display())),
    }
}

/// Fails, naming `dir`, unless `dir` is missing, empty or holds an index:
/// it has the [`MARKER`], or it is a bare Tantivy index ([`strays`]).
fn check_replaceable(dir: &Path) -> Result<()> {
    match fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_none() || has_marker(dir) {
                return Ok(());
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    }
    let Some(strays) = strays(dir)? else {
        bail!(
            "refusing to replace {}: it is not empty and does not hold a Plumb search index; \
             choose a new or empty directory",
            dir.display()
        );
    };
    if !strays.is_empty() {
        let mut named = strays[..strays.len().min(STRAYS_SHOWN)].join(", ");
        if strays.len() > STRAYS_SHOWN {
            named += &format!(" and {} more", strays.len() - STRAYS_SHOWN);
        }
        bail!(
            "refusing to replace {}: besides a search index it holds {named}, which \
             rebuilding would delete; move other files out or choose another directory",
            dir.display()
        );
    }
    Ok(())
}

/// Whether `dir` holds a [`MARKER`] file.
fn has_marker(dir: &Path) -> bool {
    fs::read_to_string(dir.join(MARKER))
        .is_ok_and(|text| text.lines().next() == Some(MARKER_SIGNATURE))
}

/// Writes the [`MARKER`] file into `dir`, synced to disk like the index
/// files themselves.
fn write_marker(dir: &Path) -> Result<()> {
    let path = dir.join(MARKER);
    let mut file =
        fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    file.write_all(MARKER_TEXT.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", path.display()))
}

/// The entries of `dir` that are not part of a Tantivy index, sorted, or
/// `None` when `dir` does not open as one. Part of an index are regular
/// files named `meta.json`, `.managed.json`, a Tantivy lock file, the
/// spelling model ([`crate::spell_model::MODEL_FILE`]) or a file that
/// `.managed.json` lists.
fn strays(dir: &Path) -> Result<Option<Vec<String>>> {
    let Ok(index) = Index::open_in_dir(dir) else {
        return Ok(None);
    };
    let managed = index.directory().list_managed_files();
    let is_index_file = |name: &Path| {
        name == Path::new("meta.json")
            || name == Path::new(".managed.json")
            || name == INDEX_WRITER_LOCK.filepath.as_path()
            || name == META_LOCK.filepath.as_path()
            || name == Path::new(crate::spell_model::MODEL_FILE)
            || managed.contains(name)
    };
    let mut strays = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let name = entry.file_name();
        let is_file = entry.file_type().is_ok_and(|kind| kind.is_file());
        if !is_file || !is_index_file(Path::new(&name)) {
            strays.push(name.to_string_lossy().into_owned());
        }
    }
    strays.sort();
    Ok(Some(strays))
}

/// Syncs the entries of `dir` (renames into or out of it) to disk. Best
/// effort: some file systems cannot sync a directory, and elsewhere than
/// Unix a directory does not open as a file.
fn sync_dir(dir: &Path) {
    if cfg!(unix) {
        if let Ok(dir) = fs::File::open(dir) {
            let _ = dir.sync_all();
        }
    }
}

fn dir_name(dir: &Path) -> Result<&OsStr> {
    dir.file_name()
        .with_context(|| format!("{} does not name a directory", dir.display()))
}

fn parent_of(dir: &Path) -> &Path {
    match dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// A hidden path next to `dir`, new on every call within this process:
/// `index` -> `.index.<tag>-<pid>-<n>`.
fn sibling(dir: &Path, tag: &str) -> Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut name = OsString::from(".");
    name.push(dir_name(dir)?);
    name.push(format!(
        ".{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    Ok(parent_of(dir).join(name))
}

/// A sibling of `dir` (see [`sibling`]) that does not exist yet.
fn unused_sibling(dir: &Path, tag: &str) -> Result<PathBuf> {
    loop {
        let path = sibling(dir, tag)?;
        match fs::symlink_metadata(&path) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(path),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
            Ok(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use plumb_core::SiteRecord;
    use tempfile::TempDir;

    use super::*;

    /// The names in `dir`, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// A directory marked as an index, holding `file`.
    fn fake_index(dir: &Path, file: &str) {
        fs::create_dir_all(dir).unwrap();
        write_marker(dir).unwrap();
        fs::write(dir.join(file), file).unwrap();
    }

    /// The error `Staging::new(dir)` fails with, as text.
    fn refusal(dir: &Path) -> String {
        match Staging::new(dir) {
            Ok(_) => panic!("{} was accepted", dir.display()),
            Err(err) => format!("{err:#}"),
        }
    }

    #[test]
    fn syncing_a_directory_is_best_effort() {
        let root = TempDir::new().unwrap();
        sync_dir(root.path());
        sync_dir(&root.path().join("missing"));
        sync_dir(parent_of(Path::new("index")));
    }

    #[test]
    fn install_replaces_the_old_directory() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("index");
        fake_index(&dir, "old");
        let staging = Staging::new(&dir).unwrap();
        fs::write(staging.path().join("new"), "new").unwrap();
        assert_eq!(entries(root.path()).len(), 2);
        staging.install().unwrap();
        assert_eq!(entries(&dir), [MARKER, "new"]);
        assert_eq!(entries(root.path()), ["index"]);
    }

    #[test]
    fn install_creates_missing_directories_and_marks_the_index() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("a/b/index");
        let staging = Staging::new(&dir).unwrap();
        fs::write(staging.path().join("new"), "new").unwrap();
        staging.install().unwrap();
        assert_eq!(entries(&dir), [MARKER, "new"]);
        assert!(has_marker(&dir));
        assert_eq!(entries(&root.path().join("a/b")), ["index"]);
    }

    #[test]
    fn dropped_staging_leaves_nothing_behind() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("index");
        fake_index(&dir, "old");
        let staging = Staging::new(&dir).unwrap();
        fake_index(staging.path(), "half-built");
        drop(staging);
        assert_eq!(entries(root.path()), ["index"]);
        assert_eq!(entries(&dir), [MARKER, "old"]);
    }

    #[test]
    fn failed_swap_puts_the_old_directory_back() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("index");
        fake_index(&dir, "old");
        let err = swap(&root.path().join("missing"), &dir).unwrap_err();
        assert!(
            format!("{err:#}").contains("moving the new index"),
            "{err:#}"
        );
        assert_eq!(entries(&dir), [MARKER, "old"]);
        assert_eq!(entries(root.path()), ["index"]);
    }

    #[test]
    fn refuses_what_is_not_an_index() {
        let root = TempDir::new().unwrap();
        let data = root.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("notes.txt"), "keep me").unwrap();
        let err = refusal(&data);
        assert!(err.contains("refusing to replace"), "{err}");
        assert!(err.contains("data: it is not empty"), "{err}");
        // A `meta.json` alone does not make an index.
        fs::write(data.join("meta.json"), "{}").unwrap();
        assert!(refusal(&data).contains("refusing to replace"));
        // Nor does a file named like the marker that is not one.
        fs::write(data.join(MARKER), "my own notes").unwrap();
        assert!(refusal(&data).contains("refusing to replace"));

        let file = root.path().join("file");
        fs::write(&file, "keep me too").unwrap();
        assert!(Staging::new(&file).is_err());
        assert!(Staging::new(Path::new("/")).is_err());
        assert_eq!(entries(root.path()), ["data", "file"]);
        assert_eq!(entries(&data), [MARKER, "meta.json", "notes.txt"]);
        assert_eq!(
            fs::read_to_string(data.join("notes.txt")).unwrap(),
            "keep me"
        );
    }

    #[test]
    fn unmarked_indexes_must_hold_only_tantivy_files() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("index");
        crate::build_index(&dir, &[SiteRecord::new("example.com")]).unwrap();
        // Like an index built before the marker: Tantivy's files only, its
        // segment files among them (listed in `.managed.json`).
        fs::remove_file(dir.join(MARKER)).unwrap();
        let files = entries(&dir);
        assert!(files.iter().any(|name| name.ends_with(".idx")), "{files:?}");
        check_replaceable(&dir).unwrap();

        fs::write(dir.join("notes.txt"), "keep me").unwrap();
        fs::create_dir(dir.join("src")).unwrap();
        let err = format!("{:#}", check_replaceable(&dir).unwrap_err());
        assert!(err.contains("refusing to replace"), "{err}");
        assert!(err.contains("it holds notes.txt, src, which"), "{err}");
        for name in ["a", "b", "c"] {
            fs::write(dir.join(name), name).unwrap();
        }
        let err = format!("{:#}", check_replaceable(&dir).unwrap_err());
        assert!(err.contains("it holds a, b, c and 2 more, which"), "{err}");
    }

    /// A process id that is not running, for leftovers of a crashed build.
    #[cfg(target_os = "linux")]
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    /// The name of `path`, as text.
    fn name_of(path: &Path) -> String {
        path.file_name().unwrap().to_str().unwrap().to_string()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn leftovers_of_crashed_builds_are_cleaned_up() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("index");
        fake_index(&dir, "current");
        // A build under way in this process is kept.
        let running = Staging::new(&dir).unwrap();
        let dead = dead_pid();
        // A build killed mid-way, and one killed while deleting the old copy.
        fake_index(&root.path().join(format!(".index.new-{dead}-0")), "half");
        fake_index(&root.path().join(format!(".index.old-{dead}-1")), "older");
        // One made by an earlier process with this process's id, as a node
        // in a container has every time it starts.
        let me = std::process::id();
        fake_index(
            &root.path().join(format!(".index.new-{me}-{}", u64::MAX)),
            "half",
        );
        // One of a process that still runs (1 always does), and things that
        // only look alike, are kept.
        let kept = [
            ".index.new-1-0".to_string(),
            ".index.new-x-0".to_string(),
            ".other.new-1-0".to_string(),
            "index.new-1-0".to_string(),
        ];
        for name in &kept {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        let file = format!(".index.new-{dead}-2");
        fs::write(root.path().join(&file), "not a directory").unwrap();
        let staging = Staging::new(&dir).unwrap();
        let mut expected = kept.to_vec();
        expected.extend([
            file,
            "index".to_string(),
            name_of(running.path()),
            name_of(staging.path()),
        ]);
        expected.sort();
        assert_eq!(entries(root.path()), expected);
        assert_eq!(entries(&dir), [MARKER, "current"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_thread_is_not_a_running_process() {
        // `/proc` has an entry under every thread's id too.
        let (send, receive) = std::sync::mpsc::channel();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let link = fs::read_link("/proc/thread-self").unwrap();
            send.send(name_of(&link).parse::<u32>().unwrap()).unwrap();
            let _ = stopped.recv();
        });
        let thread_id = receive.recv().unwrap();
        assert_ne!(thread_id, std::process::id());
        assert!(Path::new("/proc").join(thread_id.to_string()).exists());
        assert!(!is_running(thread_id));
        assert!(is_running(std::process::id()));
        assert!(!is_running(dead_pid()));
        stop.send(()).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn leftovers_of_every_build_can_be_removed() {
        let root = TempDir::new().unwrap();
        let parent = root.path();
        // Builds of indexes no longer wanted, whatever process made them.
        let me = std::process::id();
        let own_id = format!(".index-a.new-{me}-{}", u64::MAX);
        fake_index(&parent.join(&own_id), "half");
        fake_index(&parent.join(".index-b.new-1-0"), "half");
        // A crash between the renames: the old copy goes back.
        fake_index(&parent.join(".places-c.old-1-3"), "old");
        fake_index(&parent.join(".places-c.new-1-4"), "new");
        // A build under way in this process, and things that only look
        // alike, are kept.
        fake_index(&parent.join("index-d"), "current");
        let running = Staging::new(&parent.join("index-d")).unwrap();
        fs::create_dir(parent.join("sets")).unwrap();
        fs::create_dir(parent.join(".index-e.new-x-0")).unwrap();
        fs::write(parent.join(".index-f.new-1-0"), "not a directory").unwrap();

        let mut removed = remove_build_leftovers(parent).unwrap();
        removed.sort();
        assert_eq!(
            removed,
            [
                parent.join(own_id),
                parent.join(".index-b.new-1-0"),
                parent.join(".places-c.new-1-4"),
            ]
        );
        let mut expected = vec![
            ".index-e.new-x-0".to_string(),
            ".index-f.new-1-0".to_string(),
            "index-d".to_string(),
            "places-c".to_string(),
            "sets".to_string(),
            name_of(running.path()),
        ];
        expected.sort();
        assert_eq!(entries(parent), expected);
        assert_eq!(entries(&parent.join("places-c")), [MARKER, "old"]);
        assert_eq!(entries(&parent.join("index-d")), [MARKER, "current"]);

        let path = running.path().to_path_buf();
        assert!(Claim::held(&path));
        drop(running);
        assert!(!Claim::held(&path));
        assert!(remove_build_leftovers(&parent.join("missing"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn claims_end_with_the_build() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("index");
        fake_index(&dir, "old");
        let staging = Staging::new(&dir).unwrap();
        let path = staging.path().to_path_buf();
        assert!(Claim::held(&path));
        staging.install().unwrap();
        assert!(!Claim::held(&path));
        assert_eq!(entries(root.path()), ["index"]);
    }

    #[test]
    fn sibling_names_parse() {
        let parse = |name: &str| {
            parse_sibling(OsStr::new(name))
                .map(|(target, tag, pid)| (target.to_str().unwrap().to_string(), tag, pid))
        };
        assert_eq!(
            parse(".index-5de8.new-7-2"),
            Some(("index-5de8".to_string(), Tag::New, 7))
        );
        assert_eq!(
            parse(".places.tsv.gz.index-1.old-12-0"),
            Some(("places.tsv.gz.index-1".to_string(), Tag::Old, 12))
        );
        for name in [
            "index.new-7-2",
            "..new-7-2",
            ".index.new-7",
            ".index.new-7-x",
            ".index.tmp-7-2",
            ".000086-buckets.staging",
            ".records.jsonl.99.tmp",
        ] {
            assert_eq!(parse(name), None, "{name}");
        }
        // A name made by [`sibling`] parses back.
        let made = sibling(Path::new("/data/pages/index-ab"), "new").unwrap();
        assert_eq!(
            parse(&name_of(&made)),
            Some(("index-ab".to_string(), Tag::New, std::process::id()))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_crash_mid_swap_gets_the_old_index_back() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("index");
        let dead = dead_pid();
        // The old index was moved aside and the new one never moved in.
        fake_index(&root.path().join(format!(".index.old-{dead}-0")), "old");
        fake_index(&root.path().join(format!(".index.new-{dead}-1")), "new");
        let staging = Staging::new(&dir).unwrap();
        assert_eq!(entries(&dir), [MARKER, "old"]);
        drop(staging);
        assert_eq!(entries(root.path()), ["index"]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_followed() {
        let root = TempDir::new().unwrap();
        let real = root.path().join("real");
        let link = root.path().join("link");
        fake_index(&real, "old");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let staging = Staging::new(&link).unwrap();
        fs::write(staging.path().join("new"), "new").unwrap();
        staging.install().unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(entries(&real), [MARKER, "new"]);
        assert_eq!(entries(root.path()), ["link", "real"]);
    }
}
