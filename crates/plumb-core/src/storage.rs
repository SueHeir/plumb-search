//! Admission for node-managed storage. A startup count includes the entire data
//! directory; subsequent network writes reserve room before creating files.
//! Recounts accompany the node's existing storage-maintenance checkpoints.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

// Allow for block rounding and per-file filesystem metadata. Counting logical
// JSON lengths alone is particularly misleading for thousands of small files.
const BLOCK: u64 = 4096;
const FILE_OVERHEAD: u64 = 4096;

pub fn allocation_for(bytes: u64) -> u64 {
    bytes
        .div_ceil(BLOCK)
        .saturating_mul(BLOCK)
        .saturating_add(FILE_OVERHEAD)
        .saturating_add(BLOCK) // transient directory growth while staging
}

pub fn file_bytes(path: &Path) -> io::Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(metadata
            .blocks()
            .saturating_mul(512)
            .saturating_add(FILE_OVERHEAD))
    }
    #[cfg(not(unix))]
    {
        Ok(allocation_for(metadata.len()))
    }
}

pub fn existing_file_bytes(path: &Path) -> io::Result<u64> {
    match file_bytes(path) {
        Ok(bytes) => Ok(bytes),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(err),
    }
}

pub fn directory_bytes(path: &Path) -> io::Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    let mut bytes = file_bytes(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            match directory_bytes(&entry?.path()) {
                Ok(size) => bytes = bytes.saturating_add(size),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
    }
    Ok(bytes)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageStatus {
    pub limit_bytes: u64,
    pub used_bytes: u64,
    pub reserved_bytes: u64,
    /// New foreign batches, inbox data and cached buckets wait for room.
    pub backpressure: bool,
    pub rejected_writes: u64,
    /// Unlinked segment files whose mapped readers still own their allocation.
    pub reader_held_bytes: u64,
}

#[derive(Debug)]
pub struct StorageBudget {
    root: std::path::PathBuf,
    state: Mutex<StorageStatus>,
    mutation: Mutex<()>,
    counted_at: Mutex<std::time::Instant>,
    input_readers: Mutex<std::collections::HashMap<std::path::PathBuf, std::sync::Weak<FileLease>>>,
}

impl StorageBudget {
    pub fn open(root: &Path, limit: u64) -> Result<Arc<Self>> {
        ensure!(limit > 0, "storage admission needs a positive byte limit");
        let root = fs::canonicalize(root)?;
        let used = directory_bytes(&root)?;
        Ok(Arc::new(Self {
            root,
            mutation: Mutex::new(()),
            counted_at: Mutex::new(std::time::Instant::now()),
            input_readers: Mutex::default(),
            state: Mutex::new(StorageStatus {
                limit_bytes: limit,
                used_bytes: used,
                backpressure: used >= limit,
                ..StorageStatus::default()
            }),
        }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve aliases before deciding which owner's ledger a file belongs to.
    /// A missing file is scoped by its existing parent (also where staging runs).
    pub fn contains_file(&self, path: &Path) -> io::Result<bool> {
        let resolved = match fs::canonicalize(path) {
            Ok(path) => path,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "cannot scope a dangling storage symlink",
                    ));
                }
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                fs::canonicalize(parent)?.join(path.file_name().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "storage path needs a file name",
                    )
                })?)
            }
            Err(err) => return Err(err),
        };
        Ok(resolved.starts_with(&self.root))
    }

    pub fn mutation(&self) -> std::sync::MutexGuard<'_, ()> {
        self.mutation.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn status(&self) -> StorageStatus {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn available(&self) -> u64 {
        let state = self.status();
        state
            .limit_bytes
            .saturating_sub(state.used_bytes.saturating_add(state.reserved_bytes))
    }

    /// Only the node's own signed batches are exempt. Raw records, inboxes
    /// and credits are never removed to make this reservation fit.
    pub fn reserve(self: &Arc<Self>, bytes: u64, exempt: bool) -> Result<Reservation> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let next = state
            .used_bytes
            .saturating_add(state.reserved_bytes)
            .saturating_add(bytes);
        if !exempt && next > state.limit_bytes {
            state.backpressure = true;
            state.rejected_writes = state.rejected_writes.saturating_add(1);
            anyhow::bail!(
                "storage backpressure: {bytes} bytes requested, {} available",
                state
                    .limit_bytes
                    .saturating_sub(state.used_bytes.saturating_add(state.reserved_bytes))
            );
        }
        state.reserved_bytes = state.reserved_bytes.saturating_add(bytes);
        Ok(Reservation {
            budget: self.clone(),
            bytes,
        })
    }

    pub fn added(&self, bytes: u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.used_bytes = state.used_bytes.saturating_add(bytes);
        state.backpressure =
            state.used_bytes.saturating_add(state.reserved_bytes) >= state.limit_bytes;
    }

    pub fn unlinked(&self, bytes: u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.reader_held_bytes = state.reader_held_bytes.saturating_add(bytes);
    }
    pub fn release_unlinked(&self, bytes: u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.reader_held_bytes = state.reader_held_bytes.saturating_sub(bytes);
        state.used_bytes = state.used_bytes.saturating_sub(bytes);
        state.backpressure =
            state.used_bytes.saturating_add(state.reserved_bytes) >= state.limit_bytes;
    }

    /// Register an input file while holding mutation through opening its fd.
    pub fn read_lease(self: &Arc<Self>, path: &Path) -> Arc<FileLease> {
        let mut readers = self
            .input_readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(lease) = readers.get(path).and_then(std::sync::Weak::upgrade) {
            return lease;
        }
        let lease = Arc::new(FileLease {
            bytes: std::sync::atomic::AtomicU64::new(file_bytes(path).unwrap_or(0)),
            deleted: std::sync::atomic::AtomicBool::new(false),
            budget: self.clone(),
        });
        readers.insert(path.to_owned(), Arc::downgrade(&lease));
        lease
    }
    /// Account an old input generation after a successful rename, under mutation.
    pub fn retire_file(&self, path: &Path, bytes: u64) {
        let lease = self
            .input_readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(path)
            .and_then(|lease| lease.upgrade());
        if let Some(lease) = lease {
            lease
                .bytes
                .store(bytes, std::sync::atomic::Ordering::Release);
            self.unlinked(bytes);
            lease
                .deleted
                .store(true, std::sync::atomic::Ordering::Release);
        } else {
            self.removed(bytes);
        }
    }

    /// Move a still-linked generation's readers with its retained pathname.
    /// The caller holds mutation and has retired any overwritten destination.
    pub fn move_read_lease(&self, from: &Path, to: &Path) {
        let mut readers = self
            .input_readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(lease) = readers.remove(from) {
            readers.insert(to.to_owned(), lease);
        }
    }

    pub fn removed(&self, bytes: u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.used_bytes = state.used_bytes.saturating_sub(bytes);
        state.backpressure =
            state.used_bytes.saturating_add(state.reserved_bytes) >= state.limit_bytes;
    }

    /// Called at existing rebuild/retention checkpoints, never per batch.
    /// Hold admission while counting so network writes cannot race the count.
    pub fn set_limit(&self, bytes: u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.limit_bytes = if bytes == 0 { u64::MAX } else { bytes };
        state.backpressure =
            state.used_bytes.saturating_add(state.reserved_bytes) >= state.limit_bytes;
    }

    /// Invalidation checkpoints can be frequent; full reconciliation is not.
    pub fn recount_if_due(&self, root: &Path) -> Result<()> {
        self.recount_if_due_at(root, std::time::Instant::now())
    }

    pub fn recount_if_due_at(&self, root: &Path, now: std::time::Instant) -> Result<()> {
        let mut counted = self
            .counted_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if now.saturating_duration_since(*counted) >= std::time::Duration::from_secs(300) {
            self.recount(root)?;
            *counted = now;
        }
        Ok(())
    }

    pub fn recount(&self, root: &Path) -> Result<()> {
        let _mutation = self.mutation();
        // The filesystem stays quiescent while counting, but status and
        // reservations must not wait on the tree walk's state mutex.
        ensure!(
            fs::canonicalize(root)? == self.root,
            "recount root differs from storage owner"
        );
        let linked = directory_bytes(&self.root)?;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.used_bytes = linked.saturating_add(state.reader_held_bytes);
        state.backpressure =
            state.used_bytes.saturating_add(state.reserved_bytes) >= state.limit_bytes;
        Ok(())
    }
}

/// Room held for staging files or records waiting in the bounded inbox channel.
/// Dropping failed/cancelled work releases its reservation.
#[derive(Debug)]
pub struct Reservation {
    budget: Arc<StorageBudget>,
    bytes: u64,
}

impl Reservation {
    pub fn ensure(&mut self, bytes: u64) -> Result<()> {
        if bytes > self.bytes {
            let mut extra = self.budget.reserve(bytes - self.bytes, false)?;
            self.bytes = self.bytes.saturating_add(extra.bytes);
            extra.bytes = 0;
        }
        Ok(())
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Converts a reserved part into a file's measured allocation. The rest
    /// stays reserved while accepted records wait to be written to the inbox.
    pub fn commit(&mut self, reserved: u64, old: u64, new: u64) {
        let reserved = reserved.min(self.bytes);
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.reserved_bytes = state.reserved_bytes.saturating_sub(reserved);
        state.used_bytes = state.used_bytes.saturating_sub(old).saturating_add(new);
        self.bytes -= reserved;
        state.backpressure =
            state.used_bytes.saturating_add(state.reserved_bytes) >= state.limit_bytes;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.reserved_bytes = state.reserved_bytes.saturating_sub(self.bytes);
        state.backpressure =
            state.used_bytes.saturating_add(state.reserved_bytes) >= state.limit_bytes;
    }
}

/// Serializes without allocating more than the caller's admitted payload cap.
pub struct LimitedBytes {
    pub bytes: Vec<u8>,
    pub limit: usize,
}

impl Write for LimitedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other(
                "storage payload exceeds its admission bound",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_include_allocated_baseline_and_concurrent_work() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("records.jsonl"), b"raw records stay").unwrap();
        let used = directory_bytes(dir.path()).unwrap();
        let budget = StorageBudget::open(dir.path(), used + allocation_for(20)).unwrap();
        let reserved = budget.reserve(allocation_for(20), false).unwrap();
        assert!(budget.reserve(1, false).is_err());
        assert_eq!(budget.status().reserved_bytes, allocation_for(20));
        drop(reserved);
        assert_eq!(budget.status().reserved_bytes, 0);
        assert!(budget.reserve(allocation_for(20), false).is_ok());
        assert_eq!(
            fs::read(dir.path().join("records.jsonl")).unwrap(),
            b"raw records stay"
        );
    }

    #[test]
    fn own_exemption_exposes_backpressure_and_never_frees_raw_data() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("inbox.absorbing"), b"unfinished replay").unwrap();
        let budget = StorageBudget::open(dir.path(), 1).unwrap();
        assert!(budget.status().backpressure);
        assert!(budget.reserve(1, false).is_err());
        let mut own = budget.reserve(allocation_for(10), true).unwrap();
        own.commit(allocation_for(10), 0, allocation_for(10));
        assert!(budget.status().backpressure);
        assert_eq!(budget.status().reserved_bytes, 0);
        assert!(dir.path().join("inbox.absorbing").exists());
    }
}

/// A writer that admits each allocation before handing bytes to a buffered
/// filesystem writer. Used by staged index files; callers create the file only
/// after reserving its metadata. Buffered bytes remain reserved until measured.
pub struct AdmittedWriter<W: Write> {
    inner: Option<W>,
    path: std::path::PathBuf,
    budget: Arc<StorageBudget>,
    reservation: Reservation,
    logical: u64,
    allocated: u64,
    lifecycle: Option<Arc<StageLifecycle>>,
    lease: Option<Arc<FileLease>>,
}

impl<W: Write> AdmittedWriter<W> {
    pub fn new(
        inner: W,
        path: &Path,
        budget: Arc<StorageBudget>,
        mut reservation: Reservation,
    ) -> io::Result<Self> {
        let (allocated, logical) = match file_bytes(path)
            .and_then(|allocated| fs::metadata(path).map(|metadata| (allocated, metadata.len())))
        {
            Ok(measured) => measured,
            Err(err) => {
                let worst = reservation.bytes();
                reservation.commit(worst, 0, worst);
                return Err(err);
            }
        };
        reservation.commit(allocated, 0, allocated);
        Ok(Self {
            inner: Some(inner),
            path: path.to_owned(),
            budget,
            reservation,
            logical,
            allocated,
            lifecycle: None,
            lease: None,
        })
    }

    pub fn stage(&mut self, lifecycle: Arc<StageLifecycle>) {
        self.lease = Some(lifecycle.lease(&self.path));
        self.lifecycle = Some(lifecycle);
    }
    pub fn discard_inner(&mut self) -> Option<W> {
        self.inner.take()
    }

    fn measure(&mut self, directory_before: u64) -> io::Result<()> {
        // A terminated writer may outlive its staging directory's rename.
        let measured = file_bytes(&self.path).and_then(|bytes| {
            self.path
                .parent()
                .map(file_bytes)
                .transpose()
                .map(|parent| (bytes, parent.unwrap_or(0)))
        });
        let (allocated, directory_after) = match measured {
            Ok(measured) => measured,
            Err(err) => {
                // A metadata failure cannot release reserved growth that a
                // completed/partial write may already have allocated.
                let worst = allocation_for(self.logical)
                    .max(self.allocated.saturating_add(self.reservation.bytes()));
                self.reservation
                    .commit(self.reservation.bytes(), self.allocated, worst);
                self.allocated = worst;
                if let Some(lease) = &self.lease {
                    lease
                        .bytes
                        .store(worst, std::sync::atomic::Ordering::Release);
                }
                return Err(err);
            }
        };
        let growth = allocated
            .saturating_sub(self.allocated)
            .saturating_add(directory_after.saturating_sub(directory_before));
        self.reservation.commit(
            growth,
            self.allocated,
            allocated.saturating_add(directory_after.saturating_sub(directory_before)),
        );
        self.allocated = allocated;
        if let Some(lease) = &self.lease {
            lease
                .bytes
                .store(allocated, std::sync::atomic::Ordering::Release);
        }
        Ok(())
    }

    pub fn with_inner(
        &mut self,
        operation: impl FnOnce(&mut W) -> io::Result<()>,
    ) -> io::Result<()> {
        let budget = self.budget.clone();
        let _mutation = budget.mutation();
        if let Some(stage) = &self.lifecycle {
            stage.check()?;
        }
        let directory_before = self.path.parent().map(file_bytes).transpose()?.unwrap_or(0);
        let result = operation(self.inner.as_mut().expect("writer open"));
        self.measure(directory_before)?;
        result
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.flush()?;
        Ok(self.inner.take().expect("writer open"))
    }
}

impl<W: Write> Write for AdmittedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let budget = self.budget.clone();
        let _mutation = budget.mutation();
        if let Some(stage) = &self.lifecycle {
            stage.check()?;
        }
        let required = allocation_for(self.logical.saturating_add(bytes.len() as u64))
            .saturating_sub(self.allocated);
        self.reservation
            .ensure(required)
            .map_err(io::Error::other)?;
        let directory_before = self.path.parent().map(file_bytes).transpose()?.unwrap_or(0);
        let result = self.inner.as_mut().expect("writer open").write(bytes);
        if let Ok(written) = result {
            self.logical = self.logical.saturating_add(written as u64);
        }
        self.measure(directory_before)?;
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        self.with_inner(Write::flush)
    }
}

impl<W: Write> Drop for AdmittedWriter<W> {
    fn drop(&mut self) {
        if self.inner.is_some() {
            let _ = self.flush();
        }
    }
}

impl PartialEq for StorageBudget {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for StorageBudget {}

/// Bounded serialization sizing without holding a second copy of the payload.
#[derive(Default)]
pub struct ByteCount {
    pub bytes: u64,
    pub limit: u64,
}
impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.limit.saturating_sub(self.bytes) {
            return Err(io::Error::other("storage payload exceeds sizing bound"));
        }
        self.bytes += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Files created by node-managed staged writers. The plain variant preserves
/// library callers' behavior. Each filesystem mutation holds admission's lock.
pub enum BudgetFile {
    Plain(fs::File),
    Admitted(AdmittedWriter<fs::File>),
}
impl BudgetFile {
    pub fn create(path: &Path, budget: Option<Arc<StorageBudget>>) -> io::Result<Self> {
        match budget {
            None => Ok(Self::Plain(fs::File::create(path)?)),
            Some(budget) => {
                let held = budget.clone();
                let _mutation = held.mutation();
                let mut reservation = budget
                    .reserve(allocation_for(0), false)
                    .map_err(io::Error::other)?;
                let before = path.parent().map(file_bytes).transpose()?.unwrap_or(0);
                let file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?;
                let after = match path.parent().map(file_bytes).transpose() {
                    Ok(bytes) => bytes.unwrap_or(0),
                    Err(err) => {
                        reservation.commit(reservation.bytes(), 0, allocation_for(0));
                        return Err(err);
                    }
                };
                let admitted = AdmittedWriter::new(file, path, budget.clone(), reservation)?;
                budget.added(after.saturating_sub(before));
                Ok(Self::Admitted(admitted))
            }
        }
    }
    /// Open a resumable staged output. Existing allocation is already charged;
    /// truncation releases only its measured bytes, and growth remains admitted.
    pub fn open_write(
        path: &Path,
        truncate: bool,
        budget: Option<Arc<StorageBudget>>,
    ) -> io::Result<Self> {
        let Some(budget) = budget else {
            return Ok(Self::Plain(
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(truncate)
                    .open(path)?,
            ));
        };
        let held = budget.clone();
        let _mutation = held.mutation();
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(io::Error::other("quota output cannot be a symlink"));
        }
        let old = existing_file_bytes(path)?;
        let mut reservation = budget
            .reserve(if old == 0 { allocation_for(0) } else { 0 }, false)
            .map_err(io::Error::other)?;
        let before = path.parent().map(file_bytes).transpose()?.unwrap_or(0);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(truncate)
            .open(path)?;
        let measured = file_bytes(path).and_then(|allocated| {
            fs::metadata(path).and_then(|metadata| {
                path.parent()
                    .map(file_bytes)
                    .transpose()
                    .map(|parent| (allocated, metadata.len(), parent.unwrap_or(0)))
            })
        });
        let (allocated, logical, parent) = match measured {
            Ok(measured) => measured,
            Err(err) => {
                reservation.commit(reservation.bytes(), 0, allocation_for(0));
                return Err(err);
            }
        };
        reservation.commit(
            reservation.bytes(),
            old,
            allocated.saturating_add(parent.saturating_sub(before)),
        );
        Ok(Self::Admitted(AdmittedWriter {
            inner: Some(file),
            path: path.to_owned(),
            budget,
            reservation,
            logical,
            allocated,
            lifecycle: None,
            lease: None,
        }))
    }

    pub fn seek(&mut self, position: io::SeekFrom) -> io::Result<()> {
        use io::Seek;
        match self {
            Self::Plain(file) => file.seek(position).map(|_| ()),
            Self::Admitted(writer) => writer.with_inner(|file| file.seek(position).map(|_| ())),
        }
    }

    pub fn sync_all(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(file) => file.sync_all(),
            Self::Admitted(writer) => writer.with_inner(|file| file.sync_all()),
        }
    }
}
impl Write for BudgetFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(file) => file.write(bytes),
            Self::Admitted(writer) => writer.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(file) => file.flush(),
            Self::Admitted(writer) => writer.flush(),
        }
    }
}

/// A write barrier and allocation leases for a staged directory. Failed builds
/// close admission before unlinking; detached workers cannot reopen/write files,
/// and open mappings/file descriptors keep their allocation charged until drop.
#[derive(Debug)]
pub struct StageLifecycle {
    pub closed: std::sync::atomic::AtomicBool,
    files: Mutex<std::collections::HashMap<std::path::PathBuf, std::sync::Weak<FileLease>>>,
    budget: Arc<StorageBudget>,
}
#[derive(Debug)]
pub struct FileLease {
    pub bytes: std::sync::atomic::AtomicU64,
    deleted: std::sync::atomic::AtomicBool,
    budget: Arc<StorageBudget>,
}
impl Drop for FileLease {
    fn drop(&mut self) {
        if self.deleted.load(std::sync::atomic::Ordering::Acquire) {
            self.budget
                .release_unlinked(self.bytes.load(std::sync::atomic::Ordering::Acquire));
        }
    }
}
impl StageLifecycle {
    pub fn new(budget: Arc<StorageBudget>) -> Arc<Self> {
        Arc::new(Self {
            closed: std::sync::atomic::AtomicBool::new(false),
            files: Mutex::default(),
            budget,
        })
    }
    pub fn check(&self) -> io::Result<()> {
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            Err(io::Error::other("index stage closed"))
        } else {
            Ok(())
        }
    }
    pub fn lease(&self, path: &Path) -> Arc<FileLease> {
        let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(lease) = files.get(path).and_then(std::sync::Weak::upgrade) {
            return lease;
        }
        let lease = Arc::new(FileLease {
            bytes: std::sync::atomic::AtomicU64::new(file_bytes(path).unwrap_or(0)),
            deleted: std::sync::atomic::AtomicBool::new(false),
            budget: self.budget.clone(),
        });
        files.insert(path.to_owned(), Arc::downgrade(&lease));
        lease
    }
    /// Called after a successful unlink/replacement under the mutation lock.
    pub fn detach(&self, path: &Path, bytes: u64) -> bool {
        let lease = self
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(path)
            .and_then(|lease| lease.upgrade());
        if let Some(lease) = lease {
            lease
                .bytes
                .store(bytes, std::sync::atomic::Ordering::Release);
            self.budget.unlinked(bytes);
            lease
                .deleted
                .store(true, std::sync::atomic::Ordering::Release);
            true
        } else {
            false
        }
    }
    pub fn remove_stage(&self, path: &Path) {
        let _mutation = self.budget.mutation();
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        let before = directory_bytes(path).unwrap_or(0);
        let leases: Vec<_> = self
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(file, _)| file.starts_with(path))
            .filter_map(|(file, lease)| lease.upgrade().map(|lease| (file.clone(), lease)))
            .collect();
        let _ = fs::remove_dir_all(path);
        let after = match directory_bytes(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
            Err(_) => before,
        };
        let mut retained = 0u64;
        for (file, lease) in &leases {
            if fs::symlink_metadata(file).is_err_and(|err| err.kind() == io::ErrorKind::NotFound)
                && !lease
                    .deleted
                    .swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                let bytes = lease.bytes.load(std::sync::atomic::Ordering::Acquire);
                self.budget.unlinked(bytes);
                retained = retained.saturating_add(bytes);
            }
        }
        self.budget
            .removed(before.saturating_sub(after).saturating_sub(retained));
    }
}

/// RAII cleanup for synchronous staged files (buckets). Declare this after
/// their buffered writers so those close before the directory is unlinked.
pub struct StageGuard {
    path: std::path::PathBuf,
    lifecycle: Arc<StageLifecycle>,
    pub installed: bool,
}
impl StageGuard {
    pub fn create(path: &Path, budget: Arc<StorageBudget>) -> Result<Self> {
        let _mutation = budget.mutation();
        let mut reservation = budget.reserve(allocation_for(BLOCK), false)?;
        let before = path.parent().map(file_bytes).transpose()?.unwrap_or(0);
        fs::create_dir(path)?;
        let created = file_bytes(path).unwrap_or(allocation_for(0));
        let parent_growth = path
            .parent()
            .map(file_bytes)
            .transpose()
            .unwrap_or(Some(before + 4096))
            .unwrap_or(0)
            .saturating_sub(before);
        reservation.commit(
            reservation.bytes(),
            0,
            created.saturating_add(parent_growth),
        );
        Ok(Self {
            path: path.to_owned(),
            lifecycle: StageLifecycle::new(budget.clone()),
            installed: false,
        })
    }
}
impl Drop for StageGuard {
    fn drop(&mut self) {
        if !self.installed {
            self.lifecycle.remove_stage(&self.path);
        }
    }
}

/// Removal and its accounting share the same barrier as recount/writes.
pub fn remove_file(path: &Path, budget: Option<&StorageBudget>) -> io::Result<()> {
    let _mutation = budget.map(StorageBudget::mutation);
    let bytes = match file_bytes(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    fs::remove_file(path)?;
    if let Some(budget) = budget {
        budget.removed(bytes);
    }
    Ok(())
}

/// Atomic replacement with peak admission. Small buffered outputs (icons,
/// metadata and parsed lists) use this; streaming outputs use BudgetFile.
pub fn write_atomic(
    dest: &Path,
    part: &Path,
    bytes: &[u8],
    budget: Option<&Arc<StorageBudget>>,
    modified: Option<std::time::SystemTime>,
) -> io::Result<()> {
    write_atomic_mode(dest, part, bytes, budget, modified, false)
}

pub fn write_atomic_private(
    dest: &Path,
    part: &Path,
    bytes: &[u8],
    budget: Option<&Arc<StorageBudget>>,
) -> io::Result<()> {
    write_atomic_mode(dest, part, bytes, budget, None, true)
}

fn write_atomic_mode(
    dest: &Path,
    part: &Path,
    bytes: &[u8],
    budget: Option<&Arc<StorageBudget>>,
    modified: Option<std::time::SystemTime>,
    private: bool,
) -> io::Result<()> {
    let _mutation = budget.map(|budget| budget.mutation());
    let old = existing_file_bytes(dest)?;
    let prior_part = existing_file_bytes(part)?;
    if prior_part > 0 {
        fs::remove_file(part)?;
        if let Some(budget) = budget {
            budget.removed(prior_part);
        }
    }
    let before = dest.parent().map(file_bytes).transpose()?.unwrap_or(0);
    let mut reservation = budget
        .map(|budget| budget.reserve(allocation_for(bytes.len() as u64), false))
        .transpose()
        .map_err(io::Error::other)?;
    let result = (|| {
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                if private {
                    options.mode(0o600);
                }
            }
            #[cfg(not(unix))]
            let _ = private;
            let mut file = options.open(part)?;
            file.write_all(bytes)?;
            if let Some(modified) = modified {
                file.set_modified(modified)?;
            }
            file.sync_all()?;
        }
        fs::rename(part, dest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(part);
    }
    if let Some(reservation) = &mut reservation {
        let retired = result.is_ok();
        if retired {
            budget.unwrap().retire_file(dest, old);
        }
        let new = if retired {
            existing_file_bytes(dest)
        } else {
            existing_file_bytes(part)
        };
        let charged = match (new, dest.parent().map(file_bytes).transpose()) {
            (Ok(bytes), Ok(parent)) => {
                bytes.saturating_add(parent.unwrap_or(0).saturating_sub(before))
            }
            _ => allocation_for(bytes.len() as u64),
        };
        reservation.commit(reservation.bytes(), 0, charged);
    }
    result
}

pub fn replace_file(part: &Path, dest: &Path, budget: Option<&StorageBudget>) -> io::Result<()> {
    let _mutation = budget.map(StorageBudget::mutation);
    let old = existing_file_bytes(dest)?;
    fs::rename(part, dest)?;
    if let Some(budget) = budget {
        budget.retire_file(dest, old);
    }
    Ok(())
}

/// Admit directory creation before mutation, including newly created parents.
pub fn create_directory(path: &Path, budget: Option<&Arc<StorageBudget>>) -> Result<()> {
    let _mutation = budget.map(|budget| budget.mutation());
    create_directory_locked(path, budget)
}

/// Caller holds the budget mutation barrier.
pub fn create_directory_locked(path: &Path, budget: Option<&Arc<StorageBudget>>) -> Result<()> {
    let Some(budget) = budget else {
        fs::create_dir_all(path)?;
        return Ok(());
    };
    let mut missing = Vec::new();
    let mut parent = path;
    loop {
        match fs::symlink_metadata(parent) {
            Ok(_) => break,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                missing.push(parent.to_owned());
                parent = parent
                    .parent()
                    .ok_or_else(|| io::Error::other("directory has no existing parent"))?;
            }
            Err(err) => return Err(err.into()),
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let before = file_bytes(parent)?;
    let mut reservation = budget.reserve(
        allocation_for(BLOCK).saturating_mul(missing.len() as u64),
        false,
    )?;
    let result = fs::create_dir_all(path);
    let mut created = 0u64;
    for dir in &missing {
        match file_bytes(dir) {
            Ok(bytes) => created = created.saturating_add(bytes),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(_) => created = created.saturating_add(allocation_for(0)),
        }
    }
    created = created.saturating_add(
        file_bytes(parent)
            .unwrap_or(before + 4096)
            .saturating_sub(before),
    );
    reservation.commit(reservation.bytes(), 0, created);
    result?;
    Ok(())
}

/// Only for quiescent directories; mapped/open readers must retire first.
pub fn remove_directory(path: &Path, budget: Option<&StorageBudget>) -> io::Result<()> {
    let _mutation = budget.map(StorageBudget::mutation);
    let bytes = match directory_bytes(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let result = fs::remove_dir_all(path);
    let after = match directory_bytes(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
        Err(_) => bytes,
    };
    if let Some(budget) = budget {
        budget.removed(bytes.saturating_sub(after));
    }
    result
}

#[cfg(test)]
mod maintenance_tests {
    use super::*;
    #[test]
    fn limit_changes_apply_immediately_and_hot_checkpoints_do_not_recount() {
        let root = tempfile::tempdir().unwrap();
        let budget = StorageBudget::open(root.path(), 128 * 1024).unwrap();
        let used = budget.status().used_bytes;
        budget.set_limit(used - 1);
        assert!(budget.reserve(1, false).is_err());
        budget.set_limit(used + 8192);
        assert!(budget.reserve(8192, false).is_ok());
        fs::write(root.path().join("untracked"), vec![0; 65536]).unwrap();
        for _ in 0..100 {
            budget.recount_if_due(root.path()).unwrap();
        }
        assert_eq!(budget.status().used_bytes, used);
        budget.recount(root.path()).unwrap();
        assert!(budget.status().used_bytes > used);
    }
    #[test]
    fn deletion_and_recount_are_serialized_without_double_credit() {
        let root = tempfile::tempdir().unwrap();
        let budget = StorageBudget::open(root.path(), 1024 * 1024).unwrap();
        let path = root.path().join("scratch");
        let mut file = BudgetFile::create(&path, Some(budget.clone())).unwrap();
        file.write_all(&[1; 8192]).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let (budget1, root1) = (budget.clone(), root.path().to_owned());
        let recount = std::thread::spawn(move || {
            for _ in 0..100 {
                budget1.recount(&root1).unwrap();
            }
        });
        remove_file(&path, Some(&budget)).unwrap();
        recount.join().unwrap();
        assert_eq!(
            budget.status().used_bytes,
            directory_bytes(root.path()).unwrap()
        );
    }
}
