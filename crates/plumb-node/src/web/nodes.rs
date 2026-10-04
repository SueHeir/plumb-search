//! Other nodes controlled from this node's panel: "Connect to a node" in the
//! desktop app ([`crate::node::NodeConfig::manage_other_nodes`]).
//!
//! - `GET /app/nodes/new` asks for a node's address and remote control
//!   token, `POST /app/nodes` checks them with the node and saves them,
//! - `GET /app/nodes/<id>` shows that node's panel, with this node fetching
//!   it from the node's control API ([`super::control`]),
//! - `POST /app/nodes/<id>/settings`, `/features`, `/refresh`, `/pause`,
//!   `/retry` and `/network/retry` pass the panel's forms on to the node, `/remove`
//!   forgets it.
//!
//! The window never sees the tokens: they stay in `DIR/remote-nodes.json`
//! (readable by its owner only, on Unix), and this node adds them to its
//! own requests. Every page here is only for this computer, under the same
//! checks as changing this node's own settings ([`panel::refusal`]).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use axum::extract::{FromRequest, Path as UrlPath, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use plumb_core::now_unix;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use url::{Host, Url};

use super::control::{ControlError, ControlView};
use super::panel::{
    apply_features_form, forbidden, panel_error, panel_page, paused, refusal, render_panel,
    settings_from_form, FeaturesForm, PanelQuery, PanelView, PauseForm, RetryForm, SettingsForm,
    LAYOUT_STYLE, PANEL_STYLE,
};
use super::{escape_html, page_with_head, AppState, StatusSource};
use crate::node::control::{hex_encode, TOKEN_PREFIX};

/// The file the other nodes are kept in.
const FILE_NAME: &str = "remote-nodes.json";

/// How long a request to another node may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Another node, as "Connect to a node" saved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RemoteNode {
    /// Names the node in this panel's addresses.
    id: String,
    name: String,
    /// The node's origin, such as `https://homelab.example:8080`.
    url: String,
    token: String,
}

fn nodes_path(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

fn load(dir: &Path) -> Result<Vec<RemoteNode>> {
    match std::fs::read(nodes_path(dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {FILE_NAME}")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("reading {FILE_NAME}")),
    }
}

/// Writes the list in one go, readable by its owner only: it holds tokens.
fn save(dir: &Path, nodes: &[RemoteNode]) -> Result<()> {
    use std::io::Write as _;
    let path = nodes_path(dir);
    let tmp = crate::temp_path_for(&path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let written = options.open(&tmp).and_then(|mut file| {
        file.write_all(&serde_json::to_vec_pretty(nodes)?)?;
        file.sync_all()
    });
    if let Err(err) = written.and_then(|()| std::fs::rename(&tmp, &path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
}

fn find(node: &dyn StatusSource, id: &str) -> Result<Option<RemoteNode>> {
    let dir = node.data_dir().context("this node has no data folder")?;
    Ok(load(&dir)?.into_iter().find(|remote| remote.id == id))
}

/// The row of nodes above the panel: this computer, the saved nodes and
/// "Connect to a node". `current` is the id of the node shown, `None` for
/// this computer; `Some("new")` for the connect page.
pub(super) fn switcher(node: &dyn StatusSource, current: Option<&str>) -> String {
    let nodes = node
        .data_dir()
        .and_then(|dir| load(&dir).map_err(|err| warn!("{err:#}")).ok())
        .unwrap_or_default();
    let link = |href: &str, label: &str, here: bool| {
        format!(
            "<a href=\"{href}\"{}>{}</a>",
            if here { " aria-current=\"page\"" } else { "" },
            escape_html(label)
        )
    };
    let mut html = String::from("<nav class=\"node-switch\" aria-label=\"Nodes\">");
    html.push_str(&link("/app", "This computer", current.is_none()));
    for remote in &nodes {
        html.push_str(&link(
            &format!("/app/nodes/{}", remote.id),
            &remote.name,
            current == Some(remote.id.as_str()),
        ));
    }
    html.push_str(&link(
        "/app/nodes/new",
        "+ Connect to a node",
        current == Some("new"),
    ));
    html.push_str("</nav>");
    html
}

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/app/nodes", post(add))
        .route("/app/nodes/new", get(new_page))
        .route("/app/nodes/{id}", get(show))
        .route("/app/nodes/{id}/settings", post(change_settings))
        .route("/app/nodes/{id}/features", post(change_features))
        .route("/app/nodes/{id}/refresh", post(refresh))
        .route("/app/nodes/{id}/network/retry", post(retry_network))
        .route("/app/nodes/{id}/pause", post(pause))
        .route("/app/nodes/{id}/retry", post(retry))
        .route("/app/nodes/{id}/remove", post(remove))
}

/// This node, when it may control others and the request comes from its
/// own panel; the refusal otherwise.
// A response is big, but these run once per request.
#[allow(clippy::result_large_err)]
fn manager(
    state: &AppState,
    request: &Request,
) -> Result<std::sync::Arc<dyn StatusSource>, Response> {
    let node = state
        .node
        .clone()
        .filter(|node| node.manages_other_nodes() && node.data_dir().is_some())
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    if let Some(why) = refusal(request) {
        return Err(forbidden(why));
    }
    Ok(node)
}

/// A request to another node's control API went wrong; says what to do.
#[derive(Debug)]
struct ClientError(String);

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            // Nodes are on local networks, which a system proxy cannot reach.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("plumb-desktop/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("an HTTP client")
    })
}

/// Sends a request to `remote`'s control API at `path`, with `body` as JSON
/// when given, and returns the answer's body.
async fn call(
    remote: &RemoteNode,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<Vec<u8>, ClientError> {
    // Check saved entries too: files written by older versions can contain
    // plaintext LAN addresses. Refuse before constructing a bearer request.
    let origin = parse_address(&remote.url)
        .map_err(|err| ClientError(format!("Could not connect to this saved node: {err}")))?;
    let url = format!("{origin}{path}");
    let request = match body {
        Some(body) => client()
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body),
        None => client().get(&url),
    };
    let response = request
        .bearer_auth(&remote.token)
        .send()
        .await
        .map_err(|err| {
            let why = if err.is_timeout() {
                "it did not answer in time"
            } else if err.is_connect() {
                "nothing answered at that address"
            } else {
                "the connection failed"
            };
            ClientError(format!(
                "Could not reach {}: {why}. Check that the node is running and that the \
                 address and port are right. A Docker container must publish its port \
                 (for example -p 8080:8080), and a node must listen on its network \
                 address (--bind 0.0.0.0:8080), not only on 127.0.0.1.",
                remote.url
            ))
        })?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|err| ClientError(format!("The node's answer was cut off: {err}")))?
        .to_vec();
    if status.is_success() {
        return Ok(bytes);
    }
    let said = serde_json::from_slice::<ControlError>(&bytes)
        .map(|error| error.error)
        .ok();
    Err(ClientError(match status.as_u16() {
        404 => "Remote control is off on that node, or it runs a Plumb Search version \
                without it. Turn it on there: in its panel under Remote control, or with \
                \"plumb remote-control on\" (for Docker: docker exec <container> plumb \
                remote-control on)."
            .to_string(),
        401 => "The node did not accept the token. Make a new one on that node and \
                connect again with it."
            .to_string(),
        403 => said.unwrap_or_else(|| "The node refused remote control.".to_string()),
        _ => format!(
            "The node answered {status}{}",
            said.map(|said| format!(": {said}")).unwrap_or_default()
        ),
    }))
}

async fn fetch_view(remote: &RemoteNode) -> Result<ControlView, ClientError> {
    let bytes = call(remote, "/api/control", None).await?;
    serde_json::from_slice(&bytes).map_err(|err| {
        ClientError(format!(
            "The node at {} answered, but not as a Plumb Search node does: {err}",
            remote.url
        ))
    })
}

/// Whether a URL names an actual loopback address, without consulting DNS.
/// Only the exact `localhost` name is accepted; names below `.localhost` and
/// DNS names that happen to resolve locally do not get the HTTP exception.
fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain("localhost")) => true,
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    }
}

/// A node origin. Omitted schemes default to HTTPS off-host, HTTP for
/// loopback. Explicit HTTP is allowed only for loopback (for SSH tunnels).
fn parse_address(text: &str) -> Result<String> {
    let text = text.trim().trim_end_matches('/');
    if text.is_empty() {
        return Err(anyhow!(
            "Enter the node's HTTPS address, such as https://homelab.example:8080, \
             or a local tunnel address such as http://127.0.0.1:8080."
        ));
    }
    let with_scheme = if text.contains("://") {
        text.to_string()
    } else {
        format!("https://{text}")
    };
    let mut url = Url::parse(&with_scheme)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
        .filter(|url| url.username().is_empty() && url.password().is_none())
        .filter(|url| url.path() == "/" && url.query().is_none() && url.fragment().is_none())
        .ok_or_else(|| {
            anyhow!(
                "This is not a node address. Use an HTTPS origin such as \
                     https://homelab.example:8080, without credentials, a path, or a query."
            )
        })?;
    let loopback = is_loopback(&url);
    if !text.contains("://") && loopback {
        // Reparse so a user-supplied :443 is retained instead of being
        // discarded as HTTPS's default port before switching to HTTP.
        url = Url::parse(&format!("http://{text}")).expect("validated node origin");
    }
    if url.scheme() == "http" {
        if !loopback {
            return Err(anyhow!(
                "Remote control requires HTTPS outside this computer because the token \
                 grants control of the node. Use an HTTPS address with a valid certificate, \
                 or an SSH tunnel and connect to http://127.0.0.1:<local-port>. \
                 Private-network and VPN addresses also require HTTPS."
            ));
        }
        if url.host() == Some(Host::Domain("localhost")) {
            // Do not rely on a hosts file or DNS response to keep plaintext
            // localhost credentials local.
            url.set_host(Some("127.0.0.1")).expect("a valid IPv4 host");
        }
    }
    Ok(url.origin().ascii_serialization())
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct AddForm {
    name: String,
    address: String,
    token: String,
}

async fn new_page(State(state): State<AppState>, request: Request) -> Response {
    let node = match manager(&state, &request) {
        Ok(node) => node,
        Err(response) => return response,
    };
    panel_page(render_connect(node.as_ref(), &AddForm::default(), None))
}

fn render_connect(node: &dyn StatusSource, form: &AddForm, error: Option<&str>) -> String {
    let error = error
        .map(|error| {
            format!(
                "<div class=\"err\" role=\"alert\"><p>{}</p></div>",
                escape_html(error)
            )
        })
        .unwrap_or_default();
    let body = format!(
        "<main class=\"wrap node-panel\">{}<div class=\"node-heading\"><div>\
         <p class=\"eyebrow\">ANOTHER NODE</p><h1>Connect to a node</h1></div></div>\
         <p class=\"intro\">Control a Plumb Search node on another computer from here, such \
         as a Docker container or a homelab server: its settings, crawling and \
         features.</p>{error}\
         <ol class=\"howto\"><li>Turn on remote control on that node. In its panel, open \
         <strong>Remote control</strong>. For Docker, run <code>docker exec &lt;container&gt; \
         plumb remote-control on</code> on its host. Either way you get a \
         token.</li><li>Enter the node's address and the token here.</li></ol>\
         <form method=\"post\" action=\"/app/nodes\">\
         <label for=\"address\">Address</label>\
         <input id=\"address\" name=\"address\" required placeholder=\"https://homelab.example:8080\" \
         value=\"{}\" spellcheck=\"false\" autocomplete=\"off\">\
         <p class=\"hint\">Use HTTPS with a valid certificate, or an SSH tunnel at \
         http://127.0.0.1:&lt;local-port&gt;. HTTP is allowed only on this computer.</p>\
         <label for=\"token\">Remote control token</label>\
         <input id=\"token\" name=\"token\" type=\"password\" required \
         placeholder=\"{TOKEN_PREFIX}...\" spellcheck=\"false\" autocomplete=\"off\">\
         <label for=\"name\">Name <span class=\"state\">· optional</span></label>\
         <input id=\"name\" name=\"name\" placeholder=\"Homelab\" value=\"{}\">\
         <button type=\"submit\">Connect</button></form></main>",
        switcher(node, Some("new")),
        escape_html(&form.address),
        escape_html(&form.name),
    );
    page(&body, "Connect to a node")
}

fn page(body: &str, title: &str) -> String {
    let head = format!(
        "<meta name=\"referrer\" content=\"same-origin\">\
         <style>{PANEL_STYLE}</style><style>{LAYOUT_STYLE}{CONNECT_STYLE}</style>"
    );
    page_with_head(&format!("{title} - Plumb Search"), &head, body)
}

const CONNECT_STYLE: &str = ".node-panel form input:not([type=checkbox]):not([type=number]){display:block;width:100%;max-width:30rem;margin-top:.3rem;padding:.55rem .7rem;font:inherit;background:var(--bg);color:var(--fg);border:1px solid var(--line);border-radius:.5rem}";

async fn add(State(state): State<AppState>, request: Request) -> Response {
    let node = match manager(&state, &request) {
        Ok(node) => node,
        Err(response) => return response,
    };
    let Ok(Form(form)) = Form::<AddForm>::from_request(request, &state).await else {
        return panel_error(StatusCode::BAD_REQUEST, "The form could not be read.");
    };
    let again = |error: &str| {
        (
            StatusCode::BAD_REQUEST,
            panel_page(render_connect(node.as_ref(), &form, Some(error))),
        )
            .into_response()
    };
    let url = match parse_address(&form.address) {
        Ok(url) => url,
        Err(err) => return again(&err.to_string()),
    };
    let token = form.token.trim().to_string();
    if !token.starts_with(TOKEN_PREFIX) {
        return again(&format!(
            "A remote control token starts with \"{TOKEN_PREFIX}\". Copy the whole token from \
             the node."
        ));
    }
    let name = match form.name.trim() {
        "" => Url::parse(&url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| url.clone()),
        name => name.chars().take(60).collect(),
    };
    let mut id_bytes = [0u8; 6];
    if getrandom::fill(&mut id_bytes).is_err() {
        id_bytes = now_unix().to_le_bytes()[..6].try_into().unwrap();
    }
    let remote = RemoteNode {
        id: hex_encode(&id_bytes),
        name,
        url,
        token,
    };
    if let Err(ClientError(error)) = fetch_view(&remote).await {
        return again(&error);
    }
    let dir = node.data_dir().expect("checked by manager");
    let saved = load(&dir).and_then(|mut nodes| {
        // Connecting to the same node again replaces its token.
        nodes.retain(|other| other.url != remote.url);
        nodes.push(remote.clone());
        save(&dir, &nodes)
    });
    if let Err(err) = saved {
        return panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not save the node: {err:#}"),
        );
    }
    info!("connected to the node at {}", remote.url);
    Redirect::to(&format!("/app/nodes/{}", remote.id)).into_response()
}

/// The saved node `id`, or the page to answer with.
// A response is big, but these run once per request.
#[allow(clippy::result_large_err)]
fn remote_or_page(node: &dyn StatusSource, id: &str) -> Result<RemoteNode, Response> {
    match find(node, id) {
        Ok(Some(remote)) => Ok(remote),
        Ok(None) => Err(Redirect::to("/app").into_response()),
        Err(err) => Err(panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not read the saved nodes: {err:#}"),
        )),
    }
}

async fn show(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let node = match manager(&state, &request) {
        Ok(node) => node,
        Err(response) => return response,
    };
    let remote = match remote_or_page(node.as_ref(), &id) {
        Ok(remote) => remote,
        Err(response) => return response,
    };
    let query = axum::extract::Query::<PanelQuery>::try_from_uri(request.uri())
        .map(|query| query.0)
        .unwrap_or_default();
    let base = format!("/app/nodes/{}", remote.id);
    let switcher = switcher(node.as_ref(), Some(&remote.id));
    match fetch_view(&remote).await {
        Ok(view) => {
            let data_dir = view.data_dir.as_deref().map(Path::new);
            let eyebrow = remote.name.to_uppercase();
            panel_page(render_panel(&PanelView {
                status: &view.status,
                settings: &view.settings,
                origin: &remote.url,
                data_dir,
                now: now_unix(),
                query: &query,
                active: &view.features,
                saved: &view.saved_features,
                writable: true,
                private_ready: view.private_search_ready,
                base: &base,
                eyebrow: &eyebrow,
                switcher: &switcher,
                remote_control: None,
                activity: &view.activity,
                backups: None,
            }))
            .into_response()
        }
        Err(ClientError(error)) => (
            StatusCode::BAD_GATEWAY,
            panel_page(render_unreachable(&remote, &switcher, &error)),
        )
            .into_response(),
    }
}

fn render_unreachable(remote: &RemoteNode, switcher: &str, error: &str) -> String {
    let base = format!("/app/nodes/{}", remote.id);
    let body = format!(
        "<main class=\"wrap node-panel\">{switcher}<div class=\"node-heading\"><div>\
         <p class=\"eyebrow\">{}</p><h1>Plumb Search</h1></div></div>\
         <div class=\"err\" role=\"alert\"><p>{}</p></div>\
         <p>Address: <code>{}</code></p>\
         <div class=\"btns\"><a class=\"btn\" href=\"{base}\">Try again</a>\
         <a class=\"btn alt\" href=\"/app/nodes/new\">Connect with a new token</a></div>\
         <form method=\"post\" action=\"{base}/remove\">\
         <button type=\"submit\" class=\"alt\">Forget this node</button></form></main>",
        escape_html(&remote.name.to_uppercase()),
        escape_html(error),
        escape_html(&remote.url),
    );
    page(&body, &remote.name)
}

/// After a change to another node: back to its panel, or what went wrong.
fn after_change(remote: &RemoteNode, result: Result<Vec<u8>, ClientError>, back: &str) -> Response {
    match result {
        Ok(_) => Redirect::to(back).into_response(),
        Err(ClientError(error)) => panel_error(
            StatusCode::BAD_GATEWAY,
            &format!("{}: nothing was changed. {error}", remote.name),
        ),
    }
}

async fn change_settings(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let node = match manager(&state, &request) {
        Ok(node) => node,
        Err(response) => return response,
    };
    let remote = match remote_or_page(node.as_ref(), &id) {
        Ok(remote) => remote,
        Err(response) => return response,
    };
    let Ok(Form(form)) = Form::<SettingsForm>::from_request(request, &state).await else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "The settings form could not be read.",
        );
    };
    let current = match fetch_view(&remote).await {
        Ok(view) => view.settings,
        Err(error) => return after_change(&remote, Err(error), ""),
    };
    let settings = match settings_from_form(&form, &current) {
        Ok(settings) => settings,
        Err(response) => return response,
    };
    let body = serde_json::to_vec(&settings).expect("settings as JSON");
    let result = call(&remote, "/api/control/settings", Some(body)).await;
    let back = format!("/app/nodes/{}?section=resources&saved=settings", remote.id);
    after_change(&remote, result, &back)
}

async fn change_features(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let node = match manager(&state, &request) {
        Ok(node) => node,
        Err(response) => return response,
    };
    let remote = match remote_or_page(node.as_ref(), &id) {
        Ok(remote) => remote,
        Err(response) => return response,
    };
    let Ok(Form(form)) = Form::<FeaturesForm>::from_request(request, &state).await else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "The feature settings could not be read.",
        );
    };
    // The form holds one section's features; the rest stay as saved there.
    let mut features = match fetch_view(&remote).await {
        Ok(view) => view.saved_features,
        Err(error) => return after_change(&remote, Err(error), ""),
    };
    let section = match apply_features_form(&form, &mut features) {
        Ok(section) => section,
        Err(response) => return response,
    };
    let body = serde_json::to_vec(&features).expect("features as JSON");
    let result = call(&remote, "/api/control/features", Some(body)).await;
    let back = format!("/app/nodes/{}?section={section}&saved=features", remote.id);
    after_change(&remote, result, &back)
}

async fn refresh(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let node = match manager(&state, &request) {
        Ok(node) => node,
        Err(response) => return response,
    };
    let remote = match remote_or_page(node.as_ref(), &id) {
        Ok(remote) => remote,
        Err(response) => return response,
    };
    let result = call(&remote, "/api/control/refresh", Some(b"{}".to_vec())).await;
    after_change(&remote, result, &format!("/app/nodes/{}", remote.id))
}

async fn pause(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let node = match manager(&state, &request) {
        Ok(node) => node,
        Err(response) => return response,
    };
    let remote = match remote_or_page(node.as_ref(), &id) {
        Ok(remote) => remote,
        Err(response) => return response,
    };
    let Ok(Form(form)) = Form::<PauseForm>::from_request(request, &state).await else {
        return panel_error(StatusCode::BAD_REQUEST, "The pause form could not be read.");
    };
    let current = match fetch_view(&remote).await {
        Ok(view) => view.settings,
        Err(error) => return after_change(&remote, Err(error), ""),
    };
    let Some(settings) = paused(&current, &form, now_unix()) else {
        return panel_error(StatusCode::BAD_REQUEST, "The pause form could not be read.");
    };
    let body = serde_json::to_vec(&settings).expect("settings as JSON");
    let result = call(&remote, "/api/control/settings", Some(body)).await;
    after_change(&remote, result, &format!("/app/nodes/{}", remote.id))
}

async fn retry(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let node = match manager(&state, &request) {
        Ok(node) => node,
        Err(response) => return response,
    };
    let remote = match remote_or_page(node.as_ref(), &id) {
        Ok(remote) => remote,
        Err(response) => return response,
    };
    let Ok(Form(form)) = Form::<RetryForm>::from_request(request, &state).await else {
        return panel_error(StatusCode::BAD_REQUEST, "The retry form could not be read.");
    };
    let Some(what) = form.what() else {
        return panel_error(StatusCode::BAD_REQUEST, "The retry form could not be read.");
    };
    let body = serde_json::to_vec(&what).expect("a retry as JSON");
    let result = call(&remote, "/api/control/retry", Some(body)).await;
    after_change(&remote, result, &format!("/app/nodes/{}", remote.id))
}

async fn retry_network(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let node = match manager(&state, &request) {
        Ok(node) => node,
        Err(response) => return response,
    };
    let remote = match remote_or_page(node.as_ref(), &id) {
        Ok(remote) => remote,
        Err(response) => return response,
    };
    let result = call(&remote, "/api/control/reconnect", Some(b"{}".to_vec())).await;
    let back = format!("/app/nodes/{}?section=network&saved=retry", remote.id);
    after_change(&remote, result, &back)
}

async fn remove(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Response {
    let node = match manager(&state, &request) {
        Ok(node) => node,
        Err(response) => return response,
    };
    let dir = node.data_dir().expect("checked by manager");
    let removed = load(&dir).and_then(|mut nodes| {
        nodes.retain(|remote| remote.id != id);
        save(&dir, &nodes)
    });
    if let Err(err) = removed {
        return panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not forget the node: {err:#}"),
        );
    }
    Redirect::to("/app").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_node_addresses() {
        assert_eq!(
            parse_address("192.168.1.20:8080").unwrap(),
            "https://192.168.1.20:8080"
        );
        assert_eq!(
            parse_address(" https://homelab.local:8080/ ").unwrap(),
            "https://homelab.local:8080"
        );
        assert_eq!(
            parse_address("https://plumb.example").unwrap(),
            "https://plumb.example"
        );
        assert_eq!(
            parse_address("[fd00::5]:8080").unwrap(),
            "https://[fd00::5]:8080"
        );
        for (address, origin) in [
            ("localhost:8080", "http://127.0.0.1:8080"),
            ("http://LOCALHOST:8080", "http://127.0.0.1:8080"),
            ("localhost:443", "http://127.0.0.1:443"),
            ("127.0.0.1:443", "http://127.0.0.1:443"),
            ("http://127.0.0.2:8080", "http://127.0.0.2:8080"),
            ("http://2130706433:8080", "http://127.0.0.1:8080"),
            ("[::1]:8080", "http://[::1]:8080"),
            ("https://localhost:8080", "https://localhost:8080"),
            ("example.localhost:8080", "https://example.localhost:8080"),
        ] {
            assert_eq!(parse_address(address).unwrap(), origin, "{address}");
        }
        for bad in [
            "",
            "ftp://x",
            "http://user:pw@x:1",
            "http://x:1/app",
            "http://x:1/?q=1",
            "https://x/#fragment",
            "not an address",
        ] {
            assert!(parse_address(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn refuses_plaintext_even_on_private_networks_or_local_looking_names() {
        for address in [
            "http://192.168.1.20:8080",
            "http://homelab.local:8080",
            "http://10.0.0.1:8080",
            "http://172.17.0.1:8080",
            "http://100.100.1.2:8080",
            "http://[fd00::5]:8080",
            "http://[::ffff:127.0.0.1]:8080",
            "http://localhost.:8080",
            "http://example.localhost:8080",
            "http://localhost.example:8080",
            "http://0.0.0.0:8080",
            "http://[::]:8080",
            "http://example.com",
        ] {
            let error = parse_address(address).unwrap_err().to_string();
            assert!(error.contains("requires HTTPS"), "{address}: {error}");
            assert!(error.contains("SSH tunnel"), "{address}: {error}");
        }
    }

    #[test]
    fn keeps_the_saved_nodes_to_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_empty());
        let nodes = vec![RemoteNode {
            id: "a1".into(),
            name: "Homelab".into(),
            url: "https://192.168.1.20:8080".into(),
            token: "plumb_x".into(),
        }];
        save(dir.path(), &nodes).unwrap();
        assert_eq!(load(dir.path()).unwrap(), nodes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}

/// The remote control API and "Connect to a node" together: a desktop node
/// controlling a server node over a real connection.
#[cfg(test)]
mod end_to_end {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{header, Request as HttpRequest};
    use tower::ServiceExt;

    use super::*;
    use crate::node::control;
    use crate::node::features::FeatureSettings;
    use crate::node::{NodeSettings, Phase, Status, Step};
    use crate::web::{node_router, SearchBackend};

    struct FakeNode {
        dir: tempfile::TempDir,
        manages: bool,
        settings: Mutex<NodeSettings>,
        features: Mutex<FeatureSettings>,
        refreshes: Mutex<usize>,
    }

    impl FakeNode {
        fn new(manages: bool) -> Arc<Self> {
            Arc::new(FakeNode {
                dir: tempfile::tempdir().unwrap(),
                manages,
                settings: Mutex::new(NodeSettings::default()),
                features: Mutex::new(FeatureSettings::default()),
                refreshes: Mutex::new(0),
            })
        }
    }

    impl StatusSource for FakeNode {
        fn status(&self) -> Status {
            Status {
                phase: Phase::Ready,
                step: Step::Idle,
                detail: String::new(),
                progress: None,
                last_error: None,
                wikidata_missing: false,
                wikidata_error: None,
                sites: 123_456,
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
        fn settings(&self) -> Option<NodeSettings> {
            Some(self.settings.lock().unwrap().clone())
        }
        fn change_settings(&self, settings: NodeSettings) -> Result<()> {
            *self.settings.lock().unwrap() = settings;
            Ok(())
        }
        fn refresh_now(&self) {
            *self.refreshes.lock().unwrap() += 1;
        }
        fn saved_features(&self) -> Result<FeatureSettings> {
            Ok(self.features.lock().unwrap().clone())
        }
        fn change_features(&self, features: FeatureSettings) -> Result<()> {
            *self.features.lock().unwrap() = features;
            Ok(())
        }
        fn data_dir(&self) -> Option<PathBuf> {
            Some(self.dir.path().to_path_buf())
        }
        fn manages_other_nodes(&self) -> bool {
            self.manages
        }
    }

    struct NoSearch;
    impl SearchBackend for NoSearch {
        fn search(&self, _: &str, _: usize) -> Result<Vec<plumb_index::Hit>> {
            Ok(Vec::new())
        }
        fn num_docs(&self) -> u64 {
            0
        }
    }

    /// Serves `node` on a free local port; returns its origin.
    async fn serve(node: Arc<FakeNode>) -> String {
        let app = node_router(Arc::new(NoSearch), node);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        format!("http://{addr}")
    }

    /// A request to the control API of `node`, from `peer`.
    async fn control_request(
        node: Arc<FakeNode>,
        method: &str,
        path: &str,
        peer: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, String) {
        let app = node_router(Arc::new(NoSearch), node);
        let mut request = HttpRequest::builder().method(method).uri(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut request = request.body(Body::from(body.to_string())).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn the_control_api_is_off_until_turned_on_and_wants_the_token() {
        let node = FakeNode::new(false);
        let lan = "192.168.1.30:50000";
        let (status, _) = control_request(node.clone(), "GET", "/api/control", lan, &[], "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let token = control::turn_on(node.dir.path(), false).unwrap();
        let bearer = format!("Bearer {token}");
        let auth = [("authorization", bearer.as_str())];
        let (status, body) =
            control_request(node.clone(), "GET", "/api/control", lan, &auth, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let view: ControlView = serde_json::from_str(&body).unwrap();
        assert_eq!(view.status.sites, 123_456);

        for (peer, headers, want) in [
            (lan, vec![], StatusCode::UNAUTHORIZED),
            (
                lan,
                vec![("authorization", "Bearer plumb_wrong")],
                StatusCode::UNAUTHORIZED,
            ),
            (
                lan,
                vec![("authorization", token.as_str())],
                StatusCode::UNAUTHORIZED,
            ),
            // From the internet, straight or through a reverse proxy.
            ("203.0.113.9:50000", auth.to_vec(), StatusCode::FORBIDDEN),
            (
                "127.0.0.1:50000",
                vec![auth[0], ("x-forwarded-for", "203.0.113.9")],
                StatusCode::FORBIDDEN,
            ),
            (
                "127.0.0.1:50000",
                vec![auth[0], ("forwarded", "for=203.0.113.9")],
                StatusCode::FORBIDDEN,
            ),
            // From a web page.
            (
                lan,
                vec![auth[0], ("origin", "https://evil.example")],
                StatusCode::FORBIDDEN,
            ),
            // Docker's bridge, Tailscale and IPv6 local networks are fine.
            ("172.17.0.1:50000", auth.to_vec(), StatusCode::OK),
            ("100.100.1.2:50000", auth.to_vec(), StatusCode::OK),
            ("[fd00::2]:50000", auth.to_vec(), StatusCode::OK),
        ] {
            let (status, body) =
                control_request(node.clone(), "GET", "/api/control", peer, &headers, "").await;
            assert_eq!(status, want, "{peer} {headers:?}: {body}");
        }

        // Changes need the token too.
        let json = [auth[0], ("content-type", "application/json")];
        let settings =
            r#"{"background_updates":false,"download_limit_mb_per_day":7,"storage_limit_mb":9}"#;
        let (status, _) = control_request(
            node.clone(),
            "POST",
            "/api/control/settings",
            lan,
            &json[1..],
            settings,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(node.settings.lock().unwrap().background_updates);
        let (status, _) = control_request(
            node.clone(),
            "POST",
            "/api/control/settings",
            lan,
            &json,
            settings,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(node.settings.lock().unwrap().download_limit_mb_per_day, 7);
        let bad = r#"{"share_popularity":true}"#;
        let (status, body) = control_request(
            node.clone(),
            "POST",
            "/api/control/features",
            lan,
            &json,
            bad,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, _) =
            control_request(node.clone(), "POST", "/api/control/refresh", lan, &auth, "").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(*node.refreshes.lock().unwrap(), 1);

        // Public addresses, once allowed.
        let token = control::turn_on(node.dir.path(), true).unwrap();
        let bearer = format!("Bearer {token}");
        let (status, _) = control_request(
            node.clone(),
            "GET",
            "/api/control",
            "203.0.113.9:1",
            &[("authorization", &bearer)],
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        control::turn_off(node.dir.path()).unwrap();
        let (status, _) = control_request(
            node,
            "GET",
            "/api/control",
            lan,
            &[("authorization", &bearer)],
            "",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A request to the desktop node's panel, from its own window.
    async fn panel_request(
        app: Router,
        method: &str,
        path: &str,
        form: &str,
    ) -> (StatusCode, Option<String>, String) {
        let mut request = HttpRequest::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "127.0.0.1:7586");
        if method == "POST" {
            request = request
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::ORIGIN, "http://127.0.0.1:7586");
        }
        let mut request = request.body(Body::from(form.to_string())).unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:50000".parse::<SocketAddr>().unwrap(),
        ));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .map(|l| l.to_str().unwrap().to_string());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            location,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    fn form(pairs: &[(&str, &str)]) -> String {
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish()
    }

    #[tokio::test]
    async fn the_desktop_connects_to_a_node_and_changes_its_settings() {
        let server = FakeNode::new(false);
        let server_url = serve(server.clone()).await;
        let desktop = FakeNode::new(true);
        let app = node_router(Arc::new(NoSearch), desktop.clone());

        // The panel offers to connect.
        let (status, _, page) = panel_request(app.clone(), "GET", "/app", "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("This computer"), "{page}");
        assert!(page.contains("href=\"/app/nodes/new\""), "{page}");

        // Remote control is still off there.
        let address =
            server_url
                .trim_start_matches("http://")
                .replacen("127.0.0.1", "localhost", 1);
        let attempt = form(&[
            ("address", &address),
            ("token", "plumb_abc"),
            ("name", "Homelab"),
        ]);
        let (status, _, page) = panel_request(app.clone(), "POST", "/app/nodes", &attempt).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            page.contains("Remote control is off on that node"),
            "{page}"
        );

        let token = control::turn_on(server.dir.path(), false).unwrap();
        let (status, _, page) = panel_request(app.clone(), "POST", "/app/nodes", &attempt).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(page.contains("did not accept the token"), "{page}");

        let connect = form(&[
            ("address", &address),
            ("token", &token),
            ("name", "Homelab"),
        ]);
        let (status, location, page) =
            panel_request(app.clone(), "POST", "/app/nodes", &connect).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
        let panel = location.unwrap();
        assert!(panel.starts_with("/app/nodes/"));
        let saved = std::fs::read_to_string(desktop.dir.path().join(FILE_NAME)).unwrap();
        assert!(saved.contains(&token));

        // Its panel, with links to its own search page and forms through here.
        let (status, _, page) = panel_request(
            app.clone(),
            "GET",
            &format!("{panel}?section=resources"),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(page.contains("HOMELAB"), "{page}");
        assert!(page.contains(&format!("href=\"{server_url}/\"")), "{page}");
        assert!(
            page.contains(&format!("action=\"{panel}/settings\"")),
            "{page}"
        );
        assert!(!page.contains(&token), "the window never sees the token");
        assert!(!page.contains("section=remote"), "{page}");

        let settings = form(&[
            ("download_limit_mb_per_day", "42"),
            ("storage_limit_mb", ""),
        ]);
        let (status, location, _) =
            panel_request(app.clone(), "POST", &format!("{panel}/settings"), &settings).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(location.unwrap().starts_with(&panel));
        let changed = server.settings.lock().unwrap().clone();
        assert_eq!(changed.download_limit_mb_per_day, 42);
        assert!(!changed.background_updates);

        let features = form(&[("section", "search"), ("search_by_meaning", "1")]);
        let (status, _, _) =
            panel_request(app.clone(), "POST", &format!("{panel}/features"), &features).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(server.features.lock().unwrap().search_by_meaning);

        let (status, _, _) =
            panel_request(app.clone(), "POST", &format!("{panel}/refresh"), "").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(*server.refreshes.lock().unwrap(), 1);

        // A new token there: the panel says what to do.
        control::turn_on(server.dir.path(), false).unwrap();
        let (status, _, page) = panel_request(app.clone(), "GET", &panel, "").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(page.contains("did not accept the token"), "{page}");
        assert!(page.contains("Forget this node"), "{page}");

        let (status, _, _) =
            panel_request(app.clone(), "POST", &format!("{panel}/remove"), "").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(load(desktop.dir.path()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn old_plaintext_entries_are_blocked_before_any_control_request() {
        // localhost. may resolve to loopback, but is intentionally outside the
        // literal-loopback policy. A listener catches any accidental send.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let desktop = FakeNode::new(true);
        let remote = RemoteNode {
            id: "old".into(),
            name: "Old plaintext connection".into(),
            url: format!("http://localhost.:{port}"),
            token: "plumb_secret_from_old_version".into(),
        };
        save(desktop.dir.path(), std::slice::from_ref(&remote)).unwrap();
        let app = node_router(Arc::new(NoSearch), desktop.clone());

        for (method, path) in [
            ("GET", "/app/nodes/old"),
            ("POST", "/app/nodes/old/refresh"),
        ] {
            let (status, _, page) = panel_request(app.clone(), method, path, "").await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{page}");
            assert!(page.contains("requires HTTPS"), "{page}");
            assert!(page.contains("SSH tunnel"), "{page}");
            assert!(!page.contains(&remote.token));
        }

        // Adding the same insecure address cannot send a validation request,
        // either, nor does a failed connection replace the saved entry.
        let attempt = form(&[("address", &remote.url), ("token", &remote.token)]);
        let (status, _, page) = panel_request(app, "POST", "/app/nodes", &attempt).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
        assert!(page.contains("requires HTTPS"), "{page}");
        assert_eq!(load(desktop.dir.path()).unwrap(), vec![remote]);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "refused plaintext requests must not open a connection"
        );
    }

    #[tokio::test]
    async fn saved_lan_http_and_malformed_origins_cannot_reach_the_bearer_client() {
        for address in [
            "http://192.168.1.20:8080",
            "http://homelab.local:8080",
            "http://[fd00::5]:8080",
            "http://user:password@127.0.0.1:8080",
            "http://127.0.0.1:8080/api/control?token=secret",
        ] {
            let remote = RemoteNode {
                id: "old".into(),
                name: "Old connection".into(),
                url: address.into(),
                token: "plumb_secret".into(),
            };
            for body in [None, Some(b"{}".to_vec())] {
                let ClientError(error) = call(&remote, "/api/control", body).await.unwrap_err();
                assert!(
                    error.contains("Could not connect to this saved node"),
                    "{error}"
                );
                assert!(!error.contains("password"), "{error}");
                assert!(!error.contains("secret"), "{error}");
            }
        }
    }

    #[tokio::test]
    async fn only_this_computer_can_use_other_nodes() {
        let desktop = FakeNode::new(true);
        let app = node_router(Arc::new(NoSearch), desktop);
        let mut request = HttpRequest::get("/app/nodes/new")
            .header(header::HOST, "192.168.1.5:7586")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "192.168.1.9:50000".parse::<SocketAddr>().unwrap(),
        ));
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        // A server node has no other nodes.
        let server = FakeNode::new(false);
        let app = node_router(Arc::new(NoSearch), server);
        let (status, _, page) = panel_request(app.clone(), "GET", "/app/nodes/new", "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, _, page2) = panel_request(app, "GET", "/app", "").await;
        assert!(!page2.contains("Connect to a node"), "{page}{page2}");
    }

    #[tokio::test]
    async fn the_panel_turns_remote_control_on_and_shows_the_token_once() {
        let node = FakeNode::new(false);
        let app = node_router(Arc::new(NoSearch), node.clone());
        let (_, _, page) = panel_request(app.clone(), "GET", "/app?section=remote", "").await;
        assert!(page.contains("Turn on and make a token"), "{page}");
        let (status, _, page) =
            panel_request(app.clone(), "POST", "/app/remote-control", "action=on").await;
        assert_eq!(status, StatusCode::OK);
        let start = page.find(TOKEN_PREFIX).unwrap();
        let token = &page[start..start + TOKEN_PREFIX.len() + 64];
        assert!(control::load(node.dir.path())
            .unwrap()
            .unwrap()
            .accepts(token));
        let (_, _, page) = panel_request(app.clone(), "GET", "/app?section=remote", "").await;
        assert!(!page.contains(token));
        assert!(page.contains("Turn remote control off"), "{page}");
        let (status, _, _) = panel_request(app, "POST", "/app/remote-control", "action=off").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(control::load(node.dir.path()).unwrap().is_none());
    }
}
