//! `POST /mcp`: the [`crate::mcp`] server over HTTP ("Streamable HTTP"
//! without streams: every message gets one plain JSON answer, or `202
//! Accepted` for a notification), so AI apps can add a node by its address.
//!
//! The tools only read the index, so the endpoint needs no token. Requests
//! sent by web pages of other sites (an `Origin` that is not this host)
//! are refused, and tool calls are limited per client ([`Limiter`]), since
//! each costs a few searches; on a node behind a reverse proxy on the same
//! computer, the client is the proxy's `X-Forwarded-For`.
//!
//! `read_page` fetches pages from wherever the node runs, so it is offered
//! only to AI apps on the node's own computer (a request from a loopback
//! address that no proxy forwarded), unless the node runs with
//! `--mcp-read-pages`. A public node offering it to everyone would be an
//! open proxy.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tracing::error;

use super::{security_headers, AppState};
use crate::mcp::{parse_error, Mcp, Reader};

/// The largest message taken.
const MAX_BODY_BYTES: usize = 64 * 1024;
/// Tool calls a client may make in a burst...
const BURST: f64 = 30.0;
/// ...and per minute after that.
const PER_MINUTE: f64 = 60.0;
/// Clients remembered before the idle ones are forgotten.
const MAX_CLIENTS: usize = 10_000;

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/mcp",
        post(mcp)
            .get(not_streamed)
            .delete(not_streamed)
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
    )
}

/// Token buckets of tool calls, per client address.
#[derive(Debug, Default)]
pub(crate) struct Limiter {
    clients: Mutex<HashMap<Option<IpAddr>, (f64, Instant)>>,
}

impl Limiter {
    /// Takes one call for `client`, or says how many seconds until it may.
    fn take(&self, client: Option<IpAddr>, now: Instant) -> Result<(), u64> {
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if clients.len() >= MAX_CLIENTS && !clients.contains_key(&client) {
            // Forget the clients whose buckets have filled up again.
            clients.retain(|_, (tokens, at)| {
                *tokens + now.duration_since(*at).as_secs_f64() * PER_MINUTE / 60.0 < BURST
            });
        }
        let (tokens, at) = clients.entry(client).or_insert((BURST, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * PER_MINUTE / 60.0).min(BURST);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - *tokens) * 60.0 / PER_MINUTE).ceil() as u64)
        }
    }
}

/// The page reader `read_page` uses, made on first use and shared.
#[derive(Default)]
pub(crate) struct SharedReader(OnceLock<Option<Reader>>);

impl SharedReader {
    fn get(&self) -> Option<Reader> {
        self.0
            .get_or_init(|| {
                Reader::standard(tokio::runtime::Handle::current())
                    .map_err(|err| error!("{err:#}"))
                    .ok()
            })
            .clone()
    }
}

/// Whether the request comes from this computer and no proxy passed it on.
fn from_this_computer(request: &Request) -> bool {
    let headers = request.headers();
    if headers.contains_key("x-forwarded-for") || headers.contains_key(header::FORWARDED) {
        return false;
    }
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(peer)| peer.ip().to_canonical().is_loopback())
}

/// Who is asking: the peer, or for a proxy on this computer the address
/// it forwards for.
fn client(request: &Request) -> Option<IpAddr> {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| peer.ip().to_canonical())?;
    if !peer.is_loopback() {
        return Some(peer);
    }
    let forwarded = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .and_then(|last| last.trim().parse::<IpAddr>().ok());
    Some(forwarded.unwrap_or(peer))
}

/// Whether the request came from a web page of another site.
fn foreign_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return false;
    };
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|origin| url::Url::parse(origin).ok())
        .and_then(|url| {
            let host = url.host_str()?.to_string();
            Some(match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        });
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok());
    match (origin_host, host) {
        (Some(origin), Some(host)) => !origin.eq_ignore_ascii_case(host),
        _ => true,
    }
}

fn answer(status: StatusCode, body: Value) -> Response {
    (status, security_headers(), Json(body)).into_response()
}

async fn not_streamed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        security_headers(),
        [(header::ALLOW, "POST")],
        "This MCP server answers POST requests only; it opens no event stream.\n",
    )
        .into_response()
}

async fn mcp(State(state): State<AppState>, request: Request) -> Response {
    if foreign_origin(request.headers()) {
        return answer(
            StatusCode::FORBIDDEN,
            json!({ "error": "web pages of other sites cannot use this MCP server" }),
        );
    }
    let client = client(&request);
    let reads_pages = state.settings.read_pages_for_all || from_this_computer(&request);
    let Ok(body) = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await else {
        return answer(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({ "error": "the message is too big" }),
        );
    };
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return answer(StatusCode::BAD_REQUEST, parse_error());
    };
    if Mcp::is_tool_call(&message) {
        if let Err(wait) = state.mcp_limiter.take(client, Instant::now()) {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            let body = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32000, "message": format!("too many requests; try again in {wait} seconds") },
            });
            return (
                StatusCode::TOO_MANY_REQUESTS,
                security_headers(),
                [(header::RETRY_AFTER, wait.to_string())],
                Json(body),
            )
                .into_response();
        }
        if state.setting_up().is_some() {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            return answer(
                StatusCode::OK,
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": "This Plumb node is still setting up its index; try again in a few minutes." }],
                        "isError": true,
                    },
                }),
            );
        }
    }
    let country = state.settings.home.resolve(None);
    let (rates, plugins) = match Mcp::search_query(&message) {
        Some(query) => {
            let options = plumb_index::SearchOptions::default();
            tokio::join!(
                state.rates.for_query(query),
                state.plugin_results(query, &options)
            )
        }
        None => (None, Vec::new()),
    };
    let reader = if reads_pages {
        state.page_reader.get()
    } else {
        None
    };
    let server = Mcp::new(Arc::clone(&state.backend), country)
        .with_reader(reader)
        .with_rates(rates)
        .with_node(state.node.clone())
        .with_plugin_results(plugins);
    let reply = tokio::task::spawn_blocking(move || server.handle(&message)).await;
    match reply {
        Ok(Some(reply)) => answer(StatusCode::OK, reply),
        Ok(None) => (StatusCode::ACCEPTED, security_headers()).into_response(),
        Err(_) => {
            error!("an MCP request failed");
            answer(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "the request failed" }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn limits_each_client_to_a_burst_then_a_steady_rate() {
        let limiter = Limiter::default();
        let start = Instant::now();
        let a = Some("203.0.113.1".parse().unwrap());
        let b = Some("203.0.113.2".parse().unwrap());
        for _ in 0..BURST as usize {
            assert!(limiter.take(a, start).is_ok());
        }
        assert_eq!(limiter.take(a, start), Err(1));
        // Another client has its own bucket.
        assert!(limiter.take(b, start).is_ok());
        // One call a second comes back.
        assert!(limiter.take(a, start + Duration::from_secs(1)).is_ok());
        assert!(limiter.take(a, start + Duration::from_secs(1)).is_err());
    }

    #[test]
    fn only_this_computer_reads_pages() {
        let request = |peer: Option<&str>, forwarded: bool| {
            let mut request = Request::builder().uri("/mcp");
            if forwarded {
                request = request.header("x-forwarded-for", "203.0.113.9");
            }
            let mut request = request.body(axum::body::Body::empty()).unwrap();
            if let Some(peer) = peer {
                request
                    .extensions_mut()
                    .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
            }
            request
        };
        assert!(from_this_computer(&request(Some("127.0.0.1:5000"), false)));
        assert!(from_this_computer(&request(Some("[::1]:5000"), false)));
        // Caddy on the same computer, passing on someone else's request.
        assert!(!from_this_computer(&request(Some("127.0.0.1:5000"), true)));
        assert!(!from_this_computer(&request(
            Some("192.168.1.20:5000"),
            false
        )));
        assert!(!from_this_computer(&request(None, false)));
    }

    #[test]
    fn refuses_web_pages_of_other_sites() {
        let mut headers = HeaderMap::new();
        assert!(!foreign_origin(&headers));
        headers.insert(header::HOST, "plumbsearch.org".parse().unwrap());
        assert!(!foreign_origin(&headers));
        headers.insert(header::ORIGIN, "https://plumbsearch.org".parse().unwrap());
        assert!(!foreign_origin(&headers));
        headers.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(foreign_origin(&headers));
        headers.insert(header::HOST, "127.0.0.1:7586".parse().unwrap());
        headers.insert(header::ORIGIN, "http://127.0.0.1:7586".parse().unwrap());
        assert!(!foreign_origin(&headers));
        headers.insert(header::ORIGIN, "null".parse().unwrap());
        assert!(foreign_origin(&headers));
    }
}
