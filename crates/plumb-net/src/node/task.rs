//! The node's task: commands from its handles, answers from the work it
//! hands off, swarm events, connections and status.

use std::time::Duration;

use futures::StreamExt;
use libp2p::core::ConnectedPoint;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::SwarmEvent;
use libp2p::{autonat, dcutr, gossipsub, identify, kad, mdns, relay, upnp, Multiaddr, PeerId};
use plumb_core::locks::Locked;
use plumb_core::now_unix;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::batches::{crawler_views, delivery_bytes, queue_confirmed_delivery, RecordDelivery};
use super::swarm::{
    is_circuit_through, is_global, is_loopback, is_specific, redial_bootstrap, relay_of,
    without_p2p,
};
use super::BehaviourEvent;
use super::{
    peek, Answer, Asking, Command, NetStatus, Reply, Serving, Task, ASK_AGAIN_AFTER, MAX_FETCHES,
    MAX_PEER_VIEWS, MAX_RELAYS, MAX_REPORT_LISTS, MAX_WANTED, RECORD_DELIVERIES, RECOUNT_MINUTES,
    RELAY_HOP_PROTOCOL, RELIST_MINUTES, TARGET_PEERS,
};
use crate::assign::epoch_of;
use crate::credits::{CreditStatus, CreditsAtPeer, Pending, Wallet, MAX_ISSUE};
use crate::joining::{explain_dial_error, peer_of, PeerView, Route};
use crate::oblivious::{ObliviousResponse, OBLIVIOUS_PROTOCOL, RELAY_KEY_CACHE};
use crate::popularity::report_epoch;
use crate::proto::*;
use crate::reports::ReportStore;
use crate::search::BucketPeer;
use crate::store::BatchStore;

impl Task {
    pub(super) async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut answers: mpsc::UnboundedReceiver<Answer>,
    ) {
        let mut maintenance = tokio::time::interval(Duration::from_secs(60));
        maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ticks: u64 = 0;
        loop {
            let ask_again_at = self.ask_again_at;
            let batch_retry_at = self.batch_retries.next();
            let list_retry_at = self.list_retries.next();
            let can_fetch = !self.ingest_blocked
                && !self.wanted.is_empty()
                && self.pending_batches.len() < RECORD_DELIVERIES
                && self.fetching.len() + self.pending_batches.len() < MAX_FETCHES
                && self.has_fetchable_batch();
            tokio::select! {
                event = self.swarm.select_next_some() => self.on_event(event),
                command = commands.recv() => match command {
                    Some(Command::Stop) | None => break,
                    Some(command) => self.on_command(command),
                },
                Some(answer) = answers.recv() => self.on_answer(answer),
                () = async {
                    if can_fetch {
                        match self.records.reserve().await {
                            Ok(permit) => drop(permit),
                            Err(_) => std::future::pending::<()>().await,
                        }
                    } else { std::future::pending::<()>().await; }
                } => self.fetch_more(),
                () = async {
                    match batch_retry_at { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending::<()>().await }
                } => {
                    for (id, sources) in self.batch_retries.due(tokio::time::Instant::now()) {
                        if self.wanted.len() < MAX_WANTED { self.wanted.push_back((id, sources)); }
                        else { self.wanted_ids.remove(&id); self.batch_retries.forget(&id); }
                    }
                    self.fetch_more();
                },
                () = async {
                    match list_retry_at { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending::<()>().await }
                } => {
                    for (peer, since) in self.list_retries.due(tokio::time::Instant::now()) {
                        self.ask_batch_list(peer, since);
                    }
                },
                _ = maintenance.tick() => {
                    self.maintain(ticks);
                    ticks += 1;
                }
                () = async {
                    match ask_again_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.ask_again_at = None;
                    let _ = self.swarm.behaviour_mut().kad.bootstrap();
                }
            }
            self.update_status();
        }
        self.replay_stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Err(err) = self.ledger.save() {
            warn!("cannot save the credits ledger: {err:#}");
        }
        info!("network node stopped");
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Publish { sites, reply } => {
                let _ = reply.send(self.publish(sites));
            }
            Command::Rechecks(limit, reply) => {
                let _ = reply.send(self.agreement.rechecks(limit));
            }
            Command::Counting(crawlers, reply) => {
                let counting = crawlers
                    .into_iter()
                    .filter(|c| {
                        c.parse::<PeerId>()
                            .is_ok_and(|peer| self.agreement.counts(&peer))
                    })
                    .collect();
                let _ = reply.send(counting);
            }
            Command::Peers(serving, reply) => {
                let (serving, scoped) = match serving {
                    Serving::Buckets => (&self.bucket_peers, true),
                    Serving::Reports => (&self.report_peers, false),
                };
                let peers = serving
                    .iter()
                    .filter(|(peer, _)| !scoped || self.may_search(peer))
                    .map(|(peer, listening)| {
                        let mut addrs = listening.clone();
                        if let Some(addr) = self.remote_addrs.get(peer) {
                            if !addrs.contains(addr) {
                                addrs.insert(0, addr.clone());
                            }
                        }
                        // A node behind NAT that relays through us: a
                        // throwaway identity reaches it through our own
                        // listening addresses, loopback first. Its other
                        // circuits through us go: they name our public
                        // address, which a container or a router may not
                        // let us reach from inside, and the relay client
                        // dials a relay only once at a time, so one stuck
                        // dial would hold up the ones that work.
                        if self.reserved.contains(peer) {
                            let me = *self.swarm.local_peer_id();
                            addrs.retain(|a| !is_circuit_through(a, &me));
                            let mut local: Vec<Multiaddr> = self
                                .swarm
                                .listeners()
                                .filter(|l| {
                                    is_specific(l) && !l.iter().any(|p| p == Protocol::P2pCircuit)
                                })
                                .map(|l| {
                                    without_p2p(l.clone())
                                        .with(Protocol::P2p(me))
                                        .with(Protocol::P2pCircuit)
                                        .with(Protocol::P2p(*peer))
                                })
                                .collect();
                            local.sort_by_key(|a| is_global(a) || !is_loopback(a));
                            local.dedup();
                            if let Some(first) = local.first().cloned() {
                                // One local route is enough, and keeps the
                                // relay dial from going anywhere else.
                                addrs.retain(|a| !a.iter().any(|p| p == Protocol::P2pCircuit));
                                addrs.insert(0, first);
                            }
                        }
                        BucketPeer {
                            peer: *peer,
                            addrs,
                            oblivious: false,
                        }
                    })
                    .filter(|p| !p.addrs.is_empty())
                    .map(|mut p| {
                        p.oblivious = self.oblivious_peers.contains(&p.peer);
                        p
                    })
                    .collect::<Vec<_>>();
                for p in &peers {
                    debug!("peer {} at {:?}", p.peer, p.addrs);
                }
                let _ = reply.send(peers);
            }
            Command::Recount(reply) => self.count_reports(Some(reply)),
            Command::ShareLead(draft, reply) => {
                let _ = reply.send(self.share_lead(draft));
            }
            Command::Leads { keys, limit, reply } => {
                let me = *self.swarm.local_peer_id();
                let found = self.leads.matching(
                    &keys,
                    &me,
                    self.config.search_scope,
                    |peer| self.relation(peer),
                    now_unix(),
                    limit,
                );
                let _ = reply.send(found);
            }
            Command::Oblivious(request, reply) => {
                self.on_oblivious_request(request, Reply::Local(reply), None);
            }
            Command::AskTokens {
                issuer,
                wanted,
                reply,
            } => {
                let pending = match Pending::new(wanted.clamp(1, MAX_ISSUE)) {
                    Ok(pending) => pending,
                    Err(err) => {
                        let _ = reply.send(Err(err));
                        return;
                    }
                };
                let request = CreditRequest::Issue {
                    blinded: pending.blinded.clone(),
                };
                let id = self
                    .swarm
                    .behaviour_mut()
                    .credits
                    .send_request(&issuer, request);
                self.asking
                    .insert(id, Asking::Tokens(issuer, pending, Some(reply)));
            }
            Command::Answered(peers) => self.ledger.record_answers(&peers),
            Command::AskCredits(peer, reply) => {
                let id = self
                    .swarm
                    .behaviour_mut()
                    .credits
                    .send_request(&peer, CreditRequest::Balance);
                self.asking.insert(id, Asking::Credits(reply));
            }
            Command::FillPeers(reply) => {
                let mut peers: Vec<PeerId> = self.fill_peers.iter().copied().collect();
                peers.sort();
                let _ = reply.send(peers);
            }
            Command::Fill {
                prefer,
                from,
                count,
                all,
                reply,
            } => {
                let peer = prefer
                    .filter(|peer| self.fill_peers.contains(peer))
                    .or_else(|| self.fill_peers.iter().next().copied());
                let Some(peer) = peer else {
                    let _ = reply.send(Ok(None));
                    return;
                };
                let id = self
                    .swarm
                    .behaviour_mut()
                    .fill
                    .send_request(&peer, FillRequest { from, count, all });
                self.fill_asking.insert(id, (all, reply));
            }
            Command::Pages {
                request,
                peer,
                skip,
                reply,
            } => {
                let peer = match peer {
                    Some(peer) => Some(peer).filter(|p| self.pages_peers.contains(p)),
                    None => self.pages_peers.iter().find(|p| !skip.contains(p)).copied(),
                };
                let Some(peer) = peer else {
                    let _ = reply.send(Ok(None));
                    return;
                };
                let id = self
                    .swarm
                    .behaviour_mut()
                    .pages
                    .send_request(&peer, request);
                self.pages_asking.insert(id, reply);
            }
            Command::Sets {
                request,
                peer,
                skip,
                reply,
            } => {
                let peer = match peer {
                    Some(peer) => Some(peer).filter(|p| self.sets_peers.contains(p)),
                    None => {
                        let open = |p: &&PeerId| !skip.contains(p);
                        let trusted = |p: &&PeerId| self.config.trusted_peers.contains(p);
                        let mut peers = self.sets_peers.iter().filter(open);
                        self.sets_peers
                            .iter()
                            .filter(open)
                            .find(trusted)
                            .or_else(|| peers.next())
                            .copied()
                    }
                };
                let Some(peer) = peer else {
                    let _ = reply.send(Ok(None));
                    return;
                };
                let id = self.swarm.behaviour_mut().sets.send_request(&peer, request);
                self.sets_asking.insert(id, reply);
            }
            Command::Profile {
                peer,
                request,
                reply,
            } => {
                if !self.swarm.is_connected(&peer) {
                    // Addresses the swarm lacks may be found by asking.
                    self.swarm.behaviour_mut().kad.get_closest_peers(peer);
                }
                let id = self
                    .swarm
                    .behaviour_mut()
                    .profile
                    .send_request(&peer, request);
                self.profile_asking.insert(id, reply);
            }
            Command::Dial(addr) => self.dial(addr),
            Command::Reconnect => {
                info!("trying the bootstrap nodes again");
                for addr in self.config.bootstrap.clone() {
                    self.dial(addr);
                }
                if self.swarm.connected_peers().next().is_some() {
                    let _ = self.swarm.behaviour_mut().kad.bootstrap();
                }
            }
            Command::Stop => {}
        }
    }

    fn on_answer(&mut self, answer: Answer) {
        match answer {
            Answer::StoredBatch {
                id,
                crawler,
                from,
                made,
                lines,
                accepted,
                news,
                permit,
                saved,
            } => {
                self.pending_batches.remove(&id);
                match saved {
                    Ok((true, reservation)) => {
                        self.batch_retries.forget(&id);
                        self.ingest_blocked = false;
                        let kept = accepted.len();
                        if self.config.follow_crawls {
                            let mut confirmed = self.agreement.observe(crawler, accepted, made);
                            self.count_credits();
                            confirmed.extend(news.into_lines());
                            if let Some(permit) = permit.filter(|_| !confirmed.is_empty()) {
                                queue_confirmed_delivery(
                                    permit,
                                    RecordDelivery {
                                        sites: confirmed,
                                        reservation,
                                        acknowledgment: self.lock_store().delivery_ack(
                                            &id,
                                            self.config.trusted_peers.contains(&crawler),
                                        ),
                                    },
                                    delivery_bytes,
                                );
                            }
                        }
                        let agreement = self.agreement.status();
                        self.with_status(|s| {
                            s.batches_received += 1;
                            s.agreement = agreement;
                        });
                        debug!(
                            "received batch {id}: kept {kept} sites of {lines} records from {crawler}"
                        );
                    }
                    Ok((false, _)) => {
                        self.batch_retries.forget(&id);
                    } // Duplicate: no credits, delivery or quota churn.
                    Err(err) => {
                        self.wanted_ids.insert(id);
                        self.retry_known_batch(id, from, Vec::new());
                        self.ingest_blocked = true;
                        warn!("cannot keep batch {id}: {err:#}");
                    }
                }
                self.fetch_more();
            }
            Answer::Bucket(channel, response) => {
                self.answering = self.answering.saturating_sub(1);
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .buckets
                    .send_response(channel, response);
                self.with_status(|s| s.buckets_served += 1);
            }
            Answer::Batch(channel, response) => {
                self.lists_serving = self.lists_serving.saturating_sub(1);
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .batches
                    .send_response(channel, response);
            }
            Answer::Report(channel, response) => {
                self.lists_serving = self.lists_serving.saturating_sub(1);
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .reports
                    .send_response(channel, response);
            }
            Answer::Fill(channel, response) => {
                self.filling = self.filling.saturating_sub(1);
                let sent = response.records.len() as u64;
                if self
                    .swarm
                    .behaviour_mut()
                    .fill
                    .send_response(channel, response)
                    .is_ok()
                {
                    self.with_status(|s| s.fill_records_served += sent);
                }
            }
            Answer::Pages(channel, response) => {
                self.pages_serving = self.pages_serving.saturating_sub(1);
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .pages
                    .send_response(channel, response);
            }
            Answer::Sets(channel, response) => {
                self.sets_serving = self.sets_serving.saturating_sub(1);
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .sets
                    .send_response(channel, response);
            }
            Answer::Profile(channel, response) => {
                self.profile_serving = self.profile_serving.saturating_sub(1);
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .profile
                    .send_response(channel, response);
            }
            Answer::Popularity(picks) => self.with_status(|s| s.popular_picks = picks),
            Answer::Sealed(reply, response) => {
                self.answering = self.answering.saturating_sub(1);
                if matches!(response, ObliviousResponse::Sealed(Some(_))) {
                    self.with_status(|s| s.buckets_served += 1);
                }
                self.reply(reply, response);
            }
        }
    }

    pub(super) fn dial(&mut self, addr: Multiaddr) {
        if let Err(err) = self.swarm.dial(addr.clone()) {
            warn!("cannot dial {addr}: {err}");
        }
    }

    /// Once a minute: keep enough connections, look for more nodes, retry
    /// announcements and fetches, prune old batches.
    fn maintain(&mut self, ticks: u64) {
        self.ingest_blocked = false; // Retry temporary quota pressure each minute.
        let now = now_unix();
        let crawlers = crawler_views(
            &self.lock_store(),
            *self.swarm.local_peer_id(),
            &self.config.trusted_peers,
            now,
        );
        self.with_status(|s| s.crawlers = crawlers);
        if let Err(err) = self.gateway.rotate(&self.key, now) {
            warn!("cannot make a new key for sealed requests: {err:#}");
        }
        self.relay_keys
            .retain(|_, (keys, fetched)| *fetched + RELAY_KEY_CACHE > now && keys.expires > now);
        let connected = self.swarm.connected_peers().count();
        let bootstrap_connected = self
            .bootstrap_peers
            .iter()
            .any(|peer| self.swarm.is_connected(peer));
        if redial_bootstrap(connected, bootstrap_connected, ticks) {
            for addr in self.config.bootstrap.clone() {
                self.dial(addr);
            }
        }
        if connected < TARGET_PEERS {
            let known: Vec<PeerId> = self
                .swarm
                .behaviour_mut()
                .kad
                .kbuckets()
                .flat_map(|bucket| {
                    bucket
                        .iter()
                        .map(|entry| *entry.node.key.preimage())
                        .collect::<Vec<_>>()
                })
                .collect();
            let unconnected: Vec<PeerId> = known
                .into_iter()
                .filter(|p| !self.swarm.is_connected(p))
                .take(TARGET_PEERS - connected)
                .collect();
            for peer in unconnected {
                let _ = self.swarm.dial(peer);
            }
        }
        if let Err(err) = self.ledger.save() {
            warn!("cannot save the credits ledger: {err:#}");
        }
        self.collect_tokens(now);
        if ticks.is_multiple_of(5) && connected > 0 {
            let _ = self.swarm.behaviour_mut().kad.bootstrap();
        }
        if !self.unannounced.is_empty() && connected > 0 {
            for header in std::mem::take(&mut self.unannounced) {
                self.announce(header);
            }
        }
        if !self.unannounced_leads.is_empty() && connected > 0 {
            for lead in std::mem::take(&mut self.unannounced_leads) {
                self.announce_lead(lead);
            }
        }
        if ticks.is_multiple_of(60) {
            let now = now_unix();
            // Off the swarm task: a store that follows crawls reads each
            // batch it drops, and a day's worth expires at once.
            let store = self.store.clone();
            let days = self.config.keep_batches_days;
            let bytes = self.config.keep_batches_bytes;
            let own = self.key.public().encode_protobuf();
            tokio::task::spawn_blocking(move || {
                let mut store = store.locked();
                store.prune_keeping(now, days);
                if let Some(bytes) = bytes {
                    store.prune_to_bytes(bytes, &own);
                }
            });
            self.agreement.prune(now);
            let agreement = self.agreement.status();
            self.with_status(|s| s.agreement = agreement);
            self.lock_reports().prune(now);
            if let Err(err) = self.leads.prune(now) {
                warn!("cannot drop expired leads: {err:#}");
            }
            // A new week makes last week's count stale.
            self.recount = true;
        }
        if ticks > 0 && ticks.is_multiple_of(RELIST_MINUTES) {
            let since = epoch_of(now).saturating_sub(1);
            let peers: Vec<PeerId> = self.batch_peers.iter().copied().collect();
            for peer in peers {
                self.ask_batch_list(peer, since);
            }
            // Leads whose gossip was missed, as when a node shares one
            // before the gossip mesh has formed.
            self.lead_listing.clear();
            self.list_more_leads();
        }
        if self.recount && ticks.is_multiple_of(RECOUNT_MINUTES) {
            self.recount = false;
            self.count_reports(None);
        }
        self.fetch_more();
    }

    fn on_event(&mut self, event: SwarmEvent<BehaviourEvent>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                info!("listening on {address}");
                if self.config.relay_server
                    && self.config.external.is_empty()
                    && is_specific(&address)
                {
                    // A relay must tell its clients where it can be reached.
                    self.swarm.add_external_address(address);
                }
            }
            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                let addr = match &endpoint {
                    ConnectedPoint::Dialer { address, .. } => address.clone(),
                    ConnectedPoint::Listener { send_back_addr, .. } => send_back_addr.clone(),
                };
                debug!("connected to {peer_id} at {addr}");
                self.problem = None;
                self.alone_since = None;
                if !is_global(&addr) {
                    self.nearby.insert(peer_id);
                }
                // A connection through a relay comes from a bare `/p2p/...`,
                // which nobody can dial.
                let dialable = addr.iter().any(|p| !matches!(p, Protocol::P2p(_)));
                if dialable && !addr.iter().any(|p| p == Protocol::P2pCircuit) {
                    self.remote_addrs.insert(peer_id, addr);
                }
                let through = match &endpoint {
                    ConnectedPoint::Dialer { address, .. } => relay_of(address),
                    ConnectedPoint::Listener { local_addr, .. } => relay_of(local_addr),
                };
                if let Some(relay) = through {
                    self.circuits.insert(peer_id, relay);
                }
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                num_established,
                ..
            } => {
                if num_established == 0 {
                    if self.alone_since.is_none() && self.swarm.connected_peers().next().is_none() {
                        self.alone_since = Some(now_unix());
                    }
                    self.bucket_peers.remove(&peer_id);
                    self.report_peers.remove(&peer_id);
                    // Asked again on coming back, for what it took meanwhile.
                    self.report_listing.remove(&peer_id);
                    self.lead_peers.remove(&peer_id);
                    self.lead_listing.remove(&peer_id);
                    self.oblivious_peers.remove(&peer_id);
                    self.batch_peers.remove(&peer_id);
                    self.fill_peers.remove(&peer_id);
                    self.pages_peers.remove(&peer_id);
                    self.pages_asked.remove(&peer_id);
                    self.sets_peers.remove(&peer_id);
                    self.sets_asked.remove(&peer_id);
                    self.trust_listing.remove(&peer_id);
                    self.fill_asked.remove(&peer_id);
                    // Asked again on coming back, for what it sent meanwhile.
                    self.listing.remove(&peer_id);
                    self.remote_addrs.remove(&peer_id);
                    self.circuits.remove(&peer_id);
                    self.reserved.remove(&peer_id);
                    self.nearby.remove(&peer_id);
                    if self.relays.remove(&peer_id).is_some() {
                        info!("lost the relay {peer_id}");
                    }
                }
            }
            SwarmEvent::ExternalAddrConfirmed { address } => {
                info!("reachable at {address}");
            }
            SwarmEvent::ListenerClosed {
                addresses, reason, ..
            } => {
                debug!("stopped listening on {addresses:?}: {reason:?}");
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                debug!("could not connect to {peer_id:?}: {error}");
                let bootstrap = peer_id.is_some_and(|p| self.bootstrap_peers.contains(&p));
                if bootstrap && self.swarm.connected_peers().next().is_none() {
                    // Name the node by the address it was dialed at, its
                    // name if it has one.
                    let addr = self
                        .config
                        .bootstrap
                        .iter()
                        .find(|a| peer_of(a) == peer_id)
                        .cloned();
                    let problem = explain_dial_error(addr.as_ref(), &error, now_unix());
                    warn!("{} ({})", problem.message, problem.detail);
                    // Of the failures of one round (a node tried by name and
                    // by address), keep the more telling one.
                    let keep = self
                        .problem
                        .as_ref()
                        .is_some_and(|old| old.rank < problem.rank && old.at + 30 > problem.at);
                    if !keep {
                        self.problem = Some(problem);
                    }
                }
            }
            SwarmEvent::Behaviour(event) => self.on_behaviour(event),
            _ => {}
        }
    }

    fn on_behaviour(&mut self, event: BehaviourEvent) {
        match event {
            BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. }) => {
                self.on_identified(peer_id, info);
            }
            BehaviourEvent::Gossipsub(gossipsub::Event::Message {
                propagation_source,
                message_id,
                message,
            }) => {
                let verdict = if message.topic == self.report_topic.hash() {
                    self.on_gossip_report(&message.data)
                } else if message.topic == self.lead_topic.hash() {
                    self.on_gossip_lead(&message.data)
                } else {
                    self.on_header(propagation_source, &message.data)
                };
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .gossipsub
                    .report_message_validation_result(&message_id, &propagation_source, verdict);
            }
            BehaviourEvent::Gossipsub(gossipsub::Event::Subscribed { .. }) => {
                if !self.unannounced.is_empty() {
                    for header in std::mem::take(&mut self.unannounced) {
                        self.announce(header);
                    }
                }
                for lead in std::mem::take(&mut self.unannounced_leads) {
                    self.announce_lead(lead);
                }
            }
            BehaviourEvent::Buckets(event) => self.on_bucket_event(event),
            BehaviourEvent::Batches(event) => self.on_batch_event(event),
            BehaviourEvent::Reports(event) => self.on_report_event(event),
            BehaviourEvent::Oblivious(event) => self.on_oblivious_event(event),
            BehaviourEvent::Credits(event) => self.on_credit_event(event),
            BehaviourEvent::Fill(event) => self.on_fill_event(event),
            BehaviourEvent::Pages(event) => self.on_pages_event(event),
            BehaviourEvent::Sets(event) => self.on_sets_event(event),
            BehaviourEvent::Trust(event) => self.on_trust_event(event),
            BehaviourEvent::Profile(event) => self.on_profile_event(event),
            BehaviourEvent::Leads(event) => self.on_lead_event(event),
            BehaviourEvent::RelayClient(relay::client::Event::ReservationReqAccepted {
                relay_peer_id,
                renewal,
                ..
            }) => {
                if !renewal {
                    info!("reachable through the relay {relay_peer_id}");
                    // Now that others can reach us, ask around for nodes,
                    // which also puts us in their routing tables.
                    let _ = self.swarm.behaviour_mut().kad.bootstrap();
                    self.ask_again_at = Some(tokio::time::Instant::now() + ASK_AGAIN_AFTER);
                }
                self.relays.insert(relay_peer_id, true);
            }
            BehaviourEvent::Relay(relay::Event::ReservationReqAccepted { src_peer_id, .. }) => {
                self.reserved.insert(src_peer_id);
            }
            BehaviourEvent::Relay(relay::Event::ReservationTimedOut { src_peer_id }) => {
                self.reserved.remove(&src_peer_id);
            }
            BehaviourEvent::Kad(kad::Event::RoutingUpdated {
                peer, addresses, ..
            }) => {
                debug!(
                    "routing: {peer} at {:?}",
                    addresses.iter().collect::<Vec<_>>()
                );
                // A node heard of through the network, such as one behind
                // NAT that only a relay can reach: meet it while we have
                // room, rather than at the next maintenance round.
                if !self.swarm.is_connected(&peer)
                    && self.swarm.connected_peers().count() < TARGET_PEERS
                {
                    let _ = self.swarm.dial(peer);
                }
            }
            BehaviourEvent::Dcutr(dcutr::Event {
                remote_peer_id,
                result,
            }) => match result {
                Ok(_) => info!("punched a direct connection to {remote_peer_id}"),
                Err(err) => debug!("no direct connection to {remote_peer_id}: {err}"),
            },
            BehaviourEvent::Autonat(autonat::Event::StatusChanged { new, .. }) => {
                let nat = match new {
                    autonat::NatStatus::Public(_) => "public",
                    autonat::NatStatus::Private => "private",
                    autonat::NatStatus::Unknown => "unknown",
                };
                info!("NAT status: {nat}");
                self.with_status(|s| s.nat = nat.into());
            }
            BehaviourEvent::Kad(kad::Event::ModeChanged { new_mode }) => {
                debug!("routing mode: {new_mode}");
            }
            BehaviourEvent::Mdns(mdns::Event::Discovered(found)) => {
                for (peer, addr) in found {
                    if peer == *self.swarm.local_peer_id() {
                        continue;
                    }
                    debug!("found {peer} on this network at {addr}");
                    self.nearby.insert(peer);
                    self.swarm
                        .behaviour_mut()
                        .kad
                        .add_address(&peer, addr.clone());
                    // Prefer the direct route, even when already connected
                    // through a relay.
                    let direct = self.remote_addrs.get(&peer).is_some_and(|a| !is_global(a));
                    if !direct {
                        let _ = self.swarm.dial(
                            libp2p::swarm::dial_opts::DialOpts::peer_id(peer)
                                .addresses(vec![addr])
                                .condition(libp2p::swarm::dial_opts::PeerCondition::Always)
                                .build(),
                        );
                    }
                }
            }
            BehaviourEvent::Upnp(upnp::Event::NewExternalAddr { external_addr, .. }) => {
                info!("the router forwards {external_addr} to this node");
            }
            _ => {}
        }
    }

    fn on_identified(&mut self, peer: PeerId, info: identify::Info) {
        let supports = |name: &str| info.protocols.iter().any(|p| p.as_ref() == name);
        if !info.agent_version.starts_with("plumb/") {
            debug!("{peer} is not a Plumb node ({})", info.agent_version);
            return;
        }
        // Home network addresses only help nodes on the same network.
        // A circuit address is as reachable as its relay: one on our
        // network relays for nodes we could not otherwise reach.
        let nearby = |addr: &Multiaddr| {
            let through = relay_of(addr);
            self.nearby.contains(through.as_ref().unwrap_or(&peer))
        };
        let usable = |addr: &Multiaddr| is_specific(addr) && (nearby(addr) || is_global(addr));
        // Relayed addresses go in too: for a node behind NAT they are the
        // only way others can find it.
        if supports(KAD_PROTOCOL) {
            for addr in &info.listen_addrs {
                if usable(addr) {
                    self.swarm
                        .behaviour_mut()
                        .kad
                        .add_address(&peer, addr.clone());
                }
            }
        }
        if supports(BUCKET_PROTOCOL) {
            let addrs = info
                .listen_addrs
                .iter()
                .filter(|addr| usable(addr))
                .cloned()
                .collect();
            self.bucket_peers.insert(peer, addrs);
        }
        if supports(REPORT_PROTOCOL) {
            let addrs = info
                .listen_addrs
                .iter()
                .filter(|addr| usable(addr))
                .cloned()
                .collect();
            self.report_peers.insert(peer, addrs);
            if self.report_lists.len() < MAX_REPORT_LISTS && self.report_listing.insert(peer) {
                let current = report_epoch(now_unix());
                for epoch in [current.saturating_sub(1), current] {
                    let id = self
                        .swarm
                        .behaviour_mut()
                        .reports
                        .send_request(&peer, ReportRequest::List { epoch });
                    self.report_lists.insert(id);
                }
            }
        }
        if supports(LEAD_PROTOCOL) {
            self.lead_peers.insert(peer);
            self.list_more_leads();
        }
        if supports(OBLIVIOUS_PROTOCOL) {
            self.oblivious_peers.insert(peer);
        }
        if supports(FILL_PROTOCOL) && self.config.trusted_peers.contains(&peer) {
            self.fill_peers.insert(peer);
        }
        if supports(PAGES_PROTOCOL) && self.config.trusted_peers.contains(&peer) {
            self.pages_peers.insert(peer);
        }
        if supports(SETS_PROTOCOL) {
            self.sets_peers.insert(peer);
        }
        if supports(TRUST_PROTOCOL)
            && self.config.trusted_peers.contains(&peer)
            && self.trust_listing.insert(peer)
        {
            self.swarm
                .behaviour_mut()
                .trust
                .send_request(&peer, TrustRequest {});
        }
        if supports(BATCH_PROTOCOL) {
            self.batch_peers.insert(peer);
            if self.listing.insert(peer) {
                let since = epoch_of(now_unix()).saturating_sub(self.config.catch_up_epochs);
                self.ask_batch_list(peer, since);
            }
        }
        if supports(RELAY_HOP_PROTOCOL)
            && !self.config.relay_server
            && self.relays.len() < MAX_RELAYS
            && !self.relays.contains_key(&peer)
        {
            if let Some(addr) = self.remote_addrs.get(&peer).cloned() {
                let circuit = without_p2p(addr)
                    .with(Protocol::P2p(peer))
                    .with(Protocol::P2pCircuit);
                match self.swarm.listen_on(circuit.clone()) {
                    Ok(_) => {
                        debug!("asking {peer} to relay for us");
                        self.relays.insert(peer, false);
                    }
                    Err(err) => warn!("cannot listen on {circuit}: {err}"),
                }
            }
        }
    }

    pub(super) fn with_status(&self, change: impl FnOnce(&mut NetStatus)) {
        change(&mut self.status.locked());
    }

    fn update_status(&mut self) {
        let listening = self.swarm.listeners().map(ToString::to_string).collect();
        let reachable_at = self
            .swarm
            .external_addresses()
            .map(ToString::to_string)
            .collect();
        let connected_peers = self.swarm.connected_peers().count();
        let relaying_peers = self.oblivious_peers.len();
        let relays = self
            .relays
            .iter()
            .filter(|(_, granted)| **granted)
            .map(|(peer, _)| peer.to_string())
            .collect();
        // Not waited for: a lock held by work off the swarm task keeps the
        // last count instead.
        let held = peek(&self.store, BatchStore::len);
        let reports_held = peek(&self.reports, ReportStore::len);
        let tokens_held = peek(&self.wallet, Wallet::total);
        let mut peers: Vec<PeerView> = self
            .swarm
            .connected_peers()
            .map(|peer| PeerView {
                peer_id: peer.to_string(),
                route: if self.nearby.contains(peer) {
                    Route::Nearby
                } else if self.remote_addrs.contains_key(peer) {
                    Route::Direct
                } else {
                    Route::Relayed
                },
                relay: self.relays.get(peer).copied().unwrap_or(false),
                bootstrap: self.bootstrap_peers.contains(peer),
            })
            .collect();
        let nearby_peers = peers.iter().filter(|p| p.route == Route::Nearby).count();
        let search_peers = self
            .bucket_peers
            .keys()
            .filter(|peer| self.may_search(peer))
            .count();
        let friends_of_friends = self
            .friends
            .of(self.swarm.local_peer_id(), &self.config.trusted_peers)
            .len();
        let (leads_held, leads_shared) = (self.leads.len(), self.leads_shared);
        // Bootstrap nodes and relays first, then the nearest.
        peers.sort_by_key(|p| (!p.bootstrap, !p.relay, p.route != Route::Nearby));
        peers.truncate(MAX_PEER_VIEWS);
        let problem = self.problem.clone();
        let alone_since = self.alone_since;
        let me = self.ledger.account(self.swarm.local_peer_id());
        let credits = CreditStatus {
            balance: me.balance(),
            confirmed_crawls: me.confirmed,
            mismatched_crawls: me.mismatched,
            accounts: self.ledger.len(),
            tokens_issued: self.tokens_issued,
            tokens_redeemed: self.issuer.redeemed(),
            priority_answered: self.priority_answered,
            free_answers_today: self.allowance.today(),
            answer_per_day: self.allowance.per_day(),
            turned_away: self.allowance.turned_away(),
            in_credit_here: self.ledger.in_credit(),
            at_peers: {
                let mut at: Vec<CreditsAtPeer> = self
                    .credits_at
                    .iter()
                    .map(|(peer, at)| CreditsAtPeer {
                        peer_id: peer.to_string(),
                        credits: at.credits,
                        counts: at.counts,
                    })
                    .collect();
                at.sort_by(|a, b| b.credits.cmp(&a.credits).then(a.peer_id.cmp(&b.peer_id)));
                at
            },
            tokens_spent: self.tokens_spent.load(std::sync::atomic::Ordering::Relaxed),
            tokens_held: 0,
        };
        let storage = self.storage.as_ref().map(|budget| budget.status());
        let ingest_backpressure = self.ingest_blocked
            || self.records.capacity() == 0
            || storage.is_some_and(|s| s.backpressure);
        self.with_status(|s| {
            s.storage = storage;
            s.ingest_backpressure = ingest_backpressure;
            s.peers = peers;
            s.nearby_peers = nearby_peers;
            s.search_peers = search_peers;
            s.friends_of_friends = friends_of_friends;
            s.leads_held = leads_held;
            s.leads_shared = leads_shared;
            s.problem = problem;
            s.alone_since = alone_since;
            let tokens_held = tokens_held.unwrap_or(s.credits.tokens_held);
            s.credits = CreditStatus {
                tokens_held,
                ..credits
            };
            s.listening = listening;
            s.reachable_at = reachable_at;
            s.connected_peers = connected_peers;
            s.relaying_peers = relaying_peers;
            s.relays = relays;
            if let Some(held) = held {
                s.batches_held = held;
            }
            if let Some(reports_held) = reports_held {
                s.reports_held = reports_held;
            }
        });
    }
}
