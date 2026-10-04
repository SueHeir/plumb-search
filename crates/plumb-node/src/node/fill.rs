//! Filling the node's free space with the network's crawls (see
//! `plumb_net::fill`).
//!
//! Every [`FILL_EVERY`], a node in the network with room asks a node it
//! trusts for its crawled sites, best-ranked first, a page at a time, and
//! appends them to the inbox like any record from the network: they are
//! folded into the records file and indexed at the next build. It stops at
//! [`FILL_UP_TO_PERCENT`] of the storage limit, so crawling still has room,
//! and at the day's download limit. With no storage limit it takes the
//! trusted node's whole list, a full node's copy of the shared index.
//!
//! Once through the list it rests [`FILL_AGAIN_AFTER`] before starting
//! over: crawls shared since arrive anyway as they are made. How far it
//! got is kept in `DIR/net/fill.json`.

use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use plumb_core::now_unix;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::network::{self, REBUILD_AFTER_RECORDS};
use super::{Inner, MB};

/// Time between two fill rounds.
#[cfg(not(test))]
pub(super) const FILL_EVERY: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
pub(super) const FILL_EVERY: Duration = Duration::from_secs(2);

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

/// Disk a site takes for each byte of its record as sent: the records
/// file, the index, the buckets and the vector for search by meaning.
pub(super) const DISK_PER_RECORD_BYTE: u64 = 4;

/// Rest after going through a trusted node's whole list.
pub(super) const FILL_AGAIN_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);

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
    /// Bytes of disk the sites taken in since the last index build are
    /// reckoned to take once indexed, which the disk count does not show
    /// yet, and since when.
    #[serde(skip)]
    pub pending: u64,
    #[serde(skip)]
    pub pending_since: u64,
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

    pub(super) fn status(&self) -> FillStatus {
        FillStatus {
            filled: self.filled,
            position: self.next.min(self.total),
            total: self.total,
            peer: self.peer.clone(),
            detail: self.detail.clone(),
        }
    }
}

/// How many bytes of records a fill round may take in now (each byte of a
/// record taking [`DISK_PER_RECORD_BYTE`] of disk), and why none when
/// none. `limit_mb` 0 is no storage limit.
pub(super) fn room(
    limit_mb: u64,
    disk_used: u64,
    pending: u64,
) -> std::result::Result<Option<u64>, &'static str> {
    if limit_mb == 0 {
        return Ok(None);
    }
    let cap = limit_mb.saturating_mul(MB) / 100 * FILL_UP_TO_PERCENT;
    let used = disk_used.saturating_add(pending);
    if used >= cap {
        return Err("Full: the storage limit leaves no room for more sites");
    }
    Ok(Some((cap - used) / DISK_PER_RECORD_BYTE))
}

/// Fills the node's space round after round until it stops.
pub(super) async fn fill_space(inner: Arc<Inner>) {
    let mut wait = FIRST_FILL_AFTER;
    loop {
        tokio::select! {
            () = inner.stopped() => return,
            () = tokio::time::sleep(wait) => {}
        }
        wait = FILL_EVERY;
        if let Err(err) = fill_round(&inner).await {
            debug!("no sites filled this round: {err:#}");
            inner.update_fill(|state| state.detail = format!("Waiting to try again: {err:#}"));
        }
    }
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
    if let Some(done) = state.done_at {
        if now < done + FILL_AGAIN_AFTER.as_secs() {
            inner.update_fill(|s| s.detail = "Done: holds the trusted node's crawled sites".into());
            return Ok(());
        }
        state.done_at = None;
        state.next = 0;
    }
    // Sites taken in are reckoned on disk once an index is built after them.
    if inner.last_build.load(Ordering::SeqCst) > state.pending_since {
        state.pending = 0;
    }
    let settings = inner.settings();
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
    let mut room = match room(settings.storage_limit_mb, inner.disk_used(), state.pending) {
        Ok(room) => room,
        Err(why) => {
            inner.update_fill(|s| s.detail = why.into());
            return Ok(());
        }
    };
    inner.update_fill(|s| s.detail = "Asking a trusted node for its crawled sites".into());
    let mut prefer = state.peer.as_deref().and_then(|p| p.parse().ok());
    let mut taken: u64 = 0;
    let mut busy = 0;
    while taken < FILL_PER_ROUND && room != Some(0) {
        let count = (FILL_PER_ROUND - taken).min(u64::from(FILL_PAGE)) as u32;
        let Some(page) = net.fill(prefer, state.next, count).await? else {
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
        prefer = Some(page.peer);
        let peer = page.peer.to_string();
        if state.peer.as_deref() != Some(&peer) {
            // Another node's list is in another order: start it from the top.
            let restart = state.peer.is_some() && state.next > 0;
            state.peer = Some(peer);
            if restart {
                state.next = 0;
                continue;
            }
        }
        let n = page.records.len() as u64;
        let done = page.done();
        if n > 0 {
            let records = page.records;
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
        state.pending += page.bytes.saturating_mul(DISK_PER_RECORD_BYTE);
        room = room.map(|left| left.saturating_sub(page.bytes));
        taken += n;
        state.filled += n;
        state.next = page.next;
        state.total = page.total;
        if done {
            state.done_at = Some(now_unix());
        }
        let detail = if done {
            "Done: holds the trusted node's crawled sites".to_string()
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
    }
    if taken > 0 {
        info!("took in {taken} crawled sites from a trusted node to fill free space");
        inner.journal.info(format!(
            "Took in {taken} crawled sites from a trusted node to fill free space"
        ));
    }
    Ok(())
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
    fn remembers_how_far_it_got() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(FillState::load(dir.path()), FillState::default());
        let state = FillState {
            peer: Some("12D3KooW".into()),
            next: 5_000,
            total: 1_000_000,
            filled: 3_000,
            done_at: None,
            pending: 77,
            pending_since: 1,
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
        assert_eq!(state.status().position, 5_000);
    }
}
