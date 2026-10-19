//! What the rest of the program calls: a [`NetHandle`]'s methods.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use libp2p::{Multiaddr, PeerId};
use plumb_core::locks::{Locked, RwLocked};
use plumb_core::now_unix;
use plumb_core::sites::SiteList;
use tokio::sync::oneshot;
use tracing::debug;

use super::{
    count_round, keep_trusted_crawls, Command, CreditsAt, NetHandle, NetStatus, Serving,
    REPORT_TRIES,
};
use crate::assign::{epoch_of, is_assigned};
use crate::fill::FillPage;
use crate::hash::Hash;
use crate::leads::{FoundLead, Lead, LeadDraft, LeadKeys};
use crate::oblivious::{ObliviousRequest, ObliviousResponse, SignedKeys};
use crate::pages::PagesChunk;
use crate::popularity::{PopularityTable, Report};
use crate::proto::*;
use crate::search::{BucketPeer, NetSearch};
use crate::sets::SetsAnswer;
use crate::storage::StorageBudget;

impl NetHandle {
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Whether this node is assigned `domain` in the epoch of `now`.
    pub fn is_assigned(&self, domain: &str, now: u64) -> bool {
        is_assigned(epoch_of(now), &self.peer_id, domain, self.share_ppm)
    }

    /// The nodes that split the sites with this one when it crawls
    /// unassigned sites: itself and those of `partners` that sent a crawl
    /// in the last day (from [`NetStatus::crawlers`]), so the sites of a
    /// partner that went quiet go to the others.
    pub fn crawl_group(&self, partners: &[PeerId]) -> Vec<PeerId> {
        let active: HashSet<String> = self
            .status()
            .crawlers
            .into_iter()
            .filter(|view| view.homepages_last_day > 0)
            .map(|view| view.peer_id)
            .collect();
        let mut group = vec![self.peer_id];
        for peer in partners {
            if *peer != self.peer_id && !group.contains(peer) && active.contains(&peer.to_string())
            {
                group.push(*peer);
            }
        }
        group
    }

    /// Whether `domain` is this node's to crawl among `group` (see
    /// [`crate::assign::slice_owner`]).
    pub fn owns_slice(&self, group: &[PeerId], domain: &str) -> bool {
        crate::assign::slice_owner(group, domain) == Some(&self.peer_id)
    }

    pub fn storage_budget(&self) -> Option<&Arc<StorageBudget>> {
        self.storage.as_ref()
    }

    pub fn status(&self) -> NetStatus {
        self.status.locked().clone()
    }

    /// Signs `sites` (crawl results, each as its stored item) as a batch,
    /// keeps it and tells the network. Sites this node was not assigned are
    /// sent too but other nodes ignore them. `None` when there was nothing
    /// to sign.
    pub async fn publish(&self, sites: SiteList) -> Result<Option<Hash>> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Publish { sites, reply })?;
        answer.await.context("the network task stopped")?
    }

    /// Reads retained buckets locally when rounds are enabled, queueing real
    /// missing/stale bucket IDs for the independently scheduled background task.
    /// Stale results are returned immediately. An empty missing result may wait
    /// up to `wait` for a scheduled round, but never triggers or advances one.
    /// With rounds disabled, preserves legacy immediate network fetching.
    pub async fn search(&self, query: &str, wait: Duration) -> Result<NetSearch> {
        let deadline = tokio::time::Instant::now() + wait;
        let _turn = tokio::time::timeout_at(deadline, self.searches.acquire())
            .await
            .map_err(|_| anyhow::anyhow!("too many network searches at once; try again"))?
            .context("the network is shutting down")?;
        let mut found = if self.pace.every().is_some() {
            // Subscribe before reading to avoid losing a completion between a
            // cache miss and waiting. This only observes scheduled work.
            let mut updates = self.round_updates.subscribe();
            loop {
                let (cached, refresh) =
                    crate::search::cache_search_with_refresh(query, &self.cache, now_unix());
                self.pending_buckets.locked().queue(refresh);
                if cached.pending == 0 || !cached.found.is_empty() {
                    break cached;
                }
                match tokio::time::timeout_at(deadline, updates.changed()).await {
                    Ok(Ok(())) => continue,
                    _ => break cached,
                }
            }
        } else {
            let (reply, peers) = oneshot::channel();
            self.send(Command::Peers(Serving::Buckets, reply))?;
            let peers = peers.await.context("the network task stopped")?;
            let found = crate::search::search(
                query,
                &peers,
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                now_unix(),
                Some(&self.wallet),
                Some(&self.cache),
            )
            .await;
            self.tokens_spent
                .fetch_add(found.priority as u64, std::sync::atomic::Ordering::Relaxed);
            if !found.answered_by.is_empty() {
                self.send(Command::Answered(found.answered_by.clone()))?;
            }
            if found.asked > 0 {
                count_round(&self.status, &found);
            }
            found
        };
        keep_trusted_crawls(&mut found, &self.trusted, now_unix());
        // Two keys of one person can sign the same crawl: a site is
        // confirmed only by crawlers this node counts (see crate::agree).
        let crawlers: Vec<String> = found
            .found
            .iter()
            .flat_map(|site| site.crawlers.iter().cloned())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if !crawlers.is_empty() {
            let (reply, counting) = oneshot::channel();
            self.send(Command::Counting(crawlers, reply))?;
            let counting = counting.await.context("the network task stopped")?;
            for site in &mut found.found {
                site.confirmed = site
                    .crawlers
                    .iter()
                    .filter(|c| counting.contains(*c))
                    .count()
                    >= crate::agree::QUORUM;
            }
        }
        Ok(found)
    }

    /// Forgets retained buckets and queued refresh IDs. An already in-flight
    /// scheduled round may still finish and store its answers after this call;
    /// it cannot resurrect retries from the cleared queue.
    pub fn clear_search_cache(&self) {
        self.pending_buckets.locked().clear();
        self.cache.clear();
    }

    /// Up to `limit` sites whose crawlers disagree, for this node to fetch
    /// itself whether or not it is assigned them: its own crawl settles the
    /// dispute once published (see [`crate::agree`]).
    pub async fn rechecks(&self, limit: usize) -> Result<Vec<String>> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Rechecks(limit, reply))?;
        answer.await.context("the network task stopped")
    }

    /// The connected nodes that answer bucket requests, for a front end
    /// that relays sealed bucket requests for browsers.
    pub async fn bucket_peers(&self) -> Result<Vec<PeerId>> {
        let (reply, peers) = oneshot::channel();
        self.send(Command::Peers(Serving::Buckets, reply))?;
        let peers = peers.await.context("the network task stopped")?;
        Ok(peers.into_iter().map(|peer| peer.peer).collect())
    }

    /// Hands `report` to another node under a throwaway identity, trying
    /// up to [`REPORT_TRIES`] nodes at random, each for at most `wait`.
    /// That node keeps it and passes it on to the rest. It goes sealed
    /// through a third node picked at random, a relay (see
    /// [`crate::oblivious`]), so the node it is handed to never sees this
    /// node's IP address; straight only when no other node can relay (a
    /// network of two).
    pub async fn send_report(&self, report: &Report, wait: Duration) -> Result<()> {
        let (reply, peers) = oneshot::channel();
        self.send(Command::Peers(Serving::Reports, reply))?;
        let mut peers = peers.await.context("the network task stopped")?;
        if peers.is_empty() {
            anyhow::bail!("no node to hand a report to yet");
        }
        crate::search::shuffle(&mut peers);
        let relays: Vec<&BucketPeer> = peers.iter().filter(|p| p.oblivious).collect();
        let routes: Vec<(&BucketPeer, Option<&BucketPeer>)> = if relays.len() >= 2 {
            // Each target through the relay after it, so no node is both.
            (0..relays.len())
                .map(|i| (relays[i], Some(relays[(i + 1) % relays.len()])))
                .collect()
        } else {
            peers.iter().map(|p| (p, None)).collect()
        };
        let mut last_error = None;
        for (peer, relay) in routes.into_iter().take(REPORT_TRIES) {
            let sent = match relay {
                Some(relay) => {
                    crate::search::submit_report_oblivious(
                        relay,
                        &peer.peer,
                        report,
                        wait,
                        now_unix(),
                    )
                    .await
                }
                None => crate::throwaway::submit_report(peer, report, wait).await,
            };
            match sent {
                Ok(_) => {
                    self.status.locked().reports_sent += 1;
                    return Ok(());
                }
                Err(err) => {
                    debug!("{} did not take a report: {err:#}", peer.peer);
                    last_error = Some(err);
                }
            }
        }
        Err(last_error.expect("tried at least one node"))
    }

    /// What the reports this node holds say people pick, recounted every
    /// [`RECOUNT_MINUTES`](super::RECOUNT_MINUTES) minutes while new reports come in.
    pub fn popularity(&self) -> Arc<PopularityTable> {
        self.popularity.read_locked().clone()
    }

    /// Counts the reports held now rather than at the next recount.
    pub async fn recount(&self) -> Result<Arc<PopularityTable>> {
        let (reply, table) = oneshot::channel();
        self.send(Command::Recount(reply))?;
        table.await.context("the network task stopped")
    }

    /// As a relay: the key of `target`, the same one every asker gets for a
    /// while (see [`crate::oblivious`]). This node's own for itself.
    pub async fn oblivious_keys(&self, target: PeerId) -> Result<Option<SignedKeys>> {
        match self.oblivious(ObliviousRequest::Keys { target }).await? {
            ObliviousResponse::Keys(keys) => Ok(keys),
            ObliviousResponse::Sealed(_) => Ok(None),
        }
    }

    /// As a relay: passes `message`, a request sealed to `target`'s key, on
    /// to it, and returns its sealed answer. Answers it itself when it is
    /// the target. For a front end that relays for people who are not
    /// nodes, such as a browser.
    pub async fn oblivious_forward(
        &self,
        target: PeerId,
        message: Vec<u8>,
    ) -> Result<Option<Vec<u8>>> {
        let message = serde_bytes::ByteBuf::from(message);
        match self
            .oblivious(ObliviousRequest::Forward { target, message })
            .await?
        {
            ObliviousResponse::Sealed(answer) => Ok(answer.map(serde_bytes::ByteBuf::into_vec)),
            ObliviousResponse::Keys(_) => Ok(None),
        }
    }

    async fn oblivious(&self, request: ObliviousRequest) -> Result<ObliviousResponse> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Oblivious(request, reply))?;
        answer.await.context("the network task stopped")
    }

    /// Asks the connected node `issuer` for up to `wanted` tokens (at most
    /// [`MAX_ISSUE`](crate::credits::MAX_ISSUE)), paid with the credits it
    /// counts for this node, and keeps them. Returns how many it signed.
    pub async fn collect_tokens(&self, issuer: PeerId, wanted: usize) -> Result<usize> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::AskTokens {
            issuer,
            wanted,
            reply,
        })?;
        answer.await.context("the network task stopped")?
    }

    /// This node's credits as the connected node `peer` counts them.
    pub async fn credits_at(&self, peer: PeerId) -> Result<CreditsAt> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::AskCredits(peer, reply))?;
        answer.await.context("the network task stopped")?
    }

    /// Asks a connected node this node trusts for its crawled sites from
    /// position `from` of its list, at most `count` (see [`crate::fill`]):
    /// `prefer` when it is connected and fills, another one otherwise.
    /// `None` when no trusted node that fills is connected. With `all`, its
    /// sites crawled or not, for a node setting up from the network.
    pub async fn fill(
        &self,
        prefer: Option<PeerId>,
        from: u64,
        count: u32,
        all: bool,
    ) -> Result<Option<FillPage>> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Fill {
            prefer,
            from,
            count,
            all,
            reply,
        })?;
        answer.await.context("the network task stopped")?
    }

    /// The connected nodes this node trusts that answer fill requests.
    pub async fn fill_peers(&self) -> Result<Vec<PeerId>> {
        let (reply, peers) = oneshot::channel();
        self.send(Command::FillPeers(reply))?;
        peers.await.context("the network task stopped")
    }

    /// Asks a connected node this node trusts for `len` bytes from
    /// `offset` of its file of the page set `set` (see [`crate::pages`]):
    /// `peer` when given (the node a download started from, so its pieces
    /// all come from one file), else any but those in `skip` (nodes that
    /// said they have no file of it). `None` when no such trusted node that
    /// serves page sets is connected.
    pub async fn pages_chunk(
        &self,
        set: &str,
        offset: u64,
        len: u32,
        peer: Option<PeerId>,
        skip: &[PeerId],
    ) -> Result<Option<PagesChunk>> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Pages {
            request: PagesRequest {
                set: set.to_string(),
                offset,
                len,
            },
            peer,
            skip: skip.to_vec(),
            reply,
        })?;
        answer.await.context("the network task stopped")?
    }

    /// Asks a connected node that serves signed sets (see [`crate::sets`])
    /// `request`: `peer` when given (the node a part's download started
    /// from), else one not in `skip`, nodes this node trusts first. `None`
    /// when no such node is connected. The answer comes from any node, so
    /// the asker checks it: the manifest's signature, each part's hash.
    pub async fn ask_sets(
        &self,
        request: SetsRequest,
        peer: Option<PeerId>,
        skip: &[PeerId],
    ) -> Result<Option<SetsAnswer>> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Sets {
            request,
            peer,
            skip: skip.to_vec(),
            reply,
        })?;
        answer.await.context("the network task stopped")?
    }

    /// Asks `peer` about a searcher's profile (see [`ProfileRequest`]),
    /// dialling it first when it is not connected.
    pub async fn ask_profile(
        &self,
        peer: PeerId,
        request: ProfileRequest,
    ) -> Result<ProfileResponse> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Profile {
            peer,
            request,
            reply,
        })?;
        answer.await.context("the network task stopped")?
    }

    /// Tokens held that `issuer` signed.
    pub fn tokens_held(&self, issuer: &PeerId) -> usize {
        self.wallet.locked().held(issuer)
    }

    /// Signs `draft` as a lead of this node's, keeps it and passes it on to
    /// the network: the nodes connected now hear of it over gossip, the
    /// others when they next meet a node that holds it. Refused when it is
    /// not fit to share or this node shared [`crate::leads::MAX_LEADS_PER_DAY`]
    /// leads in the last day.
    pub async fn share_lead(&self, draft: LeadDraft) -> Result<Lead> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::ShareLead(draft, reply))?;
        answer.await.context("the network task stopped")?
    }

    /// [`NetHandle::share_lead`], for a blocking thread.
    pub fn share_lead_blocking(&self, draft: LeadDraft) -> Result<Lead> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::ShareLead(draft, reply))?;
        answer.blocking_recv().context("the network task stopped")?
    }

    /// Pages other nodes reported for a search with `keys`, at most `limit`:
    /// only from nodes this node's search scope asks, never its own.
    pub async fn leads(&self, keys: LeadKeys, limit: usize) -> Result<Vec<FoundLead>> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Leads { keys, limit, reply })?;
        answer.await.context("the network task stopped")
    }

    /// [`NetHandle::leads`], for a blocking thread.
    pub fn leads_blocking(&self, keys: LeadKeys, limit: usize) -> Result<Vec<FoundLead>> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Leads { keys, limit, reply })?;
        answer.blocking_recv().context("the network task stopped")
    }

    /// Dials `addr`, for tests and for adding a node by hand.
    pub fn dial(&self, addr: Multiaddr) -> Result<()> {
        self.send(Command::Dial(addr))
    }

    /// Tries the bootstrap nodes again now, rather than at the next
    /// minute's round, and asks the nodes it has for more.
    pub fn reconnect(&self) -> Result<()> {
        self.send(Command::Reconnect)
    }

    /// Stops the swarm and waits for it. Later calls do nothing.
    pub async fn shutdown(&self) {
        self.replay_stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(rounds) = self.rounds.locked().take() {
            rounds.abort();
        }
        let _ = self.commands.send(Command::Stop);
        let task = self.task.locked().take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    fn send(&self, command: Command) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow::anyhow!("the network task stopped"))
    }
}
