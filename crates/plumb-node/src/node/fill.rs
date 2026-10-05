//! Filling the node's free space with the network's crawls (see
//! `plumb_net::fill`).
//!
//! Every [`FILL_EVERY`], a node in the network with room asks a node it
//! trusts for its crawled sites, best-ranked first, a page at a time, and
//! appends them to the inbox like any record from the network: they are
//! folded into the records file and indexed at the next build. It stops at
//! [`FILL_UP_TO_PERCENT`] of the storage limit, so crawling still has room,
//! and at the day's download limit, and before the index grows past what
//! an index build can hold in half the machine's memory
//! ([`BUILD_MEMORY_PERCENT`]). With no storage limit it takes the
//! trusted node's whole list, a full node's copy of the shared index.
//!
//! Once through the list it rests [`FILL_AGAIN_AFTER`] before starting
//! over: crawls shared since arrive anyway as they are made. How far it
//! got is kept in `DIR/net/fill.json`.
//!
//! # Topics
//!
//! A node with topics to keep (its focus topics, and the interests on the
//! About pages of the browsers that search it, see
//! [`super::Inner::keep_topics`]) saves [`FOCUS_SHARE_PERCENT`] of its
//! storage limit for them: it takes the list's best sites up to the rest,
//! then reads on down the list keeping only the sites about a topic. It
//! asks for the same pages either way, so the node it asks learns nothing
//! of the topics. When the topics change, it first drops the sites below
//! the best ones that are about none of the new topics
//! ([`prune_for_topics`], with an index build), so the share is free for
//! the new topics, then reads on from where the best sites stopped again.
//!
//! # Blackhole
//!
//! A node started with [`super::NodeConfig::blackhole`] takes every trusted
//! node's list, not just one: once through one node's list it goes on to
//! the next connected trusted node it has not gone through in the last
//! [`BLACKHOLE_AGAIN_AFTER`], and comes back to each after that
//! ([`blackhole_peer`]). Each node's list is in its own order, so each is
//! read from the top. When it holds every connected trusted node's list,
//! it waits for one to come due again or a new one to connect.
//!
//! # Setting up from the network
//!
//! A new node in the network that trusts a node sets up from it rather than
//! from the seed downloads (Tranco, Common Crawl, Wikidata, Wikipedia):
//! [`seed_from_network`] asks the trusted node for every site of its list,
//! crawled or not, best-ranked first, and the first [`QUICK_SEED_SITES`] of
//! them make the first records file and index. Filling then goes on in
//! [`FillState::seed`] mode, taking every site rather than only crawled
//! ones and a round every [`SEED_FILL_EVERY`], until the node holds
//! [`super::NodeConfig::sites`] sites or the list ends; then it fills as
//! usual from where it got to. When no trusted node answers within
//! [`SEED_PEER_WAIT`], or it sends too few sites (a network just started),
//! the node downloads the seed data as before.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use plumb_core::{now_unix, RecordSet, SiteRecord};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::network::{self, REBUILD_AFTER_RECORDS};
use super::{Inner, Step, Stopped, MB};
use crate::about::Topics;
use crate::records::sorted_by_link_score;

/// Time between two fill rounds.
#[cfg(not(test))]
pub(super) const FILL_EVERY: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
pub(super) const FILL_EVERY: Duration = Duration::from_secs(2);

/// Time between two fill rounds while setting up from the network.
#[cfg(not(test))]
const SEED_FILL_EVERY: Duration = Duration::from_secs(60);
#[cfg(test)]
const SEED_FILL_EVERY: Duration = Duration::from_secs(1);

/// Sites a node setting up from the network takes in before its first
/// index: about ten pages, a minute and a half.
#[cfg(not(test))]
pub(super) const QUICK_SEED_SITES: usize = 50_000;
#[cfg(test)]
pub(super) const QUICK_SEED_SITES: usize = 3;

/// How long a node setting up waits for a trusted node to connect before
/// it downloads the seed data instead.
#[cfg(not(test))]
const SEED_PEER_WAIT: Duration = Duration::from_secs(2 * 60);
#[cfg(test)]
const SEED_PEER_WAIT: Duration = Duration::from_secs(20);

/// Fewest sites a trusted node must send for a node to set up from it;
/// fewer, and the node is better off with the seed downloads.
#[cfg(not(test))]
const MIN_SEED_SITES: usize = 1_000;
#[cfg(test)]
const MIN_SEED_SITES: usize = 3;

/// Wait after joining before the first round, so the node meets the
/// nodes it trusts first.
#[cfg(not(test))]
const FIRST_FILL_AFTER: Duration = Duration::from_secs(2 * 60);
#[cfg(test)]
const FIRST_FILL_AFTER: Duration = Duration::from_secs(1);

/// Sites one round takes in at most: about what the index takes in between
/// two builds.
pub(super) const FILL_PER_ROUND: u64 = 50_000;

/// Sites asked for at once.
const FILL_PAGE: u32 = plumb_net::fill::MAX_FILL_RECORDS;

/// Sites asked for at once while setting up from the network.
const SEED_PAGE: u32 = plumb_net::fill::MAX_SEED_RECORDS;

/// How often a node setting up looks for a trusted node to connect.
const SEED_PEER_POLL: Duration = Duration::from_millis(500);

/// Pause between two pages, so a node answers this one at most
/// `plumb_net::fill::FILL_REQUESTS_PER_MINUTE` times a minute.
#[cfg(not(test))]
const FILL_PAGE_GAP: Duration = Duration::from_secs(10);
#[cfg(test)]
const FILL_PAGE_GAP: Duration = Duration::from_millis(100);

/// Wait after a busy answer, and how many busy answers end a round.
#[cfg(not(test))]
const FILL_BUSY_WAIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const FILL_BUSY_WAIT: Duration = Duration::from_millis(500);
const FILL_BUSY_TRIES: u32 = 4;

/// Share of the storage limit filling stops at.
pub(super) const FILL_UP_TO_PERCENT: u64 = 90;

/// Share of the storage limit kept for sites about the node's topics, when
/// it has some: the best sites fill up to [`FILL_UP_TO_PERCENT`] less this.
pub(super) const FOCUS_SHARE_PERCENT: u64 = 25;

/// Disk a site takes for each byte of its record as sent: the records
/// file, the index, the buckets and the vector for search by meaning.
pub(super) const DISK_PER_RECORD_BYTE: u64 = 4;

/// Rest after going through a trusted node's whole list.
pub(super) const FILL_AGAIN_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);

/// Rest before a blackhole node goes through a trusted node's whole list
/// again: crawls shared since arrive anyway, so this only catches what
/// was missed.
pub(super) const BLACKHOLE_AGAIN_AFTER: Duration = Duration::from_secs(24 * 3600);

/// Peak memory an index build takes per site: a build of a million
/// sites peaks at about 2.2 GB, rounded up.
pub(super) const BUILD_BYTES_PER_SITE: u64 = 2_500;

/// Share of the machine's memory (or its container's limit) an index
/// build may take; filling stops before the index outgrows it, so a 4 GB
/// server stops at about 800,000 sites.
pub(super) const BUILD_MEMORY_PERCENT: u64 = 50;

const FILL_FILE: &str = "fill.json";

/// How far filling got, kept in `DIR/net/fill.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct FillState {
    /// The trusted node filled from last.
    pub peer: Option<String>,
    /// Where in its list to ask from next.
    pub next: u64,
    /// Sites in its list, crawled or not, at the last answer.
    pub total: u64,
    /// Crawled sites taken in so far.
    pub filled: u64,
    /// When its whole list was last gone through.
    pub done_at: Option<u64>,
    /// Still setting up from the network: take every site, crawled or
    /// not, until the node holds `NodeConfig::sites` of them.
    pub seed: bool,
    /// Where in the list the best sites ran out of room and keeping only
    /// sites about the node's topics began.
    pub focus_from: Option<u64>,
    /// Sites the node held when keeping only sites about its topics
    /// began: the best sites, which changing the topics never drops.
    pub focus_base: Option<u64>,
    /// The topics changed: the sites kept for the old ones are to be
    /// dropped before filling goes on.
    pub prune: bool,
    /// The topics kept last ([`crate::about::Topics::key`]).
    pub focus_key: String,
    /// Blackhole: when each trusted node's whole list was last gone
    /// through, in Unix seconds, by node id.
    pub lists_done: BTreeMap<String, u64>,
    /// Blackhole: where in each trusted node's list reading stopped when
    /// it moved on to another node's, by node id, so it goes on from there.
    pub positions: BTreeMap<String, u64>,
    /// Bytes of disk the sites taken in since the last index build are
    /// reckoned to take once indexed, which the disk count does not show
    /// yet, and since when.
    #[serde(skip)]
    pub pending: u64,
    #[serde(skip)]
    pub pending_since: u64,
    /// Sites taken in since the last index build.
    #[serde(skip)]
    pub pending_sites: u64,
    /// What filling is doing, in words.
    #[serde(skip)]
    pub detail: String,
}

/// What filling is doing, for `GET /api/status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillStatus {
    /// Crawled sites taken in from trusted nodes so far.
    pub filled: u64,
    /// How far through the trusted node's list, best-ranked first.
    pub position: u64,
    /// Sites in that list, crawled or not.
    pub total: u64,
    /// The trusted node filled from last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// What filling is doing, in words.
    pub detail: String,
    /// The node collects all the data the network offers
    /// ([`super::NodeConfig::blackhole`]).
    #[serde(default)]
    pub blackhole: bool,
    /// Trusted nodes whose whole list it went through, in blackhole mode.
    #[serde(default)]
    pub lists_done: usize,
}

impl FillState {
    pub(super) fn load(net_dir: &Path) -> FillState {
        fs::read(net_dir.join(FILL_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, net_dir: &Path) -> Result<()> {
        let path = net_dir.join(FILL_FILE);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(self)?)
            .and_then(|()| fs::rename(&tmp, &path))
            .with_context(|| format!("saving {}", path.display()))
    }

    /// Notes the topics kept now (`key`). New topics make room (`prune`)
    /// and read on from where the best sites stopped again. With no
    /// storage limit (`storage_limit_mb` 0) nothing is dropped to make
    /// room: the best sites' count is forgotten, and a prune not yet done
    /// is called off.
    fn set_topics(&mut self, key: String, storage_limit_mb: u64) {
        if storage_limit_mb == 0 {
            self.focus_base = None;
            self.prune = false;
        }
        if self.focus_key != key {
            if let Some(from) = self.focus_from {
                self.next = from;
                self.done_at = None;
                self.prune = self.focus_base.is_some();
            }
            self.focus_key = key;
        }
    }

    /// Blackhole: reads `peer`'s list next. Another node's list goes on
    /// from where reading it stopped, or from the top, and the list being
    /// left keeps its place; the same list once gone through starts over.
    fn read_list_of(&mut self, peer: String) {
        if self.peer.as_deref() != Some(peer.as_str()) {
            if let Some(old) = self.peer.take() {
                if self.next > 0 && self.next < self.total {
                    self.positions.insert(old, self.next);
                }
            }
            self.next = self.positions.remove(&peer).unwrap_or(0);
        } else if self.lists_done.contains_key(&peer) && self.total > 0 && self.next >= self.total {
            self.next = 0;
        }
        self.peer = Some(peer);
    }

    pub(super) fn status(&self, blackhole: bool) -> FillStatus {
        FillStatus {
            filled: self.filled,
            position: self.next.min(self.total),
            total: self.total,
            peer: self.peer.clone(),
            detail: self.detail.clone(),
            blackhole,
            lists_done: self.lists_done.len(),
        }
    }
}

/// Blackhole: the trusted node to fill from next among the `connected`
/// ones: `reading`, the one whose list is part read, otherwise
/// the one gone through longest ago (first one never gone through),
/// leaving out those gone through in the last [`BLACKHOLE_AGAIN_AFTER`].
/// `None` when every connected one is done.
pub(super) fn blackhole_peer(
    reading: Option<&str>,
    connected: &[String],
    lists_done: &BTreeMap<String, u64>,
    now: u64,
) -> Option<String> {
    let due = |peer: &String| {
        lists_done
            .get(peer)
            .is_none_or(|done| now >= done.saturating_add(BLACKHOLE_AGAIN_AFTER.as_secs()))
    };
    if let Some(peer) = connected
        .iter()
        .find(|peer| Some(peer.as_str()) == reading && due(peer))
    {
        return Some(peer.clone());
    }
    connected
        .iter()
        .filter(|peer| due(peer))
        .min_by_key(|peer| lists_done.get(*peer).copied().unwrap_or(0))
        .cloned()
}

/// How many bytes of records a fill round may take in now (each byte of a
/// record taking [`DISK_PER_RECORD_BYTE`] of disk), and why none when
/// none. `limit_mb` 0 is no storage limit.
pub(super) fn room(
    limit_mb: u64,
    disk_used: u64,
    pending: u64,
) -> std::result::Result<Option<u64>, &'static str> {
    room_up_to(limit_mb, FILL_UP_TO_PERCENT, disk_used, pending)
}

/// [`room`] up to `percent` of the storage limit.
fn room_up_to(
    limit_mb: u64,
    percent: u64,
    disk_used: u64,
    pending: u64,
) -> std::result::Result<Option<u64>, &'static str> {
    if limit_mb == 0 {
        return Ok(None);
    }
    let cap = limit_mb.saturating_mul(MB) / 100 * percent;
    let used = disk_used.saturating_add(pending);
    if used >= cap {
        return Err("Full: the storage limit leaves no room for more sites");
    }
    Ok(Some((cap - used) / DISK_PER_RECORD_BYTE))
}

/// How many more sites the index may grow by before a build of it takes
/// more than [`BUILD_MEMORY_PERCENT`] of `memory` bytes, when known, and
/// why none when none.
pub(super) fn site_room(
    memory: Option<u64>,
    sites: u64,
    pending_sites: u64,
) -> std::result::Result<Option<u64>, &'static str> {
    let Some(memory) = memory else {
        return Ok(None);
    };
    let cap = memory / 100 * BUILD_MEMORY_PERCENT / BUILD_BYTES_PER_SITE;
    let have = sites.saturating_add(pending_sites);
    if have >= cap {
        return Err("Full: a bigger index would not fit in this machine's memory");
    }
    Ok(Some(cap - have))
}

/// The memory this node may use, in bytes: the machine's, or its
/// container's limit when lower. `None` where it cannot be read (only
/// Linux is read).
fn memory_limit() -> Option<u64> {
    let machine = fs::read_to_string("/proc/meminfo").ok().and_then(|info| {
        let line = info.lines().find(|l| l.starts_with("MemTotal:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    });
    let container = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .iter()
    .find_map(|path| fs::read_to_string(path).ok()?.trim().parse::<u64>().ok());
    match (machine, container) {
        (Some(m), Some(c)) => Some(m.min(c)),
        (m, c) => m.or(c),
    }
}

/// Fills the node's space round after round until it stops.
pub(super) async fn fill_space(inner: Arc<Inner>) {
    let mut wait = FIRST_FILL_AFTER;
    loop {
        tokio::select! {
            () = inner.stopped() => return,
            () = tokio::time::sleep(wait) => {}
        }
        if let Err(err) = fill_round(&inner).await {
            debug!("no sites filled this round: {err:#}");
            inner.update_fill(|state| state.detail = format!("Waiting to try again: {err:#}"));
        }
        wait = if inner.fill_state().seed {
            SEED_FILL_EVERY
        } else {
            FILL_EVERY
        };
    }
}

/// Whether a new node may set up from the network rather than the seed
/// downloads: it is told to, is in the network with filling on, and
/// trusts a node to send it the sites.
pub(super) fn can_seed_from_network(inner: &Inner) -> bool {
    inner.config.seed_from_network
        && inner
            .config
            .network
            .as_ref()
            .is_some_and(|net| net.fill && !net.trusted_peers.is_empty())
        && network::handle(inner).is_some()
}

/// First start in the network: takes the best [`QUICK_SEED_SITES`] sites
/// of a trusted node's list, crawled or not, for the first records file,
/// and leaves the rest to filling. `None` when no trusted node answered in
/// [`SEED_PEER_WAIT`] or it sent fewer than [`MIN_SEED_SITES`]: the node
/// downloads the seed data instead. With the records comes the bytes
/// received.
pub(super) async fn seed_from_network(
    inner: &Arc<Inner>,
) -> Result<Option<(Vec<SiteRecord>, u64)>> {
    let Some(net) = network::handle(inner).cloned() else {
        return Ok(None);
    };
    if !can_seed_from_network(inner) {
        return Ok(None);
    }
    info!("setting up from the sites of a trusted node in the network");
    inner.set_step(
        Step::Downloading,
        "Waiting for a trusted node in the network to send its sites",
    );
    let want = inner.config.sites.min(QUICK_SEED_SITES);
    let deadline = Instant::now() + SEED_PEER_WAIT;
    let mut set = RecordSet::new();
    let mut state = FillState::default();
    let mut prefer = None;
    let mut busy = 0;
    let mut bytes = 0;
    let mut done = false;
    while set.len() < want {
        let count = (want - set.len()).min(plumb_net::fill::MAX_SEED_RECORDS as usize) as u32;
        let page = match net.fill(prefer, state.next, count, true).await {
            Ok(Some(page)) => page,
            Ok(None) if set.is_empty() && Instant::now() < deadline => {
                if pause(inner, SEED_PEER_POLL).await {
                    return Err(Stopped.into());
                }
                continue;
            }
            Ok(None) => break,
            Err(err) => {
                warn!("a trusted node stopped sending its sites: {err:#}");
                break;
            }
        };
        if page.busy {
            busy += 1;
            if busy >= FILL_BUSY_TRIES {
                break;
            }
            if pause(inner, FILL_BUSY_WAIT).await {
                return Err(Stopped.into());
            }
            continue;
        }
        if prefer.is_some_and(|peer| peer != page.peer) {
            // Another node's list is in another order: filling takes it
            // from the top.
            break;
        }
        prefer = Some(page.peer);
        state.peer = Some(page.peer.to_string());
        state.next = page.next;
        state.total = page.total;
        bytes += page.bytes;
        done = page.done();
        for record in page.records {
            set.upsert(record);
        }
        inner.set_step(
            Step::Downloading,
            "Taking in sites from a trusted node in the network",
        );
        inner.set_progress(set.len(), want, "sites");
        if done || pause(inner, FILL_PAGE_GAP).await {
            break;
        }
    }
    inner.check_stop()?;
    if set.len() < MIN_SEED_SITES.min(want) {
        let why = if state.peer.is_none() {
            "no trusted node answered".to_string()
        } else {
            format!("a trusted node sent only {} sites", set.len())
        };
        info!("{why}: setting up from the seed downloads instead");
        inner
            .journal
            .info(format!("Set up from the seed downloads: {why}"));
        return Ok(None);
    }
    let records = set.into_sorted_vec();
    state.filled = records.len() as u64;
    if done {
        state.done_at = Some(now_unix());
    } else {
        state.seed = records.len() < inner.config.sites;
    }
    state.detail = if state.seed {
        "Setting up: taking in the rest of a trusted node's sites".into()
    } else {
        "Done: holds the trusted node's sites".into()
    };
    state.save(&inner.paths.net)?;
    inner.set_fill(state);
    info!(
        "took {} sites from a trusted node to set up, without the seed downloads",
        records.len()
    );
    inner.journal.info(format!(
        "Set up from a trusted node in the network: {} sites, the rest to follow",
        records.len()
    ));
    Ok(Some((records, bytes)))
}

/// One round: pages of a trusted node's crawled sites until the round's
/// share, the room left or the list runs out.
async fn fill_round(inner: &Arc<Inner>) -> Result<()> {
    let Some(net) = network::handle(inner).cloned() else {
        return Ok(());
    };
    if inner.current().is_none() {
        inner.update_fill(|s| s.detail = "Waiting for the first index".into());
        return Ok(());
    }
    let now = now_unix();
    let mut state = inner.fill_state();
    let seed = state.seed;
    let topics = if seed {
        Topics::default()
    } else {
        inner.keep_topics()
    };
    state.set_topics(topics.key(), inner.settings().storage_limit_mb);
    if state.prune {
        state.detail = "Making room for sites about the new topics".into();
        inner.set_fill(state.clone());
        state.save(&inner.paths.net)?;
        return Ok(());
    }
    let blackhole = inner.config.blackhole && !seed;
    if blackhole {
        let connected: Vec<String> = net
            .fill_peers()
            .await?
            .iter()
            .map(ToString::to_string)
            .collect();
        let reading = state
            .peer
            .as_deref()
            .filter(|_| state.next > 0 && state.next < state.total);
        let Some(peer) = blackhole_peer(reading, &connected, &state.lists_done, now) else {
            let detail = if connected.is_empty() {
                "Waiting for a trusted node to connect"
            } else {
                "Done: holds every connected trusted node's crawled sites"
            };
            inner.update_fill(|s| s.detail = detail.into());
            return Ok(());
        };
        state.read_list_of(peer);
        state.done_at = None;
    } else if let Some(done) = state.done_at {
        if now < done + FILL_AGAIN_AFTER.as_secs() {
            inner.update_fill(|s| s.detail = "Done: holds the trusted node's crawled sites".into());
            return Ok(());
        }
        state.done_at = None;
        state.next = state.focus_from.unwrap_or(0);
    }
    // Sites taken in are reckoned on disk once an index is built after them.
    if inner.last_build.load(Ordering::SeqCst) > state.pending_since {
        state.pending = 0;
        state.pending_sites = 0;
    }
    let settings = inner.settings();
    // Setting up from the network is setup, not filling: it goes on
    // with filling off.
    if !settings.fill_from_network && !seed {
        inner.update_fill(|s| s.detail = "Off".into());
        return Ok(());
    }
    if !settings.setup_chosen {
        inner.update_fill(|s| {
            s.detail = "Waiting for you to choose how much to keep (Set up my node)".into()
        });
        return Ok(());
    }
    if !settings.background_updates || settings.paused_until.is_some_and(|until| until > now) {
        inner.update_fill(|s| s.detail = "Paused with crawling".into());
        return Ok(());
    }
    let download = settings.download_limit_mb_per_day.saturating_mul(MB);
    if download > 0 && inner.saved().downloaded_today(now) >= download {
        inner.update_fill(|s| {
            s.detail = "Paused until tomorrow: today's download limit is reached".into()
        });
        return Ok(());
    }
    if inner.inbox_records.load(Ordering::SeqCst) >= FILL_PER_ROUND {
        inner.update_fill(|s| s.detail = "Waiting for the sites taken in to be indexed".into());
        return Ok(());
    }
    let best_percent = if topics.is_empty() {
        FILL_UP_TO_PERCENT
    } else {
        FILL_UP_TO_PERCENT - FOCUS_SHARE_PERCENT
    };
    let used = inner.disk_used();
    let best = room_up_to(settings.storage_limit_mb, best_percent, used, state.pending);
    let (mut room, focus) = match best {
        Ok(room) => {
            // Room for the best sites again (a higher limit): they go on
            // from where they stopped.
            if let Some(from) = state.focus_from.take() {
                state.next = state.next.min(from);
            }
            state.focus_base = None;
            (room, false)
        }
        Err(_) if !topics.is_empty() => {
            match room(settings.storage_limit_mb, used, state.pending) {
                Ok(room) => {
                    if state.focus_from.is_none() {
                        state.focus_from = Some(state.next);
                        let sites = inner.current_summary().map_or(0, |(_, docs)| docs);
                        state.focus_base = Some(sites + state.pending_sites);
                    }
                    (room, true)
                }
                Err(why) => {
                    inner.update_fill(|s| s.detail = why.into());
                    return Ok(());
                }
            }
        }
        Err(why) => {
            inner.update_fill(|s| s.detail = why.into());
            return Ok(());
        }
    };
    let sites = inner.current_summary().map_or(0, |(_, docs)| docs);
    // While setting up, every site counts, up to the sites a node keeps.
    let mut seed_room = seed.then(|| {
        (inner.config.sites as u64).saturating_sub(sites.saturating_add(state.pending_sites))
    });
    let mut site_room = match site_room(memory_limit(), sites, state.pending_sites) {
        Ok(room) => room,
        Err(why) => {
            inner.update_fill(|s| s.detail = why.into());
            return Ok(());
        }
    };
    if seed_room == Some(0) {
        state.seed = false;
        inner.set_fill(state.clone());
        state.save(&inner.paths.net)?;
        return Ok(());
    }
    inner.update_fill(|s| s.detail = "Asking a trusted node for its crawled sites".into());
    let mut prefer = state.peer.as_deref().and_then(|p| p.parse().ok());
    let mut taken: u64 = 0;
    let mut busy = 0;
    let page_size = if seed { SEED_PAGE } else { FILL_PAGE };
    while taken < FILL_PER_ROUND && room != Some(0) && site_room != Some(0) && seed_room != Some(0)
    {
        let count = (FILL_PER_ROUND - taken)
            .min(u64::from(page_size))
            .min(site_room.unwrap_or(u64::MAX))
            .min(seed_room.unwrap_or(u64::MAX)) as u32;
        let Some(page) = net.fill(prefer, state.next, count, seed).await? else {
            inner.update_fill(|s| s.detail = "Waiting for a trusted node to connect".into());
            break;
        };
        if page.busy {
            busy += 1;
            if busy >= FILL_BUSY_TRIES {
                inner.update_fill(|s| {
                    s.detail = "The trusted node is busy; trying again later".into()
                });
                break;
            }
            if pause(inner, FILL_BUSY_WAIT).await {
                return Ok(());
            }
            continue;
        }
        let peer = page.peer.to_string();
        if blackhole && state.peer.as_deref() != Some(&peer) {
            // The node chosen is gone for now; its list waits, where it
            // stopped, for the next round rather than another node's
            // being read in its place.
            inner.update_fill(|s| s.detail = "Waiting for a trusted node to connect".into());
            break;
        }
        prefer = Some(page.peer);
        if state.peer.as_deref() != Some(&peer) {
            // Another node's list is in another order: start it from the top.
            let restart = state.peer.is_some() && state.next > 0;
            state.peer = Some(peer);
            if restart {
                state.next = 0;
                continue;
            }
        }
        let done = page.done();
        let scanned = page.records.len() as u64;
        let mut records = page.records;
        if focus {
            records.retain(|record| topics.matches(record));
        }
        let n = records.len() as u64;
        // The disk the sites kept take: all of the page, or the share kept.
        let bytes = if focus {
            page.bytes.checked_div(scanned).unwrap_or(0) * n
        } else {
            page.bytes
        };
        if n > 0 {
            let inner2 = inner.clone();
            tokio::task::spawn_blocking(move || network::append_inbox(&inner2, &records))
                .await
                .context("keeping filled sites")??;
            let total = inner.inbox_records.fetch_add(n, Ordering::SeqCst) + n;
            if total >= REBUILD_AFTER_RECORDS {
                inner.wake.notify_one();
            }
        }
        if let Err(err) = inner.add_downloaded(page.bytes) {
            warn!("cannot count the download: {err:#}");
        }
        if state.pending == 0 {
            state.pending_since = now_unix();
        }
        state.pending += bytes.saturating_mul(DISK_PER_RECORD_BYTE);
        room = room.map(|left| left.saturating_sub(bytes));
        site_room = site_room.map(|left| left.saturating_sub(n));
        seed_room = seed_room.map(|left| left.saturating_sub(n));
        state.pending_sites += n;
        // A page read counts against the round whatever was kept of it.
        taken += scanned;
        state.filled += n;
        state.next = page.next;
        state.total = page.total;
        if done {
            state.done_at = Some(now_unix());
            if let Some(peer) = state.peer.clone().filter(|_| blackhole) {
                state.lists_done.insert(peer, now_unix());
            }
        }
        if done || seed_room == Some(0) {
            // Set up: from here on, crawled sites only.
            state.seed = false;
        }
        let detail = if done && blackhole {
            "Done with this trusted node's crawled sites; the next one follows".to_string()
        } else if done && seed {
            "Done: holds the trusted node's sites".to_string()
        } else if done {
            "Done: holds the trusted node's crawled sites".to_string()
        } else if state.seed {
            "Setting up: taking in the rest of a trusted node's sites".to_string()
        } else if focus {
            "Keeping the sites about this node's topics from a trusted node's crawls".to_string()
        } else {
            "Taking in crawled sites from a trusted node".to_string()
        };
        state.detail = detail;
        inner.set_fill(state.clone());
        if let Err(err) = state.save(&inner.paths.net) {
            warn!("{err:#}");
        }
        if done || pause(inner, FILL_PAGE_GAP).await {
            break;
        }
    }
    if room == Some(0) {
        inner.update_fill(|s| {
            s.detail = "Full: the storage limit leaves no room for more sites".into()
        });
    } else if site_room == Some(0) {
        inner.update_fill(|s| {
            s.detail = "Full: a bigger index would not fit in this machine's memory".into()
        });
    }
    if taken > 0 && seed {
        info!("took in {taken} sites from a trusted node to finish setting up");
        inner.journal.info(format!(
            "Took in {taken} sites from a trusted node to finish setting up"
        ));
    } else if taken > 0 {
        info!("took in {taken} crawled sites from a trusted node to fill free space");
        inner.journal.info(format!(
            "Took in {taken} crawled sites from a trusted node to fill free space"
        ));
    }
    Ok(())
}

/// Drops the sites of `set` below its best `keep_best` (by link score) that
/// are about none of `topics`: those kept for topics the node no longer
/// has. Returns how many.
pub(super) fn prune_for_topics(set: &mut RecordSet, keep_best: usize, topics: &Topics) -> usize {
    if set.len() <= keep_best {
        return 0;
    }
    let drop: std::collections::HashSet<String> = sorted_by_link_score(set)
        .into_iter()
        .skip(keep_best)
        .filter(|record| !topics.matches(record))
        .map(|record| record.domain.clone())
        .collect();
    set.retain(|record| !drop.contains(&record.domain));
    drop.len()
}

impl Inner {
    /// Drops the sites kept for old topics from `set`, when the topics
    /// changed ([`FillState::prune`]); the caller saves it and builds the
    /// index from what is left. Returns how many were dropped.
    pub(super) fn prune_for_new_topics(&self, set: &mut RecordSet) -> Result<usize> {
        let mut state = self.fill_state();
        // A node with no storage limit never loses sites to make room.
        let dropped = match state.focus_base {
            Some(base) if self.settings().storage_limit_mb > 0 => {
                prune_for_topics(set, base as usize, &self.keep_topics())
            }
            _ => 0,
        };
        state.prune = false;
        state.detail = format!("Made room for the new topics: dropped {dropped} sites");
        state.save(&self.paths.net)?;
        self.set_fill(state);
        if dropped > 0 {
            info!("dropped {dropped} sites kept for topics this node no longer has");
            self.journal.info(format!(
                "Dropped {dropped} sites kept for old topics, to make room for the new ones"
            ));
        }
        Ok(dropped)
    }
}

impl Inner {
    /// Has filling rest [`FILL_AGAIN_AFTER`], as after going through the
    /// whole list: sites were just dropped to make room (see
    /// [`super::trim`]), and filling on down the list would take lower
    /// ranked ones in their place.
    pub(super) fn rest_fill(&self) {
        let mut state = self.fill_state();
        if state.seed || self.config.blackhole {
            return;
        }
        state.done_at = Some(now_unix());
        state.detail = "Resting: sites were dropped to stay under the storage limit".into();
        if let Err(err) = state.save(&self.paths.net) {
            warn!("{err:#}");
        }
        self.set_fill(state);
    }
}

/// Waits `wait`; true when the node stopped meanwhile.
async fn pause(inner: &Inner, wait: Duration) -> bool {
    tokio::select! {
        () = inner.stopped() => true,
        () = tokio::time::sleep(wait) => false,
    }
}

impl Inner {
    pub(super) fn fill_state(&self) -> FillState {
        self.fill
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_fill(&self, state: FillState) {
        *self.fill.lock().unwrap_or_else(PoisonError::into_inner) = state;
    }

    fn update_fill(&self, change: impl FnOnce(&mut FillState)) {
        change(&mut self.fill.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_up_to_ninety_percent_of_the_storage_limit() {
        // No limit: no cap.
        assert_eq!(room(0, 5 * MB, 0), Ok(None));
        // 1,000 MB limit, 500 MB used: 400 MB of disk left, a quarter of
        // that in records.
        assert_eq!(room(1_000, 500 * MB, 0), Ok(Some(100 * MB)));
        // Sites taken in but not indexed yet count.
        assert_eq!(room(1_000, 500 * MB, 200 * MB), Ok(Some(50 * MB)));
        assert!(room(1_000, 900 * MB, 0).is_err());
        assert!(room(1_000, 800 * MB, 100 * MB).is_err());
    }

    #[test]
    fn new_topics_drop_sites_only_under_a_storage_limit() {
        let focused = || FillState {
            next: 9_000,
            focus_from: Some(4_000),
            focus_base: Some(3_000),
            focus_key: "chess".into(),
            ..FillState::default()
        };
        // Under a limit, new topics make room and read on from the best.
        let mut state = focused();
        state.set_topics("go".into(), 1_000);
        assert!(state.prune);
        assert_eq!((state.next, state.focus_key.as_str()), (4_000, "go"));
        // With none (the limit was taken off), nothing is dropped.
        let mut state = focused();
        state.set_topics("go".into(), 0);
        assert!(!state.prune);
        assert_eq!(state.focus_base, None);
        // A prune not yet done when the limit is taken off is called off.
        let mut state = focused();
        state.prune = true;
        state.set_topics("chess".into(), 0);
        assert!(!state.prune);
    }

    #[test]
    fn topics_keep_a_share_of_the_storage_limit() {
        // 1,000 MB, 600 MB used: no room left for the best sites at 65%,
        // 300 MB of disk left for topic sites up to 90%.
        let best = FILL_UP_TO_PERCENT - FOCUS_SHARE_PERCENT;
        assert!(room_up_to(1_000, best, 650 * MB, 0).is_err());
        assert_eq!(
            room_up_to(1_000, best, 600 * MB, 0),
            Ok(Some(50 * MB / DISK_PER_RECORD_BYTE))
        );
        assert_eq!(
            room(1_000, 650 * MB, 0),
            Ok(Some(250 * MB / DISK_PER_RECORD_BYTE))
        );
    }

    #[test]
    fn new_topics_drop_the_sites_kept_for_old_ones_only() {
        let site = |domain: &str, tranco: u32, title: &str| {
            let mut r = SiteRecord::new(domain);
            r.signals.tranco_rank = Some(tranco);
            r.title = Some(title.into());
            r
        };
        let mut set = RecordSet::new();
        set.upsert(site("google.com", 1, "Google"));
        set.upsert(site("youtube.com", 2, "YouTube"));
        set.upsert(site("chess.com", 5_000, "Play Chess Games"));
        set.upsert(site("allrecipes.com", 6_000, "Recipes for cooking"));
        set.upsert(site("seriouseats.com", 7_000, "Serious Eats cooking"));
        let topics = Topics::new(&["cooking".to_owned()]);
        assert_eq!(prune_for_topics(&mut set, 2, &topics), 1);
        let mut left: Vec<&str> = set.iter().map(|r| r.domain.as_str()).collect();
        left.sort();
        assert_eq!(
            left,
            [
                "allrecipes.com",
                "google.com",
                "seriouseats.com",
                "youtube.com"
            ]
        );
        // The best sites stay whatever the topics.
        assert_eq!(prune_for_topics(&mut set, 2, &Topics::default()), 2);
        assert_eq!(set.len(), 2);
        assert_eq!(prune_for_topics(&mut set, 5, &Topics::default()), 0);
    }

    #[test]
    fn stops_before_an_index_build_outgrows_the_memory() {
        const GB: u64 = 1_000_000_000;
        assert_eq!(site_room(None, 5_000_000, 0), Ok(None));
        // A 4 GB server: about 800,000 sites.
        assert_eq!(site_room(Some(4 * GB), 600_000, 0), Ok(Some(200_000)));
        assert_eq!(site_room(Some(4 * GB), 600_000, 150_000), Ok(Some(50_000)));
        assert!(site_room(Some(4 * GB), 800_000, 0).is_err());
        assert!(site_room(Some(4 * GB), 1_190_000, 0).is_err());
        // A 16 GB desktop: 3.2 million.
        assert_eq!(site_room(Some(16 * GB), 250_000, 0), Ok(Some(2_950_000)));
        assert!(memory_limit().is_none_or(|m| m > 0));
    }

    #[test]
    fn remembers_how_far_it_got() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(FillState::load(dir.path()), FillState::default());
        let state = FillState {
            peer: Some("12D3KooW".into()),
            next: 5_000,
            total: 1_000_000,
            filled: 3_000,
            done_at: None,
            seed: true,
            focus_from: Some(4_000),
            focus_base: Some(3_000),
            prune: true,
            focus_key: "game".into(),
            lists_done: BTreeMap::from([("12D3KooW".to_string(), 42)]),
            positions: BTreeMap::from([("12D3KooX".to_string(), 7_000)]),
            pending: 77,
            pending_since: 1,
            pending_sites: 9,
            detail: "busy".into(),
        };
        state.save(dir.path()).unwrap();
        let loaded = FillState::load(dir.path());
        assert_eq!(
            (
                loaded.peer.as_deref(),
                loaded.next,
                loaded.filled,
                loaded.pending
            ),
            (Some("12D3KooW"), 5_000, 3_000, 0)
        );
        assert!(loaded.seed);
        assert_eq!(
            (
                loaded.focus_from,
                loaded.focus_base,
                loaded.focus_key.as_str()
            ),
            (Some(4_000), Some(3_000), "game")
        );
        assert!(loaded.prune);
        assert_eq!(loaded.lists_done, state.lists_done);
        assert_eq!(loaded.positions, state.positions);
        assert_eq!(state.status(false).position, 5_000);
        assert_eq!(state.status(true).lists_done, 1);
    }

    #[test]
    fn blackhole_keeps_each_lists_place_when_it_moves_between_nodes() {
        let mut state = FillState {
            peer: Some("ny".into()),
            next: 113_637,
            total: 1_575_323,
            ..FillState::default()
        };
        // New York went away part-way: the laptop's list from the top.
        state.read_list_of("mac".into());
        assert_eq!((state.peer.as_deref(), state.next), (Some("mac"), 0));
        state.next = 2_000;
        state.total = 575_823;
        // Back to New York: from where it stopped.
        state.read_list_of("ny".into());
        assert_eq!((state.peer.as_deref(), state.next), (Some("ny"), 113_637));
        assert_eq!(state.positions.get("mac"), Some(&2_000));
        // The same list again, once gone through: from the top.
        state.next = 1_575_323;
        state.total = 1_575_323;
        state.lists_done.insert("ny".into(), 1);
        state.read_list_of("ny".into());
        assert_eq!(state.next, 0);
        // A list gone through is not kept as a place to go on from.
        state.next = 1_575_323;
        state.read_list_of("mac".into());
        assert_eq!(state.next, 2_000);
        assert!(!state.positions.contains_key("ny"));
    }

    #[test]
    fn blackhole_goes_through_every_trusted_node_in_turn() {
        let day = BLACKHOLE_AGAIN_AFTER.as_secs();
        let now = 10 * day;
        let peers: Vec<String> = ["a", "b", "c"].iter().map(|p| p.to_string()).collect();
        let mut done = BTreeMap::new();
        // Nothing done yet: the one part read, else the first.
        assert_eq!(
            blackhole_peer(Some("b"), &peers, &done, now).as_deref(),
            Some("b")
        );
        assert_eq!(
            blackhole_peer(None, &peers, &done, now).as_deref(),
            Some("a")
        );
        assert_eq!(
            blackhole_peer(Some("gone"), &peers, &done, now).as_deref(),
            Some("a")
        );
        // "a" just done: on to one never gone through.
        done.insert("a".to_string(), now);
        assert_eq!(
            blackhole_peer(None, &peers, &done, now).as_deref(),
            Some("b")
        );
        done.insert("b".to_string(), now - 1);
        done.insert("c".to_string(), now - 2);
        // All done within the day: wait.
        assert_eq!(blackhole_peer(None, &peers, &done, now), None);
        // A day on, the one gone through longest ago comes first.
        assert_eq!(
            blackhole_peer(None, &peers, &done, now + day).as_deref(),
            Some("c")
        );
        // A new trusted node connecting is taken at once.
        let mut more = peers.clone();
        more.push("d".to_string());
        assert_eq!(
            blackhole_peer(None, &more, &done, now).as_deref(),
            Some("d")
        );
        // None connected: none.
        assert_eq!(blackhole_peer(None, &[], &done, now), None);
    }
}
