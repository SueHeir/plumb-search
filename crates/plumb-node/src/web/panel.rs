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
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use plumb_core::now_unix;
use serde::Deserialize;
use tracing::warn;

use super::{
    escape_html, group_thousands, page_with_head, request_origin, security_headers, time_ago,
    time_until, AppState,
};
use crate::node::{NodeSettings, Phase, Status, Step};

/// Seconds between two reloads of the panel while work is under way.
const BUSY_RELOAD_SECONDS: u32 = 5;
/// Seconds between two reloads otherwise.
const IDLE_RELOAD_SECONDS: u32 = 60;

const PANEL_STYLE: &str = "\
.panel{max-width:40rem;padding-top:1.5rem}\
.panel h1{font-size:1.6rem}\
.panel h2{font-size:1.05rem;margin:2rem 0 .5rem}\
.now{margin:1.25rem 0;padding:1rem 1.2rem;border:1px solid var(--line);border-radius:.75rem}\
.now h2{margin:0 0 .25rem;font-size:1.3rem}\
.now p{margin:.25rem 0}\
.now.ready h2{color:var(--url)}\
.now.limited h2{color:var(--accent)}\
.btn{display:inline-block;padding:.55rem 1rem;border-radius:.5rem;background:var(--accent);\
color:var(--bg);text-decoration:none}\
.steps li{display:flex;gap:.6rem;padding:.45rem 0;border:0}\
.steps .i{flex:none;width:1.2rem;text-align:center}\
.steps .done{color:var(--muted)}\
.steps .now{margin:0;padding:0;border:0;font-weight:600}\
.steps small{display:block;font-weight:400;color:var(--muted)}\
.panel form{display:block}\
.panel label{display:flex;gap:.6rem;align-items:flex-start}\
.panel label input{flex:none;margin-top:.3rem}\
.panel form button{margin-top:.75rem}\
.howto{list-style:decimal;padding-left:1.5rem}.howto li{border:0;padding:.3rem 0}\
.hint{margin:.25rem 0 0 1.6rem;font-size:.85rem;color:var(--muted)}\
code{overflow-wrap:anywhere;font:.9rem ui-monospace,monospace;padding:.1rem .3rem;\
border:1px solid var(--line);border-radius:.3rem}\
dl{display:grid;grid-template-columns:max-content 1fr;gap:.25rem 1rem;font-size:.9rem}\
dt{color:var(--muted)}dd{margin:0;overflow-wrap:anywhere}";

/// The settings form as posted. A checkbox that is not ticked is not sent.
#[derive(Debug, Deserialize)]
pub(super) struct SettingsForm {
    #[serde(default)]
    background_updates: Option<String>,
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
        security_headers(),
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
        return (
            StatusCode::BAD_REQUEST,
            security_headers(),
            "Bad settings form.\n",
        )
            .into_response();
    };
    let settings = NodeSettings {
        background_updates: form.background_updates.is_some(),
    };
    if let Err(err) = node.change_settings(settings) {
        warn!("could not save the settings: {err:#}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            security_headers(),
            format!("Could not save the settings: {err:#}\n"),
        )
            .into_response();
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
        .is_some_and(|ConnectInfo(peer)| peer.ip().is_loopback());
    if !local {
        return Some("Settings can only be changed on the computer Plumb runs on.");
    }
    let headers = request.headers();
    let own = request_origin(headers, request.uri());
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

fn forbidden(why: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        security_headers(),
        format!("{why}\n"),
    )
        .into_response()
}

/// What the panel says in its box at the top.
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

pub(super) fn render_panel(
    status: &Status,
    settings: &NodeSettings,
    origin: &str,
    data_dir: Option<&Path>,
    now: u64,
) -> String {
    let mut body = String::from("<main class=\"wrap panel\">\n<h1>Plumb Search</h1>\n");
    render_now(&mut body, status, origin, now);
    render_steps(&mut body, status, settings, now);
    render_browser(&mut body, origin);
    render_settings(&mut body, status, settings);
    render_about(&mut body, status, data_dir, now);
    body.push_str("</main>");
    let reload = if busy(status) {
        BUSY_RELOAD_SECONDS
    } else {
        IDLE_RELOAD_SECONDS
    };
    let head = format!(
        "<meta http-equiv=\"refresh\" content=\"{reload}\">\n<style>{PANEL_STYLE}</style>\n"
    );
    page_with_head("Plumb Search", &head, &body)
}

fn render_now(body: &mut String, status: &Status, origin: &str, now: u64) {
    let open = format!(
        "<p><a class=\"btn\" href=\"{}/\" target=\"_blank\">Search in your browser</a></p>\n",
        escape_html(origin)
    );
    match readiness(status) {
        Readiness::SettingUp => {
            body.push_str(
                "<section class=\"now setup\">\n<h2>Setting up search</h2>\n\
                 <p>Plumb is getting a list of popular sites and building a first index. \
                 Search is ready in a minute or two.</p>\n",
            );
            render_progress(body, status);
            if let Some(err) = &status.last_error {
                body.push_str(&format!(
                    "<p class=\"err\"><strong>Something went wrong</strong> {}: {}",
                    time_ago(err.at, now),
                    escape_html(&err.message)
                ));
                if let Some(retry_at) = err.retry_at {
                    body.push_str(&format!(
                        " Plumb will try again {}.",
                        time_until(retry_at, now)
                    ));
                }
                body.push_str("</p>\n");
            }
            body.push_str("</section>\n");
        }
        Readiness::Limited => {
            let why = if status.wikidata_error.is_some() {
                "Plumb could not download Wikidata's list of official websites yet and will \
                 try again, so results get better once it is in."
            } else {
                "Plumb is still adding Wikidata's list of official websites and more \
                 rankings, so results get better once that is done."
            };
            body.push_str(&format!(
                "<section class=\"now limited\">\n<h2>Limited search is ready</h2>\n\
                 <p>Search works now with the {} most popular sites. {}</p>\n{open}</section>\n",
                group_thousands(status.sites),
                escape_html(why)
            ));
        }
        Readiness::Ready => {
            body.push_str(&format!(
                "<section class=\"now ready\">\n<h2>Search is ready</h2>\n\
                 <p>{} sites indexed.</p>\n{open}</section>\n",
                group_thousands(status.sites)
            ));
        }
    }
}

fn render_progress(body: &mut String, status: &Status) {
    body.push_str(&format!("<p>{}</p>\n", escape_html(&status.detail)));
    if let Some(progress) = &status.progress {
        let max = progress.total.max(progress.done).max(1);
        body.push_str(&format!(
            "<progress value=\"{}\" max=\"{max}\"></progress>\n<p class=\"s\">{} of {} {}</p>\n",
            progress.done,
            group_thousands(progress.done),
            group_thousands(progress.total),
            escape_html(&progress.unit)
        ));
    }
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

fn render_steps(body: &mut String, status: &Status, settings: &NodeSettings, now: u64) {
    body.push_str("<h2>Getting search ready</h2>\n<ol class=\"steps\">\n");
    let ready = status.phase == Phase::Ready;
    let setting_up_step = |steps: &[Step]| !ready && steps.contains(&status.step);

    let list = if ready {
        "done"
    } else if setting_up_step(&[Step::Downloading, Step::Starting, Step::Retrying]) {
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
    } else if !settings.background_updates {
        (
            "wait",
            Some("Paused: background updates are off.".to_string()),
        )
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

fn render_browser(body: &mut String, origin: &str) {
    let origin = escape_html(origin);
    body.push_str(&format!(
        "<h2>Use Plumb from your browser</h2>\n\
         <p>Plumb searches in your web browser. To search from the address bar, add Plumb \
         as a search engine.</p>\n\
         <p><a class=\"btn\" href=\"{origin}{ADD_TO_FIREFOX_PATH}\" target=\"_blank\">\
         Add to Firefox</a></p>\n\
         <p>In other browsers, add a search engine in the browser's settings with this \
         address:</p>\n\
         <p><code>{origin}/search?q=%s</code></p>\n\
         <p class=\"s\">Your country and other search options are on the search page.</p>\n"
    ));
}

fn render_settings(body: &mut String, status: &Status, settings: &NodeSettings) {
    let checked = if settings.background_updates {
        " checked"
    } else {
        ""
    };
    body.push_str(&format!(
        "<h2>Settings</h2>\n<form method=\"post\" action=\"/app/settings\">\n\
         <label><input type=\"checkbox\" name=\"background_updates\" value=\"1\"{checked}>\
         <span>Keep the index up to date in the background</span></label>\n\
         <p class=\"hint\">Plumb visits a few thousand homepages a day to learn sites' names \
         and find new sites, then rebuilds its index. Turn this off to save bandwidth; search \
         keeps working with the index you have.</p>\n\
         <button type=\"submit\">Save settings</button>\n</form>\n"
    ));
    if settings.background_updates && status.phase == Phase::Ready {
        body.push_str(
            "<form method=\"post\" action=\"/app/refresh\">\
             <button type=\"submit\">Update now</button></form>\n",
        );
    }
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
    if let Some(next) = status.next_refresh.filter(|_| status.background_updates) {
        row("Next update", time_until(next, now));
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
        let mut request = Request::post(path)
            .header(header::HOST, "127.0.0.1:7586")
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
        assert!(body.contains("<h2>Setting up search</h2>"), "{body}");
        assert!(body.contains("Downloading the Tranco list"), "{body}");
        assert!(body.contains("<progress value=\"0\" max=\"1\">"), "{body}");
        assert!(body.contains("content=\"5\""), "reloads often: {body}");
        assert!(!body.contains("Search in your browser"), "{body}");
        assert!(!body.contains("Update now"), "{body}");
        assert!(body.contains("/home/me/plumb &lt;data&gt;"), "{body}");
    }

    #[tokio::test]
    async fn says_limited_search_is_ready_until_wikidata_is_in() {
        let mut limited = status(Phase::Ready, Step::Downloading);
        limited.sites = 250_000;
        limited.wikidata_missing = true;
        limited.detail = "Asking Wikidata for official websites".into();
        let body = get_panel(app(limited.clone()).0).await;
        assert!(body.contains("<h2>Limited search is ready</h2>"), "{body}");
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

        let mut ready = status(Phase::Ready, Step::Idle);
        ready.sites = 260_123;
        ready.last_refresh = Some(now_unix() - 3600);
        let body = get_panel(app(ready).0).await;
        assert!(body.contains("<h2>Search is ready</h2>"), "{body}");
        assert!(body.contains("260,123 sites indexed"), "{body}");
        assert!(body.contains("Update now"), "{body}");
        assert!(
            body.contains("content=\"60\""),
            "reloads seldom when idle: {body}"
        );
    }

    #[tokio::test]
    async fn saves_settings_from_this_computer_only() {
        let (router, node) = app(status(Phase::Ready, Step::Idle));
        let body = get_panel(router.clone()).await;
        assert!(
            body.contains("name=\"background_updates\" value=\"1\" checked"),
            "{body}"
        );

        // An unticked box is left out of the form.
        let response = post(
            router.clone(),
            "/app/settings",
            "",
            "127.0.0.1:50000",
            Some("http://127.0.0.1:7586"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/app");
        assert!(!node.settings.lock().unwrap().background_updates);
        let body = get_panel(router.clone()).await;
        assert!(
            body.contains("Paused: background updates are off."),
            "{body}"
        );
        assert!(!body.contains("Update now"), "{body}");

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
