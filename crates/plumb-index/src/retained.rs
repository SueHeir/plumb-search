//! Readers for an externally retained, immutable index generation.
//!
//! The caller must exclude live writers and pruning for the whole reader
//! lifetime. The META_LOCK below is only an in-process guard: it creates no
//! lock file and cannot coordinate with another process. Catalog checks detect
//! changes; they do not establish immutability or hash large segment contents.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, Lock, MmapDirectory, WatchCallback, WatchHandle,
    WritePtr, META_LOCK,
};
use tantivy::Index;

const MAX_FILES: usize = 4096;
pub const MAX_METADATA_BYTES: u64 = 1 << 20;

/// An externally supplied catalog. Large files are bound by identity/size/time;
/// small metadata files also require a SHA-256. No directory enumeration occurs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileBinding {
    pub bytes: u64,
    pub modified_ns: u128,
    pub device: u64,
    pub inode: u64,
    pub sha256: Option<String>,
}

impl FileBinding {
    /// Checks identity/size/time only. A supplied large-file SHA remains an
    /// external attestation, not a content hash verified by this method.
    pub fn verify_identity(&self, path: &Path) -> Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "regular retained file required"
        );
        ensure!(
            identity(&metadata) == (self.device, self.inode)
                && metadata.len() == self.bytes
                && modified_ns(&metadata)? == self.modified_ns,
            "retained file changed: {}",
            path.display()
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationBinding {
    pub generation: String,
    /// SHA of an externally verified immutable corpus manifest. The reader
    /// records this attestation and does not rehash large segment contents.
    pub corpus_manifest_sha256_attested: String,
    /// Identifier of the external retention guarantee; never a self-issued lease.
    pub retention_receipt: String,
    pub directory_device: u64,
    pub directory_inode: u64,
    pub files: BTreeMap<String, FileBinding>,
}

/// This directory never delegates mutations to its mmap directory.
#[derive(Debug, Clone)]
pub struct RetainedGeneration {
    root: PathBuf,
    mmap: MmapDirectory,
    binding: Arc<GenerationBinding>,
    meta_locked: Arc<AtomicBool>,
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "retained generation is read-only",
    )
}

#[cfg(unix)]
fn identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn identity(_: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

fn modified_ns(metadata: &std::fs::Metadata) -> Result<u128> {
    Ok(metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos())
}

fn simple_name(path: &Path) -> Option<&str> {
    let mut parts = path.components();
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(name)), None) => name.to_str(),
        _ => None,
    }
}

impl RetainedGeneration {
    pub fn open(root: &Path, binding: GenerationBinding) -> Result<Self> {
        ensure!(cfg!(unix), "retained readers require Unix file identities");
        ensure!(
            !binding.generation.is_empty() && !binding.retention_receipt.is_empty(),
            "generation and external retention receipt required"
        );
        ensure!(
            binding.corpus_manifest_sha256_attested.len() == 64
                && binding
                    .corpus_manifest_sha256_attested
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit()),
            "external corpus manifest SHA required"
        );
        ensure!(
            !binding.files.is_empty() && binding.files.len() <= MAX_FILES,
            "catalog size"
        );
        for (name, file) in &binding.files {
            ensure!(
                simple_name(Path::new(name)) == Some(name.as_str()),
                "invalid catalog name"
            );
            if let Some(hash) = &file.sha256 {
                ensure!(
                    file.bytes <= MAX_METADATA_BYTES
                        && hash.len() == 64
                        && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                    "metadata hash/size"
                );
            }
        }
        ensure!(
            binding
                .files
                .get("meta.json")
                .is_some_and(|f| f.sha256.is_some()),
            "hashed meta.json required"
        );
        let metadata = std::fs::symlink_metadata(root)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "real directory required"
        );
        let root = root.canonicalize()?;
        let directory = Self {
            mmap: MmapDirectory::open(&root)?,
            root,
            binding: Arc::new(binding),
            meta_locked: Arc::default(),
        };
        directory.verify()?;
        Ok(directory)
    }

    pub fn binding(&self) -> &GenerationBinding {
        &self.binding
    }
    pub fn path(&self) -> &Path {
        &self.root
    }

    fn check(&self, name: &str) -> Result<()> {
        let expected = self
            .binding
            .files
            .get(name)
            .context("file absent from retained catalog")?;
        expected.verify_identity(&self.root.join(name))
    }

    /// Bounded reads only for catalogued small auxiliary files.
    pub fn read_metadata(&self, name: &str) -> Result<Vec<u8>> {
        ensure!(
            simple_name(Path::new(name)) == Some(name),
            "invalid metadata name"
        );
        self.check(name)?;
        let expected = &self.binding.files[name];
        ensure!(
            expected.sha256.is_some() && expected.bytes <= MAX_METADATA_BYTES,
            "hashed, bounded metadata required: {name}"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(self.root.join(name))?
            .take(MAX_METADATA_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 == expected.bytes,
            "metadata size changed"
        );
        let hash = format!("{:x}", Sha256::digest(&bytes));
        ensure!(
            expected.sha256.as_deref() == Some(hash.as_str()),
            "metadata hash changed: {name}"
        );
        self.check(name)?;
        Ok(bytes)
    }

    /// Checks the externally supplied catalog without scanning or hashing segments.
    pub fn verify(&self) -> Result<()> {
        let metadata = std::fs::symlink_metadata(&self.root)?;
        ensure!(
            metadata.is_dir()
                && identity(&metadata)
                    == (self.binding.directory_device, self.binding.directory_inode),
            "generation directory changed"
        );
        for (name, file) in &self.binding.files {
            self.check(name)?;
            if file.sha256.is_some() {
                self.read_metadata(name)?;
            }
        }
        Ok(())
    }

    pub(crate) fn index(&self) -> Result<Index> {
        Ok(Index::open(self.clone())?)
    }
}

struct MetaGuard(Arc<AtomicBool>);
impl Drop for MetaGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Directory for RetainedGeneration {
    fn get_file_handle(
        &self,
        path: &Path,
    ) -> std::result::Result<Arc<dyn FileHandle>, OpenReadError> {
        let name =
            simple_name(path).ok_or_else(|| OpenReadError::wrap_io_error(denied(), path.into()))?;
        self.check(name).map_err(|err| {
            OpenReadError::wrap_io_error(io::Error::other(err.to_string()), path.into())
        })?;
        self.mmap.get_file_handle(path)
    }
    fn exists(&self, path: &Path) -> std::result::Result<bool, OpenReadError> {
        let Some(name) = simple_name(path) else {
            return Ok(false);
        };
        if !self.binding.files.contains_key(name) {
            return Ok(false);
        }
        self.check(name).map_err(|err| {
            OpenReadError::wrap_io_error(io::Error::other(err.to_string()), path.into())
        })?;
        Ok(true)
    }
    fn atomic_read(&self, path: &Path) -> std::result::Result<Vec<u8>, OpenReadError> {
        let name =
            simple_name(path).ok_or_else(|| OpenReadError::wrap_io_error(denied(), path.into()))?;
        // Tantivy treats missing .managed.json as an empty managed-file list.
        if !self.binding.files.contains_key(name) {
            return Err(OpenReadError::FileDoesNotExist(path.into()));
        }
        self.read_metadata(name).map_err(|err| {
            OpenReadError::wrap_io_error(io::Error::other(err.to_string()), path.into())
        })
    }
    fn delete(&self, path: &Path) -> std::result::Result<(), DeleteError> {
        Err(DeleteError::IoError {
            io_error: Arc::new(denied()),
            filepath: path.into(),
        })
    }
    fn open_write(&self, path: &Path) -> std::result::Result<WritePtr, OpenWriteError> {
        Err(OpenWriteError::wrap_io_error(denied(), path.into()))
    }
    fn atomic_write(&self, _: &Path, _: &[u8]) -> io::Result<()> {
        Err(denied())
    }
    fn sync_directory(&self) -> io::Result<()> {
        Err(denied())
    }
    fn acquire_lock(&self, lock: &Lock) -> std::result::Result<DirectoryLock, LockError> {
        if lock.filepath != META_LOCK.filepath {
            return Err(LockError::wrap_io_error(denied()));
        }
        self.meta_locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| LockError::LockBusy)?;
        Ok(DirectoryLock::from(Box::new(MetaGuard(
            self.meta_locked.clone(),
        ))))
    }
    fn watch(&self, _: WatchCallback) -> tantivy::Result<WatchHandle> {
        Err(tantivy::TantivyError::IoError(Arc::new(denied())))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::pages::{build_page_index, Page, PageSearcher};
    use crate::{build_index, Searcher};
    use plumb_core::{article::Article, SiteRecord};
    use tantivy::directory::INDEX_WRITER_LOCK;

    // Enumeration/hashing here is only over a tiny fixture owned by this test.
    fn catalog(root: &Path) -> GenerationBinding {
        let files = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_str().unwrap().to_owned();
                let metadata = path.metadata().unwrap();
                let (device, inode) = identity(&metadata);
                let sha256 = matches!(name.as_str(), "meta.json" | ".managed.json" | "pages.json")
                    .then(|| format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap())));
                (
                    name,
                    FileBinding {
                        bytes: metadata.len(),
                        modified_ns: modified_ns(&metadata).unwrap(),
                        device,
                        inode,
                        sha256,
                    },
                )
            })
            .collect();
        let (directory_device, directory_inode) = identity(&root.metadata().unwrap());
        GenerationBinding {
            generation: "synthetic".into(),
            retention_receipt: "owned fixture".into(),
            corpus_manifest_sha256_attested: "0".repeat(64),
            directory_device,
            directory_inode,
            files,
        }
    }

    #[test]
    fn retained_native_and_pages_search_without_directory_changes() {
        let temp = tempfile::tempdir().unwrap();
        let sites = temp.path().join("sites");
        let mut site = SiteRecord::new("rust-lang.org");
        site.title = Some("Rust programming language".into());
        build_index(&sites, &[site]).unwrap();
        let pages = temp.path().join("pages");
        build_page_index(
            &pages,
            [Page::from_article(
                "en",
                Article {
                    title: "Rust programming language".into(),
                    views: 20,
                    ..Default::default()
                },
            )],
        )
        .unwrap();
        let site_binding = catalog(&sites);
        let page_binding = catalog(&pages);
        let retained_sites = RetainedGeneration::open(&sites, site_binding.clone()).unwrap();
        let retained_pages = RetainedGeneration::open(&pages, page_binding.clone()).unwrap();
        assert!(!Searcher::open_retained(&retained_sites)
            .unwrap()
            .search("rust", 10)
            .unwrap()
            .is_empty());
        assert!(!PageSearcher::open_retained(&retained_pages)
            .unwrap()
            .search("rust", 10)
            .unwrap()
            .is_empty());
        retained_sites.verify().unwrap();
        retained_pages.verify().unwrap();
        assert_eq!(catalog(&sites), site_binding);
        assert_eq!(catalog(&pages), page_binding);
    }

    #[test]
    fn retained_denies_every_mutation_writer_lock_watch_and_path_escape() {
        let temp = tempfile::tempdir().unwrap();
        build_index(temp.path(), &[SiteRecord::new("rust-lang.org")]).unwrap();
        let before = catalog(temp.path());
        let retained = RetainedGeneration::open(temp.path(), before.clone()).unwrap();
        assert!(retained.open_write(Path::new("new")).is_err());
        assert!(retained
            .atomic_write(Path::new("meta.json"), b"changed")
            .is_err());
        assert!(retained.delete(Path::new("meta.json")).is_err());
        assert!(retained.sync_directory().is_err());
        assert!(retained.acquire_lock(&INDEX_WRITER_LOCK).is_err());
        assert!(retained.watch(WatchCallback::new(|| {})).is_err());
        assert!(retained.get_file_handle(Path::new("../meta.json")).is_err());
        assert!(retained.get_file_handle(Path::new("uncatalogued")).is_err());
        let guard = retained.acquire_lock(&META_LOCK).unwrap();
        assert!(retained.clone().acquire_lock(&META_LOCK).is_err());
        drop(guard);
        assert!(retained.acquire_lock(&META_LOCK).is_ok());
        assert_eq!(catalog(temp.path()), before);
    }

    #[test]
    fn retained_rejects_missing_changed_symlink_and_false_metadata_bindings() {
        let temp = tempfile::tempdir().unwrap();
        build_index(temp.path(), &[SiteRecord::new("rust-lang.org")]).unwrap();
        let binding = catalog(temp.path());
        let retained = RetainedGeneration::open(temp.path(), binding.clone()).unwrap();
        std::fs::write(temp.path().join("meta.json"), b"changed").unwrap();
        assert!(retained.verify().is_err());
        assert!(Searcher::open_retained(&retained).is_err());
        let mut wrong_hash = catalog(temp.path());
        wrong_hash.files.get_mut("meta.json").unwrap().sha256 = Some("f".repeat(64));
        assert!(RetainedGeneration::open(temp.path(), wrong_hash).is_err());
        std::fs::remove_file(temp.path().join("meta.json")).unwrap();
        assert!(RetainedGeneration::open(temp.path(), binding.clone()).is_err());
        std::os::unix::fs::symlink("outside", temp.path().join("meta.json")).unwrap();
        assert!(RetainedGeneration::open(temp.path(), binding).is_err());
    }
}
