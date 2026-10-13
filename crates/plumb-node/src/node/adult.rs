//! The adult blocklist safe search leaves out (see [`plumb_core::safe`]).
//!
//! The node keeps only whole sites in `DATA/safe/adult-domains.txt` and
//! holds them in memory as sorted hashes: under 6 MB for the list's few
//! hundred thousand sites. When it has none or its copy is
//! [`REFRESH_AFTER`] old, it takes a trusted node's copy over the page set
//! protocol (`/plumb/pages/1`, as [`SHARED_NAME`]) when one has a fresher
//! one, so the network does not ask the list's host once per node. Only
//! when no trusted node has a fresh copy does it download the list from
//! [`SeedSources::adult_list_url`]. A copy taken keeps the time of the
//! copy it came from, so it ages as that one does and the nodes that
//! build from the source still download it once a week.
//!
//! The project also publishes the list as a signed set, [`ADULT_SITES`]:
//! a layer that labels each site `adult`, taken like the denylist
//! ([`super::signed_sets`]). A node that holds a fresh one keeps its
//! sites as its list, dated by the set's generation, and asks no node for
//! the file.
//!
//! [`SeedSources::adult_list_url`]: super::SeedSources::adult_list_url

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use plumb_core::registrable_domain;
use plumb_core::safe::parse_adult_list;
use plumb_ingest::download;
use plumb_net::pages::MAX_PAGES_CHUNK;
use plumb_net::PeerId;
use tracing::{info, warn};

use super::{network, Inner};

/// The name nodes ask each other for the list by, over the page set
/// protocol.
pub(super) const SHARED_NAME: &str = "adult-domains";

/// The project's adult list as a signed set.
pub(super) const ADULT_SITES: &str = "plumb/adult-sites";

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
/// How long a node in the network waits for a trusted node to connect
/// before it downloads the list from its source instead.
#[cfg(not(test))]
const PEER_WAIT: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
const PEER_WAIT: Duration = Duration::from_secs(2);
/// Time between two looks for a trusted node while waiting for one.
#[cfg(not(test))]
const PEER_POLL: Duration = Duration::from_secs(60);
#[cfg(test)]
const PEER_POLL: Duration = Duration::from_millis(200);
/// Wait when a trusted node is busy before asking it again.
#[cfg(not(test))]
const BUSY_WAIT: Duration = Duration::from_secs(5);
#[cfg(test)]
const BUSY_WAIT: Duration = Duration::from_millis(100);
/// Largest list taken from another node; the real one is a few MB.
const MAX_SHARED_BYTES: u64 = 64 << 20;

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
    inner.adult_from_set();
    let url = inner.config.sources.adult_list_url.clone();
    let started = std::time::Instant::now();
    let mut stopped = inner.stopped.clone();
    loop {
        let wait = if is_fresh(&data) {
            LOOK_EVERY
        } else {
            let taken = match from_network(&inner, &data).await {
                Ok(Taken::List(list)) => Some(Ok(list)),
                // No trusted node has connected yet: give them time, so a
                // node just started takes the network's copy.
                Ok(Taken::NoPeer) if trusts_nodes(&inner) && started.elapsed() < PEER_WAIT => None,
                Ok(_) => source(url.as_deref(), &data, inner.storage.clone()).await,
                Err(err) => {
                    warn!("adult blocklist from a trusted node: {err:#}");
                    source(url.as_deref(), &data, inner.storage.clone()).await
                }
            };
            match taken {
                None => PEER_POLL,
                Some(Ok(list)) => {
                    info!("safe search leaves out {} adult sites", list.len());
                    inner.set_adult(list);
                    LOOK_EVERY
                }
                Some(Err(err)) => {
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

/// The list from its source, when the node has one to download it from.
async fn source(
    url: Option<&str>,
    data: &Path,
    budget: Option<Arc<plumb_core::storage::StorageBudget>>,
) -> Option<Result<AdultList>> {
    Some(refresh(url?, data, budget).await)
}

/// Whether the node is in the network and trusts nodes to send it the list.
fn trusts_nodes(inner: &Inner) -> bool {
    network::handle(inner).is_some()
        && inner
            .config
            .network
            .as_ref()
            .is_some_and(|net| !net.trusted_peers.is_empty())
}

/// What asking the trusted nodes gave.
enum Taken {
    /// A fresh list from one of them.
    List(AdultList),
    /// No trusted node that shares files is connected.
    NoPeer,
    /// None of those connected has a fresh copy.
    NoneFresh,
}

/// Takes the list from a connected trusted node whose copy is fresh
/// (younger than [`REFRESH_AFTER`]) and saves it with that copy's time.
async fn from_network(inner: &Inner, data: &Path) -> Result<Taken> {
    let Some(net) = network::handle(inner).cloned() else {
        return Ok(Taken::NoPeer);
    };
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut skip: Vec<PeerId> = Vec::new();
    let mut busy = 0;
    let first = loop {
        let Some(chunk) = net
            .pages_chunk(SHARED_NAME, 0, MAX_PAGES_CHUNK, None, &skip)
            .await?
        else {
            return Ok(if skip.is_empty() {
                Taken::NoPeer
            } else {
                Taken::NoneFresh
            });
        };
        if chunk.busy {
            busy += 1;
            if busy >= 3 {
                skip.push(chunk.peer);
            } else {
                tokio::time::sleep(BUSY_WAIT).await;
            }
            continue;
        }
        let fresh = now.saturating_sub(chunk.modified) < REFRESH_AFTER.as_secs();
        if chunk.size == 0 || !fresh || chunk.size > MAX_SHARED_BYTES {
            skip.push(chunk.peer);
            continue;
        }
        break chunk;
    };
    let (from, size, modified) = (first.peer, first.size, first.modified);
    let mut bytes = first.bytes;
    while (bytes.len() as u64) < size {
        if inner.stopping() {
            bail!("the node is stopping");
        }
        let Some(next) = net
            .pages_chunk(
                SHARED_NAME,
                bytes.len() as u64,
                MAX_PAGES_CHUNK,
                Some(from),
                &[],
            )
            .await?
        else {
            bail!("{from} went away while the list was taken");
        };
        if next.busy {
            tokio::time::sleep(BUSY_WAIT).await;
            continue;
        }
        if next.size != size || next.modified != modified {
            bail!("{from} got a new list while it was taken");
        }
        if next.bytes.is_empty() {
            bail!("{from} sent less of the list than it said it holds");
        }
        bytes.extend_from_slice(&next.bytes);
    }
    let file = list_path(data);
    let budget = inner.storage.clone();
    let list = tokio::task::spawn_blocking(move || {
        save_shared_with_budget(&file, &bytes, modified, budget)
    })
    .await
    .context("the list task failed")??;
    info!("took the adult blocklist from trusted node {from}");
    Ok(Taken::List(list))
}

/// Saves a list another node sent, dated `modified` (Unix seconds) as its
/// copy was. Only lines that are sites are kept.
#[cfg(test)]
fn save_shared(file: &Path, bytes: &[u8], modified: u64) -> Result<AdultList> {
    save_shared_with_budget(file, bytes, modified, None)
}

fn save_shared_with_budget(
    file: &Path,
    bytes: &[u8],
    modified: u64,
    budget: Option<Arc<plumb_core::storage::StorageBudget>>,
) -> Result<AdultList> {
    let text = std::str::from_utf8(bytes).context("the list is not text")?;
    let domains = parse_adult_list(text);
    anyhow::ensure!(!domains.is_empty(), "the list names no sites");
    let dir = file.parent().context("the list has no folder")?;
    plumb_core::storage::create_directory(dir, budget.as_ref())
        .with_context(|| format!("creating {}", dir.display()))?;
    let partial = file.with_extension("txt.tmp");
    plumb_core::storage::write_atomic(
        file,
        &partial,
        (domains.join("\n") + "\n").as_bytes(),
        budget.as_ref(),
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(modified)),
    )
    .with_context(|| format!("writing {}", file.display()))?;
    Ok(AdultList::new(&domains))
}

/// The list's file, for sending to other nodes; `None` when the node has
/// none.
pub(super) fn shared_file(data: &Path) -> Option<PathBuf> {
    Some(list_path(data)).filter(|path| path.is_file())
}

fn is_fresh(data: &Path) -> bool {
    std::fs::metadata(list_path(data))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < REFRESH_AFTER)
}

/// Downloads the list from `url` and keeps its whole sites.
async fn refresh(
    url: &str,
    data: &Path,
    budget: Option<Arc<plumb_core::storage::StorageBudget>>,
) -> Result<AdultList> {
    let dir = data.join(DIR);
    plumb_core::storage::create_directory(&dir, budget.as_ref())
        .with_context(|| format!("creating {}", dir.display()))?;
    let download = dir.join("download.tmp");
    let client = download::http_client()?;
    crate::meaning::download_file_with_budget(&client, url, &download, budget.clone()).await?;
    let file = list_path(data);
    tokio::task::spawn_blocking(move || {
        let text = std::fs::read_to_string(&download);
        let _ = plumb_core::storage::remove_file(&download, budget.as_deref());
        let domains = parse_adult_list(&text.context("reading the downloaded list")?);
        anyhow::ensure!(!domains.is_empty(), "the list names no sites");
        let partial = file.with_extension("txt.tmp");
        plumb_core::storage::write_atomic(
            &file,
            &partial,
            (domains.join("\n") + "\n").as_bytes(),
            budget.as_ref(),
            None,
        )
        .with_context(|| format!("writing {}", file.display()))?;
        Ok(AdultList::new(&domains))
    })
    .await
    .context("the list task failed")?
}

/// The adult list set held in `data`, with its generation (Unix seconds),
/// when a publisher the node trusts signed it.
fn held_set(data: &Path, publishers: &[plumb_format::PublicKey]) -> Option<(PathBuf, u64)> {
    let (dir, manifest) = super::signed_sets::held_trusted(data, ADULT_SITES, publishers)?;
    let generation = plumb_format::value::unix_seconds(&manifest.generation)?;
    Some((dir, generation.max(0) as u64))
}

/// The sites the adult list set in `dir` names.
fn set_sites(dir: &Path) -> Option<Vec<String>> {
    let (_, items) = crate::sets::read_items(dir).ok()?;
    let domains = items
        .iter()
        .filter(|item| !item.is_deleted() && item.labels.iter().any(|label| label == "adult"))
        .filter_map(|item| match &item.extends {
            Some(plumb_format::Ref::Site(domain)) => Some(domain.clone()),
            _ => None,
        })
        .collect();
    Some(domains)
}

impl Inner {
    /// Makes the adult list set held the node's list when it is newer than
    /// the list's file: saved as the file, dated by its generation.
    pub(super) fn adult_from_set(&self) {
        let data = &self.paths.data;
        let Some((dir, generation)) = held_set(data, &self.config.set_publishers) else {
            return;
        };
        let file_time = std::fs::metadata(list_path(data))
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_secs());
        // Before reading the set's items, which are the whole list.
        if generation <= file_time {
            return;
        }
        let Some(domains) = set_sites(&dir) else {
            return;
        };
        let text = domains.join("\n") + "\n";
        match save_shared_with_budget(
            &list_path(data),
            text.as_bytes(),
            generation,
            self.storage.clone(),
        ) {
            Ok(list) => {
                info!(
                    "safe search leaves out {} adult sites, from {ADULT_SITES}",
                    list.len()
                );
                self.set_adult(list);
            }
            Err(err) => warn!("{ADULT_SITES}: {err:#}"),
        }
    }

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
    #[test]
    fn adult_list_replacement_past_the_storage_limit_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let file = list_path(dir.path());
        save_shared(&file, b"adult.example\n", 1_700_000_000).unwrap();
        let budget = plumb_core::storage::StorageBudget::open(dir.path(), 1).unwrap();
        // The limit is a target, as in 0.2.0: the newer list is kept.
        save_shared_with_budget(
            &file,
            b"new-adult.example\n",
            1_700_000_001,
            Some(budget.clone()),
        )
        .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"new-adult.example\n");
        assert!(!file.with_extension("txt.tmp").exists());
        assert_eq!(budget.status().reserved_bytes, 0);
        assert_eq!(budget.status().rejected_writes, 0);
    }

    use super::*;

    #[test]
    fn a_held_adult_set_names_its_sites_when_a_trusted_publisher_signed_it() {
        use ed25519_dalek::SigningKey;
        use plumb_format::PublicKey;
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("porn-nl.txt");
        std::fs::write(&list, "0.0.0.0 adult.example\nnot a site\nother.example\n").unwrap();
        let out = super::super::signed_sets::set_dir(dir.path(), ADULT_SITES).unwrap();
        plumb_tools::set::adult_sites(&plumb_tools::set::AdultSitesArgs {
            list,
            name: ADULT_SITES.into(),
            out: out.clone(),
            license: "Unlicense".into(),
            attribution: "From the Block List Project".into(),
            level: 3,
        })
        .unwrap();
        let key = SigningKey::from_bytes(&[7; 32]);
        plumb_tools::set::sign_set(&out, &key, None, None).unwrap();
        let (held, generation) = held_set(dir.path(), &[PublicKey::of(&key)]).unwrap();
        let mut domains = set_sites(&held).unwrap();
        domains.sort();
        assert_eq!(domains, ["adult.example", "other.example"]);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(now.abs_diff(generation) < 600, "{generation}");
        let stranger = PublicKey::of(&SigningKey::from_bytes(&[8; 32]));
        assert!(held_set(dir.path(), &[stranger]).is_none());
    }

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

    #[test]
    fn a_list_from_another_node_keeps_its_time() {
        let dir = tempfile::tempdir().unwrap();
        assert!(shared_file(dir.path()).is_none());
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let file = list_path(dir.path());
        let list = save_shared(
            &file,
            b"adult.example\nnot a site\nother.example\n",
            now - 3600,
        )
        .unwrap();
        assert_eq!(list.len(), 2);
        assert!(is_fresh(dir.path()));
        assert_eq!(shared_file(dir.path()), Some(file.clone()));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "adult.example\nother.example\n"
        );
        // A copy that was already old at the node it came from stays old,
        // so the node goes back for a fresh one.
        let old = now - REFRESH_AFTER.as_secs() - 60;
        save_shared(&file, b"adult.example\n", old).unwrap();
        assert!(!is_fresh(dir.path()));
        assert!(save_shared(&file, b"nothing here\n", now).is_err());
    }
}
