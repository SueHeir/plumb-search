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

mod forms;
pub(super) use forms::{features_error, settings_error};

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
use crate::node::features::{AnswerLimit, FeatureSettings};
use crate::node::{
    CrawlHours, LogEntry, LogLevel, NodeSettings, Phase, Retry, Status, Step, Workload, MB,
};
use crate::pages::{thousands, PageSetSize, SETS, SIZE_CHOICES};

/// Seconds between two reloads of the panel while work is under way.
const BUSY_RELOAD_SECONDS: u32 = 30;
/// Seconds between two reloads otherwise.
const IDLE_RELOAD_SECONDS: u32 = 300;

pub(super) const PANEL_STYLE: &str = include_str!("panel.css");

/// The settings form as posted. A checkbox that is not ticked is not sent.
#[derive(Debug, Default, Deserialize)]
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
    confirm: Option<String>,
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

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct FeaturesForm {
    section: String,
    network: Option<String>,
    search_by_meaning: Option<String>,
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
    /// Which nodes network searches ask, from the network section.
    search_from: Option<String>,
    /// Set by the form that shows the credit choices.
    credits_shown: Option<String>,
    /// Free answers a day for other nodes; blank for no limit.
    answer_per_day: String,
    spend_credits: Option<String>,
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
    let section = match apply_features_form(&form, &mut features, "/app") {
        Ok(section) => section,
        Err(response) => return response,
    };
    if let Err(err) = node.change_features(features) {
        return features_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &form,
            "/app",
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
    base: &str,
) -> Result<&'static str, Response> {
    let section = if form.section == "search" {
        "search"
    } else {
        "network"
    };
    if section == "search" {
        features.search_by_meaning = form.search_by_meaning.is_some();
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
        if let Some(scope) = &form.search_from {
            match scope.parse() {
                Ok(scope) => features.search_from = Some(scope),
                Err(err) => {
                    return Err(features_error(
                        StatusCode::BAD_REQUEST,
                        form,
                        base,
                        &err.to_string(),
                    ))
                }
            }
        }
        if form.credits_shown.is_some() {
            let typed: String = form
                .answer_per_day
                .chars()
                .filter(|c| !matches!(c, ',' | '_' | ' ' | '\u{a0}'))
                .collect();
            let per_day = if typed.is_empty() {
                None
            } else {
                match typed.parse::<u64>() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        return Err(features_error(
                            StatusCode::BAD_REQUEST,
                            form,
                            base,
                            "The daily limit on searches answered for other nodes must be a \
                             whole number, or blank for no limit. Nothing was changed.",
                        ))
                    }
                }
            };
            features.answer_limit = Some(AnswerLimit { per_day });
            features.spend_credits = Some(form.spend_credits.is_some());
        }
    }
    if let Err(err) = features.check() {
        return Err(features_error(
            StatusCode::BAD_REQUEST,
            form,
            base,
            &err.to_string(),
        ));
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
    let bar = super::app_bar("", false);
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
    let settings = match settings_from_form(&form, &current, "/app") {
        Ok(settings) => settings,
        Err(response) => return response,
    };
    if let Err(err) = node.change_settings(settings) {
        warn!("could not save the settings: {err:#}");
        return settings_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &form,
            "/app",
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
    base: &str,
) -> Result<NodeSettings, Response> {
    let (Some(download), Some(storage)) = (
        parse_limit(&form.download_limit_mb_per_day),
        parse_limit(&form.storage_limit_mb),
    ) else {
        return Err(settings_error(
            StatusCode::BAD_REQUEST,
            form,
            base,
            "Limits are whole numbers of megabytes, or empty for none. Nothing was changed.",
        ));
    };
    let hour = |text: &str| text.trim().parse::<u8>().ok().filter(|h| *h < 24);
    let crawl_hours = match &form.crawl_hours {
        None => None,
        Some(_) => match (hour(&form.crawl_from), hour(&form.crawl_to)) {
            (Some(from), Some(to)) => Some(CrawlHours { from, to }),
            _ => {
                return Err(settings_error(
                    StatusCode::BAD_REQUEST,
                    form,
                    base,
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
                    return Err(settings_error(
                        StatusCode::BAD_REQUEST,
                        form,
                        base,
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
    if form.confirm.as_deref() != Some("yes") {
        return panel_page(page_with_head("Confirm restore - Plumb Search", &format!("<meta name=\"referrer\" content=\"same-origin\"><style>{PANEL_STYLE}{LAYOUT_STYLE}</style>"), &format!("<main class=\"wrap node-panel\"><h1>Restore this backup?</h1><p>This replaces this node’s identity keys, trusted nodes and settings. The current keys are lost unless backed up. Plumb saves a before-restore backup first; download a copy from Backup if you need to keep it elsewhere.</p><form method=\"post\" action=\"/app/backups/restore\"><input type=\"hidden\" name=\"name\" value=\"{}\"><button name=\"confirm\" value=\"yes\">Replace keys and settings</button> <a href=\"/app?section=backup\">Cancel</a></form></main>", escape_html(&form.name))));
    }
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
    let mut confirmed = false;
    while let Ok(Some(field)) = form.next_field().await {
        if field.name() == Some("confirm") {
            confirmed = field.text().await.ok().as_deref() == Some("yes");
            continue;
        }
        if !confirmed {
            return panel_error(StatusCode::BAD_REQUEST, "Confirm that you want to replace the node’s keys and settings before uploading a backup. Nothing was changed.");
        }
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
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    refusal_of(peer, request.headers(), request.uri())
}

/// [`refusal`] for a request from `peer` with `headers`, sent to `uri`.
pub(super) fn refusal_of(
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    uri: &Uri,
) -> Option<&'static str> {
    // A dual-stack `[::]` listener sees IPv4 peers as `::ffff:127.0.0.1`.
    let local = peer.is_some_and(|peer| peer.ip().to_canonical().is_loopback());
    // A reverse proxy on this computer connects from loopback too, and may
    // name 127.0.0.1 as the host: what it passes on came from elsewhere.
    let proxied = super::control::FORWARDED_HEADERS
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
pub(super) fn local_origin(origin: &str) -> bool {
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
    /// This node's plugins, for the choice of when they run; `None` for
    /// another node's panel.
    pub(super) plugins: Option<&'a crate::plugins::Plugins>,
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
        plugins,
    } = *view;
    let mut sections = vec![
        ("overview", "Overview"),
        ("search", "Search & browser"),
        ("resources", "Resources"),
        ("network", "Network & privacy"),
        ("integrations", "AI & plugins"),
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
    let bar = super::app_bar_for("settings", true, base);
    let mut body = format!("<div class=\"wrap node-panel\">{bar}<div class=\"node-layout\"><aside class=\"node-sidebar\"><p class=\"eyebrow\">{}</p>{switcher}<nav class=\"node-nav\" aria-label=\"Node settings\">", escape_html(eyebrow));
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
    body.push_str(&format!("</nav><a class=\"btn alt\" href=\"{site}/\">Open search</a></aside><main class=\"node-content\"><div class=\"section-heading\"><h1>{}</h1><a href=\"{base}?section={section}\">Refresh status</a></div>", escape_html(title)));
    // Only this node's own panel has a restart route; remote nodes do not.
    if active != saved && status.can_restart && writable && base == "/app" {
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
    } else if query.saved == "plugins" {
        body.push_str("<p class=\"notice\" role=\"status\">Plugin choices saved. They apply from the next search.</p>");
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
                body.push_str(&format!("<div class=\"err\" role=\"alert\"><strong>Last update failed</strong><p>{}</p><p class=\"hint\">{}{when}</p>{retry}</div>", "Check your internet connection or proxy. Open Activity for technical details.", escape_html(&format!("{}.", time_ago(err.at, now)))));
            }
            if !activity.is_empty() {
                let recent: Vec<_> = activity
                    .iter()
                    .filter(|entry| {
                        status.last_error.as_ref().is_none_or(|err| {
                            entry.message != err.message
                                && entry.level != crate::node::journal::LogLevel::Error
                        })
                    })
                    .take(5)
                    .cloned()
                    .collect();
                if !recent.is_empty() {
                    body.push_str("<h2>Recent activity</h2>");
                    render_log(&mut body, &recent, now);
                }
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
            } else if !super::private::in_build() {
                "Unavailable in this build"
            } else if !active.network && !active.private_search {
                "Off until this node joins the Plumb network"
            } else {
                "Preparing index"
            };
            let link = if private_ready {
                format!(
                    "<p><a class=\"btn alt\" href=\"{site}/private\">Open private search</a></p>"
                )
            } else {
                String::new()
            };
            card(
                &mut body,
                "",
                "Private browser search",
                private,
                &format!("<p>Rank results in your browser so your query stays there. Visitors turn it on with the Private search switch in the search page’s settings gear. It is on wherever this node has the network’s buckets.</p>{link}"),
            );
            body.push_str("</section>");
            if !writable {
                body.push_str("<fieldset disabled>");
            }
            render_features(
                &mut body,
                active,
                saved,
                "search",
                base,
                status.can_restart && base == "/app",
            );
            if let Some(plugins) = plugins {
                render_plugin_choices(&mut body, plugins, base);
            }
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
            render_features(
                &mut body,
                active,
                saved,
                "network",
                base,
                status.can_restart && base == "/app",
            );
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
            body.push_str(&format!("<p class=\"hint\">For a bug report, you can also download <a href=\"{site}/api/status\" target=\"_blank\">raw status (JSON)</a>.</p>"));
        }
        "integrations" => {
            body.push_str("<p class=\"intro\">Use your own search index from an AI app, or add sources with plugins.</p>");
            body.push_str(&format!(
                "<section class=\"cards\" aria-label=\"AI connections\">\
                 <div class=\"card\"><h3>MCP server</h3><p class=\"big\">For AI apps</p>\
                 <p>Search, official sites, packages and sourced facts. No API key.</p>\
                 <label for=\"mcp-url\">Server address</label><input id=\"mcp-url\" class=\"endpoint\" readonly value=\"{site}/mcp\">\
                 <p><a href=\"https://github.com/SueHeir/plumb-search/blob/main/docs/mcp.md\" target=\"_blank\">MCP setup guide ↗</a></p></div>\
                 <div class=\"card\"><h3>SearXNG-compatible search</h3><p class=\"big\">For local models</p>\
                 <p>Use this node in Open WebUI and other tools that take a SearXNG address.</p>\
                 <label for=\"search-url\">Base address</label><input id=\"search-url\" class=\"endpoint\" readonly value=\"{site}\">\
                 <p><a href=\"https://github.com/SueHeir/plumb-search/blob/main/docs/local-llms.md\" target=\"_blank\">App setup guides ↗</a></p></div></section>"
            ));
            body.push_str("<h2>Plugins on this node</h2><p class=\"hint\">Plugins run in a WebAssembly sandbox and can contact their declared sources. Running a plugin can send search text to that source.</p>");
            match plugins {
                Some(plugins) if plugins.list().next().is_some() => {
                    body.push_str("<ul class=\"log\">");
                    for plugin in plugins.list() {
                        body.push_str(&format!("<li><strong>{}</strong><span>{}</span></li>", escape_html(&plugin.manifest.name), escape_html(&plugin.manifest.about)));
                    }
                    body.push_str(&format!("</ul><p><a href=\"{base}?section=search#plugins\">Choose when plugins run</a></p>"));
                }
                Some(_) => body.push_str("<div class=\"notice\"><h3>No plugins installed</h3><p>Add a plugin to the plugins folder in this node’s data directory, then restart the node.</p></div>"),
                None => body.push_str("<p>Manage plugins on the remote node’s own computer.</p>"),
            }
            body.push_str("<p><a href=\"https://github.com/SueHeir/plumb-search/blob/main/docs/plugins.md\" target=\"_blank\">Plugin installation &amp; development ↗</a></p>");
        }
        "backup" => {
            if let Some(backups) = backups {
                render_backups(&mut body, backups, now, status.can_restart);
            }
        }
        "remote" => {
            if let Some(remote) = remote_control {
                render_remote_control(&mut body, remote, writable);
            }
        }
        _ => {
            render_about(&mut body, status, data_dir, now);
            body.push_str(&format!("<p>Desktop and Docker run the same node and settings panel.</p><p>Its data comes from Wikipedia, Stack Overflow and ecosyste.ms (CC BY-SA 4.0), OpenStreetMap (ODbL), Wikidata, OpenAlex and Open Library (CC0), GitHub, Tranco, Common Crawl and the Block List Project.</p><p><a href=\"https://github.com/SueHeir/plumb-search\" target=\"_blank\">Source code &amp; documentation ↗</a> · <a href=\"https://github.com/SueHeir/plumb-search#data-sources\" target=\"_blank\">Data sources &amp; licences ↗</a> · <a href=\"{site}/api/status\" target=\"_blank\">Diagnostic status ↗</a></p><p class=\"hint\">Desktop: use the tray or menu bar for Start at login and Quit Plumb Search.</p>"));
        }
    }
    if section == "overview" {
        let (value, label) = if query.refresh == "on" {
            ("off", "Pause automatic refresh")
        } else {
            ("on", "Enable automatic refresh")
        };
        body.push_str(&format!("<p class=\"hint\"><a href=\"{base}?section=overview&amp;refresh={value}\">{label}</a>. Automatic refresh reloads this page and may move keyboard focus.</p>"));
    }
    body.push_str("</main></div></div>");
    // Never reload forms: a timed reload discards unsaved edits and keyboard focus.
    let reload = if section == "overview" && query.saved.is_empty() && query.refresh == "on" {
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

pub(super) const LAYOUT_STYLE: &str = "";

/// A card: its class, title, headline and the HTML under them.
fn card(body: &mut String, class: &str, title: &str, big: &str, rest: &str) {
    body.push_str(&format!(
        "<div class=\"card {class}\">\n<h3>{}</h3>\n<p class=\"big\">{}</p>\n{rest}</div>\n",
        escape_html(title),
        escape_html(big)
    ));
}

fn meter(done: u64, total: u64, label: &str) -> String {
    let max = total.max(done).max(1);
    format!(
        "<progress aria-label=\"{label}\" value=\"{}\" max=\"{max}\"></progress>\n",
        done.min(max)
    )
}

fn render_search_card(body: &mut String, status: &Status, origin: &str, _now: u64, base: &str) {
    let origin = escape_html(origin);
    let buttons = format!(
        "<div class=\"btns\"><a class=\"btn\" href=\"{origin}/\">\
         Open search</a>\
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
                rest.push_str(&meter(progress.done, progress.total, "Setup progress"));
            }
            card(
                body,
                "search setting-up",
                "Search",
                "Setting up search",
                &rest,
            );
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
            meter(status.disk_used, limit, "Storage used"),
            bytes_words(limit)
        )
    };
    if let Some(fill) = &status.fill {
        let detail = fill.detail.trim_end_matches('.');
        rest.push_str(&format!(
            "<p>{}Filled with the network's crawls: {} sites.{}</p>\n",
            if fill.blackhole {
                format!(
                    "Blackhole: taking all the network's data ({} of the trusted nodes' lists held). ",
                    fill.lists_done
                )
            } else {
                String::new()
            },
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
            meter(status.downloaded_today, limit, "Downloads today"),
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
                    meter(p.done, p.total, "Work progress"),
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
            .map(|p| meter(p.done, p.total, "Work progress"))
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
    rest.push_str("<div class=\"btns\">");
    // Starts the next round now; while one runs, or crawling is paused,
    // there is nothing to start.
    if status.phase == Phase::Ready && !busy(status) && status.paused.is_none() {
        rest.push_str(&format!(
            "<form method=\"post\" action=\"{base}/refresh\">\
             <button type=\"submit\" class=\"alt\">Update now</button></form>\n"
        ));
    }
    rest.push_str(&pause_buttons(status, settings, base));
    rest.push_str("</div>");
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
                rest.push_str(&meter(p.done, p.total, "Work progress"));
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
        None => "<p>Find sites by what they are about, even when they use other words. Turn it on under Search \
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

fn render_backups(body: &mut String, backups: &[BackupInfo], now: u64, can_restart: bool) {
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
    let restart_hint = if can_restart {
        "then restarts this node automatically to apply the restored settings."
    } else {
        "then needs you to restart the app or container to apply the restored settings."
    };
    body.push_str(&format!(
        "<h2>Restore from a file</h2><form method=\"post\" action=\"/app/restore\" \
         enctype=\"multipart/form-data\"><label><input type=\"checkbox\" name=\"confirm\" value=\"yes\" required><span>I understand that restore replaces the current keys and settings. Keep a backup of the current keys before continuing.</span></label><label>Backup file <input type=\"file\" \
         name=\"backup\" accept=\".json,application/json\" required></label>\
         <p class=\"hint\">Restoring replaces this node\u{2019}s settings and keys with the \
         backup\u{2019}s, after saving the current ones as a backup marked before-restore, \
         {restart_hint}</p><button type=\"submit\">Restore</button></form>",
    ));
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
                    "<p>Connected: {}.</p><p>Searches ask {}: {} connected{}.</p>\
                     <p class=\"hint\">{reach} Shared crawl batches: {} received, {} \
                     published.</p>",
                    parts.join(", "),
                    net.search_scope.label().to_lowercase(),
                    net.search_peers,
                    match net.search_scope {
                        plumb_net::SearchScope::FriendsOfFriends =>
                            format!(", {} known through trusted nodes", net.friends_of_friends),
                        _ => String::new(),
                    },
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
    if !net.listening.is_empty() {
        card(
            body,
            "search",
            "Listening addresses",
            "Network connections",
            &format!(
                "<p class=\"msg\">{}</p>",
                escape_html(&net.listening.join("\n"))
            ),
        );
    }
    let agreement = &net.agreement;
    card(body, "", "Crawl agreement", &format!("{} sites confirmed", group_thousands(agreement.confirmed_sites as u64)), &format!("<p>{} waiting for agreement · {} disputed.</p><p class=\"hint\">{} trusted crawlers · {} distrusted.</p>", agreement.pending_sites, agreement.disputed_sites, agreement.vouched_crawlers, agreement.distrusted_crawlers));
    render_credits(body, &net.credits);
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
        "Node id",
        &format!(
            "<p class=\"msg\">{}</p><p class=\"msg\">{}</p>",
            escape_html(&net.peer_id),
            if net.reachable_at.is_empty() {
                "No public address yet".into()
            } else {
                escape_html(&net.reachable_at.join("\n"))
            }
        ),
    );
}

fn render_features(
    body: &mut String,
    active: &FeatureSettings,
    saved: &FeatureSettings,
    section: &str,
    base: &str,
    can_restart: bool,
) {
    let restart_hint = if can_restart {
        "Save your choices, then use Restart to apply. Search pauses briefly while the node restarts."
    } else {
        "Save your choices, then quit and reopen the app or restart the Docker container. Closing the desktop window does not quit the app."
    };
    body.push_str(&format!("<h2>Optional features</h2><p class=\"hint\">{restart_hint}</p><form method=\"post\" action=\"{base}/features\">"));
    body.push_str(&format!(
        "<input type=\"hidden\" name=\"section\" value=\"{section}\">"
    ));
    for (name, label, value, running, hint) in [
        ("network", "Join the Plumb network", saved.network, active.network, "Share crawls and search other nodes. The connection details above show the addresses this node uses."),
        ("search_by_meaning", "Search by meaning", saved.search_by_meaning, active.search_by_meaning, "Find sites by topic. Downloads a model (about 130 MB) and builds site vectors in the background."),
        ("share_popularity", "Share anonymous popularity", saved.share_popularity, active.share_popularity, "Requires the Plumb network. Reports which results are opened to help improve ranking. Off unless you enable it."),
        ("search_history", "Remember searches", saved.search_history.unwrap_or(active.search_history == Some(true)), active.search_history == Some(true), "Each browser that searches here keeps its own history on this computer: past searches, and the sites opened from them, which come first next time. Nobody sees another browser's history. Turn off on a node strangers can search."),
    ] {
        if (section == "search") != matches!(name, "search_by_meaning" | "search_history") { continue; }
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
        body.push_str(&search_scope_choice(
            saved.search_scope(),
            active.search_scope(),
        ));
        body.push_str(&credit_choices(saved, active));
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

/// What this node earned at other nodes, and what it did for them (see
/// [`plumb_net::credits`]).
fn render_credits(body: &mut String, credits: &plumb_net::credits::CreditStatus) {
    let total: i64 = credits.at_peers.iter().map(|at| at.credits.max(0)).sum();
    let headline = format!("{} credits at other nodes", group_thousands(total as u64));
    let mut rest = String::new();
    if credits.at_peers.is_empty() {
        rest.push_str(
            "<p>No node has said yet what this node has earned there. Nodes earn credits \
             at each other by crawling and by answering each other\u{2019}s searches.</p>",
        );
    } else {
        let mut rows = String::new();
        for at in credits.at_peers.iter().take(8) {
            let id = &at.peer_id;
            let short = id.get(id.len().saturating_sub(8)..).unwrap_or(id);
            let note = if at.counts {
                ""
            } else {
                " \u{b7} not spendable there yet"
            };
            rows.push_str(&format!(
                "<dt title=\"{}\"><code>\u{2026}{}</code></dt><dd>{} credits{note}</dd>",
                escape_html(id),
                escape_html(short),
                group_thousands(at.credits.max(0) as u64)
            ));
        }
        rest.push_str(&format!("<dl>{rows}</dl>"));
    }
    let limit = match credits.answer_per_day {
        Some(n) => format!(" of {} allowed", group_thousands(n)),
        None => String::new(),
    };
    rest.push_str(&format!(
        "<p>{} searches answered for other nodes today{limit} \u{b7} {} turned away as busy \
         \u{b7} {} answered for tokens.</p><p class=\"hint\">{} tokens held \u{b7} {} spent at \
         busy nodes \u{b7} {} nodes have credits here.</p>",
        group_thousands(credits.free_answers_today),
        group_thousands(credits.turned_away),
        group_thousands(credits.priority_answered),
        group_thousands(credits.tokens_held as u64),
        group_thousands(credits.tokens_spent),
        group_thousands(credits.in_credit_here as u64),
    ));
    card(body, "", "Credits", &headline, &rest);
}

/// How much this node helps other nodes, and whether it spends its credits
/// (see [`plumb_net::allowance`] and [`plumb_net::credits`]).
fn credit_choices(saved: &FeatureSettings, active: &FeatureSettings) -> String {
    let per_day = saved.answer_limit.and_then(|l| l.per_day);
    let running = match active.answer_limit.and_then(|l| l.per_day) {
        Some(n) => format!("{} a day", group_thousands(n)),
        None => "no limit".to_owned(),
    };
    let spend = saved.spend_credits.unwrap_or(true);
    let spending = if active.spend_credits.unwrap_or(true) {
        "on"
    } else {
        "off"
    };
    format!(
        "<input type=\"hidden\" name=\"credits_shown\" value=\"1\">\
         <div class=\"feature\"><label for=\"answer_per_day\">Searches answered for other \
         nodes each day <span class=\"state\">\u{b7} currently {running}</span></label>\
         <input id=\"answer_per_day\" name=\"answer_per_day\" inputmode=\"numeric\" \
         placeholder=\"No limit\" value=\"{}\" aria-describedby=\"answer-help\">\
         <p class=\"hint\" id=\"answer-help\">How much this node helps the network. Past \
         this many a day, it answers only nodes that spend credits they earned here by \
         crawling or answering. Searches on this node\u{2019}s own page are never limited. \
         Leave blank for no limit.</p></div>\
         <div class=\"feature\"><label><input type=\"checkbox\" name=\"spend_credits\" \
         value=\"1\"{}><span>Spend credits when a node is busy <span class=\"state\">\
         \u{b7} currently {spending}</span></span></label><p class=\"hint\">This node earns \
         credits at other nodes by crawling and answering their searches, and spends them to \
         be answered first when they are busy.</p></div>",
        per_day.map(|n| n.to_string()).unwrap_or_default(),
        if spend { " checked" } else { "" },
    )
}

/// The choice of which nodes network searches ask (see
/// [`plumb_net::scope`]), `saved` checked, saying which one is `running`.
fn search_scope_choice(saved: plumb_net::SearchScope, running: plumb_net::SearchScope) -> String {
    use plumb_net::SearchScope;
    let mut html = format!(
        "<fieldset class=\"feature scope\"><legend>Search the network through \
         <span class=\"state\">\u{b7} currently {}</span></legend>",
        running.label().to_lowercase()
    );
    for scope in SearchScope::ALL {
        let hint = match scope {
            SearchScope::Trusted => {
                "Only the nodes you trust: plumbsearch.org unless turned off, \
                                     and the trusted nodes below."
            }
            SearchScope::FriendsOfFriends => {
                "Your trusted nodes and the nodes they trust. The \
                                              default."
            }
            SearchScope::Anyone => {
                "Any Plumb node. Finds the most, from nodes you know nothing \
                                    about."
            }
        };
        html.push_str(&format!(
            "<label><input type=\"radio\" name=\"search_from\" value=\"{}\"{}><span>{}</span>\
             </label><p class=\"hint\">{hint}</p>",
            scope.as_str(),
            if scope == saved { " checked" } else { "" },
            scope.label()
        ));
    }
    html.push_str("</fieldset>");
    html
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
        "Visit homepages to learn what each site is about",
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
         <p class=\"hint\">Plumb visits a few thousand homepages a day to learn what sites \
         are about and find new sites, then rebuilds its index. Search keeps working when this is \
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
        "<fieldset class=\"workload page-sets\"><legend>Page sets</legend>\n\
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
            "<div class=\"page-set\"><label>{} <select name=\"page_set.{}\">{options}</select></label>\n\
             <p class=\"hint\">Keeps {} pages now, about {} MB.</p></div>\n",
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

/// For each plugin a search can fit without a keyword (by its `ids` or
/// `hints`), the choice of what such a search does with it.
fn render_plugin_choices(body: &mut String, plugins: &crate::plugins::Plugins, base: &str) {
    use crate::plugins::Suggest;
    let fitting: Vec<_> = plugins.list().filter(|p| p.manifest.can_fit()).collect();
    if fitting.is_empty() {
        return;
    }
    body.push_str(&format!(
        "<h2 id=\"plugins\">Plugins</h2><p class=\"hint\">When a search fits a plugin, such as a search \
         about a band for a music plugin, it can run on its own, or show a link that runs it, \
         which saves a source’s daily quota. Its keywords always run it.</p>\
         <form method=\"post\" action=\"{base}/plugins\">"
    ));
    for plugin in fitting {
        let chosen = plugins.suggest(plugin);
        let keywords = plugin
            .manifest
            .keywords
            .iter()
            .map(|k| k.trim())
            .filter(|k| !k.is_empty())
            .collect::<Vec<_>>();
        let mut options = String::new();
        for suggest in Suggest::ALL {
            let label = match suggest {
                Suggest::Automatic => "Run it when a search fits",
                Suggest::Button => "Show a link to its results when a search fits",
                Suggest::Keywords => "Only when a search has one of its keywords",
            };
            options.push_str(&format!(
                "<option value=\"{}\"{}>{label}</option>",
                suggest.as_str(),
                if suggest == chosen { " selected" } else { "" }
            ));
        }
        let id = format!("plugin-{}", plugin.id);
        let keywords = if keywords.is_empty() {
            String::new()
        } else {
            format!("Keywords: {}. ", keywords.join(", "))
        };
        body.push_str(&format!(
            "<div class=\"feature\"><label for=\"{id}\">{}</label> \
             <select id=\"{id}\" name=\"{}\">{options}</select>\
             <p class=\"hint\">{}{}</p></div>",
            escape_html(&plugin.manifest.name),
            escape_html(&plugin.id),
            escape_html(&keywords),
            escape_html(&plugin.manifest.about),
        ));
    }
    body.push_str("<p><button type=\"submit\">Save plugin choices</button></p></form>");
}

/// Keeps the owner's choices of when the node's plugins run, from the
/// form of [`render_plugin_choices`]: each plugin's folder name and its
/// choice.
pub(super) async fn save_plugins(State(state): State<AppState>, request: Request) -> Response {
    if state.node.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(Form(choices)) = Form::<Vec<(String, String)>>::from_request(request, &state).await
    else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "The plugin choices could not be read.",
        );
    };
    for (plugin, choice) in choices {
        let Some(suggest) = crate::plugins::Suggest::parse(&choice) else {
            return panel_error(StatusCode::BAD_REQUEST, "That is not a plugin choice.");
        };
        if let Err(err) = state.settings.plugins.set_suggest(&plugin, suggest) {
            return panel_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Could not save plugin choices: {err:#}"),
            );
        }
    }
    Redirect::to("/app?section=search&saved=plugins").into_response()
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
            storage_limit: 0,
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
        let mut setting_up = status(Phase::SettingUp, Step::Idle);
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
        assert!(
            body.contains("<progress aria-label=\"Setup progress\" value=\"0\" max=\"1\">"),
            "{body}"
        );
        assert!(body.contains("<h2>Setup</h2>"), "{body}");
        assert!(
            !body.contains("http-equiv=\"refresh\""),
            "refresh is opt-in: {body}"
        );
        assert!(!body.contains("Search in your browser"), "{body}");
        assert!(!body.contains("Update now"), "{body}");
        assert!(body.contains("/home/me/plumb &lt;data&gt;"), "{body}");
    }

    #[tokio::test]
    async fn shows_the_node_at_a_glance() {
        let mut limited = status(Phase::Ready, Step::Idle);
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
            ..Default::default()
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
            body.contains("href=\"http://127.0.0.1:7586/\">Open search"),
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
            body.contains(
                "<progress aria-label=\"Storage used\" value=\"312400000\" max=\"2000000000\">"
            ),
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
            !body.contains("http-equiv=\"refresh\""),
            "refresh is opt-in: {body}"
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
            body.contains("href=\"/app?section=resources\">Back to the panel</a>"),
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
        let response = app(status(Phase::SettingUp, Step::Idle))
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
        assert!(!search.contains("name=\"private_search\""));
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
            "network=1&credits_shown=1&answer_per_day=lots",
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
            "network=1&search_by_meaning=1&share_popularity=1",
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
        assert!(page.contains("name=\"answer_per_day\""));
        assert!(page.contains("name=\"spend_credits\" value=\"1\" checked"));
        let response = post(
            router.clone(),
            "/app/features",
            "network=1&credits_shown=1&answer_per_day=20%2C000",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let saved = node.features.lock().unwrap().clone();
        assert_eq!(saved.answer_limit.unwrap().per_day, Some(20_000));
        assert_eq!(saved.spend_credits, Some(false));
        let page = get_section(router.clone(), "network").await;
        assert!(page.contains("value=\"20000\""), "{page}");
        let response = post(
            router.clone(),
            "/app/features",
            "network=1&credits_shown=1&answer_per_day=&spend_credits=1",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let saved = node.features.lock().unwrap().clone();
        assert_eq!(saved.answer_limit.unwrap().per_day, None, "blank: no limit");
        assert_eq!(saved.spend_credits, Some(true));
        // A form without the credit choices leaves them as they were.
        post(
            router.clone(),
            "/app/features",
            "network=1",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(node.features.lock().unwrap().spend_credits, Some(true));
        let response = post(
            router.clone(),
            "/app/features",
            "section=search&search_history=1",
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(node.features.lock().unwrap().network);
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
    async fn the_credits_card_says_what_was_earned_and_given() {
        let mut status = status(Phase::Ready, Step::Idle);
        status.network = Some(plumb_net::NetStatus {
            peer_id: "me".into(),
            credits: plumb_net::credits::CreditStatus {
                at_peers: vec![
                    plumb_net::credits::CreditsAtPeer {
                        peer_id: "12D3KooWfirst-node-AAAAAAAA".into(),
                        credits: 1_200,
                        counts: true,
                    },
                    plumb_net::credits::CreditsAtPeer {
                        peer_id: "12D3KooWsecond-node-<BBBBBB>".into(),
                        credits: 34,
                        counts: false,
                    },
                ],
                free_answers_today: 4_000,
                answer_per_day: Some(10_000),
                turned_away: 7,
                tokens_held: 16,
                ..Default::default()
            },
            ..Default::default()
        });
        let (router, _) = app(status);
        let page = get_section(router, "network").await;
        assert!(page.contains("1,234 credits at other nodes"), "{page}");
        assert!(page.contains("AAAAAAAA</code></dt><dd>1,200 credits</dd>"));
        assert!(page.contains("not spendable there yet"));
        assert!(!page.contains("<BBBBBB>"), "ids are escaped");
        assert!(page.contains("4,000 searches answered for other nodes today of 10,000 allowed"));
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
        assert!(body.contains("All activity"));
        assert!(!body.contains("<strong>Error:</strong> Could not reach &lt;tranco-list.eu&gt;"));
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
    #[tokio::test]
    async fn refresh_is_opt_in_and_can_be_paused() {
        let (router, _) = app(status(Phase::SettingUp, Step::Idle));
        let normal = get_section(router.clone(), "overview").await;
        assert!(!normal.contains("http-equiv=\"refresh\""));
        let enabled = get_section(router.clone(), "overview&refresh=on").await;
        assert!(enabled.contains("content=\"30\""));
        assert!(enabled.contains("Pause automatic refresh"));
        let paused = get_section(router, "overview&refresh=off").await;
        assert!(!paused.contains("http-equiv=\"refresh\""));
    }

    #[tokio::test]
    async fn saved_restore_requires_confirmation_before_changing_anything() {
        struct BackupNode {
            dir: std::path::PathBuf,
            restored: Mutex<usize>,
        }
        impl StatusSource for BackupNode {
            fn status(&self) -> Status {
                status(Phase::Ready, Step::Idle)
            }
            fn data_dir(&self) -> Option<std::path::PathBuf> {
                Some(self.dir.clone())
            }
            fn restore_backup(&self, _: &Backup) -> anyhow::Result<()> {
                *self.restored.lock().unwrap() += 1;
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let backup = backup::save(dir.path(), None).unwrap();
        let node = Arc::new(BackupNode {
            dir: dir.path().into(),
            restored: Mutex::new(0),
        });
        let app = node_router(Arc::new(NoSearch), node.clone());
        let form = format!("name={}", backup.name);
        let response = post(
            app.clone(),
            "/app/backups/restore",
            &form,
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_text(response).await;
        assert!(body.contains("Restore this backup?"));
        assert!(body.contains("name=\"confirm\" value=\"yes\""));
        assert!(body.contains("content=\"same-origin\""));
        assert_eq!(*node.restored.lock().unwrap(), 0);
        // Leaving the confirmation page through Cancel/Back is a GET only.
        let mut cancel = Request::get("/app?section=backup")
            .header(header::HOST, "127.0.0.1:7586")
            .body(Body::empty())
            .unwrap();
        cancel.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:50000".parse::<SocketAddr>().unwrap(),
        ));
        assert_eq!(
            app.clone().oneshot(cancel).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(*node.restored.lock().unwrap(), 0);
        // A file posted without confirmation, or an interrupted multipart body,
        // must never reach the restore callback.
        for data in [
            "--test\r\nContent-Disposition: form-data; name=\"backup\"; filename=\"backup.json\"\r\n\r\n{}\r\n--test--\r\n",
            "--test\r\nContent-Disposition: form-data; name=\"confirm\"\r\n\r\nyes\r\n--test\r\nContent-Disposition: form-data; name=\"backup\"; filename=\"backup.json\"\r\n\r\n{",
        ] {
            let mut upload = Request::post("/app/restore")
                .header(header::HOST, "127.0.0.1:7586")
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=test")
                .body(Body::from(data)).unwrap();
            upload.extensions_mut().insert(ConnectInfo("127.0.0.1:50000".parse::<SocketAddr>().unwrap()));
            assert_eq!(app.clone().oneshot(upload).await.unwrap().status(), StatusCode::BAD_REQUEST);
            assert_eq!(*node.restored.lock().unwrap(), 0);
        }
        let response = post(
            app,
            "/app/backups/restore",
            &format!("{form}&confirm=yes"),
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(*node.restored.lock().unwrap(), 1);
    }
    #[test]
    fn remote_panels_never_offer_an_unrouted_restart() {
        let mut status = status(Phase::Ready, Step::Idle);
        status.can_restart = true;
        let active = FeatureSettings::default();
        let saved = FeatureSettings {
            search_by_meaning: true,
            ..active.clone()
        };
        let settings = NodeSettings::default();
        let query = PanelQuery::default();
        let mut view = PanelView {
            status: &status,
            settings: &settings,
            origin: "http://localhost:7586",
            data_dir: None,
            now: now_unix(),
            query: &query,
            active: &active,
            saved: &saved,
            writable: true,
            private_ready: false,
            base: "/app/nodes/test",
            eyebrow: "Test",
            switcher: "",
            plugins: None,
            remote_control: None,
            activity: &[],
            backups: None,
        };
        assert!(!render_panel(&view).contains("/app/nodes/test/restart"));
        view.base = "/app";
        assert!(render_panel(&view).contains("action=\"/app/restart\""));
    }
}
