//! Credits and tokens: who may ask what of this node, and the tokens it
//! collects to ask of others.

use anyhow::Result;
use libp2p::multiaddr::Protocol;
use libp2p::request_response::{self, OutboundRequestId};
use libp2p::PeerId;
use plumb_core::locks::Locked;
use tracing::{debug, warn};

use super::{
    now_millis, Asker, Asking, CreditsAt, Task, PRIORITY_SLOTS, TOKEN_ASK_MINUTES, TOKEN_LOW,
    TOKEN_REFILL,
};
use crate::agree::MIN_JUDGED;
use crate::allowance::Source;
use crate::credits::{Pending, MIN_ANSWERS_FOR_TOKENS};
use crate::leads::Relation;
use crate::proto::*;
use crate::scope::SearchScope;

fn refusal(peer: PeerId, answer: Result<CreditResponse>) -> anyhow::Error {
    match answer {
        Ok(CreditResponse::Refused(why)) => anyhow::anyhow!("{peer} refused: {why}"),
        Ok(_) => anyhow::anyhow!("{peer} answered something else"),
        Err(err) => err.context(format!("asking {peer}")),
    }
}

impl Task {
    /// Whether to answer a bucket request now. Requests from this node's
    /// own front end get in while fewer than `max_answering` plus
    /// [`PRIORITY_SLOTS`] are being answered. Anyone else's get in for free
    /// while fewer than `max_answering` are, and the address or relay it
    /// came from has free answers left (see [`crate::allowance`]); past
    /// that, up to [`PRIORITY_SLOTS`] more, for a token of ours. A token
    /// sent along is spent either way: it is only sent after this node said
    /// it was busy.
    pub(super) fn admit(&mut self, request: &BucketRequest, asker: Asker) -> bool {
        let paid = request
            .token
            .as_ref()
            .is_some_and(|token| self.issuer.redeem(token));
        let ceiling = self.config.max_answering + PRIORITY_SLOTS;
        let free = match asker {
            Asker::Own => return self.answering < ceiling,
            // A relay this node trusts may pass on as much as it likes,
            // within the day's limit.
            Asker::Relay(relay) if self.config.trusted_peers.contains(&relay) => None,
            // By its address: anyone can make keys and say they relay.
            Asker::Relay(relay) => match self.source_of(&relay) {
                Asker::From(Some(source)) => Some(source),
                _ => Some(Source::Peer(relay)),
            },
            Asker::From(source) => source,
        };
        if self.answering < self.config.max_answering
            && self.allowance.take(free.as_ref(), now_millis())
        {
            return true;
        }
        if paid && self.answering < ceiling {
            self.priority_answered += 1;
            return true;
        }
        self.allowance.turn_away();
        false
    }

    /// Where a request from `peer`, a throwaway identity most likely, comes
    /// from: its IP address (see [`Source::ip`]), or when it came through a
    /// relay circuit, that relay.
    pub(super) fn source_of(&self, peer: &PeerId) -> Asker {
        let ip = self.remote_addrs.get(peer).and_then(|addr| {
            addr.iter().find_map(|p| match p {
                Protocol::Ip4(ip) => Some(std::net::IpAddr::V4(ip)),
                Protocol::Ip6(ip) => Some(std::net::IpAddr::V6(ip)),
                _ => None,
            })
        });
        let through = || self.circuits.get(peer).map(|relay| Source::Peer(*relay));
        Asker::From(ip.map(Source::ip).or_else(through))
    }

    /// Asks the nodes this node searches for tokens, when it holds few of
    /// theirs, at most every [`TOKEN_ASK_MINUTES`] each. Nodes whose
    /// ledger has no credits for us say no, and we ask again later.
    pub(super) fn collect_tokens(&mut self, now: u64) {
        let peers: Vec<PeerId> = self.bucket_peers.keys().copied().collect();
        self.ask_balances(&peers, now);
        if !self.config.collect_tokens {
            return;
        }
        for peer in peers {
            let held = self.wallet.locked().held(&peer);
            let recent = self
                .token_asks
                .get(&peer)
                .is_some_and(|at| at + TOKEN_ASK_MINUTES * 60 > now);
            if held >= TOKEN_LOW || recent || !self.swarm.is_connected(&peer) {
                continue;
            }
            let Ok(pending) = Pending::new(TOKEN_REFILL) else {
                continue;
            };
            self.token_asks.insert(peer, now);
            let request = CreditRequest::Issue {
                blinded: pending.blinded.clone(),
            };
            let id = self
                .swarm
                .behaviour_mut()
                .credits
                .send_request(&peer, request);
            self.asking.insert(id, Asking::Tokens(peer, pending, None));
        }
    }

    /// Asks the connected nodes among `peers` for our balance there, at
    /// most every [`TOKEN_ASK_MINUTES`] each, for the status page.
    fn ask_balances(&mut self, peers: &[PeerId], now: u64) {
        self.credits_at
            .retain(|peer, _| self.bucket_peers.contains_key(peer));
        // Asks too old to hold anything back are forgotten.
        let recent = |at: &mut u64| *at + TOKEN_ASK_MINUTES * 60 > now;
        self.balance_asks.retain(|_, at| recent(at));
        self.token_asks.retain(|_, at| recent(at));
        for &peer in peers {
            let recent = self
                .balance_asks
                .get(&peer)
                .is_some_and(|at| at + TOKEN_ASK_MINUTES * 60 > now);
            if recent || !self.swarm.is_connected(&peer) {
                continue;
            }
            self.balance_asks.insert(peer, now);
            let id = self
                .swarm
                .behaviour_mut()
                .credits
                .send_request(&peer, CreditRequest::Balance);
            self.asking.insert(id, Asking::Balance(peer));
        }
    }

    /// Credits the crawls agreement just scored.
    pub(super) fn count_credits(&mut self) {
        let verdicts = self.agreement.take_verdicts();
        self.ledger.record(&verdicts);
    }

    /// Whether `crawler` can have tokens here: this node trusts it strictly
    /// ([`Agreement::vouched`](crate::agree::Agreement::vouched), whatever
    /// the quorum rules), and it was judged often enough to have a track
    /// record.
    fn crawls_count(&self, crawler: &PeerId) -> bool {
        let score = self.agreement.score(crawler);
        self.agreement.vouched(crawler) && score.agreed + score.disagreed >= MIN_JUDGED
    }

    /// Whether `peer` can have tokens for its credits here: its crawls
    /// count, this node trusts it, or it answered enough of our requests
    /// for that work to be real.
    fn may_have_tokens(&self, peer: &PeerId) -> bool {
        self.crawls_count(peer)
            || self.config.trusted_peers.contains(peer)
            || self.ledger.account(peer).answered >= MIN_ANSWERS_FOR_TOKENS
    }

    /// How this node stands to `peer`, for the leads it reported.
    pub(super) fn relation(&self, peer: &PeerId) -> Relation {
        let me = self.swarm.local_peer_id();
        if self.config.trusted_peers.contains(peer) {
            Relation::Trusted
        } else if self.friends.allows(
            SearchScope::FriendsOfFriends,
            me,
            &self.config.trusted_peers,
            peer,
        ) {
            Relation::FriendOfFriend
        } else {
            Relation::Other
        }
    }

    pub(super) fn on_credit_event(
        &mut self,
        event: request_response::Event<CreditRequest, CreditResponse>,
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
                let response = self.answer_credits(peer, request);
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .credits
                    .send_response(channel, response);
            }
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Response {
                        request_id,
                        response,
                    },
                ..
            } => self.on_credit_answer(peer, request_id, Ok(response)),
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => self.on_credit_answer(peer, request_id, Err(anyhow::anyhow!("{error}"))),
            _ => {}
        }
    }

    /// As the issuer: tokens for `peer`, paid from its credits here.
    fn answer_credits(&mut self, peer: PeerId, request: CreditRequest) -> CreditResponse {
        let counts = self.may_have_tokens(&peer);
        match request {
            CreditRequest::Balance => CreditResponse::Balance {
                credits: self.ledger.account(&peer).balance(),
                counts,
            },
            CreditRequest::Issue { blinded } => {
                let n = self.ledger.issuable(&peer, counts, blinded.len());
                if n == 0 {
                    let why = if counts {
                        "no credits left here"
                    } else {
                        "your work does not count here yet"
                    };
                    return CreditResponse::Refused(why.into());
                }
                match self.issuer.issue(&blinded[..n]) {
                    Ok(issued) => {
                        self.ledger.charge(&peer, n);
                        self.tokens_issued += n as u64;
                        debug!("issued {n} tokens to {peer}");
                        CreditResponse::Issued(issued)
                    }
                    Err(err) => CreditResponse::Refused(format!("{err:#}")),
                }
            }
        }
    }

    /// As the asker: what an issuer answered.
    fn on_credit_answer(
        &mut self,
        peer: PeerId,
        id: OutboundRequestId,
        answer: Result<CreditResponse>,
    ) {
        match (self.asking.remove(&id), answer) {
            (Some(Asking::Tokens(issuer, pending, reply)), Ok(CreditResponse::Issued(issued))) => {
                let kept = pending.finish(&issued).and_then(|tokens| {
                    let n = tokens.len();
                    self.wallet
                        .locked()
                        .add(&issuer, &issued.key, tokens)
                        .map(|()| n)
                });
                match reply {
                    Some(reply) => {
                        let _ = reply.send(kept);
                    }
                    None => match kept {
                        Ok(n) => debug!("got {n} tokens from {issuer}"),
                        Err(err) => warn!("tokens from {issuer} did not check out: {err:#}"),
                    },
                }
            }
            (Some(Asking::Credits(reply)), Ok(CreditResponse::Balance { credits, counts })) => {
                let _ = reply.send(Ok(CreditsAt { credits, counts }));
            }
            (Some(Asking::Balance(at)), Ok(CreditResponse::Balance { credits, counts })) => {
                self.credits_at.insert(at, CreditsAt { credits, counts });
            }
            (Some(Asking::Balance(at)), answer) => {
                debug!("no balance from {at}: {:#}", refusal(peer, answer));
            }
            (Some(Asking::Tokens(_, _, reply)), answer) => {
                let err = refusal(peer, answer);
                match reply {
                    Some(reply) => {
                        let _ = reply.send(Err(err));
                    }
                    None => debug!("no tokens: {err:#}"),
                }
            }
            (Some(Asking::Credits(reply)), answer) => {
                let _ = reply.send(Err(refusal(peer, answer)));
            }
            (None, _) => {}
        }
    }
}
