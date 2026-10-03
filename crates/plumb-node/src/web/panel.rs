//! The node's panel, `GET /app`: what works now, how far setup has come,
//! the settings and how to use Plumb from a browser. The desktop app's
//! window shows it; searching happens in the browser.
//!
//! - `GET /app` shows the panel, reloading itself while work is under way
//!   (the page runs no script),
//! - `POST /app/settings` saves the settings form,
//! - `POST /app/refresh` starts a refresh now,
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
use crate::node::control;
use crate::node::features::FeatureSettings;
use crate::node::{NodeSettings, Phase, Status, Step, MB};

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
    bootstrap: String,
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
    } else {
        features.network = form.network.is_some();
        features.share_popularity = form.share_popularity.is_some();
        features.bootstrap = form
            .bootstrap
            .split_whitespace()
            .map(str::to_owned)
            .collect();
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
    let settings = match settings_from_form(&form) {
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
pub(super) fn settings_from_form(form: &SettingsForm) -> Result<NodeSettings, Response> {
    let (Some(download), Some(storage)) = (
        parse_limit(&form.download_limit_mb_per_day),
        parse_limit(&form.storage_limit_mb),
    ) else {
        return Err(panel_error(
            StatusCode::BAD_REQUEST,
            "Limits are whole numbers of megabytes, or empty for none. Nothing was changed.",
        ));
    };
    Ok(NodeSettings {
        background_updates: form.background_updates.is_some(),
        download_limit_mb_per_day: download,
        storage_limit_mb: storage,
    })
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
        Ok(token) => panel_page(render_new_token(&token, &origin, node.bind())),
        Err(err) => panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not turn remote control on: {err:#}"),
        ),
    }
}

/// The page that shows a new token, the one time it can be read.
fn render_new_token(token: &str, origin: &str, bind: Option<SocketAddr>) -> String {
    let address = match bind {
        Some(addr) if addr.ip().is_unspecified() => {
            format!("http://&lt;this computer's address&gt;:{}", addr.port())
        }
        _ => escape_html(origin),
    };
    let body = format!(
        "<main class=\"wrap node-panel\">\n<h1>Remote control is on</h1>\n\
         <p>On the other computer, open the Plumb Search app, choose \
         <strong>Connect to a node</strong>, and enter this node's address and token:</p>\n\
         <dl><dt>Address</dt><dd><code>{address}</code></dd>\n\
         <dt>Token</dt><dd><code>{}</code></dd></dl>\n\
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
    if active != saved {
        body.push_str("<p class=\"notice\" role=\"status\">Feature changes saved. Quit and reopen the desktop app, or restart the Docker container, to apply them. Closing the desktop window does not quit the app.</p>");
    } else if query.saved == "settings" {
        body.push_str(
            "<p class=\"notice\" role=\"status\">Resource settings saved and applied.</p>",
        );
    } else if query.saved == "features" {
        body.push_str("<p class=\"notice\" role=\"status\">Feature settings saved. No restart is needed because they match the running node.</p>");
    }
    if !writable {
        body.push_str("<p class=\"notice\">Viewing this node remotely. Settings are read-only. To change them, open the desktop app or connect to the node through a loopback address on its host. Docker bridge networking may also require host-side configuration.</p>");
    }
    match section {
        "overview" => {
            body.push_str("<p class=\"intro\">Search readiness, background work, and the resources your node is using.</p><section class=\"cards\" aria-label=\"Node overview\">");
            render_search_card(&mut body, status, origin, now);
            render_storage_card(&mut body, status, settings);
            render_downloads_card(&mut body, status, settings);
            if !writable {
                body.push_str("<fieldset disabled>");
            }
            render_crawl_card(&mut body, status, now, base);
            if !writable {
                body.push_str("</fieldset>");
            }
            render_network_card(&mut body, status, active);
            body.push_str("</section>");
            if setting_up(status) {
                render_steps(&mut body, status, now);
            }
            if let Some(err) = &status.last_error {
                body.push_str(&format!("<div class=\"err\" role=\"alert\"><strong>Last update failed</strong><p>{}</p></div>", escape_html(&err.message)));
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
            render_search_card(&mut body, status, origin, now);
            let meaning = match status.meaning_sites {
                Some(n) => format!("{} sites ready", group_thousands(n)),
                None if active.search_by_meaning => "Preparing model".into(),
                None => "Off".into(),
            };
            card(
                &mut body,
                "",
                "Search by meaning",
                &meaning,
                "<p>Find sites by their subject as well as their name. Enable it below.</p>",
            );
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
            render_network_card(&mut body, status, active);
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
.node-switch{display:flex;flex-wrap:wrap;gap:.4rem;margin-bottom:1rem}.node-switch a{padding:.4rem .8rem;border:1px solid var(--line);border-radius:999px;text-decoration:none;font-size:.9rem;color:var(--fg)}.node-switch a[aria-current]{border-color:var(--accent);color:var(--accent);font-weight:600}.node-panel{max-width:72rem;padding:2rem 2rem 4rem}.node-heading,.section-heading{display:flex;align-items:center;justify-content:space-between;gap:1rem}.node-heading h1{font-size:1.8rem}.eyebrow{font-size:.7rem;letter-spacing:.13em;color:var(--muted);margin:0 0 .3rem}.node-nav{display:flex;flex-wrap:wrap;gap:.4rem;border-bottom:1px solid var(--line);padding:1.5rem 0 1rem;margin-bottom:1.5rem}.node-panel a{color:var(--accent)}.node-panel a.btn:not(.alt){color:var(--bg)}.node-nav a{padding:.55rem .85rem;text-decoration:none;border-radius:.5rem;color:var(--muted)}.node-nav a[aria-current]{background:var(--accent);color:var(--bg);font-weight:600}.section-heading h2{margin:0;font-size:1.4rem}.section-heading>a{font-size:.85rem}.intro{color:var(--muted);max-width:45rem}.notice{padding:.85rem 1rem;border-left:3px solid var(--accent);background:color-mix(in srgb,var(--accent) 8%,var(--bg));border-radius:.3rem}.node-panel form{max-width:46rem}.node-panel fieldset{border:0;margin:0;padding:0;min-width:0}.node-panel fieldset:disabled{opacity:.65}.node-panel textarea{display:block;width:100%;min-height:6rem;font:inherit;background:var(--bg);color:var(--fg);padding:.75rem;border:1px solid var(--line);border-radius:.5rem}.node-panel .feature{padding:.8rem 0;border-bottom:1px solid var(--line)}.node-panel .feature label{margin:0}.node-panel .feature p{margin:.35rem 0 0 1.65rem}.node-panel .state{font-size:.8rem;color:var(--muted)}.node-panel :focus-visible{outline:3px solid var(--accent);outline-offset:3px}.node-panel dl{grid-template-columns:minmax(6rem,auto) minmax(0,1fr)}@media(max-width:600px){.node-panel{padding:1rem 1rem 3rem}.node-heading{align-items:flex-start}.node-heading h1{font-size:1.5rem}.node-nav{gap:.2rem}.node-nav a{padding:.5rem .6rem;font-size:.9rem}.cards{grid-template-columns:minmax(0,1fr)}.node-panel label{flex-wrap:wrap}.section-heading{align-items:flex-start}.section-heading>a{white-space:nowrap}}";

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

fn render_search_card(body: &mut String, status: &Status, origin: &str, now: u64) {
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
            let rest = format!(
                "<p>Search works now with the {} most popular sites. {}</p>\n{buttons}",
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
    let rest = if limit == 0 {
        "<p>No storage limit.</p>\n".to_string()
    } else {
        format!(
            "{}<p>of {} allowed.</p>\n",
            meter(status.disk_used, limit),
            bytes_words(limit)
        )
    };
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

fn render_crawl_card(body: &mut String, status: &Status, now: u64, base: &str) {
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
        (
            "Rebuilding index".to_string(),
            "<p>Preparing updated search results.</p>".to_string(),
        )
    } else if status.phase != Phase::Ready {
        ("Waiting for setup".to_string(), String::new())
    } else if status.last_error.is_some() {
        (
            "Retrying update".to_string(),
            "<p>The last update failed. Plumb will retry automatically.</p>".to_string(),
        )
    } else if let Some(reason) = &status.paused {
        (
            "Paused".to_string(),
            format!("<p>{}.</p>\n", escape_html(reason)),
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
    card(body, "", "Crawling", &big, &rest);
}

fn render_network_card(body: &mut String, status: &Status, active: &FeatureSettings) {
    let (headline, rest) = match &status.network {
        Some(net) => (if net.connected_peers > 0 { "Connected" } else { "Looking for peers" }, format!("<p>{} nodes connected · {} relay peers.</p><p class=\"hint\">Shared crawl batches: {} received, {} published.</p>", net.connected_peers, net.relaying_peers, net.batches_received, net.batches_published)),
        None if active.network => ("Starting", "<p>The Plumb network is enabled and is starting.</p>".into()),
        None => ("Not connected", "<p>Network sharing is off. This node searches its own index.</p>".into()),
    };
    card(body, "", "Plumb network", headline, &rest);
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
        ("private_search", "Private browser search", saved.private_search, active.private_search, "Keep search words in the browser. Requires a build with the private-search module and extra disk space for search buckets."),
        ("share_popularity", "Share anonymous popularity", saved.share_popularity, active.share_popularity, "Requires the Plumb network. Reports which results are opened to help improve ranking. Off unless you enable it."),
    ] {
        if (section == "search") != matches!(name, "search_by_meaning" | "private_search") { continue; }
        body.push_str(&format!("<div class=\"feature\"><label><input type=\"checkbox\" name=\"{name}\" value=\"1\"{}><span>{label} <span class=\"state\">· currently {}</span></span></label><p class=\"hint\">{hint}</p></div>", if value { " checked" } else { "" }, if running { "on" } else { "off" }));
    }
    if section == "network" {
        body.push_str(&format!("<label for=\"bootstrap\">Bootstrap nodes</label><p class=\"hint\" id=\"bootstrap-help\">One multiaddress per line. Leave empty to discover nearby nodes only; remote peers need a reachable bootstrap node.</p><textarea id=\"bootstrap\" name=\"bootstrap\" aria-describedby=\"bootstrap-help\" spellcheck=\"false\">{}</textarea>", escape_html(&saved.bootstrap.join("\n"))));
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
    let checked = if settings.background_updates {
        " checked"
    } else {
        ""
    };
    let limit = |mb: u64| {
        if mb == 0 {
            String::new()
        } else {
            mb.to_string()
        }
    };
    body.push_str(&format!(
        "<h2>Crawling &amp; limits</h2>\n<form method=\"post\" action=\"{base}/settings\">\n\
         <label><input type=\"checkbox\" name=\"background_updates\" value=\"1\"{checked}>\
         <span>Keep the index up to date in the background</span></label>\n\
         <p class=\"hint\">Plumb visits a few thousand homepages a day to learn sites' names \
         and find new sites, then rebuilds its index. Search keeps working when this is \
         off.</p>\n\
         <label>Download limit <input type=\"number\" name=\"download_limit_mb_per_day\" \
         min=\"0\" step=\"1\" value=\"{}\" placeholder=\"none\"> MB per UTC day</label>\n\
         <p class=\"hint\">Crawling pauses for the rest of the UTC day once it is reached. Empty \
         for no limit.</p>\n\
         <label>Storage limit <input type=\"number\" name=\"storage_limit_mb\" min=\"0\" \
         step=\"1\" value=\"{}\" placeholder=\"none\"> MB</label>\n\
         <p class=\"hint\">Crawling pauses while the data folder is bigger. Empty for no \
         limit.</p>\n\
         <button type=\"submit\">Save settings</button>\n</form>\n",
        limit(settings.download_limit_mb_per_day),
        limit(settings.storage_limit_mb)
    ));
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
            network: None,
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
        assert!(
            body.contains("<p class=\"big\">Not connected</p>"),
            "{body}"
        );
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
            retry_at: Some(now_unix() + 600),
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
        ready.next_refresh = Some(now_unix() + 7200);
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
            }
        );

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
        assert!(body.contains("3 nodes connected · 2 relay peers"));
        assert!(body.contains("&lt;untrusted-peer&gt;"));
        assert!(!body.contains("<untrusted-peer>"));
    }

    #[tokio::test]
    async fn feature_changes_require_local_access_validate_and_show_restart() {
        let (router, node) = app(status(Phase::Ready, Step::Idle));
        for form in ["share_popularity=1", "network=1&bootstrap=not-an-address"] {
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
    async fn plumb_serve_has_no_panel() {
        let router = crate::web::router(Arc::new(NoSearch));
        let request = Request::get("/app").body(Body::empty()).unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
