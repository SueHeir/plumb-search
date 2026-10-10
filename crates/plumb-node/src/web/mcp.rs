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
//! `--mcp-read-pages`. Others then read pages on ports 80 and 443 only,
//! fewer a minute than other tool calls, and only a few at once across all
//! clients ([`PublicReads`]): the node fetches what anyone asks, so it
//! must not become a way to knock on other servers' ports or to load the
//! web through it.
//!
//! `/mcp?answers=text` leaves the JSON copy of each tool's answer out, for
//! AI apps that would give the model the JSON instead of the short text
//! ([`crate::mcp::text_only`]). `/mcp?findings=off` answers as if the
//! client were on another computer of a node without `--mcp-findings`: no
//! findings or leads with search results, and no `report_finding`, for
//! runs that compare searches with and without them.
//!
//! `report_finding`, and the findings listed with search results, are
//! offered only to apps on the node's own computer: they hold what its
//! agents searched for (see [`crate::findings`]). A node run with
//! `--mcp-findings` offers them to every client. Only this computer's apps
//! ever get the leads other nodes shared, listed with search results, and
//! sharing a finding with `report_finding`'s `share`, on a node that
//! allows it: a lead goes out signed with the node's key. Behind a reverse proxy on the
//! same computer that does not say who it forwards for, every request looks
//! local (see `docs/docker.md`), so such a node should not share findings.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{header, Extensions, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tracing::error;

use super::{security_headers, AppState};
use crate::mcp::{error as rpc_error, parse_error, text_only, Mcp, Reader};

/// The largest message taken.
const MAX_BODY_BYTES: usize = 64 * 1024;
/// Tool calls a client may make in a burst...
const BURST: f64 = 30.0;
/// ...and per minute after that.
const PER_MINUTE: f64 = 60.0;
/// Clients remembered before the idle ones are forgotten.
const MAX_CLIENTS: usize = 10_000;
/// `read_page` calls a client not on this computer may make in a burst...
const READ_BURST: f64 = 10.0;
/// ...and per minute after that.
const READ_PER_MINUTE: f64 = 20.0;
/// Pages read at once for clients not on this computer, all together.
const MAX_PUBLIC_READS: usize = 8;

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    router.route(
        "/mcp",
        post(mcp)
            .get(not_streamed)
            .delete(not_streamed)
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
    )
}

/// Token buckets of requests, per client address: tool calls to `/mcp`
/// by default.
#[derive(Debug)]
pub(crate) struct Limiter {
    burst: f64,
    per_minute: f64,
    clients: Mutex<HashMap<Option<IpAddr>, (f64, Instant)>>,
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter::new(BURST, PER_MINUTE)
    }
}

impl Limiter {
    /// `burst` requests at once, then `per_minute`.
    pub(crate) fn new(burst: f64, per_minute: f64) -> Self {
        Limiter {
            burst,
            per_minute,
            clients: Mutex::default(),
        }
    }

    /// Takes one request for `client`, or says how many seconds until it may.
    pub(crate) fn take(&self, client: Option<IpAddr>, now: Instant) -> Result<(), u64> {
        let (burst, rate) = (self.burst, self.per_minute / 60.0);
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if clients.len() >= MAX_CLIENTS && !clients.contains_key(&client) {
            // Forget the clients whose buckets have filled up again.
            clients.retain(|_, (tokens, at)| {
                *tokens + now.duration_since(*at).as_secs_f64() * rate < burst
            });
            // Still full of busy clients: a new one waits, so the table
            // (and each scan of it) cannot grow without limit.
            if clients.len() >= MAX_CLIENTS {
                return Err((1.0 / rate).ceil() as u64);
            }
        }
        let (tokens, at) = clients.entry(client).or_insert((burst, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * rate).min(burst);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - *tokens) / rate).ceil() as u64)
        }
    }
}

/// The page reader `read_page` uses, made on first use and shared.
#[derive(Default)]
pub(crate) struct SharedReader(OnceLock<Option<Reader>>);

impl SharedReader {
    fn get(&self, config: &plumb_crawl::ReadConfig) -> Option<Reader> {
        self.0
            .get_or_init(|| {
                Reader::with_config(config.clone(), tokio::runtime::Handle::current())
                    .map_err(|err| error!("{err:#}"))
                    .ok()
            })
            .clone()
    }
}

/// How `read_page` reads for clients not on this computer, when the node
/// offers it to everyone.
pub(crate) struct PublicReads {
    limiter: Limiter,
    slots: Arc<tokio::sync::Semaphore>,
    reader: SharedReader,
}

impl Default for PublicReads {
    fn default() -> Self {
        PublicReads {
            limiter: Limiter::new(READ_BURST, READ_PER_MINUTE),
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_PUBLIC_READS)),
            reader: SharedReader::default(),
        }
    }
}

impl PublicReads {
    /// The node's reader settings, kept to ports 80 and 443.
    fn reader(&self, config: &plumb_crawl::ReadConfig) -> Option<Reader> {
        self.reader.get(&plumb_crawl::ReadConfig {
            web_ports_only: true,
            ..config.clone()
        })
    }
}

/// Whether the request comes from this computer and no proxy passed it on.
/// The address it was sent to must be a local name as well: a web page
/// whose DNS name was rebound to 127.0.0.1 also connects from loopback.
fn from_this_computer(request: &Request) -> bool {
    let headers = request.headers();
    if super::control::FORWARDED_HEADERS
        .iter()
        .any(|name| headers.contains_key(*name))
    {
        return false;
    }
    if !super::request_origin(headers, request.uri())
        .is_some_and(|own| super::panel::local_origin(&own))
    {
        return false;
    }
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(peer)| peer.ip().to_canonical().is_loopback())
}

/// Who is asking, for the rate limit: the peer, or for a proxy on this
/// computer or a private network (Caddy in front of a Docker container)
/// the address it forwards for. An IPv6 client is its /64 network, which
/// is what one home or server gets.
fn client(request: &Request) -> Option<IpAddr> {
    client_of(request.extensions(), request.headers())
}

/// [`client`] from a request's parts.
pub(super) fn client_of(extensions: &Extensions, headers: &HeaderMap) -> Option<IpAddr> {
    let peer = extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| peer.ip().to_canonical())?;
    let forwarded = || {
        headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.rsplit(',').next())
            .and_then(|last| last.trim().parse::<IpAddr>().ok())
            .map(|ip| ip.to_canonical())
    };
    let client = match is_private(peer).then(forwarded).flatten() {
        Some(forwarded) => forwarded,
        None => peer,
    };
    Some(match client {
        IpAddr::V6(ip) => IpAddr::V6((u128::from(ip) & !((1u128 << 64) - 1)).into()),
        ip => ip,
    })
}

/// Whether `ip` is this computer or on a private network, where a proxy
/// may stand in front of the node.
fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
        IpAddr::V6(ip) => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00,
    }
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

/// `429 Too Many Requests` for the call `id`, which may come again in
/// `wait` seconds.
fn too_many_requests(id: Value, wait: u64) -> Response {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32000, "message": format!("too many requests; try again in {wait} seconds") },
    });
    (
        StatusCode::TOO_MANY_REQUESTS,
        security_headers(),
        [(header::RETRY_AFTER, wait.to_string())],
        Json(body),
    )
        .into_response()
}

/// The call `id` failed, for the model to read why.
fn tool_error(id: Value, text: &str) -> Response {
    answer(
        StatusCode::OK,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "text", "text": text }], "isError": true },
        }),
    )
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

/// Whether the address asks for tool answers as text alone
/// (`/mcp?answers=text`).
fn wants_text_answers(uri: &Uri) -> bool {
    uri.query().is_some_and(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .any(|(key, value)| key == "answers" && value.eq_ignore_ascii_case("text"))
    })
}

/// Whether the address asks for searches without findings or leads, and
/// no `report_finding` (`/mcp?findings=off`).
fn wants_no_findings(uri: &Uri) -> bool {
    uri.query().is_some_and(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .any(|(key, value)| key == "findings" && value.eq_ignore_ascii_case("off"))
    })
}

async fn mcp(State(state): State<AppState>, request: Request) -> Response {
    if foreign_origin(request.headers()) {
        return answer(
            StatusCode::FORBIDDEN,
            rpc_error(
                Value::Null,
                -32600,
                "web pages of other sites cannot use this MCP server",
            ),
        );
    }
    let client = client(&request);
    let here = from_this_computer(&request);
    let reads_pages = state.settings.read_pages_for_all || here;
    let no_findings = wants_no_findings(request.uri());
    let keeps_findings = !no_findings && (state.settings.findings_for_all || here);
    let text_answers = wants_text_answers(request.uri());
    let Ok(body) = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await else {
        return answer(
            StatusCode::PAYLOAD_TOO_LARGE,
            rpc_error(Value::Null, -32600, "the message is too big"),
        );
    };
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return answer(StatusCode::BAD_REQUEST, parse_error());
    };
    let public_read = reads_pages && !here && Mcp::is_read_call(&message);
    // Held until the page is read.
    let mut _read_slot = None;
    if Mcp::is_tool_call(&message) {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let now = Instant::now();
        if let Err(wait) = state.mcp_limiter.take(client, now) {
            return too_many_requests(id, wait);
        }
        if public_read {
            if let Err(wait) = state.public_reads.limiter.take(client, now) {
                return too_many_requests(id, wait);
            }
            match Arc::clone(&state.public_reads.slots).try_acquire_owned() {
                Ok(slot) => _read_slot = Some(slot),
                Err(_) => {
                    return tool_error(
                        id,
                        "This Plumb node is reading as many pages as it can for others right \
                         now; try again in a few seconds.",
                    )
                }
            }
        }
        if state.setting_up().is_some() {
            // A notification gets no answer, as when the node is ready.
            if message.get("id").is_none() {
                return (StatusCode::ACCEPTED, security_headers()).into_response();
            }
            return tool_error(
                id,
                "This Plumb node is still setting up its index; try again in a few minutes.",
            );
        }
    }
    let country = state.settings.home.resolve(None);
    let (rates, plugins) = match Mcp::search_query(&message) {
        Some(query) => {
            let options = plumb_index::SearchOptions::default();
            tokio::join!(
                state.rates.for_query(&query),
                // MCP's search runs later, in the server: plugins go by
                // their keywords alone here.
                state.plugin_results(&query, &options, None, None)
            )
        }
        None => (None, Vec::new()),
    };
    let reader = if here {
        state.page_reader.get(&state.settings.page_reader)
    } else if reads_pages {
        state.public_reads.reader(&state.settings.page_reader)
    } else {
        None
    };
    let server = Mcp::new(Arc::clone(&state.backend), country)
        .with_reader(reader)
        .with_rates(rates)
        .with_node(state.node.clone())
        .with_findings(if keeps_findings {
            state.findings()
        } else {
            None
        })
        .with_leads(here && !no_findings)
        .with_plugin_results(plugins);
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let reply = tokio::task::spawn_blocking(move || server.handle(&message)).await;
    match reply {
        Ok(Some(mut reply)) => {
            if text_answers {
                text_only(&mut reply);
            }
            answer(StatusCode::OK, reply)
        }
        Ok(None) => (StatusCode::ACCEPTED, security_headers()).into_response(),
        Err(_) => {
            error!("an MCP request failed");
            answer(
                StatusCode::INTERNAL_SERVER_ERROR,
                rpc_error(id, -32603, "the request failed"),
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
            let mut request = Request::builder()
                .uri("/mcp")
                .header(header::HOST, "127.0.0.1:7586");
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
        // A web page whose name was rebound to 127.0.0.1.
        let mut rebound = request(Some("127.0.0.1:5000"), false);
        rebound
            .headers_mut()
            .insert(header::HOST, "evil.example:7586".parse().unwrap());
        assert!(!from_this_computer(&rebound));
        let mut by_name = request(Some("127.0.0.1:5000"), false);
        by_name
            .headers_mut()
            .insert(header::HOST, "localhost:7586".parse().unwrap());
        assert!(from_this_computer(&by_name));
    }

    #[test]
    fn clients_behind_a_proxy_are_told_apart() {
        let request = |peer: &str, forwarded: Option<&str>| {
            let mut request = Request::builder().uri("/mcp");
            if let Some(forwarded) = forwarded {
                request = request.header("x-forwarded-for", forwarded);
            }
            let mut request = request.body(axum::body::Body::empty()).unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
            request
        };
        let ip = |text: &str| Some(text.parse::<IpAddr>().unwrap());
        // Caddy on this computer, or in front of a Docker container.
        assert_eq!(
            client(&request("127.0.0.1:5000", Some("203.0.113.9"))),
            ip("203.0.113.9")
        );
        assert_eq!(
            client(&request("172.18.0.1:5000", Some("203.0.113.9"))),
            ip("203.0.113.9")
        );
        // Someone on the internet cannot pick their own address.
        assert_eq!(
            client(&request("198.51.100.7:5000", Some("203.0.113.9"))),
            ip("198.51.100.7")
        );
        // An IPv6 network counts as one client.
        assert_eq!(
            client(&request("[2001:db8:1:2:3:4:5:6]:5000", None)),
            ip("2001:db8:1:2::")
        );
    }

    #[test]
    fn answers_text_alone_when_the_address_asks() {
        let asks = |uri: &str| wants_text_answers(&uri.parse::<Uri>().unwrap());
        assert!(asks("/mcp?answers=text"));
        assert!(asks("/mcp?x=1&answers=Text"));
        assert!(!asks("/mcp"));
        assert!(!asks("/mcp?answers=json"));
        assert!(!asks("/mcp?text"));
    }

    #[test]
    fn leaves_findings_out_when_the_address_asks() {
        let asks = |uri: &str| wants_no_findings(&uri.parse::<Uri>().unwrap());
        assert!(asks("/mcp?findings=off"));
        assert!(asks("/mcp?answers=text&findings=OFF"));
        assert!(!asks("/mcp"));
        assert!(!asks("/mcp?findings=on"));
        assert!(!asks("/mcp?answers=text"));
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
