//! The page that shares a browser's profile with its searcher's other
//! browsers and nodes (see [`crate::sync`]):
//!
//! - `GET /link` lists the nodes the profile is shared with,
//! - `POST /link/new` makes a link code,
//! - `POST /link/join` uses one, and `POST /link/stop` stops sharing.
//!
//! Only on nodes that keep profiles, like the rest of [`super::history`].

use std::fmt::Write as _;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use plumb_core::now_unix;
use plumb_net::PeerId;
use serde::Deserialize;
use tracing::warn;

use super::history::{cross_site, no_store, refuse_cross_site, Visitor};
use super::{escape_html, html_response, page, time_ago, AppState};
use crate::sync::{self, Joined, Links, CODE_MINUTES};

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/link", get(link_page))
        .route("/link/new", post(new_code))
        .route("/link/join", post(join))
        .route("/link/stop", post(stop))
}

/// What the page says above its lists.
enum Note {
    None,
    /// A link code was just made.
    Code(String),
    Done(String),
    Problem(String),
}

/// `GET /link`.
async fn link_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    show(&state, &visitor, StatusCode::OK, Note::None)
}

/// `POST /link/new`: a link code for the browser's profile, made now if it
/// has none.
async fn new_code(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if cross_site(&headers) {
        return refuse_cross_site();
    }
    let Some(mut visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let made = visitor
        .profile_or_new()
        .ok_or_else(|| anyhow::anyhow!("no profile"))
        .and_then(|profile| sync::make_code(&profile));
    let note = match made {
        Ok(token) => Note::Code(sync::format_code(&token, me(&state).as_ref())),
        Err(err) => {
            warn!("could not make a link code: {err:#}");
            Note::Problem("No link code could be made. The server log has the details.".into())
        }
    };
    let response = show(&state, &visitor, StatusCode::OK, note);
    visitor.send_cookies(response)
}

#[derive(Debug, Deserialize)]
struct CodeForm {
    #[serde(default)]
    code: String,
}

/// `POST /link/join`: uses a link code, merging what the browser had into
/// the profile it joins.
async fn join(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CodeForm>,
) -> Response {
    if cross_site(&headers) {
        return refuse_cross_site();
    }
    let Some(mut visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let net = state.node.as_ref().and_then(|node| node.network());
    let joined = sync::join(
        visitor.dir(),
        net.as_deref(),
        me(&state),
        &form.code,
        visitor.profile(),
    )
    .await;
    let (status, note) = match joined {
        Ok(joined) => {
            visitor.use_profile(joined.profile());
            let said = match &joined {
                Joined::Here(_) => "Done. This browser now uses that profile.".to_owned(),
                Joined::There { node, .. } => format!(
                    "Done. This browser now uses the profile from node {}, and the two \
                     nodes keep it alike.",
                    short(&node.to_string())
                ),
            };
            (StatusCode::OK, Note::Done(said))
        }
        Err(err) => (StatusCode::BAD_REQUEST, Note::Problem(format!("{err:#}"))),
    };
    let response = show(&state, &visitor, status, note);
    visitor.send_cookies(response)
}

#[derive(Debug, Deserialize)]
struct StopForm {
    #[serde(default)]
    node: String,
}

/// `POST /link/stop`: stops sharing the profile with a node. Both copies
/// stay; they are no longer kept alike.
async fn stop(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<StopForm>,
) -> Response {
    if cross_site(&headers) {
        return refuse_cross_site();
    }
    let Some(visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (Some(profile), Ok(peer)) = (visitor.profile(), form.node.parse::<PeerId>()) else {
        return show(&state, &visitor, StatusCode::BAD_REQUEST, Note::None);
    };
    let net = state.node.as_ref().and_then(|node| node.network());
    let note = match sync::stop(visitor.dir(), net.as_deref(), profile, peer).await {
        Ok(()) => Note::Done(format!(
            "Stopped sharing with node {}. Its copy stays there until cleared on it.",
            short(&form.node)
        )),
        Err(err) => {
            warn!("could not stop sharing a profile: {err:#}");
            Note::Problem("That did not work. The server log has the details.".into())
        }
    };
    show(&state, &visitor, StatusCode::OK, note)
}

/// This node's id, when it is on the network.
fn me(state: &AppState) -> Option<PeerId> {
    state
        .node
        .as_ref()
        .and_then(|node| node.network())
        .map(|net| net.peer_id())
}

/// The start of a node id, enough to tell one's own nodes apart.
fn short(id: &str) -> String {
    let start: String = id.chars().take(16).collect();
    if start.len() < id.len() {
        format!("{start}\u{2026}")
    } else {
        start
    }
}

fn show(state: &AppState, visitor: &Visitor, status: StatusCode, note: Note) -> Response {
    let links = visitor
        .profile()
        .map(|profile| sync::links(visitor.dir(), profile))
        .unwrap_or_default();
    no_store(html_response(
        status,
        render(&links, me(state).is_some(), &note, now_unix()),
    ))
}

fn render(links: &Links, networked: bool, note: &Note, now: u64) -> String {
    let mut body = String::from(
        "<div class=\"wrap hist about\">\n<header><a class=\"logo\" href=\"/\">Plumb</a></header>\n\
         <main>\n<h1>Your profile on your other computers</h1>\n\
         <p class=\"s\">Your history, About you, and what Plumb learned from your clicks and \
         ratings can follow you to your other browsers, and to your other Plumb nodes, such \
         as the app on your laptop and a node at home. It goes only to the nodes you link \
         here, over encrypted connections, and never to public servers.</p>\n",
    );
    match note {
        Note::None => {}
        Note::Code(code) => {
            let _ = write!(
                body,
                "<h2>Your link code</h2>\n\
                 <p><input id=\"code\" readonly value=\"{}\" aria-label=\"Link code\"></p>\n\
                 <p class=\"m\">Open the link page (/link) in the other browser or on the other \
                 node and paste it there. It works once, for {CODE_MINUTES} minutes. Anyone \
                 with it could take your profile until then, so share it only with \
                 yourself.</p>\n",
                escape_html(code)
            );
        }
        Note::Done(said) => {
            let _ = writeln!(
                body,
                "<p class=\"s\"><strong>{}</strong></p>",
                escape_html(said)
            );
        }
        Note::Problem(said) => {
            let _ = writeln!(
                body,
                "<p class=\"s\"><strong>That did not work: {}</strong></p>",
                escape_html(said)
            );
        }
    }
    body.push_str("<h2>Shared with</h2>\n");
    if links.nodes.is_empty() {
        body.push_str("<p class=\"none\">No other node yet.</p>\n");
    } else {
        body.push_str("<ul>\n");
        for link in &links.nodes {
            let state = match (&link.problem, link.last_synced()) {
                (Some(problem), _) => format!("not synced lately: {problem}"),
                (None, 0) => "not synced yet".to_owned(),
                (None, at) => format!("synced {}", time_ago(at, now)),
            };
            let _ = writeln!(
                body,
                "<li><strong title=\"{id}\">Node {}</strong> <span class=\"m\">{}</span> \
                 <form method=\"post\" action=\"/link/stop\" class=\"inline\">\
                 <input type=\"hidden\" name=\"node\" value=\"{id}\">\
                 <button type=\"submit\">Stop sharing</button></form></li>",
                escape_html(&short(&link.peer)),
                escape_html(&state),
                id = escape_html(&link.peer),
            );
        }
        body.push_str("</ul>\n");
    }
    body.push_str(
        "<h2>Link another browser or node</h2>\n\
         <form method=\"post\" action=\"/link/new\"><button type=\"submit\">Make a link \
         code</button></form>\n",
    );
    if !networked {
        body.push_str(
            "<p class=\"m\">This node is not on the Plumb network, so its code works only in \
             other browsers that search here.</p>\n",
        );
    }
    body.push_str(
        "<h2>Have a link code?</h2>\n\
         <form method=\"post\" action=\"/link/join\">\n\
         <label for=\"paste\"><strong>Paste it here</strong></label>\n\
         <p class=\"m\">This browser then uses that profile. What it had here is merged \
         in, so nothing is lost.</p>\n\
         <input id=\"paste\" name=\"code\" autocomplete=\"off\" \
         placeholder=\"ABCD-1234@12D3KooW\u{2026}\">\n\
         <p><button type=\"submit\">Use this code</button></p>\n</form>\n\
         <p class=\"m\"><a href=\"/history\">Your history</a> \u{b7} \
         <a href=\"/about\">About you</a></p>\n</main>\n</div>",
    );
    page("Your other computers - Plumb Search", &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::Link;

    #[test]
    fn the_page_escapes_what_it_shows() {
        let links = Links {
            nodes: vec![Link {
                peer: "<b>".into(),
                problem: Some("<i>".into()),
                ..Link::default()
            }],
        };
        let html = render(&links, true, &Note::Problem("<script>".into()), 10);
        assert!(!html.contains("<script>") && !html.contains("<b>") && !html.contains("<i>"));
        assert!(html.contains("Stop sharing"));
        let html = render(&Links::default(), false, &Note::Code("AB-CD@x".into()), 10);
        assert!(html.contains("AB-CD@x") && html.contains("not on the Plumb network"));
    }
}
