//! Building the swarm, and what an address says about where it leads.

use std::time::Duration;

use anyhow::{Context, Result};
use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::request_response::{self, ProtocolSupport};
use libp2p::{
    autonat, connection_limits, dcutr, gossipsub, identify, kad, mdns, noise, ping, relay, tcp,
    upnp, yamux, Multiaddr, PeerId, StreamProtocol, Swarm,
};
use tracing::warn;

use super::{
    Behaviour, NetConfig, BOOTSTRAP_REDIAL_MINUTES, CIRCUIT_BURST, CIRCUIT_REFILL,
    MAX_BATCH_RESPONSE, MAX_ESTABLISHED_INCOMING, MAX_ESTABLISHED_PER_PEER, MAX_LEAD_RESPONSE,
    MAX_PENDING_INCOMING, MAX_REPORT_RESPONSE,
};
use crate::hash::Hash;
use crate::oblivious::{MAX_MESSAGE, OBLIVIOUS_PROTOCOL, REPORT_REQUEST_SIZE};
use crate::proto::*;
use crate::sets::{MAX_MANIFEST_BYTES, MAX_SET_CHUNK};

fn connection_limits() -> connection_limits::ConnectionLimits {
    connection_limits::ConnectionLimits::default()
        .with_max_pending_incoming(Some(MAX_PENDING_INCOMING))
        .with_max_established_incoming(Some(MAX_ESTABLISHED_INCOMING))
        .with_max_established_per_peer(Some(MAX_ESTABLISHED_PER_PEER))
}

pub(super) fn build_swarm(key: &Keypair, config: &NetConfig) -> Result<Swarm<Behaviour>> {
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
                sets: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(1024)
                        .set_response_size_maximum(
                            u64::from(MAX_SET_CHUNK).max(MAX_MANIFEST_BYTES as u64) + 64 * 1024,
                        ),
                    [(StreamProtocol::new(SETS_PROTOCOL), ProtocolSupport::Full)],
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
                    request_config.clone(),
                ),
                leads: request_response::Behaviour::with_codec(
                    request_response::cbor::codec::Codec::default()
                        .set_request_size_maximum(64)
                        .set_response_size_maximum(MAX_LEAD_RESPONSE),
                    [(StreamProtocol::new(LEAD_PROTOCOL), ProtocolSupport::Full)],
                    request_config,
                ),
            })
        })
        .map_err(|err| anyhow::anyhow!("setting up the network: {err}"))?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
        .build();
    Ok(swarm)
}

/// Not an unspecified (`0.0.0.0`, `::`) address.
pub(super) fn is_specific(addr: &Multiaddr) -> bool {
    !addr.iter().any(|p| match p {
        Protocol::Ip4(ip) => ip.is_unspecified(),
        Protocol::Ip6(ip) => ip.is_unspecified(),
        _ => false,
    })
}

/// Reachable from anywhere: not a loopback, private, shared (CGNAT),
/// link-local or unique-local address. Names (`/dns4/...`) count as global.
pub(super) fn is_global(addr: &Multiaddr) -> bool {
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

pub(super) fn is_loopback(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| match p {
        Protocol::Ip4(ip) => ip.is_loopback(),
        Protocol::Ip6(ip) => ip.is_loopback(),
        _ => false,
    })
}

pub(super) fn relay_of(addr: &Multiaddr) -> Option<PeerId> {
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

pub(super) fn is_circuit_through(addr: &Multiaddr, relay: &PeerId) -> bool {
    relay_of(addr) == Some(*relay)
}

pub(super) fn without_p2p(addr: Multiaddr) -> Multiaddr {
    addr.into_iter()
        .filter(|p| !matches!(p, Protocol::P2p(_)))
        .collect()
}

/// Whether to dial the bootstrap nodes at maintenance tick `ticks`: every
/// tick while the node has no connections at all, and every
/// [`BOOTSTRAP_REDIAL_MINUTES`] while it has some but none to a bootstrap
/// node. Without that, a node behind NAT that lost its relay while other
/// nodes kept it company was never reachable again.
pub(super) fn redial_bootstrap(connected: usize, bootstrap_connected: bool, ticks: u64) -> bool {
    connected == 0 || (!bootstrap_connected && ticks.is_multiple_of(BOOTSTRAP_REDIAL_MINUTES))
}
