//! Serving what trusted and other nodes fetch: fills, page files,
//! signed sets and profiles, a few at a time.

use libp2p::request_response::{self};
use libp2p::PeerId;
use plumb_core::now_unix;
use tracing::debug;

use super::{Answer, Task, MAX_PROFILE_SERVING};
use crate::fill::{FillPage, FILL_REQUESTS_PER_MINUTE, MAX_FILLING};
use crate::pages::{PagesChunk, MAX_SERVING, PAGES_REQUESTS_PER_MINUTE};
use crate::proto::*;
use crate::sets::{SetsAnswer, MAX_SETS_SERVING, SETS_REQUESTS_PER_MINUTE};

impl Task {
    pub(super) fn on_fill_event(
        &mut self,
        event: request_response::Event<FillRequest, FillResponse>,
    ) {
        match event {
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => {
                if !self.admit_fill(peer) {
                    let _ = self.swarm.behaviour_mut().fill.send_response(
                        channel,
                        FillResponse {
                            records: Vec::new(),
                            next: request.from,
                            total: 0,
                            busy: true,
                        },
                    );
                    return;
                }
                debug!("filling {peer} from {}", request.from);
                self.filling += 1;
                let source = self.source.clone();
                let tx = self.answers_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let response =
                        crate::fill::answer(&*source, request.from, request.count, request.all);
                    let _ = tx.send(Answer::Fill(channel, response));
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
            } => {
                let Some((all, reply)) = self.fill_asking.remove(&request_id) else {
                    return;
                };
                let now = now_unix();
                let bytes = response.records.iter().map(|r| r.len() as u64).sum();
                let accept = if all {
                    crate::fill::accept_seed
                } else {
                    crate::fill::accept_filled
                };
                let sites = response
                    .records
                    .iter()
                    .filter_map(|line| accept(line, now))
                    .collect();
                let _ = reply.send(Ok(Some(FillPage {
                    peer,
                    sites,
                    next: response.next,
                    total: response.total,
                    busy: response.busy,
                    bytes,
                })));
            }
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                if let Some((_, reply)) = self.fill_asking.remove(&request_id) {
                    let _ = reply.send(Err(anyhow::anyhow!("{peer} did not answer: {error}")));
                }
            }
            _ => {}
        }
    }

    /// Whether to answer a fill request from `peer` now: this node serves
    /// its index to others, is not filling [`MAX_FILLING`] nodes already,
    /// and `peer` asked fewer than [`FILL_REQUESTS_PER_MINUTE`] times this
    /// minute.
    fn admit_fill(&mut self, peer: PeerId) -> bool {
        if !self.config.answer_searches || self.filling >= MAX_FILLING {
            return false;
        }
        let minute = now_unix() / 60;
        let asked = self.fill_asked.entry(peer).or_insert((minute, 0));
        if asked.0 != minute {
            *asked = (minute, 0);
        }
        if asked.1 >= FILL_REQUESTS_PER_MINUTE {
            return false;
        }
        asked.1 += 1;
        true
    }

    pub(super) fn on_pages_event(
        &mut self,
        event: request_response::Event<PagesRequest, PagesResponse>,
    ) {
        match event {
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => {
                if !self.admit_pages(peer) {
                    let _ = self.swarm.behaviour_mut().pages.send_response(
                        channel,
                        PagesResponse {
                            size: 0,
                            modified: 0,
                            bytes: serde_bytes::ByteBuf::new(),
                            busy: true,
                            layers: None,
                            quality: None,
                        },
                    );
                    return;
                }
                self.pages_serving += 1;
                let source = self.source.clone();
                let tx = self.answers_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let response = source.page_chunk(&request);
                    let _ = tx.send(Answer::Pages(channel, response));
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
            } => {
                let Some(reply) = self.pages_asking.remove(&request_id) else {
                    return;
                };
                let _ = reply.send(Ok(Some(PagesChunk {
                    peer,
                    size: response.size,
                    modified: response.modified,
                    bytes: response.bytes.into_vec(),
                    busy: response.busy,
                    layers: response.layers,
                    quality: response.quality,
                })));
            }
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                if let Some(reply) = self.pages_asking.remove(&request_id) {
                    let _ = reply.send(Err(anyhow::anyhow!("{peer} did not answer: {error}")));
                }
            }
            _ => {}
        }
    }

    pub(super) fn on_sets_event(
        &mut self,
        event: request_response::Event<SetsRequest, SetsResponse>,
    ) {
        match event {
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => {
                if !self.admit_sets(peer) {
                    let _ = self
                        .swarm
                        .behaviour_mut()
                        .sets
                        .send_response(channel, SetsResponse::Busy);
                    return;
                }
                self.sets_serving += 1;
                let source = self.source.clone();
                let tx = self.answers_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let response = source.sets_answer(&request);
                    let _ = tx.send(Answer::Sets(channel, response));
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
            } => {
                if let Some(reply) = self.sets_asking.remove(&request_id) {
                    let _ = reply.send(Ok(Some(SetsAnswer { peer, response })));
                }
            }
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                if let Some(reply) = self.sets_asking.remove(&request_id) {
                    let _ = reply.send(Err(anyhow::anyhow!("{peer} did not answer: {error}")));
                }
            }
            _ => {}
        }
    }

    /// Whether to answer a signed set request from `peer`: as for page
    /// sets ([`Self::admit_pages`]), with counts of their own.
    fn admit_sets(&mut self, peer: PeerId) -> bool {
        if !self.config.answer_searches || self.sets_serving >= MAX_SETS_SERVING {
            return false;
        }
        let minute = now_unix() / 60;
        let asked = self.sets_asked.entry(peer).or_insert((minute, 0));
        if asked.0 != minute {
            *asked = (minute, 0);
        }
        if asked.1 >= SETS_REQUESTS_PER_MINUTE {
            return false;
        }
        asked.1 += 1;
        true
    }

    /// Profile requests are answered by the node's front end (see
    /// [`BucketSource::profile`](crate::bucket::BucketSource::profile)), which
    /// answers only for profiles linked with the asker; a few at a time.
    pub(super) fn on_profile_event(
        &mut self,
        event: request_response::Event<ProfileRequest, ProfileResponse>,
    ) {
        match event {
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => {
                if self.profile_serving >= MAX_PROFILE_SERVING {
                    let _ = self.swarm.behaviour_mut().profile.send_response(
                        channel,
                        ProfileResponse::Refused("busy, ask again later".into()),
                    );
                    return;
                }
                self.profile_serving += 1;
                let source = self.source.clone();
                let tx = self.answers_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let response = source.profile(peer, request);
                    let _ = tx.send(Answer::Profile(channel, response));
                });
            }
            request_response::Event::Message {
                message:
                    request_response::Message::Response {
                        request_id,
                        response,
                    },
                ..
            } => {
                if let Some(reply) = self.profile_asking.remove(&request_id) {
                    let _ = reply.send(Ok(response));
                }
            }
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                if let Some(reply) = self.profile_asking.remove(&request_id) {
                    let _ = reply.send(Err(anyhow::anyhow!("{peer} did not answer: {error}")));
                }
            }
            _ => {}
        }
    }

    /// Whether to answer a page set request from `peer` now: this node
    /// serves its index to others, is not answering [`MAX_SERVING`] already,
    /// and `peer` asked fewer than [`PAGES_REQUESTS_PER_MINUTE`] times this
    /// minute.
    fn admit_pages(&mut self, peer: PeerId) -> bool {
        if !self.config.answer_searches || self.pages_serving >= MAX_SERVING {
            return false;
        }
        let minute = now_unix() / 60;
        let asked = self.pages_asked.entry(peer).or_insert((minute, 0));
        if asked.0 != minute {
            *asked = (minute, 0);
        }
        if asked.1 >= PAGES_REQUESTS_PER_MINUTE {
            return false;
        }
        asked.1 += 1;
        true
    }
}
