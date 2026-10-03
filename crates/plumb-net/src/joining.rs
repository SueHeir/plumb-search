//! Joining the network without typing addresses: the nodes every node can
//! start from, what a node says about the nodes it is connected to, and
//! why it could not reach a bootstrap node, in words its owner can act on.

use libp2p::core::transport::TransportError;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::DialError;
use libp2p::{Multiaddr, PeerId};
use serde::{Deserialize, Serialize};

/// The Plumb network's own first nodes: the relay on plumbsearch.org, by
/// name and by address. A node that starts from them learns the others
/// from them, and nodes on the same home network find each other without
/// them (mDNS).
pub const DEFAULT_BOOTSTRAP: [&str; 2] = [
    "/dns4/plumbsearch.org/tcp/4001/p2p/12D3KooWJ2UWUBsxmPfXTfHa8cBBmzifa6kj5pFZKfJXYNQyJ69a",
    "/ip4/198.211.114.63/tcp/4001/p2p/12D3KooWJ2UWUBsxmPfXTfHa8cBBmzifa6kj5pFZKfJXYNQyJ69a",
];

/// [`DEFAULT_BOOTSTRAP`], parsed.
pub fn default_bootstrap() -> Vec<Multiaddr> {
    DEFAULT_BOOTSTRAP
        .iter()
        .map(|addr| addr.parse().expect("a valid multiaddr"))
        .collect()
}

/// How this node reaches a connected node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// Over the home or office network (or this computer).
    Nearby,
    /// Straight over the internet.
    Direct,
    /// Through a relay, because neither side could reach the other.
    Relayed,
}

/// A node this node is connected to, for the panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerView {
    pub peer_id: String,
    pub route: Route,
    /// It relays for this node, which makes this node reachable.
    #[serde(default)]
    pub relay: bool,
    /// One of the bootstrap nodes.
    #[serde(default)]
    pub bootstrap: bool,
}

/// A bootstrap node this node could not reach, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinProblem {
    /// What went wrong and what to do about it, in a sentence or two.
    pub message: String,
    /// The error as libp2p gave it, for a bug report.
    pub detail: String,
    /// When, in Unix seconds.
    pub at: u64,
    /// How telling the cause is, lowest first: of two failures in the same
    /// round, the panel shows the more telling one.
    #[serde(skip)]
    pub(crate) rank: u8,
}

impl JoinProblem {
    pub fn new(message: impl Into<String>, detail: impl Into<String>, at: u64) -> JoinProblem {
        JoinProblem {
            message: message.into(),
            detail: detail.into(),
            at,
            rank: Cause::Other as u8,
        }
    }
}

/// The peer id a bootstrap address ends in.
pub fn peer_of(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|p| match p {
        Protocol::P2p(peer) => Some(peer),
        _ => None,
    })
}

/// The host of a bootstrap address, as people would write it.
fn host_of(addr: &Multiaddr) -> Option<String> {
    addr.iter().find_map(|p| match p {
        Protocol::Dns(name) | Protocol::Dns4(name) | Protocol::Dns6(name) => Some(name.to_string()),
        Protocol::Ip4(ip) => Some(ip.to_string()),
        Protocol::Ip6(ip) => Some(ip.to_string()),
        _ => None,
    })
}

/// What kind of failure one address hit, most telling first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Cause {
    Offline,
    Refused,
    TimedOut,
    NoName,
    Other,
}

fn cause(text: &str) -> Cause {
    let text = text.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| text.contains(w));
    if has(&[
        "networkunreachable",
        "network is unreachable",
        "hostunreachable",
        "no route to host",
        "addrnotavailable",
    ]) {
        Cause::Offline
    } else if has(&["connectionrefused", "connection refused"]) {
        Cause::Refused
    } else if has(&["timedout", "timed out", "timeout"]) {
        Cause::TimedOut
    } else if has(&[
        "resolve",
        "dns",
        "no record found",
        "nxdomain",
        "name or service not known",
        "nodename nor servname",
    ]) {
        Cause::NoName
    } else {
        Cause::Other
    }
}

/// Says why dialing the bootstrap node `addr` failed with `error`.
pub fn explain_dial_error(addr: Option<&Multiaddr>, error: &DialError, now: u64) -> JoinProblem {
    let host = addr.and_then(host_of);
    let who = match &host {
        Some(host) => format!("the bootstrap node at {host}"),
        None => "the bootstrap node".to_owned(),
    };
    let mut rank = Cause::Other;
    let message = match error {
        DialError::WrongPeerId { .. } => {
            rank = Cause::Offline;
            format!(
                "{} answered as a different node, so its address in the settings is out of date. \
             Update Plumb Search, or fix the address under Advanced.",
                capitalize(&who)
            )
        }
        DialError::Denied { .. } => format!(
            "This node turned down the connection to {who}, most likely because it already \
             has as many connections as it keeps."
        ),
        DialError::Transport(errors) => {
            let worst = errors
                .iter()
                .map(|(_, err)| match err {
                    TransportError::MultiaddrNotSupported(_) => Cause::Other,
                    TransportError::Other(err) => cause(&format!("{err:?} {err}")),
                })
                // A name that does not resolve says less than an address
                // that does not answer, when the node was tried by both.
                .min();
            rank = worst.unwrap_or(Cause::Other);
            match worst {
                Some(Cause::Offline) => {
                    "This computer seems to be offline: it has no route to the internet. \
                     Plumb keeps trying and connects once the internet is back."
                        .to_owned()
                }
                Some(Cause::Refused) => format!(
                    "{} is up but turned the connection away, so its Plumb node is probably \
                     restarting or down. Plumb keeps trying every minute.",
                    capitalize(&who)
                ),
                Some(Cause::TimedOut) => format!(
                    "{} did not answer. A firewall or network that blocks outgoing \
                     connections on port {} (common at work and on public Wi-Fi) is the \
                     usual cause.",
                    capitalize(&who),
                    addr.and_then(port_of).unwrap_or(4001)
                ),
                Some(Cause::NoName) => format!(
                    "This computer could not look up {}. Check the internet connection; \
                     if other sites load, the network's DNS may be blocking it.",
                    host.as_deref().unwrap_or("the bootstrap node's name")
                ),
                Some(Cause::Other) | None => {
                    format!("Plumb could not connect to {who}. Plumb keeps trying every minute.")
                }
            }
        }
        _ => format!("Plumb could not connect to {who}. Plumb keeps trying every minute."),
    };
    JoinProblem {
        message,
        detail: error.to_string(),
        at: now,
        rank: rank as u8,
    }
}

fn port_of(addr: &Multiaddr) -> Option<u16> {
    addr.iter().find_map(|p| match p {
        Protocol::Tcp(port) | Protocol::Udp(port) => Some(port),
        _ => None,
    })
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    fn transport(kinds: &[ErrorKind]) -> DialError {
        let addr: Multiaddr = DEFAULT_BOOTSTRAP[0].parse().unwrap();
        DialError::Transport(
            kinds
                .iter()
                .map(|kind| (addr.clone(), TransportError::Other(Error::from(*kind))))
                .collect(),
        )
    }

    fn says(error: &DialError) -> String {
        let addr: Multiaddr = DEFAULT_BOOTSTRAP[0].parse().unwrap();
        explain_dial_error(Some(&addr), error, 1).message
    }

    #[test]
    fn the_default_bootstrap_nodes_parse_and_name_their_peer() {
        for addr in default_bootstrap() {
            assert!(peer_of(&addr).is_some());
        }
    }

    #[test]
    fn dial_errors_are_explained_in_words_people_can_act_on() {
        assert!(says(&transport(&[ErrorKind::ConnectionRefused]))
            .starts_with("The bootstrap node at plumbsearch.org is up but turned"));
        let timeout = says(&transport(&[ErrorKind::TimedOut]));
        assert!(timeout.contains("firewall"), "{timeout}");
        assert!(timeout.contains("port 4001"), "{timeout}");
        assert!(says(&transport(&[ErrorKind::NetworkUnreachable])).contains("offline"));
        let dns = says(&DialError::Transport(vec![(
            DEFAULT_BOOTSTRAP[0].parse().unwrap(),
            TransportError::Other(Error::other("failed to resolve plumbsearch.org")),
        )]));
        assert!(dns.contains("could not look up plumbsearch.org"), "{dns}");
        // Tried by name and by address: the address's answer says more.
        let both = DialError::Transport(vec![
            (
                DEFAULT_BOOTSTRAP[0].parse().unwrap(),
                TransportError::Other(Error::other("no record found for plumbsearch.org")),
            ),
            (
                DEFAULT_BOOTSTRAP[1].parse().unwrap(),
                TransportError::Other(Error::from(ErrorKind::TimedOut)),
            ),
        ]);
        assert!(says(&both).contains("firewall"));
        let wrong = says(&DialError::WrongPeerId {
            obtained: PeerId::random(),
            address: DEFAULT_BOOTSTRAP[0].parse().unwrap(),
        });
        assert!(wrong.contains("out of date"), "{wrong}");
    }
}
