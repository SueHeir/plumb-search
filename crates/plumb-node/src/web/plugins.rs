//! The buttons on plugins' results, and plugin lookups for a page.
//!
//! - `POST /plugins/act` presses a button from a results page: the form
//!   names the plugin, carries the button's data and the node's button
//!   token, and the answer is a short page saying what the plugin did,
//!   which goes back to the results.
//! - `POST /api/plugins/act` presses one from a program, such as a browser
//!   extension: `{"plugin": "...", "data": ...}` as JSON, answered with
//!   `{"message": "..."}` or `{"error": "..."}`.
//! - `GET /api/plugins/page?url=` says what the plugins that know a site
//!   say about one of its pages, for an extension to show beside it.
//!
//! Buttons are for the node's owner: they are shown, and pressed, only on
//! the computer the node runs on, as the panel's settings are. A page of
//! another site cannot press them: the form needs the token, and the JSON
//! API needs a JSON body, which a page can only send another site after
//! asking, and the node never says yes.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, Extensions, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use super::panel::{local_origin, refusal_of};
use super::{
    escape_html, html_response, page_with_head, request_origin, security_headers, AppState,
};

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/plugins/act", post(act_form))
        .route("/api/plugins/act", post(act_json))
        .route("/api/plugins/page", get(page))
}

fn peer(extensions: &Extensions) -> Option<SocketAddr> {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer)
}

/// The token to put in a results page's buttons, when the page is for
/// the node's owner and some plugin has buttons; `None` shows the results
/// without them.
pub(super) fn button_token(
    state: &AppState,
    extensions: &Extensions,
    headers: &HeaderMap,
    uri: &Uri,
) -> Option<String> {
    let plugins = &state.settings.plugins;
    if !plugins.any_act() || plugins.token().is_empty() {
        return None;
    }
    refusal_of(peer(extensions), headers, uri)
        .is_none()
        .then(|| plugins.token().to_string())
}

#[derive(Debug, Deserialize)]
struct ActForm {
    plugin: String,
    data: String,
    token: String,
    /// The search the button was on, to go back to.
    #[serde(default)]
    q: String,
}

async fn act_form(
    State(state): State<AppState>,
    extensions: Extensions,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<ActForm>,
) -> Response {
    let back = format!(
        "/search?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", &form.q)
            .finish()
    );
    let plugins = &state.settings.plugins;
    let refused = match refusal_of(peer(&extensions), &headers, &uri) {
        Some(_) => Some("Buttons work only on the computer Plumb runs on, from its own pages."),
        None => (!plugins.accepts(&form.token)).then_some("This button is out of date."),
    };
    let (status, said) = match refused {
        Some(why) => (StatusCode::FORBIDDEN, why.to_string()),
        None => match plugins.act(&form.plugin, &form.data).await {
            Ok(message) => (StatusCode::OK, message),
            Err(error) => {
                warn!("plugin {} button failed: {error:#}", form.plugin);
                (
                    StatusCode::BAD_GATEWAY,
                    format!("It did not work: {error:#}"),
                )
            }
        },
    };
    let name = plugins
        .list()
        .find(|p| p.id == form.plugin)
        .map(|p| p.manifest.name.clone())
        .unwrap_or_else(|| form.plugin.clone());
    let back = escape_html(&back);
    // Back to the results on its own after a moment; the page runs no
    // script.
    let head = format!("<meta http-equiv=\"refresh\" content=\"3;url={back}\">\n");
    let body = format!(
        "<div class=\"wrap\">\n<main>\n<h1>{}</h1>\n<p>{}</p>\n\
         <p><a href=\"{back}\">Back to the results</a></p>\n</main>\n</div>",
        escape_html(&name),
        escape_html(&said)
    );
    html_response(status, page_with_head(&name, &head, &body))
}

#[derive(Debug, Deserialize)]
struct ActRequest {
    plugin: String,
    #[serde(default)]
    data: serde_json::Value,
}

async fn act_json(
    State(state): State<AppState>,
    extensions: Extensions,
    headers: HeaderMap,
    uri: Uri,
    Json(request): Json<ActRequest>,
) -> Response {
    if let Some(why) = api_refusal(peer(&extensions), &headers, &uri) {
        return answer(StatusCode::FORBIDDEN, json!({ "error": why }));
    }
    let data = request.data.to_string();
    match state.settings.plugins.act(&request.plugin, &data).await {
        Ok(message) => answer(StatusCode::OK, json!({ "message": message })),
        Err(error) => {
            warn!("plugin {} button failed: {error:#}", request.plugin);
            answer(
                StatusCode::BAD_GATEWAY,
                json!({ "error": format!("{error:#}") }),
            )
        }
    }
}

/// Why a program's button press is refused, or `None`: it must come from
/// this computer, not through a proxy, to a local name, and not from a
/// web page of another site. (Its JSON body already keeps pages of other
/// sites out: a browser asks first, and the node never says yes.) A
/// browser extension's own origin is fine.
fn api_refusal(peer: Option<SocketAddr>, headers: &HeaderMap, uri: &Uri) -> Option<&'static str> {
    let local = peer.is_some_and(|peer| peer.ip().to_canonical().is_loopback());
    let proxied = super::control::FORWARDED_HEADERS
        .iter()
        .any(|name| headers.contains_key(*name));
    if !local || proxied {
        return Some("buttons can only be pressed on the computer Plumb runs on");
    }
    let own = request_origin(headers, uri);
    if !own.as_deref().is_some_and(local_origin) {
        return Some("buttons can only be pressed through a local address");
    }
    let origin = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok());
    let allowed = origin.is_none_or(|origin| {
        Some(origin) == own.as_deref()
            || origin.starts_with("chrome-extension://")
            || origin.starts_with("moz-extension://")
            || origin.starts_with("safari-web-extension://")
    });
    (!allowed).then_some("buttons cannot be pressed from another site")
}

#[derive(Debug, Deserialize)]
struct PageParams {
    url: String,
}

async fn page(State(state): State<AppState>, Query(params): Query<PageParams>) -> Response {
    let found = state
        .settings
        .plugins
        .page(&params.url, plumb_core::SafeSearch::Moderate, None)
        .await;
    answer(StatusCode::OK, json!({ "plugins": found }))
}

fn answer(status: StatusCode, body: serde_json::Value) -> Response {
    (status, security_headers(), Json(body)).into_response()
}
