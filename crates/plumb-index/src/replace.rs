//! Replacing an index directory without ever leaving it missing or half built.
//!
//! A new index is written to a hidden staging directory next to the target
//! (same parent, so renames stay on one file system). Once it is complete,
//! the old directory is renamed aside, the new one renamed into place and
//! the old one deleted. A failed build or swap deletes the staging directory
//! and leaves the old index where it was.
//!
//! The swap is two renames, so a crash between them leaves both copies on
//! disk under their hidden names and nothing at the target. Renaming also
//! means the target cannot be a mount point; mount its parent instead.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};

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
    /// `dir` may be replaced when it is missing, empty or holds an index; a
    /// symlink is followed, so the directory it points to gets replaced.
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

    /// Moves the staging directory into place, replacing what was there.
    pub(crate) fn install(mut self) -> Result<()> {
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

/// Fails unless `dir` is missing, empty or holds an index.
fn check_replaceable(dir: &Path) -> Result<()> {
    match fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() && !dir.join("meta.json").is_file() {
                bail!(
                    "refusing to replace {}: it is not empty and does not hold a search index",
                    dir.display()
                );
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("reading {}", dir.display())),
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

    /// A directory that looks like an index, holding `file`.
    fn fake_index(dir: &Path, file: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("meta.json"), "{}").unwrap();
        fs::write(dir.join(file), file).unwrap();
    }

    #[test]
    fn install_replaces_the_old_directory() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("index");
        fake_index(&dir, "old");
        let staging = Staging::new(&dir).unwrap();
        fake_index(staging.path(), "new");
        assert_eq!(entries(root.path()).len(), 2);
        staging.install().unwrap();
        assert_eq!(entries(&dir), ["meta.json", "new"]);
        assert_eq!(entries(root.path()), ["index"]);
    }

    #[test]
    fn install_creates_missing_directories() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("a/b/index");
        let staging = Staging::new(&dir).unwrap();
        fake_index(staging.path(), "new");
        staging.install().unwrap();
        assert_eq!(entries(&dir), ["meta.json", "new"]);
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
        assert_eq!(entries(&dir), ["meta.json", "old"]);
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
        assert_eq!(entries(&dir), ["meta.json", "old"]);
        assert_eq!(entries(root.path()), ["index"]);
    }

    #[test]
    fn refuses_what_is_not_an_index() {
        let root = TempDir::new().unwrap();
        let data = root.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("notes.txt"), "keep me").unwrap();
        assert!(Staging::new(&data).is_err());
        let file = root.path().join("file");
        fs::write(&file, "keep me too").unwrap();
        assert!(Staging::new(&file).is_err());
        assert!(Staging::new(Path::new("/")).is_err());
        assert_eq!(entries(root.path()), ["data", "file"]);
        assert_eq!(entries(&data), ["notes.txt"]);
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
        fake_index(staging.path(), "new");
        staging.install().unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(entries(&real), ["meta.json", "new"]);
        assert_eq!(entries(root.path()), ["link", "real"]);
    }
}
