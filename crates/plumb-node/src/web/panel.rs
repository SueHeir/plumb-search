//! The node's panel, `GET /app`: what works now, how far setup has come,
//! the settings and how to use Plumb from a browser. The desktop app's
//! window shows it; searching happens in the browser.
//!
//! - `GET /app` shows the panel, reloading itself while work is under way
//!   (the page runs no script),
//! - `POST /app/settings` saves the settings form,
//! - `POST /app/refresh` starts a refresh now,
//! - `POST /app/network/retry` tries the network's bootstrap nodes again,
//! - `POST /app/pause` pauses background work for an hour or until
//!   tomorrow, or resumes it,
//! - `POST /app/restart` restarts the node to apply saved feature changes,
//!   where the program running it allows,
//! - `POST /app/retry` tries failed work again now,
//! - `POST /app/backup` saves a backup in the data folder, `GET
//!   /app/backups/<name>` downloads one, and `POST /app/backups/restore`
//!   (a saved one, by name) and `POST /app/restore` (an uploaded one)
//!   restore one,
//! - `GET /add-to-firefox` says how to add Plumb to Firefox as a search
//!   engine. It is meant to be opened in Firefox (the desktop app opens it
//!   there), which offers to add the engine of a page that links to an
//!   OpenSearch description, as every Plumb page does. No page can add it by
//!   itself: Firefox dropped `window.external.AddSearchProvider`.
//!
//! Changes are taken only from this computer (a loopback peer) and only from
//! the panel itself (no `Origin` of another site), so that neither another
//! machine on the network nor a web page open in a browser can change them.

mod actions;
mod forms;
mod guard;
mod settings_view;
mod view;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use plumb_core::now_unix;
use serde::Deserialize;

use self::actions::parse_limit;
pub(super) use self::actions::{
    apply_features_form, backup, download_backup, pause, paused, refresh, restart, restore_saved,
    restore_upload, retry, retry_network, save_features, save_plugins, save_remote_control,
    save_settings, settings_from_form, FeaturesForm, PauseForm, RetryForm, SettingsForm,
};
pub(super) use self::forms::{features_error, settings_error};
pub(super) use self::guard::{forbidden, local_origin, panel_error, refusal, refusal_of};
use self::guard::{panel_headers, PANEL_REFERRER_POLICY};
pub(super) use self::view::{render_panel, PanelView, RemoteControlView, LAYOUT_STYLE};
use super::{
    escape_html, group_thousands, page_with_head, request_origin, security_headers, AppState,
};
use crate::node::backup;
use crate::node::control;
use crate::node::Status;

/// Seconds between two reloads of the panel while work is under way.
const BUSY_RELOAD_SECONDS: u32 = 30;

/// Seconds between two reloads otherwise.
const IDLE_RELOAD_SECONDS: u32 = 300;

pub(super) const PANEL_STYLE: &str = include_str!("panel.css");

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct PanelQuery {
    section: String,
    saved: String,
    refresh: String,
}

pub(super) async fn panel(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(origin) = request_origin(request.headers(), request.uri()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let query = match axum::extract::Query::<PanelQuery>::try_from_uri(request.uri()) {
        Ok(query) => query.0,
        Err(_) => {
            return panel_error(
                StatusCode::BAD_REQUEST,
                "The panel address could not be read.",
            )
        }
    };
    let active = node.features();
    let saved = match node.saved_features() {
        Ok(saved) => saved,
        Err(err) => {
            return panel_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Could not read feature settings: {err}"),
            )
        }
    };
    let data_dir = node.data_dir();
    let writable = refusal(&request).is_none();
    // Other nodes are only listed for someone who could control them.
    let switcher = if writable && node.manages_other_nodes() {
        super::nodes::switcher(node.as_ref(), None)
    } else {
        String::new()
    };
    let remote_control = RemoteControlView {
        on: data_dir
            .as_deref()
            .and_then(|dir| control::load(dir).ok().flatten())
            .map(|control| control.allow_public),
        loopback_only: node.bind().is_some_and(|addr| addr.ip().is_loopback()),
    };
    let page = render_panel(&PanelView {
        status: &node.status(),
        settings: &node.settings().unwrap_or_default(),
        origin: &origin,
        data_dir: data_dir.as_deref(),
        now: now_unix(),
        query: &query,
        active: &active,
        saved: &saved,
        writable,
        private_ready: state.private_search(),
        base: "/app",
        eyebrow: if switcher.is_empty() {
            "YOUR SEARCH NODE"
        } else {
            "THIS COMPUTER"
        },
        switcher: &switcher,
        remote_control: data_dir.is_some().then_some(&remote_control),
        activity: &node.activity_log(),
        backups: (writable && data_dir.is_some())
            .then(|| backup::list(data_dir.as_deref().expect("checked")))
            .as_deref(),
        plugins: Some(&state.settings.plugins),
    });
    panel_page(page)
}

/// A panel page as served: never cached, and sending its origin with forms.
pub(super) fn panel_page(page: String) -> Response {
    (
        StatusCode::OK,
        panel_headers(),
        [(header::CACHE_CONTROL, "no-store")],
        Html(page),
    )
        .into_response()
}

/// The address of the page with the steps to add Plumb to Firefox.
pub const ADD_TO_FIREFOX_PATH: &str = "/add-to-firefox";

pub(super) async fn add_to_firefox(headers: HeaderMap, uri: Uri) -> Response {
    let Some(origin) = request_origin(&headers, &uri) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    (
        StatusCode::OK,
        security_headers(),
        Html(render_add_to_firefox(&origin)),
    )
        .into_response()
}

fn render_add_to_firefox(origin: &str) -> String {
    let origin = escape_html(origin);
    let body = format!(
        "<main class=\"wrap node-panel\">\n<h1>Add Plumb Search to Firefox</h1>\n\
         <p>Firefox does not let a page add a search engine by itself, so it takes two \
         clicks in this window:</p>\n\
         <ol class=\"howto\">\n\
         <li>Right-click the address bar at the top of this window.</li>\n\
         <li>Choose <strong>Add \"Plumb Search\"</strong>.</li>\n\
         </ol>\n\
         <p>To make Plumb the search engine Firefox uses for the address bar, open \
         <strong>Settings &gt; Search</strong> and pick <strong>Plumb Search</strong> as the \
         default search engine.</p>\n\
         <h2>If Firefox does not offer \"Add\"</h2>\n\
         <p>In <strong>Settings &gt; Search</strong>, under <strong>Search Shortcuts</strong>, \
         click <strong>Add</strong> and enter the name <strong>Plumb Search</strong> and this \
         address:</p>\n<p><code>{origin}/search?q=%s</code></p>\n\
         <p><a class=\"btn\" href=\"{origin}/\">Go to Plumb Search</a></p>\n</main>"
    );
    let head = format!("<style>{PANEL_STYLE}</style>\n");
    page_with_head("Add Plumb Search to Firefox", &head, &body)
}

/// [`Status::items`] with what they are: "1,260,123: 260,123 sites and
/// 1,000,000 pages".
fn items_in_words(status: &Status) -> String {
    let pages = status.page_coverage.as_ref().map_or(0, |c| c.indexed_pages);
    let parts: Vec<String> = [
        (status.sites, "sites"),
        (pages, "pages"),
        (status.places, "places"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, what)| format!("{} {what}", group_thousands(count)))
    .collect();
    let total = group_thousands(status.items());
    match parts.split_last() {
        Some((last, rest)) if !rest.is_empty() => {
            format!("{total}: {} and {last}", rest.join(", "))
        }
        _ => total,
    }
}

#[cfg(test)]
mod tests;
