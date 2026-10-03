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
use crate::node::{NodeSettings, Phase, Status, Step, MB};

/// Seconds between two reloads of the panel while work is under way.
const BUSY_RELOAD_SECONDS: u32 = 5;
/// Seconds between two reloads otherwise.
const IDLE_RELOAD_SECONDS: u32 = 60;

const PANEL_STYLE: &str = "\
.panel{max-width:56rem;padding-top:1.5rem}\
.panel h1{font-size:1.6rem}\
.panel h2{font-size:1.05rem;margin:2rem 0 .5rem}\
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
.panel form{display:block}\
.panel label{display:flex;gap:.6rem;align-items:center;margin-top:.75rem}\
.panel label input[type=checkbox]{flex:none}\
.panel input[type=number]{flex:none;width:7rem}\
.panel form button{margin-top:.9rem}\
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

pub(super) async fn panel(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(origin) = request_origin(&headers, &uri) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let status = node.status();
    let settings = node.settings().unwrap_or_default();
    let data_dir = node.data_dir();
    let page = render_panel(&status, &settings, &origin, data_dir.as_deref(), now_unix());
    (
        StatusCode::OK,
        panel_headers(),
        [(header::CACHE_CONTROL, "no-store")],
        Html(page),
    )
        .into_response()
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
fn panel_error(status: StatusCode, message: &str) -> Response {
    let body = format!(
        "<main class=\"wrap panel\">\n<h1>Plumb Search node</h1>\n\
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
        "<main class=\"wrap panel\">\n<h1>Add Plumb Search to Firefox</h1>\n\
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
    let (Some(download), Some(storage)) = (
        parse_limit(&form.download_limit_mb_per_day),
        parse_limit(&form.storage_limit_mb),
    ) else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "Limits are whole numbers of megabytes, or empty for none. Nothing was changed.",
        );
    };
    let settings = NodeSettings {
        background_updates: form.background_updates.is_some(),
        download_limit_mb_per_day: download,
        storage_limit_mb: storage,
    };
    if let Err(err) = node.change_settings(settings) {
        warn!("could not save the settings: {err:#}");
        return panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not save the settings: {err:#}"),
        );
    }
    Redirect::to("/app").into_response()
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
fn refusal(request: &Request) -> Option<&'static str> {
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

fn forbidden(why: &str) -> Response {
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

pub(super) fn render_panel(
    status: &Status,
    settings: &NodeSettings,
    origin: &str,
    data_dir: Option<&Path>,
    now: u64,
) -> String {
    let mut body = String::from("<main class=\"wrap panel\">\n<h1>Plumb Search node</h1>\n");
    body.push_str("<section class=\"cards\">\n");
    render_search_card(&mut body, status, origin, now);
    render_storage_card(&mut body, status, settings);
    render_downloads_card(&mut body, status, settings);
    render_crawl_card(&mut body, status, now);
    render_network_card(&mut body);
    body.push_str("</section>\n");
    if setting_up(status) {
        render_steps(&mut body, status, now);
    }
    render_settings(&mut body, settings);
    render_browser(&mut body, origin);
    render_about(&mut body, status, data_dir, now);
    body.push_str("</main>");
    let reload = if busy(status) {
        BUSY_RELOAD_SECONDS
    } else {
        IDLE_RELOAD_SECONDS
    };
    // After page_with_head's no-referrer, so it wins (see panel_headers).
    let head = format!(
        "<meta name=\"referrer\" content=\"{PANEL_REFERRER_POLICY}\">\n\
         <meta http-equiv=\"refresh\" content=\"{reload}\">\n<style>{PANEL_STYLE}</style>\n"
    );
    page_with_head("Plumb Search node", &head, &body)
}

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

fn render_crawl_card(body: &mut String, status: &Status, now: u64) {
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
    if status.phase == Phase::Ready && !crawling && status.paused.is_none() {
        rest.push_str(
            "<form method=\"post\" action=\"/app/refresh\">\
             <button type=\"submit\" class=\"alt\">Update now</button></form>\n",
        );
    }
    card(body, "", "Crawling", &big, &rest);
}

fn render_network_card(body: &mut String) {
    card(
        body,
        "",
        "Plumb network",
        "Not connected",
        "<p>0 nodes connected. This node builds and searches its own index; connecting to \
         other Plumb nodes comes with network support.</p>\n",
    );
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

fn render_settings(body: &mut String, settings: &NodeSettings) {
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
        "<h2>Settings</h2>\n<form method=\"post\" action=\"/app/settings\">\n\
         <label><input type=\"checkbox\" name=\"background_updates\" value=\"1\"{checked}>\
         <span>Keep the index up to date in the background</span></label>\n\
         <p class=\"hint\">Plumb visits a few thousand homepages a day to learn sites' names \
         and find new sites, then rebuilds its index. Search keeps working when this is \
         off.</p>\n\
         <label>Download limit <input type=\"number\" name=\"download_limit_mb_per_day\" \
         min=\"0\" step=\"1\" value=\"{}\" placeholder=\"none\"> MB a day</label>\n\
         <p class=\"hint\">Crawling pauses for the rest of the day once it is reached. Empty \
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
    body.push_str("<h2>About</h2>\n<dl>\n");
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
        });
        (node_router(Arc::new(NoSearch), node.clone()), node)
    }

    async fn get_panel(app: Router) -> String {
        let request = Request::get("/app")
            .header(header::HOST, "127.0.0.1:7586")
            .body(Body::empty())
            .unwrap();
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
        assert_eq!(response.headers()[header::LOCATION], "/app");
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
    async fn plumb_serve_has_no_panel() {
        let router = crate::web::router(Arc::new(NoSearch));
        let request = Request::get("/app").body(Body::empty()).unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
