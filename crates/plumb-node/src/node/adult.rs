//! The adult blocklist safe search leaves out (see [`plumb_core::safe`]).
//!
//! The node downloads the list from [`SeedSources::adult_list_url`] when
//! it has none or its copy is [`REFRESH_AFTER`] old, keeps only whole
//! sites in `DATA/safe/adult-domains.txt`, and holds them in memory as
//! sorted hashes: under 6 MB for the list's few hundred thousand sites.
//!
//! [`SeedSources::adult_list_url`]: super::SeedSources::adult_list_url

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use plumb_core::registrable_domain;
use plumb_core::safe::parse_adult_list;
use plumb_ingest::download;
use tracing::{info, warn};

use super::Inner;

/// The list's folder in the data directory.
const DIR: &str = "safe";
/// The list of whole adult sites, one registrable domain per line.
const FILE: &str = "adult-domains.txt";
/// A list this old is downloaded again.
const REFRESH_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);
/// Wait after a failed download before trying again.
const RETRY_WAIT: Duration = Duration::from_secs(3600);
/// How many more sites a search ranks when the list may leave some out.
pub(super) const MARGIN: usize = 10;
/// How often the job looks at the list's age.
const LOOK_EVERY: Duration = Duration::from_secs(3600);

/// Adult sites, as sorted hashes of their registrable domains.
#[derive(Debug, Default)]
pub(super) struct AdultList(Vec<u64>);

impl AdultList {
    fn new(domains: &[String]) -> Self {
        let mut hashes: Vec<u64> = domains.iter().map(|d| hash(d)).collect();
        hashes.sort_unstable();
        hashes.dedup();
        AdultList(hashes)
    }

    /// Whether the site of `host` (a domain or an address's host) is on the
    /// list.
    pub(super) fn contains(&self, host: &str) -> bool {
        let domain = registrable_domain(host).unwrap_or_else(|| host.to_ascii_lowercase());
        self.0.binary_search(&hash(&domain)).is_ok()
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
}

fn hash(domain: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    domain.hash(&mut hasher);
    hasher.finish()
}

fn list_path(data: &Path) -> PathBuf {
    data.join(DIR).join(FILE)
}

/// Reads the list the node keeps, if it has one.
fn load(data: &Path) -> Option<AdultList> {
    let text = std::fs::read_to_string(list_path(data)).ok()?;
    let domains: Vec<String> = text.lines().map(str::to_string).collect();
    Some(AdultList::new(&domains))
}

/// Keeps the list loaded and fresh until the node stops.
pub(super) async fn run(inner: Arc<Inner>) {
    let data = inner.paths.data.clone();
    if let Some(list) = load(&data) {
        info!("safe search leaves out {} adult sites", list.len());
        inner.set_adult(list);
    }
    let Some(url) = inner.config.sources.adult_list_url.clone() else {
        return;
    };
    let mut stopped = inner.stopped.clone();
    loop {
        let wait = if is_fresh(&data) {
            LOOK_EVERY
        } else {
            match refresh(&url, &data).await {
                Ok(list) => {
                    info!("safe search leaves out {} adult sites", list.len());
                    inner.set_adult(list);
                    LOOK_EVERY
                }
                Err(err) => {
                    warn!("adult blocklist: {err:#}");
                    RETRY_WAIT
                }
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stopped.wait_for(|stop| *stop) => return,
        }
        if inner.stopping() {
            return;
        }
    }
}

fn is_fresh(data: &Path) -> bool {
    std::fs::metadata(list_path(data))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < REFRESH_AFTER)
}

/// Downloads the list from `url` and keeps its whole sites.
async fn refresh(url: &str, data: &Path) -> Result<AdultList> {
    let dir = data.join(DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let download = dir.join("download.tmp");
    let client = download::http_client()?;
    download::download_to_file(&client, url, &download).await?;
    let file = list_path(data);
    tokio::task::spawn_blocking(move || {
        let text = std::fs::read_to_string(&download);
        let _ = std::fs::remove_file(&download);
        let domains = parse_adult_list(&text.context("reading the downloaded list")?);
        anyhow::ensure!(!domains.is_empty(), "the list names no sites");
        let partial = file.with_extension("txt.tmp");
        std::fs::write(&partial, domains.join("\n") + "\n")
            .with_context(|| format!("writing {}", partial.display()))?;
        std::fs::rename(&partial, &file).with_context(|| format!("writing {}", file.display()))?;
        Ok(AdultList::new(&domains))
    })
    .await
    .context("the list task failed")?
}

impl Inner {
    fn set_adult(&self, list: AdultList) {
        *self.adult.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(list));
    }

    /// The adult blocklist, once the node has one.
    pub(super) fn adult_list(&self) -> Option<Arc<AdultList>> {
        self.adult
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_holds_whole_sites() {
        let list = AdultList::new(&["adult.example".into(), "other.example".into()]);
        assert!(list.contains("adult.example"));
        assert!(list.contains("www.adult.example"));
        assert!(list.contains("https://videos.adult.example/x"));
        assert!(!list.contains("example.com"));
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn a_kept_list_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_none());
        assert!(!is_fresh(dir.path()));
        std::fs::create_dir_all(dir.path().join(DIR)).unwrap();
        std::fs::write(list_path(dir.path()), "adult.example\n").unwrap();
        assert!(load(dir.path()).unwrap().contains("adult.example"));
        assert!(is_fresh(dir.path()));
    }
}
