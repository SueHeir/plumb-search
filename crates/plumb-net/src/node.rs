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
//!   header on (or the crawler), checks it, keeps it, and hands the sites
//!   it accepts ([`accept_batch`](crate::batch::accept_batch)) to the
//!   [`Agreement`] step, which hands a site to the receiver returned by
//!   [`start`] only once two crawlers agree on it (see [`crate::agree`]),
//!   each as its stored item, held as its line ([`RecordDelivery`]).
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
//! * Leads: [`NetHandle::share_lead`] signs a page an agent found useful
//!   and passes it on over the gossip topic; every node keeps the leads it
//!   hears of, and [`NetHandle::leads`] finds those for a search from nodes
//!   in the search scope (see [`crate::leads`]). On meeting a node, it asks
//!   for the newest leads it holds.

mod answering;
mod batches;
mod gossip;
mod handle;
mod serving;
mod swarm;
mod task;
#[cfg(test)]
mod tests;
mod tokens;

use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use libp2p::identity::Keypair;
use libp2p::request_response::{self, OutboundRequestId, ResponseChannel};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::NetworkBehaviour;
use libp2p::{
    autonat, connection_limits, dcutr, gossipsub, identify, kad, mdns, ping, relay, upnp,
    Multiaddr, PeerId, Swarm,
};
use plumb_core::locks::Locked;
use plumb_core::now_unix;
use plumb_core::sites::SiteList;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use self::batches::{
    crawler_views, replay_agreement, resend_pending, BatchListRetries, BatchRetries,
};
pub use self::batches::{RecordDelivery, MAX_RECORD_DELIVERY_BYTES};
use self::swarm::build_swarm;
use crate::agree::{Agreement, AgreementStatus};
use crate::allowance::{Allowance, Source};
use crate::assign::MAX_SHARE_PPM;
use crate::batch::SignedHeader;
use crate::bucket::BucketSource;
use crate::credits::{CreditStatus, Issuer, Ledger, Pending, Wallet};
use crate::fill::FillPage;
use crate::hash::Hash;
use crate::joining::{peer_of, JoinProblem, PeerView};
use crate::leads::{FoundLead, Lead, LeadDraft, LeadKeys, LeadStore, LEADS_FILE};
use crate::oblivious::{Gateway, ObliviousRequest, ObliviousResponse, SignedKeys};
use crate::pages::PagesChunk;
use crate::popularity::PopularityTable;
use crate::proto::*;
use crate::reports::ReportStore;
use crate::rounds::{Pace, PendingBuckets, RoundStatus, ROUND_EVERY};
use crate::scope::{Friends, SearchScope};
use crate::search::{BucketPeer, NetSearch};
use crate::sets::SetsAnswer;
use crate::storage::{Reservation, StorageBudget, StorageStatus};
use crate::store::{BatchStore, CrawlerView, StoreLimits, RETAIN_EPOCHS};

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

/// After first becoming reachable through a relay, a node asks around for
/// nodes once more this much later: nodes that joined at the same moment
/// (all of them, after the bootstrap node restarts) are not known to the
/// relay yet when each first asks, and would otherwise meet only at the
/// next periodic ask, minutes later.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(5);

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

// Leave each peer's four serving slots room for startup/report requests.
const MAX_FETCHES_PER_PEER: usize = 2;

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

/// Largest answer taken on `/plumb/batch/2`: a batch of at most
/// [`crate::batch::MAX_BATCH_BYTES`] of records, or
/// [`MAX_LISTED_BATCHES`] headers of a few hundred bytes each.
const MAX_BATCH_RESPONSE: u64 = crate::batch::MAX_BATCH_BYTES as u64 + 4 * 1024 * 1024;

/// Largest answer taken on `/plumb/report/1`: [`MAX_LISTED_REPORTS`]
/// reports, each about 600 bytes and 1.3 KB at the longest pick.
const MAX_REPORT_RESPONSE: u64 = 64 * 1024 * 1024;

/// Report lists asked of other nodes at once: one node's two weeks. Nodes
/// met meanwhile are asked when they next identify themselves.
const MAX_REPORT_LISTS: usize = 2;

/// Largest answer taken on `/plumb/leads/1`:
/// [`MAX_LISTED_LEADS`](crate::leads::MAX_LISTED_LEADS) leads of at most a
/// few KB each.
const MAX_LEAD_RESPONSE: u64 = 16 * 1024 * 1024;

/// Lead lists asked of other nodes at once.
const MAX_LEAD_LISTS: usize = 2;

/// Batch and report requests of other nodes answered at once: one can mean
/// reading a 16 MB batch. More are answered with nothing for now.
const MAX_LISTS_SERVING: usize = 4;

const MAX_BATCH_LISTS: usize = 64;

const RELAY_HOP_PROTOCOL: &str = "/libp2p/circuit/relay/0.2.0/hop";

/// Nodes a report is offered to, one after the other, until one takes it.
pub const REPORT_TRIES: usize = 3;

/// Minutes between two recounts of the reports, when new ones came in.
pub const RECOUNT_MINUTES: u64 = 10;

/// Where the counted reports are written, for anyone curious.
const POPULARITY_FILE: &str = "popularity.json";

/// Where what the trusted nodes trust is kept (see [`crate::scope`]).
const FRIENDS_FILE: &str = "friends.json";

/// Pending disk writes and downstream record deliveries, at most.
const RECORD_DELIVERIES: usize = 4;

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
    /// Follow the crawls in the batches held: agreement on each site (which
    /// crawls are passed on to the node, which sites are disputed, which
    /// crawlers earn credits, see `crate::agree`) and where each crawl is,
    /// for proofs to searchers. Both are rebuilt from every batch held at
    /// start and kept in memory, about a kilobyte a crawled homepage with
    /// the sites it links to: gigabytes for a few weeks of the network's
    /// crawls. On unless changed. A node that only crawls turns it off
    /// along with `answer_searches`: it still holds, serves and passes on
    /// batches, and publishes its own.
    pub follow_crawls: bool,
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
    /// Most bytes of other crawlers' batches kept, the oldest deleted
    /// first; `None` for no limit but [`NetConfig::keep_batches_days`].
    pub keep_batches_bytes: Option<u64>,
    /// Whole data-directory allocation budget for foreign batches, cached
    /// buckets and their inbox. Own signed batches retain their exemption.
    pub storage_limit: Option<(PathBuf, u64)>,
    /// A node can share admission with its native/page/vector builders.
    pub storage_budget: Option<Arc<StorageBudget>>,
    /// Fixed interval between independently scheduled bucket rounds (see
    /// [`crate::rounds`]). Enabled searches read retained local data. `None`
    /// preserves legacy immediate-fetch behavior; default is [`ROUND_EVERY`].
    pub round_every: Option<Duration>,
    /// Ask trusted nodes for their crawls to fill this node's free space
    /// (see [`crate::fill`]); the node decides how much. On unless changed.
    pub fill: bool,
    /// Epochs of batches asked for on meeting a node, [`CATCH_UP_EPOCHS`]
    /// unless changed; at most
    /// [`MAX_BATCH_AGE_EPOCHS`](crate::batch::MAX_BATCH_AGE_EPOCHS) are taken.
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
            follow_crawls: true,
            trusted_peers: DEFAULT_TRUSTED_PEERS
                .iter()
                .map(|id| id.parse().expect("a valid peer id"))
                .collect(),
            search_scope: SearchScope::default(),
            max_answering: MAX_ANSWERING,
            collect_tokens: true,
            answer_per_day: None,
            keep_batches_days: RETAIN_EPOCHS,
            keep_batches_bytes: None,
            storage_limit: None,
            storage_budget: None,
            round_every: Some(ROUND_EVERY),
            fill: true,
            catch_up_epochs: CATCH_UP_EPOCHS,
        }
    }
}

/// What the network side of a node is doing, for `GET /api/status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetStatus {
    #[serde(default)]
    pub storage: Option<StorageStatus>,
    #[serde(default)]
    pub ingest_backpressure: bool,
    #[serde(default)]
    pub batch_busy_replies: u64,
    #[serde(default)]
    pub batch_retries: u64,
    #[serde(default)]
    pub batch_list_retries: u64,
    #[serde(default)]
    pub batch_lists_received: u64,
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
    /// Leads held, this node's own included (see [`crate::leads`]).
    #[serde(default)]
    pub leads_held: usize,
    /// Leads this node shared since it started.
    #[serde(default)]
    pub leads_shared: u64,
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
    storage: Option<Arc<StorageBudget>>,
    replay_stopped: Arc<std::sync::atomic::AtomicBool>,
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

/// Where the answer to a signed set request goes.
type SetsReply = oneshot::Sender<Result<Option<SetsAnswer>>>;

type ProfileReply = oneshot::Sender<Result<ProfileResponse>>;

enum Command {
    Publish {
        sites: SiteList,
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
        peer: Option<PeerId>,
        skip: Vec<PeerId>,
        reply: PagesReply,
    },
    Sets {
        request: SetsRequest,
        peer: Option<PeerId>,
        skip: Vec<PeerId>,
        reply: SetsReply,
    },
    Profile {
        peer: PeerId,
        request: ProfileRequest,
        reply: ProfileReply,
    },
    ShareLead(LeadDraft, oneshot::Sender<Result<Lead>>),
    Leads {
        keys: LeadKeys,
        limit: usize,
        reply: oneshot::Sender<Vec<FoundLead>>,
    },
    Dial(Multiaddr),
    Reconnect,
    Stop,
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
            plumb_core::storage::write_atomic_private(
                path,
                &path.with_extension("tmp"),
                &bytes,
                None,
            )
            .with_context(|| format!("saving {}", path.display()))?;
            info!("made a new node key in {}", path.display());
            Ok(key)
        }
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
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
    sets: request_response::cbor::Behaviour<SetsRequest, SetsResponse>,
    trust: request_response::cbor::Behaviour<TrustRequest, TrustResponse>,
    profile: request_response::cbor::Behaviour<ProfileRequest, ProfileResponse>,
    leads: request_response::cbor::Behaviour<LeadRequest, LeadResponse>,
}

pub async fn start(
    config: NetConfig,
    source: Arc<dyn BucketSource>,
) -> Result<(NetHandle, mpsc::Receiver<RecordDelivery>)> {
    let key = load_or_create_key(&config.dir.join("node.key"))?;
    let (reports, table, leads) = {
        let dir = config.dir.clone();
        tokio::task::spawn_blocking(move || {
            let now = now_unix();
            let reports = ReportStore::open(&dir.join("reports"), now)?;
            let table = reports.table(now);
            let leads = LeadStore::open(&dir.join(LEADS_FILE), now)?;
            anyhow::Ok((reports, table, leads))
        })
        .await
        .context("opening the report and lead stores")??
    };
    let peer_id = key.public().to_peer_id();
    // The network's own first nodes start from the same default list as
    // everyone else, which names them.
    let mut config = config;
    config
        .bootstrap
        .retain(|addr| peer_of(addr) != Some(peer_id));
    let gateway = Gateway::new(&key, now_unix())?;
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
    let storage = match (&config.storage_budget, &config.storage_limit) {
        (Some(budget), _) => Some(budget.clone()),
        (None, Some((root, limit))) => {
            let (root, limit) = (root.clone(), *limit);
            Some(tokio::task::spawn_blocking(move || StorageBudget::open(&root, limit)).await??)
        }
        (None, None) => None,
    };
    if config.storage_budget.is_some() {
        let budget = storage.clone().expect("shared startup ledger");
        tokio::task::spawn_blocking(move || budget.recount(budget.root())).await??;
    }
    let reports = reports.with_budget(storage.clone());
    let leads = leads.with_budget(storage.clone());
    let ledger = ledger.with_budget(storage.clone());
    let wallet = wallet.with_budget(storage.clone());
    let cache = Arc::new(crate::cache::BucketCache::open_with_budget(
        &config.dir.join("bucket-cache"),
        now_unix(),
        storage.clone(),
    ));
    let (store, agreement, replay) = {
        let dir = config.dir.join("batches");
        let trusted = config.trusted_peers.clone();
        let follow = config.follow_crawls;
        let limits = StoreLimits {
            own: key.public().encode_protobuf(),
            bytes: config.keep_batches_bytes,
            now: now_unix(),
            epochs: config.keep_batches_days,
            storage: storage.clone(),
        };
        tokio::task::spawn_blocking(move || -> Result<_> {
            let store = BatchStore::open_retained(&dir, follow, limits)?;
            let (agreement, replay) = if follow {
                replay_agreement(&store, peer_id, &trusted)
            } else {
                // Nothing is passed on, so nothing is sent again either:
                // the node takes them in once it follows crawls again.
                (Agreement::new(peer_id, trusted.iter().copied()), Vec::new())
            };
            Ok((store, agreement, replay))
        })
        .await
        .context("opening the batch store")??
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
    let lead_topic = gossipsub::IdentTopic::new(LEAD_TOPIC);
    swarm
        .behaviour_mut()
        .gossipsub
        .subscribe(&lead_topic)
        .context("subscribing to the lead topic")?;

    let status = Arc::new(Mutex::new(NetStatus {
        peer_id: peer_id.to_string(),
        nat: "unknown".into(),
        batches_held: store.len(),
        reports_held: reports.len(),
        leads_held: leads.len(),
        popular_picks: table.picks.len(),
        agreement: agreement.status(),
        crawlers: crawler_views(&store, peer_id, &config.trusted_peers, now_unix()),
        search_scope: config.search_scope,
        ..NetStatus::default()
    }));
    let popularity = Arc::new(RwLock::new(Arc::new(table)));
    let (commands, commands_rx) = mpsc::unbounded_channel();
    let (records_tx, records_rx) = mpsc::channel(RECORD_DELIVERIES);
    let replay_stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let store = Arc::new(Mutex::new(store));
    {
        let (store, records, trusted) = (
            store.clone(),
            records_tx.clone(),
            config.trusted_peers.clone(),
        );
        let stopped = replay_stopped.clone();
        if !replay.is_empty() {
            info!(
                "replaying {} held batches to pass on records not yet delivered",
                replay.len()
            );
        }
        tokio::task::spawn_blocking(move || {
            resend_pending(&store, &replay, &records, peer_id, &trusted, &stopped)
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
        storage: storage.clone(),
        reports: Arc::new(Mutex::new(reports)),
        popularity: popularity.clone(),
        recount: false,
        report_peers: HashMap::new(),
        report_listing: HashSet::new(),
        report_lists: HashSet::new(),
        lead_topic,
        leads,
        lead_peers: HashSet::new(),
        lead_listing: HashSet::new(),
        lead_lists: HashSet::new(),
        leads_shared: 0,
        unannounced_leads: Vec::new(),
        source,
        status: status.clone(),
        records: records_tx,
        replay_stopped: replay_stopped.clone(),
        pending_batches: HashSet::new(),
        ingest_blocked: false,
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
        ask_again_at: None,
        remote_addrs: HashMap::new(),
        circuits: HashMap::new(),
        reserved: HashSet::new(),
        nearby: HashSet::new(),
        wanted: VecDeque::new(),
        wanted_ids: HashSet::new(),
        refused: HashSet::new(),
        fetching: HashMap::new(),
        batch_retries: BatchRetries::default(),
        batch_lists: HashMap::new(),
        list_retries: BatchListRetries::default(),
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
        sets_peers: HashSet::new(),
        sets_serving: 0,
        sets_asked: HashMap::new(),
        sets_asking: HashMap::new(),
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
        if let Err(err) = plumb_core::storage::write_atomic(
            &scope_file,
            &scope_file.with_extension("tmp"),
            config.search_scope.as_str().as_bytes(),
            storage.as_ref(),
            None,
        ) {
            warn!("writing {}: {err}", scope_file.display());
        }
    }
    let pace = Arc::new(Pace::new(config.round_every));
    status.locked().rounds.every_secs = pace.every().map(|e| e.as_secs());
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
            storage,
            replay_stopped,
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
        if signed.id != site.site.domain() {
            continue;
        }
        site.site.update(|shown| {
            shown.url = signed.url.clone();
            shown.title = signed.title.clone();
            shown.about = signed.about.clone();
            plumb_core::site::set_headings(shown, plumb_core::site::headings(&signed));
            shown.text = signed.text.clone();
            plumb_core::site::set_terms(shown, plumb_core::site::terms(&signed));
            plumb_core::site::set_key_pages(shown, plumb_core::site::key_pages(&signed));
            plumb_core::site::set_links_to(shown, plumb_core::site::links_to(&signed));
            plumb_core::site::set_crawled(shown, plumb_core::site::crawled(&signed));
        });
        if !site.verified {
            site.verified = true;
            site.crawler = Some(crawler.to_string());
            if !site.crawlers.contains(&crawler.to_string()) {
                site.crawlers.insert(0, crawler.to_string());
            }
        }
        site.shared = Some(signed.into());
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
                    let (generation, queued) = pending.locked().take_round();
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
                    pending.locked().finish(generation, &queued, retry);
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
    let mut status = status.locked();
    let rounds = &mut status.rounds;
    rounds.sent += 1;
    rounds.answers += round.answered as u64;
}

/// Work done off the swarm task whose result goes back to a remote node.
enum Answer {
    StoredBatch {
        id: Hash,
        crawler: PeerId,
        from: PeerId,
        made: u64,
        lines: usize,
        accepted: SiteList,
        news: SiteList,
        permit: Option<mpsc::OwnedPermit<RecordDelivery>>,
        saved: Result<(bool, Option<Reservation>)>,
    },
    Bucket(ResponseChannel<BucketResponse>, BucketResponse),
    Batch(ResponseChannel<BatchResponse>, BatchResponse),
    Report(ResponseChannel<ReportResponse>, ReportResponse),
    Fill(ResponseChannel<FillResponse>, FillResponse),
    Pages(ResponseChannel<PagesResponse>, PagesResponse),
    Sets(ResponseChannel<SetsResponse>, SetsResponse),
    Profile(ResponseChannel<ProfileResponse>, ProfileResponse),
    /// Picks counted in a recount of the reports.
    Popularity(usize),
    Sealed(Reply, ObliviousResponse),
}

/// Whoever waits for an answer on `/plumb/oblivious/2`: a remote node, or
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
    replay_stopped: Arc<std::sync::atomic::AtomicBool>,
    swarm: Swarm<Behaviour>,
    key: Keypair,
    config: NetConfig,
    topic: gossipsub::IdentTopic,
    report_topic: gossipsub::IdentTopic,
    store: Arc<Mutex<BatchStore>>,
    /// The storage ledger the store charges, kept here so the swarm task
    /// reads it without the store's lock, which work off the task holds
    /// across whole batch writes.
    storage: Option<Arc<StorageBudget>>,
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
    lead_topic: gossipsub::IdentTopic,
    /// Leads held, this node's own and other nodes' (see [`crate::leads`]).
    leads: LeadStore,
    /// Connected nodes that hand out leads.
    lead_peers: HashSet<PeerId>,
    /// Connected nodes asked for their leads.
    lead_listing: HashSet<PeerId>,
    /// Those requests not answered yet (see [`MAX_LEAD_LISTS`]).
    lead_lists: HashSet<OutboundRequestId>,
    /// Leads this node shared since it started.
    leads_shared: u64,
    /// This node's leads not passed on yet: no node took the topic.
    unannounced_leads: Vec<Lead>,
    source: Arc<dyn BucketSource>,
    status: Arc<Mutex<NetStatus>>,
    records: mpsc::Sender<RecordDelivery>,
    pending_batches: HashSet<Hash>,
    ingest_blocked: bool,
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
    /// When to ask around for nodes again (see [`ASK_AGAIN_AFTER`]).
    ask_again_at: Option<tokio::time::Instant>,
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
    fetching: HashMap<OutboundRequestId, (Hash, Vec<PeerId>, PeerId)>,
    batch_retries: BatchRetries,
    batch_lists: HashMap<OutboundRequestId, (PeerId, u64)>,
    list_retries: BatchListRetries,
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
    /// Connected nodes that serve signed sets, trusted or not.
    sets_peers: HashSet<PeerId>,
    /// Signed set requests being answered.
    sets_serving: usize,
    /// Signed set requests answered for each node, and the minute counted.
    sets_asked: HashMap<PeerId, (u64, u32)>,
    /// Our signed set requests not yet answered.
    sets_asking: HashMap<OutboundRequestId, SetsReply>,
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

/// `read` of what `lock` guards, unless another thread holds it now.
fn peek<T, R>(lock: &Mutex<T>, read: impl FnOnce(&T) -> R) -> Option<R> {
    match lock.try_lock() {
        Ok(guard) => Some(read(&guard)),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(read(&poisoned.into_inner())),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// Now, in Unix milliseconds.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
