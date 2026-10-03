//! A node's place in the network: one libp2p swarm, run on a Tokio task,
//! driven through a [`NetHandle`].
//!
//! # Getting connected without port forwarding
//!
//! A node dials out to its bootstrap nodes and to the nodes they tell it
//! about (Kademlia), over TCP and QUIC. That is all most nodes need:
//! fetching batches, sending searches and answering the searches of nodes
//! it dialed all happen over connections it opened itself.
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
//!   it accepts ([`accept_batch`]) to the receiver returned by [`start`].
//!   On meeting a node, it asks for the batches of the last
//!   [`CATCH_UP_EPOCHS`] epochs it missed.
//! * Network search: [`NetHandle::search`] asks up to
//!   [`SEARCH_FANOUT`] nodes at random and merges their answers; answers
//!   with a proof are checked, and an answer whose proof fails is dropped.
//!   The node answers other nodes' searches from its own index through
//!   [`LocalSearch`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
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
    autonat, dcutr, gossipsub, identify, kad, noise, ping, relay, tcp, upnp, yamux, Multiaddr,
    PeerId, StreamProtocol, Swarm,
};
use plumb_core::{canonical_domain, now_unix, registrable_domain, SiteRecord};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::assign::{epoch_of, is_assigned, MAX_SHARE_PPM};
use crate::batch::{accept_batch, Batch, SignedHeader};
use crate::hash::Hash;
use crate::proto::*;
use crate::store::BatchStore;

/// Relays a node behind NAT takes reservations on.
pub const MAX_RELAYS: usize = 2;
/// Nodes a network search asks.
pub const SEARCH_FANOUT: usize = 3;
/// Epochs of batches a node asks for when it meets another.
pub const CATCH_UP_EPOCHS: u64 = 3;
/// A node dials more nodes it knows of while it has fewer connections.
pub const TARGET_PEERS: usize = 8;
/// Batch fetches in flight at once.
const MAX_FETCHES: usize = 16;
/// Searches from other nodes answered at once; more are turned away.
const MAX_ANSWERING: usize = 8;
const RELAY_HOP_PROTOCOL: &str = "/libp2p/circuit/relay/0.2.0/hop";

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
    /// Share of all sites this node takes on each epoch, in parts per
    /// million, at most [`MAX_SHARE_PPM`].
    pub share_ppm: u32,
    /// Answer other nodes' searches from the local index.
    pub answer_searches: bool,
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
            share_ppm: MAX_SHARE_PPM,
            answer_searches: true,
        }
    }
}

/// A hit from this node's own index, for answering other nodes.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalHit {
    pub domain: String,
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub score: f32,
}

/// Searches this node's own index for other nodes. Called on a blocking
/// thread.
pub trait LocalSearch: Send + Sync + 'static {
    fn search(&self, query: &str, limit: usize) -> Vec<LocalHit>;
}

/// The result of a network search.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NetSearch {
    /// Nodes asked.
    pub asked: usize,
    /// Nodes that answered in time.
    pub answered: usize,
    /// Answers dropped because a proof in them did not check out.
    pub rejected: usize,
    /// Merged hits, best first.
    pub hits: Vec<NetworkHit>,
}

/// A hit merged from the answers of several nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkHit {
    pub domain: String,
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// The text comes from a signed crawl whose proof checked out.
    pub verified: bool,
    /// The node that signed that crawl.
    pub crawler: Option<String>,
    /// How many of the nodes that answered returned this site.
    pub answered_by: usize,
    /// Mean of the answering nodes' scores, each scaled to their best hit.
    pub score: f32,
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
    /// Relays this node holds a reservation on.
    pub relays: Vec<String>,
    pub batches_held: usize,
    pub batches_published: u64,
    pub batches_received: u64,
    pub searches_answered: u64,
}

/// Talks to the swarm task.
#[derive(Debug)]
pub struct NetHandle {
    peer_id: PeerId,
    share_ppm: u32,
    commands: mpsc::UnboundedSender<Command>,
    status: Arc<Mutex<NetStatus>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

enum Command {
    Publish {
        records: Vec<SiteRecord>,
        reply: oneshot::Sender<Result<Option<Hash>>>,
    },
    Search {
        query: String,
        limit: u32,
        answers: mpsc::UnboundedSender<(PeerId, Option<SearchResponse>)>,
        asked: oneshot::Sender<usize>,
    },
    Dial(Multiaddr),
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

    /// Asks up to [`SEARCH_FANOUT`] nodes for `query` and merges what comes
    /// back within `wait`.
    pub async fn search(&self, query: &str, limit: usize, wait: Duration) -> Result<NetSearch> {
        let limit = (limit as u32).clamp(1, MAX_SEARCH_HITS);
        let (answers_tx, mut answers) = mpsc::unbounded_channel();
        let (asked_tx, asked) = oneshot::channel();
        self.send(Command::Search {
            query: query.to_string(),
            limit,
            answers: answers_tx,
            asked: asked_tx,
        })?;
        let asked = asked.await.context("the network task stopped")?;
        let mut collected = Vec::new();
        let deadline = tokio::time::Instant::now() + wait;
        while collected.len() < asked {
            match tokio::time::timeout_at(deadline, answers.recv()).await {
                Ok(Some(answer)) => collected.push(answer),
                Ok(None) | Err(_) => break,
            }
        }
        Ok(merge_answers(asked, collected, limit as usize, now_unix()))
    }

    /// Dials `addr`, for tests and for adding a node by hand.
    pub fn dial(&self, addr: Multiaddr) -> Result<()> {
        self.send(Command::Dial(addr))
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
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    kad: kad::Behaviour<kad::store::MemoryStore>,
    gossipsub: gossipsub::Behaviour,
    search: request_response::cbor::Behaviour<SearchRequest, SearchResponse>,
    batches: request_response::cbor::Behaviour<BatchRequest, BatchResponse>,
}

/// Starts the network side of a node. Returns its handle and the records
/// accepted from other nodes' batches, one batch at a time.
pub async fn start(
    config: NetConfig,
    local: Arc<dyn LocalSearch>,
) -> Result<(NetHandle, mpsc::UnboundedReceiver<Vec<SiteRecord>>)> {
    let key = load_or_create_key(&config.dir.join("node.key"))?;
    let store = {
        let dir = config.dir.join("batches");
        tokio::task::spawn_blocking(move || BatchStore::open(&dir))
            .await
            .context("opening the batch store")??
    };
    let peer_id = key.public().to_peer_id();
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

    let status = Arc::new(Mutex::new(NetStatus {
        peer_id: peer_id.to_string(),
        nat: "unknown".into(),
        batches_held: store.len(),
        ..NetStatus::default()
    }));
    let (commands, commands_rx) = mpsc::unbounded_channel();
    let (records_tx, records_rx) = mpsc::unbounded_channel();
    let (answers_tx, answers_rx) = mpsc::unbounded_channel();
    info!("network node {peer_id} starting");
    let mut task = Task {
        swarm,
        key: key.clone(),
        config: config.clone(),
        topic,
        store: Arc::new(Mutex::new(store)),
        local,
        status: status.clone(),
        records: records_tx,
        answers_tx,
        searchable: HashSet::new(),
        batch_peers: HashSet::new(),
        relays: HashMap::new(),
        remote_addrs: HashMap::new(),
        searches: HashMap::new(),
        wanted: VecDeque::new(),
        wanted_ids: HashSet::new(),
        fetching: HashMap::new(),
        listing: HashSet::new(),
        unannounced: Vec::new(),
        answering: 0,
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
            task: Mutex::new(Some(handle)),
        },
        records_rx,
    ))
}

fn build_swarm(key: &Keypair, config: &NetConfig) -> Result<Swarm<Behaviour>> {
    let peer_id = key.public().to_peer_id();
    let relay_server = config.relay_server;
    let upnp = config.upnp;
    let search_support = if config.answer_searches {
        ProtocolSupport::Full
    } else {
        ProtocolSupport::Outbound
    };
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
                relay::Behaviour::new(
                    peer_id,
                    relay::Config {
                        max_reservations: 1024,
                        max_circuits: 256,
                        ..relay::Config::default()
                    },
                )
            });
            let request_config =
                request_response::Config::default().with_request_timeout(Duration::from_secs(20));
            Ok(Behaviour {
                relay_client,
                relay: relay.into(),
                dcutr: dcutr::Behaviour::new(peer_id),
                autonat: autonat::Behaviour::new(peer_id, autonat::Config::default()),
                upnp: upnp.then(upnp::tokio::Behaviour::default).into(),
                identify: identify::Behaviour::new(
                    identify::Config::new(IDENTIFY_PROTOCOL.into(), key.public())
                        .with_agent_version(format!("plumb/{}", env!("CARGO_PKG_VERSION"))),
                ),
                ping: ping::Behaviour::default(),
                kad,
                gossipsub,
                search: request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new(SEARCH_PROTOCOL), search_support)],
                    request_config.clone(),
                ),
                batches: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(4 * 1024)
                        .set_response_size_maximum(64 * 1024 * 1024),
                    [(StreamProtocol::new(BATCH_PROTOCOL), ProtocolSupport::Full)],
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
    Search(ResponseChannel<SearchResponse>, SearchResponse),
    Batch(ResponseChannel<BatchResponse>, BatchResponse),
}

type Answers = mpsc::UnboundedSender<(PeerId, Option<SearchResponse>)>;

struct Task {
    swarm: Swarm<Behaviour>,
    key: Keypair,
    config: NetConfig,
    topic: gossipsub::IdentTopic,
    store: Arc<Mutex<BatchStore>>,
    local: Arc<dyn LocalSearch>,
    status: Arc<Mutex<NetStatus>>,
    records: mpsc::UnboundedSender<Vec<SiteRecord>>,
    answers_tx: mpsc::UnboundedSender<Answer>,
    /// Connected nodes that answer searches.
    searchable: HashSet<PeerId>,
    /// Connected nodes that serve batches.
    batch_peers: HashSet<PeerId>,
    /// Relays we asked for a reservation, and whether it was granted.
    relays: HashMap<PeerId, bool>,
    /// The address of each connected node, as we reached it or it reached us.
    remote_addrs: HashMap<PeerId, Multiaddr>,
    /// Network searches waiting for answers.
    searches: HashMap<OutboundRequestId, Answers>,
    /// Batches to fetch, and from whom.
    wanted: VecDeque<(Hash, Vec<PeerId>)>,
    wanted_ids: HashSet<Hash>,
    fetching: HashMap<OutboundRequestId, (Hash, Vec<PeerId>)>,
    /// Nodes asked for their batch headers this session.
    listing: HashSet<PeerId>,
    /// Our own headers not yet announced to anyone.
    unannounced: Vec<SignedHeader>,
    answering: usize,
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
        info!("network node stopped");
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Publish { records, reply } => {
                let _ = reply.send(self.publish(records));
            }
            Command::Search {
                query,
                limit,
                answers,
                asked,
            } => {
                let mut peers: Vec<PeerId> = self.searchable.iter().copied().collect();
                shuffle(&mut peers);
                peers.truncate(SEARCH_FANOUT);
                for peer in &peers {
                    let id = self.swarm.behaviour_mut().search.send_request(
                        peer,
                        SearchRequest {
                            query: query.clone(),
                            limit,
                        },
                    );
                    self.searches.insert(id, answers.clone());
                }
                let _ = asked.send(peers.len());
            }
            Command::Dial(addr) => self.dial(addr),
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
        self.with_status(|s| {
            s.batches_published += 1;
            s.batches_held = held;
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
            Answer::Search(channel, response) => {
                self.answering = self.answering.saturating_sub(1);
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .search
                    .send_response(channel, response);
                self.with_status(|s| s.searches_answered += 1);
            }
            Answer::Batch(channel, response) => {
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .batches
                    .send_response(channel, response);
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
        if ticks.is_multiple_of(5) && connected > 0 {
            let _ = self.swarm.behaviour_mut().kad.bootstrap();
        }
        if !self.unannounced.is_empty() && connected > 0 {
            for header in std::mem::take(&mut self.unannounced) {
                self.announce(header);
            }
        }
        if ticks.is_multiple_of(60) {
            self.lock_store().prune(now_unix());
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
                    self.searchable.remove(&peer_id);
                    self.batch_peers.remove(&peer_id);
                    self.remote_addrs.remove(&peer_id);
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
                let verdict = self.on_header(propagation_source, &message.data);
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
            BehaviourEvent::Search(event) => self.on_search_event(event),
            BehaviourEvent::Batches(event) => self.on_batch_event(event),
            BehaviourEvent::RelayClient(relay::client::Event::ReservationReqAccepted {
                relay_peer_id,
                renewal,
                ..
            }) => {
                if !renewal {
                    info!("reachable through the relay {relay_peer_id}");
                }
                self.relays.insert(relay_peer_id, true);
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
        if supports(KAD_PROTOCOL) {
            for addr in &info.listen_addrs {
                if is_specific(addr) && !addr.iter().any(|p| p == Protocol::P2pCircuit) {
                    self.swarm
                        .behaviour_mut()
                        .kad
                        .add_address(&peer, addr.clone());
                }
            }
        }
        if supports(SEARCH_PROTOCOL) {
            self.searchable.insert(peer);
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

    fn on_search_event(&mut self, event: request_response::Event<SearchRequest, SearchResponse>) {
        match event {
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => {
                if self.answering >= MAX_ANSWERING || request.query.len() > MAX_QUERY_BYTES {
                    let _ = self
                        .swarm
                        .behaviour_mut()
                        .search
                        .send_response(channel, SearchResponse { hits: Vec::new() });
                    return;
                }
                debug!("answering a search from {peer}");
                self.answering += 1;
                let local = self.local.clone();
                let store = self.store.clone();
                let tx = self.answers_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let limit = request.limit.min(MAX_SEARCH_HITS) as usize;
                    let hits = local
                        .search(&request.query, limit)
                        .into_iter()
                        .map(|hit| {
                            let proof = store
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .proof(&hit.domain)
                                .unwrap_or_else(|err| {
                                    warn!("cannot read a batch: {err:#}");
                                    None
                                });
                            NetHit {
                                domain: hit.domain,
                                url: hit.url,
                                title: hit.title,
                                description: hit.description,
                                score: hit.score,
                                proof,
                            }
                        })
                        .collect();
                    let _ = tx.send(Answer::Search(channel, SearchResponse { hits }));
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
                if let Some(answers) = self.searches.remove(&request_id) {
                    let _ = answers.send((peer, Some(response)));
                }
            }
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                debug!("search sent to {peer} failed: {error}");
                if let Some(answers) = self.searches.remove(&request_id) {
                    let _ = answers.send((peer, None));
                }
            }
            _ => {}
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
        info!(
            "received batch {id} from {from}: kept {} of {} records crawled by {crawler}",
            accepted.len(),
            batch.records.len()
        );
        self.with_status(|s| {
            s.batches_received += 1;
            s.batches_held = held;
        });
        if !accepted.is_empty() {
            let _ = self.records.send(accepted);
        }
    }

    fn lock_store(&self) -> std::sync::MutexGuard<'_, BatchStore> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
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
        let relays = self
            .relays
            .iter()
            .filter(|(_, granted)| **granted)
            .map(|(peer, _)| peer.to_string())
            .collect();
        let held = self.lock_store().len();
        self.with_status(|s| {
            s.listening = listening;
            s.reachable_at = reachable_at;
            s.connected_peers = connected_peers;
            s.relays = relays;
            s.batches_held = held;
        });
    }
}

/// Checks and merges the answers of several nodes. Hits with a proof that
/// fails make the whole answer count as a lie and be dropped. A site's
/// score is the mean of its scores scaled to each node's best hit, and
/// sites more nodes returned come first.
fn merge_answers(
    asked: usize,
    answers: Vec<(PeerId, Option<SearchResponse>)>,
    limit: usize,
    now: u64,
) -> NetSearch {
    let mut out = NetSearch {
        asked,
        ..NetSearch::default()
    };
    let mut merged: Vec<NetworkHit> = Vec::new();
    let mut scores: HashMap<String, Vec<f32>> = HashMap::new();
    for (peer, response) in answers {
        let Some(response) = response else {
            continue;
        };
        out.answered += 1;
        let mut checked = Vec::new();
        let mut honest = true;
        for hit in response.hits.into_iter().take(MAX_SEARCH_HITS as usize) {
            if canonical_domain(&hit.domain).as_deref() != Some(hit.domain.as_str()) {
                continue;
            }
            // A link always goes to the site the hit names, whatever URL
            // an unproven answer gives.
            let url = if registrable_domain(&hit.url).as_deref() == Some(hit.domain.as_str()) {
                hit.url
            } else {
                format!("https://{}/", hit.domain)
            };
            let mut network_hit = NetworkHit {
                domain: hit.domain.clone(),
                url,
                title: hit.title,
                description: hit.description,
                verified: false,
                crawler: None,
                answered_by: 1,
                score: hit.score,
            };
            if let Some(proof) = &hit.proof {
                match proof.verify(now) {
                    Ok((record, crawler)) if record.domain == hit.domain => {
                        network_hit.url = record
                            .url
                            .unwrap_or_else(|| format!("https://{}/", record.domain));
                        network_hit.title = record.title;
                        network_hit.description = record.description;
                        network_hit.verified = true;
                        network_hit.crawler = Some(crawler.to_string());
                    }
                    Ok(_) | Err(_) => {
                        warn!("{peer} answered a search with a proof that does not check out");
                        honest = false;
                        break;
                    }
                }
            }
            checked.push(network_hit);
        }
        if !honest {
            out.rejected += 1;
            continue;
        }
        let best = checked
            .iter()
            .map(|h| h.score)
            .fold(f32::MIN, f32::max)
            .max(f32::EPSILON);
        for hit in checked {
            let scaled = (hit.score / best).clamp(0.0, 1.0);
            scores.entry(hit.domain.clone()).or_default().push(scaled);
            match merged.iter_mut().find(|m| m.domain == hit.domain) {
                Some(existing) => {
                    existing.answered_by += 1;
                    if hit.verified && !existing.verified {
                        let answered_by = existing.answered_by;
                        *existing = NetworkHit { answered_by, ..hit };
                    }
                }
                None => merged.push(hit),
            }
        }
    }
    for hit in &mut merged {
        let s = &scores[&hit.domain];
        hit.score = s.iter().sum::<f32>() / out.answered.max(1) as f32;
    }
    merged.sort_by(|a, b| {
        b.answered_by
            .cmp(&a.answered_by)
            .then(b.score.total_cmp(&a.score))
            .then(a.domain.cmp(&b.domain))
    });
    merged.truncate(limit);
    out.hits = merged;
    out
}

/// Not an unspecified (`0.0.0.0`, `::`) address.
fn is_specific(addr: &Multiaddr) -> bool {
    !addr.iter().any(|p| match p {
        Protocol::Ip4(ip) => ip.is_unspecified(),
        Protocol::Ip6(ip) => ip.is_unspecified(),
        _ => false,
    })
}

fn without_p2p(addr: Multiaddr) -> Multiaddr {
    addr.into_iter()
        .filter(|p| !matches!(p, Protocol::P2p(_)))
        .collect()
}

fn shuffle<T>(items: &mut [T]) {
    let mut rng = rand_core::OsRng;
    for i in (1..items.len()).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(domain: &str, score: f32) -> NetHit {
        NetHit {
            domain: domain.into(),
            url: format!("https://{domain}/"),
            title: Some(domain.into()),
            description: None,
            score,
            proof: None,
        }
    }

    #[test]
    fn unproven_hits_link_only_to_the_site_they_name() {
        let mut lying = hit("usbank.com", 1.0);
        lying.url = "https://usbank-login.example/".into();
        let merged = merge_answers(
            1,
            vec![(
                PeerId::random(),
                Some(SearchResponse {
                    hits: vec![lying, hit("not a domain", 1.0)],
                }),
            )],
            10,
            0,
        );
        assert_eq!(merged.hits.len(), 1);
        assert_eq!(merged.hits[0].url, "https://usbank.com/");
    }

    #[test]
    fn an_answer_with_a_forged_proof_is_dropped_whole() {
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
        let batch = Batch::sign(&key, &[record], epoch_of(now), MAX_SHARE_PPM, now)
            .unwrap()
            .unwrap();
        let mut proof = batch.proof(0);
        proof.record = proof.record.replace("Real Bank", "Log in here");
        let mut forged = hit(&domain, 1.0);
        forged.proof = Some(proof);
        let honest = NetHit {
            proof: Some(batch.proof(0)),
            ..hit(&domain, 1.0)
        };
        let merged = merge_answers(
            2,
            vec![
                (
                    PeerId::random(),
                    Some(SearchResponse {
                        hits: vec![hit("other.com", 2.0), forged],
                    }),
                ),
                (
                    PeerId::random(),
                    Some(SearchResponse { hits: vec![honest] }),
                ),
            ],
            10,
            now,
        );
        assert_eq!(merged.rejected, 1);
        assert_eq!(merged.hits.len(), 1);
        assert!(merged.hits[0].verified);
        assert_eq!(merged.hits[0].title.as_deref(), Some("Real Bank"));
    }

    #[test]
    fn sites_more_nodes_agree_on_come_first() {
        let (a, b) = (PeerId::random(), PeerId::random());
        let merged = merge_answers(
            3,
            vec![
                (
                    a,
                    Some(SearchResponse {
                        hits: vec![hit("x.com", 9.0), hit("y.com", 3.0)],
                    }),
                ),
                (
                    b,
                    Some(SearchResponse {
                        hits: vec![hit("y.com", 1.0)],
                    }),
                ),
                (PeerId::random(), None),
            ],
            10,
            0,
        );
        assert_eq!((merged.asked, merged.answered), (3, 2));
        let order: Vec<_> = merged
            .hits
            .iter()
            .map(|h| (h.domain.as_str(), h.answered_by))
            .collect();
        assert_eq!(order, vec![("y.com", 2), ("x.com", 1)]);
    }
}
