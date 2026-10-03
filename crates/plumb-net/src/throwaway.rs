//! Throwaway identities: a new key, a swarm of its own and a new
//! connection for one request, dropped after. The node asked cannot tie
//! the request to the asking node's permanent id, nor to its other
//! requests. It still sees the IP address the request comes from, so
//! requests go sealed through another node when one can relay (see
//! [`crate::oblivious`]).

use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use libp2p::identity::Keypair;
use libp2p::request_response::{self, ProtocolSupport};
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{noise, relay, tcp, yamux, StreamProtocol, Swarm};
use tracing::debug;

use crate::popularity::Report;
use crate::proto::{ReportRequest, ReportResponse, REPORT_PROTOCOL};
use crate::search::BucketPeer;

/// A swarm under a new identity, over TCP, QUIC and relays, with the
/// behaviour `make` builds around the relay client.
pub(crate) fn swarm<B: NetworkBehaviour>(
    make: impl FnOnce(relay::client::Behaviour) -> B,
) -> Result<Swarm<B>> {
    let key = Keypair::generate_ed25519();
    Ok(libp2p::SwarmBuilder::with_existing_identity(key)
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
        .with_behaviour(|_, relay_client| make(relay_client))
        .map_err(|err| anyhow::anyhow!("setting up a throwaway swarm: {err}"))?
        .build())
}

#[derive(NetworkBehaviour)]
struct Reporter {
    relay_client: relay::client::Behaviour,
    reports: request_response::cbor::Behaviour<ReportRequest, ReportResponse>,
}

/// Hands `report` to `peer` under a new identity. Returns whether the
/// node took it (it may already hold it).
pub async fn submit_report(peer: &BucketPeer, report: &Report, wait: Duration) -> Result<bool> {
    let mut swarm = swarm(|relay_client| Reporter {
        relay_client,
        reports: request_response::Behaviour::with_codec(
            request_response::cbor::codec::Codec::default()
                .set_request_size_maximum(8 * 1024)
                .set_response_size_maximum(1024),
            [(
                StreamProtocol::new(REPORT_PROTOCOL),
                ProtocolSupport::Outbound,
            )],
            request_response::Config::default().with_request_timeout(wait),
        ),
    })?;
    for addr in &peer.addrs {
        swarm.add_peer_address(peer.peer, addr.clone());
    }
    swarm
        .behaviour_mut()
        .reports
        .send_request(&peer.peer, ReportRequest::Submit(report.clone()));
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let event = tokio::time::timeout_at(deadline, swarm.select_next_some())
            .await
            .context("no answer in time")?;
        match event {
            SwarmEvent::Behaviour(ReporterEvent::Reports(request_response::Event::Message {
                message: request_response::Message::Response { response, .. },
                ..
            })) => {
                return match response {
                    ReportResponse::Taken(taken) => Ok(taken),
                    ReportResponse::Reports(_) => bail!("{} answered something else", peer.peer),
                }
            }
            SwarmEvent::Behaviour(ReporterEvent::Reports(
                request_response::Event::OutboundFailure { error, .. },
            )) => bail!("handing {} a report: {error}", peer.peer),
            SwarmEvent::OutgoingConnectionError { error, .. } => {
                debug!(
                    "a throwaway identity could not reach {}: {error}",
                    peer.peer
                );
            }
            _ => {}
        }
    }
}
