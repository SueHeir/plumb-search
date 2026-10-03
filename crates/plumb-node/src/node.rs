//! A long-running node: the web page plus background work that sets up the
//! index on first start and keeps it growing. `plumb run` uses it, and so
//! does the desktop app, which embeds a node in-process.
//!
//! # What a node does
//!
//! 1. On first start, when `DIR/records.jsonl` is missing, it downloads the
//!    Tranco list alone, keeps its best [`NodeConfig::sites`] sites, writes
//!    the records file and builds a first index, which is searchable from
//!    then on, a minute or two after starting. After its first crawl (2), it
//!    downloads the rest of the seed data, which takes many minutes (half an
//!    hour or more when Wikidata is busy): Wikidata's official
//!    websites and, when [`NodeConfig::cc_release`] is set, the top rows of
//!    Common Crawl's domain ranks. It keeps the best sites of all three,
//!    replaces the quick records with them and swaps in a new index. Until
//!    then the status says Wikidata is missing. Wikidata is the one source
//!    the node can do without: when only it fails, the others are folded in,
//!    and the node keeps trying to get it (waiting as after other failures,
//!    below) while it goes on with its work. Once Wikidata answers, its
//!    official websites are added to the records and the index is rebuilt.
//! 2. Right after the first index, it crawls [`NodeConfig::initial_crawl`]
//!    homepages, rebuilds the index and swaps the new one in. This comes
//!    before the rest of the seed data so that a node in the network has
//!    crawls to share within minutes. A paused node gets the seed data
//!    first.
//! 3. Every [`NodeConfig::refresh_every`] it crawls
//!    [`NodeConfig::crawl_per_refresh`] more homepages, rebuilds and swaps
//!    again. As with `plumb crawl`, half of each round goes to sites never
//!    crawled and half to sites due again (30 days after their last visit,
//!    sooner for sites that could not be reached), best-ranked first, so the
//!    node keeps reaching new sites while it refreshes the best ones.
//!
//! Until the first index is ready, `/` and `/search` show the setup step,
//! its progress and the last error, reloading every 5 seconds;
//! `GET /api/status` reports a [`Status`] as JSON at any time. Searches use
//! the newest index as soon as it is swapped in.
//!
//! A failure, such as no network, a host that refuses us or a full disk,
//! never stops a node: it is shown on the setup page and in `/api/status`,
//! and the work is tried again after [`NodeConfig::retry_wait`] (10 minutes),
//! doubling after each failure in a row up to [`NodeConfig::max_retry_wait`]
//! (6 hours). A batch of homepages that nearly all fail counts as such a
//! failure (the network is down, or a proxy is needed; see
//! [`NodeConfig::use_system_proxy`]) and is not saved. After a restart a node
//! picks up where it left off: setup is not repeated, a crawl that was cut
//! short goes on, and the next refresh falls due on schedule.
//!
//! # Data directory
//!
//! ```text
//! DIR/
//!   node.lock                locked while a node runs, so two never share DIR
//!   state.json               progress that survives restarts
//!   records.jsonl            every known site, one JSON line each
//!   records.jsonl.journal    crawl results not yet folded into records.jsonl
//!   seed/                    first-start downloads; may be deleted once
//!                            records.jsonl exists
//!   indexes/000001/          a complete search index
//!   indexes/000002/          ...the newest one that opens is searched
//!   icons/3f/example.com.png site icons for results pages (see
//!                            [`crate::icons`]); empty when a site had none
//! ```
//!
//! The node owns the directory. The records and state files are replaced
//! atomically (written to a temporary file and flushed to disk, then
//! renamed), so a crash or a power cut leaves the previous version, never a
//! half-written one, and downloads only take their final name once complete.
//! Crawls append each batch to the journal instead of rewriting the records
//! file, and fold it in once it reaches a quarter of the file's size (see
//! [`crate::records`]); the journal is replayed whenever the records are
//! read. Each index build goes
//! into a new numbered directory, by way of a hidden staging directory, and
//! is never renamed or changed after that; older indexes are deleted once no
//! search has them open (Windows refuses to delete open files), and a failed
//! deletion is retried later. Leftovers of interrupted work (staging
//! directories, temporary files, partial downloads) are removed at startup.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use plumb_core::now_unix;
use plumb_index::{Hit, RankConfig, SearchOptions, SearchResults, Searcher};
use plumb_ingest::download;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::country::HomeCountry;
use crate::meaning::SharedMeaning;
use crate::web::{self, IndexBackend, SearchBackend, StatusSource};
use crate::websearch::{Engine, WebSettings};

pub mod backup;
pub mod control;
mod embedding;
pub mod features;
pub mod journal;
mod network;
pub mod schedule;
mod store;
mod worker;

#[cfg(test)]
mod tests;

pub use journal::{LogEntry, LogLevel};
pub use schedule::{CrawlHours, Workload};
use store::{DirLock, Paths, SavedState};

/// How long a count of the data folder's size is used before counting again.
const DISK_COUNT_MAX_AGE: Duration = Duration::from_secs(30);

/// How long [`NodeHandle::shutdown`] lets open requests finish before it
/// closes their connections.
const SERVER_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// What a long-running node does, and how much.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeConfig {
    /// Holds the downloads, the records file and the search index.
    pub data_dir: PathBuf,
    /// Where the web page and JSON API listen. Port 0 picks a free port.
    pub bind: SocketAddr,
    /// How many of the best-ranked sites to keep from the seed data.
    pub sites: usize,
    /// Homepages to crawl right after the first index is built.
    pub initial_crawl: usize,
    /// How often to crawl more homepages and rebuild the index; `None` turns
    /// refreshing off.
    pub refresh_every: Option<Duration>,
    /// Homepages crawled per refresh.
    pub crawl_per_refresh: usize,
    /// Fetch homepages through the system proxy (`HTTP_PROXY`, `HTTPS_PROXY`
    /// or `ALL_PROXY`, except hosts in `NO_PROXY`), for machines that reach
    /// the internet only through one. Off by default: homepages are fetched
    /// directly, which lets the crawler refuse sites whose names lead to
    /// private networks, something it cannot check through a proxy. Seed
    /// downloads always use these variables.
    pub use_system_proxy: bool,
    /// Common Crawl web graph release to take domain ranks from, such as
    /// `cc-main-2025-26-nov-dec-jan`; only the rows needed are downloaded.
    pub cc_release: Option<String>,
    /// Weight of the popularity prior in the ranking; `None` uses the index default.
    pub alpha: Option<f32>,
    /// The home country of searches that do not name one.
    pub country: HomeCountry,
    /// The web search engine the results page links to; `None` for no link.
    pub web_search: Option<Engine>,
    /// Rank by meaning too, for searches that name no site: the node
    /// downloads a small embedding model into `DIR/model` (about 130 MB)
    /// and keeps a vector of each site's text in `DIR/vectors.bin`, made in
    /// the background after each index build, best-ranked sites first.
    pub search_by_meaning: bool,
    /// Where the seed data is downloaded from on first start.
    pub sources: SeedSources,
    /// How long to wait before trying failed work again. The wait doubles
    /// after each failure in a row, up to `max_retry_wait`.
    pub retry_wait: Duration,
    /// The longest wait between two tries of failed work.
    pub max_retry_wait: Duration,
    /// Join the Plumb network: share crawl work with other nodes and answer
    /// their searches (see [`network`]). Its `dir` is replaced with
    /// `DIR/net`. `None`, the default for now, keeps the node on its own.
    pub network: Option<plumb_net::NetConfig>,
    /// Serve private search (`/private`): browsers fetch buckets of sites
    /// and rank them themselves, so the node never sees their searches.
    /// Each index build also writes its buckets, which take about as much
    /// disk as the records file. Off by default.
    pub private_search: bool,
    /// Share which result people open for a search, anonymously, so the
    /// network learns what is popular (see [`network`]). Needs `network`.
    /// Off by default.
    pub share_popularity: bool,
    /// The settings until someone changes them on the panel, which saves
    /// them in `DIR/settings.json`.
    pub settings: NodeSettings,
    /// Let the panel control other nodes too ("Connect to a node"), with
    /// their remote control tokens, kept in `DIR/remote-nodes.json`. On for
    /// the desktop app, which is meant to be the control center of a
    /// person's nodes; off for servers.
    pub manage_other_nodes: bool,
}

impl NodeConfig {
    /// Defaults for a server or homelab: 1,000,000 sites, 10,000 homepages
    /// crawled at first and 5,000 more every 24 hours, on 127.0.0.1:8080.
    pub fn server(data_dir: PathBuf) -> Self {
        NodeConfig {
            data_dir,
            bind: SocketAddr::from(([127, 0, 0, 1], 8080)),
            sites: 1_000_000,
            initial_crawl: 10_000,
            refresh_every: Some(Duration::from_secs(24 * 60 * 60)),
            crawl_per_refresh: 5_000,
            use_system_proxy: false,
            cc_release: None,
            alpha: None,
            country: HomeCountry::Auto,
            web_search: None,
            search_by_meaning: false,
            sources: SeedSources::default(),
            retry_wait: Duration::from_secs(10 * 60),
            max_retry_wait: Duration::from_secs(6 * 60 * 60),
            network: None,
            private_search: false,
            share_popularity: false,
            settings: NodeSettings::default(),
            manage_other_nodes: false,
        }
    }

    /// Defaults for a desktop: 250,000 sites, 2,000 homepages crawled at
    /// first and 1,000 more every 12 hours, on 127.0.0.1 with a free port.
    pub fn desktop(data_dir: PathBuf) -> Self {
        NodeConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            sites: 250_000,
            initial_crawl: 2_000,
            refresh_every: Some(Duration::from_secs(12 * 60 * 60)),
            crawl_per_refresh: 1_000,
            settings: NodeSettings::desktop(),
            manage_other_nodes: true,
            ..NodeConfig::server(data_dir)
        }
    }

    /// The Common Crawl ranks file to take the top rows of, if any.
    fn cc_ranks_url(&self) -> Option<String> {
        self.sources.cc_ranks_url.clone().or_else(|| {
            self.cc_release
                .as_deref()
                .map(|release| download::cc_domain_ranks_url(release.trim()))
        })
    }

    /// Catches settings that cannot work before anything is started.
    fn check(&self) -> Result<()> {
        if self.sites == 0 {
            bail!("sites must be at least 1");
        }
        if self.share_popularity && self.network.is_none() {
            bail!("sharing popularity needs the network");
        }
        if let Some(alpha) = self.alpha {
            if !(0.0..=1.0).contains(&alpha) {
                bail!("alpha must be from 0 to 1, got {alpha}");
            }
        }
        if let Some(release) = &self.cc_release {
            check_release_name(release)?;
        }
        if self.retry_wait.is_zero() || self.max_retry_wait < self.retry_wait {
            bail!("retry_wait must be above 0 and at most max_retry_wait");
        }
        if self.refresh_every.is_some_and(|every| every.is_zero()) {
            bail!("refresh_every must be above 0; use None to turn refreshing off");
        }
        Ok(())
    }
}

/// Fails unless `release` looks like a web graph release name, such as
/// `cc-main-2025-26-nov-dec-jan`, so that a pasted URL or path is caught
/// before it is spliced into a URL.
pub(crate) fn check_release_name(release: &str) -> Result<()> {
    let release = release.trim();
    let well_formed = !release.is_empty()
        && release
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !well_formed {
        bail!(
            "a Common Crawl release is a name such as cc-main-2025-26-nov-dec-jan \
             (listed on https://commoncrawl.org/web-graphs), got {release:?}"
        );
    }
    Ok(())
}

/// Where a node downloads its seed data on first start. The default is the
/// public datasets; a mirror, or a test server, can stand in for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedSources {
    /// The Tranco list: a zip holding a CSV, as Tranco serves it, or a plain
    /// or gzipped CSV.
    pub tranco_url: String,
    /// A SPARQL endpoint that answers Wikidata queries.
    pub wikidata_sparql_url: String,
    /// Only Wikidata items with at least this many Wikipedia sitelinks are
    /// fetched, which keeps the download small enough to finish (see
    /// [`download::download_wikidata_official_sites`]).
    pub wikidata_min_sitelinks: u32,
    /// How the Wikidata queries are spaced out: the pause between two and
    /// the wait before trying one again.
    pub wikidata_pacing: download::WikidataPacing,
    /// A Common Crawl domain ranks file to read instead of the one of
    /// [`NodeConfig::cc_release`]; when set, Common Crawl ranks are used
    /// even without a release.
    pub cc_ranks_url: Option<String>,
    /// Where the embedding model's files are downloaded from, for search by
    /// meaning: each of [`plumb_embed::MODEL_FILES`] is appended.
    pub model_base_url: String,
}

impl Default for SeedSources {
    fn default() -> Self {
        SeedSources {
            tranco_url: download::TRANCO_LATEST_URL.to_string(),
            wikidata_sparql_url: download::WIKIDATA_SPARQL_URL.to_string(),
            wikidata_min_sitelinks: 25,
            wikidata_pacing: download::WikidataPacing::default(),
            cc_ranks_url: None,
            model_base_url: plumb_embed::MODEL_BASE_URL.to_string(),
        }
    }
}

/// What the person running a node chose, on the settings panel (`/app`).
/// Kept in `DIR/settings.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeSettings {
    /// Crawl homepages and rebuild the index in the background: the crawl
    /// after setup and the scheduled refreshes. Off pauses them (after the
    /// batch of homepages under way); setup still finishes.
    pub background_updates: bool,
    /// Megabytes the crawls may download per day (UTC), 0 for no limit.
    /// Once a day's downloads reach it, crawling pauses until the next
    /// day. Setup's downloads count, but are never held back.
    pub download_limit_mb_per_day: u64,
    /// Megabytes the data folder may take, 0 for no limit. Above it,
    /// crawling pauses, since crawls add sites; search keeps working.
    pub storage_limit_mb: u64,
    /// How hard to crawl; a preset also sets the two limits above.
    pub workload: Workload,
    /// Crawl only during these hours of the node's own clock; `None` for
    /// any time.
    pub crawl_hours: Option<CrawlHours>,
    /// Crawling is paused until this Unix time ("pause for an hour").
    pub paused_until: Option<u64>,
}

impl Default for NodeSettings {
    fn default() -> Self {
        NodeSettings {
            background_updates: true,
            download_limit_mb_per_day: 0,
            storage_limit_mb: 0,
            workload: Workload::Custom,
            crawl_hours: None,
            paused_until: None,
        }
    }
}

impl NodeSettings {
    /// Defaults for a desktop: 500 MB of downloads a day and 2 GB of disk.
    pub fn desktop() -> Self {
        NodeSettings {
            download_limit_mb_per_day: 500,
            storage_limit_mb: 2_000,
            workload: Workload::Balanced,
            ..NodeSettings::default()
        }
    }

    /// Takes on `workload`, and its limits when it is a preset.
    pub fn set_workload(&mut self, workload: Workload) {
        self.workload = workload;
        if let Some((download, storage)) = workload.limits() {
            self.download_limit_mb_per_day = download;
            self.storage_limit_mb = storage;
        }
    }
}

/// Bytes in a megabyte, as the limits count them.
pub const MB: u64 = 1_000_000;

/// What a node is doing, as `GET /api/status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// `setting_up` until the first index is searchable, then `ready`.
    pub phase: Phase,
    /// The work under way.
    pub step: Step,
    /// The work under way, in words, e.g. "Crawling homepages".
    pub detail: String,
    /// How far the step has come, when that can be counted.
    pub progress: Option<Progress>,
    /// The latest failure; cleared once the work that failed succeeds.
    pub last_error: Option<LastError>,
    /// True while the index lacks Wikidata's official websites: right
    /// after the quick first setup, while they download, and after a failure
    /// to download them (see `wikidata_error`). The node keeps trying and rebuilds
    /// the index once they arrive; until then, official websites get no
    /// boost over look-alikes and no names from Wikidata.
    pub wikidata_missing: bool,
    /// The last failure to download Wikidata's official websites, and when
    /// the node tries again; `None` once they are in.
    pub wikidata_error: Option<LastError>,
    /// Sites in the index being searched; 0 while setting up.
    pub sites: u64,
    /// The directory under `indexes/` of the index being searched.
    pub index: Option<String>,
    /// When the last refresh (a round of crawling and the rebuild after it)
    /// ended, in Unix seconds.
    pub last_refresh: Option<u64>,
    /// When the next refresh is due, in Unix seconds; `None` when refreshing
    /// is off or the initial crawl has not ended yet.
    pub next_refresh: Option<u64>,
    /// The version of Plumb running the node.
    pub version: String,
    /// The node's place in the Plumb network, when it has joined it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<plumb_net::NetStatus>,
    /// Homepages still to crawl in the round under way (the crawl after
    /// setup or a refresh); 0 when none is under way.
    pub crawl_left: u64,
    /// [`NodeSettings::background_updates`].
    pub background_updates: bool,
    /// Why crawling is paused, in words, when it is: background updates
    /// off, paused for a while, outside the crawl hours, or a download or
    /// storage limit reached.
    pub paused: Option<String>,
    /// When that pause ends by itself, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_until: Option<u64>,
    /// Bytes the data folder takes, counted at most a minute ago.
    pub disk_used: u64,
    /// Bytes downloaded today (UTC): crawls and seed data.
    pub downloaded_today: u64,
    /// Bytes downloaded since the node was set up.
    pub downloaded_total: u64,
    /// Homepages visited since the node was set up.
    pub homepages_visited: u64,
    /// Sites with a vector for search by meaning, when it is on and its
    /// model is loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meaning_sites: Option<u64>,
    /// What search by meaning is doing in the background: downloading its
    /// model, making site vectors, or waiting after a failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meaning_work: Option<BackgroundWork>,
    /// Whether the node can be restarted from the panel, to apply saved
    /// feature changes (the desktop app's node can).
    #[serde(default)]
    pub can_restart: bool,
}

/// Work going on beside the main step, for the panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundWork {
    /// What it is doing, in words.
    pub detail: String,
    pub progress: Option<Progress>,
    /// The last failure, when it waits to try again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<LastError>,
}

/// Whether a node can search yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// No index yet: the node is downloading seed data or building its first index.
    SettingUp,
    /// An index is being searched. Crawls and rebuilds go on in the background.
    Ready,
}

/// The work a node is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Opening the data directory.
    Starting,
    /// Downloading seed data.
    Downloading,
    /// Folding the seed data into site records.
    Ingesting,
    /// Building a search index.
    Indexing,
    /// Fetching homepages.
    Crawling,
    /// Waiting to try failed work again; see [`Status::last_error`].
    Retrying,
    /// Nothing to do until the next refresh.
    Idle,
    /// Shutting down.
    Stopping,
}

/// How far a step has come: `done` of `total` `unit`s, e.g. 1,500 of 10,000 homepages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    /// What is counted, plural: `files`, `homepages`.
    pub unit: String,
}

/// A failure the node is waiting to retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastError {
    /// What went wrong, with its causes.
    pub message: String,
    /// When it happened, in Unix seconds.
    pub at: u64,
    /// When the work is tried again, in Unix seconds.
    pub retry_at: Option<u64>,
}

/// A running node. Dropping the handle leaves the node running until the
/// runtime shuts down; call [`NodeHandle::shutdown`] to stop it cleanly.
#[derive(Debug)]
pub struct NodeHandle {
    addr: SocketAddr,
    inner: Arc<Inner>,
    stop: watch::Sender<bool>,
    server: JoinHandle<std::io::Result<()>>,
    worker: JoinHandle<()>,
    embedding: Option<JoinHandle<()>>,
}

impl NodeHandle {
    /// The address the web page is served on (the real port when 0 was asked for).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://<addr>/`.
    pub fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    /// What the node is doing: the report `GET /api/status` serves.
    pub fn status(&self) -> Status {
        self.inner.status()
    }

    /// Starts a refresh now rather than at its scheduled time: crawls
    /// [`NodeConfig::crawl_per_refresh`] homepages and rebuilds the index,
    /// even when nothing was crawled. A node waiting to retry failed work
    /// retries right away instead. During setup or a refresh already under
    /// way, the request is folded into the work in progress.
    pub fn refresh_now(&self) {
        self.inner.request_refresh();
    }

    /// Asks for the node to be restarted, which the panel offers once a
    /// program waits on this signal: the desktop app stops the node and
    /// starts it again with the saved features.
    pub fn restart_signal(&self) -> RestartSignal {
        self.inner.restart.0.listening.store(true, Ordering::SeqCst);
        self.inner.restart.clone()
    }

    /// Stops the web server and the background work and waits for both. A
    /// crawl or index build in progress stops at its next safe point, so the
    /// records file and the index on disk are never left half-written.
    ///
    /// A crawl stops between two homepages, giving up the unsaved part of
    /// its batch, which is crawled again later. An index build cannot be cut
    /// short, so one under way is finished first; that takes about a minute
    /// for a million sites.
    pub async fn shutdown(self) -> Result<()> {
        let NodeHandle {
            inner,
            stop,
            mut server,
            worker,
            embedding,
            ..
        } = self;
        info!("stopping the node in {}", inner.paths.data.display());
        stop.send_replace(true);

        let served = match tokio::time::timeout(SERVER_STOP_TIMEOUT, &mut server).await {
            Ok(joined) => joined
                .context("the web server task failed")
                .and_then(|served| served.context("serving HTTP")),
            Err(_) => {
                warn!(
                    "open requests did not finish within {} seconds; closing them",
                    SERVER_STOP_TIMEOUT.as_secs()
                );
                server.abort();
                Ok(())
            }
        };
        let worked = worker.await.context("the background work failed");
        if let Some(embedding) = embedding {
            if let Err(err) = embedding.await {
                warn!("search by meaning failed: {err}");
            }
        }
        network::stop(&inner).await;
        // Only now may another node take over the data directory.
        drop(
            inner
                .lock
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        inner.journal.info("Stopped");
        info!("node stopped");
        served.and(worked)
    }
}

/// Has the node in `data_dir` fold the seed files in `DIR/seed` into its
/// records again at its next start, as it does when Wikidata's arrive late:
/// files from the last week are used as they are, older or missing ones are
/// downloaded again. For a node whose records predate a change to how seed
/// data is read. Returns false, changing nothing, when the directory has no
/// node yet. Fails while another node holds the directory.
pub fn request_reseed(data_dir: &Path) -> Result<bool> {
    let paths = Paths::new(data_dir);
    if !paths.records.is_file() {
        return Ok(false);
    }
    let _lock = store::lock(&paths)?;
    let Some(mut state) = store::load_state(&paths) else {
        return Ok(false);
    };
    state.wikidata_missing = true;
    store::save_state(&paths, &state)?;
    Ok(true)
}

/// Starts a node on the current Tokio runtime and returns once the web
/// server is listening. Setup and refreshes run in the background; until the
/// first index is ready, the web page shows setup progress instead of a
/// search box. A records file already in `data_dir` skips the downloads.
///
/// Fails when the data directory cannot be created or another node is using
/// it, when the address cannot be bound, or when the configuration cannot
/// work (such as an alpha above 1). A multi-threaded runtime is best: crawls
/// and index builds run on its blocking threads.
pub async fn start(mut config: NodeConfig) -> Result<NodeHandle> {
    crate::limits::raise_open_file_limit();
    if let Some(features) = features::FeatureSettings::load(&config.data_dir)? {
        features.apply(&mut config)?;
    }
    config.check()?;
    let rank = crate::rank_config(config.alpha);
    let opened = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || open_data_dir(&config, rank))
            .await
            .context("opening the data directory")??
    };
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("listening on {}", config.bind))?;
    let addr = listener
        .local_addr()
        .context("reading the address listened on")?;

    let (stop, stopped) = watch::channel(false);
    let inner = Arc::new(Inner::new(config, rank, opened, stopped.clone()));
    inner.journal.info(format!(
        "Plumb Search {} started",
        env!("CARGO_PKG_VERSION")
    ));
    let settings = WebSettings {
        home: inner.config.country.clone(),
        web_search: inner.config.web_search,
    };
    let app = web::node_router_with(inner.clone(), inner.clone(), settings);
    let server = tokio::spawn(async move {
        let mut stopped = stopped;
        // The settings panel takes changes only from this computer.
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = stopped.wait_for(|&stop| stop).await;
        })
        .await
    });
    if let Err(err) = network::start(&inner).await {
        // The node still searches and crawls on its own.
        warn!("{err:#}");
    }
    let worker = tokio::spawn(worker::run(inner.clone()));
    let embedding = inner.config.search_by_meaning.then(|| {
        let inner = inner.clone();
        tokio::task::spawn_blocking(move || embedding::run(inner))
    });
    info!(
        "serving http://{addr}/ with data in {}",
        inner.paths.data.display()
    );
    Ok(NodeHandle {
        addr,
        inner,
        stop,
        server,
        worker,
        embedding,
    })
}

/// What [`open_data_dir`] found.
struct Opened {
    paths: Paths,
    lock: Option<DirLock>,
    saved: SavedState,
    settings: NodeSettings,
    index: Option<ServingIndex>,
    /// Index directories that could not be deleted yet.
    leftover: Vec<Retired>,
}

/// Creates and locks the data directory, clears leftovers, reads the saved
/// state and opens the newest index that opens. Older indexes, and newer
/// ones that do not open, are deleted.
fn open_data_dir(config: &NodeConfig, rank: RankConfig) -> Result<Opened> {
    let paths = Paths::new(&config.data_dir);
    std::fs::create_dir_all(&paths.indexes)
        .with_context(|| format!("creating {}", paths.indexes.display()))?;
    let lock = store::lock(&paths)?;
    store::remove_leftovers(&paths);
    let mut saved =
        store::load_state(&paths).unwrap_or_else(|| SavedState::fresh(config.initial_crawl));

    let mut index = None;
    let mut leftover = Vec::new();
    let mut broken = false;
    for id in store::index_ids(&paths) {
        let dir = paths.index(id);
        if index.is_none() {
            match ServingIndex::open(id, &dir, rank) {
                Ok(opened) => {
                    info!("searching {} sites in {}", opened.docs, dir.display());
                    index = Some(opened);
                    continue;
                }
                Err(err) => {
                    warn!("cannot use the index in {}: {err:#}", dir.display());
                    broken = true;
                }
            }
        }
        if let Err(err) = store::remove_index(&dir) {
            warn!("cannot delete the old index {}: {err}", dir.display());
            leftover.push(Retired::closed(dir));
        }
    }
    if broken && !saved.index_stale {
        // The records may hold changes that only the broken index had, so
        // the next index is built from them, even after a restart.
        saved.index_stale = true;
        if let Err(err) = store::save_state(&paths, &saved) {
            warn!("{err:#}");
        }
    }
    let settings = store::load_settings(&paths).unwrap_or_else(|| config.settings.clone());
    Ok(Opened {
        paths,
        lock,
        saved,
        settings,
        index,
        leftover,
    })
}

/// Raised inside the background work when the node is shutting down.
#[derive(Debug)]
struct Stopped;

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the node is shutting down")
    }
}

impl std::error::Error for Stopped {}

/// The state shared by the web server, the background work and the handle.
struct Inner {
    config: NodeConfig,
    paths: Paths,
    rank: RankConfig,
    activity: Mutex<Activity>,
    saved: Mutex<SavedState>,
    /// The index searched; `None` until the first one is ready.
    index: RwLock<Option<Arc<ServingIndex>>>,
    /// Replaced indexes whose directories are still to be deleted.
    retired: Mutex<Vec<Retired>>,
    stopped: watch::Receiver<bool>,
    /// Wakes the background work for a refresh request.
    wake: Notify,
    refresh_requested: AtomicBool,
    lock: Mutex<Option<DirLock>>,
    /// Tries at Wikidata's official websites while setup went on without
    /// them ([`SavedState::wikidata_missing`]).
    wikidata: Mutex<WikidataTries>,
    /// The network side, once joined.
    net: std::sync::OnceLock<Arc<plumb_net::NetHandle>>,
    /// Records in the network inbox not yet folded in.
    inbox_records: std::sync::atomic::AtomicU64,
    /// Held while the inbox is appended to or moved aside.
    inbox_lock: Mutex<()>,
    /// Set once the index was rebuilt to add missing buckets.
    buckets_rebuilt: AtomicBool,
    /// The results opened this week, when sharing popularity.
    picks: Mutex<Option<plumb_net::PickLog>>,
    settings: Mutex<NodeSettings>,
    /// The last count of the data folder's size, and when it was made.
    disk: Mutex<Option<(std::time::Instant, u64)>>,
    /// The model and vectors of search by meaning, once loaded.
    meaning: SharedMeaning,
    /// What search by meaning is doing in the background.
    meaning_work: Mutex<Option<MeaningWork>>,
    /// Asks whoever runs the node (the desktop app) to restart it.
    restart: RestartSignal,
    /// What the node did, for the panel's activity log.
    journal: journal::Journal,
    /// Cuts short search by meaning's wait after a failure.
    meaning_retry: AtomicBool,
}

/// Failed work the panel can have tried again now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retry {
    /// The setup, crawl or rebuild that failed.
    Work,
    /// The download of Wikidata's official websites.
    Wikidata,
    /// Search by meaning's model download or vectors.
    Meaning,
}

/// A settings change in words, for the activity log.
fn settings_change_words(old: &NodeSettings, new: &NodeSettings) -> String {
    let now = now_unix();
    if old.paused_until != new.paused_until
        && *old
            == (NodeSettings {
                paused_until: old.paused_until,
                ..new.clone()
            })
    {
        return match new.paused_until {
            Some(until) if until > now => format!(
                "Paused by you for {}",
                crate::web::duration_words(until - now)
            ),
            _ => "Resumed by you".to_owned(),
        };
    }
    let mut parts = Vec::new();
    if old.background_updates != new.background_updates {
        parts.push(if new.background_updates {
            "background updates on".to_owned()
        } else {
            "background updates off".to_owned()
        });
    }
    if old.workload != new.workload {
        parts.push(format!("workload {}", new.workload.name()));
    }
    if old.download_limit_mb_per_day != new.download_limit_mb_per_day {
        parts.push(match new.download_limit_mb_per_day {
            0 => "no download limit".to_owned(),
            mb => format!("download limit {mb} MB a day"),
        });
    }
    if old.storage_limit_mb != new.storage_limit_mb {
        parts.push(match new.storage_limit_mb {
            0 => "no storage limit".to_owned(),
            mb => format!("storage limit {mb} MB"),
        });
    }
    if old.crawl_hours != new.crawl_hours {
        parts.push(match new.crawl_hours {
            Some(hours) => format!("crawl hours {}", hours.words()),
            None => "crawl at any hour".to_owned(),
        });
    }
    if parts.is_empty() {
        "Settings saved".to_owned()
    } else {
        format!("Settings changed: {}", parts.join(", "))
    }
}

/// Why background work waits, and until when.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pause {
    reason: String,
    /// When it ends by itself; `None` when someone has to act.
    until: Option<u64>,
}

impl Pause {
    fn new(reason: impl Into<String>, until: Option<u64>) -> Pause {
        Pause {
            reason: reason.into(),
            until,
        }
    }
}

/// What search by meaning is doing, for [`Inner::meaning_work`].
#[derive(Debug, Clone)]
enum MeaningWork {
    Downloading,
    Loading,
    Embedding { done: u64, total: u64 },
    Failed(LastError),
}

/// A request to restart the node, from the panel to the program running
/// it. Only a program that listens ([`NodeHandle::restart_signal`]) makes
/// the panel offer it.
#[derive(Debug, Clone, Default)]
pub struct RestartSignal(Arc<RestartState>);

#[derive(Debug, Default)]
struct RestartState {
    listening: AtomicBool,
    requested: Notify,
}

impl RestartSignal {
    /// Waits for a restart request.
    pub async fn requested(&self) {
        self.0.listening.store(true, Ordering::SeqCst);
        self.0.requested.notified().await;
    }

    fn can_restart(&self) -> bool {
        self.0.listening.load(Ordering::SeqCst)
    }

    fn request(&self) -> bool {
        if self.can_restart() {
            self.0.requested.notify_one();
        }
        self.can_restart()
    }
}

/// The failures to download Wikidata's official websites, which have their
/// own wait between tries: other work goes on meanwhile.
#[derive(Debug)]
struct WikidataTries {
    last_error: Option<LastError>,
    backoff: worker::Backoff,
}

/// What the background work is doing, for [`Status`].
#[derive(Debug, Clone)]
struct Activity {
    step: Step,
    detail: String,
    progress: Option<Progress>,
    last_error: Option<LastError>,
}

impl Inner {
    fn new(
        config: NodeConfig,
        rank: RankConfig,
        opened: Opened,
        stopped: watch::Receiver<bool>,
    ) -> Self {
        let backoff = worker::Backoff::new(config.retry_wait, config.max_retry_wait);
        let journal = journal::Journal::open(&opened.paths.data);
        Inner {
            config,
            paths: opened.paths,
            rank,
            activity: Mutex::new(Activity {
                step: Step::Starting,
                detail: "Starting".to_string(),
                progress: None,
                last_error: None,
            }),
            saved: Mutex::new(opened.saved),
            index: RwLock::new(opened.index.map(Arc::new)),
            retired: Mutex::new(opened.leftover),
            stopped,
            wake: Notify::new(),
            refresh_requested: AtomicBool::new(false),
            lock: Mutex::new(opened.lock),
            wikidata: Mutex::new(WikidataTries {
                last_error: None,
                backoff,
            }),
            net: std::sync::OnceLock::new(),
            inbox_records: std::sync::atomic::AtomicU64::new(0),
            inbox_lock: Mutex::new(()),
            buckets_rebuilt: AtomicBool::new(false),
            picks: Mutex::new(None),
            settings: Mutex::new(opened.settings),
            disk: Mutex::new(None),
            meaning: SharedMeaning::default(),
            meaning_work: Mutex::new(None),
            restart: RestartSignal::default(),
            journal,
            meaning_retry: AtomicBool::new(false),
        }
    }

    fn status(&self) -> Status {
        let activity = self.activity().clone();
        let saved = self.saved();
        let index = self.current_summary();
        let pause = self.pause();
        Status {
            phase: if index.is_some() {
                Phase::Ready
            } else {
                Phase::SettingUp
            },
            step: activity.step,
            detail: activity.detail,
            progress: activity.progress,
            last_error: activity.last_error,
            wikidata_missing: saved.wikidata_missing,
            wikidata_error: if saved.wikidata_missing {
                self.wikidata_tries().last_error.clone()
            } else {
                None
            },
            sites: index.map_or(0, |(_, docs)| docs),
            index: index.map(|(id, _)| store::index_name(id)),
            last_refresh: saved.last_refresh,
            next_refresh: self.next_refresh(&saved),
            version: env!("CARGO_PKG_VERSION").to_string(),
            network: network::handle(self).map(|net| net.status()),
            crawl_left: saved.crawl_left as u64,
            meaning_sites: self.meaning.get().map(|meaning| meaning.len() as u64),
            meaning_work: self.meaning_work(),
            can_restart: self.restart.can_restart(),
            background_updates: self.settings().background_updates,
            paused: pause.as_ref().map(|p| p.reason.clone()),
            paused_until: pause.and_then(|p| p.until),
            disk_used: self.disk_used(),
            downloaded_today: saved.downloaded_today(now_unix()),
            downloaded_total: saved.downloaded_total,
            homepages_visited: saved.homepages_visited,
        }
    }

    /// Why crawls and refreshes must wait now, if they must.
    fn pause_reason(&self) -> Option<String> {
        self.pause().map(|pause| pause.reason)
    }

    /// Why crawls and refreshes must wait now, and until when, if they must.
    fn pause(&self) -> Option<Pause> {
        let settings = self.settings();
        let now = now_unix();
        if !settings.background_updates {
            return Some(Pause::new("Background updates are off", None));
        }
        if let Some(until) = settings.paused_until.filter(|&until| until > now) {
            return Some(Pause::new("Paused by you", Some(until)));
        }
        if let Some(hours) = settings.crawl_hours {
            let (hour, minute, second) = schedule::local_time();
            let wait = hours.wait(hour, minute, second);
            if wait > 0 {
                return Some(Pause::new(
                    format!("Waiting for the crawl hours, {}", hours.words()),
                    Some(now + wait),
                ));
            }
        }
        let limit = settings.download_limit_mb_per_day;
        if limit > 0 && self.saved().downloaded_today(now) >= limit.saturating_mul(MB) {
            return Some(Pause::new(
                "Paused until tomorrow: today's download limit is reached",
                Some(store::next_day(now)),
            ));
        }
        let limit = settings.storage_limit_mb;
        if limit > 0 && self.disk_used() >= limit.saturating_mul(MB) {
            return Some(Pause::new("Paused: the storage limit is reached", None));
        }
        None
    }

    /// What search by meaning is doing, for [`Status::meaning_work`].
    fn meaning_work(&self) -> Option<BackgroundWork> {
        let work = self
            .meaning_work
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        Some(match work {
            MeaningWork::Downloading => {
                // The model's files, the one being written included.
                let done = store::dir_size(&self.paths.data.join(embedding::MODEL_DIR)) / MB;
                BackgroundWork {
                    detail: "Downloading the search-by-meaning model".into(),
                    progress: Some(Progress {
                        done: done.min(embedding::MODEL_MB),
                        total: embedding::MODEL_MB,
                        unit: "MB".into(),
                    }),
                    error: None,
                }
            }
            MeaningWork::Loading => BackgroundWork {
                detail: "Loading the search-by-meaning model".into(),
                progress: None,
                error: None,
            },
            MeaningWork::Embedding { done, total } => BackgroundWork {
                detail: "Making site vectors for search by meaning".into(),
                progress: Some(Progress {
                    done,
                    total,
                    unit: "sites".into(),
                }),
                error: None,
            },
            MeaningWork::Failed(error) => BackgroundWork {
                detail: "Search by meaning is waiting to try again".into(),
                progress: None,
                error: Some(error),
            },
        })
    }

    fn set_meaning_work(&self, work: Option<MeaningWork>) {
        *self
            .meaning_work
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = work;
    }

    /// Counts bytes downloaded now.
    fn add_downloaded(&self, bytes: u64) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        let now = now_unix();
        self.update_saved(|saved| saved.add_downloaded(bytes, now))
    }

    /// The size of the data folder, counted again when the last count is
    /// more than [`DISK_COUNT_MAX_AGE`] old or [`Inner::recount_disk`]
    /// asked for it.
    fn disk_used(&self) -> u64 {
        let mut count = self.disk.lock().unwrap_or_else(PoisonError::into_inner);
        match *count {
            Some((at, bytes)) if at.elapsed() < DISK_COUNT_MAX_AGE => bytes,
            _ => {
                let bytes = store::dir_size(&self.paths.data);
                *count = Some((std::time::Instant::now(), bytes));
                bytes
            }
        }
    }

    /// Has the next [`Inner::disk_used`] count the data folder again.
    fn recount_disk(&self) {
        *self.disk.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn settings(&self) -> NodeSettings {
        self.settings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Saves new settings and puts them in force; the background work
    /// looks at them again at once.
    fn change_settings(&self, new: NodeSettings) -> Result<()> {
        {
            let mut settings = self.settings.lock().unwrap_or_else(PoisonError::into_inner);
            if *settings != new {
                store::save_settings(&self.paths, &new)?;
                info!("settings changed: {new:?}");
                self.journal.info(settings_change_words(&settings, &new));
                *settings = new;
            }
        }
        self.wake.notify_one();
        Ok(())
    }

    /// When the next refresh is due, in Unix seconds.
    fn next_refresh(&self, saved: &SavedState) -> Option<u64> {
        let every = self.config.refresh_every?;
        Some(saved.last_refresh?.saturating_add(every.as_secs()))
    }

    fn activity(&self) -> std::sync::MutexGuard<'_, Activity> {
        self.activity.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Shows a new step, without progress until [`Inner::set_progress`].
    fn set_step(&self, step: Step, detail: impl Into<String>) {
        let mut activity = self.activity();
        activity.step = step;
        activity.detail = detail.into();
        activity.progress = None;
    }

    fn set_progress(&self, done: usize, total: usize, unit: &str) {
        self.activity().progress = Some(Progress {
            done: done as u64,
            total: total as u64,
            unit: unit.to_string(),
        });
    }

    fn set_error(&self, err: &anyhow::Error, retry_at: u64) {
        let message = format!("{err:#}");
        let repeated = self
            .activity()
            .last_error
            .as_ref()
            .is_some_and(|last| last.message == message);
        if !repeated {
            self.journal.error(message);
        }
        let mut activity = self.activity();
        activity.step = Step::Retrying;
        activity.detail = "Waiting to try again after an error".to_string();
        activity.progress = None;
        activity.last_error = Some(LastError {
            message: format!("{err:#}"),
            at: now_unix(),
            retry_at: Some(retry_at),
        });
    }

    fn clear_error(&self) {
        if self.activity().last_error.take().is_some() {
            self.journal.info("Working again after the error");
        }
    }

    fn wikidata_tries(&self) -> std::sync::MutexGuard<'_, WikidataTries> {
        self.wikidata.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes a failure to download Wikidata's official websites and returns
    /// when to try again, in Unix seconds.
    fn wikidata_failed(&self, err: &anyhow::Error) -> u64 {
        self.journal.warning(format!(
            "Could not download Wikidata's official websites: {err:#}"
        ));
        let mut tries = self.wikidata_tries();
        let now = now_unix();
        let retry_at = now.saturating_add(tries.backoff.next_delay().as_secs());
        tries.last_error = Some(LastError {
            message: format!("{err:#}"),
            at: now,
            retry_at: Some(retry_at),
        });
        retry_at
    }

    /// When to try Wikidata again, in Unix seconds: 0 (now) when it has not
    /// failed since the node started.
    fn wikidata_retry_at(&self) -> u64 {
        self.wikidata_tries()
            .last_error
            .as_ref()
            .and_then(|err| err.retry_at)
            .unwrap_or(0)
    }

    /// Forgets the failures once Wikidata has answered.
    fn wikidata_arrived(&self) {
        self.journal.info("Wikidata's official websites are in");
        let mut tries = self.wikidata_tries();
        tries.last_error = None;
        tries.backoff.reset();
    }

    fn saved(&self) -> SavedState {
        self.saved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Changes the saved state and writes it to disk.
    fn update_saved(&self, change: impl FnOnce(&mut SavedState)) -> Result<()> {
        let mut saved = self.saved.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = saved.clone();
        change(&mut next);
        if next != *saved {
            store::save_state(&self.paths, &next)?;
            *saved = next;
        }
        Ok(())
    }

    fn stopping(&self) -> bool {
        *self.stopped.borrow()
    }

    /// `Err(Stopped)` once the node is shutting down.
    fn check_stop(&self) -> Result<()> {
        if self.stopping() {
            return Err(Stopped.into());
        }
        Ok(())
    }

    /// Resolves once the node is shutting down.
    async fn stopped(&self) {
        let mut stopped = self.stopped.clone();
        // An error means the handle is gone, which also means stop.
        let _ = stopped.wait_for(|&stop| stop).await;
    }

    fn request_refresh(&self) {
        self.refresh_requested.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    /// The index searched now. Holding it keeps it open, which delays the
    /// deletion of its directory once a newer index replaces it.
    fn current(&self) -> Option<Arc<ServingIndex>> {
        self.index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The id and the size of the index searched now, read without holding
    /// the index open.
    fn current_summary(&self) -> Option<(u64, u64)> {
        self.index
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|index| (index.id, index.docs))
    }

    /// Makes `index` the one searched. Searches already running finish on
    /// the old one, whose directory is deleted once the last of them ends
    /// (see [`Inner::sweep`]).
    fn install(&self, index: ServingIndex) {
        info!(
            "now searching {} sites in {}",
            index.docs,
            index.dir.display()
        );
        let old = self
            .index
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(Arc::new(index));
        if let Some(old) = old {
            self.retired_list().push(Retired {
                dir: old.dir.clone(),
                closed: Arc::clone(&old.closed),
                failures: 0,
            });
        }
    }

    fn retired_list(&self) -> std::sync::MutexGuard<'_, Vec<Retired>> {
        self.retired.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn has_retired(&self) -> bool {
        !self.retired_list().is_empty()
    }

    /// Deletes the directories of replaced indexes that no search has open
    /// any more. Ones that cannot be deleted yet are tried again next time.
    /// Does file system work, so call it off the async threads.
    fn sweep(&self) {
        self.retired_list().retain_mut(|retired| {
            if !retired.closed.load(Ordering::Acquire) {
                return true;
            }
            match store::remove_index(&retired.dir) {
                Ok(()) => {
                    info!("deleted the old index {}", retired.dir.display());
                    false
                }
                Err(err) => {
                    retired.failures += 1;
                    if retired.failures == 1 {
                        warn!(
                            "cannot delete the old index {} yet: {err}; will try again",
                            retired.dir.display()
                        );
                    } else {
                        debug!(
                            "still cannot delete {} (try {}): {err}",
                            retired.dir.display(),
                            retired.failures
                        );
                    }
                    true
                }
            }
        });
    }
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("data_dir", &self.paths.data)
            .field("index", &self.current_summary().map(|(id, _)| id))
            .finish_non_exhaustive()
    }
}

impl SearchBackend for Inner {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        Ok(self
            .search_full(query, limit, &SearchOptions::default())?
            .hits)
    }

    /// The index's results, re-ranked with what the network's popularity
    /// reports say people pick for the query (see [`network`]).
    fn search_full(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        let Some(index) = self.current() else {
            bail!("the search index is not ready yet");
        };
        let meaning = self.meaning.get();
        let Some(table) = network::handle(self).map(|net| net.popularity()) else {
            return index
                .backend()
                .search_full_with(query, limit, options, meaning.as_deref());
        };
        let candidates = limit.max(network::POPULARITY_CANDIDATES);
        let mut results =
            index
                .backend()
                .search_full_with(query, candidates, options, meaning.as_deref())?;
        network::apply_popularity(&table, query, &mut results.hits);
        results.hits.truncate(limit);
        Ok(results)
    }

    fn num_docs(&self) -> u64 {
        self.current_summary().map_or(0, |(_, docs)| docs)
    }
}

impl StatusSource for Inner {
    fn status(&self) -> Status {
        Inner::status(self)
    }

    fn network(&self) -> Option<Arc<plumb_net::NetHandle>> {
        network::handle(self).cloned()
    }

    fn rank(&self) -> RankConfig {
        self.rank
    }

    fn bucket_table(&self) -> Option<String> {
        if !self.config.private_search {
            return None;
        }
        let index = self.current()?;
        index.buckets.as_ref()?;
        index.bucket_table.clone()
    }

    fn bucket(&self, table: &str, bucket: u32) -> Option<Result<Vec<String>>> {
        if !self.config.private_search {
            return None;
        }
        let index = self.current()?;
        let buckets = index.buckets.as_ref()?;
        (index.bucket_table.as_deref() == Some(table)).then(|| buckets.get(bucket))
    }

    fn shares_popularity(&self) -> bool {
        network::shares_popularity(self)
    }

    fn record_pick(&self, query: &str, domain: &str) {
        network::record_pick(self, query, domain);
    }

    fn icon(&self, domain: &str) -> Option<Vec<u8>> {
        crate::icons::IconStore::new(&self.paths.icons).get(domain)
    }

    fn features(&self) -> features::FeatureSettings {
        features::FeatureSettings::from_config(&self.config)
    }

    fn saved_features(&self) -> Result<features::FeatureSettings> {
        Ok(features::FeatureSettings::load(&self.paths.data)?.unwrap_or_else(|| self.features()))
    }

    fn change_features(&self, features: features::FeatureSettings) -> Result<()> {
        features.save(&self.paths.data)?;
        self.journal
            .info("Feature settings saved; they apply when the node restarts");
        Ok(())
    }

    fn activity_log(&self) -> Vec<LogEntry> {
        self.journal.entries()
    }

    fn retry(&self, what: Retry) -> Result<()> {
        match what {
            Retry::Work => self.request_refresh(),
            Retry::Wikidata => {
                if let Some(err) = &mut self.wikidata_tries().last_error {
                    err.retry_at = Some(now_unix());
                }
                self.wake.notify_one();
            }
            Retry::Meaning => {
                if !self.config.search_by_meaning {
                    bail!("Search by meaning is off on this node.");
                }
                self.meaning_retry.store(true, Ordering::SeqCst);
            }
        }
        self.journal.info(match what {
            Retry::Work => "Trying the failed work again, as asked",
            Retry::Wikidata => "Trying Wikidata again, as asked",
            Retry::Meaning => "Trying search by meaning again, as asked",
        });
        Ok(())
    }

    fn make_backup(&self) -> Result<backup::BackupInfo> {
        let made = backup::save(&self.paths.data, None)?;
        self.journal.info(format!("Backup made: {}", made.name));
        Ok(made)
    }

    fn restore_backup(&self, restored: &backup::Backup) -> Result<()> {
        // Restoring can be undone with the backup made first.
        let before = backup::save(&self.paths.data, Some("before-restore"))?;
        restored.restore(&self.paths.data)?;
        if let Some(settings) = store::load_settings(&self.paths) {
            *self.settings.lock().unwrap_or_else(PoisonError::into_inner) = settings;
            self.wake.notify_one();
        }
        self.journal.warning(format!(
            "Backup from {} restored; the settings before it are in {}",
            restored.version, before.name
        ));
        // Keys and features take effect at the next start.
        if self.restart.request() {
            self.journal.info("Restarting to finish restoring");
        }
        Ok(())
    }

    fn settings(&self) -> Option<NodeSettings> {
        Some(Inner::settings(self))
    }

    fn change_settings(&self, settings: NodeSettings) -> Result<()> {
        Inner::change_settings(self, settings)
    }

    fn refresh_now(&self) {
        self.request_refresh();
    }

    fn restart(&self) -> Result<()> {
        if !self.restart.request() {
            bail!("This node cannot restart itself. Restart it where it runs.");
        }
        info!("restart asked for from the panel");
        self.journal.info("Restarting, as asked on the panel");
        Ok(())
    }

    fn data_dir(&self) -> Option<PathBuf> {
        Some(self.paths.data.clone())
    }

    fn bind(&self) -> Option<SocketAddr> {
        Some(self.config.bind)
    }

    fn manages_other_nodes(&self) -> bool {
        self.config.manage_other_nodes
    }
}

/// An index the node searches, or did until a newer one replaced it.
struct ServingIndex {
    id: u64,
    dir: PathBuf,
    docs: u64,
    /// Always `Some` until dropped; an `Option` so that [`Drop`] can close
    /// the index before saying it is closed.
    backend: Option<IndexBackend>,
    /// Set once the index files are closed and may be deleted.
    closed: Arc<AtomicBool>,
    /// The index's buckets, which other nodes search (`indexes/NNNNNN/buckets/`);
    /// only built by a node in the network or serving private search.
    buckets: Option<plumb_net::BucketTable>,
    /// Names [`ServingIndex::buckets`] for browsers: the index id and a hash
    /// of the bucket index, so a cached bucket is never taken for one of
    /// another index, even after the data directory is started over.
    bucket_table: Option<String>,
}

impl ServingIndex {
    fn open(id: u64, dir: &std::path::Path, rank: RankConfig) -> Result<Self> {
        let searcher = Searcher::open(dir)?;
        Ok(ServingIndex {
            id,
            dir: dir.to_path_buf(),
            docs: searcher.num_docs(),
            backend: Some(IndexBackend::new(searcher, rank)),
            closed: Arc::new(AtomicBool::new(false)),
            buckets: plumb_net::BucketTable::open(&dir.join(network::BUCKETS_DIR)).ok(),
            bucket_table: network::bucket_table_name(id, dir),
        })
    }

    fn backend(&self) -> &IndexBackend {
        self.backend
            .as_ref()
            .expect("an index is open until dropped")
    }
}

impl Drop for ServingIndex {
    fn drop(&mut self) {
        // Close the files first: Windows cannot delete open ones.
        drop(self.backend.take());
        self.closed.store(true, Ordering::Release);
    }
}

/// A replaced index waiting for its last search to end, or for a deletion
/// that failed to be tried again.
#[derive(Debug)]
struct Retired {
    dir: PathBuf,
    closed: Arc<AtomicBool>,
    failures: u32,
}

impl Retired {
    /// An index directory that nothing has open.
    fn closed(dir: PathBuf) -> Self {
        Retired {
            dir,
            closed: Arc::new(AtomicBool::new(true)),
            failures: 1,
        }
    }
}
