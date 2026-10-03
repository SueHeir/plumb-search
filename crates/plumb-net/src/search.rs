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
//! domain it names.

use std::collections::HashMap;
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

use crate::batch::MAX_RECORD_BYTES;
use crate::bucket::{matches, search_buckets};
use crate::oblivious::{
    open_response, seal_request, ObliviousRequest, ObliviousResponse, MAX_MESSAGE,
    OBLIVIOUS_PROTOCOL,
};
use crate::proto::{BucketRequest, BucketResponse, BUCKET_PROTOCOL};

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
    /// The sites that match the query, unranked.
    pub found: Vec<FoundSite>,
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
/// random ones), each of up to [`NODES_PER_BUCKET`] nodes under a
/// throwaway identity, and keeps the sites that match the query.
pub async fn search(query: &str, peers: &[BucketPeer], wait: Duration, now: u64) -> NetSearch {
    let (buckets, keys) = search_buckets(query);
    let mut out = NetSearch {
        buckets: buckets.len(),
        ..NetSearch::default()
    };
    if peers.is_empty() {
        return out;
    }
    // Spread the buckets over the nodes so that no node gets two buckets
    // of one search while others get none.
    let mut order: Vec<usize> = (0..peers.len()).collect();
    shuffle(&mut order);
    let mut next = 0;
    let mut requests = Vec::new();
    for &bucket in &buckets {
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
            if through.is_empty() {
                return fetch_bucket(&target, bucket, wait)
                    .await
                    .map(|r| (r, false));
            }
            // A relay that cannot reach the node gets one stand-in, in
            // what is left of the time. Never straight to the node: that
            // would show it who asks.
            let deadline = tokio::time::Instant::now() + wait;
            let mut last = None;
            for relay in &through {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                match fetch_oblivious(relay, &target.peer, bucket, left, now).await {
                    Ok(response) => return Ok((response, true)),
                    Err(err) => {
                        debug!("relay {} for {}: {err:#}", relay.peer, target.peer);
                        last = Some(err);
                    }
                }
            }
            Err(last.unwrap_or_else(|| anyhow::anyhow!("no relay")))
        },
    ))
    .await;

    let mut merged: HashMap<String, FoundSite> = HashMap::new();
    for answer in answers {
        let (response, relayed) = match answer {
            Ok(response) => response,
            Err(err) => {
                debug!("a bucket request failed: {err:#}");
                continue;
            }
        };
        let Some(records) = response.records else {
            continue;
        };
        out.answered += 1;
        if relayed {
            out.relayed += 1;
        }
        let Some(checked) = check_answer(records, now) else {
            out.rejected += 1;
            continue;
        };
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
    let mut found: Vec<FoundSite> = merged.into_values().collect();
    found.sort_by(|a, b| a.record.domain.cmp(&b.record.domain));
    out.found = found;
    out
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
        };
        if let Some(proof) = &item.proof {
            match proof.verify(now) {
                Ok((signed, crawler)) if signed.domain == site.record.domain => {
                    site.record.url = signed.url;
                    site.record.title = signed.title;
                    site.record.description = signed.description;
                    site.record.crawled_at = signed.crawled_at;
                    site.verified = true;
                    site.crawler = Some(crawler.to_string());
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
    if other.verified && !existing.verified {
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
                .set_request_size_maximum(4 * 1024)
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
    let mut swarm = throwaway(wait)?;
    for addr in &peer.addrs {
        swarm.add_peer_address(peer.peer, addr.clone());
    }
    swarm
        .behaviour_mut()
        .buckets
        .send_request(&peer.peer, BucketRequest { bucket });
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
    let (message, opener) = seal_request(&keys, target, now, &BucketRequest { bucket })?;
    let request = ObliviousRequest::Forward {
        target: *target,
        message: serde_bytes::ByteBuf::from(message),
    };
    match ask(&mut swarm, relay.peer, request, deadline).await? {
        ObliviousResponse::Sealed(Some(answer)) => open_response(opener, &answer),
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
    use crate::assign::{epoch_of, is_assigned, MAX_SHARE_PPM};
    use crate::batch::Batch;
    use crate::proto::BucketRecord;

    fn item(record: &SiteRecord) -> BucketRecord {
        BucketRecord {
            record: serde_json::to_string(record).unwrap(),
            proof: None,
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
        };
        assert!(check_answer(vec![item(&SiteRecord::new("other.com")), forged], now).is_none());

        let honest = BucketRecord {
            record: serde_json::to_string(&shown).unwrap(),
            proof: Some(batch.proof(0)),
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
        };
        let mut merged = site(boosted);
        merge_site(&mut merged, site(honest));
        assert_eq!(merged.record.signals.tranco_rank, Some(900_000));
        assert!(!merged.record.signals.official_site);
        assert_eq!(merged.answers, 2);
    }
}
