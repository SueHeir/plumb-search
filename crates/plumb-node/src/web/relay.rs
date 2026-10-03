//! Relaying private searches to other nodes, so that no one sees both who
//! searches and what for. For a node that serves private search and is in
//! the Plumb network:
//!
//! - `GET /api/oblivious/targets` lists a few connected nodes that answer
//!   bucket requests, each with its key, signed by that node
//!   ([`plumb_net::oblivious::SignedKeys`]). This node fetches the keys
//!   as a relay and hands everyone the same ones for a while, so a target
//!   cannot tell askers apart by the key they were given.
//! - `POST /api/oblivious/forward/{peer}` passes a bucket request the
//!   browser sealed to that node's key on to it, and its sealed answer back
//!   ([`plumb_net::NetHandle::oblivious_forward`]).
//!
//! So this node sees the browser's address and which node it asks, but not
//! the bucket, and the node answering sees the bucket but only this node's
//! address. The browser falls back to `GET /api/buckets/...` when there is
//! no other node to ask.

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use plumb_net::PeerId;
use serde::Serialize;
use tracing::debug;

use super::AppState;

/// Most nodes listed at once.
const MAX_TARGETS: usize = 8;
/// Largest sealed request taken; a real one is well under this.
const MAX_REQUEST_BYTES: usize = 1024;
/// How long fetching a node's key, or its answer, may take.
const WAIT: Duration = Duration::from_secs(5);

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/oblivious/targets", get(targets))
        .route("/api/oblivious/forward/{peer}", post(forward))
}

#[derive(Debug, Serialize)]
struct Target {
    peer: String,
    keys: plumb_net::oblivious::SignedKeys,
}

#[derive(Debug, Default, Serialize)]
struct Targets {
    targets: Vec<Target>,
}

/// The network handle, when this node relays private searches.
fn relay(state: &AppState) -> Option<std::sync::Arc<plumb_net::NetHandle>> {
    state
        .node
        .as_ref()
        .filter(|node| node.bucket_table().is_some())?;
    state.network()
}

/// `GET /api/oblivious/targets`.
async fn targets(State(state): State<AppState>) -> Response {
    let mut out = Targets::default();
    if let Some(net) = relay(&state) {
        let mut peers = net.bucket_peers().await.unwrap_or_default();
        plumb_net::search::shuffle(&mut peers);
        let mut fetches = tokio::task::JoinSet::new();
        for peer in peers.into_iter().take(MAX_TARGETS) {
            let net = net.clone();
            fetches.spawn(async move {
                match tokio::time::timeout(WAIT, net.oblivious_keys(peer)).await {
                    Ok(Ok(Some(keys))) => Some(Target {
                        peer: peer.to_string(),
                        keys,
                    }),
                    Ok(Ok(None)) => None,
                    Ok(Err(err)) => {
                        debug!("no key from {peer}: {err:#}");
                        None
                    }
                    Err(_) => None,
                }
            });
        }
        while let Some(fetched) = fetches.join_next().await {
            out.targets.extend(fetched.ok().flatten());
        }
    }
    ([(header::CACHE_CONTROL, "no-store")], axum::Json(out)).into_response()
}

/// `POST /api/oblivious/forward/{peer}`.
async fn forward(State(state): State<AppState>, Path(peer): Path<String>, body: Bytes) -> Response {
    let Some(net) = relay(&state) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(peer) = peer.parse::<PeerId>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if body.len() > MAX_REQUEST_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    // Only to nodes that answer bucket requests, so this is no open relay.
    if !net.bucket_peers().await.unwrap_or_default().contains(&peer) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::time::timeout(WAIT, net.oblivious_forward(peer, body.to_vec())).await {
        Ok(Ok(Some(answer))) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            answer,
        )
            .into_response(),
        Ok(Ok(None)) => StatusCode::BAD_GATEWAY.into_response(),
        Ok(Err(err)) => {
            debug!("relaying to {peer} failed: {err:#}");
            StatusCode::BAD_GATEWAY.into_response()
        }
        Err(_) => StatusCode::GATEWAY_TIMEOUT.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use plumb_core::{now_unix, SiteRecord};
    use plumb_net::oblivious::{seal_response, Gateway, Opened};
    use plumb_net::proto::{BucketRecord, BucketResponse};
    use plumb_private::sealed::{self, Target};

    /// The browser's sealed requests are the network's own: a node's
    /// gateway opens them, and the browser opens the gateway's answer.
    #[test]
    fn the_browser_and_a_node_understand_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let key = plumb_net::load_or_create_key(&dir.path().join("node.key")).unwrap();
        let now = now_unix();
        let gateway = Gateway::new(&key, now).unwrap();
        let target = Target {
            peer: key.public().to_peer_id().to_string(),
            keys: gateway.keys(),
        };
        // As GET /api/oblivious/targets sends it, as JSON.
        let json = serde_json::to_string(&serde_json::json!({
            "targets": [{"peer": target.peer, "keys": target.keys}]
        }))
        .unwrap();
        let listed: sealed::Targets = serde_json::from_str(&json).unwrap();

        let (request, opener) = sealed::seal(&listed.targets[0], 1234, now).unwrap();
        let (asked, reply) = gateway.open(&request).unwrap();
        assert!(matches!(asked, Opened::Bucket(asked) if asked.bucket == 1234));

        let mut usbank = SiteRecord::new("usbank.com");
        usbank.url = Some("https://www.usbank.com/".into());
        let mut lying = SiteRecord::new("Chase.com");
        lying.url = Some("https://chase-login.example/".into());
        let answer = BucketResponse {
            records: Some(vec![
                BucketRecord {
                    record: serde_json::to_string(&usbank).unwrap(),
                    proof: None,
                    also: Vec::new(),
                },
                BucketRecord {
                    record: serde_json::to_string(&lying).unwrap(),
                    proof: None,
                    also: Vec::new(),
                },
                BucketRecord {
                    record: "not json".into(),
                    proof: None,
                    also: Vec::new(),
                },
            ]),
            busy: false,
        };
        let sites = sealed::open(opener, &seal_response(reply, &answer).unwrap()).unwrap();
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].url.as_deref(), Some("https://www.usbank.com/"));
        assert_eq!(sites[1].domain, "chase.com");
        assert_eq!(sites[1].url, None, "a URL off the site's own domain");

        // Keys of another node, or expired ones, are refused.
        let other = Target {
            peer: plumb_net::PeerId::random().to_string(),
            ..listed.targets[0].clone()
        };
        assert!(sealed::seal(&other, 1, now).is_err());
        assert!(sealed::seal(&listed.targets[0], 1, now + 10 * 86_400).is_err());
    }
}
