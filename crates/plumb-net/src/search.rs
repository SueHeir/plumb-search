//! Searching the network without sending the query (see [`crate::bucket`]).
//!
//! For each bucket of a search, a node makes a **throwaway identity**: a
//! new key, a swarm of its own and a new connection, used for that one
//! request and dropped. So the node answering cannot tie the request to the
//! asking node's permanent id (the one its crawls are signed with), nor to
//! the other buckets of the same search, which go to other nodes under
//! other identities. And each request goes **through another node**, a
//! relay picked at random, sealed to the answering node's key (see
//! [`crate::oblivious`]), so the node answering sees the relay's IP address,
//! not the asker's, and the relay never sees the bucket. Only when no other
//! node can relay (a network of two) does a request go straight to the
//! node; [`NetSearch::direct`] counts those.
//!
//! Every answer is checked: a record with a proof gets the text of its
//! signed crawl, an answer holding a proof that does not check out is
//! dropped whole, and a record without a proof only ever links to the
//! domain it names. A site is **confirmed** once signed crawls from
//! [`QUORUM`] different crawlers agree on it ([`crate::agree`]), whether
//! one answer carried them all or several answers did.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use libp2p::request_response::{self, ProtocolSupport};
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{relay, Multiaddr, PeerId, StreamProtocol};
use plumb_core::{canonical_domain, registrable_domain, Signals, SiteRecord};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::agree::{agree, QUORUM};
use crate::batch::MAX_RECORD_BYTES;
use crate::bucket::{bucket_of, matches, query_keys, search_buckets};
use crate::cache::BucketCache;
use crate::credits::Wallet;
use crate::oblivious::{
    open_response, seal_request, seal_request_sized, ObliviousRequest, ObliviousResponse,
    MAX_MESSAGE, OBLIVIOUS_PROTOCOL, PRIORITY_REQUEST_SIZE, REPORT_REQUEST_SIZE,
};
use crate::popularity::Report;
use crate::proto::{
    BucketRequest, BucketResponse, ReportResponse, BUCKET_PROTOCOL, MAX_EXTRA_PROOFS,
};
use crate::rounds::fill_round;

/// Nodes each bucket is asked of, when there are that many, so that one
/// node cannot hide a site or boost one's popularity alone.
pub const NODES_PER_BUCKET: usize = 2;
/// Most records accepted in one bucket.
pub const MAX_BUCKET_RECORDS: usize = 20_000;
/// Relays a bucket request is tried through before giving up on it.
pub const RELAY_TRIES: usize = 2;

/// The result of a network search: candidate sites to rank locally.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NetSearch {
    /// Buckets asked for, the real ones and the padding.
    pub buckets: usize,
    /// Requests sent, each to one node under its own identity.
    pub asked: usize,
    /// Requests answered in time.
    pub answered: usize,
    /// Of those, answered sealed through a relay, so the node answering
    /// never saw this node's IP address.
    pub relayed: usize,
    /// Requests sent straight to the node answering, because no other node
    /// could relay them. That node saw this node's IP address.
    pub direct: usize,
    /// Answers dropped because a proof in them did not check out.
    pub rejected: usize,
    /// Requests a busy node turned away, and of those, the ones it then
    /// answered for a token (see [`crate::credits`]).
    #[serde(default)]
    pub busy: usize,
    #[serde(default)]
    pub priority: usize,
    /// The query's buckets answered from this node's own copies of ones it
    /// fetched lately, without asking the network (see [`crate::cache`]).
    #[serde(default)]
    pub cached: usize,
    /// Real buckets absent from usable local storage, awaiting a scheduled round.
    #[serde(default)]
    pub pending: usize,
    /// Retained buckets used past their freshness deadline; refresh is queued.
    #[serde(default)]
    pub stale: usize,
    /// The sites that match the query, unranked.
    pub found: Vec<FoundSite>,
    /// Size of the records fetched, for [`crate::rounds::RoundStatus`].
    #[serde(skip)]
    pub bytes: u64,
}

/// A site another node had, as checked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FoundSite {
    pub record: SiteRecord,
    /// The URL, title and description come from a signed crawl whose proof
    /// checked out.
    pub verified: bool,
    /// The node that signed that crawl.
    pub crawler: Option<String>,
    /// How many answers held this site.
    pub answers: usize,
    /// Every crawler whose signed crawl agrees with the text shown, the
    /// first being `crawler`.
    #[serde(default)]
    pub crawlers: Vec<String>,
    /// Signed crawls from at least [`QUORUM`] different crawlers agree.
    #[serde(default)]
    pub confirmed: bool,
}

impl FoundSite {
    fn add_crawler(&mut self, crawler: String) {
        if !self.crawlers.contains(&crawler) {
            self.crawlers.push(crawler);
        }
        self.confirmed = self.crawlers.len() >= QUORUM;
    }
}

/// A node that serves buckets, and where it can be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketPeer {
    pub peer: PeerId,
    pub addrs: Vec<Multiaddr>,
    /// It relays sealed requests for others, and answers those sealed to
    /// it (see [`crate::oblivious`]).
    pub oblivious: bool,
}

/// Searches `peers` for `query`: asks for the query's buckets (padded with
/// buckets this node has not fetched lately to a round, see
/// [`crate::rounds`]), each of up to [`NODES_PER_BUCKET`] nodes under a
/// throwaway identity, and keeps the sites that match the query. A node
/// that says it is busy is asked again with one of its tokens from
/// `wallet`, when it holds one.
pub async fn search(
    query: &str,
    peers: &[BucketPeer],
    wait: Duration,
    now: u64,
    wallet: Option<&Mutex<Wallet>>,
    cache: Option<&BucketCache>,
) -> NetSearch {
    let (mut buckets, keys) = search_buckets(query);
    let mut out = NetSearch::default();
    // The query's own buckets this node fetched lately are used as they
    // are; only the rest are asked for, filled out again to a whole round,
    // so a search answered partly from here looks like any other round.
    let mut kept: Vec<Vec<crate::proto::BucketRecord>> = Vec::new();
    if let Some(cache) = cache {
        let mut real: Vec<u32> = keys.iter().map(|k| bucket_of(k)).collect();
        real.sort_unstable();
        real.dedup();
        let mut missing = Vec::new();
        for bucket in real {
            match cache.get(bucket, now) {
                // A valid empty answer is also a cache hit: absence of a
                // matching site must not trigger query-dependent refetching.
                Some(answers)
                    if answers
                        .iter()
                        .any(|a| check_answer(a.clone(), now).is_some()) =>
                {
                    out.cached += 1;
                    kept.extend(answers);
                }
                _ => missing.push(bucket),
            }
        }
        buckets = if missing.is_empty() {
            Vec::new()
        } else {
            fill_round(missing, Some(cache), now)
        };
    }
    round(buckets, &keys, kept, peers, wait, now, wallet, cache, out).await
}

/// A background round (see [`crate::rounds`]): buckets this node has not
/// fetched lately, asked for exactly as a search's are, and kept in
/// `cache`.
pub async fn background_round(
    peers: &[BucketPeer],
    wait: Duration,
    now: u64,
    wallet: Option<&Mutex<Wallet>>,
    cache: &BucketCache,
) -> NetSearch {
    background_round_for(peers, wait, now, wallet, cache, Vec::new()).await
}

/// Only the scheduler calls this, with at most one round of queued bucket IDs.
pub(crate) async fn background_round_for(
    peers: &[BucketPeer],
    wait: Duration,
    now: u64,
    wallet: Option<&Mutex<Wallet>>,
    cache: &BucketCache,
    queued: Vec<u32>,
) -> NetSearch {
    let buckets = fill_round(queued, Some(cache), now);
    let out = NetSearch::default();
    round(
        buckets,
        &[],
        Vec::new(),
        peers,
        wait,
        now,
        wallet,
        Some(cache),
        out,
    )
    .await
}

/// Asks `peers` for `buckets` and adds the sites that match `keys`, and
/// those of `kept` answers, to `out`. Every fetched answer that checks out
/// goes into `cache`.
#[allow(clippy::too_many_arguments)]
async fn round(
    buckets: Vec<u32>,
    keys: &[String],
    kept: Vec<Vec<crate::proto::BucketRecord>>,
    peers: &[BucketPeer],
    wait: Duration,
    now: u64,
    wallet: Option<&Mutex<Wallet>>,
    cache: Option<&BucketCache>,
    mut out: NetSearch,
) -> NetSearch {
    out.buckets = buckets.len();
    let peers = if buckets.is_empty() { &[][..] } else { peers };
    // Spread the buckets over the nodes so that no node gets two buckets
    // of one search while others get none.
    let mut order: Vec<usize> = (0..peers.len()).collect();
    shuffle(&mut order);
    let mut next = 0;
    let mut requests = Vec::new();
    for &bucket in &buckets {
        if peers.is_empty() {
            break;
        }
        let mut chosen = Vec::new();
        for _ in 0..NODES_PER_BUCKET.min(peers.len()) {
            let peer = &peers[order[next % order.len()]];
            next += 1;
            if !chosen.contains(&peer.peer) {
                chosen.push(peer.peer);
                requests.push((bucket, peer.clone()));
            }
        }
    }
    out.asked = requests.len();
    // Relays, in a random order; each request starts at its own place in
    // it, so the requests of one search go through different relays.
    let mut relays: Vec<&BucketPeer> = peers.iter().filter(|p| p.oblivious).collect();
    shuffle(&mut relays);
    let routed: Vec<_> = requests
        .into_iter()
        .enumerate()
        .map(|(i, (bucket, target))| {
            let through: Vec<BucketPeer> = if target.oblivious {
                (0..relays.len())
                    .map(|k| relays[(i + k) % relays.len()])
                    .filter(|r| r.peer != target.peer)
                    .take(RELAY_TRIES)
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            };
            (bucket, target, through)
        })
        .collect();
    out.direct = routed
        .iter()
        .filter(|(_, _, through)| through.is_empty())
        .count();
    let answers = futures::future::join_all(routed.into_iter().map(
        |(bucket, target, through)| async move {
            let deadline = tokio::time::Instant::now() + wait;
            let answer = async {
                let (response, relayed) =
                    ask_bucket(&target, &through, BucketRequest::new(bucket), deadline, now)
                        .await?;
                if !response.busy {
                    return Ok((response, relayed, false));
                }
                let token = wallet.and_then(|w| {
                    w.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .take(&target.peer)
                });
                let Some(token) = token else {
                    return Ok((response, relayed, false));
                };
                let paid = BucketRequest {
                    bucket,
                    token: Some(token),
                };
                let (response, relayed) =
                    ask_bucket(&target, &through, paid, deadline, now).await?;
                Ok::<_, anyhow::Error>((response, relayed, true))
            };
            (bucket, answer.await)
        },
    ))
    .await;

    let mut merged: HashMap<String, FoundSite> = HashMap::new();
    let mut add = |checked: Vec<FoundSite>| {
        for site in checked {
            if !matches(&site.record, keys) {
                continue;
            }
            match merged.get_mut(&site.record.domain) {
                Some(existing) => merge_site(existing, site),
                None => {
                    merged.insert(site.record.domain.clone(), site);
                }
            }
        }
    };
    let mut fetched: HashMap<u32, Vec<Vec<crate::proto::BucketRecord>>> = HashMap::new();
    for (bucket, answer) in answers {
        let (response, relayed, paid) = match answer {
            Ok(response) => response,
            Err(err) => {
                debug!("a bucket request failed: {err:#}");
                continue;
            }
        };
        if response.busy {
            out.busy += 1;
        }
        let Some(records) = response.records else {
            continue;
        };
        if paid {
            out.busy += 1;
            out.priority += 1;
        }
        out.answered += 1;
        out.bytes += records.iter().map(|r| r.record.len() as u64).sum::<u64>();
        if relayed {
            out.relayed += 1;
        }
        let keep = cache.is_some().then(|| records.clone());
        let Some(checked) = check_answer(records, now) else {
            out.rejected += 1;
            continue;
        };
        if let Some(records) = keep {
            fetched.entry(bucket).or_default().push(records);
        }
        add(checked);
    }
    // Kept answers are checked again: a proof may have expired since.
    for records in kept {
        if let Some(checked) = check_answer(records, now) {
            add(checked);
        }
    }
    if let Some(cache) = cache {
        for (bucket, answers) in fetched {
            cache.put(bucket, answers, now);
        }
    }
    let mut found: Vec<FoundSite> = merged.into_values().collect();
    found.sort_by(|a, b| a.record.domain.cmp(&b.record.domain));
    out.found = found;
    out
}

/// Search retained buckets locally, including stale and valid empty answers.
/// This never sends requests, alters round deadlines, or extends proof validity.
pub fn cache_search(query: &str, cache: &BucketCache, now: u64) -> NetSearch {
    cache_search_with_refresh(query, cache, now).0
}

/// Return only real bucket IDs for the in-memory refresh queue, never the query.
pub(crate) fn cache_search_with_refresh(
    query: &str,
    cache: &BucketCache,
    now: u64,
) -> (NetSearch, Vec<u32>) {
    // Preserve pick_buckets' whole-query-first key order and four-unique-
    // bucket limit, including collision keys encountered before that limit.
    let mut keys = Vec::new();
    let mut buckets = Vec::new();
    for key in query_keys(query) {
        if buckets.len() == crate::bucket::BUCKETS_PER_SEARCH {
            break;
        }
        let bucket = bucket_of(&key);
        if !buckets.contains(&bucket) {
            buckets.push(bucket);
        }
        keys.push(key);
    }
    let mut out = NetSearch::default();
    let mut refresh = Vec::new();
    let mut merged: HashMap<String, FoundSite> = HashMap::new();
    for bucket in buckets {
        let Some(saved) = cache.get_retained(bucket, now) else {
            out.pending += 1;
            refresh.push(bucket);
            continue;
        };
        let mut usable = false;
        for records in saved.answers {
            let Some(checked) = check_answer(records, now) else {
                out.rejected += 1;
                continue;
            };
            usable = true;
            for site in checked {
                if !matches(&site.record, &keys) {
                    continue;
                }
                match merged.get_mut(&site.record.domain) {
                    Some(existing) => merge_site(existing, site),
                    None => {
                        merged.insert(site.record.domain.clone(), site);
                    }
                }
            }
        }
        if usable {
            out.cached += 1;
            if saved.stale {
                out.stale += 1;
                refresh.push(bucket);
            }
        } else {
            out.pending += 1;
            refresh.push(bucket);
        }
    }
    out.found = merged.into_values().collect();
    out.found
        .sort_by(|a, b| a.record.domain.cmp(&b.record.domain));
    (out, refresh)
}

pub(crate) fn bucket_ready(cache: &BucketCache, bucket: u32, now: u64) -> bool {
    cache.get_retained(bucket, now).is_some_and(|saved| {
        !saved.stale
            && saved
                .answers
                .into_iter()
                .any(|answer| check_answer(answer, now).is_some())
    })
}

/// Asks `target` for a bucket: through one of `through` (relays, tried in
/// turn) when there are any, else straight. Returns the answer and whether
/// it came through a relay.
async fn ask_bucket(
    target: &BucketPeer,
    through: &[BucketPeer],
    request: BucketRequest,
    deadline: tokio::time::Instant,
    now: u64,
) -> Result<(BucketResponse, bool)> {
    let left = || deadline.saturating_duration_since(tokio::time::Instant::now());
    if through.is_empty() {
        return fetch_bucket_with(target, request, left())
            .await
            .map(|r| (r, false));
    }
    // A relay that cannot reach the node gets one stand-in, in what is left
    // of the time. Never straight to the node: that would show it who asks.
    let mut last = None;
    for relay in through {
        match fetch_oblivious_with(relay, &target.peer, &request, left(), now).await {
            Ok(response) => return Ok((response, true)),
            Err(err) => {
                debug!("relay {} for {}: {err:#}", relay.peer, target.peer);
                last = Some(err);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no relay")))
}

/// The records of one answer, checked, or `None` when a proof in it fails.
fn check_answer(records: Vec<crate::proto::BucketRecord>, now: u64) -> Option<Vec<FoundSite>> {
    let mut out = Vec::new();
    for item in records.into_iter().take(MAX_BUCKET_RECORDS) {
        if item.record.len() > MAX_RECORD_BYTES {
            continue;
        }
        let Ok(mut record) = serde_json::from_str::<SiteRecord>(&item.record) else {
            continue;
        };
        let Some(domain) = canonical_domain(&record.domain) else {
            continue;
        };
        record.domain = domain;
        // Local bookkeeping of the node answering, of no use here.
        record.crawl_attempted_at = None;
        record.crawl_failures = 0;
        if record
            .url
            .as_deref()
            .is_some_and(|url| registrable_domain(url).as_deref() != Some(record.domain.as_str()))
        {
            record.url = None;
        }
        let mut site = FoundSite {
            record,
            verified: false,
            crawler: None,
            answers: 1,
            crawlers: Vec::new(),
            confirmed: false,
        };
        // A proof only too old to check out says nothing either way (nodes
        // hold crawls longer than proofs last), so the site is just not
        // verified; nor does a real signed crawl this node's rules do not
        // count (one its crawler was not assigned, which a node on another
        // version may offer). Any other bad proof, a forgery, means the
        // answer is not to be trusted.
        if let Some(proof) = item.proof.as_ref().filter(|p| !p.header.expired(now)) {
            match proof.verify(now) {
                Ok((signed, crawler)) if signed.domain == site.record.domain => {
                    // Other crawlers' proofs must check out too, and count
                    // only when they agree with the first.
                    let mut agreeing = Vec::new();
                    for other in item
                        .also
                        .iter()
                        .filter(|p| !p.header.expired(now))
                        .take(MAX_EXTRA_PROOFS)
                    {
                        match other.verify(now) {
                            Ok((theirs, by)) if theirs.domain == signed.domain => {
                                if agree(&signed, &theirs) {
                                    agreeing.push(by.to_string());
                                }
                            }
                            Err(_) if other.check_signed(now).is_ok() => {}
                            Ok(_) | Err(_) => {
                                warn!("a node answered with a proof that does not check out");
                                return None;
                            }
                        }
                    }
                    site.record.url = signed.url;
                    site.record.title = signed.title;
                    site.record.description = signed.description;
                    site.record.crawled_at = signed.crawled_at;
                    site.verified = true;
                    site.crawler = Some(crawler.to_string());
                    site.add_crawler(crawler.to_string());
                    for by in agreeing {
                        site.add_crawler(by);
                    }
                }
                Err(_) if proof.check_signed(now).is_ok() => {
                    debug!(
                        "a signed crawl of {} that does not count here",
                        site.record.domain
                    );
                }
                Ok(_) | Err(_) => {
                    warn!("a node answered with a proof that does not check out");
                    return None;
                }
            }
        }
        out.push(site);
    }
    Some(out)
}

/// Folds a second answer's copy of a site into the first: a signed crawl
/// wins for the text, and each popularity signal keeps the less favorable
/// value, so one node alone cannot make a site look more popular.
fn merge_site(existing: &mut FoundSite, other: FoundSite) {
    existing.answers += 1;
    let worse = |a: Signals, b: &Signals| Signals {
        harmonic_rank: worse_rank(a.harmonic_rank, b.harmonic_rank),
        pagerank_rank: worse_rank(a.pagerank_rank, b.pagerank_rank),
        tranco_rank: worse_rank(a.tranco_rank, b.tranco_rank),
        linking_domains: a.linking_domains.min(b.linking_domains),
        official_site: a.official_site && b.official_site,
        sitelinks: a.sitelinks.min(b.sitelinks),
    };
    let signals = worse(existing.record.signals.clone(), &other.record.signals);
    if other.verified && existing.verified && agree(&existing.record, &other.record) {
        for crawler in other.crawlers {
            existing.add_crawler(crawler);
        }
    } else if other.verified
        && (!existing.verified || other.crawlers.len() > existing.crawlers.len())
    {
        // A signed crawl beats none, and more agreeing crawlers beat fewer.
        let answers = existing.answers;
        *existing = FoundSite { answers, ..other };
    }
    existing.record.signals = signals;
}

/// The larger rank (worse), counting a missing one as the worst.
fn worse_rank<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        _ => None,
    }
}

#[derive(NetworkBehaviour)]
struct Fetcher {
    relay_client: relay::client::Behaviour,
    buckets: request_response::cbor::Behaviour<BucketRequest, BucketResponse>,
    oblivious: request_response::cbor::Behaviour<ObliviousRequest, ObliviousResponse>,
}

/// A swarm under a new identity, made for one request and dropped after.
fn throwaway(wait: Duration) -> Result<libp2p::Swarm<Fetcher>> {
    let config = request_response::Config::default().with_request_timeout(wait);
    crate::throwaway::swarm(|relay_client| Fetcher {
        relay_client,
        buckets: request_response::Behaviour::with_codec(
            request_response::cbor::codec::Codec::default()
                .set_request_size_maximum(1024)
                .set_response_size_maximum(64 * 1024 * 1024),
            [(
                StreamProtocol::new(BUCKET_PROTOCOL),
                ProtocolSupport::Outbound,
            )],
            config.clone(),
        ),
        oblivious: request_response::Behaviour::with_codec(
            request_response::cbor::codec::Codec::default()
                .set_request_size_maximum(2 * REPORT_REQUEST_SIZE as u64)
                .set_response_size_maximum(MAX_MESSAGE as u64 + 1024),
            [(
                StreamProtocol::new(OBLIVIOUS_PROTOCOL),
                ProtocolSupport::Outbound,
            )],
            config,
        ),
    })
}

/// Asks `peer` for `bucket` under a new identity, on a swarm made for this
/// one request. The node sees where the request comes from; a search uses
/// [`fetch_oblivious`] whenever it can.
pub async fn fetch_bucket(
    peer: &BucketPeer,
    bucket: u32,
    wait: Duration,
) -> Result<BucketResponse> {
    fetch_bucket_with(peer, BucketRequest::new(bucket), wait).await
}

/// [`fetch_bucket`] for any request, one that spends a token included.
pub async fn fetch_bucket_with(
    peer: &BucketPeer,
    request: BucketRequest,
    wait: Duration,
) -> Result<BucketResponse> {
    let mut swarm = throwaway(wait)?;
    for addr in &peer.addrs {
        swarm.add_peer_address(peer.peer, addr.clone());
    }
    swarm
        .behaviour_mut()
        .buckets
        .send_request(&peer.peer, request);
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let event = tokio::time::timeout_at(deadline, swarm.select_next_some())
            .await
            .context("no answer in time")?;
        match event {
            SwarmEvent::Behaviour(FetcherEvent::Buckets(request_response::Event::Message {
                message: request_response::Message::Response { response, .. },
                ..
            })) => return Ok(response),
            SwarmEvent::Behaviour(FetcherEvent::Buckets(
                request_response::Event::OutboundFailure { error, .. },
            )) => bail!("asking {} for a bucket: {error}", peer.peer),
            SwarmEvent::OutgoingConnectionError { error, .. } => {
                debug!(
                    "a throwaway identity could not reach {}: {error}",
                    peer.peer
                );
            }
            _ => {}
        }
    }
}

/// Asks `target` for `bucket` through `relay`, under a new identity: gets
/// the target's key from the relay, seals the request to it, and has the
/// relay pass it on (see [`crate::oblivious`]). The relay sees this node's
/// address but not the bucket; the target sees the bucket but only the
/// relay's address.
pub async fn fetch_oblivious(
    relay: &BucketPeer,
    target: &PeerId,
    bucket: u32,
    wait: Duration,
    now: u64,
) -> Result<BucketResponse> {
    fetch_oblivious_with(relay, target, &BucketRequest::new(bucket), wait, now).await
}

/// [`fetch_oblivious`] for any request. One that spends a token is padded
/// to [`PRIORITY_REQUEST_SIZE`] rather than the free requests' size.
pub async fn fetch_oblivious_with(
    relay: &BucketPeer,
    target: &PeerId,
    request: &BucketRequest,
    wait: Duration,
    now: u64,
) -> Result<BucketResponse> {
    let mut swarm = throwaway(wait)?;
    for addr in &relay.addrs {
        swarm.add_peer_address(relay.peer, addr.clone());
    }
    let deadline = tokio::time::Instant::now() + wait;
    let keys = match ask(
        &mut swarm,
        relay.peer,
        ObliviousRequest::Keys { target: *target },
        deadline,
    )
    .await?
    {
        ObliviousResponse::Keys(Some(keys)) => keys,
        _ => bail!("the relay {} has no key for {target}", relay.peer),
    };
    let (message, opener) = if request.token.is_some() {
        seal_request_sized(&keys, target, now, request, PRIORITY_REQUEST_SIZE)?
    } else {
        seal_request(&keys, target, now, request)?
    };
    let request = ObliviousRequest::Forward {
        target: *target,
        message: serde_bytes::ByteBuf::from(message),
    };
    match ask(&mut swarm, relay.peer, request, deadline).await? {
        ObliviousResponse::Sealed(Some(answer)) => open_response(opener, &answer),
        _ => bail!("the relay {} got no answer from {target}", relay.peer),
    }
}

/// Hands `report` to `target` through `relay`, under a new identity, sealed
/// the same way as [`fetch_oblivious`] and padded to
/// [`REPORT_REQUEST_SIZE`]: the relay sees this node's address but not the
/// report, the target the report but only the relay's address. Returns
/// whether the target took it (it may already hold it).
pub async fn submit_report_oblivious(
    relay: &BucketPeer,
    target: &PeerId,
    report: &Report,
    wait: Duration,
    now: u64,
) -> Result<bool> {
    let mut swarm = throwaway(wait)?;
    for addr in &relay.addrs {
        swarm.add_peer_address(relay.peer, addr.clone());
    }
    let deadline = tokio::time::Instant::now() + wait;
    let keys = match ask(
        &mut swarm,
        relay.peer,
        ObliviousRequest::Keys { target: *target },
        deadline,
    )
    .await?
    {
        ObliviousResponse::Keys(Some(keys)) => keys,
        _ => bail!("the relay {} has no key for {target}", relay.peer),
    };
    let (message, opener) = seal_request_sized(&keys, target, now, report, REPORT_REQUEST_SIZE)?;
    let request = ObliviousRequest::Forward {
        target: *target,
        message: serde_bytes::ByteBuf::from(message),
    };
    match ask(&mut swarm, relay.peer, request, deadline).await? {
        ObliviousResponse::Sealed(Some(answer)) => {
            match open_response::<ReportResponse>(opener, &answer)? {
                ReportResponse::Taken(taken) => Ok(taken),
                ReportResponse::Reports(_) => bail!("{target} answered something else"),
            }
        }
        _ => bail!("the relay {} got no answer from {target}", relay.peer),
    }
}

/// Sends `request` to `relay` and waits for its answer until `deadline`.
async fn ask(
    swarm: &mut libp2p::Swarm<Fetcher>,
    relay: PeerId,
    request: ObliviousRequest,
    deadline: tokio::time::Instant,
) -> Result<ObliviousResponse> {
    let id = swarm
        .behaviour_mut()
        .oblivious
        .send_request(&relay, request);
    loop {
        let event = tokio::time::timeout_at(deadline, swarm.select_next_some())
            .await
            .context("no answer in time")?;
        match event {
            SwarmEvent::Behaviour(FetcherEvent::Oblivious(request_response::Event::Message {
                message:
                    request_response::Message::Response {
                        request_id,
                        response,
                    },
                ..
            })) if request_id == id => return Ok(response),
            SwarmEvent::Behaviour(FetcherEvent::Oblivious(
                request_response::Event::OutboundFailure {
                    request_id, error, ..
                },
            )) if request_id == id => bail!("asking the relay {relay}: {error}"),
            SwarmEvent::OutgoingConnectionError { error, .. } => {
                debug!("a throwaway identity could not reach the relay {relay}: {error}");
            }
            _ => {}
        }
    }
}

pub fn shuffle<T>(items: &mut [T]) {
    let mut rng = rand_core::OsRng;
    for i in (1..items.len()).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use libp2p::identity::Keypair;

    use super::*;
    use crate::assign::{epoch_of, is_assigned, EPOCH_SECS, MAX_SHARE_PPM};
    use crate::batch::Batch;
    use crate::proto::BucketRecord;

    fn item(record: &SiteRecord) -> BucketRecord {
        BucketRecord {
            record: serde_json::to_string(record).unwrap(),
            proof: None,
            also: Vec::new(),
        }
    }

    #[test]
    fn unproven_records_link_only_to_their_own_site_and_drop_bookkeeping() {
        let mut lying = SiteRecord::new("usbank.com");
        lying.url = Some("https://usbank-login.example/".into());
        lying.crawl_failures = 4;
        let checked = check_answer(
            vec![item(&lying), item(&SiteRecord::new("not a domain"))],
            0,
        )
        .unwrap();
        assert_eq!(checked.len(), 1);
        assert_eq!(checked[0].record.url, None);
        assert_eq!(checked[0].record.crawl_failures, 0);
    }

    #[test]
    fn an_answer_with_a_forged_proof_is_dropped_whole_and_a_good_one_wins() {
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let now = 1_790_000_000;
        let domain = (0..)
            .map(|i| format!("bank{i}.com"))
            .find(|d| is_assigned(epoch_of(now), &peer, d, MAX_SHARE_PPM))
            .unwrap();
        let mut record = SiteRecord::new(domain.as_str());
        record.title = Some("Real Bank".into());
        record.crawled_at = Some(now);
        let batch = Batch::sign(
            &key,
            std::slice::from_ref(&record),
            epoch_of(now),
            MAX_SHARE_PPM,
            now,
        )
        .unwrap()
        .unwrap();
        let mut forged_proof = batch.proof(0);
        forged_proof.record = forged_proof.record.replace("Real Bank", "Log in here");
        let mut shown = record.clone();
        shown.title = Some("Log in here".into());
        let forged = BucketRecord {
            record: serde_json::to_string(&shown).unwrap(),
            proof: Some(forged_proof),
            also: Vec::new(),
        };
        assert!(check_answer(vec![item(&SiteRecord::new("other.com")), forged], now).is_none());

        let honest = BucketRecord {
            record: serde_json::to_string(&shown).unwrap(),
            proof: Some(batch.proof(0)),
            also: Vec::new(),
        };
        let checked = check_answer(vec![honest], now).unwrap();
        assert!(checked[0].verified);
        assert_eq!(checked[0].record.title.as_deref(), Some("Real Bank"));
    }

    #[test]
    fn two_answers_keep_the_less_favorable_popularity() {
        let mut boosted = SiteRecord::new("phish.com");
        boosted.signals.tranco_rank = Some(1);
        boosted.signals.official_site = true;
        let mut honest = SiteRecord::new("phish.com");
        honest.signals.tranco_rank = Some(900_000);
        let site = |record| FoundSite {
            record,
            verified: false,
            crawler: None,
            answers: 1,
            crawlers: Vec::new(),
            confirmed: false,
        };
        let mut merged = site(boosted);
        merge_site(&mut merged, site(honest));
        assert_eq!(merged.record.signals.tranco_rank, Some(900_000));
        assert!(!merged.record.signals.official_site);
        assert_eq!(merged.answers, 2);
    }

    /// A crawl of a site both `keys` are assigned, signed by each, with
    /// the titles given.
    fn crawls_by(keys: &[Keypair], titles: &[&str], now: u64) -> Vec<Batch> {
        let domain = (0..)
            .map(|i| format!("bank{i}.com"))
            .find(|d| {
                keys.iter()
                    .all(|k| is_assigned(epoch_of(now), &k.public().to_peer_id(), d, MAX_SHARE_PPM))
            })
            .unwrap();
        keys.iter()
            .zip(titles)
            .map(|(key, title)| {
                let mut record = SiteRecord::new(domain.as_str());
                record.title = Some(title.to_string());
                record.crawled_at = Some(now);
                Batch::sign(key, &[record], epoch_of(now), MAX_SHARE_PPM, now)
                    .unwrap()
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn two_agreeing_crawlers_confirm_a_site_in_one_answer_or_two() {
        let now = 1_790_000_000;
        let keys = [Keypair::generate_ed25519(), Keypair::generate_ed25519()];
        let batches = crawls_by(&keys, &["Real Bank", "Real Bank!"], now);
        let shown = batches[0].records[0].clone();
        let one = BucketRecord {
            record: shown.clone(),
            proof: Some(batches[0].proof(0)),
            also: vec![batches[1].proof(0)],
        };
        let checked = check_answer(vec![one], now).unwrap();
        assert!(checked[0].confirmed);
        assert_eq!(checked[0].crawlers.len(), 2);

        // The same, from two answers holding one proof each.
        let single = |batch: &Batch| BucketRecord {
            record: shown.clone(),
            proof: Some(batch.proof(0)),
            also: Vec::new(),
        };
        let mut a = check_answer(vec![single(&batches[0])], now)
            .unwrap()
            .remove(0);
        assert!(a.verified && !a.confirmed);
        let b = check_answer(vec![single(&batches[1])], now)
            .unwrap()
            .remove(0);
        merge_site(&mut a, b);
        assert!(a.confirmed);
    }

    #[test]
    fn a_disagreeing_or_repeated_crawler_does_not_confirm_and_a_forged_one_is_dropped() {
        let now = 1_790_000_000;
        let keys = [Keypair::generate_ed25519(), Keypair::generate_ed25519()];
        let batches = crawls_by(&keys, &["Real Bank", "Free crypto giveaway"], now);
        let shown = batches[0].records[0].clone();
        let disagreeing = BucketRecord {
            record: shown.clone(),
            proof: Some(batches[0].proof(0)),
            also: vec![batches[1].proof(0)],
        };
        let checked = check_answer(vec![disagreeing], now).unwrap();
        assert!(checked[0].verified && !checked[0].confirmed);
        assert_eq!(checked[0].record.title.as_deref(), Some("Real Bank"));

        let repeated = BucketRecord {
            record: shown.clone(),
            proof: Some(batches[0].proof(0)),
            also: vec![batches[0].proof(0)],
        };
        assert!(!check_answer(vec![repeated], now).unwrap()[0].confirmed);

        let mut forged = batches[1].proof(0);
        forged.record = forged.record.replace("Free crypto giveaway", "Real Bank");
        let forged = BucketRecord {
            record: shown,
            proof: Some(batches[0].proof(0)),
            also: vec![forged],
        };
        assert!(check_answer(vec![forged], now).is_none());
    }

    #[test]
    fn a_proof_too_old_to_check_leaves_the_site_unverified_not_the_answer_dropped() {
        let made = 1_790_000_000;
        let now = made + 10 * EPOCH_SECS;
        let keys = [Keypair::generate_ed25519(), Keypair::generate_ed25519()];
        let batches = crawls_by(&keys, &["Real Bank", "Real Bank!"], made);
        let shown = batches[0].records[0].clone();
        let old = BucketRecord {
            record: shown.clone(),
            proof: Some(batches[0].proof(0)),
            also: vec![batches[1].proof(0)],
        };
        let checked = check_answer(vec![old, item(&SiteRecord::new("other.com"))], now).unwrap();
        assert_eq!(checked.len(), 2);
        assert!(!checked[0].verified && !checked[0].confirmed);

        // A fresh proof with an old second one still counts on its own.
        let fresh = crawls_by(&keys[..1], &["Real Bank"], now);
        let mixed = BucketRecord {
            record: fresh[0].records[0].clone(),
            proof: Some(fresh[0].proof(0)),
            also: vec![batches[1].proof(0)],
        };
        let checked = check_answer(vec![mixed], now).unwrap();
        assert!(checked[0].verified && !checked[0].confirmed);
    }

    #[test]
    fn a_real_crawl_that_does_not_count_here_leaves_the_site_unverified() {
        // A trusted node may publish crawls of sites it was not assigned;
        // an answer offering one as a proof is honest, just not proof here.
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let now = 1_790_000_000;
        let domain = (0..)
            .map(|i| format!("bank{i}.com"))
            .find(|d| !is_assigned(epoch_of(now), &peer, d, MAX_SHARE_PPM))
            .unwrap();
        let mut record = SiteRecord::new(domain.as_str());
        record.title = Some("Real Bank".into());
        record.crawled_at = Some(now);
        let batch = Batch::sign(&key, &[record], epoch_of(now), MAX_SHARE_PPM, now)
            .unwrap()
            .unwrap();
        let unassigned = BucketRecord {
            record: batch.records[0].clone(),
            proof: Some(batch.proof(0)),
            also: Vec::new(),
        };
        let checked = check_answer(vec![unassigned, item(&SiteRecord::new("other.com"))], now)
            .expect("the answer is kept");
        assert_eq!(checked.len(), 2);
        assert!(!checked[0].verified);

        // As a second proof beside a good one, it is just not counted.
        let good = crawls_by(
            std::slice::from_ref(&Keypair::generate_ed25519()),
            &["Bank"],
            now,
        );
        let mixed = BucketRecord {
            record: good[0].records[0].clone(),
            proof: Some(good[0].proof(0)),
            also: vec![batch.proof(0)],
        };
        let checked = check_answer(vec![mixed], now).unwrap();
        assert!(checked[0].verified && checked[0].crawlers.len() == 1);
    }

    #[test]
    fn retained_stale_results_and_empty_answers_are_searchable_offline() {
        let now = 1_790_000_000;
        let dir = tempfile::tempdir().unwrap();
        let cache = BucketCache::open(dir.path(), now);
        let record = SiteRecord::new("acme.com");
        cache.put(bucket_of("acme"), vec![vec![item(&record)]], now);
        let stale = cache_search("acme", &cache, now + crate::cache::FRESH_FOR);
        assert_eq!(stale.found.len(), 1);
        assert_eq!(stale.cached, 1);
        assert_eq!(stale.stale, 1);
        assert_eq!(stale.pending, 0);
        assert_eq!(stale.asked, 0);
        assert_eq!(stale.buckets, 0);
        cache.put(bucket_of("absent"), vec![vec![]], now);
        let negative = cache_search("absent", &cache, now);
        assert_eq!(negative.cached, 1);
        assert_eq!(negative.pending, 0);
        assert_eq!(negative.stale, 0);
        assert!(negative.found.is_empty());
        assert!(cache_search_with_refresh("absent", &cache, now)
            .1
            .is_empty());
    }

    #[tokio::test]
    async fn fresh_negative_cache_does_not_launch_a_legacy_round() {
        let now = 1_790_000_000;
        let dir = tempfile::tempdir().unwrap();
        let cache = BucketCache::open(dir.path(), now);
        cache.put(bucket_of("absent"), vec![vec![]], now);
        let out = search(
            "absent",
            &[],
            Duration::from_secs(1),
            now,
            None,
            Some(&cache),
        )
        .await;
        assert_eq!(out.cached, 1);
        assert_eq!(out.buckets, 0); // A round would have four buckets even with no peers.
        assert_eq!(out.asked, 0);
    }

    #[test]
    fn cache_read_rejects_forged_answers_and_rechecks_expired_proofs() {
        let now = 1_790_000_000;
        let dir = tempfile::tempdir().unwrap();
        let cache = BucketCache::open(dir.path(), now);
        let batch = crawls_by(&[Keypair::generate_ed25519()], &["Real Bank"], now).remove(0);
        let honest = BucketRecord {
            record: batch.records[0].clone(),
            proof: Some(batch.proof(0)),
            also: Vec::new(),
        };
        let mut forged = honest.clone();
        forged.proof.as_mut().unwrap().record.push(' ');
        cache.put(bucket_of("bank"), vec![vec![forged]], now);
        let rejected = cache_search("bank", &cache, now);
        assert_eq!(rejected.pending, 1);
        assert_eq!(rejected.cached, 0);
        assert_eq!(rejected.rejected, 1);
        assert!(rejected.found.is_empty());
        cache.put(bucket_of("bank"), vec![vec![honest]], now);
        assert!(cache_search("bank", &cache, now).found[0].verified);
        let old = cache_search("bank", &cache, now + 10 * EPOCH_SECS);
        assert_eq!(old.stale, 1);
        assert!(!old.found[0].verified);
        assert!(!old.found[0].confirmed);
    }

    #[test]
    fn cache_search_keeps_the_existing_four_unique_bucket_key_policy() {
        let now = 1_790_000_000;
        let dir = tempfile::tempdir().unwrap();
        let cache = BucketCache::open(dir.path(), now);
        let query = "one two three four five six seven eight nine ten";
        let (_, keys) = search_buckets(query);
        let mut expected = Vec::new();
        for key in keys {
            let bucket = bucket_of(&key);
            if !expected.contains(&bucket) {
                expected.push(bucket);
            }
        }
        let (result, queued) = cache_search_with_refresh(query, &cache, now);
        assert_eq!(queued, expected);
        assert_eq!(result.pending, crate::bucket::BUCKETS_PER_SEARCH);
    }
}
