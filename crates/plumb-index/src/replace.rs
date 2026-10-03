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
//! disk under their hidden names and nothing at the target. Renaming also
//! means the target cannot be a mount point; mount its parent instead.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
}

impl Staging {
    /// Checks that `dir` may be replaced, then creates an empty staging
    /// directory next to it (creating `dir`'s parents if needed).
    ///
    /// `dir` may be replaced when it is missing, empty or holds an index
    /// (see the module docs); a symlink is followed, so the directory it
    /// points to gets replaced.
    pub(crate) fn new(dir: &Path) -> Result<Staging> {
        let target = resolve(dir)?;
        dir_name(&target)?;
        check_replaceable(&target)?;
        let parent = parent_of(&target);
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        loop {
            let path = sibling(&target, "new")?;
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Staging {
                        path,
                        target,
                        installed: false,
                    })
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| format!("creating {}", path.display()))
                }
            }
        }
    }

    /// The directory to build in.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Marks the staging directory as an index ([`MARKER`]) and moves it
    /// into place, replacing what was there.
    pub(crate) fn install(mut self) -> Result<()> {
        write_marker(&self.path)?;
        swap(&self.path, &self.target)?;
        self.installed = true;
        Ok(())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.installed {
            // Best effort: a leftover only wastes space, the old index is intact.
            let _ = fs::remove_dir_all(&self.path);
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
            Some(old)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };
    if let Err(err) = fs::rename(new, dir) {
        let err =
            anyhow::Error::new(err).context(format!("moving the new index into {}", dir.display()));
        if let Some(old) = old {
            if let Err(restore) = fs::rename(&old, dir) {
                return Err(err.context(format!(
                    "could not move the old index back from {} ({restore})",
                    old.display()
                )));
            }
        }
        return Err(err);
    }
    if let Some(old) = old {
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
/// files named `meta.json`, `.managed.json`, a Tantivy lock file or a file
/// that `.managed.json` lists.
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
