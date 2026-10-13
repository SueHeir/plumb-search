//! Who may change the node (only this computer, and only from the panel
//! itself), and the headers and error page every panel answer has.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request};
use axum::http::{header, HeaderMap, HeaderName, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use url::{Host, Url};

use super::PANEL_STYLE;
use crate::web::{escape_html, page_with_head, request_origin, security_headers};

/// [`security_headers`] with one change: the panel's forms send their
/// origin, which [`refusal`] checks. Under the other pages' `no-referrer`
/// policy, browsers send `Origin: null` with every form post.
pub(super) fn panel_headers() -> [(HeaderName, &'static str); 3] {
    security_headers().map(|(name, value)| {
        if name == header::REFERRER_POLICY {
            (name, PANEL_REFERRER_POLICY)
        } else {
            (name, value)
        }
    })
}

/// Sends the origin, and no more, to this node only.
pub(super) const PANEL_REFERRER_POLICY: &str = "same-origin";

/// A page saying what went wrong with a change, with the way back to the
/// panel: the desktop app's window has no back button.
pub(in crate::web) fn panel_error(status: StatusCode, message: &str) -> Response {
    let bar = crate::web::app_bar("", false);
    let body = format!(
        "<div class=\"wrap node-panel\">{bar}<main class=\"node-standalone\">\n<h1>Plumb Search node</h1>\n\
         <p class=\"err\">{}</p>\n\
         <p><a class=\"btn\" href=\"/app\">Back to the panel</a></p>\n</main></div>",
        escape_html(message)
    );
    let head = format!(
        "<meta name=\"referrer\" content=\"{PANEL_REFERRER_POLICY}\">\n\
         <style>{PANEL_STYLE}</style>\n"
    );
    (
        status,
        panel_headers(),
        [(header::CACHE_CONTROL, "no-store")],
        Html(page_with_head("Plumb Search node", &head, &body)),
    )
        .into_response()
}

/// Why a change is refused: it does not come from this computer, or a page
/// of another site sent it. `None` when it may go ahead.
pub(in crate::web) fn refusal(request: &Request) -> Option<&'static str> {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    refusal_of(peer, request.headers(), request.uri())
}

/// [`refusal`] for a request from `peer` with `headers`, sent to `uri`.
pub(in crate::web) fn refusal_of(
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    uri: &Uri,
) -> Option<&'static str> {
    // A dual-stack `[::]` listener sees IPv4 peers as `::ffff:127.0.0.1`.
    let local = peer.is_some_and(|peer| peer.ip().to_canonical().is_loopback());
    // A reverse proxy on this computer connects from loopback too, and may
    // name 127.0.0.1 as the host: what it passes on came from elsewhere.
    let proxied = crate::web::control::FORWARDED_HEADERS
        .iter()
        .any(|name| headers.contains_key(*name));
    if !local || proxied {
        return Some("Settings can only be changed on the computer Plumb runs on.");
    }
    let own = request_origin(headers, uri);
    // A page whose DNS name was rebound to 127.0.0.1 is same-origin with
    // itself, so the Origin check alone would let it through: the page must
    // also have been opened by a local name.
    if !own.as_deref().is_some_and(local_origin) {
        return Some("Settings can only be changed from Plumb's own settings page.");
    }
    let origin = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok());
    let cross_site = headers
        .get("sec-fetch-site")
        .and_then(|site| site.to_str().ok())
        .is_some_and(|site| !matches!(site, "same-origin" | "none"));
    if cross_site || origin.is_some_and(|origin| Some(origin) != own.as_deref()) {
        return Some("Settings can only be changed from Plumb's own settings page.");
    }
    None
}

/// Whether `origin` names this computer: `localhost` or a loopback address.
pub(in crate::web) fn local_origin(origin: &str) -> bool {
    match Url::parse(origin)
        .ok()
        .and_then(|url| url.host().map(|h| h.to_owned()))
    {
        Some(Host::Domain(name)) => {
            let name = name.trim_end_matches('.');
            name == "localhost" || name.ends_with(".localhost")
        }
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.to_canonical().is_loopback(),
        None => false,
    }
}

pub(in crate::web) fn forbidden(why: &str) -> Response {
    panel_error(StatusCode::FORBIDDEN, why)
}
