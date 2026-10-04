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

use std::net::SocketAddr;
use std::path::Path;

use axum::extract::{ConnectInfo, FromRequest, Request, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use plumb_core::now_unix;
use serde::Deserialize;
use tracing::warn;
use url::{Host, Url};

use super::{
    escape_html, group_thousands, page_with_head, request_origin, security_headers, time_ago,
    time_until, AppState,
};
use crate::node::backup::{self, Backup, BackupInfo};
use crate::node::control;
use crate::node::features::FeatureSettings;
use crate::node::{
    CrawlHours, LogEntry, LogLevel, NodeSettings, Phase, Retry, Status, Step, Workload, MB,
};
use crate::pages::{thousands, PageSetSize, SETS, SIZE_CHOICES};

/// Seconds between two reloads of the panel while work is under way.
const BUSY_RELOAD_SECONDS: u32 = 5;
/// Seconds between two reloads otherwise.
const IDLE_RELOAD_SECONDS: u32 = 60;

pub(super) const PANEL_STYLE: &str = "\
.node-panel{max-width:56rem;padding-top:1.5rem}\
.node-panel h1{font-size:1.6rem}\
.node-panel h2{font-size:1.05rem;margin:2rem 0 .5rem}\
.cards{display:grid;grid-template-columns:repeat(auto-fill,minmax(15rem,1fr));gap:.75rem;\
margin:1.25rem 0}\
.card{padding:.9rem 1rem;border:1px solid var(--line);border-radius:.75rem}\
.card h3{margin:0;font-size:.8rem;font-weight:600;text-transform:uppercase;\
letter-spacing:.04em;color:var(--muted)}\
.card .big{margin:.3rem 0 .2rem;font-size:1.35rem;font-weight:600}\
.card p{margin:.2rem 0;font-size:.9rem}\
.card.search{grid-column:1/-1}\
.ready .big{color:var(--url)}.limited .big{color:var(--accent)}.warn .big{color:var(--err)}\
.btns{display:flex;flex-wrap:wrap;gap:.5rem;margin-top:.6rem}\
.btn{display:inline-block;padding:.5rem .9rem;border-radius:.5rem;background:var(--accent);\
color:var(--bg);text-decoration:none}\
.btn.alt,button.alt{background:none;color:var(--accent);border:1px solid var(--accent)}\
.steps li{display:flex;gap:.6rem;padding:.45rem 0;border:0}\
.steps .i{flex:none;width:1.2rem;text-align:center}\
.steps .done{color:var(--muted)}\
.steps .now{font-weight:600}\
.steps small{display:block;font-weight:400;color:var(--muted)}\
.node-panel form{display:block}\
.node-panel label{display:flex;gap:.6rem;align-items:center;margin-top:.75rem}\
.node-panel label input[type=checkbox]{flex:none}\
.node-panel input[type=number]{flex:none;width:7rem}\
.node-panel form button{margin-top:.9rem}\
.hint{margin:.2rem 0 0;font-size:.85rem;color:var(--muted)}\
.howto{list-style:decimal;padding-left:1.5rem}.howto li{border:0;padding:.3rem 0}\
code{overflow-wrap:anywhere;font:.9rem ui-monospace,monospace;padding:.1rem .3rem;\
border:1px solid var(--line);border-radius:.3rem}\
dl{display:grid;grid-template-columns:max-content 1fr;gap:.25rem 1rem;font-size:.9rem}\
dt{color:var(--muted)}dd{margin:0;overflow-wrap:anywhere}";

/// The settings form as posted. A checkbox that is not ticked is not sent.
#[derive(Debug, Deserialize)]
pub(super) struct SettingsForm {
    #[serde(default)]
    background_updates: Option<String>,
    /// Megabytes; empty for no limit.
    #[serde(default)]
    download_limit_mb_per_day: String,
    /// Megabytes; empty for no limit.
    #[serde(default)]
    storage_limit_mb: String,
    /// A [`Workload`] name; empty keeps the limits as typed.
    #[serde(default)]
    workload: String,
    /// Ticked to crawl only between `crawl_from` and `crawl_to`.
    #[serde(default)]
    crawl_hours: Option<String>,
    #[serde(default)]
    crawl_from: String,
    #[serde(default)]
    crawl_to: String,
    /// Ticked to fill free space from the network.
    #[serde(default)]
    fill_from_network: Option<String>,
    /// Sent by forms that show the fill checkbox; without it the setting
    /// is kept as it is.
    #[serde(default)]
    fill_shown: Option<String>,
    /// Focus topics, one per line or comma.
    #[serde(default)]
    focus_topics: String,
    /// Sent by forms that show the focus topics; without it they are kept.
    #[serde(default)]
    focus_shown: Option<String>,
    /// Sent by forms that show the page sets; without it they are kept.
    #[serde(default)]
    page_sets_shown: Option<String>,
    /// `page_set.<id>`: a [`PageSetSize`] for each page set shown.
    #[serde(flatten)]
    other: std::collections::HashMap<String, String>,
}

/// The retry form: `work`, `wikidata` or `meaning`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct RetryForm {
    what: String,
}

impl RetryForm {
    pub(super) fn what(&self) -> Option<Retry> {
        match self.what.as_str() {
            "work" => Some(Retry::Work),
            "wikidata" => Some(Retry::Wikidata),
            "meaning" => Some(Retry::Meaning),
            _ => None,
        }
    }
}

/// The form restoring a saved backup.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct RestoreForm {
    name: String,
}

/// The pause form: `hour`, `tomorrow` or `resume`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct PauseForm {
    until: String,
}

/// `settings` paused or resumed as the pause form asks, at `now`; `None`
/// when the form makes no sense.
pub(super) fn paused(settings: &NodeSettings, form: &PauseForm, now: u64) -> Option<NodeSettings> {
    let until = match form.until.as_str() {
        "hour" => Some(now + 3600),
        "tomorrow" => Some(next_morning(now)),
        "resume" => None,
        _ => return None,
    };
    Some(NodeSettings {
        paused_until: until,
        ..settings.clone()
    })
}

/// 06:00 tomorrow on the node's clock, roughly: the seconds left of today
/// and six hours.
fn next_morning(now: u64) -> u64 {
    let (hour, minute, second) = crate::node::schedule::local_time();
    let today = u64::from(hour) * 3600 + u64::from(minute) * 60 + u64::from(second);
    now + (24 * 3600 - today) + 6 * 3600
}

/// A limit typed into the form, in megabytes: empty or 0 for none.
fn parse_limit(text: &str) -> Option<u64> {
    let text = text.trim().replace([',', '_', ' '], "");
    if text.is_empty() {
        return Some(0);
    }
    text.parse().ok()
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct PanelQuery {
    section: String,
    saved: String,
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

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct FeaturesForm {
    section: String,
    network: Option<String>,
    search_by_meaning: Option<String>,
    private_search: Option<String>,
    share_popularity: Option<String>,
    search_history: Option<String>,
    /// Find nodes through the Plumb network's own bootstrap nodes.
    plumb_bootstrap: Option<String>,
    /// Further bootstrap nodes, one per line.
    bootstrap: String,
    /// Set by the form that shows the trust choices, so that a form without
    /// them leaves trust as it was.
    trust_shown: Option<String>,
    default_trust: Option<String>,
    trusted: String,
}

pub(super) async fn save_features(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(Form(form)) = Form::<FeaturesForm>::from_request(request, &state).await else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "The feature settings could not be read.",
        );
    };
    let mut features = match node.saved_features() {
        Ok(features) => features,
        Err(err) => return panel_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let section = match apply_features_form(&form, &mut features) {
        Ok(section) => section,
        Err(response) => return response,
    };
    if let Err(err) = node.change_features(features) {
        return panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not save feature settings: {err}"),
        );
    }
    Redirect::to(&format!("/app?section={section}&saved=features")).into_response()
}

/// Puts the choices of the features form, from section "search" or
/// "network", into `features`, and returns the section; the error page when
/// they cannot work together.
// A response is big, but these run once per request.
#[allow(clippy::result_large_err)]
pub(super) fn apply_features_form(
    form: &FeaturesForm,
    features: &mut FeatureSettings,
) -> Result<&'static str, Response> {
    let section = if form.section == "search" {
        "search"
    } else {
        "network"
    };
    if section == "search" {
        features.search_by_meaning = form.search_by_meaning.is_some();
        features.private_search = form.private_search.is_some();
        features.search_history = Some(form.search_history.is_some());
    } else {
        features.network = form.network.is_some();
        features.share_popularity = form.share_popularity.is_some();
        features.set_bootstrap(
            form.plumb_bootstrap.is_some(),
            form.bootstrap.split_whitespace(),
        );
        if form.trust_shown.is_some() {
            features.no_default_trust = form.default_trust.is_none();
            features.trusted = form.trusted.split_whitespace().map(str::to_owned).collect();
        }
    }
    if let Err(err) = features.check() {
        return Err(panel_error(StatusCode::BAD_REQUEST, &err.to_string()));
    }
    Ok(section)
}

/// [`security_headers`] with one change: the panel's forms send their
/// origin, which [`refusal`] checks. Under the other pages' `no-referrer`
/// policy, browsers send `Origin: null` with every form post.
fn panel_headers() -> [(HeaderName, &'static str); 3] {
    security_headers().map(|(name, value)| {
        if name == header::REFERRER_POLICY {
            (name, PANEL_REFERRER_POLICY)
        } else {
            (name, value)
        }
    })
}

/// Sends the origin, and no more, to this node only.
const PANEL_REFERRER_POLICY: &str = "same-origin";

/// A page saying what went wrong with a change, with the way back to the
/// panel: the desktop app's window has no back button.
pub(super) fn panel_error(status: StatusCode, message: &str) -> Response {
    let body = format!(
        "<main class=\"wrap node-panel\">\n<h1>Plumb Search node</h1>\n\
         <p class=\"err\">{}</p>\n\
         <p><a class=\"btn\" href=\"/app\">Back to the panel</a></p>\n</main>",
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

pub(super) async fn save_settings(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(Form(form)) = Form::<SettingsForm>::from_request(request, &state).await else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "The settings form could not be read.",
        );
    };
    let current = node.settings().unwrap_or_default();
    let settings = match settings_from_form(&form, &current) {
        Ok(settings) => settings,
        Err(response) => return response,
    };
    if let Err(err) = node.change_settings(settings) {
        warn!("could not save the settings: {err:#}");
        return panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not save the settings: {err:#}"),
        );
    }
    Redirect::to("/app?section=resources&saved=settings").into_response()
}

/// The settings the settings form asks for; the error page when its limits
/// are not numbers.
// A response is big, but these run once per request.
#[allow(clippy::result_large_err)]
pub(super) fn settings_from_form(
    form: &SettingsForm,
    current: &NodeSettings,
) -> Result<NodeSettings, Response> {
    let (Some(download), Some(storage)) = (
        parse_limit(&form.download_limit_mb_per_day),
        parse_limit(&form.storage_limit_mb),
    ) else {
        return Err(panel_error(
            StatusCode::BAD_REQUEST,
            "Limits are whole numbers of megabytes, or empty for none. Nothing was changed.",
        ));
    };
    let hour = |text: &str| text.trim().parse::<u8>().ok().filter(|h| *h < 24);
    let crawl_hours = match &form.crawl_hours {
        None => None,
        Some(_) => match (hour(&form.crawl_from), hour(&form.crawl_to)) {
            (Some(from), Some(to)) => Some(CrawlHours { from, to }),
            _ => {
                return Err(panel_error(
                    StatusCode::BAD_REQUEST,
                    "Crawl hours are whole hours from 0 to 23. Nothing was changed.",
                ))
            }
        },
    };
    let mut page_sets = current.page_sets.clone();
    if form.page_sets_shown.is_some() {
        for set in SETS {
            let Some(size) = form.other.get(&format!("page_set.{}", set.id)) else {
                continue;
            };
            match size.parse::<PageSetSize>() {
                Ok(size) => page_sets.set(set.id, size),
                Err(_) => {
                    return Err(panel_error(
                        StatusCode::BAD_REQUEST,
                        "A page set size is automatic, off, all or a number of pages. Nothing \
                         was changed.",
                    ))
                }
            }
        }
    }
    let mut settings = NodeSettings {
        background_updates: form.background_updates.is_some(),
        download_limit_mb_per_day: download,
        storage_limit_mb: storage,
        workload: Workload::Custom,
        crawl_hours,
        // A pause stays until it ends or is lifted.
        paused_until: current.paused_until,
        fill_from_network: if form.fill_shown.is_some() {
            form.fill_from_network.is_some()
        } else {
            current.fill_from_network
        },
        // Saving the resources is a choice of how much to keep too.
        setup_chosen: true,
        page_sets,
        focus_topics: if form.focus_shown.is_some() {
            crate::about::About::topics_from_text(&form.focus_topics)
        } else {
            current.focus_topics.clone()
        },
    };
    settings.set_workload(Workload::from_name(&form.workload).unwrap_or(Workload::Custom));
    Ok(settings)
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct RemoteControlForm {
    /// `on` makes a new token; anything else turns remote control off.
    action: String,
    allow_public: Option<String>,
}

/// `POST /app/remote-control`: turns remote control on with a new token,
/// shown on the page this answers with and nowhere else, or off.
pub(super) async fn save_remote_control(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Some(dir) = node.data_dir() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let origin = request_origin(request.headers(), request.uri()).unwrap_or_default();
    let Ok(Form(form)) = Form::<RemoteControlForm>::from_request(request, &state).await else {
        return panel_error(StatusCode::BAD_REQUEST, "The form could not be read.");
    };
    if form.action != "on" {
        if let Err(err) = control::turn_off(&dir) {
            return panel_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Could not turn remote control off: {err:#}"),
            );
        }
        return Redirect::to("/app?section=remote").into_response();
    }
    let allow_public = form.allow_public.is_some();
    match control::turn_on(&dir, allow_public) {
        Ok(token) => {
            let https = node.https_bind().map(|https| {
                let fingerprint = crate::tls::load_or_create(&dir)
                    .map(|cert| cert.fingerprint())
                    .unwrap_or_else(|err| format!("unreadable: {err:#}"));
                (https, fingerprint)
            });
            panel_page(render_new_token(&token, &origin, node.bind(), https))
        }
        Err(err) => panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not turn remote control on: {err:#}"),
        ),
    }
}

/// The page that shows a new token, the one time it can be read.
fn render_new_token(
    token: &str,
    origin: &str,
    bind: Option<SocketAddr>,
    https: Option<(SocketAddr, String)>,
) -> String {
    let address = match (&https, bind) {
        (Some((https, _)), _) => format!(
            "https://{}:{}",
            if https.ip().is_unspecified() {
                "&lt;this computer's address&gt;".to_string()
            } else {
                escape_html(&https.ip().to_string())
            },
            https.port()
        ),
        (None, Some(addr)) if addr.ip().is_unspecified() => {
            format!("http://&lt;this computer's address&gt;:{}", addr.port())
        }
        _ => escape_html(origin),
    };
    let fingerprint = https
        .map(|(_, fingerprint)| {
            format!(
                "<dt>Certificate fingerprint</dt><dd><code>{}</code></dd>\n",
                escape_html(&fingerprint)
            )
        })
        .unwrap_or_default();
    let body = format!(
        "<main class=\"wrap node-panel\">\n<h1>Remote control is on</h1>\n\
         <p>On the other computer, open the Plumb Search app, choose \
         <strong>Connect to a node</strong>, and enter this node's address and token:</p>\n\
         <dl><dt>Address</dt><dd><code>{address}</code></dd>\n\
         <dt>Token</dt><dd><code>{}</code></dd>\n{fingerprint}</dl>\n\
         <p class=\"notice\">Copy the token now: this is the only time it is shown. \
         Anyone with it can change this node's settings, so keep it like a password. \
         Making a new token, or turning remote control off, stops the old one working.</p>\n\
         <p><a class=\"btn\" href=\"/app?section=remote\">Done</a></p>\n</main>",
        escape_html(token)
    );
    let head = format!(
        "<meta name=\"referrer\" content=\"{PANEL_REFERRER_POLICY}\">\n\
         <style>{PANEL_STYLE}</style><style>{LAYOUT_STYLE}</style>\n"
    );
    page_with_head("Remote control - Plumb Search", &head, &body)
}

fn render_remote_control(body: &mut String, remote: &RemoteControlView, writable: bool) {
    body.push_str(
        "<p class=\"intro\">Let the Plumb Search app on another computer change this \
         node's settings, crawling and features. It can read the node's status and change \
         its settings, nothing else.</p>",
    );
    let (big, rest) = match remote.on {
        None => ("Off", "<p>No other computer can change this node.</p>"),
        Some(false) => (
            "On",
            "<p>From this computer and local networks only (home and office \
             networks, Docker, Tailscale), with the token.</p>",
        ),
        Some(true) => (
            "On, from anywhere",
            "<p>From any address, with the token. Use HTTPS in front of the node so the \
             token is not sent in the clear.</p>",
        ),
    };
    body.push_str("<section class=\"cards\">");
    card(body, "", "Remote control", big, rest);
    body.push_str("</section>");
    if remote.loopback_only {
        body.push_str(
            "<p class=\"notice\">This node only listens on this computer (127.0.0.1), so \
             other computers cannot reach it. Remote control is for nodes that serve the \
             network, such as a Docker container or a homelab server started with \
             <code>--bind 0.0.0.0:8080</code>.</p>",
        );
    }
    if !writable {
        body.push_str(
            "<p class=\"hint\">To turn remote control on for a Docker container, run \
             <code>docker exec &lt;container&gt; plumb remote-control on</code> \
             on its host. It prints the token.</p><fieldset disabled>",
        );
    }
    let turn_on = if remote.on.is_some() {
        "Make a new token"
    } else {
        "Turn on and make a token"
    };
    body.push_str(&format!(
        "<form method=\"post\" action=\"/app/remote-control\">\
         <input type=\"hidden\" name=\"action\" value=\"on\">\
         <label><input type=\"checkbox\" name=\"allow_public\" value=\"1\"{}>\
         <span>Also from public addresses and through a reverse proxy</span></label>\
         <p class=\"hint\">Leave this off unless the node is behind HTTPS. Off, only \
         this computer and local networks can use the token.</p>\
         <button type=\"submit\">{turn_on}</button></form>",
        if remote.on == Some(true) {
            " checked"
        } else {
            ""
        }
    ));
    if remote.on.is_some() {
        body.push_str(
            "<form method=\"post\" action=\"/app/remote-control\">\
             <input type=\"hidden\" name=\"action\" value=\"off\">\
             <button type=\"submit\" class=\"alt\">Turn remote control off</button></form>",
        );
    }
    if !writable {
        body.push_str("</fieldset>");
    }
}

pub(super) async fn refresh(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    node.refresh_now();
    Redirect::to("/app").into_response()
}

pub(super) async fn pause(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(Form(form)) = Form::<PauseForm>::from_request(request, &state).await else {
        return panel_error(StatusCode::BAD_REQUEST, "The pause form could not be read.");
    };
    let current = node.settings().unwrap_or_default();
    let Some(settings) = paused(&current, &form, now_unix()) else {
        return panel_error(StatusCode::BAD_REQUEST, "The pause form could not be read.");
    };
    if let Err(err) = node.change_settings(settings) {
        return panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not save the settings: {err:#}"),
        );
    }
    Redirect::to("/app").into_response()
}

pub(super) async fn retry(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(Form(form)) = Form::<RetryForm>::from_request(request, &state).await else {
        return panel_error(StatusCode::BAD_REQUEST, "The retry form could not be read.");
    };
    let Some(what) = form.what() else {
        return panel_error(StatusCode::BAD_REQUEST, "The retry form could not be read.");
    };
    if let Err(err) = node.retry(what) {
        return panel_error(StatusCode::CONFLICT, &err.to_string());
    }
    Redirect::to("/app").into_response()
}

pub(super) async fn backup(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    match node.make_backup() {
        Ok(_) => Redirect::to("/app?section=backup&saved=backup").into_response(),
        Err(err) => panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not make a backup: {err:#}"),
        ),
    }
}

pub(super) async fn download_backup(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    request: Request,
) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // It holds keys: only for this computer, like the settings.
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Some(path) = node.data_dir().and_then(|dir| backup::path_of(&dir, &name)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match std::fs::read(&path) {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json".to_owned()),
                (header::CACHE_CONTROL, "no-store".to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{name}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(super) async fn restore_saved(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(Form(form)) = Form::<RestoreForm>::from_request(request, &state).await else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "The restore form could not be read.",
        );
    };
    let Some(path) = node
        .data_dir()
        .and_then(|dir| backup::path_of(&dir, &form.name))
    else {
        return panel_error(StatusCode::NOT_FOUND, "There is no such backup.");
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return panel_error(StatusCode::NOT_FOUND, "There is no such backup.");
    };
    restore_bytes(node.as_ref(), &bytes)
}

pub(super) async fn restore_upload(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(mut form) = axum::extract::Multipart::from_request(request, &state).await else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "The backup file could not be read.",
        );
    };
    while let Ok(Some(field)) = form.next_field().await {
        if field.name() == Some("backup") {
            return match field.bytes().await {
                Ok(bytes) if !bytes.is_empty() => restore_bytes(node.as_ref(), &bytes),
                _ => panel_error(StatusCode::BAD_REQUEST, "Choose a backup file first."),
            };
        }
    }
    panel_error(StatusCode::BAD_REQUEST, "Choose a backup file first.")
}

fn restore_bytes(node: &dyn super::StatusSource, bytes: &[u8]) -> Response {
    let backup = match Backup::parse(bytes) {
        Ok(backup) => backup,
        Err(err) => {
            return panel_error(
                StatusCode::BAD_REQUEST,
                &format!("{err:#}. Nothing was changed."),
            )
        }
    };
    if let Err(err) = node.restore_backup(&backup) {
        return panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not restore the backup: {err:#}"),
        );
    }
    Redirect::to("/app?section=backup&saved=restored").into_response()
}

pub(super) async fn restart(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    if let Err(err) = node.restart() {
        return panel_error(StatusCode::CONFLICT, &err.to_string());
    }
    panel_page(page_with_head(
        "Restarting - Plumb Search",
        &format!(
            "<meta http-equiv=\"refresh\" content=\"5;url=/app\"><style>{PANEL_STYLE}</style>"
        ),
        "<main class=\"wrap node-panel\"><h1>Restarting Plumb Search</h1>\
         <p>The node is stopping and starting again with the saved settings. This page \
         comes back by itself.</p></main>",
    ))
}

pub(super) async fn retry_network(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    if let Err(err) = node.reconnect_network() {
        return panel_error(StatusCode::CONFLICT, &err.to_string());
    }
    Redirect::to("/app?section=network&saved=retry").into_response()
}

/// Why a change is refused: it does not come from this computer, or a page
/// of another site sent it. `None` when it may go ahead.
pub(super) fn refusal(request: &Request) -> Option<&'static str> {
    let local = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        // A dual-stack `[::]` listener sees IPv4 peers as `::ffff:127.0.0.1`.
        .is_some_and(|ConnectInfo(peer)| peer.ip().to_canonical().is_loopback());
    if !local {
        return Some("Settings can only be changed on the computer Plumb runs on.");
    }
    let headers = request.headers();
    let own = request_origin(headers, request.uri());
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
fn local_origin(origin: &str) -> bool {
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

pub(super) fn forbidden(why: &str) -> Response {
    panel_error(StatusCode::FORBIDDEN, why)
}

/// What the panel says about search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    SettingUp,
    /// Searchable, without all of the seed data yet.
    Limited,
    Ready,
}

fn readiness(status: &Status) -> Readiness {
    match status.phase {
        Phase::SettingUp => Readiness::SettingUp,
        Phase::Ready if status.wikidata_missing => Readiness::Limited,
        Phase::Ready => Readiness::Ready,
    }
}

/// Whether work is under way that the panel should follow closely.
fn busy(status: &Status) -> bool {
    status.phase != Phase::Ready
        || !matches!(status.step, Step::Idle | Step::Retrying | Step::Stopping)
}

/// Whether setup still has steps to go: the full index or the first crawl.
fn setting_up(status: &Status) -> bool {
    status.phase != Phase::Ready || status.wikidata_missing || status.last_refresh.is_none()
}

/// Bytes in words: `850 KB`, `312 MB`, `1.4 GB`.
fn bytes_words(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const GB: u64 = 1_000 * MB;
    if bytes >= 10 * GB {
        format!("{} GB", group_thousands(bytes / GB))
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else {
        format!("{} KB", bytes.div_ceil(KB))
    }
}

pub(super) struct PanelView<'a> {
    pub(super) status: &'a Status,
    pub(super) settings: &'a NodeSettings,
    /// Where the node's search pages are: links to them start with it.
    pub(super) origin: &'a str,
    pub(super) data_dir: Option<&'a Path>,
    pub(super) now: u64,
    pub(super) query: &'a PanelQuery,
    pub(super) active: &'a FeatureSettings,
    pub(super) saved: &'a FeatureSettings,
    pub(super) writable: bool,
    pub(super) private_ready: bool,
    /// The panel's own address, `/app` for this node and
    /// `/app/nodes/<id>` for another node controlled from here; its links
    /// and forms start with it.
    pub(super) base: &'a str,
    /// Above the title: what node this is.
    pub(super) eyebrow: &'a str,
    /// The row of nodes to switch between, as HTML; empty for none.
    pub(super) switcher: &'a str,
    /// This node's remote control, for its "Remote control" section; `None`
    /// on the panel of another node.
    pub(super) remote_control: Option<&'a RemoteControlView>,
    /// The node's activity log, newest first.
    pub(super) activity: &'a [LogEntry],
    /// This node's saved backups, for its "Backup" section; `None` on the
    /// panel of another node.
    pub(super) backups: Option<&'a [BackupInfo]>,
}

/// What the "Remote control" section shows.
pub(super) struct RemoteControlView {
    /// On, and whether from public addresses too.
    pub(super) on: Option<bool>,
    /// The node only listens on this computer, so no other can reach it.
    pub(super) loopback_only: bool,
}

pub(super) fn render_panel(view: &PanelView<'_>) -> String {
    let PanelView {
        status,
        settings,
        origin,
        data_dir,
        now,
        query,
        active,
        saved,
        writable,
        private_ready,
        base,
        eyebrow,
        switcher,
        remote_control,
        activity,
        backups,
    } = *view;
    let mut sections = vec![
        ("overview", "Overview"),
        ("search", "Search & browser"),
        ("resources", "Resources"),
        ("network", "Network & privacy"),
    ];
    if remote_control.is_some() {
        sections.push(("remote", "Remote control"));
    }
    sections.push(("activity", "Activity"));
    if backups.is_some() {
        sections.push(("backup", "Backup"));
    }
    sections.push(("about", "About"));
    let section = if sections.iter().any(|(key, _)| *key == query.section) {
        query.section.as_str()
    } else {
        "overview"
    };
    let title = sections.iter().find(|(key, _)| *key == section).unwrap().1;
    let site = escape_html(origin);
    let mut body = format!("<main class=\"wrap node-panel\">{switcher}<div class=\"node-heading\"><div><p class=\"eyebrow\">{}</p><h1>Plumb Search</h1></div><a class=\"btn alt\" href=\"{site}/\" target=\"_blank\">Open search ↗</a></div><nav class=\"node-nav\" aria-label=\"Node settings\">", escape_html(eyebrow));
    for (key, label) in sections.iter().copied() {
        body.push_str(&format!(
            "<a href=\"{base}?section={key}\"{}>{}</a>",
            if key == section {
                " aria-current=\"page\""
            } else {
                ""
            },
            escape_html(label)
        ));
    }
    body.push_str(&format!("</nav><div class=\"section-heading\"><h2>{}</h2><a href=\"{base}?section={section}\">Refresh status</a></div>", escape_html(title)));
    if active != saved && status.can_restart && writable {
        body.push_str(&format!("<form method=\"post\" action=\"{base}/restart\" class=\"notice\" role=\"status\"><p>Feature changes saved. They apply once the node restarts, which takes a few seconds; search pauses meanwhile.</p><button type=\"submit\">Restart to apply</button></form>"));
    } else if active != saved {
        body.push_str("<p class=\"notice\" role=\"status\">Feature changes saved. Quit and reopen the desktop app, or restart the Docker container, to apply them. Closing the desktop window does not quit the app.</p>");
    } else if query.saved == "settings" {
        body.push_str(
            "<p class=\"notice\" role=\"status\">Resource settings saved and applied.</p>",
        );
    } else if query.saved == "setup" {
        body.push_str("<p class=\"notice\" role=\"status\">Your node is set up. It takes in the network's crawls in the background, up to the size you chose.</p>");
    } else if query.saved == "backup" {
        body.push_str("<p class=\"notice\" role=\"status\">Backup saved.</p>");
    } else if query.saved == "restored" {
        body.push_str(if status.can_restart {
            "<p class=\"notice\" role=\"status\">Backup restored. The node is restarting to \
             finish; the settings from before are in a backup marked before-restore.</p>"
        } else {
            "<p class=\"notice\" role=\"status\">Backup restored. Restart the node (for \
             Docker, the container) to finish; the settings from before are in a backup \
             marked before-restore.</p>"
        });
    } else if query.saved == "retry" {
        body.push_str("<p class=\"notice\" role=\"status\">Trying the bootstrap nodes again. Refresh the status in a few seconds.</p>");
    } else if query.saved == "features" {
        body.push_str("<p class=\"notice\" role=\"status\">Feature settings saved. No restart is needed because they match the running node.</p>");
    }
    if !writable {
        body.push_str("<p class=\"notice\">Viewing this node remotely. Settings are read-only. To change them, open the desktop app or connect to the node through a loopback address on its host. Docker bridge networking may also require host-side configuration.</p>");
    }
    match section {
        "overview" => {
            if !settings.setup_chosen && writable && base == "/app" {
                super::setup::render_setup(&mut body, settings);
            }
            body.push_str("<p class=\"intro\">Search readiness, background work, and the resources your node is using.</p><section class=\"cards\" aria-label=\"Node overview\">");
            render_search_card(&mut body, status, origin, now, base);
            render_storage_card(&mut body, status, settings);
            render_downloads_card(&mut body, status, settings);
            if !writable {
                body.push_str("<fieldset disabled>");
            }
            render_crawl_card(&mut body, status, settings, now, base);
            render_network_card(&mut body, status, active, now, base);
            if !writable {
                body.push_str("</fieldset>");
            }
            if status.meaning_work.is_some() {
                render_meaning_card(&mut body, status, active, now, base);
            }
            body.push_str("</section>");
            if setting_up(status) {
                render_steps(&mut body, status, now);
            }
            if let Some(err) = &status.last_error {
                let retry = if writable {
                    retry_button(base, "work", "Try again now")
                } else {
                    String::new()
                };
                let when = err
                    .retry_at
                    .map(|at| format!(" Plumb tries again by itself {}.", time_until(at, now)))
                    .unwrap_or_default();
                body.push_str(&format!("<div class=\"err\" role=\"alert\"><strong>Last update failed</strong><p>{}</p><p class=\"hint\">{}{when}</p>{retry}</div>", escape_html(&err.message), escape_html(&time_ago(err.at, now))));
            }
            if !activity.is_empty() {
                body.push_str("<h2>Recent activity</h2>");
                render_log(&mut body, &activity[..activity.len().min(5)], now);
                body.push_str(&format!(
                    "<p><a href=\"{base}?section=activity\">All activity</a></p>"
                ));
            }
        }
        "resources" => {
            body.push_str("<p class=\"intro\">Control background crawling and its download and storage budget. Changes apply immediately.</p><section class=\"cards\">");
            render_storage_card(&mut body, status, settings);
            render_downloads_card(&mut body, status, settings);
            body.push_str("</section>");
            if !writable {
                body.push_str("<fieldset disabled>");
            }
            render_settings(&mut body, settings, base);
            if !writable {
                body.push_str("</fieldset>");
            }
        }
        "search" => {
            body.push_str("<p class=\"intro\">Choose how you search and connect your browser to this node.</p><section class=\"cards\">");
            render_search_card(&mut body, status, origin, now, base);
            render_meaning_card(&mut body, status, active, now, base);
            let private = if private_ready {
                "Ready"
            } else if !active.private_search {
                "Off"
            } else if !super::private::in_build() {
                "Unavailable in this build"
            } else {
                "Preparing index"
            };
            let link = if private_ready {
                format!("<p><a class=\"btn alt\" href=\"{site}/private\" target=\"_blank\">Open private search ↗</a></p>")
            } else {
                String::new()
            };
            card(
                &mut body,
                "",
                "Private browser search",
                private,
                &format!("<p>Rank results in your browser so your query stays there.</p>{link}"),
            );
            body.push_str("</section>");
            if !writable {
                body.push_str("<fieldset disabled>");
            }
            render_features(&mut body, active, saved, "search", base);
            if !writable {
                body.push_str("</fieldset>");
            }
            render_browser(&mut body, origin);
        }
        "network" => {
            body.push_str("<p class=\"intro\">Connect to other nodes, check shared crawling, and choose optional search features.</p><section class=\"cards\">");
            if !writable {
                body.push_str("<fieldset disabled>");
            }
            render_network_card(&mut body, status, active, now, base);
            if !writable {
                body.push_str("</fieldset>");
            }
            render_peers(&mut body, status);
            render_network_details(&mut body, status);
            body.push_str("</section>");
            if !writable {
                body.push_str("<fieldset disabled>");
            }
            render_features(&mut body, active, saved, "network", base);
            if !writable {
                body.push_str("</fieldset>");
            }
        }
        "activity" => {
            body.push_str("<p class=\"intro\">What the node did and what went wrong, newest first. It keeps the last few hundred entries.</p>");
            if activity.is_empty() {
                body.push_str("<p>Nothing yet.</p>");
            } else {
                render_log(&mut body, activity, now);
            }
            body.push_str(&format!("<p class=\"hint\">For a bug report, the <a href=\"{site}/api/status\" target=\"_blank\">diagnostic status</a> has the details.</p>"));
        }
        "backup" => {
            if let Some(backups) = backups {
                render_backups(&mut body, backups, now);
            }
        }
        "remote" => {
            if let Some(remote) = remote_control {
                render_remote_control(&mut body, remote, writable);
            }
        }
        _ => {
            render_about(&mut body, status, data_dir, now);
            body.push_str(&format!("<p>Desktop and Docker run the same node and settings panel.</p><p><a href=\"https://github.com/SueHeir/plumb-search\" target=\"_blank\">Source code &amp; documentation ↗</a> · <a href=\"{site}/api/status\" target=\"_blank\">Diagnostic status ↗</a></p><p class=\"hint\">Desktop: use the tray or menu bar for Start at login and Quit Plumb Search.</p>"));
        }
    }
    body.push_str("</main>");
    // Never reload forms: a timed reload discards unsaved edits and keyboard focus.
    let reload = if section == "overview" && query.saved.is_empty() {
        format!(
            "<meta http-equiv=\"refresh\" content=\"{}\">",
            if busy(status) {
                BUSY_RELOAD_SECONDS
            } else {
                IDLE_RELOAD_SECONDS
            }
        )
    } else {
        String::new()
    };
    let head = format!("<meta name=\"referrer\" content=\"{PANEL_REFERRER_POLICY}\">{reload}<style>{PANEL_STYLE}</style><style>{LAYOUT_STYLE}</style>");
    page_with_head(&format!("{title} - Plumb Search"), &head, &body)
}

pub(super) const LAYOUT_STYLE: &str = "
.node-switch{display:flex;flex-wrap:wrap;gap:.4rem;margin-bottom:1rem}.node-switch a{padding:.4rem .8rem;border:1px solid var(--line);border-radius:999px;text-decoration:none;font-size:.9rem;color:var(--fg)}.node-switch a[aria-current]{border-color:var(--accent);color:var(--accent);font-weight:600}.node-panel{max-width:72rem;padding:2rem 2rem 4rem}.node-heading,.section-heading{display:flex;align-items:center;justify-content:space-between;gap:1rem}.node-heading h1{font-size:1.8rem}.eyebrow{font-size:.7rem;letter-spacing:.13em;color:var(--muted);margin:0 0 .3rem}.node-nav{display:flex;flex-wrap:wrap;gap:.4rem;border-bottom:1px solid var(--line);padding:1.5rem 0 1rem;margin-bottom:1.5rem}.node-panel a{color:var(--accent)}.node-panel a.btn:not(.alt){color:var(--bg)}.node-nav a{padding:.55rem .85rem;text-decoration:none;border-radius:.5rem;color:var(--muted)}.node-nav a[aria-current]{background:var(--accent);color:var(--bg);font-weight:600}.section-heading h2{margin:0;font-size:1.4rem}.section-heading>a{font-size:.85rem}.intro{color:var(--muted);max-width:45rem}.notice{padding:.85rem 1rem;border-left:3px solid var(--accent);background:color-mix(in srgb,var(--accent) 8%,var(--bg));border-radius:.3rem}.node-panel form{max-width:46rem}.node-panel fieldset{border:0;margin:0;padding:0;min-width:0}.node-panel .workload{margin-top:1rem}.log{list-style:none;padding:0;margin:.5rem 0}.log li{display:flex;flex-wrap:wrap;gap:.25rem 1rem;align-items:baseline;padding:.45rem 0;border-bottom:1px solid var(--line)}.log time{flex:none;min-width:7rem;color:var(--muted);font-size:.85rem}.log li span{flex:1 1 20rem;overflow-wrap:anywhere}.log .error span{color:var(--err)}.log.backups li{align-items:center}.log.backups form{margin:0;display:inline}.log.backups .btns{flex:none;margin:0}.node-panel .workload legend{font-weight:600}.node-panel select{font:inherit;background:var(--bg);color:var(--fg);border:1px solid var(--line);border-radius:.3rem}.cards>fieldset{display:contents}.node-panel details{margin-top:1rem}.node-panel summary{cursor:pointer;color:var(--accent)}.node-panel fieldset:disabled{opacity:.65}.node-panel textarea{display:block;width:100%;min-height:6rem;font:inherit;background:var(--bg);color:var(--fg);padding:.75rem;border:1px solid var(--line);border-radius:.5rem}.node-panel .feature{padding:.8rem 0;border-bottom:1px solid var(--line)}.node-panel .feature label{margin:0}.node-panel .feature p{margin:.35rem 0 0 1.65rem}.node-panel .state{font-size:.8rem;color:var(--muted)}.node-panel :focus-visible{outline:3px solid var(--accent);outline-offset:3px}.node-panel dl{grid-template-columns:minmax(6rem,auto) minmax(0,1fr)}@media(max-width:600px){.node-panel{padding:1rem 1rem 3rem}.node-heading{align-items:flex-start}.node-heading h1{font-size:1.5rem}.node-nav{gap:.2rem}.node-nav a{padding:.5rem .6rem;font-size:.9rem}.cards{grid-template-columns:minmax(0,1fr)}.node-panel label{flex-wrap:wrap}.section-heading{align-items:flex-start}.section-heading>a{white-space:nowrap}}";

/// A card: its class, title, headline and the HTML under them.
fn card(body: &mut String, class: &str, title: &str, big: &str, rest: &str) {
    body.push_str(&format!(
        "<div class=\"card {class}\">\n<h3>{}</h3>\n<p class=\"big\">{}</p>\n{rest}</div>\n",
        escape_html(title),
        escape_html(big)
    ));
}

fn meter(done: u64, total: u64) -> String {
    let max = total.max(done).max(1);
    format!(
        "<progress value=\"{}\" max=\"{max}\"></progress>\n",
        done.min(max)
    )
}

fn render_search_card(body: &mut String, status: &Status, origin: &str, now: u64, base: &str) {
    let origin = escape_html(origin);
    let buttons = format!(
        "<div class=\"btns\"><a class=\"btn\" href=\"{origin}/\" target=\"_blank\">\
         Search in your browser</a>\
         <a class=\"btn alt\" href=\"{origin}{ADD_TO_FIREFOX_PATH}\" target=\"_blank\">\
         Add to Firefox</a></div>\n"
    );
    match readiness(status) {
        Readiness::SettingUp => {
            let mut rest = format!(
                "<p>Getting a list of popular sites and building a first index. Search is \
                 ready in a minute or two.</p>\n<p>{}</p>\n",
                escape_html(&status.detail)
            );
            if let Some(progress) = &status.progress {
                rest.push_str(&meter(progress.done, progress.total));
            }
            if let Some(err) = &status.last_error {
                rest.push_str(&format!(
                    "<p class=\"err\"><strong>Something went wrong</strong> {}: {}",
                    time_ago(err.at, now),
                    escape_html(&err.message)
                ));
                if let Some(retry_at) = err.retry_at {
                    rest.push_str(&format!(
                        " Plumb will try again {}.",
                        time_until(retry_at, now)
                    ));
                }
                rest.push_str("</p>\n");
            }
            card(body, "search setup", "Search", "Setting up search", &rest);
        }
        Readiness::Limited => {
            let why = if status.wikidata_error.is_some() {
                "Plumb could not download Wikidata's list of official websites yet and will \
                 try again, so results get better once it is in."
            } else {
                "Plumb is still adding Wikidata's list of official websites and more \
                 rankings, so results get better once that is done."
            };
            let retry = if status.wikidata_error.is_some() {
                retry_button(base, "wikidata", "Try Wikidata again now")
            } else {
                String::new()
            };
            let rest = format!(
                "<p>Search works now with the {} most popular sites. {}</p>\n{retry}{buttons}",
                group_thousands(status.sites),
                escape_html(why)
            );
            card(
                body,
                "search limited",
                "Search",
                "Limited search is ready",
                &rest,
            );
        }
        Readiness::Ready => {
            let rest = format!(
                "<p>{} sites indexed.</p>\n{buttons}",
                group_thousands(status.sites)
            );
            card(body, "search ready", "Search", "Search is ready", &rest);
        }
    }
}

fn render_storage_card(body: &mut String, status: &Status, settings: &NodeSettings) {
    let limit = settings.storage_limit_mb.saturating_mul(MB);
    let mut rest = if limit == 0 {
        "<p>No storage limit.</p>\n".to_string()
    } else {
        format!(
            "{}<p>of {} allowed.</p>\n",
            meter(status.disk_used, limit),
            bytes_words(limit)
        )
    };
    if let Some(fill) = &status.fill {
        let detail = fill.detail.trim_end_matches('.');
        rest.push_str(&format!(
            "<p>Filled with the network's crawls: {} sites.{}</p>\n",
            group_thousands(fill.filled),
            if detail.is_empty() {
                String::new()
            } else {
                format!(" {}.", escape_html(detail))
            }
        ));
    }
    let class = if limit > 0 && status.disk_used >= limit {
        "warn"
    } else {
        ""
    };
    card(
        body,
        class,
        "Storage",
        &bytes_words(status.disk_used),
        &rest,
    );
}

fn render_downloads_card(body: &mut String, status: &Status, settings: &NodeSettings) {
    let limit = settings.download_limit_mb_per_day.saturating_mul(MB);
    let mut rest = if limit == 0 {
        "<p>No daily limit.</p>\n".to_string()
    } else {
        format!(
            "{}<p>of {} a day.</p>\n",
            meter(status.downloaded_today, limit),
            bytes_words(limit)
        )
    };
    rest.push_str(&format!(
        "<p class=\"hint\">{} since setup.</p>\n",
        bytes_words(status.downloaded_total)
    ));
    let class = if limit > 0 && status.downloaded_today >= limit {
        "warn"
    } else {
        ""
    };
    card(
        body,
        class,
        "Downloads today",
        &bytes_words(status.downloaded_today),
        &rest,
    );
}

fn render_crawl_card(
    body: &mut String,
    status: &Status,
    settings: &NodeSettings,
    now: u64,
    base: &str,
) {
    let crawling = status.phase == Phase::Ready && status.step == Step::Crawling;
    let (big, mut rest) = if crawling {
        let rest = status
            .progress
            .as_ref()
            .map(|p| {
                format!(
                    "{}<p>{} of {} homepages in this round.</p>\n",
                    meter(p.done, p.total),
                    group_thousands(p.done),
                    group_thousands(p.total)
                )
            })
            .unwrap_or_default();
        ("Visiting homepages".to_string(), rest)
    } else if status.step == Step::Indexing {
        let meter = status
            .progress
            .as_ref()
            .map(|p| meter(p.done, p.total))
            .unwrap_or_default();
        (
            "Rebuilding index".to_string(),
            format!("{meter}<p>{}.</p>", escape_html(&status.detail)),
        )
    } else if status.phase != Phase::Ready {
        ("Waiting for setup".to_string(), String::new())
    } else if status.last_error.is_some() {
        (
            "Retrying update".to_string(),
            "<p>The last update failed. Plumb will retry automatically.</p>".to_string(),
        )
    } else if let Some(reason) = &status.paused {
        let resumes = status
            .paused_until
            .map(|until| format!("<p>Resumes {}.</p>\n", time_until(until, now)))
            .unwrap_or_default();
        (
            "Paused".to_string(),
            format!("<p>{}.</p>\n{resumes}", escape_html(reason)),
        )
    } else if status.crawl_left > 0 {
        (
            "Waiting".to_string(),
            format!(
                "<p>{} homepages to visit once setup is done.</p>\n",
                group_thousands(status.crawl_left)
            ),
        )
    } else if let Some(next) = status.next_refresh {
        (
            "Up to date".to_string(),
            format!("<p>Next round {}.</p>\n", time_until(next, now)),
        )
    } else {
        ("Up to date".to_string(), String::new())
    };
    rest.push_str(&format!(
        "<p class=\"hint\">{} homepages visited since setup.</p>\n",
        group_thousands(status.homepages_visited)
    ));
    // Starts the next round now; while one runs, or crawling is paused,
    // there is nothing to start.
    if status.phase == Phase::Ready && !busy(status) && status.paused.is_none() {
        rest.push_str(&format!(
            "<form method=\"post\" action=\"{base}/refresh\">\
             <button type=\"submit\" class=\"alt\">Update now</button></form>\n"
        ));
    }
    rest.push_str(&pause_buttons(status, settings, base));
    card(body, "", "Crawling", &big, &rest);
}

fn render_meaning_card(
    body: &mut String,
    status: &Status,
    active: &FeatureSettings,
    now: u64,
    base: &str,
) {
    let headline = match status.meaning_sites {
        Some(n) => format!("{} sites ready", group_thousands(n)),
        None if active.search_by_meaning => "Preparing".into(),
        None => "Off".into(),
    };
    let rest = match &status.meaning_work {
        Some(work) => {
            let mut rest = String::new();
            if let Some(p) = &work.progress {
                rest.push_str(&meter(p.done, p.total));
            }
            rest.push_str(&format!("<p>{}", escape_html(&work.detail)));
            if let Some(p) = &work.progress {
                let about = if p.unit == "MB" { "about " } else { "" };
                rest.push_str(&format!(
                    ": {} of {about}{} {}",
                    group_thousands(p.done),
                    group_thousands(p.total),
                    escape_html(&p.unit)
                ));
            }
            rest.push_str(".</p>");
            if let Some(err) = &work.error {
                let retry = err
                    .retry_at
                    .map(|at| format!(" Trying again {}.", time_until(at, now)))
                    .unwrap_or_default();
                rest.push_str(&format!(
                    "<p class=\"hint\">{}{retry}</p>{}",
                    escape_html(&err.message),
                    retry_button(base, "meaning", "Try again now")
                ));
            }
            rest
        }
        None => "<p>Find sites by their subject as well as their name. Turn it on under Search \
                 &amp; browser.</p>"
            .to_owned(),
    };
    let class = if status
        .meaning_work
        .as_ref()
        .is_some_and(|w| w.error.is_some())
    {
        "warn"
    } else {
        ""
    };
    card(body, class, "Search by meaning", &headline, &rest);
}

/// A form with one button that tries `what` again.
fn retry_button(base: &str, what: &str, label: &str) -> String {
    format!(
        "<form method=\"post\" action=\"{base}/retry\"><input type=\"hidden\" name=\"what\" \
         value=\"{what}\"><button type=\"submit\" class=\"alt\">{label}</button></form>"
    )
}

/// Activity log entries, newest first as given.
fn render_log(body: &mut String, entries: &[LogEntry], now: u64) {
    body.push_str("<ol class=\"log\">");
    for entry in entries {
        let (class, label) = match entry.level {
            LogLevel::Info => ("info", ""),
            LogLevel::Warning => ("warning", "<strong>Warning:</strong> "),
            LogLevel::Error => ("error", "<strong>Error:</strong> "),
        };
        let at = chrono::DateTime::from_timestamp(entry.at as i64, 0)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        body.push_str(&format!(
            "<li class=\"{class}\"><time title=\"{at}\">{}</time><span>{label}{}</span></li>",
            escape_html(&time_ago(entry.at, now)),
            escape_html(&entry.message)
        ));
    }
    body.push_str("</ol>");
}

fn render_backups(body: &mut String, backups: &[BackupInfo], now: u64) {
    let what: Vec<&str> = backup::FILES.iter().map(|(_, what)| *what).collect();
    body.push_str(&format!(
        "<p class=\"intro\">A backup keeps what cannot be downloaded again: {}. Sites and \
         the index are left out; a restored node rebuilds them. Backups hold this node\u{2019}s \
         keys, so keep them private.</p>\
         <form method=\"post\" action=\"/app/backup\"><button type=\"submit\">Make a backup \
         now</button></form>",
        escape_html(&what.join(", ").to_lowercase())
    ));
    body.push_str("<h2>Saved backups</h2>");
    if backups.is_empty() {
        body.push_str("<p>None yet.</p>");
    } else {
        body.push_str("<ol class=\"log backups\">");
        for b in backups {
            let name = escape_html(&b.name);
            body.push_str(&format!(
                "<li><span><code>{name}</code> <small>{} \u{b7} {} KB</small></span>\
                 <span class=\"btns\"><a class=\"btn alt\" href=\"/app/backups/{name}\" \
                 target=\"_blank\">Download</a><form method=\"post\" \
                 action=\"/app/backups/restore\"><input type=\"hidden\" name=\"name\" \
                 value=\"{name}\"><button type=\"submit\" class=\"alt\">Restore</button>\
                 </form></span></li>",
                escape_html(&time_ago(b.created_at, now)),
                b.bytes.div_ceil(1000)
            ));
        }
        body.push_str("</ol>");
    }
    body.push_str(
        "<h2>Restore from a file</h2><form method=\"post\" action=\"/app/restore\" \
         enctype=\"multipart/form-data\"><label>Backup file <input type=\"file\" \
         name=\"backup\" accept=\".json,application/json\" required></label>\
         <p class=\"hint\">Restoring replaces this node\u{2019}s settings and keys with the \
         backup\u{2019}s, after saving the current ones as a backup marked before-restore, \
         then restarts the node.</p><button type=\"submit\">Restore</button></form>",
    );
}

/// "Pause for an hour" and "Pause until tomorrow" while background work may
/// run, "Resume" while someone paused it.
fn pause_buttons(status: &Status, settings: &NodeSettings, base: &str) -> String {
    let paused_by_hand = settings.paused_until.is_some_and(|until| {
        status.paused_until == Some(until) && status.paused.as_deref() == Some("Paused by you")
    });
    let buttons = if paused_by_hand {
        "<button type=\"submit\" name=\"until\" value=\"resume\">Resume now</button>"
    } else if status.background_updates && status.paused.is_none() {
        "<button type=\"submit\" name=\"until\" value=\"hour\" class=\"alt\">Pause for an \
         hour</button><button type=\"submit\" name=\"until\" value=\"tomorrow\" \
         class=\"alt\">Pause until tomorrow</button>"
    } else {
        return String::new();
    };
    format!("<form method=\"post\" action=\"{base}/pause\" class=\"btns\">{buttons}</form>\n")
}

fn render_network_card(
    body: &mut String,
    status: &Status,
    active: &FeatureSettings,
    now: u64,
    base: &str,
) {
    let retry = format!(
        "<form method=\"post\" action=\"{base}/network/retry\">\
         <button type=\"submit\" class=\"alt\">Try again now</button></form>"
    );
    let no_bootstrap = if active.bootstrap.is_empty() {
        "<p class=\"hint\">No bootstrap nodes are set, so only Plumb nodes on this \
         network can be found. Turn on \u{201c}Find nodes through plumbsearch.org\u{201d} \
         under Network &amp; privacy.</p>"
    } else {
        ""
    };
    let (class, headline, rest) = match &status.network {
        Some(net) if net.connected_peers > 0 => {
            let n = net.connected_peers;
            let relayed = net
                .peers
                .iter()
                .filter(|p| p.route == plumb_net::Route::Relayed)
                .count();
            let direct = n.saturating_sub(net.nearby_peers + relayed);
            let mut parts = Vec::new();
            for (count, words) in [
                (net.nearby_peers, "on this network"),
                (direct, "over the internet"),
                (relayed, "through a relay"),
            ] {
                if count > 0 {
                    parts.push(format!("{count} {words}"));
                }
            }
            let reach = if net.nat == "public" {
                "Other nodes can reach this one directly."
            } else if !net.relays.is_empty() {
                "Other nodes reach this one through a relay, so no port needs opening."
            } else {
                "Other nodes cannot reach this one yet; it still searches and shares through \
                 the connections it opens."
            };
            (
                "ready",
                format!("{n} {}", if n == 1 { "node" } else { "nodes" }),
                format!(
                    "<p>Connected: {}.</p><p class=\"hint\">{reach} Shared crawl batches: {} \
                     received, {} published.</p>",
                    parts.join(", "),
                    net.batches_received,
                    net.batches_published
                ),
            )
        }
        Some(net) => match &net.problem {
            Some(problem) => (
                "warn",
                "Can\u{2019}t connect".to_owned(),
                format!(
                    "<p>{}</p><p class=\"hint\">Last tried {}. Nodes on this network are \
                     still found without it.</p>{no_bootstrap}{retry}",
                    escape_html(&problem.message),
                    time_ago(problem.at, now)
                ),
            ),
            None => {
                let since = net
                    .alone_since
                    .map(|at| format!(" Looking since {}.", time_ago(at, now)))
                    .unwrap_or_default();
                (
                    "limited",
                    "Looking for nodes".to_owned(),
                    format!(
                        "<p>Asking the bootstrap nodes for other Plumb nodes, and looking on \
                         this network.{since}</p>{no_bootstrap}{retry}"
                    ),
                )
            }
        },
        None if active.network => (
            "",
            "Starting".to_owned(),
            "<p>The Plumb network is enabled and is starting.</p>".to_owned(),
        ),
        None => (
            "",
            "Off".to_owned(),
            "<p>This node searches its own index. Turn on \u{201c}Join the Plumb \
             network\u{201d} under Network &amp; privacy to share crawls with others.</p>"
                .to_owned(),
        ),
    };
    card(body, class, "Plumb network", &headline, &rest);
}

/// The nodes this node is connected to, and how it reaches each.
fn render_peers(body: &mut String, status: &Status) {
    let Some(net) = &status.network else {
        return;
    };
    if net.peers.is_empty() {
        return;
    }
    let mut rows = String::new();
    for peer in &net.peers {
        let id = &peer.peer_id;
        // The end of a peer id tells nodes apart; the start is the same
        // for every Ed25519 key.
        let short = id.get(id.len().saturating_sub(8)..).unwrap_or(id);
        let route = match peer.route {
            plumb_net::Route::Nearby => "On this network",
            plumb_net::Route::Direct => "Over the internet",
            plumb_net::Route::Relayed => "Through a relay",
        };
        let mut roles = Vec::new();
        if peer.bootstrap {
            roles.push("bootstrap node");
        }
        if peer.relay {
            roles.push("relays for this node");
        }
        let roles = if roles.is_empty() {
            String::new()
        } else {
            format!(" · {}", roles.join(" · "))
        };
        rows.push_str(&format!(
            "<dt title=\"{}\"><code>\u{2026}{}</code></dt><dd>{route}{roles}</dd>",
            escape_html(id),
            escape_html(short)
        ));
    }
    let more = net.connected_peers.saturating_sub(net.peers.len());
    let more = if more > 0 {
        format!("<p class=\"hint\">And {more} more.</p>")
    } else {
        String::new()
    };
    card(
        body,
        "search",
        "Connected nodes",
        &format!("{} connected", net.connected_peers),
        &format!("<dl>{rows}</dl>{more}"),
    );
}

fn render_network_details(body: &mut String, status: &Status) {
    let Some(net) = &status.network else {
        return;
    };
    let agreement = &net.agreement;
    card(body, "", "Crawl agreement", &format!("{} sites confirmed", group_thousands(agreement.confirmed_sites as u64)), &format!("<p>{} waiting for agreement · {} disputed.</p><p class=\"hint\">{} trusted crawlers · {} distrusted.</p>", agreement.pending_sites, agreement.disputed_sites, agreement.vouched_crawlers, agreement.distrusted_crawlers));
    card(
        body,
        "",
        "Popularity sharing",
        &format!("{} reports sent", net.reports_sent),
        &format!(
            "<p>{} reports held · {} popular picks.</p>",
            net.reports_held, net.popular_picks
        ),
    );
    card(body, "", "Relay activity", &format!("{} requests relayed", net.requests_relayed), &format!("<p>{} bucket requests answered.</p><p class=\"hint\">Reachability: {}. {} relay reservations.</p>", net.buckets_served, escape_html(&net.nat), net.relays.len()));
    card(
        body,
        "search",
        "Node identity",
        "Network addresses",
        &format!(
            "<p class=\"msg\">{}</p><p class=\"msg\">{}</p>",
            escape_html(&net.peer_id),
            escape_html(&net.reachable_at.join("\n"))
        ),
    );
}

fn render_features(
    body: &mut String,
    active: &FeatureSettings,
    saved: &FeatureSettings,
    section: &str,
    base: &str,
) {
    body.push_str(&format!("<h2>Optional features</h2><p class=\"hint\">Saved choices apply after you quit and reopen the app or restart the container. Existing Docker transport and relay flags are preserved.</p><form method=\"post\" action=\"{base}/features\">"));
    body.push_str(&format!(
        "<input type=\"hidden\" name=\"section\" value=\"{section}\">"
    ));
    for (name, label, value, running, hint) in [
        ("network", "Join the Plumb network", saved.network, active.network, "Share signed crawls and search other nodes. Uses port 4001 by default, local discovery, and UPnP; existing server flags still configure transport."),
        ("search_by_meaning", "Search by meaning", saved.search_by_meaning, active.search_by_meaning, "Find sites by topic. Downloads a model (about 130 MB) and builds site vectors in the background."),
        ("private_search", "Private browser search", saved.private_search, active.private_search, "Adds a Private search switch to the search page\u{2019}s settings gear. Visitors who turn it on search inside their browser, so this node never sees their words. Requires a build with the private-search module and extra disk space for search buckets."),
        ("share_popularity", "Share anonymous popularity", saved.share_popularity, active.share_popularity, "Requires the Plumb network. Reports which results are opened to help improve ranking. Off unless you enable it."),
        ("search_history", "Remember searches", saved.search_history.unwrap_or(active.search_history == Some(true)), active.search_history == Some(true), "Each browser that searches here keeps its own history on this computer: past searches, and the sites opened from them, which come first next time. Nobody sees another browser's history. Turn off on a node strangers can search."),
    ] {
        if (section == "search") != matches!(name, "search_by_meaning" | "private_search" | "search_history") { continue; }
        body.push_str(&format!("<div class=\"feature\"><label><input type=\"checkbox\" name=\"{name}\" value=\"1\"{}><span>{label} <span class=\"state\">· currently {}</span></span></label><p class=\"hint\">{hint}</p></div>", if value { " checked" } else { "" }, if running { "on" } else { "off" }));
    }
    if section == "network" {
        let extra: Vec<&str> = saved.extra_bootstrap().collect();
        body.push_str(&format!(
            "<div class=\"feature\"><label><input type=\"checkbox\" name=\"plumb_bootstrap\" \
             value=\"1\"{}><span>Find nodes through plumbsearch.org</span></label>\
             <p class=\"hint\">The Plumb network\u{2019}s own node introduces this one to the \
             others. Plumb nodes on this network are found without it.</p></div>",
            // On by default for a node not yet in the network.
            if saved.uses_default_bootstrap() || (!saved.network && saved.bootstrap.is_empty()) {
                " checked"
            } else {
                ""
            }
        ));
        body.push_str(&format!("<input type=\"hidden\" name=\"trust_shown\" value=\"1\"><div class=\"feature\"><label><input type=\"checkbox\" name=\"default_trust\" value=\"1\"{}><span>Trust plumbsearch.org's crawler</span></label><p class=\"hint\">Take in crawls from the plumbsearch.org node at once, so a new node fills up while the network is small. Turn off to keep only crawls a second crawler confirms.</p></div>", if saved.no_default_trust { "" } else { " checked" }));
        body.push_str(&format!(
            "<details{}><summary>Advanced: more bootstrap and trusted nodes</summary>\
             <label for=\"bootstrap\">Bootstrap nodes</label><p class=\"hint\" \
             id=\"bootstrap-help\">Most people never need these. One multiaddress per line, \
             such as a friend\u{2019}s node or your own server: \
             <code>/dns4/example.org/tcp/4001/p2p/12D3Koo\u{2026}</code></p>\
             <textarea id=\"bootstrap\" name=\"bootstrap\" aria-describedby=\"bootstrap-help\" \
             spellcheck=\"false\">{}</textarea>",
            if extra.is_empty() && saved.trusted.is_empty() {
                ""
            } else {
                " open"
            },
            escape_html(&extra.join("\n"))
        ));
        body.push_str(&format!("<label for=\"trusted\">Trusted nodes</label><p class=\"hint\" id=\"trusted-help\">Other node ids whose crawls are taken in at once, one per line. Only add nodes you run or know.</p><textarea id=\"trusted\" name=\"trusted\" aria-describedby=\"trusted-help\" spellcheck=\"false\">{}</textarea></details>", escape_html(&saved.trusted.join("\n"))));
    }
    body.push_str("<button type=\"submit\">Save feature settings</button></form>");
}

/// One line of the setup steps: done, under way, waiting or to do.
fn step_item(body: &mut String, state: &str, text: &str, note: Option<String>) {
    let icon = match state {
        "done" => "&#10003;",
        "now" => "&#9679;",
        "wait" => "&#8987;",
        _ => "&#9675;",
    };
    let note = note
        .map(|note| format!("<small>{}</small>", escape_html(&note)))
        .unwrap_or_default();
    body.push_str(&format!(
        "<li class=\"{state}\"><span class=\"i\" aria-hidden=\"true\">{icon}</span>\
         <span>{}{note}</span></li>\n",
        escape_html(text)
    ));
}

fn render_steps(body: &mut String, status: &Status, now: u64) {
    body.push_str("<h2>Setup</h2>\n<ol class=\"steps\">\n");
    let ready = status.phase == Phase::Ready;
    let setting_up_step = |steps: &[Step]| !ready && steps.contains(&status.step);

    let list = if setting_up_step(&[Step::Downloading, Step::Starting, Step::Retrying]) {
        "now"
    } else {
        "done"
    };
    step_item(body, list, "Get a list of popular sites", None);
    let first = if ready {
        "done"
    } else if setting_up_step(&[Step::Ingesting, Step::Indexing]) {
        "now"
    } else {
        "todo"
    };
    step_item(body, first, "Build a first index: limited search", None);

    let (seed, note) = if !ready {
        ("todo", None)
    } else if !status.wikidata_missing {
        ("done", None)
    } else if let Some(err) = &status.wikidata_error {
        let when = err
            .retry_at
            .map(|at| format!(" Plumb will try again {}.", time_until(at, now)))
            .unwrap_or_default();
        ("wait", Some(format!("Could not download it yet.{when}")))
    } else {
        ("now", Some(status.detail.clone()))
    };
    step_item(
        body,
        seed,
        "Add Wikidata's official websites and more rankings: full search",
        note,
    );

    let crawling = ready && status.step == Step::Crawling;
    let (crawl, note) = if crawling {
        let note = status.progress.as_ref().map(|p| {
            format!(
                "{} of {} homepages",
                group_thousands(p.done),
                group_thousands(p.total)
            )
        });
        ("now", note)
    } else if let Some(reason) = status.paused.as_ref().filter(|_| status.crawl_left > 0) {
        ("wait", Some(format!("{reason}.")))
    } else if status.last_refresh.is_some() && status.crawl_left == 0 {
        ("done", None)
    } else {
        ("todo", None)
    };
    step_item(
        body,
        crawl,
        "Visit homepages to learn more of each site's names",
        note,
    );
    body.push_str("</ol>\n");
}

fn render_settings(body: &mut String, settings: &NodeSettings, base: &str) {
    let checked = |on: bool| if on { " checked" } else { "" };
    let limit = |mb: u64| {
        if mb == 0 {
            String::new()
        } else {
            mb.to_string()
        }
    };
    body.push_str(&format!(
        "<h2>Crawling &amp; limits</h2>\n<form method=\"post\" action=\"{base}/settings\">\n\
         <label><input type=\"checkbox\" name=\"background_updates\" value=\"1\"{}>\
         <span>Keep the index up to date in the background</span></label>\n\
         <p class=\"hint\">Plumb visits a few thousand homepages a day to learn sites' names \
         and find new sites, then rebuilds its index. Search keeps working when this is \
         off.</p>\n<fieldset class=\"workload\"><legend>Workload</legend>\n",
        checked(settings.background_updates)
    ));
    for (workload, label, hint) in [
        (
            Workload::Light,
            "Light",
            "4 homepages at a time, 100 MB of downloads a day, 1 GB of disk. For a laptop or \
             a slow connection.",
        ),
        (
            Workload::Balanced,
            "Balanced",
            "16 homepages at a time, 500 MB a day, 2 GB of disk.",
        ),
        (
            Workload::Full,
            "Full",
            "32 homepages at a time and no limits. For a server or homelab.",
        ),
        (
            Workload::Custom,
            "Custom",
            "16 homepages at a time, with the limits below.",
        ),
    ] {
        body.push_str(&format!(
            "<label><input type=\"radio\" name=\"workload\" value=\"{}\"{}><span>{label} \
             <span class=\"state\">{hint}</span></span></label>\n",
            workload.name(),
            checked(settings.workload == workload)
        ));
    }
    body.push_str(&format!(
        "</fieldset>\n\
         <label>Download limit <input type=\"number\" name=\"download_limit_mb_per_day\" \
         min=\"0\" step=\"1\" value=\"{}\" placeholder=\"none\"> MB per UTC day</label>\n\
         <p class=\"hint\">Crawling pauses for the rest of the UTC day once it is reached. Empty \
         for no limit. Used with Custom; a preset sets it.</p>\n\
         <label>Storage limit <input type=\"number\" name=\"storage_limit_mb\" min=\"0\" \
         step=\"1\" value=\"{}\" placeholder=\"none\"> MB</label>\n\
         <p class=\"hint\">Crawling pauses while the data folder is bigger. Empty for no \
         limit. Used with Custom; a preset sets it.</p>\n\
         <input type=\"hidden\" name=\"fill_shown\" value=\"1\">\
         <label><input type=\"checkbox\" name=\"fill_from_network\" value=\"1\"{}>\
         <span>Fill free space with the network's crawls</span></label>\n\
         <p class=\"hint\">Asks a node you trust for the sites it has crawled, most popular \
         first, until 90% of the storage limit (or of what this machine's memory can index) \
         is used. In the network only.</p>\n\
         <input type=\"hidden\" name=\"focus_shown\" value=\"1\">\
         <label for=\"focus_topics\">Focus topics</label>\n\
         <textarea id=\"focus_topics\" name=\"focus_topics\" rows=\"3\" \
         placeholder=\"games\">{}</textarea>\n\
         <p class=\"hint\">One per line. This node crawls sites about them first and twice as \
         often, and keeps more of them when the storage limit is tight, so the more nodes \
         focus on a topic, the better Plumb knows it. Other nodes can tell from what this \
         node crawls. The interests on a browser\u{2019}s About you page stay private: they \
         only change which sites this node keeps.</p>\n",
        limit(settings.download_limit_mb_per_day),
        limit(settings.storage_limit_mb),
        checked(settings.fill_from_network),
        escape_html(&settings.focus_topics.join("\n"))
    ));
    let hours = settings
        .crawl_hours
        .unwrap_or(CrawlHours { from: 22, to: 7 });
    let options = |selected: u8| {
        (0..24u8)
            .map(|h| {
                format!(
                    "<option value=\"{h}\"{}>{h:02}:00</option>",
                    if h == selected { " selected" } else { "" }
                )
            })
            .collect::<String>()
    };
    let (hour, minute, _) = crate::node::schedule::local_time();
    let page_sets = render_page_sets(settings);
    body.push_str(&format!(
        "<label><input type=\"checkbox\" name=\"crawl_hours\" value=\"1\"{}>\
         <span>Only crawl between <select name=\"crawl_from\" aria-label=\"From\">{}</select> \
         and <select name=\"crawl_to\" aria-label=\"Until\">{}</select></span></label>\n\
         <p class=\"hint\">On the node\u{2019}s clock, which reads {hour:02}:{minute:02} now. \
         Overnight hours work too, such as 22:00 to 07:00.</p>\n\
         {page_sets}<button type=\"submit\">Save settings</button>\n</form>\n",
        checked(settings.crawl_hours.is_some()),
        options(hours.from),
        options(hours.to)
    ));
}

/// The page sets part of the settings form: how many pages of each set
/// (Wikipedia articles) to list with the sites.
fn render_page_sets(settings: &NodeSettings) -> String {
    let mut out = String::from(
        "<fieldset class=\"workload\"><legend>Page sets</legend>\n\
         <input type=\"hidden\" name=\"page_sets_shown\" value=\"1\">\n\
         <p class=\"hint\">Single pages, such as Wikipedia articles, listed with the sites. \
         Only each page's title and one-line description are kept, about 100 bytes a page.</p>\n",
    );
    for set in SETS {
        let current = settings.page_sets.size(set.id);
        let mut choices = vec![PageSetSize::Auto];
        choices.extend_from_slice(SIZE_CHOICES);
        if !choices.contains(&current) {
            choices.push(current);
        }
        let options: String = choices
            .iter()
            .map(|size| {
                format!(
                    "<option value=\"{size}\"{}>{}</option>",
                    if *size == current { " selected" } else { "" },
                    escape_html(&size.words())
                )
            })
            .collect();
        let kept = current.pages(settings.storage_limit_mb).min(set.pages);
        out.push_str(&format!(
            "<label>{} <select name=\"page_set.{}\">{options}</select></label>\n\
             <p class=\"hint\">Keeps {} pages now, about {} MB.</p>\n",
            escape_html(set.name),
            escape_html(set.id),
            if kept == set.pages {
                format!("all of them, about {}", thousands(set.pages))
            } else {
                thousands(kept)
            },
            (kept * set.bytes_per_page).div_ceil(1_000_000)
        ));
    }
    out.push_str("</fieldset>\n");
    out
}

fn render_browser(body: &mut String, origin: &str) {
    let origin = escape_html(origin);
    body.push_str(&format!(
        "<h2>Search from your browser</h2>\n\
         <p>Plumb searches in your web browser. Add it to Firefox with the button above, or \
         add a search engine in another browser's settings with this address:</p>\n\
         <p><code>{origin}/search?q=%s</code></p>\n\
         <p class=\"hint\">Your country and other search options are on the search page.</p>\n"
    ));
}

fn render_about(body: &mut String, status: &Status, data_dir: Option<&Path>, now: u64) {
    body.push_str("<dl>\n");
    let mut row = |name: &str, value: String| {
        body.push_str(&format!(
            "<dt>{}</dt><dd>{}</dd>\n",
            escape_html(name),
            escape_html(&value)
        ));
    };
    row("Version", status.version.clone());
    row("Sites indexed", group_thousands(status.sites));
    if let Some(last) = status.last_refresh {
        row("Last updated", time_ago(last, now));
    }
    if let Some(dir) = data_dir {
        row("Data folder", dir.display().to_string());
    }
    body.push_str("</dl>\n");
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::Router;
    use tower::ServiceExt;

    use super::*;
    use crate::node::{LastError, Progress};
    use crate::web::{node_router, StatusSource};

    struct FakeNode {
        status: Status,
        settings: Mutex<NodeSettings>,
        refreshes: Mutex<usize>,
        features: Mutex<FeatureSettings>,
    }

    impl StatusSource for FakeNode {
        fn status(&self) -> Status {
            self.status.clone()
        }
        fn settings(&self) -> Option<NodeSettings> {
            Some(self.settings.lock().unwrap().clone())
        }
        fn change_settings(&self, settings: NodeSettings) -> anyhow::Result<()> {
            *self.settings.lock().unwrap() = settings;
            Ok(())
        }
        fn refresh_now(&self) {
            *self.refreshes.lock().unwrap() += 1;
        }
        fn saved_features(&self) -> anyhow::Result<FeatureSettings> {
            Ok(self.features.lock().unwrap().clone())
        }
        fn change_features(&self, features: FeatureSettings) -> anyhow::Result<()> {
            *self.features.lock().unwrap() = features;
            Ok(())
        }
        fn data_dir(&self) -> Option<std::path::PathBuf> {
            Some("/home/me/plumb <data>".into())
        }
        fn activity_log(&self) -> Vec<LogEntry> {
            vec![LogEntry {
                at: now_unix() - 120,
                level: LogLevel::Error,
                message: "Could not reach <tranco-list.eu>".into(),
            }]
        }
        fn retry(&self, what: Retry) -> anyhow::Result<()> {
            match what {
                Retry::Work => Ok(()),
                _ => anyhow::bail!("Search by meaning is off on this node."),
            }
        }
    }

    fn status(phase: Phase, step: Step) -> Status {
        Status {
            phase,
            step,
            detail: "Downloading the Tranco list of popular sites".into(),
            progress: None,
            last_error: None,
            wikidata_missing: false,
            wikidata_error: None,
            sites: 0,
            index: None,
            last_refresh: None,
            next_refresh: None,
            version: "0.1.0".into(),
            crawl_left: 0,
            background_updates: true,
            paused: None,
            disk_used: 0,
            downloaded_today: 0,
            downloaded_total: 0,
            homepages_visited: 0,
            meaning_sites: None,
            meaning_work: None,
            can_restart: false,
            paused_until: None,
            network: None,
            fill: None,
        }
    }

    struct NoSearch;
    impl crate::web::SearchBackend for NoSearch {
        fn search(&self, _: &str, _: usize) -> anyhow::Result<Vec<plumb_index::Hit>> {
            Ok(Vec::new())
        }
        fn num_docs(&self) -> u64 {
            0
        }
    }

    fn app(status: Status) -> (Router, Arc<FakeNode>) {
        let node = Arc::new(FakeNode {
            status,
            settings: Mutex::new(NodeSettings::default()),
            refreshes: Mutex::new(0),
            features: Mutex::new(FeatureSettings::default()),
        });
        (node_router(Arc::new(NoSearch), node.clone()), node)
    }

    async fn get_panel(app: Router) -> String {
        let mut body = String::new();
        for section in ["overview", "search", "resources", "network", "about"] {
            body.push_str(&get_section(app.clone(), section).await);
        }
        body
    }

    async fn get_section(app: Router, section: &str) -> String {
        let mut request = Request::get(format!("/app?section={section}"))
            .header(header::HOST, "127.0.0.1:7586")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:50000".parse::<SocketAddr>().unwrap(),
        ));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // So that its forms send their origin: no-referrer sends "null".
        assert_eq!(response.headers()[header::REFERRER_POLICY], "same-origin");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        let no_referrer = body.find("content=\"no-referrer\"").unwrap();
        let same_origin = body
            .find("<meta name=\"referrer\" content=\"same-origin\">")
            .unwrap();
        assert!(same_origin > no_referrer, "the later one wins: {body}");
        body
    }

    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// A form post to `path` from `peer`, with an `Origin` header if given.
    async fn post(
        app: Router,
        path: &str,
        body: &str,
        peer: &str,
        origin: Option<&str>,
    ) -> Response {
        post_to_host(app, path, body, peer, "127.0.0.1:7586", origin).await
    }

    /// [`post`], with the given `Host` header.
    async fn post_to_host(
        app: Router,
        path: &str,
        body: &str,
        peer: &str,
        host: &str,
        origin: Option<&str>,
    ) -> Response {
        let mut request = Request::post(path)
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        let mut request = request.body(Body::from(body.to_string())).unwrap();
        let peer: SocketAddr = peer.parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        app.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn says_what_works_while_setting_up() {
        let mut setting_up = status(Phase::SettingUp, Step::Downloading);
        setting_up.progress = Some(Progress {
            done: 0,
            total: 1,
            unit: "files".into(),
        });
        let body = get_panel(app(setting_up).0).await;
        assert!(
            body.contains("<p class=\"big\">Setting up search</p>"),
            "{body}"
        );
        assert!(body.contains("Downloading the Tranco list"), "{body}");
        assert!(body.contains("<progress value=\"0\" max=\"1\">"), "{body}");
        assert!(body.contains("<h2>Setup</h2>"), "{body}");
        assert!(body.contains("content=\"5\""), "reloads often: {body}");
        assert!(!body.contains("Search in your browser"), "{body}");
        assert!(!body.contains("Update now"), "{body}");
        assert!(body.contains("/home/me/plumb &lt;data&gt;"), "{body}");
    }

    #[tokio::test]
    async fn shows_the_node_at_a_glance() {
        let mut limited = status(Phase::Ready, Step::Downloading);
        limited.sites = 250_000;
        limited.wikidata_missing = true;
        limited.detail = "Asking Wikidata for official websites".into();
        limited.disk_used = 312_400_000;
        limited.downloaded_today = 48_000_000;
        limited.downloaded_total = 1_400_000_000;
        limited.homepages_visited = 3_456;
        limited.crawl_left = 2_000;
        limited.fill = Some(crate::node::FillStatus {
            filled: 41_000,
            position: 60_000,
            total: 1_190_000,
            peer: None,
            detail: "Taking in crawled sites from a trusted node".into(),
        });
        let (router, node) = app(limited.clone());
        *node.settings.lock().unwrap() = NodeSettings::desktop();
        let body = get_panel(router).await;
        assert!(
            body.contains("<p class=\"big\">Limited search is ready</p>"),
            "{body}"
        );
        assert!(body.contains("the 250,000 most popular sites"), "{body}");
        assert!(
            body.contains(
                "Filled with the network's crawls: 41,000 sites. Taking in crawled sites from a trusted node."
            ),
            "{body}"
        );
        assert!(
            body.contains(
                "href=\"http://127.0.0.1:7586/\" target=\"_blank\">Search in your browser"
            ),
            "{body}"
        );
        assert!(
            body.contains("<code>http://127.0.0.1:7586/search?q=%s</code>"),
            "{body}"
        );
        assert!(
            body.contains("<small>Asking Wikidata for official websites</small>"),
            "{body}"
        );
        // Storage and downloads, against the desktop's limits.
        assert!(body.contains("<p class=\"big\">312 MB</p>"), "{body}");
        assert!(
            body.contains("<progress value=\"312400000\" max=\"2000000000\">"),
            "{body}"
        );
        assert!(body.contains("<p>of 2.0 GB allowed.</p>"), "{body}");
        assert!(body.contains("<p class=\"big\">48 MB</p>"), "{body}");
        assert!(body.contains("<p>of 500 MB a day.</p>"), "{body}");
        assert!(body.contains("1.4 GB since setup."), "{body}");
        assert!(
            body.contains("2,000 homepages to visit once setup is done."),
            "{body}"
        );
        assert!(
            body.contains("3,456 homepages visited since setup."),
            "{body}"
        );
        assert!(body.contains("<p class=\"big\">Off</p>"), "{body}");
        assert!(
            body.contains("name=\"download_limit_mb_per_day\" min=\"0\" step=\"1\" value=\"500\""),
            "{body}"
        );
        assert!(
            body.contains("name=\"storage_limit_mb\" min=\"0\" step=\"1\" value=\"2000\""),
            "{body}"
        );

        limited.step = Step::Idle;
        limited.wikidata_error = Some(LastError {
            message: "HTTP 429".into(),
            at: now_unix(),
            // A little past 10 minutes, so a second ticking over before the
            // page renders still reads "10 minutes".
            retry_at: Some(now_unix() + 630),
        });
        let body = get_panel(app(limited).0).await;
        assert!(body.contains("could not download Wikidata"), "{body}");
        assert!(
            body.contains("Plumb will try again in 10 minutes"),
            "{body}"
        );
        assert!(body.contains("No storage limit."), "{body}");

        let mut ready = status(Phase::Ready, Step::Idle);
        ready.sites = 260_123;
        ready.last_refresh = Some(now_unix() - 3600);
        // Past 2 hours by a margin, so a tick before rendering can't make it "1 hour".
        ready.next_refresh = Some(now_unix() + 7230);
        let body = get_panel(app(ready.clone()).0).await;
        assert!(
            body.contains("<p class=\"big\">Search is ready</p>"),
            "{body}"
        );
        assert!(body.contains("260,123 sites indexed"), "{body}");
        assert!(body.contains("Next round in 2 hours"), "{body}");
        assert!(!body.contains("<h2>Setup</h2>"), "setup is done: {body}");
        assert!(body.contains("Update now"), "{body}");
        assert!(
            body.contains("content=\"60\""),
            "reloads seldom when idle: {body}"
        );

        // A round under way leaves nothing to start.
        let mut crawling = ready.clone();
        crawling.step = Step::Crawling;
        let body = get_panel(app(crawling).0).await;
        assert!(body.contains("Visiting homepages"), "{body}");
        assert!(!body.contains("Update now"), "{body}");

        ready.paused = Some("Paused until tomorrow: today's download limit is reached".into());
        let body = get_panel(app(ready).0).await;
        assert!(body.contains("<p class=\"big\">Paused</p>"), "{body}");
        assert!(
            body.contains("today&#39;s download limit is reached."),
            "{body}"
        );
        assert!(!body.contains("Update now"), "{body}");
    }

    #[test]
    fn reads_limits_and_words_bytes() {
        assert_eq!(parse_limit(""), Some(0));
        assert_eq!(parse_limit(" 1,500 "), Some(1_500));
        assert_eq!(parse_limit("0"), Some(0));
        assert_eq!(parse_limit("-5"), None);
        assert_eq!(parse_limit("2.5"), None);
        assert_eq!(bytes_words(0), "0 KB");
        assert_eq!(bytes_words(1_200), "2 KB");
        assert_eq!(bytes_words(312_400_000), "312 MB");
        assert_eq!(bytes_words(1_450_000_000), "1.4 GB");
        assert_eq!(bytes_words(150_000_000_000), "150 GB");
    }

    #[tokio::test]
    async fn saves_settings_from_this_computer_only() {
        let (router, node) = app(status(Phase::Ready, Step::Idle));
        let body = get_panel(router.clone()).await;
        assert!(
            body.contains("name=\"background_updates\" value=\"1\" checked"),
            "{body}"
        );

        // An unticked box is left out of the form; empty limits are none.
        let form = "download_limit_mb_per_day=250&storage_limit_mb=";
        let response = post(
            router.clone(),
            "/app/settings",
            form,
            "127.0.0.1:50000",
            Some("http://127.0.0.1:7586"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION],
            "/app?section=resources&saved=settings"
        );
        assert_eq!(
            *node.settings.lock().unwrap(),
            NodeSettings {
                background_updates: false,
                download_limit_mb_per_day: 250,
                storage_limit_mb: 0,
                ..NodeSettings::default()
            }
        );

        // Focus topics are kept as typed, once each; forms without them
        // keep them.
        post(
            router.clone(),
            "/app/settings",
            "focus_shown=1&focus_topics=games%0D%0ARust+programming,games",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(
            node.settings.lock().unwrap().focus_topics,
            ["games", "Rust programming"]
        );
        post(
            router.clone(),
            "/app/settings",
            "storage_limit_mb=",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(node.settings.lock().unwrap().focus_topics.len(), 2);
        node.settings.lock().unwrap().focus_topics.clear();

        // A limit that is not a number changes nothing.
        let response = post(
            router.clone(),
            "/app/settings",
            "background_updates=1&storage_limit_mb=lots",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // The window has no back button: every error page leads back.
        let body = body_text(response).await;
        assert!(body.contains("Nothing was changed."), "{body}");
        assert!(
            body.contains("<a class=\"btn\" href=\"/app\">Back to the panel</a>"),
            "{body}"
        );
        assert!(!node.settings.lock().unwrap().background_updates);

        // Without an Origin header (some browsers leave it out) is fine too.
        let response = post(
            router.clone(),
            "/app/settings",
            "background_updates=1",
            "[::1]:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(node.settings.lock().unwrap().background_updates);
        // A form without the fill box keeps the fill setting.
        assert!(node.settings.lock().unwrap().fill_from_network);
        let panel = get_panel(router.clone()).await;
        assert!(
            panel.contains("name=\"fill_from_network\" value=\"1\" checked"),
            "{panel}"
        );
        // The panel's form shows it; unticked turns filling off.
        let response = post(
            router.clone(),
            "/app/settings",
            "background_updates=1&fill_shown=1",
            "[::1]:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(!node.settings.lock().unwrap().fill_from_network);
        let response = post(
            router.clone(),
            "/app/settings",
            "background_updates=1&fill_shown=1&fill_from_network=1",
            "[::1]:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(node.settings.lock().unwrap().fill_from_network);

        // Another machine, or a page of another site, may not.
        for (peer, origin) in [
            ("192.168.1.20:50000", Some("http://127.0.0.1:7586")),
            ("127.0.0.1:50000", Some("https://evil.example")),
            ("127.0.0.1:50000", Some("null")),
        ] {
            let response = post(router.clone(), "/app/settings", "", peer, origin).await;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{peer} {origin:?}"
            );
            let response = post(router.clone(), "/app/refresh", "", peer, origin).await;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{peer} {origin:?}"
            );
        }
        assert!(node.settings.lock().unwrap().background_updates);
        assert_eq!(*node.refreshes.lock().unwrap(), 0);

        let response = post(
            router,
            "/app/refresh",
            "",
            "127.0.0.1:50000",
            Some("http://127.0.0.1:7586"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(*node.refreshes.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn refuses_changes_from_a_rebound_dns_name() {
        let (router, node) = app(status(Phase::Ready, Step::Idle));
        // attacker.example now resolves to 127.0.0.1: the browser connects
        // from loopback and calls the request same-origin.
        for path in ["/app/settings", "/app/refresh"] {
            let response = post_to_host(
                router.clone(),
                path,
                "storage_limit_mb=",
                "127.0.0.1:50000",
                "attacker.example:7586",
                Some("http://attacker.example:7586"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        }
        assert!(node.settings.lock().unwrap().background_updates);
        assert_eq!(*node.refreshes.lock().unwrap(), 0);

        // Local names still work.
        for (host, origin) in [
            ("localhost:7586", "http://localhost:7586"),
            ("[::1]:7586", "http://[::1]:7586"),
        ] {
            let response = post_to_host(
                router.clone(),
                "/app/refresh",
                "",
                "127.0.0.1:50000",
                host,
                Some(origin),
            )
            .await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "{host}");
        }
    }

    #[tokio::test]
    async fn accepts_ipv4_peers_on_a_dual_stack_listener() {
        let (router, node) = app(status(Phase::Ready, Step::Idle));
        let response = post(
            router,
            "/app/refresh",
            "",
            "[::ffff:127.0.0.1]:50000",
            Some("http://127.0.0.1:7586"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(*node.refreshes.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn shows_how_to_add_plumb_to_firefox() {
        let body = get_panel(app(status(Phase::Ready, Step::Idle)).0).await;
        assert!(
            body.contains(
                "href=\"http://127.0.0.1:7586/add-to-firefox\" target=\"_blank\">Add to Firefox"
            ),
            "{body}"
        );
        let request = Request::get("/add-to-firefox")
            .header(header::HOST, "127.0.0.1:7586")
            .body(Body::empty())
            .unwrap();
        let response = app(status(Phase::SettingUp, Step::Downloading))
            .0
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // Firefox offers "Add" for pages that link to an OpenSearch description.
        assert!(
            body.contains("<link rel=\"search\" type=\"application/opensearchdescription+xml\""),
            "{body}"
        );
        assert!(
            body.contains("Choose <strong>Add \"Plumb Search\"</strong>"),
            "{body}"
        );
        assert!(
            body.contains("<code>http://127.0.0.1:7586/search?q=%s</code>"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn editing_pages_keep_unsaved_input_and_show_real_network_state() {
        let mut status = status(Phase::Ready, Step::Idle);
        status.network = Some(plumb_net::NetStatus {
            connected_peers: 3,
            relaying_peers: 2,
            batches_received: 8,
            peer_id: "<untrusted-peer>".into(),
            ..Default::default()
        });
        let (router, _) = app(status);
        for section in ["resources", "network", "search", "about"] {
            let body = get_section(router.clone(), section).await;
            assert!(!body.contains("http-equiv=\"refresh\""));
            assert!(body.contains(&format!(
                "href=\"/app?section={section}\" aria-current=\"page\""
            )));
        }
        let search = get_section(router.clone(), "search").await;
        assert!(search.contains("name=\"search_by_meaning\""));
        assert!(search.contains("name=\"private_search\""));
        assert!(!search.contains("name=\"network\""));
        assert!(search.contains("class=\"wrap node-panel\""));
        let body = get_section(router, "network").await;
        assert!(body.contains("Connected: 3 over the internet."), "{body}");
        assert!(body.contains("&lt;untrusted-peer&gt;"));
        assert!(!body.contains("<untrusted-peer>"));
    }

    #[tokio::test]
    async fn feature_changes_require_local_access_validate_and_show_restart() {
        let (router, node) = app(status(Phase::Ready, Step::Idle));
        for form in [
            "share_popularity=1",
            "network=1&bootstrap=not-an-address",
            "network=1&trust_shown=1&trusted=not-a-node",
        ] {
            let response = post(
                router.clone(),
                "/app/features",
                form,
                "127.0.0.1:50000",
                None,
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(*node.features.lock().unwrap(), FeatureSettings::default());
        }
        for (peer, origin) in [
            ("192.168.1.20:50000", None),
            ("127.0.0.1:50000", Some("https://evil.example")),
        ] {
            assert_eq!(
                post(router.clone(), "/app/features", "network=1", peer, origin)
                    .await
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        let response = post(
            router.clone(),
            "/app/features",
            "network=1&private_search=1&search_by_meaning=1&share_popularity=1",
            "127.0.0.1:50000",
            Some("http://127.0.0.1:7586"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(node.features.lock().unwrap().network);
        assert!(!node.features.lock().unwrap().no_default_trust);
        let page = get_section(router.clone(), "network").await;
        assert!(page.contains("name=\"default_trust\" value=\"1\" checked"));
        assert!(page.contains("name=\"trusted\""));
        let node_id = "12D3KooWEwYB7PYxRNgvSWiwkLXvwYajSmYn4yoPqmkN7NbNqJjg";
        let response = post(
            router.clone(),
            "/app/features",
            &format!("network=1&trust_shown=1&trusted={node_id}"),
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(node.features.lock().unwrap().no_default_trust);
        assert_eq!(node.features.lock().unwrap().trusted, [node_id]);
        let response = post(
            router.clone(),
            "/app/features",
            "section=search&private_search=1",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(node.features.lock().unwrap().network);
        assert!(node.features.lock().unwrap().private_search);
        assert!(!node.features.lock().unwrap().search_by_meaning);
        assert!(get_section(router.clone(), "network")
            .await
            .contains("Feature changes saved."));
        let mut request = Request::get("/app?section=resources")
            .header(header::HOST, "server:8080")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "192.168.1.20:50000".parse::<SocketAddr>().unwrap(),
        ));
        let body = body_text(router.oneshot(request).await.unwrap()).await;
        assert!(body.contains("Settings are read-only"));
        assert!(body.contains("<fieldset disabled>"));
    }

    #[tokio::test]
    async fn the_network_card_explains_why_a_node_is_not_connected() {
        let mut status = status(Phase::Ready, Step::Idle);
        status.network = Some(plumb_net::NetStatus {
            alone_since: Some(now_unix() - 300),
            problem: Some(plumb_net::JoinProblem::new(
                "The bootstrap node at plumbsearch.org did not answer. <A firewall>",
                "timed out",
                now_unix() - 20,
            )),
            ..Default::default()
        });
        let (router, _) = app(status);
        let overview = get_section(router.clone(), "overview").await;
        assert!(overview.contains("Can\u{2019}t connect"), "{overview}");
        assert!(overview.contains("did not answer. &lt;A firewall&gt;"));
        assert!(overview.contains("action=\"/app/network/retry\""));
        // No bootstrap nodes saved: only nearby nodes can be found.
        assert!(overview.contains("only Plumb nodes on this network"));
        // The fake node has no network to retry.
        let response = post(
            router.clone(),
            "/app/network/retry",
            "",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            post(router, "/app/network/retry", "", "192.168.1.20:50000", None)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn the_network_section_lists_connected_nodes_and_one_bootstrap_switch() {
        let mut status = status(Phase::Ready, Step::Idle);
        let peer = |id: &str, route, relay, bootstrap| plumb_net::PeerView {
            peer_id: id.into(),
            route,
            relay,
            bootstrap,
        };
        status.network = Some(plumb_net::NetStatus {
            connected_peers: 4,
            nearby_peers: 1,
            nat: "private".into(),
            relays: vec!["relay".into()],
            peers: vec![
                peer(
                    "12D3KooWrelayAAAA11111111",
                    plumb_net::Route::Direct,
                    true,
                    true,
                ),
                peer(
                    "12D3KooWhomeBBBB22222222",
                    plumb_net::Route::Nearby,
                    false,
                    false,
                ),
                peer(
                    "12D3KooWfarCCCC<33333333>",
                    plumb_net::Route::Relayed,
                    false,
                    false,
                ),
            ],
            ..Default::default()
        });
        let (router, node) = app(status);
        let body = get_section(router.clone(), "network").await;
        assert!(
            body.contains("Connected: 1 on this network, 2 over the internet, 1 through a relay."),
            "{body}"
        );
        assert!(body.contains("reach this one through a relay"));
        assert!(body.contains("11111111</code></dt><dd>Over the internet · bootstrap node · relays for this node</dd>"));
        assert!(body.contains("<dd>On this network</dd>"));
        assert!(body.contains("&lt;33333333&gt;"));
        assert!(body.contains("And 1 more."));
        // A node not yet in the network finds others through plumbsearch.org
        // unless its owner says otherwise.
        assert!(body.contains("name=\"plumb_bootstrap\" value=\"1\" checked"));
        assert!(!body.contains("<details open>"));

        let response = post(
            router.clone(),
            "/app/features",
            "network=1&plumb_bootstrap=1&bootstrap=%2Fip4%2F10.0.0.2%2Ftcp%2F4001",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let saved = node.features.lock().unwrap().clone();
        assert!(saved.uses_default_bootstrap());
        assert_eq!(
            saved.extra_bootstrap().collect::<Vec<_>>(),
            ["/ip4/10.0.0.2/tcp/4001"]
        );
        let body = get_section(router.clone(), "network").await;
        assert!(body.contains("<details open>"));
        assert!(body.contains(">/ip4/10.0.0.2/tcp/4001</textarea>"));

        post(
            router.clone(),
            "/app/features",
            "network=1",
            "127.0.0.1:50000",
            None,
        )
        .await;
        let saved = node.features.lock().unwrap().clone();
        assert!(saved.bootstrap.is_empty());
        let body = get_section(router, "network").await;
        assert!(!body.contains("name=\"plumb_bootstrap\" value=\"1\" checked"));
    }

    #[tokio::test]
    async fn presets_crawl_hours_and_pauses_are_one_click() {
        let (router, node) = app(status(Phase::Ready, Step::Idle));
        let form = "background_updates=1&workload=light&download_limit_mb_per_day=999\
                    &crawl_hours=1&crawl_from=22&crawl_to=7";
        let response = post(
            router.clone(),
            "/app/settings",
            form,
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let saved = node.settings.lock().unwrap().clone();
        assert_eq!(saved.workload, Workload::Light);
        // The preset's limits win over what was typed.
        assert_eq!(
            (saved.download_limit_mb_per_day, saved.storage_limit_mb),
            (100, 1_000)
        );
        assert_eq!(saved.crawl_hours, Some(CrawlHours { from: 22, to: 7 }));
        let body = get_section(router.clone(), "resources").await;
        assert!(body.contains("value=\"light\" checked"), "{body}");
        assert!(body.contains("name=\"crawl_hours\" value=\"1\" checked"));
        assert!(body.contains("<option value=\"22\" selected>22:00</option>"));

        let response = post(
            router.clone(),
            "/app/settings",
            "crawl_hours=1&crawl_from=25&crawl_to=7",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let before = now_unix();
        let response = post(
            router.clone(),
            "/app/pause",
            "until=hour",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let until = node.settings.lock().unwrap().paused_until.unwrap();
        assert!((before + 3600..=now_unix() + 3600).contains(&until));
        // Saving the settings form keeps the pause.
        post(
            router.clone(),
            "/app/settings",
            "workload=full",
            "127.0.0.1:50000",
            None,
        )
        .await;
        let saved = node.settings.lock().unwrap().clone();
        assert_eq!(saved.paused_until, Some(until));
        assert_eq!(saved.workload, Workload::Full);
        assert_eq!(saved.crawl_hours, None);
        post(
            router.clone(),
            "/app/pause",
            "until=resume",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(node.settings.lock().unwrap().paused_until, None);
        for (form, peer, code) in [
            ("until=forever", "127.0.0.1:50000", StatusCode::BAD_REQUEST),
            ("until=hour", "192.168.1.20:50000", StatusCode::FORBIDDEN),
        ] {
            assert_eq!(
                post(router.clone(), "/app/pause", form, peer, None)
                    .await
                    .status(),
                code
            );
        }
        // This node cannot restart itself.
        assert_eq!(
            post(router, "/app/restart", "", "127.0.0.1:50000", None)
                .await
                .status(),
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn the_panel_shows_pauses_progress_and_restart_to_apply() {
        let mut paused = status(Phase::Ready, Step::Idle);
        paused.background_updates = true;
        paused.paused = Some("Paused by you".into());
        paused.paused_until = Some(now_unix() + 1800);
        paused.can_restart = true;
        paused.meaning_work = Some(crate::node::BackgroundWork {
            detail: "Downloading the search-by-meaning model".into(),
            progress: Some(Progress {
                done: 48,
                total: 130,
                unit: "MB".into(),
            }),
            error: None,
        });
        let (router, node) = app(paused);
        node.settings.lock().unwrap().paused_until = Some(now_unix() + 1800);
        let body = get_section(router.clone(), "overview").await;
        assert!(body.contains("<p>Resumes in "), "{body}");
        assert!(body.contains("value=\"resume\">Resume now</button>"));
        assert!(body.contains("Downloading the search-by-meaning model: 48 of about 130 MB."));
        assert!(!body.contains("Restart to apply"));
        // Saved feature changes that differ from the running ones.
        node.features.lock().unwrap().search_by_meaning = true;
        let body = get_section(router, "overview").await;
        assert!(body.contains("action=\"/app/restart\""), "{body}");
        assert!(body.contains("Restart to apply"));

        let mut running = status(Phase::Ready, Step::Idle);
        running.background_updates = true;
        let (router, _) = app(running);
        let body = get_section(router, "overview").await;
        assert!(body.contains("value=\"hour\" class=\"alt\">Pause for an hour"));
    }

    #[tokio::test]
    async fn failures_offer_a_retry_and_the_activity_log_reads_plainly() {
        let mut failing = status(Phase::Ready, Step::Retrying);
        failing.last_error = Some(LastError {
            message: "downloading the Tranco list failed".into(),
            at: now_unix() - 60,
            // A little past 10 minutes, so a second ticking over before the
            // page renders still reads "10 minutes".
            retry_at: Some(now_unix() + 630),
        });
        let (router, _) = app(failing);
        let body = get_section(router.clone(), "overview").await;
        assert!(
            body.contains("value=\"work\"><button type=\"submit\" class=\"alt\">Try again now"),
            "{body}"
        );
        assert!(
            body.contains("Plumb tries again by itself in 10 minutes."),
            "{body}"
        );
        assert!(body.contains("<h2>Recent activity</h2>"));
        assert!(body.contains("<strong>Error:</strong> Could not reach &lt;tranco-list.eu&gt;"));
        let body = get_section(router.clone(), "activity").await;
        assert!(body.contains("2 minutes ago"), "{body}");
        // The fake node keeps no backups, but its own panel offers them.
        let body = get_section(router.clone(), "backup").await;
        assert!(body.contains("Make a backup now"), "{body}");
        assert!(body.contains("enctype=\"multipart/form-data\""));

        for (form, code) in [
            ("what=work", StatusCode::SEE_OTHER),
            ("what=meaning", StatusCode::CONFLICT),
            ("what=everything", StatusCode::BAD_REQUEST),
        ] {
            let response = post(router.clone(), "/app/retry", form, "127.0.0.1:50000", None).await;
            assert_eq!(response.status(), code, "{form}");
        }
        let response = post(
            router.clone(),
            "/app/retry",
            "what=work",
            "192.168.1.20:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = post(
            router,
            "/app/backups/restore",
            "name=..%2F..%2Fetc%2Fpasswd",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn plumb_serve_has_no_panel() {
        let router = crate::web::router(Arc::new(NoSearch));
        let request = Request::get("/app").body(Body::empty()).unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
