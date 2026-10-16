//! Answering other nodes' searches: buckets, private (oblivious) search
//! and relayed requests, and who may search this node.

use std::sync::Mutex;

use libp2p::request_response::{self, OutboundRequestId};
use libp2p::PeerId;
use plumb_core::locks::Locked;
use plumb_core::now_unix;
use tracing::{debug, warn};

use super::{Answer, Asker, Relayed, Reply, Task, MAX_RELAYING};
use crate::bucket::{BucketSource, BUCKETS};
use crate::hash::Hash;
use crate::oblivious::{
    seal_response, ObliviousRequest, ObliviousResponse, Opened, MAX_MESSAGE, RELAY_KEY_CACHE,
};
use crate::proto::*;
use crate::scope::{SearchScope, MAX_SHARED_TRUST};
use crate::store::BatchStore;

/// Bucket `bucket` of `source`, each site as its item, with the proof of
/// its signed crawl when `store` holds one that has not expired by `now`.
pub(super) fn lookup(
    source: &dyn BucketSource,
    store: &Mutex<BatchStore>,
    bucket: u32,
    now: u64,
) -> BucketResponse {
    let records = source.bucket(bucket).map(|lines| {
        let sites: Vec<(String, String)> = lines
            .iter()
            .filter_map(|line| {
                let record = plumb_core::stored::read(line.as_bytes()).ok()?;
                Some((record.to_line().ok()?, record.domain))
            })
            .collect();
        // Where each site's proofs are is in memory; the batches are read
        // after the lock is let go, so gossip and fetches aren't held up.
        let (sources, mut reader) = {
            let store = store.locked();
            let sources: Vec<Vec<(Hash, usize)>> = sites
                .iter()
                .map(|(_, domain)| store.proof_sources(domain, now))
                .collect();
            (sources, store.reader())
        };
        sites
            .into_iter()
            .zip(sources)
            .map(|((record, _), sources)| {
                let mut proofs = reader
                    .proofs(&sources, 1 + MAX_EXTRA_PROOFS)
                    .unwrap_or_default()
                    .into_iter();
                let proof = proofs.next();
                BucketRecord {
                    record,
                    proof,
                    also: proofs.collect(),
                }
            })
            .collect()
    });
    BucketResponse {
        records,
        busy: false,
    }
}

/// A relayed address whose relay is `relay`.
/// The relay a circuit address goes through, if it is one.
/// True when the lines of a batch name a site, homepage, key page or
/// headline the node's denylist takes off ([`BucketSource::denies`]): a
/// record, an icon or a site's headlines.
pub(super) fn holds_denied(source: &dyn BucketSource, lines: &[String]) -> bool {
    if !source.denying() {
        return false;
    }
    lines.iter().any(|line| {
        let Ok(line) = serde_json::from_str::<serde_json::Value>(line) else {
            return false;
        };
        let text = |value: &serde_json::Value, key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        let list = |key: &str| {
            let list = line.get(key).and_then(serde_json::Value::as_array);
            list.into_iter().flatten()
        };
        // A site's item names it by its id, and its key pages by edges.
        let sites = ["id", "icon_of", "news_of"]
            .iter()
            .filter_map(|key| text(&line, key))
            .map(|domain| format!("https://{domain}/"));
        let key_pages = list("edges")
            .filter_map(|edge| Some(text(edge, "to")?.strip_prefix("url:")?.to_string()));
        let headlines = list("items").filter_map(|headline| text(headline, "url"));
        let mut urls = sites
            .chain(text(&line, "url"))
            .chain(key_pages)
            .chain(headlines);
        urls.any(|url| source.denies(&url))
    })
}

impl Task {
    pub(super) fn on_bucket_event(
        &mut self,
        event: request_response::Event<BucketRequest, BucketResponse>,
    ) {
        let request_response::Event::Message {
            peer,
            message:
                request_response::Message::Request {
                    request, channel, ..
                },
            ..
        } = event
        else {
            return;
        };
        let asker = self.source_of(&peer);
        let busy = !self.admit(&request, asker);
        if busy || request.bucket >= BUCKETS {
            let _ = self.swarm.behaviour_mut().buckets.send_response(
                channel,
                BucketResponse {
                    records: None,
                    busy,
                },
            );
            return;
        }
        debug!("serving bucket {} to {peer}", request.bucket);
        self.answering += 1;
        let source = self.source.clone();
        let store = self.store.clone();
        let tx = self.answers_tx.clone();
        tokio::task::spawn_blocking(move || {
            let response = lookup(&*source, &store, request.bucket, now_unix());
            let _ = tx.send(Answer::Bucket(channel, response));
        });
    }

    pub(super) fn on_oblivious_event(
        &mut self,
        event: request_response::Event<ObliviousRequest, ObliviousResponse>,
    ) {
        match event {
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => self.on_oblivious_request(request, Reply::Remote(channel), Some(peer)),
            request_response::Event::Message {
                message:
                    request_response::Message::Response {
                        request_id,
                        response,
                    },
                ..
            } => self.on_relayed(request_id, Some(response)),
            request_response::Event::OutboundFailure {
                request_id, error, ..
            } => {
                debug!("passing on a sealed request failed: {error}");
                self.on_relayed(request_id, None);
            }
            _ => {}
        }
    }

    /// A request on `/plumb/oblivious/2`, from another node or our own
    /// handle (see [`crate::oblivious`]).
    pub(super) fn on_oblivious_request(
        &mut self,
        request: ObliviousRequest,
        reply: Reply,
        from: Option<PeerId>,
    ) {
        let me = *self.swarm.local_peer_id();
        match request {
            ObliviousRequest::OwnKeys => {
                let keys = self.gateway.keys();
                self.reply(reply, ObliviousResponse::Keys(Some(keys)));
            }
            ObliviousRequest::Keys { target } if target == me => {
                let keys = self.gateway.keys();
                self.reply(reply, ObliviousResponse::Keys(Some(keys)));
            }
            ObliviousRequest::Keys { target } => {
                let now = now_unix();
                if let Some((keys, fetched)) = self.relay_keys.get(&target) {
                    if *fetched + RELAY_KEY_CACHE > now && keys.expires > now {
                        let keys = keys.clone();
                        self.reply(reply, ObliviousResponse::Keys(Some(keys)));
                        return;
                    }
                }
                if let Some(waiting) = self.waiting_keys.get_mut(&target) {
                    waiting.push(reply);
                    return;
                }
                if self.relaying.len() >= MAX_RELAYING {
                    self.reply(reply, ObliviousResponse::Keys(None));
                    return;
                }
                self.add_known_addresses(&target);
                let id = self
                    .swarm
                    .behaviour_mut()
                    .oblivious
                    .send_request(&target, ObliviousRequest::OwnKeys);
                self.relaying.insert(id, Relayed::Keys(target));
                self.waiting_keys.insert(target, vec![reply]);
            }
            ObliviousRequest::Deliver { message } => {
                // Passed on by a relay for whoever sealed it. One we do not
                // know as a relay may be a throwaway identity posing as
                // one: it goes by its address.
                let asker = match (&reply, from) {
                    (Reply::Local(_), _) | (_, None) => Asker::Own,
                    (Reply::Remote(_), Some(relay)) if self.oblivious_peers.contains(&relay) => {
                        Asker::Relay(relay)
                    }
                    (Reply::Remote(_), Some(peer)) => self.source_of(&peer),
                };
                self.open_sealed(message.into_vec(), reply, asker);
            }
            ObliviousRequest::Forward { target, message } if target == me => {
                // Sealed, but handed to us by the one who sealed it.
                let asker = match (&reply, from) {
                    (Reply::Local(_), _) | (_, None) => Asker::Own,
                    (Reply::Remote(_), Some(peer)) => self.source_of(&peer),
                };
                self.open_sealed(message.into_vec(), reply, asker);
            }
            ObliviousRequest::Forward { target, message } => {
                if self.relaying.len() >= MAX_RELAYING || message.len() > MAX_MESSAGE {
                    self.reply(reply, ObliviousResponse::Sealed(None));
                    return;
                }
                self.add_known_addresses(&target);
                let id = self
                    .swarm
                    .behaviour_mut()
                    .oblivious
                    .send_request(&target, ObliviousRequest::Deliver { message });
                self.relaying.insert(id, Relayed::Forward(reply));
                self.with_status(|s| s.requests_relayed += 1);
            }
        }
    }

    /// As the target: opens a sealed bucket request or report, and answers
    /// it sealed.
    fn open_sealed(&mut self, message: Vec<u8>, reply: Reply, asker: Asker) {
        let (request, sealer) = match self.gateway.open(&message) {
            Ok((Opened::Bucket(request), sealer)) => (request, sealer),
            Ok((Opened::Report(report), sealer)) => {
                let taken = self.take_submitted(&report);
                let sealed = match seal_response(sealer, &ReportResponse::Taken(taken)) {
                    Ok(sealed) => Some(serde_bytes::ByteBuf::from(sealed)),
                    Err(err) => {
                        warn!("cannot seal an answer: {err:#}");
                        None
                    }
                };
                self.reply(reply, ObliviousResponse::Sealed(sealed));
                return;
            }
            Err(err) => {
                debug!("a sealed request we cannot open: {err:#}");
                self.reply(reply, ObliviousResponse::Sealed(None));
                return;
            }
        };
        if !self.config.answer_searches || request.bucket >= BUCKETS {
            self.reply(reply, ObliviousResponse::Sealed(None));
            return;
        }
        if !self.admit(&request, asker) {
            let busy = BucketResponse {
                records: None,
                busy: true,
            };
            let sealed = seal_response(sealer, &busy)
                .ok()
                .map(serde_bytes::ByteBuf::from);
            self.reply(reply, ObliviousResponse::Sealed(sealed));
            return;
        }
        debug!("serving a sealed request for bucket {}", request.bucket);
        self.answering += 1;
        let source = self.source.clone();
        let store = self.store.clone();
        let tx = self.answers_tx.clone();
        tokio::task::spawn_blocking(move || {
            let response = lookup(&*source, &store, request.bucket, now_unix());
            let sealed = match seal_response(sealer, &response) {
                Ok(sealed) => Some(serde_bytes::ByteBuf::from(sealed)),
                Err(err) => {
                    warn!("cannot seal an answer: {err:#}");
                    None
                }
            };
            let _ = tx.send(Answer::Sealed(reply, ObliviousResponse::Sealed(sealed)));
        });
    }

    /// As a relay: the answer to a request we passed on.
    fn on_relayed(&mut self, id: OutboundRequestId, response: Option<ObliviousResponse>) {
        match self.relaying.remove(&id) {
            Some(Relayed::Keys(target)) => {
                let keys = match response {
                    Some(ObliviousResponse::Keys(Some(keys)))
                        if keys.verify(&target, now_unix()).is_ok() =>
                    {
                        self.relay_keys.insert(target, (keys.clone(), now_unix()));
                        Some(keys)
                    }
                    _ => None,
                };
                for reply in self.waiting_keys.remove(&target).unwrap_or_default() {
                    self.reply(reply, ObliviousResponse::Keys(keys.clone()));
                }
            }
            Some(Relayed::Forward(reply)) => {
                let answer = match response {
                    Some(ObliviousResponse::Sealed(answer)) => answer,
                    _ => None,
                };
                self.reply(reply, ObliviousResponse::Sealed(answer));
            }
            None => {}
        }
    }

    pub(super) fn reply(&mut self, reply: Reply, response: ObliviousResponse) {
        match reply {
            Reply::Remote(channel) => {
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .oblivious
                    .send_response(channel, response);
            }
            Reply::Local(tx) => {
                let _ = tx.send(response);
            }
        }
    }

    /// Whether this node's searches may ask `peer`, under
    /// [`NetConfig::search_scope`](super::NetConfig::search_scope).
    pub(super) fn may_search(&self, peer: &PeerId) -> bool {
        self.friends.allows(
            self.config.search_scope,
            self.swarm.local_peer_id(),
            &self.config.trusted_peers,
            peer,
        )
    }

    /// Any node may ask which nodes this one trusts: node ids are public
    /// keys, and the list says no more than the crawls this node takes in.
    /// Only the answers of nodes this node trusts are kept.
    pub(super) fn on_trust_event(
        &mut self,
        event: request_response::Event<TrustRequest, TrustResponse>,
    ) {
        match event {
            request_response::Event::Message {
                message: request_response::Message::Request { channel, .. },
                ..
            } => {
                let trusted = self
                    .config
                    .trusted_peers
                    .iter()
                    .filter(|peer| *peer != self.swarm.local_peer_id())
                    .take(MAX_SHARED_TRUST)
                    .map(ToString::to_string)
                    .collect();
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .trust
                    .send_response(channel, TrustResponse { trusted });
            }
            request_response::Event::Message {
                peer,
                message: request_response::Message::Response { response, .. },
                ..
            } => {
                if !self.config.trusted_peers.contains(&peer)
                    || !self.friends.set(peer, &response.trusted)
                {
                    return;
                }
                debug!("{peer} trusts {} nodes", response.trusted.len());
                if let Err(err) = self.friends.save(&self.config.trusted_peers) {
                    warn!("{err:#}");
                }
                if self.config.search_scope == SearchScope::FriendsOfFriends {
                    // Friends of friends are asked only once connected.
                    let me = *self.swarm.local_peer_id();
                    for friend in self.friends.of(&me, &self.config.trusted_peers) {
                        if !self.swarm.is_connected(&friend) {
                            let _ = self.swarm.dial(friend);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
