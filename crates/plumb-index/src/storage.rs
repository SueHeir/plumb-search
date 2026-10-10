//! Disk admission for node-managed Tantivy stages, including concurrent merges.
//! Library/CLI callers use the normal directory unless they pass a budget.

use std::io::{self, BufWriter, Write};
use std::ops::{Deref, Range};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tantivy::directory::OwnedBytes;
use tantivy::HasLen;

use plumb_core::storage::{
    allocation_for, file_bytes, AdmittedWriter, FileLease, StageLifecycle, StorageBudget,
};
use tantivy::directory::error::{DeleteError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    AntiCallToken, Directory, FileHandle, MmapDirectory, TerminatingWrite, WatchCallback,
    WatchHandle, WritePtr,
};
use tantivy::schema::Schema;
use tantivy::{Index, IndexSettings};

#[derive(Debug, Clone)]
struct QuotaDirectory {
    directory: MmapDirectory,
    path: PathBuf,
    budget: Arc<StorageBudget>,
    lifecycle: Arc<StageLifecycle>,
}

pub(crate) fn create_index(
    path: &Path,
    schema: Schema,
    budget: Option<Arc<StorageBudget>>,
    lifecycle: Option<Arc<StageLifecycle>>,
) -> anyhow::Result<Index> {
    match budget {
        None => Ok(Index::create_in_dir(path, schema)?),
        Some(budget) => {
            // Staging admitted and accounted for the directory itself.
            let directory = QuotaDirectory {
                directory: MmapDirectory::open(path)?,
                path: path.to_owned(),
                budget,
                lifecycle: lifecycle.expect("budgeted stage has a lifecycle"),
            };
            Ok(Index::create(directory, schema, IndexSettings::default())?)
        }
    }
}

struct QuotaWriter {
    writer: AdmittedWriter<WritePtr>,
}
impl Write for QuotaWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writer.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}
impl TerminatingWrite for QuotaWriter {
    fn terminate_ref(&mut self, token: AntiCallToken) -> io::Result<()> {
        self.writer.with_inner(|writer| writer.terminate_ref(token))
    }
}

impl Directory for QuotaDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        let _mutation = self.budget.mutation();
        self.lifecycle
            .check()
            .map_err(|err| OpenReadError::wrap_io_error(err, path.to_owned()))?;
        let handle = self.directory.get_file_handle(path)?;
        let allocation = self.lifecycle.lease(&self.path.join(path));
        Ok(Arc::new(LeasedHandle { handle, allocation }))
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.directory.exists(path)
    }
    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.directory.atomic_read(path)
    }
    fn sync_directory(&self) -> io::Result<()> {
        self.directory.sync_directory()
    }
    fn watch(&self, callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        self.directory.watch(callback)
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        let _mutation = self.budget.mutation();
        self.lifecycle.check().map_err(|err| DeleteError::IoError {
            io_error: Arc::new(err),
            filepath: path.to_owned(),
        })?;
        let size = file_bytes(&self.path.join(path)).unwrap_or(0);
        self.directory.delete(path)?;
        if !self.lifecycle.detach(&self.path.join(path), size) {
            self.budget.removed(size);
        }
        Ok(())
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        let _mutation = self.budget.mutation();
        self.lifecycle
            .check()
            .map_err(|err| OpenWriteError::wrap_io_error(err, path.to_owned()))?;
        let mut reserved = self
            .budget
            .reserve(allocation_for(0), false)
            .map_err(|err| OpenWriteError::wrap_io_error(io::Error::other(err), path.to_owned()))?;
        let directory_before = file_bytes(&self.path)
            .map_err(|err| OpenWriteError::wrap_io_error(io::Error::other(err), path.to_owned()))?;
        let writer = self.directory.open_write(path)?;
        let directory_after = match file_bytes(&self.path) {
            Ok(bytes) => bytes,
            Err(err) => {
                reserved.commit(reserved.bytes(), 0, allocation_for(0));
                return Err(OpenWriteError::wrap_io_error(err, path.to_owned()));
            }
        };
        let mut writer =
            AdmittedWriter::new(writer, &self.path.join(path), self.budget.clone(), reserved)
                .map_err(|err| OpenWriteError::wrap_io_error(err, path.to_owned()))?;
        writer.stage(self.lifecycle.clone());
        self.budget
            .added(directory_after.saturating_sub(directory_before));
        Ok(BufWriter::new(Box::new(QuotaWriter { writer })))
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let _mutation = self.budget.mutation();
        self.lifecycle.check()?;
        let mut reservation = self
            .budget
            .reserve(allocation_for(data.len() as u64), false)
            .map_err(io::Error::other)?;
        let file = self.path.join(path);
        let old = match file_bytes(&file) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err),
        };
        let directory_before = file_bytes(&self.path)?;
        let result = self.directory.atomic_write(path, data);
        let measured = match (file_bytes(&file), file_bytes(&self.path)) {
            (Ok(new), Ok(dir)) => Some(new.saturating_add(dir.saturating_sub(directory_before))),
            (Err(err), Ok(dir)) if err.kind() == io::ErrorKind::NotFound => {
                Some(dir.saturating_sub(directory_before))
            }
            _ => None,
        };
        let retired = result.is_ok() && self.lifecycle.detach(&file, old);
        if let Some(new) = measured {
            reservation.commit(reservation.bytes(), if retired { 0 } else { old }, new);
        } else {
            // Metadata failures retain a conservative charge for the full
            // replacement and leave the old allocation charged too.
            reservation.commit(reservation.bytes(), 0, allocation_for(data.len() as u64));
        }
        result
    }
}

impl Drop for QuotaWriter {
    fn drop(&mut self) {
        // Directory's contract requires explicit flush/terminate. Always
        // discard its inner buffer on drop: a close racing this drop can never
        // trigger BufWriter's implicit flush outside the admission barrier.
        if let Some(writer) = self.writer.discard_inner() {
            drop(writer.into_parts());
        }
    }
}

#[derive(Debug)]
struct LeasedHandle {
    handle: Arc<dyn FileHandle>,
    allocation: Arc<FileLease>,
}
impl HasLen for LeasedHandle {
    fn len(&self) -> usize {
        self.handle.len()
    }
}
#[async_trait::async_trait]
impl FileHandle for LeasedHandle {
    fn read_bytes(&self, range: Range<usize>) -> io::Result<OwnedBytes> {
        Ok(OwnedBytes::new(LeasedBytes {
            bytes: self.handle.read_bytes(range)?,
            allocation: self.allocation.clone(),
        }))
    }
}
struct LeasedBytes {
    bytes: OwnedBytes,
    allocation: Arc<FileLease>,
}
impl Deref for LeasedBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        let _held = &self.allocation;
        &self.bytes
    }
}

// OwnedBytes keeps its slice at a fixed allocation across wrapper moves.
unsafe impl stable_deref_trait::StableDeref for LeasedBytes {}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_core::storage::{allocation_for, directory_bytes, StageLifecycle};

    fn directory(root: &Path, limit: u64) -> (QuotaDirectory, Arc<StorageBudget>) {
        let budget = StorageBudget::open(root, directory_bytes(root).unwrap() + limit).unwrap();
        let lifecycle = StageLifecycle::new(budget.clone());
        (
            QuotaDirectory {
                directory: MmapDirectory::open(root).unwrap(),
                path: root.to_owned(),
                budget: budget.clone(),
                lifecycle,
            },
            budget,
        )
    }

    #[test]
    fn failed_stage_closes_writes_and_keeps_unlinked_writer_and_reader_allocations() {
        let root = tempfile::tempdir().unwrap();
        let (directory, budget) = directory(root.path(), 128 * 1024);
        let mut writer = directory.open_write(Path::new("segment")).unwrap();
        writer.write_all(&[7; 8192]).unwrap();
        writer.flush().unwrap();
        let handle = directory.get_file_handle(Path::new("segment")).unwrap();
        let bytes = handle.read_bytes(0..8192).unwrap();
        let allocated = file_bytes(&root.path().join("segment")).unwrap();
        directory.lifecycle.remove_stage(root.path());
        assert!(directory.open_write(Path::new("late")).is_err());
        assert!(writer.write_all(&[8; 16384]).is_err());
        assert!(budget.status().reader_held_bytes >= allocated);
        drop(handle);
        drop(writer);
        assert!(
            budget.status().reader_held_bytes >= allocated,
            "mapped bytes still hold allocation"
        );
        assert_eq!(bytes[0], 7);
        drop(bytes);
        assert_eq!(budget.status().reader_held_bytes, 0);
        assert_eq!(budget.status().reserved_bytes, 0);
        assert!(!root.path().exists());
    }

    #[test]
    fn atomic_replacement_reserves_both_generations_and_rejection_preserves_old_bytes() {
        let root = tempfile::tempdir().unwrap();
        let (directory, budget) = directory(root.path(), 64 * 1024);
        directory
            .atomic_write(Path::new("metadata"), &[1; 8192])
            .unwrap();
        let used = budget.status().used_bytes;
        budget.set_limit(used + allocation_for(8192) - 1);
        assert!(directory
            .atomic_write(Path::new("metadata"), &[2; 8192])
            .is_err());
        assert_eq!(
            directory.atomic_read(Path::new("metadata")).unwrap(),
            vec![1; 8192]
        );
        assert_eq!(budget.status().used_bytes, used);
        assert_eq!(budget.status().reserved_bytes, 0);
    }

    #[test]
    fn mid_build_quota_failure_preserves_the_old_native_searcher_and_releases_stage() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("index");
        let mut protected = plumb_core::SiteRecord::new("protected.example");
        protected.title = Some("Protected Search".into());
        crate::build_index(&path, &[protected]).unwrap();
        let old = crate::Searcher::open(&path).unwrap();
        let baseline = directory_bytes(root.path()).unwrap();
        let budget = StorageBudget::open(root.path(), baseline + 64 * 1024).unwrap();
        let records: Vec<_> = (0..2000)
            .map(|i| {
                let mut record = plumb_core::SiteRecord::new(format!("fresh-{i}.example"));
                record.title = Some(format!("Fresh Index Content {i}"));
                record.description = Some(format!("Unique tokens for domain {i} {}", i * 7919));
                record
            })
            .collect();
        assert!(crate::build_index_with_budget(&path, &records, Some(budget.clone())).is_err());
        assert_eq!(
            old.search("protected", 1).unwrap()[0].domain,
            "protected.example"
        );
        assert_eq!(
            crate::Searcher::open(&path)
                .unwrap()
                .search("protected", 1)
                .unwrap()[0]
                .domain,
            "protected.example"
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        // Detached Tantivy workers have closed the failed stage's descriptors;
        // their allocation leases can release without a full root recount.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while budget.status().reader_held_bytes > 0 || budget.status().reserved_bytes > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "stage writer leases leaked"
            );
            std::thread::yield_now();
        }
        assert!(budget.status().used_bytes <= baseline + 8192);
    }
}
