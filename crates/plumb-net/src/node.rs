//! A node's place in the network: one libp2p swarm, run on a Tokio task,
//! driven through a [`NetHandle`].
//!
//! # Getting connected without port forwarding
//!
//! A node dials out to its bootstrap nodes and to the nodes they tell it
//! about (Kademlia), over TCP and QUIC. That is all most nodes need:
//! fetching batches, asking for buckets and answering bucket requests all
//! happen over connections it opened itself.
//!
//! Reachable nodes (a server, a VPS, a homelab with a forwarded port or
//! UPnP) run with [`NetConfig::relay_server`] on and their public address in
//! [`NetConfig::external`]. They accept connections and relay small
//! messages for nodes behind NAT. A node that is not a relay itself takes a
//! reservation on up to [`MAX_RELAYS`] relays it is connected to, which
//! makes it reachable at `<relay>/p2p-circuit/p2p/<id>`. When two nodes meet
//! over a relay, DCUtR tries to punch a direct connection through both NATs
//! and moves the traffic there when it works. UPnP asks home routers to
//! forward a port where they allow it.
//!
//! # What the swarm does
//!
//! * Crawl sharing: [`NetHandle::publish`] signs a batch of crawl results,
//!   keeps it and announces its header on the gossip topic. A node that
//!   hears of a batch it lacks fetches it from the node that passed the
//!   header on (or the crawler), checks it, keeps it, and hands the records
//!   it accepts ([`accept_batch`]) to the [`Agreement`] step, which hands
//!   a record to the receiver returned by [`start`] only once two crawlers
//!   agree on it (see [`crate::agree`]).
//!   On meeting a node, it asks for the batches of the last
//!   [`CATCH_UP_EPOCHS`] epochs it missed, and every [`RELIST_MINUTES`]
//!   asks the nodes it is connected to again for recent ones.
//! * Network search: [`NetHandle::search`] never sends the query; scheduled
//!   rounds fetch buckets and searches read retained local copies (see
//!   [`crate::bucket`] and [`crate::search`]). The node answers other
//!   nodes' bucket requests from its own [`BucketSource`], with a proof for
//!   every site it holds a signed crawl of.
//! * Popularity sharing: [`NetHandle::send_report`] hands a popularity
//!   report to another node under a throwaway identity. A node handed a
//!   report keeps it and passes it on over the gossip topic, so every node
//!   holds every report and counts them itself into the table
//!   [`NetHandle::popularity`] returns (see [`crate::popularity`]). On
//!   meeting a node, it asks for the reports of this week and last week.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::core::ConnectedPoint;
use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::request_response::{self, OutboundRequestId, ProtocolSupport, ResponseChannel};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{
    autonat, connection_limits, dcutr, gossipsub, identify, kad, mdns, noise, ping, relay, tcp,
    upnp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm,
};
use plumb_core::{now_unix, SiteRecord};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::agree::{Agreement, AgreementStatus, MIN_JUDGED};
use crate::allowance::{Allowance, Source};
use crate::assign::{epoch_of, is_assigned, MAX_SHARE_PPM};
use crate::batch::{
    accept_batch, accept_news, accept_own_batch, accept_trusted_batch, mostly_kept, Batch,
    SignedHeader, MAX_BATCH_AGE_EPOCHS, MAX_BATCH_RECORDS,
};
use crate::bucket::{BucketSource, BUCKETS};
use crate::credits::{
    CreditStatus, CreditsAtPeer, Issuer, Ledger, Pending, Wallet, MAX_ISSUE, MIN_ANSWERS_FOR_TOKENS,
};
use crate::fill::{FillPage, FILL_REQUESTS_PER_MINUTE, MAX_FILLING};
use crate::hash::Hash;
use crate::joining::{explain_dial_error, peer_of, JoinProblem, PeerView, Route};
use crate::oblivious::{
    seal_response, Gateway, ObliviousRequest, ObliviousResponse, Opened, SignedKeys, MAX_MESSAGE,
    OBLIVIOUS_PROTOCOL, RELAY_KEY_CACHE, REPORT_REQUEST_SIZE,
};
use crate::pages::{PagesChunk, MAX_SERVING, PAGES_REQUESTS_PER_MINUTE};
use crate::popularity::{report_epoch, PopularityTable, Report};
use crate::proto::*;
use crate::reports::ReportStore;
use crate::rounds::{Pace, PendingBuckets, RoundStatus, ROUND_EVERY};
use crate::scope::{Friends, SearchScope, MAX_SHARED_TRUST};
use crate::search::{BucketPeer, NetSearch};
use crate::store::{read_held, BatchStore, CrawlerView, RETAIN_EPOCHS};

/// Relays a node behind NAT takes reservations on.
pub const MAX_RELAYS: usize = 2;
/// Profile requests answered at once.
const MAX_PROFILE_SERVING: usize = 4;
/// Epochs of batches a node asks for when it meets another.
pub const CATCH_UP_EPOCHS: u64 = 3;
/// Minutes between asking connected nodes again for the batches of this
/// epoch and the last, for any whose announcement or fetch was missed: a
/// busy node's fetches time out, and a batch heard of while its holders
/// were out of reach is otherwise never fetched.
pub const RELIST_MINUTES: u64 = 30;
/// A node dials more nodes it knows of while it has fewer connections.
pub const TARGET_PEERS: usize = 8;
/// Minutes between tries at the bootstrap nodes while a node has other
/// connections but none to them, as after a bootstrap node restarts: they
/// are the relays that let nodes behind NAT be reached.
pub const BOOTSTRAP_REDIAL_MINUTES: u64 = 5;
/// As a relay: circuits one node or IP address may open at once, before
/// it is held to one every [`CIRCUIT_REFILL`].
const CIRCUIT_BURST: NonZeroU32 = NonZeroU32::new(600).unwrap();
const CIRCUIT_REFILL: Duration = Duration::from_millis(100);
/// Batch fetches in flight at once.
const MAX_FETCHES: usize = 16;

/// Network searches run at once by one node ([`NetHandle::search`]).
pub const MAX_SEARCHES: usize = 4;

/// Batches waiting to be fetched, at most.
const MAX_WANTED: usize = 50_000;

/// Refused batch ids remembered; the set is emptied when it reaches this.
const MAX_REFUSED: usize = 100_000;
/// Bucket requests answered at once for free; more are turned away as
/// busy unless they spend a token (see [`crate::credits`]).
pub const MAX_ANSWERING: usize = 8;
/// Bucket requests answered at once beyond [`MAX_ANSWERING`], for tokens.
pub const PRIORITY_SLOTS: usize = 8;
/// A node keeps at least this many tokens from each node it searches, and
/// asks for [`TOKEN_REFILL`] more when it has fewer.
pub const TOKEN_LOW: usize = 4;
pub const TOKEN_REFILL: usize = 16;
/// Minutes between two asks for tokens from one node.
const TOKEN_ASK_MINUTES: u64 = 30;
/// Requests passed on for others at once (see [`crate::oblivious`]).
const MAX_RELAYING: usize = 32;
/// Connections a node accepts: every bucket request of a search comes on
/// a connection of its own, so this is generous, but bounded.
const MAX_PENDING_INCOMING: u32 = 256;
const MAX_ESTABLISHED_INCOMING: u32 = 1024;
/// Connections with one node at once (TCP, QUIC, through relays).
const MAX_ESTABLISHED_PER_PEER: u32 = 8;

fn connection_limits() -> connection_limits::ConnectionLimits {
    connection_limits::ConnectionLimits::default()
        .with_max_pending_incoming(Some(MAX_PENDING_INCOMING))
        .with_max_established_incoming(Some(MAX_ESTABLISHED_INCOMING))
        .with_max_established_per_peer(Some(MAX_ESTABLISHED_PER_PEER))
}

/// Largest answer taken on `/plumb/batch/1`: a batch of at most
/// [`crate::batch::MAX_BATCH_BYTES`] of records, or
/// [`MAX_LISTED_BATCHES`] headers of a few hundred bytes each.
const MAX_BATCH_RESPONSE: u64 = crate::batch::MAX_BATCH_BYTES as u64 + 4 * 1024 * 1024;
/// Largest answer taken on `/plumb/report/1`: [`MAX_LISTED_REPORTS`]
/// reports, each about 600 bytes and 1.3 KB at the longest pick.
const MAX_REPORT_RESPONSE: u64 = 64 * 1024 * 1024;
/// Report lists asked of other nodes at once: one node's two weeks. Nodes
/// met meanwhile are asked when they next identify themselves.
const MAX_REPORT_LISTS: usize = 2;

/// Batch and report requests of other nodes answered at once: one can mean
/// reading a 16 MB batch. More are answered with nothing for now.
const MAX_LISTS_SERVING: usize = 4;
const RELAY_HOP_PROTOCOL: &str = "/libp2p/circuit/relay/0.2.0/hop";
/// Nodes a report is offered to, one after the other, until one takes it.
pub const REPORT_TRIES: usize = 3;
/// Minutes between two recounts of the reports, when new ones came in.
pub const RECOUNT_MINUTES: u64 = 10;
/// Where the counted reports are written, for anyone curious.
const POPULARITY_FILE: &str = "popularity.json";
/// Where what the trusted nodes trust is kept (see [`crate::scope`]).
const FRIENDS_FILE: &str = "friends.json";
/// Between two batches sent again by [`resend_trusted`].
const RESEND_PAUSE: Duration = Duration::from_millis(10);
/// The trusted nodes whose held batches were taken in as trusted.
const TRUST_APPLIED_FILE: &str = "trusted-applied";
/// The scope the bucket cache was filled under.
const SCOPE_FILE: &str = "search-scope";

/// Nodes every node trusts unless told otherwise (see
/// [`NetConfig::trusted_peers`]): the plumbsearch.org node, so a new node
/// takes in crawls from the start while the network is small (Liz,
/// 2026-10-03).
pub const DEFAULT_TRUSTED_PEERS: &[&str] =
    &["12D3KooWEDPBv4sacn42shoToAwu62CreVC89QFiAA31HrWv3xrg"];

/// How a node joins the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetConfig {
    /// Holds the node key (`node.key`) and the batches (`batches/`).
    pub dir: PathBuf,
    /// Addresses to listen on.
    pub listen: Vec<Multiaddr>,
    /// Addresses others can reach this node at, for a node with a public
    /// address. A relay server needs at least one.
    pub external: Vec<Multiaddr>,
    /// Nodes to connect to first, with their `/p2p/<id>`.
    pub bootstrap: Vec<Multiaddr>,
    /// Accept relay reservations from nodes behind NAT. Only for nodes
    /// others can reach.
    pub relay_server: bool,
    /// Ask the home router to forward a port (UPnP).
    pub upnp: bool,
    /// Find other nodes on the same home network (mDNS), which a relay
    /// cannot always join: many routers do not let two machines behind
    /// them reach each other through the router's public address.
    pub local_discovery: bool,
    /// Share of all sites this node takes on each epoch, in parts per
    /// million, at most [`MAX_SHARE_PPM`].
    pub share_ppm: u32,
    /// Answer other nodes' bucket requests (network searches) from the
    /// local [`BucketSource`].
    pub answer_searches: bool,
    /// Nodes whose crawls are taken in as soon as they sign them, instead
    /// of waiting for a second crawler to agree (see `crate::agree`).
    /// [`DEFAULT_TRUSTED_PEERS`] unless changed.
    pub trusted_peers: Vec<PeerId>,
    /// Which nodes this node's network searches ask (see [`crate::scope`]):
    /// friends of friends unless changed.
    pub search_scope: SearchScope,
    /// Bucket requests answered at once for free, [`MAX_ANSWERING`] unless
    /// changed; up to [`PRIORITY_SLOTS`] more for requests that spend a
    /// token.
    pub max_answering: usize,
    /// Keep a few tokens from each node it searches, bought with this
    /// node's credits, to be answered when that node is busy.
    pub collect_tokens: bool,
    /// Most bucket requests answered for free a day for nodes this node
    /// does not trust (see [`crate::allowance`]); `None` for no daily
    /// limit. Requests that pay with credits are still answered past it.
    pub answer_per_day: Option<u64>,
    /// Days of batches kept, [`RETAIN_EPOCHS`] unless changed; fewer for a
    /// node that crawls a lot on a small disk.
    pub keep_batches_days: u64,
    /// Fixed interval between independently scheduled bucket rounds (see
    /// [`crate::rounds`]). Enabled searches read retained local data. `None`
    /// preserves legacy immediate-fetch behavior; default is [`ROUND_EVERY`].
    pub round_every: Option<Duration>,
    /// Ask trusted nodes for their crawls to fill this node's free space
    /// (see [`crate::fill`]); the node decides how much. On unless changed.
    pub fill: bool,
    /// Epochs of batches asked for on meeting a node, [`CATCH_UP_EPOCHS`]
    /// unless changed; at most [`MAX_BATCH_AGE_EPOCHS`] are taken.
    pub catch_up_epochs: u64,
}

impl NetConfig {
    /// Listens on port 4001 over TCP and QUIC, IPv4 and IPv6, answers
    /// searches, takes the largest share, asks for UPnP; no relay server, no
    /// bootstrap nodes.
    pub fn new(dir: PathBuf) -> NetConfig {
        NetConfig {
            dir,
            listen: vec![
                "/ip4/0.0.0.0/tcp/4001".parse().expect("valid"),
                "/ip4/0.0.0.0/udp/4001/quic-v1".parse().expect("valid"),
                "/ip6/::/tcp/4001".parse().expect("valid"),
                "/ip6/::/udp/4001/quic-v1".parse().expect("valid"),
            ],
            external: Vec::new(),
            bootstrap: Vec::new(),
            relay_server: false,
            upnp: true,
            local_discovery: true,
            share_ppm: MAX_SHARE_PPM,
            answer_searches: true,
            trusted_peers: DEFAULT_TRUSTED_PEERS
                .iter()
                .map(|id| id.parse().expect("a valid peer id"))
                .collect(),
            search_scope: SearchScope::default(),
            max_answering: MAX_ANSWERING,
            collect_tokens: true,
            answer_per_day: None,
            keep_batches_days: RETAIN_EPOCHS,
            round_every: Some(ROUND_EVERY),
            fill: true,
            catch_up_epochs: CATCH_UP_EPOCHS,
        }
    }
}

/// What the network side of a node is doing, for `GET /api/status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetStatus {
    pub peer_id: String,
    pub listening: Vec<String>,
    /// Addresses others can reach this node at, as far as it knows.
    pub reachable_at: Vec<String>,
    /// `public`, `private` (behind NAT) or `unknown`, from AutoNAT.
    pub nat: String,
    pub connected_peers: usize,
    /// Of those, the ones that relay sealed requests (see
    /// [`crate::oblivious`]).
    pub relaying_peers: usize,
    /// Relays this node holds a reservation on.
    pub relays: Vec<String>,
    pub batches_held: usize,
    pub batches_published: u64,
    pub batches_received: u64,
    /// Crawls waiting for a second crawler, and crawlers no longer counted.
    pub agreement: AgreementStatus,
    /// Bucket requests from other nodes answered, sealed ones included.
    pub buckets_served: u64,
    /// Crawled sites sent to nodes filling their space (see
    /// [`crate::fill`]).
    #[serde(default)]
    pub fill_records_served: u64,
    /// Popularity reports held, this week's and last week's.
    pub reports_held: usize,
    /// Popularity reports this node sent.
    pub reports_sent: u64,
    /// Picks that enough reports were sent of to be read.
    pub popular_picks: usize,
    /// Sealed requests (bucket requests and popularity reports) passed on
    /// for others, as their relay.
    pub requests_relayed: u64,
    /// The connected nodes, at most [`MAX_PEER_VIEWS`] of them.
    #[serde(default)]
    pub peers: Vec<PeerView>,
    /// Connected nodes on the same home or office network.
    #[serde(default)]
    pub nearby_peers: usize,
    /// Since when, in Unix seconds, this node has had no connections.
    #[serde(default)]
    pub alone_since: Option<u64>,
    /// Why the last try to reach a bootstrap node failed, while no node is
    /// connected.
    #[serde(default)]
    pub problem: Option<JoinProblem>,
    /// What crawling earned, and the tokens issued and held (see
    /// [`crate::credits`]).
    #[serde(default)]
    pub credits: CreditStatus,
    /// What each crawler sent, this node included, from the batches held;
    /// updated every minute.
    #[serde(default)]
    pub crawlers: Vec<CrawlerView>,
    /// This node's scheduled rounds, or immediate legacy rounds when disabled
    /// (see [`crate::rounds`]); no separate search or pending-query counters.
    #[serde(default)]
    pub rounds: RoundStatus,
    /// Which nodes this node's network searches ask.
    #[serde(default)]
    pub search_scope: SearchScope,
    /// Nodes the trusted nodes trust, as far as they told this node.
    #[serde(default)]
    pub friends_of_friends: usize,
    /// Connected nodes that answer searches and that this node's searches
    /// may ask under [`NetStatus::search_scope`].
    #[serde(default)]
    pub search_peers: usize,
}

/// How many connected nodes [`NetStatus::peers`] lists.
pub const MAX_PEER_VIEWS: usize = 50;

/// Talks to the swarm task.
#[derive(Debug)]
pub struct NetHandle {
    peer_id: PeerId,
    share_ppm: u32,
    /// [`NetConfig::trusted_peers`], whose signed crawls found by a search
    /// this node keeps whole (see [`NetHandle::search`]).
    trusted: Vec<PeerId>,
    commands: mpsc::UnboundedSender<Command>,
    status: Arc<Mutex<NetStatus>>,
    popularity: Arc<RwLock<Arc<PopularityTable>>>,
    wallet: Arc<Mutex<Wallet>>,
    /// Tokens this node's searches spent.
    tokens_spent: Arc<std::sync::atomic::AtomicU64>,
    /// Buckets retained from scheduled rounds or legacy immediate searches (see
    /// [`crate::cache`]).
    cache: Arc<crate::cache::BucketCache>,
    /// When background rounds go (see [`crate::rounds`]).
    pace: Arc<Pace>,
    pending_buckets: Arc<Mutex<PendingBuckets>>,
    round_updates: watch::Sender<u64>,
    rounds: Mutex<Option<JoinHandle<()>>>,
    /// Searches under way, at most [`MAX_SEARCHES`]: each one sends many
    /// requests and may spend tokens, and a public node's search page is
    /// open to anyone.
    searches: Arc<tokio::sync::Semaphore>,
    task: Mutex<Option<JoinHandle<()>>>,
}

/// This node's credits as another node counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditsAt {
    pub credits: i64,
    /// Its crawls count there yet; it gets tokens only once they do.
    pub counts: bool,
}

/// Which connected nodes a [`Command::Peers`] asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Serving {
    Buckets,
    Reports,
}

/// Where the answer to a fill request goes.
type FillReply = oneshot::Sender<Result<Option<FillPage>>>;

/// Where the answer to a page set request goes.
type PagesReply = oneshot::Sender<Result<Option<PagesChunk>>>;
type ProfileReply = oneshot::Sender<Result<ProfileResponse>>;

enum Command {
    Publish {
        records: Vec<SiteRecord>,
        reply: oneshot::Sender<Result<Option<Hash>>>,
    },
    Peers(Serving, oneshot::Sender<Vec<BucketPeer>>),
    Recount(oneshot::Sender<Arc<PopularityTable>>),
    Rechecks(usize, oneshot::Sender<Vec<String>>),
    Counting(Vec<String>, oneshot::Sender<HashSet<String>>),
    Oblivious(ObliviousRequest, oneshot::Sender<ObliviousResponse>),
    AskTokens {
        issuer: PeerId,
        wanted: usize,
        reply: oneshot::Sender<Result<usize>>,
    },
    AskCredits(PeerId, oneshot::Sender<Result<CreditsAt>>),
    /// Nodes that answered bucket requests of ours, once per answer.
    Answered(Vec<PeerId>),
    FillPeers(oneshot::Sender<Vec<PeerId>>),
    Fill {
        prefer: Option<PeerId>,
        from: u64,
        count: u32,
        all: bool,
        reply: FillReply,
    },
    Pages {
        request: PagesRequest,
        reply: PagesReply,
    },
    Profile {
        peer: PeerId,
        request: ProfileRequest,
        reply: ProfileReply,
    },
    Dial(Multiaddr),
    Reconnect,
    Stop,
}

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

    pub fn status(&self) -> NetStatus {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Signs `records` (crawl results) as a batch, keeps it and tells the
    /// network. Records of sites this node was not assigned are sent too but
    /// other nodes ignore them. `None` when there was nothing to sign.
    pub async fn publish(&self, records: Vec<SiteRecord>) -> Result<Option<Hash>> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Publish { records, reply })?;
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
                self.pending_buckets
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .queue(refresh);
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
        self.pending_buckets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
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
                    self.status
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .reports_sent += 1;
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
    /// [`RECOUNT_MINUTES`] minutes while new reports come in.
    pub fn popularity(&self) -> Arc<PopularityTable> {
        self.popularity
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
    /// [`MAX_ISSUE`]), paid with the credits it counts for this node, and
    /// keeps them. Returns how many it signed.
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
    /// `offset` of its file of the page set `set` (see [`crate::pages`]).
    /// `None` when no trusted node that serves page sets is connected.
    pub async fn pages_chunk(
        &self,
        set: &str,
        offset: u64,
        len: u32,
    ) -> Result<Option<PagesChunk>> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Pages {
            request: PagesRequest {
                set: set.to_string(),
                offset,
                len,
            },
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
        self.wallet
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .held(issuer)
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
        if let Some(rounds) = self
            .rounds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            rounds.abort();
        }
        let _ = self.commands.send(Command::Stop);
        let task = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
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

/// Loads the node key from `path`, or makes one and saves it there. The
/// key is the node's identity: its peer id, and the signature on its
/// batches.
pub fn load_or_create_key(path: &std::path::Path) -> Result<Keypair> {
    match std::fs::read(path) {
        Ok(bytes) => Keypair::from_protobuf_encoding(&bytes)
            .with_context(|| format!("{} is not a node key", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let key = Keypair::generate_ed25519();
            let bytes = key
                .to_protobuf_encoding()
                .context("encoding the node key")?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let tmp = path.with_extension("tmp");
            write_private(&tmp, &bytes)?;
            std::fs::rename(&tmp, path).with_context(|| format!("saving {}", path.display()))?;
            info!("made a new node key in {}", path.display());
            Ok(key)
        }
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", path.display()))
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    /// First, so a connection past the limits is turned away before any
    /// other behaviour sees it.
    limits: connection_limits::Behaviour,
    relay_client: relay::client::Behaviour,
    relay: Toggle<relay::Behaviour>,
    dcutr: dcutr::Behaviour,
    autonat: autonat::Behaviour,
    upnp: Toggle<upnp::tokio::Behaviour>,
    mdns: Toggle<mdns::tokio::Behaviour>,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    kad: kad::Behaviour<kad::store::MemoryStore>,
    gossipsub: gossipsub::Behaviour,
    buckets: request_response::cbor::Behaviour<BucketRequest, BucketResponse>,
    batches: request_response::cbor::Behaviour<BatchRequest, BatchResponse>,
    reports: request_response::cbor::Behaviour<ReportRequest, ReportResponse>,
    oblivious: request_response::cbor::Behaviour<ObliviousRequest, ObliviousResponse>,
    credits: request_response::cbor::Behaviour<CreditRequest, CreditResponse>,
    fill: request_response::cbor::Behaviour<FillRequest, FillResponse>,
    pages: request_response::cbor::Behaviour<PagesRequest, PagesResponse>,
    trust: request_response::cbor::Behaviour<TrustRequest, TrustResponse>,
    profile: request_response::cbor::Behaviour<ProfileRequest, ProfileResponse>,
}

/// Starts the network side of a node. Returns its handle and the records
/// accepted from other nodes' batches, one batch at a time. Records that
/// carry headlines ([`SiteRecord::news`]) carry nothing else: they are
/// trusted crawlers' feed checks, for the node's headline store.
pub async fn start(
    config: NetConfig,
    source: Arc<dyn BucketSource>,
) -> Result<(NetHandle, mpsc::UnboundedReceiver<Vec<SiteRecord>>)> {
    let key = load_or_create_key(&config.dir.join("node.key"))?;
    let (reports, table) = {
        let dir = config.dir.clone();
        tokio::task::spawn_blocking(move || {
            let now = now_unix();
            let reports = ReportStore::open(&dir.join("reports"), now)?;
            let table = reports.table(now);
            anyhow::Ok((reports, table))
        })
        .await
        .context("opening the report store")??
    };
    let peer_id = key.public().to_peer_id();
    // The network's own first nodes start from the same default list as
    // everyone else, which names them.
    let mut config = config;
    config
        .bootstrap
        .retain(|addr| peer_of(addr) != Some(peer_id));
    let gateway = Gateway::new(&key, now_unix())?;
    // Crawlers trusted since the last start: the batches held from them
    // were taken in without their text, so they are sent again whole.
    let trust_file = config.dir.join(TRUST_APPLIED_FILE);
    let newly_trusted = newly_trusted(&trust_file, peer_id, &config.trusted_peers);
    let (store, agreement, replay) = {
        let dir = config.dir.join("batches");
        let trusted = config.trusted_peers.clone();
        let newly = newly_trusted.clone();
        tokio::task::spawn_blocking(move || -> Result<_> {
            let store = BatchStore::open(&dir)?;
            let (agreement, replay) = replay_agreement(&store, peer_id, &trusted, &newly);
            Ok((store, agreement, replay))
        })
        .await
        .context("opening the batch store")??
    };
    let (ledger, issuer, wallet) = {
        let dir = config.dir.join("credits");
        tokio::task::spawn_blocking(move || -> Result<_> {
            Ok((
                Ledger::open(&dir)?,
                Issuer::open(&dir)?,
                Wallet::open(&dir)?,
            ))
        })
        .await
        .context("opening the credits")??
    };
    let wallet = Arc::new(Mutex::new(wallet));
    let tokens_spent = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut swarm = build_swarm(&key, &config)?;
    for addr in &config.listen {
        if let Err(err) = swarm.listen_on(addr.clone()) {
            warn!("cannot listen on {addr}: {err}");
        }
    }
    for addr in &config.external {
        swarm.add_external_address(addr.clone());
    }
    let topic = gossipsub::IdentTopic::new(BATCH_TOPIC);
    swarm
        .behaviour_mut()
        .gossipsub
        .subscribe(&topic)
        .context("subscribing to the batch topic")?;
    let report_topic = gossipsub::IdentTopic::new(REPORT_TOPIC);
    swarm
        .behaviour_mut()
        .gossipsub
        .subscribe(&report_topic)
        .context("subscribing to the report topic")?;

    let status = Arc::new(Mutex::new(NetStatus {
        peer_id: peer_id.to_string(),
        nat: "unknown".into(),
        batches_held: store.len(),
        reports_held: reports.len(),
        popular_picks: table.picks.len(),
        agreement: agreement.status(),
        crawlers: crawler_views(&store, peer_id, &config.trusted_peers, now_unix()),
        search_scope: config.search_scope,
        ..NetStatus::default()
    }));
    let popularity = Arc::new(RwLock::new(Arc::new(table)));
    let (commands, commands_rx) = mpsc::unbounded_channel();
    let (records_tx, records_rx) = mpsc::unbounded_channel();
    let store = Arc::new(Mutex::new(store));
    {
        let (store, records, trusted) = (
            store.clone(),
            records_tx.clone(),
            config.trusted_peers.clone(),
        );
        tokio::task::spawn_blocking(move || {
            resend_trusted(&store, &replay, &records, &trust_file, &trusted)
        });
    }
    let (answers_tx, answers_rx) = mpsc::unbounded_channel();
    info!("network node {peer_id} starting");
    let mut task = Task {
        swarm,
        key: key.clone(),
        config: config.clone(),
        topic,
        report_topic,
        store: store.clone(),
        reports: Arc::new(Mutex::new(reports)),
        popularity: popularity.clone(),
        recount: false,
        report_peers: HashMap::new(),
        report_listing: HashSet::new(),
        report_lists: HashSet::new(),
        source,
        status: status.clone(),
        records: records_tx,
        agreement,
        ledger,
        issuer,
        wallet: wallet.clone(),
        tokens_spent: tokens_spent.clone(),
        tokens_issued: 0,
        priority_answered: 0,
        allowance: Allowance::new(config.answer_per_day),
        credits_at: HashMap::new(),
        balance_asks: HashMap::new(),
        token_asks: HashMap::new(),
        asking: HashMap::new(),
        answers_tx,
        bucket_peers: HashMap::new(),
        batch_peers: HashSet::new(),
        relays: HashMap::new(),
        remote_addrs: HashMap::new(),
        circuits: HashMap::new(),
        reserved: HashSet::new(),
        nearby: HashSet::new(),
        wanted: VecDeque::new(),
        wanted_ids: HashSet::new(),
        refused: HashSet::new(),
        fetching: HashMap::new(),
        listing: HashSet::new(),
        unannounced: Vec::new(),
        answering: 0,
        fill_peers: HashSet::new(),
        filling: 0,
        fill_asked: HashMap::new(),
        fill_asking: HashMap::new(),
        pages_peers: HashSet::new(),
        pages_serving: 0,
        lists_serving: 0,
        pages_asked: HashMap::new(),
        pages_asking: HashMap::new(),
        profile_serving: 0,
        profile_asking: HashMap::new(),
        gateway,
        oblivious_peers: HashSet::new(),
        relay_keys: HashMap::new(),
        waiting_keys: HashMap::new(),
        relaying: HashMap::new(),
        bootstrap_peers: config.bootstrap.iter().filter_map(peer_of).collect(),
        problem: None,
        alone_since: Some(now_unix()),
        friends: Friends::open(&config.dir.join(FRIENDS_FILE)),
        trust_listing: HashSet::new(),
    };
    for addr in &config.bootstrap {
        task.dial(addr.clone());
    }
    let handle = tokio::spawn(task.run(commands_rx, answers_rx));
    let cache = Arc::new(crate::cache::BucketCache::open(
        &config.dir.join("bucket-cache"),
        now_unix(),
    ));
    // Buckets kept from nodes a narrower scope no longer asks would still
    // answer searches, so a change of scope starts the cache afresh.
    let scope_file = config.dir.join(SCOPE_FILE);
    // Nodes from before scopes asked anyone.
    let last_scope = std::fs::read_to_string(&scope_file)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(SearchScope::Anyone);
    if last_scope != config.search_scope || !scope_file.exists() {
        if last_scope != config.search_scope {
            info!(
                "searches now ask {}: forgetting fetched buckets",
                config.search_scope
            );
            cache.clear();
        }
        if let Err(err) = std::fs::write(&scope_file, config.search_scope.as_str()) {
            warn!("writing {}: {err}", scope_file.display());
        }
    }
    let pace = Arc::new(Pace::new(config.round_every));
    status
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .rounds
        .every_secs = pace.every().map(|e| e.as_secs());
    let pending_buckets = Arc::new(Mutex::new(PendingBuckets::default()));
    let (round_updates, _) = watch::channel(0);
    let rounds = pace.every().is_some().then(|| {
        tokio::spawn(background_rounds(
            pace.clone(),
            commands.clone(),
            wallet.clone(),
            cache.clone(),
            status.clone(),
            pending_buckets.clone(),
            round_updates.clone(),
            tokens_spent.clone(),
        ))
    });
    Ok((
        NetHandle {
            peer_id,
            share_ppm: config.share_ppm.min(MAX_SHARE_PPM),
            trusted: config.trusted_peers.clone(),
            commands,
            status,
            popularity,
            wallet,
            tokens_spent,
            cache,
            pace,
            pending_buckets,
            round_updates,
            rounds: Mutex::new(rounds),
            searches: Arc::new(tokio::sync::Semaphore::new(MAX_SEARCHES)),
            task: Mutex::new(Some(handle)),
        },
        records_rx,
    ))
}

/// Reads the found sites' signed crawls with this node's trust list: a
/// crawl a trusted node signed counts whole, as its batches do (any site,
/// headings and text included), so the site is shown and ranked with that
/// crawl's text, and [`crate::search::FoundSite::shared`] carries all of it for this node
/// to keep. Other sites keep what [`crate::search`] checked.
fn keep_trusted_crawls(found: &mut NetSearch, trusted: &[PeerId], now: u64) {
    for site in &mut found.found {
        let Some(proof) = &site.proof else {
            continue;
        };
        let Ok(crawler) = proof.check_signed(now) else {
            continue;
        };
        if !trusted.contains(&crawler) {
            continue;
        }
        let Ok((signed, _)) = proof.verify_trusted(now) else {
            continue;
        };
        if signed.domain != site.record.domain {
            continue;
        }
        let record = &mut site.record;
        record.url = signed.url.clone();
        record.title = signed.title.clone();
        record.description = signed.description.clone();
        record.headings = signed.headings.clone();
        record.body_text = signed.body_text.clone();
        record.key_pages = signed.key_pages.clone();
        record.links_to = signed.links_to.clone();
        record.crawled_at = signed.crawled_at;
        if !site.verified {
            site.verified = true;
            site.crawler = Some(crawler.to_string());
            if !site.crawlers.contains(&crawler.to_string()) {
                site.crawlers.insert(0, crawler.to_string());
            }
        }
        site.shared = Some(signed);
        site.trusted = true;
    }
}

/// Fixed absolute deadlines, with no query wakeup or catch-up bursts. A slow
/// round consumes its slot; ticks while it is in flight are skipped rather than
/// shifting subsequent deadlines. JoinSet aborts in-flight work on shutdown.
#[allow(clippy::too_many_arguments)]
async fn background_rounds(
    pace: Arc<Pace>,
    commands: mpsc::UnboundedSender<Command>,
    wallet: Arc<Mutex<Wallet>>,
    cache: Arc<crate::cache::BucketCache>,
    status: Arc<Mutex<NetStatus>>,
    pending: Arc<Mutex<PendingBuckets>>,
    updates: watch::Sender<u64>,
    tokens_spent: Arc<std::sync::atomic::AtomicU64>,
) {
    let Some(every) = pace.every() else {
        return;
    };
    let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut running = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = commands.closed() => return,
            _ = running.join_next(), if !running.is_empty() => {},
            _ = timer.tick() => {
                if !running.is_empty() {
                    continue;
                }
                let commands = commands.clone();
                let wallet = wallet.clone();
                let cache = cache.clone();
                let status = status.clone();
                let pending = pending.clone();
                let updates = updates.clone();
                let tokens_spent = tokens_spent.clone();
                running.spawn(async move {
                    let (reply, peers) = oneshot::channel();
                    if commands.send(Command::Peers(Serving::Buckets, reply)).is_err() {
                        return;
                    }
                    let Ok(peers) = peers.await else { return; };
                    if peers.is_empty() {
                        return; // Queued IDs stay queued until a later due slot.
                    }
                    let (generation, queued) = pending.lock().unwrap_or_else(PoisonError::into_inner).take_round();
                    let found = crate::search::background_round_for(
                        &peers, ROUND_WAIT, now_unix(), Some(&wallet), &cache, queued.clone()
                    ).await;
                    tokens_spent.fetch_add(found.priority as u64, std::sync::atomic::Ordering::Relaxed);
                    if !found.answered_by.is_empty() {
                        let _ = commands.send(Command::Answered(found.answered_by.clone()));
                    }
                    count_round(&status, &found);
                    let now = now_unix();
                    let retry = queued.iter().copied()
                        .filter(|&bucket| !crate::search::bucket_ready(&cache, bucket, now))
                        .collect();
                    pending.lock().unwrap_or_else(PoisonError::into_inner).finish(generation, &queued, retry);
                    updates.send_modify(|generation| *generation = generation.wrapping_add(1));
                });
            }
        }
    }
}

/// How long a background round waits for its answers: as long as a
/// search on a node's web page does, so it holds its connections open as
/// long.
const ROUND_WAIT: Duration = Duration::from_secs(4);

fn count_round(status: &Mutex<NetStatus>, round: &NetSearch) {
    let mut status = status.lock().unwrap_or_else(PoisonError::into_inner);
    let rounds = &mut status.rounds;
    rounds.sent += 1;
    rounds.answers += round.answered as u64;
}

fn build_swarm(key: &Keypair, config: &NetConfig) -> Result<Swarm<Behaviour>> {
    let peer_id = key.public().to_peer_id();
    let relay_server = config.relay_server;
    let upnp = config.upnp;
    let local_discovery = config.local_discovery;
    // Buckets are only ever asked for by throwaway swarms (crate::search),
    // so this one only answers, and only when it serves buckets at all.
    let bucket_protocols: Vec<(StreamProtocol, ProtocolSupport)> = config
        .answer_searches
        .then(|| {
            (
                StreamProtocol::new(BUCKET_PROTOCOL),
                ProtocolSupport::Inbound,
            )
        })
        .into_iter()
        .collect();
    // A node that answers searches also relays sealed ones for others, and
    // answers those sealed to it (crate::oblivious).
    let oblivious_protocols: Vec<(StreamProtocol, ProtocolSupport)> = config
        .answer_searches
        .then(|| {
            (
                StreamProtocol::new(OBLIVIOUS_PROTOCOL),
                ProtocolSupport::Full,
            )
        })
        .into_iter()
        .collect();
    let swarm = libp2p::SwarmBuilder::with_existing_identity(key.clone())
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )
        .context("setting up TCP")?
        .with_quic()
        .with_dns()
        .context("setting up DNS")?
        .with_relay_client(noise::Config::new, yamux::Config::default)
        .context("setting up the relay client")?
        .with_behaviour(|key, relay_client| {
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .validation_mode(gossipsub::ValidationMode::Strict)
                .validate_messages()
                .message_id_fn(|message: &gossipsub::Message| {
                    gossipsub::MessageId::from(Hash::of(&[&message.data]).to_hex())
                })
                .max_transmit_size(16 * 1024)
                .build()
                .map_err(|err| anyhow::anyhow!("gossipsub config: {err}"))?;
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )
            .map_err(|err| anyhow::anyhow!("gossipsub: {err}"))?;
            let mut kad = kad::Behaviour::with_config(
                peer_id,
                kad::store::MemoryStore::new(peer_id),
                kad::Config::new(StreamProtocol::new(KAD_PROTOCOL)),
            );
            if relay_server {
                kad.set_mode(Some(kad::Mode::Server));
            }
            let relay = relay_server.then(|| {
                let mut config = relay::Config {
                    max_reservations: 1024,
                    max_circuits: 256,
                    // Circuits to or from one node. A node behind NAT
                    // gets every network search through its relay, each
                    // bucket on its own throwaway connection, so the
                    // default of 4 turns searches away.
                    max_circuits_per_peer: 64,
                    circuit_src_rate_limiters: Vec::new(),
                    ..relay::Config::default()
                }
                .circuit_src_per_peer(CIRCUIT_BURST, CIRCUIT_REFILL);
                // libp2p's default lets one address open 60 circuits and
                // then one a minute. Every bucket of a search is a circuit
                // of its own, and a relay passes on sealed requests for
                // everyone, so that cut searches off after a handful. Not
                // limited at all from this machine: the relay reaches the
                // nodes relaying through it over loopback for its own
                // searches and the sealed requests it passes on.
                let mut per_ip = relay::Config {
                    circuit_src_rate_limiters: Vec::new(),
                    ..relay::Config::default()
                }
                .circuit_src_per_ip(CIRCUIT_BURST, CIRCUIT_REFILL)
                .circuit_src_rate_limiters;
                config.circuit_src_rate_limiters.push(Box::new(
                    move |peer, addr: &Multiaddr, now| {
                        is_loopback(addr) || per_ip.iter_mut().all(|l| l.try_next(peer, addr, now))
                    },
                ));
                relay::Behaviour::new(peer_id, config)
            });
            let request_config =
                request_response::Config::default().with_request_timeout(Duration::from_secs(20));
            Ok(Behaviour {
                limits: connection_limits::Behaviour::new(connection_limits()),
                relay_client,
                relay: relay.into(),
                dcutr: dcutr::Behaviour::new(peer_id),
                autonat: autonat::Behaviour::new(peer_id, autonat::Config::default()),
                upnp: upnp.then(upnp::tokio::Behaviour::default).into(),
                mdns: if local_discovery {
                    match mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id) {
                        Ok(mdns) => Some(mdns),
                        Err(err) => {
                            warn!("no local discovery: {err}");
                            None
                        }
                    }
                } else {
                    None
                }
                .into(),
                identify: identify::Behaviour::new(
                    identify::Config::new(IDENTIFY_PROTOCOL.into(), key.public())
                        .with_agent_version(format!("plumb/{}", env!("CARGO_PKG_VERSION")))
                        // A node behind NAT gets its relayed address only
                        // after the first exchange; tell connected nodes.
                        .with_push_listen_addr_updates(true),
                ),
                ping: ping::Behaviour::default(),
                kad,
                gossipsub,
                buckets: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(1024)
                        .set_response_size_maximum(64 * 1024 * 1024),
                    bucket_protocols,
                    request_config.clone(),
                ),
                batches: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(4 * 1024)
                        .set_response_size_maximum(MAX_BATCH_RESPONSE),
                    [(StreamProtocol::new(BATCH_PROTOCOL), ProtocolSupport::Full)],
                    request_config.clone(),
                ),
                reports: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(8 * 1024)
                        .set_response_size_maximum(MAX_REPORT_RESPONSE),
                    [(StreamProtocol::new(REPORT_PROTOCOL), ProtocolSupport::Full)],
                    request_config.clone(),
                ),
                oblivious: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(2 * REPORT_REQUEST_SIZE as u64)
                        .set_response_size_maximum(MAX_MESSAGE as u64 + 1024),
                    oblivious_protocols,
                    request_config.clone(),
                ),
                credits: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(16 * 1024)
                        .set_response_size_maximum(16 * 1024),
                    [(StreamProtocol::new(CREDIT_PROTOCOL), ProtocolSupport::Full)],
                    request_config.clone(),
                ),
                fill: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(1024)
                        .set_response_size_maximum(64 * 1024 * 1024),
                    [(StreamProtocol::new(FILL_PROTOCOL), ProtocolSupport::Full)],
                    request_config.clone(),
                ),
                pages: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(1024)
                        .set_response_size_maximum(
                            u64::from(crate::pages::MAX_PAGES_CHUNK) + 64 * 1024,
                        ),
                    [(StreamProtocol::new(PAGES_PROTOCOL), ProtocolSupport::Full)],
                    request_config.clone(),
                ),
                trust: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(64)
                        .set_response_size_maximum(64 * 1024),
                    [(StreamProtocol::new(TRUST_PROTOCOL), ProtocolSupport::Full)],
                    request_config.clone(),
                ),
                profile: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(MAX_PROFILE_MESSAGE)
                        .set_response_size_maximum(MAX_PROFILE_MESSAGE),
                    [(StreamProtocol::new(PROFILE_PROTOCOL), ProtocolSupport::Full)],
                    request_config,
                ),
            })
        })
        .map_err(|err| anyhow::anyhow!("setting up the network: {err}"))?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
        .build();
    Ok(swarm)
}

/// Work done off the swarm task whose result goes back to a remote node.
enum Answer {
    Bucket(ResponseChannel<BucketResponse>, BucketResponse),
    Batch(ResponseChannel<BatchResponse>, BatchResponse),
    Report(ResponseChannel<ReportResponse>, ReportResponse),
    Fill(ResponseChannel<FillResponse>, FillResponse),
    Pages(ResponseChannel<PagesResponse>, PagesResponse),
    Profile(ResponseChannel<ProfileResponse>, ProfileResponse),
    /// Picks counted in a recount of the reports.
    Popularity(usize),
    Sealed(Reply, ObliviousResponse),
}

/// Whoever waits for an answer on `/plumb/oblivious/1`: a remote node, or
/// this node's own [`NetHandle`].
enum Reply {
    Remote(ResponseChannel<ObliviousResponse>),
    Local(oneshot::Sender<ObliviousResponse>),
}

/// Who a bucket request comes from, as far as this node can tell.
#[derive(Debug, Clone, Copy)]
enum Asker {
    /// This node's own front end, for its own searchers.
    Own,
    /// A relay passing on a sealed request: could be anyone's.
    Relay(PeerId),
    /// The one who asks, by address when known.
    From(Option<Source>),
}

/// What this node asked another for on `/plumb/credits/1`.
enum Asking {
    /// For tokens; `None` when the node tops up its own wallet.
    Tokens(PeerId, Pending, Option<oneshot::Sender<Result<usize>>>),
    Credits(oneshot::Sender<Result<CreditsAt>>),
    /// For our balance there, for the status page.
    Balance(PeerId),
}

/// A request this node passed on as a relay.
enum Relayed {
    /// For the key of this node; the askers wait in `waiting_keys`.
    Keys(PeerId),
    Forward(Reply),
}

struct Task {
    swarm: Swarm<Behaviour>,
    key: Keypair,
    config: NetConfig,
    topic: gossipsub::IdentTopic,
    report_topic: gossipsub::IdentTopic,
    store: Arc<Mutex<BatchStore>>,
    reports: Arc<Mutex<ReportStore>>,
    popularity: Arc<RwLock<Arc<PopularityTable>>>,
    /// New reports came in since the last count.
    recount: bool,
    /// Connected nodes that take reports, and the addresses they listen on.
    report_peers: HashMap<PeerId, Vec<Multiaddr>>,
    /// Connected nodes asked for their reports.
    report_listing: HashSet<PeerId>,
    /// Those requests not answered yet (see [`MAX_REPORT_LISTS`]).
    report_lists: HashSet<OutboundRequestId>,
    source: Arc<dyn BucketSource>,
    status: Arc<Mutex<NetStatus>>,
    records: mpsc::UnboundedSender<Vec<SiteRecord>>,
    /// Crawls held until a second crawler agrees.
    agreement: Agreement,
    /// Every crawler's credits, as this node counts them.
    ledger: Ledger,
    /// This node's key for signing tokens, and the tokens spent with it.
    issuer: Issuer,
    /// Tokens other nodes signed for this node.
    wallet: Arc<Mutex<Wallet>>,
    tokens_spent: Arc<std::sync::atomic::AtomicU64>,
    tokens_issued: u64,
    /// Bucket requests answered while busy because they spent a token.
    priority_answered: u64,
    /// What this node answers for free (see [`crate::allowance`]).
    allowance: Allowance,
    /// Our credits at each node we search, as it last told us, and when we
    /// last asked.
    credits_at: HashMap<PeerId, CreditsAt>,
    balance_asks: HashMap<PeerId, u64>,
    /// When we last asked each node for tokens.
    token_asks: HashMap<PeerId, u64>,
    /// Our requests on `/plumb/credits/1` not yet answered.
    asking: HashMap<OutboundRequestId, Asking>,
    answers_tx: mpsc::UnboundedSender<Answer>,
    /// Connected nodes that serve buckets, and the addresses they listen on.
    bucket_peers: HashMap<PeerId, Vec<Multiaddr>>,
    /// Connected nodes that serve batches.
    batch_peers: HashSet<PeerId>,
    /// Relays we asked for a reservation, and whether it was granted.
    relays: HashMap<PeerId, bool>,
    /// The address of each connected node, as we reached it or it reached us.
    remote_addrs: HashMap<PeerId, Multiaddr>,
    /// Connected nodes reached through a relay, and that relay.
    circuits: HashMap<PeerId, PeerId>,
    /// Nodes that hold a reservation with us (when we are a relay).
    reserved: HashSet<PeerId>,
    /// Nodes we reached over a home-network or loopback address (directly
    /// or through a relay there), which may use the same kind.
    nearby: HashSet<PeerId>,
    /// Batches to fetch, and from whom.
    wanted: VecDeque<(Hash, Vec<PeerId>)>,
    wanted_ids: HashSet<Hash>,
    /// Batches fetched and found useless or bad, so not fetched again.
    refused: HashSet<Hash>,
    fetching: HashMap<OutboundRequestId, (Hash, Vec<PeerId>)>,
    /// Nodes asked for their batch headers since they last connected.
    listing: HashSet<PeerId>,
    /// Our own headers not yet announced to anyone.
    unannounced: Vec<SignedHeader>,
    answering: usize,
    /// Connected nodes this node trusts that answer fill requests.
    fill_peers: HashSet<PeerId>,
    /// Fill requests being answered.
    filling: usize,
    /// Fill requests answered for each node, and the minute counted.
    fill_asked: HashMap<PeerId, (u64, u32)>,
    /// Our fill requests not yet answered, and whether each asked for
    /// every site.
    fill_asking: HashMap<OutboundRequestId, (bool, FillReply)>,
    /// Connected nodes this node trusts that serve page sets.
    pages_peers: HashSet<PeerId>,
    /// What this node's trusted nodes trust, for searches that ask friends
    /// of friends (see [`crate::scope`]).
    friends: Friends,
    /// Trusted nodes asked what they trust since they last connected.
    trust_listing: HashSet<PeerId>,
    /// Page set requests being answered.
    pages_serving: usize,
    /// Batch and report requests being answered (see [`MAX_LISTS_SERVING`]).
    lists_serving: usize,
    /// Page set requests answered for each node, and the minute counted.
    pages_asked: HashMap<PeerId, (u64, u32)>,
    /// Our page set requests not yet answered.
    pages_asking: HashMap<OutboundRequestId, PagesReply>,
    /// Profile requests being answered (see [`MAX_PROFILE_SERVING`]).
    profile_serving: usize,
    /// Our profile requests not yet answered.
    profile_asking: HashMap<OutboundRequestId, ProfileReply>,
    /// This node's keys for sealed requests.
    gateway: Gateway,
    /// Connected nodes that relay and answer sealed requests.
    oblivious_peers: HashSet<PeerId>,
    /// As a relay: other nodes' keys, and when we fetched them.
    relay_keys: HashMap<PeerId, (SignedKeys, u64)>,
    /// As a relay: who waits for a node's keys we asked it for.
    waiting_keys: HashMap<PeerId, Vec<Reply>>,
    /// As a relay: requests passed on and not yet answered.
    relaying: HashMap<OutboundRequestId, Relayed>,
    /// The peer ids the bootstrap addresses name.
    bootstrap_peers: HashSet<PeerId>,
    /// Why a bootstrap node could not be reached, until a node connects.
    problem: Option<JoinProblem>,
    /// Since when no node is connected.
    alone_since: Option<u64>,
}

impl Task {
    async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut answers: mpsc::UnboundedReceiver<Answer>,
    ) {
        let mut maintenance = tokio::time::interval(Duration::from_secs(60));
        maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ticks: u64 = 0;
        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => self.on_event(event),
                command = commands.recv() => match command {
                    Some(Command::Stop) | None => break,
                    Some(command) => self.on_command(command),
                },
                Some(answer) = answers.recv() => self.on_answer(answer),
                _ = maintenance.tick() => {
                    self.maintain(ticks);
                    ticks += 1;
                }
            }
            self.update_status();
        }
        if let Err(err) = self.ledger.save() {
            warn!("cannot save the credits ledger: {err:#}");
        }
        info!("network node stopped");
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Publish { records, reply } => {
                let _ = reply.send(self.publish(records));
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
            Command::Pages { request, reply } => {
                let Some(peer) = self.pages_peers.iter().next().copied() else {
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

    /// Signs and announces `records` as batches of this node's crawls: one
    /// per epoch the homepages were crawled in (records not crawled go with
    /// the current one), at most [`MAX_BATCH_RECORDS`] each. Crawls too old
    /// for other nodes to take are left out. Returns the last batch's id.
    fn publish(&mut self, records: Vec<SiteRecord>) -> Result<Option<Hash>> {
        let now = now_unix();
        let mut last = None;
        for (epoch, records) in by_crawl_epoch(records, now) {
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
        self.agreement
            .observe(me, accept_own_batch(&batch, &me, now), now);
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

    fn announce(&mut self, header: SignedHeader) {
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

    fn on_answer(&mut self, answer: Answer) {
        match answer {
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

    fn dial(&mut self, addr: Multiaddr) {
        if let Err(err) = self.swarm.dial(addr.clone()) {
            warn!("cannot dial {addr}: {err}");
        }
    }

    /// Once a minute: keep enough connections, look for more nodes, retry
    /// announcements and fetches, prune old batches.
    fn maintain(&mut self, ticks: u64) {
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
        if ticks.is_multiple_of(60) {
            let now = now_unix();
            self.lock_store()
                .prune_keeping(now, self.config.keep_batches_days);
            self.agreement.prune(now);
            let agreement = self.agreement.status();
            self.with_status(|s| s.agreement = agreement);
            self.lock_reports().prune(now);
            // A new week makes last week's count stale.
            self.recount = true;
        }
        if ticks > 0 && ticks.is_multiple_of(RELIST_MINUTES) {
            let since = epoch_of(now).saturating_sub(1);
            let peers: Vec<PeerId> = self.batch_peers.iter().copied().collect();
            for peer in peers {
                self.swarm
                    .behaviour_mut()
                    .batches
                    .send_request(&peer, BatchRequest::List { since_epoch: since });
            }
        }
        if self.recount && ticks.is_multiple_of(RECOUNT_MINUTES) {
            self.recount = false;
            self.count_reports(None);
        }
        self.fetch_more();
    }

    /// Counts the reports held, off the swarm task, and saves the table.
    fn count_reports(&self, reply: Option<oneshot::Sender<Arc<PopularityTable>>>) {
        let reports = self.reports.clone();
        let popularity = self.popularity.clone();
        let path = self.config.dir.join(POPULARITY_FILE);
        let tx = self.answers_tx.clone();
        tokio::task::spawn_blocking(move || {
            // Counted with the store let go: STAR counting takes a while.
            let held = reports
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .snapshots(now_unix());
            let table = crate::reports::count(held);
            if let Err(err) = table.save(&path) {
                warn!("cannot save the popularity table: {err:#}");
            }
            let picks = table.picks.len();
            info!("counted the popularity reports: {picks} picks");
            let table = Arc::new(table);
            *popularity.write().unwrap_or_else(PoisonError::into_inner) = table.clone();
            if let Some(reply) = reply {
                let _ = reply.send(table);
            }
            let _ = tx.send(Answer::Popularity(picks));
        });
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
                    self.oblivious_peers.remove(&peer_id);
                    self.batch_peers.remove(&peer_id);
                    self.fill_peers.remove(&peer_id);
                    self.pages_peers.remove(&peer_id);
                    self.pages_asked.remove(&peer_id);
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
            }
            BehaviourEvent::Buckets(event) => self.on_bucket_event(event),
            BehaviourEvent::Batches(event) => self.on_batch_event(event),
            BehaviourEvent::Reports(event) => self.on_report_event(event),
            BehaviourEvent::Oblivious(event) => self.on_oblivious_event(event),
            BehaviourEvent::Credits(event) => self.on_credit_event(event),
            BehaviourEvent::Fill(event) => self.on_fill_event(event),
            BehaviourEvent::Pages(event) => self.on_pages_event(event),
            BehaviourEvent::Trust(event) => self.on_trust_event(event),
            BehaviourEvent::Profile(event) => self.on_profile_event(event),
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
        if supports(OBLIVIOUS_PROTOCOL) {
            self.oblivious_peers.insert(peer);
        }
        if supports(FILL_PROTOCOL) && self.config.trusted_peers.contains(&peer) {
            self.fill_peers.insert(peer);
        }
        if supports(PAGES_PROTOCOL) && self.config.trusted_peers.contains(&peer) {
            self.pages_peers.insert(peer);
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
                self.swarm
                    .behaviour_mut()
                    .batches
                    .send_request(&peer, BatchRequest::List { since_epoch: since });
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

    /// A batch header heard on the gossip topic: check it, and fetch the
    /// batch if it is new to us.
    fn on_header(&mut self, from: PeerId, data: &[u8]) -> gossipsub::MessageAcceptance {
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
        if self.refused.len() >= MAX_REFUSED {
            self.refused.clear();
        }
        self.refused.insert(id);
    }

    fn want(&mut self, id: Hash, sources: Vec<PeerId>) {
        // A full queue drops the newcomer: a real batch is heard of again
        // (gossip, catch-up), and fresh keys can't grow the queue forever.
        if self.wanted.len() >= MAX_WANTED
            || self.refused.contains(&id)
            || self.lock_store().contains(&id)
            || !self.wanted_ids.insert(id)
        {
            return;
        }
        self.wanted.push_back((id, sources));
        self.fetch_more();
    }

    fn fetch_more(&mut self) {
        while self.fetching.len() < MAX_FETCHES {
            let Some((id, mut sources)) = self.wanted.pop_front() else {
                return;
            };
            sources.retain(|p| self.swarm.is_connected(p) && *p != self.key.public().to_peer_id());
            let Some(source) = sources.first().copied() else {
                // Nobody connected has it; forget it until it is heard of again.
                self.wanted_ids.remove(&id);
                continue;
            };
            let request = self
                .swarm
                .behaviour_mut()
                .batches
                .send_request(&source, BatchRequest::Get(id));
            sources.remove(0);
            self.fetching.insert(request, (id, sources));
        }
    }

    fn on_bucket_event(&mut self, event: request_response::Event<BucketRequest, BucketResponse>) {
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
            let response = lookup(&*source, &store, request.bucket);
            let _ = tx.send(Answer::Bucket(channel, response));
        });
    }

    fn on_oblivious_event(
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

    /// A request on `/plumb/oblivious/1`, from another node or our own
    /// handle (see [`crate::oblivious`]).
    fn on_oblivious_request(
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
            let response = lookup(&*source, &store, request.bucket);
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

    fn reply(&mut self, reply: Reply, response: ObliviousResponse) {
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

    /// Tells the swarm where a node we know of listens, before asking it
    /// for something while not connected.
    fn add_known_addresses(&mut self, peer: &PeerId) {
        if self.swarm.is_connected(peer) {
            return;
        }
        for addr in self.bucket_peers.get(peer).cloned().unwrap_or_default() {
            self.swarm.add_peer_address(*peer, addr);
        }
    }

    fn on_batch_event(&mut self, event: request_response::Event<BatchRequest, BatchResponse>) {
        match event {
            request_response::Event::Message {
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => {
                if self.lists_serving >= MAX_LISTS_SERVING {
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
                tokio::task::spawn_blocking(move || {
                    let response = match request {
                        // Read and parsed with the store let go: a batch
                        // can be 16 MB.
                        BatchRequest::Get(id) => {
                            let held = store
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .located(&id);
                            let batch = held.map_or(Ok(None), |path| read_held(&path));
                            BatchResponse::Batch(batch.unwrap_or_else(|err| {
                                warn!("cannot read a batch: {err:#}");
                                None
                            }))
                        }
                        BatchRequest::List { since_epoch } => BatchResponse::Headers(
                            store
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
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
                    let now = now_unix();
                    for header in headers.into_iter().take(MAX_LISTED_BATCHES) {
                        if let Ok(crawler) = header.check(now) {
                            self.want(header.id(), vec![peer, crawler]);
                        }
                    }
                }
                BatchResponse::Batch(batch) => {
                    let Some((id, rest)) = self.fetching.remove(&request_id) else {
                        return;
                    };
                    match batch {
                        Some(batch) if batch.id() == id => self.on_batch(peer, batch),
                        Some(_) => {
                            warn!("{peer} sent a different batch than asked for");
                            self.retry_fetch(id, rest);
                        }
                        None => self.retry_fetch(id, rest),
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
                if let Some((id, rest)) = self.fetching.remove(&request_id) {
                    self.retry_fetch(id, rest);
                    self.fetch_more();
                }
            }
            _ => {}
        }
    }

    fn retry_fetch(&mut self, id: Hash, rest: Vec<PeerId>) {
        if rest.is_empty() {
            self.wanted_ids.remove(&id);
        } else {
            self.wanted.push_back((id, rest));
        }
    }

    fn on_batch(&mut self, from: PeerId, batch: Batch) {
        let id = batch.id();
        self.wanted_ids.remove(&id);
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
        // Written and synced off the swarm task.
        let store = self.store.clone();
        let status = self.status.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = store.lock().unwrap_or_else(PoisonError::into_inner);
            match store.insert(&batch) {
                Ok(()) => {
                    let held = store.len();
                    drop(store);
                    status
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .batches_held = held;
                }
                Err(err) => warn!("cannot keep batch {id}: {err:#}"),
            }
        });
        let kept = accepted.len();
        let confirmed = self.agreement.observe(crawler, accepted, made);
        self.count_credits();
        info!(
            "received batch {id} from {from}: kept {kept} of {lines} records crawled by {crawler}, {} now confirmed by a second crawler",
            confirmed.len()
        );
        let agreement = self.agreement.status();
        self.with_status(|s| {
            s.batches_received += 1;
            s.agreement = agreement;
        });
        if !confirmed.is_empty() {
            let _ = self.records.send(confirmed);
        }
        if !news.is_empty() {
            let _ = self.records.send(news);
        }
    }

    fn lock_store(&self) -> std::sync::MutexGuard<'_, BatchStore> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_reports(&self) -> std::sync::MutexGuard<'_, ReportStore> {
        self.reports.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps a report if it checks out and is new. Returns `None` for one
    /// that does not check out, else whether it was new.
    fn take_report(&mut self, report: &Report) -> Option<bool> {
        if let Err(err) = report.check(now_unix()) {
            debug!("refused a report: {err:#}");
            return None;
        }
        let new = match self.lock_reports().insert(report) {
            Ok(new) => new,
            Err(err) => {
                warn!("cannot keep a report: {err:#}");
                false
            }
        };
        if new {
            self.recount = true;
        }
        Some(new)
    }

    /// A report handed to this node, straight or sealed through a relay:
    /// keeps it and, if new, passes it on. Returns whether it was taken.
    fn take_submitted(&mut self, report: &Report) -> bool {
        let taken = self.take_report(report) == Some(true);
        if taken {
            // Nodes that miss it get it when they next ask for the week's
            // reports.
            let data = serde_json::to_vec(report).expect("reports encode");
            let topic = self.report_topic.clone();
            if let Err(err) = self.swarm.behaviour_mut().gossipsub.publish(topic, data) {
                debug!("cannot pass a report on yet: {err}");
            }
        }
        taken
    }

    /// A report passed on over gossip.
    fn on_gossip_report(&mut self, data: &[u8]) -> gossipsub::MessageAcceptance {
        let Ok(report) = serde_json::from_slice::<Report>(data) else {
            return gossipsub::MessageAcceptance::Reject;
        };
        match self.take_report(&report) {
            Some(_) => gossipsub::MessageAcceptance::Accept,
            None => gossipsub::MessageAcceptance::Reject,
        }
    }

    fn on_report_event(&mut self, event: request_response::Event<ReportRequest, ReportResponse>) {
        match event {
            request_response::Event::Message {
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => match request {
                ReportRequest::Submit(report) => {
                    let taken = self.take_submitted(&report);
                    let _ = self
                        .swarm
                        .behaviour_mut()
                        .reports
                        .send_response(channel, ReportResponse::Taken(taken));
                }
                ReportRequest::List { epoch } => {
                    if self.lists_serving >= MAX_LISTS_SERVING {
                        let _ = self
                            .swarm
                            .behaviour_mut()
                            .reports
                            .send_response(channel, ReportResponse::Reports(Vec::new()));
                        return;
                    }
                    self.lists_serving += 1;
                    let reports = self.reports.clone();
                    let tx = self.answers_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        // Copied out with the store let go.
                        let held = reports
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .snapshot(epoch);
                        let list = crate::reports::list(held, MAX_LISTED_REPORTS);
                        let _ = tx.send(Answer::Report(channel, ReportResponse::Reports(list)));
                    });
                }
            },
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Response {
                        request_id,
                        response,
                    },
                ..
            } => {
                self.report_lists.remove(&request_id);
                let ReportResponse::Reports(reports) = response else {
                    return;
                };
                let mut new = 0;
                for report in reports.iter().take(MAX_LISTED_REPORTS) {
                    if self.take_report(report) == Some(true) {
                        new += 1;
                    }
                }
                if new > 0 {
                    info!("caught up on {new} popularity reports from {peer}");
                }
            }
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                self.report_lists.remove(&request_id);
                debug!("report request to {peer} failed: {error}");
            }
            _ => {}
        }
    }

    /// Whether to answer a bucket request now. Requests from this node's
    /// own front end get in while fewer than `max_answering` plus
    /// [`PRIORITY_SLOTS`] are being answered. Anyone else's get in for free
    /// while fewer than `max_answering` are, and the address or relay it
    /// came from has free answers left (see [`crate::allowance`]); past
    /// that, up to [`PRIORITY_SLOTS`] more, for a token of ours. A token
    /// sent along is spent either way: it is only sent after this node said
    /// it was busy.
    fn admit(&mut self, request: &BucketRequest, asker: Asker) -> bool {
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
    fn source_of(&self, peer: &PeerId) -> Asker {
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
    fn collect_tokens(&mut self, now: u64) {
        let peers: Vec<PeerId> = self.bucket_peers.keys().copied().collect();
        self.ask_balances(&peers, now);
        if !self.config.collect_tokens {
            return;
        }
        for peer in peers {
            let held = self
                .wallet
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .held(&peer);
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
    fn count_credits(&mut self) {
        let verdicts = self.agreement.take_verdicts();
        self.ledger.record(&verdicts);
    }

    /// Whether `crawler` can have tokens here: this node trusts it strictly
    /// ([`Agreement::vouched`], whatever the quorum rules), and it was
    /// judged often enough to have a track record.
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

    /// Whether this node's searches may ask `peer`, under
    /// [`NetConfig::search_scope`].
    fn may_search(&self, peer: &PeerId) -> bool {
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
    fn on_trust_event(&mut self, event: request_response::Event<TrustRequest, TrustResponse>) {
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

    fn on_fill_event(&mut self, event: request_response::Event<FillRequest, FillResponse>) {
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
                let records = response
                    .records
                    .iter()
                    .filter_map(|line| accept(line, now))
                    .collect();
                let _ = reply.send(Ok(Some(FillPage {
                    peer,
                    records,
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

    fn on_pages_event(&mut self, event: request_response::Event<PagesRequest, PagesResponse>) {
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
                        },
                    );
                    return;
                }
                self.pages_serving += 1;
                let source = self.source.clone();
                let tx = self.answers_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let file = source.page_set_file(&request.set);
                    let response = crate::pages::answer(file.as_deref(), &request);
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

    /// Profile requests are answered by the node's front end (see
    /// [`BucketSource::profile`]), which answers only for profiles linked
    /// with the asker; a few at a time.
    fn on_profile_event(
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

    fn on_credit_event(&mut self, event: request_response::Event<CreditRequest, CreditResponse>) {
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
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
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

    fn with_status(&self, change: impl FnOnce(&mut NetStatus)) {
        change(&mut self.status.lock().unwrap_or_else(PoisonError::into_inner));
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
        self.with_status(|s| {
            s.peers = peers;
            s.nearby_peers = nearby_peers;
            s.search_peers = search_peers;
            s.friends_of_friends = friends_of_friends;
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

/// `read` of what `lock` guards, unless another thread holds it now.
fn peek<T, R>(lock: &Mutex<T>, read: impl FnOnce(&T) -> R) -> Option<R> {
    match lock.try_lock() {
        Ok(guard) => Some(read(&guard)),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(read(&poisoned.into_inner())),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

fn refusal(peer: PeerId, answer: Result<CreditResponse>) -> anyhow::Error {
    match answer {
        Ok(CreditResponse::Refused(why)) => anyhow::anyhow!("{peer} refused: {why}"),
        Ok(_) => anyhow::anyhow!("{peer} answered something else"),
        Err(err) => err.context(format!("asking {peer}")),
    }
}

/// Bucket `bucket` of `source`, each site with the proof of its signed
/// crawl when `store` holds one.
fn lookup(source: &dyn BucketSource, store: &Mutex<BatchStore>, bucket: u32) -> BucketResponse {
    let records = source.bucket(bucket).map(|lines| {
        let now = now_unix();
        // Where each site's proofs are is in memory; the batches are read
        // after the lock is let go, so gossip and fetches aren't held up.
        let (sources, mut reader) = {
            let store = store.lock().unwrap_or_else(PoisonError::into_inner);
            let sources: Vec<Vec<(Hash, usize)>> = lines
                .iter()
                .map(|record| {
                    serde_json::from_str::<SiteRecord>(record)
                        .map(|r| store.proof_sources(&r.domain, now))
                        .unwrap_or_default()
                })
                .collect();
            (sources, store.reader())
        };
        lines
            .into_iter()
            .zip(sources)
            .map(|(record, sources)| {
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

/// `records` in runs that fit one batch each: [`MAX_BATCH_RECORDS`] lines,
/// a record with an icon taking two (see [`Batch::sign`]).
fn batch_chunks(records: &[SiteRecord]) -> Vec<&[SiteRecord]> {
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
fn crawler_views(store: &BatchStore, me: PeerId, trusted: &[PeerId], now: u64) -> Vec<CrawlerView> {
    let me = me.to_string();
    let trusted: Vec<String> = trusted.iter().map(ToString::to_string).collect();
    let mut crawlers = store.crawlers(now);
    for view in &mut crawlers {
        view.me = view.peer_id == me;
        view.trusted = trusted.contains(&view.peer_id);
    }
    crawlers
}

/// Also returns the batches of `newly_trusted` crawlers, oldest first.
fn replay_agreement(
    store: &BatchStore,
    me: PeerId,
    trusted: &[PeerId],
    newly_trusted: &[PeerId],
) -> (Agreement, Vec<Hash>) {
    let now = now_unix();
    let mut replay = Vec::new();
    let mut agreement = Agreement::new(me, trusted.iter().copied());
    for id in store.ids_oldest_first() {
        let batch = match store.get(&id) {
            Ok(Some(batch)) => batch,
            Ok(None) => continue,
            Err(err) => {
                warn!("cannot read batch {id}: {err:#}");
                continue;
            }
        };
        // Checked as of when it was made, as it was when it came in.
        let made = batch.header.header.created_at;
        let Ok(crawler) = batch.check(made) else {
            continue;
        };
        let records = if crawler == me {
            accept_own_batch(&batch, &crawler, made)
        } else if trusted.contains(&crawler) {
            accept_trusted_batch(&batch, &crawler, made)
        } else {
            accept_batch(&batch, &crawler, made)
        };
        if newly_trusted.contains(&crawler) {
            replay.push(id);
        }
        agreement.observe(crawler, records, made);
    }
    agreement.prune(now);
    // These were credited when the batches first came in.
    agreement.take_verdicts();
    (agreement, replay)
}

/// The nodes in `trusted` that were not when [`TRUST_APPLIED_FILE`] was
/// last written: all of them on a node that never wrote it.
fn newly_trusted(file: &Path, me: PeerId, trusted: &[PeerId]) -> Vec<PeerId> {
    let applied: HashSet<PeerId> = std::fs::read_to_string(file)
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|id| id.parse().ok())
        .collect();
    trusted
        .iter()
        .filter(|peer| **peer != me && !applied.contains(peer))
        .copied()
        .collect()
}

/// Sends the homepages of the held batches `ids`, as a trusted crawler's
/// count (text included), to `records`, one batch at a time; then notes
/// `trusted` as applied in `file`. Agreement has seen them already: a
/// homepage taken in before without its text is filled in, and a newer
/// crawl of the same site still wins.
fn resend_trusted(
    store: &Mutex<BatchStore>,
    ids: &[Hash],
    records: &mpsc::UnboundedSender<Vec<SiteRecord>>,
    file: &Path,
    trusted: &[PeerId],
) {
    let mut sent = 0;
    for id in ids {
        let batch = match store.lock().unwrap_or_else(PoisonError::into_inner).get(id) {
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
        let homepages: Vec<SiteRecord> = accept_trusted_batch(&batch, &crawler, made)
            .into_iter()
            .filter(|record| record.crawled_at.is_some())
            .collect();
        if homepages.is_empty() {
            continue;
        }
        sent += homepages.len();
        if records.send(homepages).is_err() {
            return;
        }
        // The channel is unbounded: a node with weeks of batches would
        // otherwise read them into memory faster than they are folded in.
        if ids.len() > 1 {
            std::thread::sleep(RESEND_PAUSE);
        }
    }
    if !ids.is_empty() {
        info!(
            "took in {sent} homepages again, with their text, from {} batches of newly trusted nodes",
            ids.len()
        );
    }
    let list: Vec<String> = trusted.iter().map(ToString::to_string).collect();
    if let Err(err) = std::fs::write(file, list.join("\n")) {
        warn!("writing {}: {err}", file.display());
    }
}

/// `records` by the epoch their homepage was crawled in (records not
/// crawled, such as linked sites, in the current one), leaving out crawls
/// too old for other nodes to take and ones from the future.
fn by_crawl_epoch(records: Vec<SiteRecord>, now: u64) -> BTreeMap<u64, Vec<SiteRecord>> {
    let current = epoch_of(now);
    let mut by_epoch: BTreeMap<u64, Vec<SiteRecord>> = BTreeMap::new();
    for record in records {
        let epoch = record.crawled_at.map_or(current, epoch_of);
        if epoch + MAX_BATCH_AGE_EPOCHS > current && epoch <= current {
            by_epoch.entry(epoch).or_default().push(record);
        }
    }
    by_epoch
}

/// Not an unspecified (`0.0.0.0`, `::`) address.
fn is_specific(addr: &Multiaddr) -> bool {
    !addr.iter().any(|p| match p {
        Protocol::Ip4(ip) => ip.is_unspecified(),
        Protocol::Ip6(ip) => ip.is_unspecified(),
        _ => false,
    })
}

/// Reachable from anywhere: not a loopback, private, shared (CGNAT),
/// link-local or unique-local address. Names (`/dns4/...`) count as global.
fn is_global(addr: &Multiaddr) -> bool {
    addr.iter()
        .find_map(|p| match p {
            Protocol::Ip4(ip) => Some(
                !(ip.is_loopback()
                    || ip.is_private()
                    || ip.is_link_local()
                    || ip.is_unspecified()
                    || (ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 64)),
            ),
            Protocol::Ip6(ip) => {
                let first = ip.segments()[0];
                Some(
                    !(ip.is_loopback()
                        || ip.is_unspecified()
                        || first & 0xfe00 == 0xfc00
                        || first & 0xffc0 == 0xfe80),
                )
            }
            _ => None,
        })
        .unwrap_or(true)
}

fn is_loopback(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| match p {
        Protocol::Ip4(ip) => ip.is_loopback(),
        Protocol::Ip6(ip) => ip.is_loopback(),
        _ => false,
    })
}

/// A relayed address whose relay is `relay`.
/// The relay a circuit address goes through, if it is one.
fn relay_of(addr: &Multiaddr) -> Option<PeerId> {
    let mut previous = None;
    for p in addr.iter() {
        if p == Protocol::P2pCircuit {
            return previous;
        }
        previous = match p {
            Protocol::P2p(id) => Some(id),
            _ => None,
        };
    }
    None
}

fn is_circuit_through(addr: &Multiaddr, relay: &PeerId) -> bool {
    relay_of(addr) == Some(*relay)
}

fn without_p2p(addr: Multiaddr) -> Multiaddr {
    addr.into_iter()
        .filter(|p| !matches!(p, Protocol::P2p(_)))
        .collect()
}

/// Whether to dial the bootstrap nodes at maintenance tick `ticks`: every
/// tick while the node has no connections at all, and every
/// [`BOOTSTRAP_REDIAL_MINUTES`] while it has some but none to a bootstrap
/// node. Without that, a node behind NAT that lost its relay while other
/// nodes kept it company was never reachable again.
fn redial_bootstrap(connected: usize, bootstrap_connected: bool, ticks: u64) -> bool {
    connected == 0 || (!bootstrap_connected && ticks.is_multiple_of(BOOTSTRAP_REDIAL_MINUTES))
}

/// Now, in Unix milliseconds.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_batches_of_a_newly_trusted_crawler_are_taken_in_again_with_their_text() {
        let dir = tempfile::tempdir().unwrap();
        let now = now_unix();
        let key = Keypair::generate_ed25519();
        let crawler = key.public().to_peer_id();
        let me = PeerId::random();
        let mut record = SiteRecord::new("shoes.example");
        record.title = Some("Shoes".into());
        record.body_text = Some("Handmade leather shoes".into());
        record.crawled_at = Some(now);
        let batch = Batch::sign(&key, &[record], epoch_of(now), MAX_SHARE_PPM, now)
            .unwrap()
            .unwrap();
        let mut store = BatchStore::open(&dir.path().join("batches")).unwrap();
        store.insert(&batch).unwrap();
        let file = dir.path().join(TRUST_APPLIED_FILE);

        // Trusted since the last start (or never noted): sent again whole.
        let newly = newly_trusted(&file, me, &[me, crawler]);
        assert_eq!(newly, [crawler]);
        let (_, replay) = replay_agreement(&store, me, &[crawler], &newly);
        assert_eq!(replay, [batch.id()]);
        let store = Mutex::new(store);
        let (tx, mut rx) = mpsc::unbounded_channel();
        resend_trusted(&store, &replay, &tx, &file, &[crawler]);
        let sent = rx.try_recv().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].body_text.as_deref(), Some("Handmade leather shoes"));

        // Noted: the next start sends nothing again.
        let newly = newly_trusted(&file, me, &[crawler]);
        assert!(newly.is_empty());
        let store = store.into_inner().unwrap();
        let (_, replay) = replay_agreement(&store, me, &[crawler], &newly);
        assert!(replay.is_empty());
    }

    #[test]
    fn a_search_keeps_the_text_of_crawls_only_from_trusted_nodes() {
        use crate::proto::BucketRecord;
        let now = 1_790_000_000;
        let epoch = epoch_of(now);
        let key = Keypair::generate_ed25519();
        let crawler = key.public().to_peer_id();
        let site = |assigned: bool| {
            (0..)
                .map(|i| format!("shop{i}.com"))
                .find(|d| is_assigned(epoch, &crawler, d, MAX_SHARE_PPM) == assigned)
                .unwrap()
        };
        let crawled = |domain: &str| {
            let mut record = SiteRecord::new(domain);
            record.title = Some("Shop".into());
            record.description = Some("Handmade shoes".into());
            record.body_text = Some("Handmade leather shoes, made to order".into());
            record.crawled_at = Some(now);
            record
        };
        let (assigned, unassigned) = (site(true), site(false));
        let batch = Batch::sign(
            &key,
            &[crawled(&assigned), crawled(&unassigned)],
            epoch,
            MAX_SHARE_PPM,
            now,
        )
        .unwrap()
        .unwrap();
        // The node answering holds no text for either site.
        let answer = || {
            (0..2)
                .map(|i| {
                    let mut bare: SiteRecord = serde_json::from_str(&batch.records[i]).unwrap();
                    bare.body_text = None;
                    BucketRecord {
                        record: serde_json::to_string(&bare).unwrap(),
                        proof: Some(batch.proof(i)),
                        also: Vec::new(),
                    }
                })
                .collect::<Vec<_>>()
        };
        let search = |trusted: &[PeerId]| {
            let mut found = NetSearch {
                found: crate::search::check_answer(answer(), now).unwrap(),
                ..NetSearch::default()
            };
            keep_trusted_crawls(&mut found, trusted, now);
            let by = |d: &str| found.found.iter().find(|s| s.record.domain == d).cloned();
            (by(&assigned).unwrap(), by(&unassigned).unwrap())
        };

        // Untrusted: an assigned crawl's homepage facts count, never its
        // text, and an unassigned crawl counts not at all.
        let (a, u) = search(&[]);
        // One untrusted crawler alone is not kept: anyone can make a key
        // assigned the site.
        assert!(a.keeps().is_none());
        let shared = a.shared.clone().expect("an assigned crawl counts");
        assert_eq!(shared.description.as_deref(), Some("Handmade shoes"));
        assert_eq!(shared.body_text, None);
        assert!(u.shared.is_none() && !u.verified);
        // Confirmed by crawlers this node counts, it is.
        let confirmed = crate::search::FoundSite {
            confirmed: true,
            ..a
        };
        assert_eq!(confirmed.keeps(), Some(&shared));

        // Trusted: both count whole, text included, and are ranked with it.
        let (a, u) = search(&[crawler]);
        for site in [a, u] {
            assert!(site.verified);
            assert!(site.keeps().is_some());
            let text = Some("Handmade leather shoes, made to order");
            assert_eq!(site.shared.unwrap().body_text.as_deref(), text);
            assert_eq!(site.record.body_text.as_deref(), text);
        }
    }

    #[test]
    fn the_largest_honest_answers_fit_under_the_response_caps() {
        // A batch of the most records, as many bytes as a batch may hold.
        let key = Keypair::generate_ed25519();
        let now = now_unix();
        let mut batch = Batch::sign(
            &key,
            &[SiteRecord::new("a.com")],
            epoch_of(now),
            MAX_SHARE_PPM,
            now,
        )
        .unwrap()
        .unwrap();
        let line = "x".repeat(crate::batch::MAX_BATCH_BYTES / MAX_BATCH_RECORDS);
        batch.records = vec![line; MAX_BATCH_RECORDS];
        assert!(size(&BatchResponse::Batch(Some(batch.clone()))) < MAX_BATCH_RESPONSE);
        let headers = vec![batch.header; MAX_LISTED_BATCHES];
        assert!(size(&BatchResponse::Headers(headers)) < MAX_BATCH_RESPONSE);
        // The most reports listed, of the longest pick there is.
        let domain = format!("{}.com", "a".repeat(59));
        let query =
            crate::popularity::pick_query(&format!("{} {}", "b".repeat(30), "c".repeat(30)))
                .expect("a query that is reported");
        let report = Report::new(crate::popularity::report_epoch(now), &query, &domain).unwrap();
        let reports = vec![report; MAX_LISTED_REPORTS];
        assert!(size(&ReportResponse::Reports(reports)) < MAX_REPORT_RESPONSE);
    }

    /// The size of `value` in CBOR, as the request-response codec sends it.
    fn size<T: Serialize>(value: &T) -> u64 {
        cbor4ii::serde::to_vec(Vec::new(), value).unwrap().len() as u64
    }

    #[test]
    fn a_node_goes_back_to_its_bootstrap_nodes_after_losing_them() {
        // Alone: every minute.
        assert!(redial_bootstrap(0, false, 1));
        // Connected to a bootstrap node: never.
        assert!(!(0..=BOOTSTRAP_REDIAL_MINUTES).any(|t| redial_bootstrap(3, true, t)));
        // Other nodes only: every few minutes.
        let tries = (1..=3 * BOOTSTRAP_REDIAL_MINUTES)
            .filter(|&t| redial_bootstrap(3, false, t))
            .count();
        assert_eq!(tries, 3);
    }

    #[test]
    fn a_record_with_an_icon_takes_two_lines_of_a_batch() {
        let record = |i: usize, icon: bool| {
            let mut r = SiteRecord::new(format!("s{i}.com").as_str());
            r.icon = icon.then(|| "x".to_string());
            r
        };
        let plain: Vec<SiteRecord> = (0..MAX_BATCH_RECORDS + 1)
            .map(|i| record(i, false))
            .collect();
        let sizes: Vec<usize> = batch_chunks(&plain).iter().map(|c| c.len()).collect();
        assert_eq!(sizes, [MAX_BATCH_RECORDS, 1]);
        let iconed: Vec<SiteRecord> = (0..MAX_BATCH_RECORDS).map(|i| record(i, true)).collect();
        let sizes: Vec<usize> = batch_chunks(&iconed).iter().map(|c| c.len()).collect();
        assert_eq!(sizes, [MAX_BATCH_RECORDS / 2, MAX_BATCH_RECORDS / 2]);
        assert!(batch_chunks(&[]).is_empty());
    }

    #[test]
    fn crawls_are_published_in_the_epoch_they_were_made() {
        let now = 1_790_000_000;
        let day = crate::assign::EPOCH_SECS;
        let at = |domain: &str, crawled_at: Option<u64>| {
            let mut r = SiteRecord::new(domain);
            r.crawled_at = crawled_at;
            r
        };
        let records = vec![
            at("today.com", Some(now)),
            at("linked.com", None),
            at("yesterday.com", Some(now - day)),
            at("lastweek.com", Some(now - MAX_BATCH_AGE_EPOCHS * day)),
            at("tomorrow.com", Some(now + 2 * day)),
        ];
        let grouped = by_crawl_epoch(records, now);
        let domains: Vec<(u64, Vec<&str>)> = grouped
            .iter()
            .map(|(epoch, rs)| (*epoch, rs.iter().map(|r| r.domain.as_str()).collect()))
            .collect();
        let today = epoch_of(now);
        assert_eq!(
            domains,
            vec![
                (today - 1, vec!["yesterday.com"]),
                (today, vec!["today.com", "linked.com"]),
            ]
        );
    }

    #[test]
    fn home_network_addresses_are_not_global() {
        for local in [
            "/ip4/127.0.0.1/tcp/4001",
            "/ip4/192.168.4.21/tcp/4101",
            "/ip4/10.0.0.2/udp/4001/quic-v1",
            "/ip4/100.64.1.1/tcp/1",
            "/ip6/fd2e:4ba7:777b:1::5/udp/4101/quic-v1",
            "/ip6/fe80::1/tcp/1",
            "/ip6/::1/tcp/1",
        ] {
            assert!(!is_global(&local.parse().unwrap()), "{local}");
        }
        for public in [
            "/ip4/198.211.114.63/tcp/4001",
            "/ip6/2604:a880:400:d1::5:1815:7001/tcp/4001",
            "/dns4/plumbsearch.org/tcp/4001",
            "/ip4/198.211.114.63/tcp/4001/p2p/12D3KooWJ2UWUBsxmPfXTfHa8cBBmzifa6kj5pFZKfJXYNQyJ69a/p2p-circuit",
        ] {
            assert!(is_global(&public.parse().unwrap()), "{public}");
        }
    }

    #[test]
    fn circuits_are_matched_by_their_relay() {
        let relay = PeerId::random();
        let other = PeerId::random();
        let target = PeerId::random();
        let through = |r: PeerId| -> Multiaddr {
            format!("/ip4/198.211.114.63/tcp/4001/p2p/{r}/p2p-circuit/p2p/{target}")
                .parse()
                .unwrap()
        };
        assert!(is_circuit_through(&through(relay), &relay));
        assert!(!is_circuit_through(&through(other), &relay));
        let direct: Multiaddr = format!("/ip4/1.2.3.4/tcp/1/p2p/{relay}").parse().unwrap();
        assert!(!is_circuit_through(&direct, &relay));
    }
}
