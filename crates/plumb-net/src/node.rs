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
//!   [`CATCH_UP_EPOCHS`] epochs it missed.
//! * Network search: [`NetHandle::search`] never sends the query; it asks
//!   other nodes for buckets of sites under throwaway identities (see
//!   [`crate::bucket`] and [`crate::search`]). The node answers other
//!   nodes' bucket requests from its own [`BucketSource`], with a proof for
//!   every site it holds a signed crawl of.
//! * Popularity sharing: [`NetHandle::send_report`] hands a popularity
//!   report to another node under a throwaway identity. A node handed a
//!   report keeps it and passes it on over the gossip topic, so every node
//!   holds every report and counts them itself into the table
//!   [`NetHandle::popularity`] returns (see [`crate::popularity`]). On
//!   meeting a node, it asks for the reports of this week and last week.

use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroU32;
use std::path::PathBuf;
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
    autonat, dcutr, gossipsub, identify, kad, mdns, noise, ping, relay, tcp, upnp, yamux,
    Multiaddr, PeerId, StreamProtocol, Swarm,
};
use plumb_core::{now_unix, SiteRecord};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::agree::{Agreement, AgreementStatus, MIN_JUDGED};
use crate::assign::{epoch_of, is_assigned, MAX_SHARE_PPM};
use crate::batch::{accept_batch, accept_own_batch, Batch, SignedHeader};
use crate::bucket::{BucketSource, BUCKETS};
use crate::credits::{CreditStatus, Issuer, Ledger, Pending, Wallet, MAX_ISSUE};
use crate::hash::Hash;
use crate::joining::{explain_dial_error, peer_of, JoinProblem, PeerView, Route};
use crate::oblivious::{
    seal_response, Gateway, ObliviousRequest, ObliviousResponse, Opened, SignedKeys, MAX_MESSAGE,
    OBLIVIOUS_PROTOCOL, RELAY_KEY_CACHE, REPORT_REQUEST_SIZE,
};
use crate::popularity::{report_epoch, PopularityTable, Report};
use crate::proto::*;
use crate::reports::ReportStore;
use crate::search::{BucketPeer, NetSearch};
use crate::store::BatchStore;

/// Relays a node behind NAT takes reservations on.
pub const MAX_RELAYS: usize = 2;
/// Epochs of batches a node asks for when it meets another.
pub const CATCH_UP_EPOCHS: u64 = 3;
/// A node dials more nodes it knows of while it has fewer connections.
pub const TARGET_PEERS: usize = 8;
/// As a relay: circuits one node or IP address may open at once, before
/// it is held to one every [`CIRCUIT_REFILL`].
const CIRCUIT_BURST: NonZeroU32 = NonZeroU32::new(600).unwrap();
const CIRCUIT_REFILL: Duration = Duration::from_millis(100);
/// Batch fetches in flight at once.
const MAX_FETCHES: usize = 16;
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
const RELAY_HOP_PROTOCOL: &str = "/libp2p/circuit/relay/0.2.0/hop";
/// Nodes a report is offered to, one after the other, until one takes it.
pub const REPORT_TRIES: usize = 3;
/// Minutes between two recounts of the reports, when new ones came in.
pub const RECOUNT_MINUTES: u64 = 10;
/// Where the counted reports are written, for anyone curious.
const POPULARITY_FILE: &str = "popularity.json";

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
    /// Bucket requests answered at once for free, [`MAX_ANSWERING`] unless
    /// changed; up to [`PRIORITY_SLOTS`] more for requests that spend a
    /// token.
    pub max_answering: usize,
    /// Keep a few tokens from each node it searches, bought with this
    /// node's credits, to be answered when that node is busy.
    pub collect_tokens: bool,
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
            max_answering: MAX_ANSWERING,
            collect_tokens: true,
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
}

/// How many connected nodes [`NetStatus::peers`] lists.
pub const MAX_PEER_VIEWS: usize = 50;

/// Talks to the swarm task.
#[derive(Debug)]
pub struct NetHandle {
    peer_id: PeerId,
    share_ppm: u32,
    commands: mpsc::UnboundedSender<Command>,
    status: Arc<Mutex<NetStatus>>,
    popularity: Arc<RwLock<Arc<PopularityTable>>>,
    wallet: Arc<Mutex<Wallet>>,
    /// Tokens this node's searches spent.
    tokens_spent: Arc<std::sync::atomic::AtomicU64>,
    /// Buckets this node's own searches fetched (see [`crate::cache`]).
    cache: crate::cache::BucketCache,
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

    /// Searches the network for `query` without sending it: fetches the
    /// query's buckets, padded with random ones, from other nodes under
    /// throwaway identities, waiting at most `wait`, and returns the sites
    /// that match, checked but unranked (see [`crate::search`]). Buckets
    /// this node fetched lately are used again instead of asked for (see
    /// [`crate::cache`]).
    pub async fn search(&self, query: &str, wait: Duration) -> Result<NetSearch> {
        let (reply, peers) = oneshot::channel();
        self.send(Command::Peers(Serving::Buckets, reply))?;
        let peers = peers.await.context("the network task stopped")?;
        let mut found = crate::search::search(
            query,
            &peers,
            wait,
            now_unix(),
            Some(&self.wallet),
            Some(&self.cache),
        )
        .await;
        self.tokens_spent
            .fetch_add(found.priority as u64, std::sync::atomic::Ordering::Relaxed);
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

    /// Forgets the buckets kept from this node's searches.
    pub fn clear_search_cache(&self) {
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
}

/// Starts the network side of a node. Returns its handle and the records
/// accepted from other nodes' batches, one batch at a time.
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
    let (store, agreement) = {
        let dir = config.dir.join("batches");
        let trusted = config.trusted_peers.clone();
        tokio::task::spawn_blocking(move || -> Result<_> {
            let store = BatchStore::open(&dir)?;
            let agreement = replay_agreement(&store, peer_id, &trusted);
            Ok((store, agreement))
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
        ..NetStatus::default()
    }));
    let popularity = Arc::new(RwLock::new(Arc::new(table)));
    let (commands, commands_rx) = mpsc::unbounded_channel();
    let (records_tx, records_rx) = mpsc::unbounded_channel();
    let (answers_tx, answers_rx) = mpsc::unbounded_channel();
    info!("network node {peer_id} starting");
    let mut task = Task {
        swarm,
        key: key.clone(),
        config: config.clone(),
        topic,
        report_topic,
        store: Arc::new(Mutex::new(store)),
        reports: Arc::new(Mutex::new(reports)),
        popularity: popularity.clone(),
        recount: false,
        report_peers: HashMap::new(),
        report_listing: HashSet::new(),
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
        token_asks: HashMap::new(),
        asking: HashMap::new(),
        answers_tx,
        bucket_peers: HashMap::new(),
        batch_peers: HashSet::new(),
        relays: HashMap::new(),
        remote_addrs: HashMap::new(),
        reserved: HashSet::new(),
        nearby: HashSet::new(),
        wanted: VecDeque::new(),
        wanted_ids: HashSet::new(),
        fetching: HashMap::new(),
        listing: HashSet::new(),
        unannounced: Vec::new(),
        answering: 0,
        gateway,
        oblivious_peers: HashSet::new(),
        relay_keys: HashMap::new(),
        waiting_keys: HashMap::new(),
        relaying: HashMap::new(),
        bootstrap_peers: config.bootstrap.iter().filter_map(peer_of).collect(),
        problem: None,
        alone_since: Some(now_unix()),
    };
    for addr in &config.bootstrap {
        task.dial(addr.clone());
    }
    let handle = tokio::spawn(task.run(commands_rx, answers_rx));
    Ok((
        NetHandle {
            peer_id,
            share_ppm: config.share_ppm.min(MAX_SHARE_PPM),
            commands,
            status,
            popularity,
            wallet,
            tokens_spent,
            cache: crate::cache::BucketCache::open(&config.dir.join("bucket-cache"), now_unix()),
            task: Mutex::new(Some(handle)),
        },
        records_rx,
    ))
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
                        .set_response_size_maximum(64 * 1024 * 1024),
                    [(StreamProtocol::new(BATCH_PROTOCOL), ProtocolSupport::Full)],
                    request_config.clone(),
                ),
                reports: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(8 * 1024)
                        .set_response_size_maximum(128 * 1024 * 1024),
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

/// What this node asked another for on `/plumb/credits/1`.
enum Asking {
    /// For tokens; `None` when the node tops up its own wallet.
    Tokens(PeerId, Pending, Option<oneshot::Sender<Result<usize>>>),
    Credits(oneshot::Sender<Result<CreditsAt>>),
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
    /// Nodes asked for their reports this session.
    report_listing: HashSet<PeerId>,
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
    /// Nodes that hold a reservation with us (when we are a relay).
    reserved: HashSet<PeerId>,
    /// Nodes we reached over a home-network or loopback address (directly
    /// or through a relay there), which may use the same kind.
    nearby: HashSet<PeerId>,
    /// Batches to fetch, and from whom.
    wanted: VecDeque<(Hash, Vec<PeerId>)>,
    wanted_ids: HashSet<Hash>,
    fetching: HashMap<OutboundRequestId, (Hash, Vec<PeerId>)>,
    /// Nodes asked for their batch headers this session.
    listing: HashSet<PeerId>,
    /// Our own headers not yet announced to anyone.
    unannounced: Vec<SignedHeader>,
    answering: usize,
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
                let serving = match serving {
                    Serving::Buckets => &self.bucket_peers,
                    Serving::Reports => &self.report_peers,
                };
                let peers = serving
                    .iter()
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
                self.on_oblivious_request(request, Reply::Local(reply));
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
            Command::AskCredits(peer, reply) => {
                let id = self
                    .swarm
                    .behaviour_mut()
                    .credits
                    .send_request(&peer, CreditRequest::Balance);
                self.asking.insert(id, Asking::Credits(reply));
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

    fn publish(&mut self, records: Vec<SiteRecord>) -> Result<Option<Hash>> {
        let now = now_unix();
        let share = self.config.share_ppm.min(MAX_SHARE_PPM);
        let Some(batch) = Batch::sign(&self.key, &records, epoch_of(now), share, now)? else {
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
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .batches
                    .send_response(channel, response);
            }
            Answer::Report(channel, response) => {
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .reports
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
        if let Err(err) = self.gateway.rotate(&self.key, now) {
            warn!("cannot make a new key for sealed requests: {err:#}");
        }
        self.relay_keys
            .retain(|_, (keys, fetched)| *fetched + RELAY_KEY_CACHE > now && keys.expires > now);
        let connected = self.swarm.connected_peers().count();
        if connected == 0 {
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
            self.lock_store().prune(now);
            self.agreement.prune(now);
            let agreement = self.agreement.status();
            self.with_status(|s| s.agreement = agreement);
            self.lock_reports().prune(now);
            // A new week makes last week's count stale.
            self.recount = true;
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
            let table = reports
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .table(now_unix());
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
                if !addr.iter().any(|p| p == Protocol::P2pCircuit) {
                    self.remote_addrs.insert(peer_id, addr);
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
                    self.oblivious_peers.remove(&peer_id);
                    self.batch_peers.remove(&peer_id);
                    self.remote_addrs.remove(&peer_id);
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
        let nearby = self.nearby.contains(&peer);
        let usable = |addr: &Multiaddr| is_specific(addr) && (nearby || is_global(addr));
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
            if self.report_listing.insert(peer) {
                let current = report_epoch(now_unix());
                for epoch in [current.saturating_sub(1), current] {
                    self.swarm
                        .behaviour_mut()
                        .reports
                        .send_request(&peer, ReportRequest::List { epoch });
                }
            }
        }
        if supports(OBLIVIOUS_PROTOCOL) {
            self.oblivious_peers.insert(peer);
        }
        if supports(BATCH_PROTOCOL) {
            self.batch_peers.insert(peer);
            if self.listing.insert(peer) {
                let since = epoch_of(now_unix()).saturating_sub(CATCH_UP_EPOCHS);
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

    fn want(&mut self, id: Hash, sources: Vec<PeerId>) {
        if self.lock_store().contains(&id) || !self.wanted_ids.insert(id) {
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
        let busy = !self.admit(&request);
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
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => self.on_oblivious_request(request, Reply::Remote(channel)),
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
    fn on_oblivious_request(&mut self, request: ObliviousRequest, reply: Reply) {
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
            ObliviousRequest::Deliver { message } => self.open_sealed(message.into_vec(), reply),
            ObliviousRequest::Forward { target, message } if target == me => {
                self.open_sealed(message.into_vec(), reply);
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
    fn open_sealed(&mut self, message: Vec<u8>, reply: Reply) {
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
        if !self.admit(&request) {
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
                let store = self.store.clone();
                let tx = self.answers_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let store = store.lock().unwrap_or_else(PoisonError::into_inner);
                    let response = match request {
                        BatchRequest::Get(id) => {
                            BatchResponse::Batch(store.get(&id).unwrap_or_else(|err| {
                                warn!("cannot read a batch: {err:#}");
                                None
                            }))
                        }
                        BatchRequest::List { since_epoch } => BatchResponse::Headers(
                            store.headers_since(since_epoch, MAX_LISTED_BATCHES),
                        ),
                    };
                    drop(store);
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
                return;
            }
        };
        let held = {
            let mut store = self.lock_store();
            if let Err(err) = store.insert(&batch) {
                warn!("cannot keep batch {id}: {err:#}");
                return;
            }
            store.len()
        };
        let accepted = accept_batch(&batch, &crawler, now);
        let kept = accepted.len();
        let confirmed = self
            .agreement
            .observe(crawler, accepted, batch.header.header.created_at);
        self.count_credits();
        info!(
            "received batch {id} from {from}: kept {kept} of {} records crawled by {crawler}, {} now confirmed by a second crawler",
            batch.records.len(),
            confirmed.len()
        );
        let agreement = self.agreement.status();
        self.with_status(|s| {
            s.batches_received += 1;
            s.batches_held = held;
            s.agreement = agreement;
        });
        if !confirmed.is_empty() {
            let _ = self.records.send(confirmed);
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
                    let reports = self.reports.clone();
                    let tx = self.answers_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let list = reports
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .list(epoch, MAX_LISTED_REPORTS);
                        let _ = tx.send(Answer::Report(channel, ReportResponse::Reports(list)));
                    });
                }
            },
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Response {
                        response: ReportResponse::Reports(reports),
                        ..
                    },
                ..
            } => {
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
            request_response::Event::OutboundFailure { peer, error, .. } => {
                debug!("report request to {peer} failed: {error}");
            }
            _ => {}
        }
    }

    /// Whether to answer a bucket request now: for free while fewer than
    /// `max_answering` are being answered, and for a token of ours up to
    /// [`PRIORITY_SLOTS`] more. A token sent along is spent either way: it
    /// is only sent after this node said it was busy.
    fn admit(&mut self, request: &BucketRequest) -> bool {
        let paid = request
            .token
            .as_ref()
            .is_some_and(|token| self.issuer.redeem(token));
        if self.answering < self.config.max_answering {
            return true;
        }
        if paid && self.answering < self.config.max_answering + PRIORITY_SLOTS {
            self.priority_answered += 1;
            return true;
        }
        false
    }

    /// Asks the nodes this node searches for tokens, when it holds few of
    /// theirs, at most every [`TOKEN_ASK_MINUTES`] each. Nodes whose
    /// ledger has no credits for us say no, and we ask again later.
    fn collect_tokens(&mut self, now: u64) {
        if !self.config.collect_tokens {
            return;
        }
        let peers: Vec<PeerId> = self.bucket_peers.keys().copied().collect();
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
        let counts = self.crawls_count(&peer);
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
                        "your crawls do not count here yet"
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
        let held = self.lock_store().len();
        let reports_held = self.lock_reports().len();
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
            tokens_spent: self.tokens_spent.load(std::sync::atomic::Ordering::Relaxed),
            tokens_held: self
                .wallet
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .total(),
        };
        self.with_status(|s| {
            s.peers = peers;
            s.nearby_peers = nearby_peers;
            s.problem = problem;
            s.alone_since = alone_since;
            s.credits = credits;
            s.listening = listening;
            s.reachable_at = reachable_at;
            s.connected_peers = connected_peers;
            s.relaying_peers = relaying_peers;
            s.relays = relays;
            s.batches_held = held;
            s.reports_held = reports_held;
        });
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
        let store = store.lock().unwrap_or_else(PoisonError::into_inner);
        lines
            .into_iter()
            .map(|record| {
                let mut proofs = serde_json::from_str::<SiteRecord>(&record)
                    .ok()
                    .map(|r| {
                        store
                            .proofs(&r.domain, 1 + MAX_EXTRA_PROOFS)
                            .unwrap_or_default()
                    })
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

/// Rebuilds the agreement step from the batches held, oldest first, so it
/// needs no file of its own. What it confirms was passed on before.
fn replay_agreement(store: &BatchStore, me: PeerId, trusted: &[PeerId]) -> Agreement {
    let now = now_unix();
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
        } else {
            accept_batch(&batch, &crawler, made)
        };
        agreement.observe(crawler, records, made);
    }
    agreement.prune(now);
    // These were credited when the batches first came in.
    agreement.take_verdicts();
    agreement
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
fn is_circuit_through(addr: &Multiaddr, relay: &PeerId) -> bool {
    let mut previous = None;
    for p in addr.iter() {
        if p == Protocol::P2pCircuit {
            return previous == Some(*relay);
        }
        previous = match p {
            Protocol::P2p(id) => Some(id),
            _ => None,
        };
    }
    false
}

fn without_p2p(addr: Multiaddr) -> Multiaddr {
    addr.into_iter()
        .filter(|p| !matches!(p, Protocol::P2p(_)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
