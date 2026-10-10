//! Optional admission for a node's seed-download task. Task-local scope keeps
//! concurrent nodes isolated; CLI/library downloads use their original I/O.
use plumb_core::storage::{BudgetFile, StorageBudget};
use std::future::Future;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

tokio::task_local! { static BUDGET: Option<Arc<StorageBudget>>; }

pub async fn with_budget<F: Future>(budget: Option<Arc<StorageBudget>>, work: F) -> F::Output {
    BUDGET.scope(budget, work).await
}
fn budget() -> Option<Arc<StorageBudget>> {
    BUDGET.try_with(Clone::clone).ok().flatten()
}

pub(crate) async fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    match budget() {
        Some(budget) => plumb_core::storage::create_directory(path.as_ref(), Some(&budget))
            .map_err(io::Error::other),
        None => tokio::fs::create_dir_all(path).await,
    }
}
pub(crate) async fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    match budget() {
        Some(budget) => {
            let mut file = BudgetFile::open_write(path.as_ref(), true, Some(budget))?;
            file.write_all(bytes.as_ref())
        }
        None => tokio::fs::write(path, bytes).await,
    }
}
pub(crate) async fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    match budget() {
        Some(budget) => {
            plumb_core::storage::replace_file(from.as_ref(), to.as_ref(), Some(&budget))
        }
        None => tokio::fs::rename(from, to).await,
    }
}
pub(crate) async fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    match budget() {
        Some(budget) => plumb_core::storage::remove_file(path.as_ref(), Some(&budget)),
        None => tokio::fs::remove_file(path).await,
    }
}
pub(crate) async fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    match budget() {
        Some(budget) => plumb_core::storage::remove_directory(path.as_ref(), Some(&budget)),
        None => tokio::fs::remove_dir_all(path).await,
    }
}

pub(crate) enum OutputFile {
    Plain(tokio::fs::File),
    Admitted(BudgetFile),
}
impl OutputFile {
    pub(crate) async fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open(path.as_ref(), true).await
    }
    pub(crate) async fn open(path: &Path, truncate: bool) -> io::Result<Self> {
        match budget() {
            Some(budget) => Ok(Self::Admitted(BudgetFile::open_write(
                path,
                truncate,
                Some(budget),
            )?)),
            None => Ok(Self::Plain(
                tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(truncate)
                    .open(path)
                    .await?,
            )),
        }
    }
    pub(crate) async fn append(path: &Path) -> io::Result<Self> {
        match budget() {
            Some(budget) => {
                std::fs::metadata(path)?;
                let mut file = BudgetFile::open_write(path, false, Some(budget))?;
                file.seek(io::SeekFrom::End(0))?;
                Ok(Self::Admitted(file))
            }
            None => Ok(Self::Plain(
                tokio::fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .await?,
            )),
        }
    }
    pub(crate) async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Plain(file) => file.write_all(bytes).await,
            Self::Admitted(file) => file.write_all(bytes),
        }
    }
    pub(crate) async fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(file) => file.flush().await,
            Self::Admitted(file) => file.flush(),
        }
    }
    pub(crate) async fn sync_all(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(file) => file.sync_all().await,
            Self::Admitted(file) => file.sync_all(),
        }
    }
    pub(crate) async fn seek(&mut self, position: io::SeekFrom) -> io::Result<()> {
        match self {
            Self::Plain(file) => file.seek(position).await.map(|_| ()),
            Self::Admitted(file) => file.seek(position),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn download_scopes_are_isolated_and_partial_growth_never_exceeds_room() {
        let full = tempfile::tempdir().unwrap();
        let roomy = tempfile::tempdir().unwrap();
        let a = StorageBudget::open(full.path(), 20 * 1024).unwrap();
        let b = StorageBudget::open(roomy.path(), 128 * 1024).unwrap();
        let p = full.path().join("seed.part");
        let q = roomy.path().join("seed.part");
        let (blocked, saved) = tokio::join!(
            with_budget(Some(a.clone()), async {
                let mut file = OutputFile::create(&p).await.unwrap();
                file.write_all(&[1; 4096]).await.unwrap();
                file.write_all(&vec![2; 64 * 1024]).await
            }),
            with_budget(Some(b.clone()), async {
                write(&q, vec![3; 64 * 1024]).await
            })
        );
        assert!(blocked.is_err());
        saved.unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), vec![1; 4096]);
        assert_eq!(std::fs::read(&q).unwrap(), vec![3; 64 * 1024]);
        assert_eq!(a.status().reserved_bytes, 0);
        assert!(
            a.status().used_bytes >= plumb_core::storage::directory_bytes(full.path()).unwrap()
        );
        assert!(a.status().used_bytes <= a.status().limit_bytes);
        assert_eq!(b.status().reserved_bytes, 0);
        let used = a.status().used_bytes;
        with_budget(Some(a.clone()), remove_file(&p)).await.unwrap();
        assert!(a.status().used_bytes < used);
    }
}
