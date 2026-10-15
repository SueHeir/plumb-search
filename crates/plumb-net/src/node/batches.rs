//! Crawl batches: publishing them, hearing of them, fetching them, and
//! handing on the records two crawlers agree on.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use libp2p::request_response::{self};
use libp2p::{gossipsub, PeerId};
use plumb_core::locks::Locked;
use plumb_core::{now_unix, SiteRecord};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::answering::holds_denied;
use super::{
    Answer, Task, MAX_BATCH_LISTS, MAX_FETCHES, MAX_FETCHES_PER_PEER, MAX_LISTS_SERVING,
    MAX_REFUSED, MAX_WANTED, RECORD_DELIVERIES,
};
use crate::agree::Agreement;
use crate::assign::{epoch_of, is_assigned, MAX_SHARE_PPM};
use crate::batch::{
    accept_batch, accept_news, accept_own_batch, accept_trusted_batch, mostly_kept, Batch,
    SignedHeader, MAX_BATCH_AGE_EPOCHS, MAX_BATCH_RECORDS,
};
use crate::hash::Hash;
use crate::proto::*;
use crate::storage::{allocation_for, Reservation};
use crate::store::{read_held_with_budget, BatchStore, CrawlerView};

/// Starts the network side of a node. Returns its handle and the records
/// accepted from other nodes' batches, one batch at a time. Records that
/// carry headlines ([`SiteRecord::news`]) carry nothing else: they are
/// trusted crawlers' feed checks, for the node's headline store.
/// One bounded delivery with disk room held until the receiver has saved it.
/// Call `acknowledgment.acknowledge()` after durable persistence. Unacknowledged
/// held batches replay after restart; replay never awards credits again.
#[derive(Debug)]
pub struct RecordDelivery {
    pub records: Vec<SiteRecord>,
    pub reservation: Option<Reservation>,
    pub acknowledgment: crate::store::DeliveryAck,
}

/// The receiver's bounded raw encoding envelope. A recovered output above
/// this size reaches the receiver without a storage token: only the receiver
/// can partition headlines and filter sites before deciding what to persist.
pub const MAX_RECORD_DELIVERY_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn max_delivery_bytes(records: usize) -> u64 {
    // Agreement can release a crawl and a mention per input record. Leave
    // room for merged metadata as well as a full protocol-sized record.
    allocation_for(
        (records as u64).saturating_mul(2 * (crate::batch::MAX_RECORD_BYTES as u64 + 4096)),
    )
}

/// Queue only the serialized payload's allocation, including the append's
/// possible recovery newline. News admits its complete rewrite separately.
pub(super) fn delivery_bytes(records: &[SiteRecord]) -> Result<u64> {
    delivery_bytes_up_to(records, u64::MAX)
}

fn delivery_bytes_up_to(records: &[SiteRecord], limit: u64) -> Result<u64> {
    use std::io::Write;
    let mut size = plumb_core::storage::ByteCount { bytes: 0, limit };
    for record in records {
        plumb_core::stored::write(&mut size, record)?;
        size.write_all(b"\n")?;
    }
    Ok(allocation_for(size.bytes.saturating_add(1)))
}

/// Keep the final confirmed output even when sizing fails. The receiver
/// admits actual growth before writing and retries without observing agreement
/// or crediting this stored batch again.
pub(super) fn queue_confirmed_delivery(
    permit: mpsc::OwnedPermit<RecordDelivery>,
    mut delivery: RecordDelivery,
    measured: impl FnOnce(&[SiteRecord]) -> Result<u64>,
) {
    if let Err(err) =
        release_delivery_slack_with(&mut delivery.reservation, || measured(&delivery.records))
    {
        warn!(
            "cannot reduce confirmed delivery, retaining its admission for receiver retry: {err:#}"
        );
    }
    permit.send(delivery);
}

pub(super) fn release_delivery_slack_with(
    reservation: &mut Option<Reservation>,
    measured: impl FnOnce() -> Result<u64>,
) -> Result<()> {
    let Some(reserved) = reservation else {
        return Ok(());
    };
    let bytes = measured()?;
    anyhow::ensure!(
        bytes <= reserved.bytes(),
        "confirmed delivery exceeds its admitted size bound"
    );
    reserved.commit(reserved.bytes() - bytes, 0, 0);
    Ok(())
}

// A missing reply can also mean the peer's serving slots are busy. Retain
// its source and wake separately from channel capacity/maintenance. Both the
// number of IDs and attempts per ID are bounded; invalid batches are refused.
#[derive(Default)]
pub(super) struct BatchRetries {
    pub(super) entries: HashMap<Hash, BatchRetry>,
}

// Empty headers also represent a busy source on the existing wire protocol.
// Retry boundedly, even before any IDs are known; never wait for maintenance.
#[derive(Default)]
pub(super) struct BatchListRetries {
    pub(super) entries: HashMap<PeerId, BatchListRetry>,
}

pub(super) struct BatchListRetry {
    since: u64,
    at: Option<tokio::time::Instant>,
    attempts: u8,
}

impl BatchListRetries {
    pub(super) fn defer(&mut self, peer: PeerId, since: u64, now: tokio::time::Instant) -> bool {
        if self.entries.len() >= MAX_BATCH_LISTS && !self.entries.contains_key(&peer) {
            return false;
        }
        let entry = self.entries.entry(peer).or_insert(BatchListRetry {
            since,
            at: None,
            attempts: 0,
        });
        if entry.attempts >= 16 {
            self.entries.remove(&peer);
            return false;
        }
        entry.since = entry.since.min(since);
        entry.at = Some(now + Duration::from_millis((100u64 << entry.attempts.min(5)).min(2000)));
        entry.attempts += 1;
        true
    }
    pub(super) fn next(&self) -> Option<tokio::time::Instant> {
        self.entries.values().filter_map(|entry| entry.at).min()
    }
    pub(super) fn due(&mut self, now: tokio::time::Instant) -> Vec<(PeerId, u64)> {
        self.entries
            .iter_mut()
            .filter_map(|(peer, entry)| {
                if entry.at.is_some_and(|at| at <= now) {
                    entry.at = None;
                    Some((*peer, entry.since))
                } else {
                    None
                }
            })
            .collect()
    }
    pub(super) fn forget(&mut self, peer: &PeerId) {
        self.entries.remove(peer);
    }
}

pub(super) struct BatchRetry {
    sources: Vec<PeerId>,
    at: Option<tokio::time::Instant>,
    pub(super) attempts: u8,
}

impl BatchRetries {
    pub(super) fn defer(
        &mut self,
        id: Hash,
        source: PeerId,
        mut rest: Vec<PeerId>,
        now: tokio::time::Instant,
    ) -> bool {
        if self.entries.len() >= MAX_WANTED && !self.entries.contains_key(&id) {
            return false;
        }
        let entry = self.entries.entry(id).or_insert(BatchRetry {
            sources: Vec::new(),
            at: None,
            attempts: 0,
        });
        if entry.attempts >= 16 {
            self.entries.remove(&id);
            return false;
        }
        if !rest.contains(&source) {
            rest.push(source);
        }
        rest.sort();
        rest.dedup();
        entry.sources = rest;
        entry.at = Some(now + Duration::from_millis((100u64 << entry.attempts.min(5)).min(2000)));
        entry.attempts += 1;
        true
    }
    pub(super) fn next(&self) -> Option<tokio::time::Instant> {
        self.entries.values().filter_map(|entry| entry.at).min()
    }
    pub(super) fn due(&mut self, now: tokio::time::Instant) -> Vec<(Hash, Vec<PeerId>)> {
        self.entries
            .iter_mut()
            .filter_map(|(id, entry)| {
                if entry.at.is_some_and(|at| at <= now) {
                    entry.at = None;
                    Some((*id, std::mem::take(&mut entry.sources)))
                } else {
                    None
                }
            })
            .collect()
    }
    pub(super) fn forget(&mut self, id: &Hash) {
        self.entries.remove(id);
    }
}

/// `records` in runs that fit one batch each: [`MAX_BATCH_RECORDS`] lines,
/// a record with an icon taking two (see [`Batch::sign`]).
pub(super) fn batch_chunks(records: &[SiteRecord]) -> Vec<&[SiteRecord]> {
    let mut chunks = Vec::new();
    let (mut start, mut lines) = (0, 0);
    for (i, record) in records.iter().enumerate() {
        let need = 1 + usize::from(record.icon.is_some()) + usize::from(!record.news.is_empty());
        if lines + need > MAX_BATCH_RECORDS {
            chunks.push(&records[start..i]);
            (start, lines) = (i, 0);
        }
        lines += need;
    }
    if start < records.len() {
        chunks.push(&records[start..]);
    }
    chunks
}

/// Rebuilds the agreement step from the batches held, oldest first, so it
/// needs no file of its own. What it confirms was passed on before.
/// [`BatchStore::crawlers`], with this node and its trusted nodes marked.
pub(super) fn crawler_views(
    store: &BatchStore,
    me: PeerId,
    trusted: &[PeerId],
    now: u64,
) -> Vec<CrawlerView> {
    let me = me.to_string();
    let trusted: Vec<String> = trusted.iter().map(ToString::to_string).collect();
    let mut crawlers = store.crawlers(now);
    for view in &mut crawlers {
        view.me = view.peer_id == me;
        view.trusted = trusted.contains(&view.peer_id);
    }
    crawlers
}

/// Rebuild agreement without awarding its already-counted credits again.
/// Also returns the batches held, oldest first, up to the last one whose
/// records may not have been delivered, for [`resend_pending`] to replay:
/// none unless the node stopped with deliveries in flight or trusts a
/// crawler anew, so it doesn't normally build a second agreement.
pub(super) fn replay_agreement(
    store: &BatchStore,
    me: PeerId,
    trusted: &[PeerId],
) -> (Agreement, Vec<Hash>) {
    let now = now_unix();
    let mut replay = store.ids_oldest_first();
    let mut agreement = Agreement::new(me, trusted.iter().copied());
    let mut pending = 0;
    for (n, id) in replay.iter().enumerate() {
        let batch = match store.get(id) {
            Ok(Some(batch)) => batch,
            Ok(None) => continue,
            Err(err) => {
                warn!("cannot read batch {id}: {err:#}");
                continue;
            }
        };
        let made = batch.header.header.created_at;
        let Ok(crawler) = batch.check(made) else {
            continue;
        };
        let is_trusted = trusted.contains(&crawler);
        let records = if crawler == me {
            accept_own_batch(&batch, &crawler, made)
        } else if is_trusted {
            accept_trusted_batch(&batch, &crawler, made)
        } else {
            accept_batch(&batch, &crawler, made)
        };
        let confirmed = agreement.observe(crawler, records, made);
        // Records resend_pending would send again.
        if crawler != me
            && !store.delivery_ack(id, is_trusted).acknowledged()
            && (!confirmed.is_empty() || (is_trusted && !accept_news(&batch, made).is_empty()))
        {
            pending = n + 1;
        }
    }
    replay.truncate(pending);
    agreement.prune(now);
    agreement.take_verdicts();
    (agreement, replay)
}

/// Reconstruct one batch's downstream output at a time. Per-batch durable
/// acknowledgments distinguish an applied agreement from a persisted inbox.
/// Trusted and quorum deliveries have different markers, so newly trusting a
/// crawler still upgrades previously accepted records with their full text.
pub(super) fn resend_pending(
    store: &Mutex<BatchStore>,
    ids: &[Hash],
    records: &mpsc::Sender<RecordDelivery>,
    me: PeerId,
    trusted: &[PeerId],
    stopped: &std::sync::atomic::AtomicBool,
) {
    let stopping = || records.is_closed() || stopped.load(std::sync::atomic::Ordering::SeqCst);
    let mut agreement = Agreement::new(me, trusted.iter().copied());
    for id in ids {
        if stopping() {
            return;
        }
        let (batch, budget) = {
            let store = store.locked();
            (store.get(id), store.storage().cloned())
        };
        let batch = match batch {
            Ok(Some(batch)) => batch,
            Ok(None) => continue,
            Err(err) => {
                warn!("cannot read batch {id}: {err:#}");
                continue;
            }
        };
        let made = batch.header.header.created_at;
        let Ok(crawler) = batch.check(made) else {
            continue;
        };
        let is_trusted = trusted.contains(&crawler);
        let accepted = if crawler == me {
            accept_own_batch(&batch, &crawler, made)
        } else if is_trusted {
            accept_trusted_batch(&batch, &crawler, made)
        } else {
            accept_batch(&batch, &crawler, made)
        };
        let mut confirmed = agreement.observe(crawler, accepted, made);
        agreement.take_verdicts(); // Recovery must never award credits.
        if crawler == me {
            continue;
        }
        if is_trusted {
            confirmed.extend(accept_news(&batch, made));
        }
        let acknowledgment = store.locked().delivery_ack(id, is_trusted);
        if confirmed.is_empty() || acknowledgment.acknowledged() {
            continue;
        }
        let bytes = if budget.is_some() {
            match delivery_bytes_up_to(&confirmed, MAX_RECORD_DELIVERY_BYTES) {
                Ok(bytes) => Some(bytes),
                Err(err) => {
                    // Agreement can merge observations from many signed
                    // batches. Their individual bounds do not bound this
                    // final output. Never wait for an impossible full-output
                    // admission before the receiver can classify it; its
                    // headline partition and site filter may also make the
                    // actual raw payload smaller than this envelope.
                    warn!("pending delivery exceeds sizing envelope; receiver will classify and admit it: {err:#}");
                    None
                }
            }
        } else {
            None
        };
        let mut delay = std::time::Duration::from_millis(100);
        let reservation = loop {
            if stopping() {
                return;
            }
            let Some(bytes) = bytes else {
                break None;
            };
            match budget.as_ref().map(|b| b.reserve(bytes, false)).transpose() {
                Ok(reservation) => break reservation,
                Err(err) => {
                    debug!("pending delivery waits for storage: {err:#}");
                    std::thread::sleep(delay);
                    delay = delay
                        .saturating_mul(2)
                        .min(std::time::Duration::from_secs(5));
                }
            }
        };
        let mut delivery = RecordDelivery {
            records: confirmed,
            reservation,
            acknowledgment,
        };
        loop {
            if stopping() {
                return;
            }
            match records.try_send(delivery) {
                Ok(()) => break,
                Err(mpsc::error::TrySendError::Closed(_)) => return,
                Err(mpsc::error::TrySendError::Full(pending)) => {
                    delivery = pending;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }
}

/// `records` by the epoch their homepage was crawled in (records not
/// crawled, such as linked sites, in the current one), leaving out crawls
/// too old for other nodes to take and ones from the future. A crawl goes
/// with the epoch before its own when it was this node's to crawl then
/// (`assigned`) and not in its own: a crawl round picks its sites by the
/// epoch it starts in and can run on past midnight, and a batch of an
/// epoch takes crawls until the end of the next ([`crate::batch`]).
pub(super) fn by_crawl_epoch(
    records: Vec<SiteRecord>,
    now: u64,
    assigned: impl Fn(u64, &str) -> bool,
) -> BTreeMap<u64, Vec<SiteRecord>> {
    let current = epoch_of(now);
    let mut by_epoch: BTreeMap<u64, Vec<SiteRecord>> = BTreeMap::new();
    for record in records {
        let epoch = match record.crawled_at.map(epoch_of) {
            None => current,
            Some(own)
                if own > 0
                    && !assigned(own, &record.domain)
                    && assigned(own - 1, &record.domain) =>
            {
                own - 1
            }
            Some(own) => own,
        };
        if epoch + MAX_BATCH_AGE_EPOCHS > current && epoch <= current {
            by_epoch.entry(epoch).or_default().push(record);
        }
    }
    by_epoch
}

impl Task {
    /// Signs and announces `records` as batches of this node's crawls: one
    /// per epoch the homepages were crawled in (records not crawled go with
    /// the current one), at most [`MAX_BATCH_RECORDS`] each. Crawls too old
    /// for other nodes to take are left out. Returns the last batch's id.
    pub(super) fn publish(&mut self, records: Vec<SiteRecord>) -> Result<Option<Hash>> {
        let now = now_unix();
        let me = *self.swarm.local_peer_id();
        let share = self.config.share_ppm;
        let assigned = |epoch, domain: &str| is_assigned(epoch, &me, domain, share);
        let mut last = None;
        for (epoch, records) in by_crawl_epoch(records, now, assigned) {
            for chunk in batch_chunks(&records) {
                if let Some(id) = self.publish_batch(chunk, epoch, now)? {
                    last = Some(id);
                }
            }
        }
        Ok(last)
    }

    fn publish_batch(
        &mut self,
        records: &[SiteRecord],
        epoch: u64,
        now: u64,
    ) -> Result<Option<Hash>> {
        // Fresh crawls reserve their headroom before signing. This
        // reservation remains held while the store writes, so the ledger
        // counts it meanwhile.
        let budget = self.storage.clone();
        let _headroom = if let Some(budget) = budget {
            let mut size = crate::storage::ByteCount {
                bytes: 0,
                limit: crate::batch::MAX_BATCH_BYTES as u64,
            };
            serde_json::to_writer(&mut size, records)?;
            Some(budget.reserve(
                allocation_for(size.bytes.saturating_mul(2).saturating_add(4096)),
                false,
            )?)
        } else {
            None
        };
        let share = self.config.share_ppm.min(MAX_SHARE_PPM);
        let Some(batch) = Batch::sign(&self.key, records, epoch, share, now)? else {
            return Ok(None);
        };
        let id = batch.id();
        let held = {
            let mut store = self.lock_store();
            store.insert(&batch)?;
            store.len()
        };
        // Our own crawl counts as one crawler towards agreement, sites
        // fetched to settle a dispute included; what it confirms we
        // already have.
        let me = *self.swarm.local_peer_id();
        if self.config.follow_crawls {
            self.agreement
                .observe(me, accept_own_batch(&batch, &me, now), now);
        }
        self.count_credits();
        let agreement = self.agreement.status();
        self.with_status(|s| {
            s.batches_published += 1;
            s.batches_held = held;
            s.agreement = agreement;
        });
        info!("published batch {id} of {} records", batch.records.len());
        self.announce(batch.header);
        Ok(Some(id))
    }

    pub(super) fn announce(&mut self, header: SignedHeader) {
        let data = serde_json::to_vec(&header).expect("headers encode");
        match self
            .swarm
            .behaviour_mut()
            .gossipsub
            .publish(self.topic.clone(), data)
        {
            Ok(_) => {}
            Err(gossipsub::PublishError::NoPeersSubscribedToTopic) => {
                debug!("no node to announce a batch to yet; trying again later");
                self.unannounced.push(header);
            }
            Err(err) => warn!("cannot announce a batch: {err}"),
        }
    }

    /// A batch header heard on the gossip topic: check it, and fetch the
    /// batch if it is new to us.
    pub(super) fn on_header(&mut self, from: PeerId, data: &[u8]) -> gossipsub::MessageAcceptance {
        let Ok(header) = serde_json::from_slice::<SignedHeader>(data) else {
            return gossipsub::MessageAcceptance::Reject;
        };
        let crawler = match header.check(now_unix()) {
            Ok(crawler) => crawler,
            Err(err) => {
                debug!("rejected a batch header from {from}: {err:#}");
                return gossipsub::MessageAcceptance::Reject;
            }
        };
        self.want(header.id(), vec![from, crawler]);
        gossipsub::MessageAcceptance::Accept
    }

    /// Remembers not to fetch batch `id` again.
    fn refuse(&mut self, id: Hash) {
        self.batch_retries.forget(&id);
        if self.refused.len() >= MAX_REFUSED {
            self.refused.clear();
        }
        self.refused.insert(id);
    }

    fn want(&mut self, id: Hash, sources: Vec<PeerId>) {
        // A full queue drops the newcomer: a real batch is heard of again
        // (gossip, catch-up), and fresh keys can't grow the queue forever.
        if self.wanted_ids.len() >= MAX_WANTED
            || self.pending_batches.contains(&id)
            || self.refused.contains(&id)
            || self.lock_store().contains(&id)
            || !self.wanted_ids.insert(id)
        {
            return;
        }
        self.wanted.push_back((id, sources));
        self.fetch_more();
    }

    fn peer_has_fetch_room(&self, peer: &PeerId) -> bool {
        self.fetching
            .values()
            .filter(|(_, _, source)| source == peer)
            .count()
            < MAX_FETCHES_PER_PEER
    }

    pub(super) fn has_fetchable_batch(&self) -> bool {
        self.wanted.iter().any(|(_, sources)| {
            sources.iter().any(|peer| {
                *peer != self.key.public().to_peer_id() && self.peer_has_fetch_room(peer)
            })
        })
    }

    pub(super) fn fetch_more(&mut self) {
        let mut remaining = self.wanted.len();
        while remaining > 0
            && !self.ingest_blocked
            && self.fetching.len() + self.pending_batches.len() < MAX_FETCHES
            && self.pending_batches.len() < RECORD_DELIVERIES
            && (!self.config.follow_crawls || self.records.capacity() > 0)
        {
            remaining -= 1;
            let Some((id, mut sources)) = self.wanted.pop_front() else {
                return;
            };
            sources.retain(|p| *p != self.key.public().to_peer_id());
            if sources.is_empty() {
                self.wanted_ids.remove(&id);
                self.batch_retries.forget(&id);
                continue;
            }
            let Some(source_at) = sources
                .iter()
                .position(|peer| self.peer_has_fetch_room(peer))
            else {
                self.wanted.push_back((id, sources));
                continue;
            };
            let Some(source) = sources.get(source_at).copied() else {
                // Nobody connected has it; forget it until it is heard of again.
                self.wanted_ids.remove(&id);
                continue;
            };
            self.add_known_addresses(&source);
            let request = self
                .swarm
                .behaviour_mut()
                .batches
                .send_request(&source, BatchRequest::Get(id));
            sources.remove(source_at);
            self.fetching.insert(request, (id, sources, source));
        }
    }

    /// Tells the swarm where a node we know of listens, before asking it
    /// for something while not connected.
    pub(super) fn add_known_addresses(&mut self, peer: &PeerId) {
        if self.swarm.is_connected(peer) {
            return;
        }
        for addr in self.bucket_peers.get(peer).cloned().unwrap_or_default() {
            self.swarm.add_peer_address(*peer, addr);
        }
    }

    pub(super) fn on_batch_event(
        &mut self,
        event: request_response::Event<BatchRequest, BatchResponse>,
    ) {
        match event {
            request_response::Event::Message {
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => {
                if self.lists_serving >= MAX_LISTS_SERVING {
                    self.with_status(|status| status.batch_busy_replies += 1);
                    let response = match request {
                        BatchRequest::Get(_) => BatchResponse::Batch(None),
                        BatchRequest::List { .. } => BatchResponse::Headers(Vec::new()),
                    };
                    let _ = self
                        .swarm
                        .behaviour_mut()
                        .batches
                        .send_response(channel, response);
                    return;
                }
                self.lists_serving += 1;
                let store = self.store.clone();
                let tx = self.answers_tx.clone();
                let source = self.source.clone();
                tokio::task::spawn_blocking(move || {
                    let response = match request {
                        // Read and parsed with the store let go: a batch
                        // can be 16 MB.
                        BatchRequest::Get(id) => {
                            let (held, budget) = {
                                let store = store.locked();
                                (store.located(&id), store.storage().cloned())
                            };
                            let batch = held.map_or(Ok(None), |path| {
                                read_held_with_budget(&path, budget.as_deref())
                            });
                            let batch = batch.unwrap_or_else(|err| {
                                warn!("cannot read a batch: {err:#}");
                                None
                            });
                            // Signed, it can't be passed on without what
                            // the denylist takes off: the asker gets it
                            // from another node, or not at all.
                            BatchResponse::Batch(
                                batch
                                    .filter(|batch| !holds_denied(source.as_ref(), &batch.records)),
                            )
                        }
                        BatchRequest::List { since_epoch } => BatchResponse::Headers(
                            store
                                .locked()
                                .headers_since(since_epoch, MAX_LISTED_BATCHES),
                        ),
                    };
                    let _ = tx.send(Answer::Batch(channel, response));
                });
            }
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Response {
                        request_id,
                        response,
                    },
                ..
            } => match response {
                BatchResponse::Headers(headers) => {
                    let Some((_, since)) = self.batch_lists.remove(&request_id) else {
                        if let Some((id, rest, source)) = self.fetching.remove(&request_id) {
                            self.retry_known_batch(id, source, rest);
                            self.fetch_more();
                        }
                        return;
                    };
                    if headers.is_empty() {
                        self.retry_batch_list(peer, since);
                        return;
                    }
                    self.list_retries.forget(&peer);
                    self.with_status(|status| status.batch_lists_received += 1);
                    let now = now_unix();
                    for header in headers.into_iter().take(MAX_LISTED_BATCHES) {
                        if let Ok(crawler) = header.check(now) {
                            self.want(header.id(), vec![peer, crawler]);
                        }
                    }
                }
                BatchResponse::Batch(batch) => {
                    if let Some((peer, since)) = self.batch_lists.remove(&request_id) {
                        self.retry_batch_list(peer, since);
                        return;
                    }
                    let Some((id, rest, source)) = self.fetching.remove(&request_id) else {
                        return;
                    };
                    match batch {
                        Some(batch) if batch.id() == id => self.on_batch(peer, batch),
                        Some(_) => {
                            warn!("{peer} sent a different batch than asked for");
                            self.retry_known_batch(id, source, rest);
                        }
                        None => self.retry_known_batch(id, source, rest),
                    }
                    self.fetch_more();
                }
            },
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                debug!("batch request to {peer} failed: {error}");
                if let Some((peer, since)) = self.batch_lists.remove(&request_id) {
                    self.retry_batch_list(peer, since);
                }
                if let Some((id, rest, source)) = self.fetching.remove(&request_id) {
                    self.retry_known_batch(id, source, rest);
                    self.fetch_more();
                }
            }
            _ => {}
        }
    }

    pub(super) fn retry_known_batch(&mut self, id: Hash, source: PeerId, rest: Vec<PeerId>) {
        if !self
            .batch_retries
            .defer(id, source, rest, tokio::time::Instant::now())
        {
            self.wanted_ids.remove(&id);
            debug!("batch {id}: bounded retry attempts exhausted");
        } else {
            self.with_status(|status| status.batch_retries += 1);
        }
    }

    pub(super) fn ask_batch_list(&mut self, peer: PeerId, since: u64) {
        if self
            .batch_lists
            .values()
            .any(|(pending, _)| *pending == peer)
        {
            return;
        }
        if self.batch_lists.len() >= MAX_BATCH_LISTS {
            self.retry_batch_list(peer, since);
            return;
        }
        self.add_known_addresses(&peer);
        let id = self
            .swarm
            .behaviour_mut()
            .batches
            .send_request(&peer, BatchRequest::List { since_epoch: since });
        self.batch_lists.insert(id, (peer, since));
    }

    fn retry_batch_list(&mut self, peer: PeerId, since: u64) {
        if self
            .list_retries
            .defer(peer, since, tokio::time::Instant::now())
        {
            self.with_status(|status| status.batch_list_retries += 1);
        } else {
            debug!("batch headers from {peer}: bounded retry attempts exhausted");
        }
    }

    fn defer_batch(&mut self, id: Hash, from: PeerId) {
        if self.wanted.len() < MAX_WANTED && self.wanted_ids.insert(id) {
            self.wanted.push_back((id, vec![from]));
        }
    }

    fn on_batch(&mut self, from: PeerId, batch: Batch) {
        let id = batch.id();
        self.wanted_ids.remove(&id);
        if self.pending_batches.contains(&id) || self.lock_store().contains(&id) {
            return;
        }
        let now = now_unix();
        let crawler = match batch.check(now) {
            Ok(crawler) => crawler,
            Err(err) => {
                warn!("rejected batch {id} from {from}: {err:#}");
                self.refuse(id);
                return;
            }
        };
        let trusted = self.config.trusted_peers.contains(&crawler);
        let accepted = if trusted {
            accept_trusted_batch(&batch, &crawler, now)
        } else {
            accept_batch(&batch, &crawler, now)
        };
        // A batch none of whose records count here is not kept: anyone can
        // make keys and sign batches, and they would otherwise fill the disk.
        // Nor is one mostly of lines that do not count, held whole for the
        // few that do.
        if !trusted && (accepted.is_empty() || !mostly_kept(&batch, &accepted)) {
            debug!(
                "batch {id} from {from} is mostly records this node does not keep; not holding it"
            );
            self.refuse(id);
            return;
        }
        // Headlines skip agreement, as icons do: only trusted crawlers'
        // are taken, and they go to the node's headline store, not its
        // records (records carrying only headlines; see `accept_news`).
        let news = if trusted {
            accept_news(&batch, now)
        } else {
            Vec::new()
        };
        let lines = batch.records.len();
        let made = batch.header.header.created_at;
        // Reserve a bounded delivery slot before spawning disk work. Capacity
        // exhaustion is temporary: do not add the id to the permanent refusals.
        let permit = if self.config.follow_crawls {
            match self.records.clone().try_reserve_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    self.defer_batch(id, from);
                    return;
                }
            }
        } else {
            None
        };
        if self.pending_batches.len() >= RECORD_DELIVERIES {
            self.defer_batch(id, from);
            return;
        }
        if !self.pending_batches.insert(id) {
            return;
        }
        let delivery = if permit.is_some() {
            max_delivery_bytes(accepted.len() + news.len())
        } else {
            0
        };
        let store = self.store.clone();
        let answers = self.answers_tx.clone();
        tokio::task::spawn_blocking(move || {
            let saved = store.locked().insert_for_delivery(&batch, delivery);
            let _ = answers.send(Answer::StoredBatch {
                id,
                crawler,
                from,
                made,
                lines,
                accepted,
                news,
                permit,
                saved,
            });
        });
    }

    pub(super) fn lock_store(&self) -> std::sync::MutexGuard<'_, BatchStore> {
        self.store.locked()
    }
}
