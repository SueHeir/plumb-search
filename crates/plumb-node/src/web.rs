//! `plumb serve`: a small web front end and JSON API.
//!
//! - `GET /` shows a search box,
//! - `GET /search?q=` shows results as server-rendered HTML,
//! - `GET /api/search?q=&limit=` returns a JSON list of [`Hit`]s, or with
//!   `full=1` a [`SearchResults`] object that also holds the site search link,
//!
//! Both searches take `country=XX` (a two-letter code, or `any` for none)
//! and `only=1` (leave out other countries' sites). Without `country`, the
//! server's [`HomeCountry`] setting decides, by default from the browser's
//! `Accept-Language` and then this computer's region settings.
//! - `GET /opensearch.xml` describes the search engine to browsers
//!   (OpenSearch 1.1), so that they can offer to add it; every page links to
//!   it.
//!
//! A long-running node (`plumb run`, see [`crate::node`]) serves the same
//! pages through [`node_router`], plus `GET /api/status`, which returns the
//! node's [`Status`] as JSON, and the node's panel at `/app` (see
//! [`panel`]), which the desktop app shows in its window. Until its first
//! index is ready, `/` and `/search` show the setup step, its progress and the last error instead,
//! reloading every few seconds with a `<meta http-equiv="refresh">` (no
//! script), and `/api/search` answers 503.
//!
//! Titles, descriptions and URLs in the index come from the open web, so
//! every piece of record text is HTML-escaped, only `http`/`https` URLs
//! become links, and pages are served with a Content-Security-Policy that
//! allows no scripts and no external resources. Searches run on Tokio's
//! blocking thread pool.

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use plumb_core::{collapse_whitespace, now_unix, truncate_chars};
use plumb_index::{Hit, RankConfig, SearchOptions, SearchResults, Searcher, SiteSearch};
use serde::Deserialize;
use tracing::{debug, error, info};
use url::Url;

use crate::cli::ServeArgs;
use crate::country::{country_name, HomeCountry, COUNTRY_CHOICES};
use crate::node::{NodeSettings, Phase, Status, Step};

mod panel;

use crate::{block_on, rank_config};
pub use panel::ADD_TO_FIREFOX_PATH;

/// Results returned when a request does not say how many.
pub const DEFAULT_LIMIT: usize = 10;
/// Most results one request can get.
pub const MAX_LIMIT: usize = 100;
/// Longer queries are cut to this many characters.
pub const MAX_QUERY_CHARS: usize = 200;

/// No scripts, no external resources, forms only to this server. Inline
/// styles are allowed for the page's own `<style>` element.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; \
     form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

/// Seconds between two reloads of the setup page, and the `Retry-After` of
/// a search asked for before the index is ready.
const SETUP_RELOAD_SECONDS: u32 = 5;

/// The media type of an OpenSearch description.
const OPENSEARCH_TYPE: &str = "application/opensearchdescription+xml";

/// In the `<head>` of every page, so that browsers offer to add Plumb as a
/// search engine.
const OPENSEARCH_LINK: &str = "<link rel=\"search\" \
     type=\"application/opensearchdescription+xml\" title=\"Plumb Search\" \
     href=\"/opensearch.xml\">\n";

/// Answers queries for the web handlers. [`IndexBackend`] is the real one;
/// tests can plug in their own.
pub trait SearchBackend: Send + Sync {
    /// Best `limit` hits for `query`, best first.
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>>;
    /// [`SearchBackend::search`] with the searcher's choices, plus a site
    /// search link. By default the choices are ignored and there is no link.
    fn search_full(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        let _ = options;
        Ok(SearchResults {
            hits: self.search(query, limit)?,
            site_search: None,
        })
    }
    /// Number of sites that can be found.
    fn num_docs(&self) -> u64;
}

/// A [`Searcher`] with fixed ranking settings.
pub struct IndexBackend {
    searcher: Searcher,
    rank: RankConfig,
}

impl IndexBackend {
    pub fn new(searcher: Searcher, rank: RankConfig) -> Self {
        IndexBackend { searcher, rank }
    }
}

impl SearchBackend for IndexBackend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.searcher.search_with(query, limit, &self.rank)
    }

    fn search_full(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        self.searcher.search_full(query, limit, &self.rank, options)
    }

    fn num_docs(&self) -> u64 {
        self.searcher.num_docs()
    }
}

/// What a long-running node tells its web pages about itself, and what its
/// panel (`/app`) can change.
pub trait StatusSource: Send + Sync {
    /// The node's status, as `GET /api/status` returns it.
    fn status(&self) -> Status;

    /// The node's settings; `None` when it has none.
    fn settings(&self) -> Option<NodeSettings> {
        None
    }

    /// Saves new settings and puts them in force.
    fn change_settings(&self, _settings: NodeSettings) -> Result<()> {
        anyhow::bail!("this node has no settings")
    }

    /// Starts a refresh now.
    fn refresh_now(&self) {}

    /// Where the node keeps its data, to show on the panel.
    fn data_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
}

#[derive(Clone)]
struct AppState {
    backend: Arc<dyn SearchBackend>,
    /// Set for a long-running node, `None` for `plumb serve`.
    node: Option<Arc<dyn StatusSource>>,
    /// The home country of searches that do not name one.
    home: HomeCountry,
}

impl AppState {
    /// The node's status while it is still setting up; `None` once it is
    /// ready, and always for `plumb serve`.
    fn setting_up(&self) -> Option<Status> {
        let status = self.node.as_ref()?.status();
        (status.phase != Phase::Ready).then_some(status)
    }
}

/// The web app: `/`, `/search` and `/api/search`.
pub fn router(backend: Arc<dyn SearchBackend>) -> Router {
    router_with(backend, HomeCountry::Auto)
}

/// [`router`] with a [`HomeCountry`] setting.
pub fn router_with(backend: Arc<dyn SearchBackend>, home: HomeCountry) -> Router {
    app(AppState {
        backend,
        node: None,
        home,
    })
}

/// The web app of a long-running node: what [`router`] serves, plus
/// `GET /api/status`. Until `status` reports [`Phase::Ready`], `/` and
/// `/search` show the setup page and `/api/search` answers 503.
pub fn node_router(backend: Arc<dyn SearchBackend>, status: Arc<dyn StatusSource>) -> Router {
    node_router_with(backend, status, HomeCountry::Auto)
}

/// [`node_router`] with a [`HomeCountry`] setting.
pub fn node_router_with(
    backend: Arc<dyn SearchBackend>,
    status: Arc<dyn StatusSource>,
    home: HomeCountry,
) -> Router {
    app(AppState {
        backend,
        node: Some(status),
        home,
    })
}

fn app(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/", get(home))
        .route("/search", get(search_page))
        .route("/api/search", get(api_search))
        .route("/opensearch.xml", get(opensearch));
    if state.node.is_some() {
        router = router
            .route("/api/status", get(api_status))
            .route("/app", get(panel::panel))
            .route("/app/settings", post(panel::save_settings))
            .route("/app/refresh", post(panel::refresh))
            .route(panel::ADD_TO_FIREFOX_PATH, get(panel::add_to_firefox));
    }
    router.with_state(state)
}

/// Opens the index and serves it until Ctrl-C or SIGTERM.
pub fn run(args: ServeArgs) -> Result<()> {
    let searcher = Searcher::open(&args.index)
        .with_context(|| format!("opening the index in {}", args.index.display()))?;
    let docs = searcher.num_docs();
    let app = router_with(
        Arc::new(IndexBackend::new(searcher, rank_config(args.alpha))),
        args.country.clone(),
    );
    block_on(async move {
        let listener = tokio::net::TcpListener::bind(args.bind)
            .await
            .with_context(|| format!("listening on {}", args.bind))?;
        let addr = listener.local_addr().context("reading the bound address")?;
        info!("serving {docs} sites on http://{addr}/ (Ctrl-C to stop)");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .context("serving HTTP")
    })?
}

/// Resolves on Ctrl-C, or on SIGTERM (which `docker stop` and systemd send).
pub(crate) async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    info!("shutting down");
}

#[derive(Debug, Default, Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: String,
    limit: Option<usize>,
    /// A two-letter code, `any` for no home country, or empty for the default.
    country: Option<String>,
    /// `1` (or `on`, `true`): only the home country's sites and global ones.
    only: Option<String>,
    /// `1`: `/api/search` answers with a [`SearchResults`] object.
    full: Option<String>,
}

/// Whether a flag parameter is set: `1`, `on`, `true` or `yes`.
fn flag(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "on" | "true" | "yes"
        )
    })
}

impl SearchParams {
    /// The query with whitespace collapsed, cut to [`MAX_QUERY_CHARS`].
    fn query(&self) -> String {
        truncate_chars(&collapse_whitespace(&self.q), MAX_QUERY_CHARS)
    }

    fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
    }

    /// The searcher's choices: the `country` parameter when it is valid,
    /// else the server's setting for a request with these headers.
    fn options(&self, home: &HomeCountry, headers: &HeaderMap) -> SearchOptions {
        let asked = self
            .country
            .as_deref()
            .filter(|c| !c.trim().is_empty())
            .and_then(|c| HomeCountry::parse(c).ok());
        let accept_language = headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|value| value.to_str().ok());
        let country = match asked {
            Some(HomeCountry::Auto) | None => home.resolve(accept_language),
            Some(asked) => asked.resolve(accept_language),
        };
        SearchOptions {
            only_country: flag(&self.only) && country.is_some(),
            country,
        }
    }
}

async fn home(State(state): State<AppState>) -> Response {
    home_or_setup(&state)
}

/// The home page, or the setup page while a node is still setting up.
fn home_or_setup(state: &AppState) -> Response {
    let status = state.node.as_ref().map(|node| node.status());
    let now = now_unix();
    match &status {
        Some(status) if status.phase != Phase::Ready => setup_response(status, now),
        _ => html_response(
            StatusCode::OK,
            render_home(state.backend.num_docs(), status.as_ref(), now),
        ),
    }
}

fn setup_response(status: &Status, now: u64) -> Response {
    (
        StatusCode::OK,
        security_headers(),
        [(header::CACHE_CONTROL, "no-store")],
        Html(render_setup(status, now)),
    )
        .into_response()
}

async fn search_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    if let Some(status) = state.setting_up() {
        // Reloading keeps the query, so the results show up once the index is ready.
        return setup_response(&status, now_unix());
    }
    let query = params.query();
    if query.is_empty() {
        return home_or_setup(&state);
    }
    let options = params.options(&state.home, &headers);
    match run_search(&state, &query, params.limit(), &options).await {
        Ok(results) => html_response(StatusCode::OK, render_results(&query, &results, &options)),
        Err(err) => {
            error!("search for {query:?} failed: {err:#}");
            html_response(StatusCode::INTERNAL_SERVER_ERROR, render_error(&query))
        }
    }
}

async fn api_search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    if let Some(status) = state.setting_up() {
        let body = serde_json::json!({
            "error": "the search index is not ready yet",
            "phase": status.phase,
            "step": status.step,
        });
        let retry_after = SETUP_RELOAD_SECONDS.to_string();
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            security_headers(),
            [(header::RETRY_AFTER, retry_after)],
            Json(body),
        )
            .into_response();
    }
    let query = params.query();
    let full = flag(&params.full);
    if query.is_empty() {
        return if full {
            (
                StatusCode::OK,
                security_headers(),
                Json(SearchResults::default()),
            )
                .into_response()
        } else {
            (StatusCode::OK, security_headers(), Json(Vec::<Hit>::new())).into_response()
        };
    }
    let options = params.options(&state.home, &headers);
    match run_search(&state, &query, params.limit(), &options).await {
        Ok(results) if full => (StatusCode::OK, security_headers(), Json(results)).into_response(),
        Ok(results) => (StatusCode::OK, security_headers(), Json(results.hits)).into_response(),
        Err(err) => {
            error!("search for {query:?} failed: {err:#}");
            let body = serde_json::json!({ "error": "search failed" });
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                security_headers(),
                Json(body),
            )
                .into_response()
        }
    }
}

/// `GET /api/status`, routed for nodes only.
async fn api_status(State(state): State<AppState>) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        StatusCode::OK,
        security_headers(),
        [(header::CACHE_CONTROL, "no-store")],
        Json(node.status()),
    )
        .into_response()
}

/// `GET /opensearch.xml`: the OpenSearch description browsers add Plumb
/// from. Its search URL is on the host the request was sent to, so it is
/// right however the server is reached: `127.0.0.1:8080`, the desktop app's
/// port or a server's name on the LAN.
async fn opensearch(headers: HeaderMap, uri: Uri) -> Response {
    let Some(origin) = request_origin(&headers, &uri) else {
        return (
            StatusCode::BAD_REQUEST,
            security_headers(),
            "The Host header is missing or is not a host name and port.\n",
        )
            .into_response();
    };
    (
        StatusCode::OK,
        security_headers(),
        [(header::CONTENT_TYPE, OPENSEARCH_TYPE)],
        render_opensearch(&origin),
    )
        .into_response()
}

/// The origin a request was sent to, such as `http://127.0.0.1:7586`: the
/// `Host` header (or, without one, the request's authority, as in HTTP/2),
/// with `https` when a proxy in front says so in `X-Forwarded-Proto`. `None`
/// unless the host is a plain host name or address with an optional port.
fn request_origin(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    let host = match headers.get(header::HOST) {
        Some(host) => host.to_str().ok()?,
        None => uri.authority()?.as_str(),
    };
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|proto| proto.to_str().ok())
        .and_then(|proto| proto.split(',').next())
        .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"));
    let scheme = if https { "https" } else { "http" };
    let url = Url::parse(&format!("{scheme}://{host}/")).ok()?;
    let plain = url.username().is_empty()
        && url.password().is_none()
        && url.host().is_some()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none();
    plain.then(|| url.origin().ascii_serialization())
}

/// The OpenSearch 1.1 description of the search engine at `origin`.
fn render_opensearch(origin: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <OpenSearchDescription xmlns=\"http://a9.com/-/spec/opensearch/1.1/\">\n\
         <ShortName>Plumb Search</ShortName>\n\
         <Description>Find a site by its name.</Description>\n\
         <InputEncoding>UTF-8</InputEncoding>\n\
         <Image width=\"32\" height=\"32\" type=\"image/png\">\
         data:image/png;base64,{ICON_PNG_BASE64}</Image>\n\
         <Url type=\"text/html\" method=\"get\" template=\"{}/search?q={{searchTerms}}\"/>\n\
         </OpenSearchDescription>\n",
        escape_html(origin)
    )
}

/// The desktop app's icon (`crates/plumb-desktop/icons/32x32.png`, cut to
/// 255 colors), for the OpenSearch description.
const ICON_PNG_BASE64: &str = "\
iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAMAAABEpIrGAAABjFBMVEX///8sWZ4oVpw3Y6Tk6fIqV5suX6osWqEpVpk4\
ZKbo7vcoVJUmU5g2YKDj6fEmUZIoVZg1XpwjTIrj6O/m8f9YZXjYyLDYyK8oUY4mUJAZSZMqToa1dBeybgskTYsYRoxJ\
Z429sYTaq03YpkjapUW4oWhHYoZoe4z/3YL/5IT846T90Gj5zGT9yVbzvEtkb3ccSIz536H31YP0xFn0wVLvvFDyuUTw\
tT5HXn0iSochSYUXRIj/4nv325L63pz50nrttUT4tzixklA5WYP913bxt0HusTvrrjjhpzg2UnseRH0WQYNWa4LmqTXp\
qzVPX3EgRoH2y2nipDPkoizanTAdRYEdQnu/qG7/0F/poymphEMhRXsSPoFOYnjeoDHhnSlLWm0cQnoOOoCWiWX/xkmG\
c0/BjjUcQHYQO31YYmhRWWMXOm4OOXqehU/3tjbnoCeSd0cYOm0aPXIVO3QnRXDUnTjenSzJkDANN3hfYFzhnCdbXFkZ\
PHAZPHENN3aogTynfzsxR2kxSGkXOW3LNf1CAAAAAXRSTlMAQObYZgAAAeZJREFUOMutk+tf0lAYxyWVTY9yxEshKnir\
CJFQpiTWkDSnTuYEd0pnmHmZilaAmhleyn/c7dwge+vv1fN8f9+dz3lxVlf3SHG5nuDUNzTUk8nlqqkb3QKJ2NTc3CTS\
xd1Y7QEQcVpaPZ7WFjIDwA03FMX/BVGEbtK3CYALXo/HywUgtGGh3T6NBno7OryQr2I7EQBHUOjsFKob4AINgF1Pn3X5\
IN+pALsp8Pf09vX19vip0g2ZEMDxBfsHBoeGh5+/8PsIYQLrX4ZehUMjkdHoaz9BgX+EWH8oNBYej0sTk4k3sVphKpB0\
lumBt+/CckqSojPp90GHJANTTLATmx2fk2U5IkU/zCsLizGHUWHJmZPq8lxKHktJ0cmEktFWghguUUEnwmoK9/NaNrcy\
bSO9KthRZ1cjcdwrmezaouEwJujIsHf146c4vkAmu76hmrphIJ0KBrJjbn4epX1+64tqOswgwjbCMb/u7M4kFC2bX9/b\
NAnbrhVsZTCtaJl8bt9khAmWs1jmwWFasy+4dXRgEkKFArJojk++5de+/zhmOypgoYhKJFb59Cx3/vO0bFGAiuRRMqFk\
XfzKXf6+YH0J0VddQVcMXd/cXrP5ClXYj1Hh35T//C3z83jv3KNw9yCF4mP91vcYNadv9VISrAAAAABJRU5ErkJggg==";

/// Runs a search on the blocking thread pool, since searching is CPU and
/// disk work. A panicking backend becomes an error, not a dropped connection.
async fn run_search(
    state: &AppState,
    query: &str,
    limit: usize,
    options: &SearchOptions,
) -> Result<SearchResults> {
    if limit == 0 {
        return Ok(SearchResults::default());
    }
    let backend = Arc::clone(&state.backend);
    let owned_query = query.to_string();
    let owned_options = options.clone();
    let results = tokio::task::spawn_blocking(move || {
        backend.search_full(&owned_query, limit, &owned_options)
    })
    .await
    .context("the search task failed")??;
    debug!("{query:?}: {} hits", results.hits.len());
    Ok(results)
}

fn security_headers() -> [(HeaderName, &'static str); 3] {
    [
        (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ]
}

fn html_response(status: StatusCode, page: String) -> Response {
    (status, security_headers(), Html(page)).into_response()
}

/// Escapes text for HTML element content and quoted attribute values.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Where a hit links to: its URL when that is `http` or `https`, else its
/// domain's homepage, else nothing.
fn safe_href(hit: &Hit) -> Option<String> {
    http_url(&hit.url).or_else(|| homepage_url(&hit.domain))
}

/// `raw` re-serialized, when it is an absolute `http` or `https` URL with a host.
fn http_url(raw: &str) -> Option<String> {
    let url = Url::parse(raw.trim()).ok()?;
    let ok = matches!(url.scheme(), "http" | "https") && url.host_str().is_some();
    ok.then(|| url.to_string())
}

/// `https://<domain>/`, when `domain` is a plain hostname and nothing more.
fn homepage_url(domain: &str) -> Option<String> {
    let url = Url::parse(&format!("https://{domain}/")).ok()?;
    let plain = url.host_str() == Some(domain) && url.port().is_none() && url.path() == "/";
    plain.then(|| url.to_string())
}

/// `12345` -> `12,345`.
pub(crate) fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A rough length of time in words: `90` -> `1 minute`, `7200` -> `2 hours`.
pub(crate) fn duration_words(seconds: u64) -> String {
    let (n, unit) = match seconds {
        0..=59 => (seconds, "second"),
        60..=3_599 => (seconds / 60, "minute"),
        3_600..=86_399 => (seconds / 3_600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// `at` (Unix seconds) seen from `now`: `just now`, `5 minutes ago`.
fn time_ago(at: u64, now: u64) -> String {
    match now.saturating_sub(at) {
        0..=9 => "just now".to_string(),
        age => format!("{} ago", duration_words(age)),
    }
}

/// `at` (Unix seconds) seen from `now`: `in 10 minutes`, `any moment now`.
fn time_until(at: u64, now: u64) -> String {
    match at.saturating_sub(now) {
        0 => "any moment now".to_string(),
        wait => format!("in {}", duration_words(wait)),
    }
}

const STYLE: &str = "\
:root{color-scheme:light dark;--bg:#fff;--fg:#202124;--muted:#5f6368;--link:#1a0dab;\
--url:#0d652d;--line:#dadce0;--accent:#1a73e8;--err:#b3261e}\
@media (prefers-color-scheme:dark){:root{--bg:#1f1f1f;--fg:#e8eaed;--muted:#9aa0a6;\
--link:#8ab4f8;--url:#81c995;--line:#3c4043;--accent:#8ab4f8;--err:#f2b8b5}}\
*{box-sizing:border-box}\
body{margin:0;background:var(--bg);color:var(--fg);\
font:16px/1.5 system-ui,-apple-system,\"Segoe UI\",Roboto,sans-serif}\
.wrap{max-width:44rem;margin:0 auto;padding:1rem}\
.home{padding-top:18vh;text-align:center}\
.home form{margin:1.5rem auto 0;max-width:36rem}\
h1{margin:0;font-size:2.5rem;letter-spacing:-.02em}\
header{display:flex;flex-wrap:wrap;align-items:center;gap:.75rem;\
padding-bottom:.75rem;border-bottom:1px solid var(--line)}\
.logo{font-weight:700;font-size:1.25rem;color:var(--fg);text-decoration:none}\
form{display:flex;gap:.5rem;flex:1;min-width:14rem}\
input{flex:1;min-width:0;font:inherit;padding:.55rem .8rem;border:1px solid var(--line);\
border-radius:.5rem;background:var(--bg);color:var(--fg)}\
button{font:inherit;padding:.55rem 1rem;border:0;border-radius:.5rem;\
background:var(--accent);color:var(--bg);cursor:pointer}\
ol{list-style:none;margin:0;padding:0}\
li{padding:.9rem 0;border-bottom:1px solid var(--line)}\
.t{font-size:1.15rem;color:var(--link);text-decoration:none;overflow-wrap:anywhere}\
a.t:hover{text-decoration:underline}\
.u{color:var(--url);font-size:.875rem;overflow-wrap:anywhere}\
.d{margin:.25rem 0 0;overflow-wrap:anywhere}\
.tag,.m,.s{color:var(--muted)}\
.m,.s{font-size:.8rem}\
.m{margin-top:.25rem}\
.s{margin-top:1.5rem}\
.none{margin:1.5rem 0}\
header form{flex-wrap:wrap}\
.f{flex-basis:100%;display:flex;flex-wrap:wrap;gap:.5rem 1rem;align-items:center;\
font-size:.85rem;color:var(--muted)}\
.f input{flex:none}\
select{font:inherit;padding:.15rem .3rem;border:1px solid var(--line);border-radius:.35rem;\
background:var(--bg);color:var(--fg)}\
.ss{margin:1rem 0 .25rem;padding:.6rem .8rem;border:1px solid var(--line);border-radius:.5rem}\
.ss a{color:var(--link)}\
.setup{max-width:36rem}\
.step{margin:2rem 0 .5rem;font-size:1.1rem}\
progress{width:100%;height:.75rem;accent-color:var(--accent)}\
.err{margin-top:1.5rem;padding:.25rem 1rem;border:1px solid var(--err);border-radius:.5rem;\
text-align:left}\
.err strong{color:var(--err)}\
.msg{white-space:pre-wrap;overflow-wrap:anywhere;font:.85rem/1.4 ui-monospace,monospace}";

/// A whole HTML document; `body` must already be escaped.
fn page(title: &str, body: &str) -> String {
    page_with_head(title, "", body)
}

/// [`page`] with more elements in its `<head>`, which must be safe HTML.
fn page_with_head(title: &str, head: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n{OPENSEARCH_LINK}{head}\
         <title>{}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n{body}\n</body>\n</html>\n",
        escape_html(title)
    )
}

fn search_form(query: &str, autofocus: bool) -> String {
    format!(
        "<form action=\"/search\" method=\"get\" role=\"search\">\
         <input type=\"search\" name=\"q\" value=\"{}\" placeholder=\"A site's name, e.g. us bank\" \
         aria-label=\"Search\" autocomplete=\"off\"{}>\
         <button type=\"submit\">Search</button></form>",
        escape_html(query),
        if autofocus { " autofocus" } else { "" }
    )
}

/// The home page; a node's `status` adds what it is doing to the count of sites.
fn render_home(docs: u64, status: Option<&Status>, now: u64) -> String {
    let note = status
        .and_then(|status| node_note(status, now))
        .map(|note| format!(" &middot; {}", escape_html(&note)))
        .unwrap_or_default();
    let wikidata = status
        .and_then(|status| wikidata_note(status, now))
        .map(|note| format!("\n<p class=\"s\">{}</p>", escape_html(&note)))
        .unwrap_or_default();
    let body = format!(
        "<main class=\"wrap home\">\n<h1>Plumb</h1>\n<p class=\"tag\">Find a site by its name.</p>\n\
         {}\n<p class=\"s\">{} sites indexed{note}</p>{wikidata}\n</main>",
        search_form("", true),
        group_thousands(docs)
    );
    page("Plumb Search", &body)
}

/// What a ready node is up to, in a few words for the home page.
fn node_note(status: &Status, now: u64) -> Option<String> {
    let note = match (status.step, &status.progress) {
        (Step::Crawling, Some(p)) => format!(
            "crawling homepages, {} of {}",
            group_thousands(p.done),
            group_thousands(p.total)
        ),
        (Step::Indexing, _) => "rebuilding the index".to_string(),
        _ if status.last_error.is_some() => {
            "the last update failed and will be tried again (see /api/status)".to_string()
        }
        _ => format!("updated {}", time_ago(status.last_refresh?, now)),
    };
    Some(note)
}

/// Says that the index lacks Wikidata's official websites, while it does.
fn wikidata_note(status: &Status, now: u64) -> Option<String> {
    if !status.wikidata_missing {
        return None;
    }
    let Some(err) = &status.wikidata_error else {
        // Right after the quick first setup, Wikidata is next.
        if status.phase == Phase::SettingUp {
            return None;
        }
        return Some(
            "Plumb is still downloading Wikidata's list of official websites and more \
             rankings. Search works now, and results get better once those are in."
                .to_string(),
        );
    };
    let mut note = "Wikidata's list of official websites could not be downloaded yet, so the \
                    index does without it for now: official sites get no boost over look-alikes."
        .to_string();
    if let Some(retry_at) = err.retry_at {
        let _ = write!(note, " Plumb will try again {}.", time_until(retry_at, now));
    }
    Some(note)
}

/// The page shown while a node sets up: the step, its progress and the last
/// error. It reloads itself, since the page allows no script.
fn render_setup(status: &Status, now: u64) -> String {
    let mut body = String::from(
        "<main class=\"wrap home setup\">\n<h1>Plumb</h1>\n\
         <p class=\"tag\">Setting up your search engine</p>\n",
    );
    let _ = writeln!(
        body,
        "<p class=\"step\">{}</p>",
        escape_html(&status.detail)
    );
    if let Some(progress) = &status.progress {
        let max = progress.total.max(progress.done).max(1);
        let _ = writeln!(
            body,
            "<progress value=\"{}\" max=\"{max}\"></progress>\n<p>{} of {} {}</p>",
            progress.done,
            group_thousands(progress.done),
            group_thousands(progress.total),
            escape_html(&progress.unit)
        );
    }
    if let Some(err) = &status.last_error {
        let _ = write!(
            body,
            "<div class=\"err\" role=\"alert\">\n<p><strong>Something went wrong</strong> \
             {}:</p>\n<p class=\"msg\">{}</p>\n",
            time_ago(err.at, now),
            escape_html(&err.message)
        );
        if let Some(retry_at) = err.retry_at {
            let _ = writeln!(
                body,
                "<p>Plumb will try again {}.</p>",
                time_until(retry_at, now)
            );
        }
        body.push_str("</div>\n");
    }
    if let Some(note) = wikidata_note(status, now) {
        let _ = writeln!(body, "<p>{}</p>", escape_html(&note));
        if let Some(err) = &status.wikidata_error {
            let _ = writeln!(body, "<p class=\"msg\">{}</p>", escape_html(&err.message));
        }
    }
    let _ = write!(
        body,
        "<p class=\"s\">On its first start, Plumb downloads a public list of popular websites \
         and builds a first search index from it, which takes a minute or two. It adds more \
         lists while you search. This page reloads every {SETUP_RELOAD_SECONDS} seconds.</p>\n</main>"
    );
    let head = format!("<meta http-equiv=\"refresh\" content=\"{SETUP_RELOAD_SECONDS}\">\n");
    page_with_head("Setting up - Plumb Search", &head, &body)
}

fn results_header(query: &str) -> String {
    format!(
        "<header><a class=\"logo\" href=\"/\">Plumb</a>{}</header>",
        search_form(query, false)
    )
}

/// The results page's search form, which also picks the home country and
/// whether to leave out other countries' sites.
fn results_form(query: &str, options: &SearchOptions) -> String {
    let current = options.country.as_deref();
    let mut choices = format!(
        "<option value=\"any\"{}>Any country</option>",
        if current.is_none() { " selected" } else { "" }
    );
    let listed = current.is_some_and(|c| COUNTRY_CHOICES.iter().any(|(code, _)| *code == c));
    if let (Some(code), false) = (current, listed) {
        let _ = write!(
            choices,
            "<option value=\"{0}\" selected>{0}</option>",
            escape_html(code)
        );
    }
    for (code, name) in COUNTRY_CHOICES {
        let selected = if current == Some(*code) {
            " selected"
        } else {
            ""
        };
        let _ = write!(
            choices,
            "<option value=\"{code}\"{selected}>{name}</option>"
        );
    }
    format!(
        "<header><a class=\"logo\" href=\"/\">Plumb</a>\
         <form action=\"/search\" method=\"get\" role=\"search\">\
         <input type=\"search\" name=\"q\" value=\"{}\" placeholder=\"A site's name, e.g. us bank\" \
         aria-label=\"Search\" autocomplete=\"off\">\
         <button type=\"submit\">Search</button>\
         <div class=\"f\"><label>Country <select name=\"country\">{choices}</select></label> \
         <label><input type=\"checkbox\" name=\"only\" value=\"1\"{}> Only this country</label></div>\
         </form></header>",
        escape_html(query),
        if options.only_country { " checked" } else { "" }
    )
}

fn render_results(query: &str, results: &SearchResults, options: &SearchOptions) -> String {
    let hits = &results.hits;
    let mut body = format!(
        "<div class=\"wrap\">\n{}\n<main>\n",
        results_form(query, options)
    );
    if let Some(site_search) = &results.site_search {
        render_site_search(&mut body, site_search);
    }
    if hits.is_empty() {
        let _ = writeln!(
            body,
            "<p class=\"none\">No sites match <strong>{}</strong>.</p>",
            escape_html(query)
        );
    } else {
        body.push_str("<ol>\n");
        for hit in hits {
            render_hit(&mut body, hit);
        }
        body.push_str("</ol>\n");
    }
    let mut api = url::form_urlencoded::Serializer::new(String::new());
    api.append_pair("q", query);
    if let Some(country) = &options.country {
        api.append_pair("country", country);
    }
    if options.only_country {
        api.append_pair("only", "1");
    }
    let api = escape_html(&format!("/api/search?{}", api.finish()));
    let _ = write!(
        body,
        "<p class=\"s\">As JSON: <a href=\"{api}\">{api}</a></p>\n</main>\n</div>"
    );
    page(&format!("{query} - Plumb Search"), &body)
}

/// "Search github.com for sueheir plumb-search", above the results.
fn render_site_search(out: &mut String, site_search: &SiteSearch) {
    let Some(href) = http_url(&site_search.url) else {
        return;
    };
    let _ = writeln!(
        out,
        "<p class=\"ss\"><a href=\"{}\" rel=\"noreferrer\">Search {} for <strong>{}</strong></a></p>",
        escape_html(&href),
        escape_html(&site_search.domain),
        escape_html(&truncate_chars(&site_search.terms, 150))
    );
}

fn render_hit(out: &mut String, hit: &Hit) {
    let name = hit
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(&hit.domain);
    let name = escape_html(&truncate_chars(name, 150));
    out.push_str("<li>");
    match safe_href(hit) {
        Some(href) => {
            let _ = write!(
                out,
                "<a class=\"t\" href=\"{}\" rel=\"noreferrer\">{name}</a>\
                 <div class=\"u\">{}</div>",
                escape_html(&href),
                escape_html(&truncate_chars(&href, 100))
            );
        }
        None => {
            let _ = write!(
                out,
                "<span class=\"t\">{name}</span><div class=\"u\">{}</div>",
                escape_html(&hit.domain)
            );
        }
    }
    if let Some(description) = hit.description.as_deref().filter(|d| !d.trim().is_empty()) {
        let _ = write!(out, "<p class=\"d\">{}</p>", escape_html(description));
    }
    let country = hit
        .country
        .as_deref()
        .map(|code| format!(" &middot; {}", escape_html(country_name(code))))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "<div class=\"m\">{}{country} &middot; score {:.3} (text {:.3}, link {:.3})</div></li>",
        escape_html(&hit.domain),
        hit.score,
        hit.text_score,
        hit.link_score
    );
}

fn render_error(query: &str) -> String {
    let body = format!(
        "<div class=\"wrap\">\n{}\n<main>\n<p class=\"none\">Something went wrong while searching. \
         The server log has the details.</p>\n</main>\n</div>",
        results_header(query)
    );
    page("Error - Plumb Search", &body)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::Body;
    use axum::http::{HeaderMap, Request};
    use tower::ServiceExt;

    use super::*;
    use crate::node::{LastError, Progress};

    /// Returns canned hits and remembers what it was asked.
    #[derive(Default)]
    struct FakeBackend {
        hits: Vec<Hit>,
        fail: bool,
        panic: bool,
        calls: Mutex<Vec<(String, usize)>>,
    }

    impl SearchBackend for FakeBackend {
        fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
            self.calls.lock().unwrap().push((query.to_string(), limit));
            if self.panic {
                panic!("backend exploded");
            }
            if self.fail {
                anyhow::bail!("index is broken");
            }
            Ok(self.hits.iter().take(limit).cloned().collect())
        }

        fn num_docs(&self) -> u64 {
            12_345
        }
    }

    fn hit(domain: &str, url: &str, title: Option<&str>, description: Option<&str>) -> Hit {
        Hit {
            domain: domain.to_string(),
            url: url.to_string(),
            title: title.map(str::to_string),
            description: description.map(str::to_string),
            score: 0.9,
            text_score: 0.8,
            link_score: 0.7,
            country: None,
        }
    }

    fn bank_hits() -> Vec<Hit> {
        vec![
            hit(
                "usbank.com",
                "https://www.usbank.com/",
                Some("U.S. Bank | Personal & Business Banking"),
                Some("Checking, savings & loans."),
            ),
            hit(
                "usbank-login-help.com",
                "https://usbank-login-help.com/",
                None,
                None,
            ),
        ]
    }

    async fn get(backend: Arc<FakeBackend>, uri: &str) -> (StatusCode, HeaderMap, String) {
        send(router(backend), uri).await
    }

    async fn send(app: Router, uri: &str) -> (StatusCode, HeaderMap, String) {
        send_with_headers(app, uri, &[]).await
    }

    async fn send_with_headers(
        app: Router,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        let mut request = Request::builder().uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    fn backend(hits: Vec<Hit>) -> Arc<FakeBackend> {
        Arc::new(FakeBackend {
            hits,
            ..FakeBackend::default()
        })
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            escape_html(r#"<a href="x">Tom & 'Jerry'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; &#39;Jerry&#39;&lt;/a&gt;"
        );
        assert_eq!(escape_html("plain text"), "plain text");
    }

    #[test]
    fn links_only_to_http_urls() {
        let h = |domain: &str, url: &str| hit(domain, url, None, None);
        assert_eq!(
            safe_href(&h("usbank.com", "https://www.usbank.com/?a=1&b=2")).as_deref(),
            Some("https://www.usbank.com/?a=1&b=2")
        );
        assert_eq!(
            safe_href(&h("example.com", "HTTP://Example.com")).as_deref(),
            Some("http://example.com/")
        );
        for bad in [
            "javascript:alert(1)",
            " JavaScript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "file:///etc/passwd",
            "//evil.com/",
            "not a url",
        ] {
            assert_eq!(
                safe_href(&h("evil-example.com", bad)).as_deref(),
                Some("https://evil-example.com/"),
                "{bad}"
            );
        }
        assert_eq!(safe_href(&h("evil.com/\"><b>", "javascript:x")), None);
        assert_eq!(safe_href(&h("evil.com:8080", "javascript:x")), None);
        assert_eq!(safe_href(&h("", "javascript:x")), None);
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(12_345_678), "12,345,678");
    }

    #[tokio::test]
    async fn home_page_has_a_search_box() {
        let (status, headers, body) = get(backend(Vec::new()), "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html"));
        assert!(headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("default-src 'none'"));
        assert!(body.contains("<form action=\"/search\""), "{body}");
        assert!(body.contains("name=\"viewport\""));
        assert!(body.contains("12,345 sites indexed"));
    }

    #[tokio::test]
    async fn search_page_lists_hits() {
        let fake = backend(bank_hits());
        let (status, _, body) = get(Arc::clone(&fake), "/search?q=us+bank").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(
            "<a class=\"t\" href=\"https://www.usbank.com/\" rel=\"noreferrer\">\
             U.S. Bank | Personal &amp; Business Banking</a>"
        ));
        assert!(body.contains("<p class=\"d\">Checking, savings &amp; loans.</p>"));
        // A hit without a title is shown by its domain.
        assert!(body.contains(">usbank-login-help.com</a>"));
        assert!(body.contains("value=\"us bank\""));
        assert!(body.contains("href=\"/api/search?q=us+bank\""));
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec![("us bank".to_string(), DEFAULT_LIMIT)]
        );
    }

    #[tokio::test]
    async fn record_text_is_escaped() {
        let evil = hit(
            "evil-example.com",
            "javascript:alert(1)",
            Some("<script>alert('title')</script> Evil & Co"),
            Some("<img src=x onerror=alert(1)> \"quoted\""),
        );
        let (status, _, body) = get(backend(vec![evil]), "/search?q=%3Cb%3Eevil%3C%2Fb%3E").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("<script>"), "{body}");
        assert!(!body.contains("<img"), "{body}");
        assert!(!body.contains("<b>evil"), "{body}");
        assert!(!body.contains("javascript:"), "{body}");
        assert!(body.contains("&lt;script&gt;alert(&#39;title&#39;)&lt;/script&gt; Evil &amp; Co"));
        assert!(body.contains("&lt;img src=x onerror=alert(1)&gt; &quot;quoted&quot;"));
        assert!(body.contains("value=\"&lt;b&gt;evil&lt;/b&gt;\""));
        assert!(body.contains("href=\"https://evil-example.com/\""));
    }

    #[tokio::test]
    async fn empty_query_shows_the_home_page() {
        let fake = backend(bank_hits());
        let (status, _, body) = get(Arc::clone(&fake), "/search?q=+++").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("sites indexed"));
        assert!(fake.calls.lock().unwrap().is_empty());
        let (_, _, body) = get(Arc::clone(&fake), "/search").await;
        assert!(body.contains("sites indexed"));
    }

    #[tokio::test]
    async fn no_hits_message() {
        let (status, _, body) = get(backend(Vec::new()), "/search?q=zzz").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("No sites match <strong>zzz</strong>."));
    }

    #[tokio::test]
    async fn api_returns_hits_as_json() {
        let fake = backend(bank_hits());
        let (status, headers, body) =
            get(Arc::clone(&fake), "/api/search?q=us%20bank&limit=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        let hits: Vec<Hit> = serde_json::from_str(&body).unwrap();
        assert_eq!(hits, bank_hits()[..1].to_vec());

        let (_, _, body) = get(Arc::clone(&fake), "/api/search?q=x&limit=100000").await;
        assert_eq!(serde_json::from_str::<Vec<Hit>>(&body).unwrap().len(), 2);
        let (_, _, body) = get(Arc::clone(&fake), "/api/search?q=x&limit=0").await;
        assert_eq!(body, "[]");
        let (_, _, body) = get(Arc::clone(&fake), "/api/search").await;
        assert_eq!(body, "[]");
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec![("us bank".to_string(), 1), ("x".to_string(), MAX_LIMIT)]
        );
    }

    #[tokio::test]
    async fn bad_limit_is_a_client_error() {
        let (status, _, _) = get(backend(Vec::new()), "/api/search?q=x&limit=lots").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn backend_errors_become_500s() {
        let failing = Arc::new(FakeBackend {
            fail: true,
            ..FakeBackend::default()
        });
        let (status, _, body) = get(Arc::clone(&failing), "/search?q=x").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("Something went wrong"));
        assert!(!body.contains("index is broken"));
        let (status, _, body) = get(failing, "/api/search?q=x").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, r#"{"error":"search failed"}"#);

        let panicking = Arc::new(FakeBackend {
            panic: true,
            ..FakeBackend::default()
        });
        let (status, _, _) = get(panicking, "/search?q=x").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// A node that reports a set status.
    struct FakeNode(Status);

    impl StatusSource for FakeNode {
        fn status(&self) -> Status {
            self.0.clone()
        }
    }

    fn node(status: Status) -> Arc<FakeNode> {
        Arc::new(FakeNode(status))
    }

    fn node_status(phase: Phase, step: Step) -> Status {
        Status {
            phase,
            step,
            detail: "Downloading the Tranco list of popular sites".to_string(),
            progress: None,
            last_error: None,
            wikidata_missing: false,
            wikidata_error: None,
            sites: 0,
            index: None,
            last_refresh: None,
            next_refresh: None,
            version: "0.1.0".to_string(),
            crawl_left: 0,
            background_updates: true,
            paused: None,
            disk_used: 0,
            downloaded_today: 0,
            downloaded_total: 0,
            homepages_visited: 0,
        }
    }

    #[tokio::test]
    async fn a_node_reports_its_status_as_json() {
        let mut status = node_status(Phase::SettingUp, Step::Downloading);
        status.progress = Some(Progress {
            done: 1,
            total: 3,
            unit: "files".into(),
        });
        status.last_error = Some(LastError {
            message: "no network".into(),
            at: 1_700_000_000,
            retry_at: Some(1_700_000_600),
        });
        let app = node_router(backend(Vec::new()), node(status.clone()));
        let (code, headers, body) = send(app, "/api/status").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(serde_json::from_str::<Status>(&body).unwrap(), status);
        // The names are an interface: the desktop app and scripts read them.
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["phase"], "setting_up");
        assert_eq!(json["step"], "downloading");
        assert_eq!(
            json["progress"],
            serde_json::json!({"done": 1, "total": 3, "unit": "files"})
        );
        assert_eq!(
            json["last_error"],
            serde_json::json!({"message": "no network", "at": 1_700_000_000u64,
                               "retry_at": 1_700_000_600u64})
        );
        assert_eq!(json["wikidata_missing"], false);
        for null in ["index", "last_refresh", "next_refresh", "wikidata_error"] {
            assert!(json[null].is_null(), "{null}");
        }
        assert_eq!(json["sites"], 0);

        // `plumb serve` has no status to report.
        let (code, _, _) = get(backend(Vec::new()), "/api/status").await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_node_setting_up_shows_its_progress_instead_of_searching() {
        let now = now_unix();
        let mut status = node_status(Phase::SettingUp, Step::Retrying);
        status.detail = "Waiting <to> try again".to_string();
        status.progress = Some(Progress {
            done: 1_500,
            total: 10_000,
            unit: "homepages".into(),
        });
        status.last_error = Some(LastError {
            message: "HTTP 503: <script>alert('x')</script> & more".into(),
            at: now - 150,
            retry_at: Some(now + 630),
        });
        let fake = backend(bank_hits());
        let node = node(status);
        for uri in ["/", "/search?q=us+bank", "/search"] {
            let app = node_router(fake.clone(), node.clone());
            let (code, headers, body) = send(app, uri).await;
            assert_eq!(code, StatusCode::OK, "{uri}");
            assert_eq!(headers[header::CACHE_CONTROL], "no-store");
            assert!(headers[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("default-src 'none'"));
            for expected in [
                "<meta http-equiv=\"refresh\" content=\"5\">",
                "<title>Setting up - Plumb Search</title>",
                "<p class=\"step\">Waiting &lt;to&gt; try again</p>",
                "<progress value=\"1500\" max=\"10000\"></progress>",
                "1,500 of 10,000 homepages",
                "Something went wrong</strong> 2 minutes ago:",
                "HTTP 503: &lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; more",
                "Plumb will try again in 10 minutes.",
            ] {
                assert!(body.contains(expected), "{uri}: no {expected:?} in {body}");
            }
            assert!(!body.contains("<script"), "{body}");
            assert!(!body.contains("<form"), "{body}");
        }
        let app = node_router(fake.clone(), node.clone());
        let (code, headers, body) = send(app, "/api/search?q=us+bank").await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(headers[header::RETRY_AFTER], "5");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"error": "the search index is not ready yet",
                               "phase": "setting_up", "step": "retrying"})
        );
        assert!(fake.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn the_setup_page_shows_only_what_it_knows() {
        let status = node_status(Phase::SettingUp, Step::Starting);
        let body = render_setup(&status, 1_700_000_000);
        assert!(body.contains("Downloading the Tranco list"));
        assert!(!body.contains("<progress"));
        assert!(!body.contains("Something went wrong"));
        let mut status = status;
        status.progress = Some(Progress {
            done: 0,
            total: 0,
            unit: "files".into(),
        });
        status.last_error = Some(LastError {
            message: "boom".into(),
            at: 1_700_000_000,
            retry_at: None,
        });
        let body = render_setup(&status, 1_700_000_000);
        assert!(body.contains("<progress value=\"0\" max=\"1\"></progress>"));
        assert!(body.contains("Something went wrong</strong> just now:"));
        assert!(!body.contains("will try again"));
    }

    #[tokio::test]
    async fn a_ready_node_searches_and_says_what_it_is_doing() {
        let fake = backend(bank_hits());
        let mut status = node_status(Phase::Ready, Step::Crawling);
        status.progress = Some(Progress {
            done: 1_500,
            total: 10_000,
            unit: "homepages".into(),
        });
        let node = node(status);
        let app = || node_router(fake.clone(), node.clone());
        let (code, _, body) = send(app(), "/").await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains("<form action=\"/search\""), "{body}");
        assert!(
            body.contains("12,345 sites indexed &middot; crawling homepages, 1,500 of 10,000</p>"),
            "{body}"
        );
        assert!(!body.contains("http-equiv"), "{body}");
        let (code, _, body) = send(app(), "/search?q=us+bank").await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains("href=\"https://www.usbank.com/\""), "{body}");
        let (code, _, body) = send(app(), "/api/search?q=us+bank&limit=1").await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Vec<Hit>>(&body).unwrap(),
            bank_hits()[..1].to_vec()
        );
        assert_eq!(fake.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_node_without_wikidata_says_so() {
        let now = now_unix();
        let mut status = node_status(Phase::Ready, Step::Idle);
        status.wikidata_missing = true;
        status.wikidata_error = Some(LastError {
            message: "Wikidata stopped the query <at> its time limit".into(),
            at: now - 60,
            retry_at: Some(now + 630),
        });
        let fake = backend(bank_hits());
        let (code, _, body) = send(node_router(fake.clone(), node(status.clone())), "/").await;
        assert_eq!(code, StatusCode::OK);
        assert!(
            body.contains(
                "<p class=\"s\">Wikidata&#39;s list of official websites could not be \
                 downloaded yet, so the index does without it for now: official sites get no \
                 boost over look-alikes. Plumb will try again in 10 minutes.</p>"
            ),
            "{body}"
        );
        let (_, _, body) = send(
            node_router(fake.clone(), node(status.clone())),
            "/api/status",
        )
        .await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["wikidata_missing"], true);
        assert_eq!(
            json["wikidata_error"]["message"],
            "Wikidata stopped the query <at> its time limit"
        );

        // While the first index is built, the setup page says it too, with why.
        status.phase = Phase::SettingUp;
        status.step = Step::Indexing;
        let body = render_setup(&status, now);
        assert!(body.contains("could not be downloaded yet"), "{body}");
        assert!(
            body.contains(
                "<p class=\"msg\">Wikidata stopped the query &lt;at&gt; its time limit</p>"
            ),
            "{body}"
        );

        // Before Wikidata is first tried, it is still to come.
        status.wikidata_error = None;
        assert!(!render_setup(&status, now).contains("Wikidata"));
        status.phase = Phase::Ready;
        assert!(
            render_home(12, Some(&status), now).contains(
                "Plumb is still downloading Wikidata&#39;s list of official websites and more \
                 rankings. Search works now, and results get better once those are in."
            ),
            "{}",
            render_home(12, Some(&status), now)
        );

        // Once Wikidata is in, nothing is said.
        status.wikidata_missing = false;
        status.wikidata_error = None;
        assert!(!render_setup(&status, now).contains("Wikidata"));
        assert!(!render_home(12, Some(&status), now).contains("Wikidata"));
    }

    #[test]
    fn notes_on_the_home_page_of_a_ready_node() {
        let now = 1_700_000_000;
        let note = |step, progress: Option<Progress>, failed: bool, last_refresh| {
            let mut status = node_status(Phase::Ready, step);
            status.progress = progress;
            status.last_refresh = last_refresh;
            if failed {
                status.last_error = Some(LastError {
                    message: "boom".into(),
                    at: now,
                    retry_at: None,
                });
            }
            node_note(&status, now)
        };
        let crawled = |done| {
            Some(Progress {
                done,
                total: 5_000,
                unit: "homepages".into(),
            })
        };
        let cases = [
            (
                note(Step::Crawling, crawled(0), false, None),
                "crawling homepages, 0 of 5,000",
            ),
            (
                note(Step::Crawling, crawled(2_500), true, None),
                "crawling homepages, 2,500 of 5,000",
            ),
            (
                note(Step::Indexing, None, false, Some(now)),
                "rebuilding the index",
            ),
            (
                note(Step::Retrying, None, true, Some(now)),
                "the last update failed and will be tried again (see /api/status)",
            ),
            (
                note(Step::Idle, None, false, Some(now - 2 * 3_600)),
                "updated 2 hours ago",
            ),
            (
                note(Step::Crawling, None, false, Some(now - 30)),
                "updated 30 seconds ago",
            ),
        ];
        for (got, expected) in cases {
            assert_eq!(got.as_deref(), Some(expected));
        }
        assert_eq!(note(Step::Idle, None, false, None), None);
    }

    #[test]
    fn durations_in_words() {
        let words: Vec<String> = [
            0, 1, 59, 60, 119, 3_599, 3_600, 7_200, 86_399, 86_400, 259_200,
        ]
        .into_iter()
        .map(duration_words)
        .collect();
        assert_eq!(
            words,
            [
                "0 seconds",
                "1 second",
                "59 seconds",
                "1 minute",
                "1 minute",
                "59 minutes",
                "1 hour",
                "2 hours",
                "23 hours",
                "1 day",
                "3 days"
            ]
        );
        let now = 1_700_000_000;
        assert_eq!(time_ago(now, now), "just now");
        assert_eq!(time_ago(now - 9, now), "just now");
        assert_eq!(time_ago(now - 10, now), "10 seconds ago");
        assert_eq!(time_ago(now - 3_600, now), "1 hour ago");
        // A clock that went back a little.
        assert_eq!(time_ago(now + 60, now), "just now");
        assert_eq!(time_until(now + 600, now), "in 10 minutes");
        assert_eq!(time_until(now, now), "any moment now");
        assert_eq!(time_until(now - 60, now), "any moment now");
    }

    /// Whether `body` links to the OpenSearch description from its `<head>`.
    fn links_to_opensearch(body: &str) -> bool {
        let link = "<link rel=\"search\" type=\"application/opensearchdescription+xml\" \
                    title=\"Plumb Search\" href=\"/opensearch.xml\">";
        match (body.find(link), body.find("</head>")) {
            (Some(link), Some(head_end)) => link < head_end,
            _ => false,
        }
    }

    #[tokio::test]
    async fn every_page_links_to_the_opensearch_description() {
        for uri in ["/", "/search?q=us+bank"] {
            let (_, headers, body) = get(backend(bank_hits()), uri).await;
            assert!(links_to_opensearch(&body), "{uri}: {body}");
            // The link loads nothing into the page, so the policy stays as strict.
            assert_eq!(
                headers[header::CONTENT_SECURITY_POLICY],
                CONTENT_SECURITY_POLICY
            );
        }
        let failing = Arc::new(FakeBackend {
            fail: true,
            ..FakeBackend::default()
        });
        let (_, _, body) = get(failing, "/search?q=x").await;
        assert!(links_to_opensearch(&body), "{body}");
        let setting_up = node(node_status(Phase::SettingUp, Step::Downloading));
        let (_, _, body) = send(node_router(backend(Vec::new()), setting_up), "/").await;
        assert!(body.contains("<title>Setting up - Plumb Search</title>"));
        assert!(links_to_opensearch(&body), "{body}");
    }

    async fn opensearch_for(
        app: Router,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        send_with_headers(app, "/opensearch.xml", headers).await
    }

    #[tokio::test]
    async fn the_opensearch_description_describes_plumb() {
        let (code, headers, body) =
            opensearch_for(router(backend(Vec::new())), &[("host", "127.0.0.1:7586")]).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            headers[header::CONTENT_TYPE],
            "application/opensearchdescription+xml"
        );
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert!(
            body.starts_with(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <OpenSearchDescription xmlns=\"http://a9.com/-/spec/opensearch/1.1/\">\n"
            ),
            "{body}"
        );
        for expected in [
            "\n<ShortName>Plumb Search</ShortName>\n",
            "\n<Description>Find a site by its name.</Description>\n",
            "\n<InputEncoding>UTF-8</InputEncoding>\n",
            // A PNG starts with these bytes, in base64.
            "\n<Image width=\"32\" height=\"32\" type=\"image/png\">\
             data:image/png;base64,iVBORw0KGgo",
            "\n<Url type=\"text/html\" method=\"get\" \
             template=\"http://127.0.0.1:7586/search?q={searchTerms}\"/>\n",
        ] {
            assert!(body.contains(expected), "no {expected:?} in {body}");
        }
        assert!(body.ends_with("</OpenSearchDescription>\n"), "{body}");
        let base64 = body.split("base64,").nth(1).unwrap();
        let base64 = &base64[..base64.find('<').unwrap()];
        assert!(base64
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)));
    }

    #[tokio::test]
    async fn the_opensearch_description_searches_the_host_it_was_asked_from() {
        let cases: [(&[(&str, &str)], &str); 6] = [
            (&[("host", "127.0.0.1:8080")], "http://127.0.0.1:8080"),
            (&[("host", "Plumb.LAN:8080")], "http://plumb.lan:8080"),
            (&[("host", "192.168.1.20")], "http://192.168.1.20"),
            (&[("host", "[::1]:7586")], "http://[::1]:7586"),
            (&[("host", "localhost:80")], "http://localhost"),
            (
                &[
                    ("host", "search.example.org"),
                    ("x-forwarded-proto", "https, http"),
                ],
                "https://search.example.org",
            ),
        ];
        for (headers, origin) in cases {
            let (code, _, body) = opensearch_for(router(backend(Vec::new())), headers).await;
            assert_eq!(code, StatusCode::OK, "{headers:?}");
            let template = format!("template=\"{origin}/search?q={{searchTerms}}\"");
            assert!(
                body.contains(&template),
                "{headers:?}: no {template} in {body}"
            );
        }

        // A node serves it too, even before its index is ready.
        let setting_up = node(node_status(Phase::SettingUp, Step::Downloading));
        let app = node_router(backend(Vec::new()), setting_up);
        let (code, _, body) = opensearch_for(app, &[("host", "127.0.0.1:7586")]).await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains("template=\"http://127.0.0.1:7586/search?q={searchTerms}\""));
    }

    #[tokio::test]
    async fn the_opensearch_description_needs_a_plain_host() {
        for host in [
            "",
            "user@example.com",
            "example.com/path",
            "example.com?q=1",
            "example.com#top",
            "exa mple.com",
            "example.com:99999",
            "\"><script>alert(1)</script>",
        ] {
            let (code, _, body) =
                opensearch_for(router(backend(Vec::new())), &[("host", host)]).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{host:?}: {body}");
            assert!(!body.contains("<script"), "{body}");
        }
        let (code, _, _) = opensearch_for(router(backend(Vec::new())), &[]).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn the_opensearch_description_escapes_the_host() {
        let body = render_opensearch("http://a&b'c\"d<e>.example");
        assert!(
            body.contains(
                "template=\"http://a&amp;b&#39;c&quot;d&lt;e&gt;.example/search?q={searchTerms}\""
            ),
            "{body}"
        );
    }

    /// Answers with one hit and a site search link, and remembers the options.
    #[derive(Default)]
    struct OptionsBackend {
        link: String,
        options: Mutex<Vec<SearchOptions>>,
    }

    impl SearchBackend for OptionsBackend {
        fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Hit>> {
            unreachable!("pages search with options")
        }

        fn search_full(
            &self,
            query: &str,
            _limit: usize,
            options: &SearchOptions,
        ) -> Result<SearchResults> {
            self.options.lock().unwrap().push(options.clone());
            let mut github = hit("github.com", "https://github.com/", Some("GitHub"), None);
            github.country = Some("US".into());
            Ok(SearchResults {
                hits: vec![github],
                site_search: Some(SiteSearch {
                    domain: "github.com".into(),
                    terms: query.trim_start_matches("github ").into(),
                    url: self.link.clone(),
                }),
            })
        }

        fn num_docs(&self) -> u64 {
            1
        }
    }

    fn options_backend(link: &str) -> Arc<OptionsBackend> {
        Arc::new(OptionsBackend {
            link: link.into(),
            ..OptionsBackend::default()
        })
    }

    #[tokio::test]
    async fn pages_pick_the_country_and_link_to_site_search() {
        let fake = options_backend("https://github.com/search?q=plumb%20%3Cb%3E");
        let app = || router_with(fake.clone(), HomeCountry::Fixed("US".into()));
        let (code, _, body) =
            send(app(), "/search?q=github+plumb+%3Cb%3E&country=de&only=on").await;
        assert_eq!(code, StatusCode::OK);
        assert!(
            body.contains(
                "<p class=\"ss\"><a href=\"https://github.com/search?q=plumb%20%3Cb%3E\" \
             rel=\"noreferrer\">Search github.com for <strong>plumb &lt;b&gt;</strong></a></p>"
            ),
            "{body}"
        );
        assert!(body.contains("<option value=\"DE\" selected>Germany</option>"));
        assert!(body.contains("name=\"only\" value=\"1\" checked"));
        assert!(body.contains("github.com &middot; United States &middot; score"));
        assert!(body.contains("country=DE&amp;only=1"), "{body}");

        // No country asked for: the server's setting, then the browser's.
        send(app(), "/search?q=github").await;
        let auto = router_with(fake.clone(), HomeCountry::Auto);
        send_with_headers(
            auto,
            "/search?q=github",
            &[("accept-language", "en-GB,en;q=0.8")],
        )
        .await;
        // `any` turns the home country off, and `only` needs one.
        send(app(), "/search?q=github&country=any&only=1").await;
        // A bad code falls back to the setting.
        send(app(), "/search?q=github&country=zz").await;
        let seen: Vec<(Option<String>, bool)> = fake
            .options
            .lock()
            .unwrap()
            .iter()
            .map(|o| (o.country.clone(), o.only_country))
            .collect();
        let code = |c: &str| Some(c.to_string());
        assert_eq!(
            seen,
            [
                (code("DE"), true),
                (code("US"), false),
                (code("GB"), false),
                (None, false),
                (code("US"), false),
            ]
        );
    }

    #[tokio::test]
    async fn site_search_links_must_be_web_addresses() {
        let fake = options_backend("javascript:alert(1)");
        let (_, _, body) = send(router(fake), "/search?q=github+x").await;
        assert!(!body.contains("class=\"ss\""), "{body}");
        assert!(!body.contains("javascript:"));
    }

    #[tokio::test]
    async fn full_api_answers_include_the_site_search() {
        let fake = options_backend("https://github.com/search?q=x");
        let (_, _, body) = send(router(fake.clone()), "/api/search?q=github+x&full=1").await;
        let results: SearchResults = serde_json::from_str(&body).unwrap();
        assert_eq!(results.hits[0].domain, "github.com");
        assert_eq!(
            results.site_search.unwrap().url,
            "https://github.com/search?q=x"
        );
        // Without `full`, the plain list as before.
        let (_, _, body) = send(router(fake), "/api/search?q=github+x").await;
        let hits: Vec<Hit> = serde_json::from_str(&body).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn long_queries_are_cut() {
        let params = SearchParams {
            q: format!("  us \n bank {}", "x".repeat(500)),
            ..SearchParams::default()
        };
        let query = params.query();
        assert!(query.starts_with("us bank x"));
        assert_eq!(query.chars().count(), MAX_QUERY_CHARS);
        assert_eq!(params.limit(), DEFAULT_LIMIT);
    }
}
