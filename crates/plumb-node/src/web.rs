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
//! - `POST /mcp` answers AI apps over the Model Context Protocol (see
//!   [`crate::mcp`]),
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
//! A node in the Plumb network also serves `GET /network?q=`, which
//! searches other nodes without sending them the query
//! ([`plumb_net::NetHandle::search`]: it fetches buckets of sites under
//! throwaway identities), ranks what comes back with its own ranking, and
//! shows it; `GET /api/network/search?q=` returns the same as JSON.
//!
//! The search pages keep their settings (country, "only this country" and
//! "use the Plumb network") behind a gear, a `<details>` element, so they
//! need no script. With `net=1`, `/search` asks the network as well as this
//! node's own index and merges the two by score; sites only the network
//! found are tinted. Every results page says where its results came from,
//! and a node that has not joined the network shows the setting turned off
//! and says why.
//!
//! A node that shares popularity (`plumb run --share-popularity`) links its
//! results through `GET /go?q=&d=`, which notes that the site `d` was
//! picked for the query ([`StatusSource::record_pick`]) and redirects to
//! it. It redirects only to a site the same search returns, so it cannot
//! be used to send people elsewhere, and its result pages say that picks
//! are shared.
//!
//! Titles, descriptions and URLs in the index come from the open web, so
//! every piece of record text is HTML-escaped, only `http`/`https` URLs
//! become links, and pages are served with a Content-Security-Policy that
//! allows no scripts and no external resources. Searches run on Tokio's
//! blocking thread pool.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderName, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use plumb_core::{
    collapse_whitespace, display_url, language_code, now_unix, site_initial, truncate_chars,
    KeyPage, PageIntent, RecentNews, SafeSearch, SiteRecord,
};
use plumb_index::pages::{place_operator_pages, place_pages, PageHit};
use plumb_index::{
    build_index, Hit, RankConfig, SearchOptions, SearchResults, Searcher, SiteSearch, Spelling,
};
use plumb_net::{FoundSite, NetHandle, NetSearch};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};
use url::Url;

use crate::cli::ServeArgs;
use crate::country::{country_name, HomeCountry, COUNTRY_CHOICES};
use crate::meaning::{MeaningIndex, SharedMeaning};
use crate::news::Recent;
use crate::node::{NodeSettings, Phase, Status, Step};
use crate::plugins::PluginResults;
use crate::websearch::{bang_url, Engine, WebSettings};

pub(crate) mod answers;
mod control;
mod history;
mod nodes;
mod panel;
mod places;

use crate::{block_on, rank_config};
pub use panel::ADD_TO_FIREFOX_PATH;

mod mcp;
pub(crate) mod private;
mod relay;
mod searxng;
mod setup;

/// Results returned when a request does not say how many.
pub const DEFAULT_LIMIT: usize = 10;
/// Most results one request can get.
pub const MAX_LIMIT: usize = 100;
/// Longer queries are cut to this many characters.
pub const MAX_QUERY_CHARS: usize = 200;

/// No scripts, no external resources, forms only to this server. Inline
/// styles are allowed for the page's own `<style>` element.
const CONTENT_SECURITY_POLICY: &str =
    "default-src 'none'; style-src 'unsafe-inline'; img-src data:; \
     form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

/// Seconds between two reloads of the setup page, and the `Retry-After` of
/// a search asked for before the index is ready.
const SETUP_RELOAD_SECONDS: u32 = 5;

/// How long a network search waits for other nodes to answer.
const NETWORK_SEARCH_WAIT: Duration = Duration::from_secs(4);

/// The media type of an OpenSearch description.
const OPENSEARCH_TYPE: &str = "application/opensearchdescription+xml";

/// In the `<head>` of every page, so that browsers offer to add Plumb as a
/// search engine.
const OPENSEARCH_LINK: &str = "<link rel=\"search\" \
     type=\"application/opensearchdescription+xml\" title=\"Plumb Search\" \
     href=\"/opensearch.xml\">\n";

/// The places `query` asks for, searched on a blocking thread.
async fn run_places(
    state: &AppState,
    query: &str,
    home: Option<&str>,
    country: Option<&str>,
) -> Option<plumb_index::places::PlaceResults> {
    let backend = Arc::clone(&state.backend);
    let (query, home, country) = (
        query.to_string(),
        home.map(str::to_string),
        country.map(str::to_string),
    );
    tokio::task::spawn_blocking(move || backend.places(&query, home.as_deref(), country.as_deref()))
        .await
        .ok()
        .flatten()
}

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
            pages: Vec::new(),
            site_search: None,
            spelling: None,
        })
    }
    /// Number of sites that can be found.
    fn num_docs(&self) -> u64;
    /// The places `query` asks for, when it asks for places somewhere
    /// ("pizza in denver"), around `home`, the searcher's own town, for
    /// "near me"; see [`plumb_index::places`]. By default there are none.
    fn places(
        &self,
        query: &str,
        home: Option<&str>,
        country: Option<&str>,
    ) -> Option<plumb_index::places::PlaceResults> {
        let _ = (query, home, country);
        None
    }
}

/// A [`Searcher`] with fixed ranking settings.
pub struct IndexBackend {
    searcher: Searcher,
    rank: RankConfig,
    meaning: SharedMeaning,
    places: Option<plumb_index::places::PlaceSearcher>,
}

impl IndexBackend {
    pub fn new(searcher: Searcher, rank: RankConfig) -> Self {
        IndexBackend {
            searcher,
            rank,
            meaning: SharedMeaning::default(),
            places: None,
        }
    }

    /// Also lists the places of `places` for queries that ask for them.
    pub fn with_places(mut self, places: plumb_index::places::PlaceSearcher) -> Self {
        self.places = Some(places);
        self
    }

    /// Also ranks by meaning, for queries that name no site, once
    /// `meaning` holds a model and vectors.
    pub fn with_meaning(mut self, meaning: SharedMeaning) -> Self {
        self.meaning = meaning;
        self
    }

    /// Whether the index holds `domain`.
    pub fn has_domain(&self, domain: &str) -> bool {
        self.searcher.has_domain(domain)
    }

    /// [`SearchBackend::search_full`], ranking by `meaning` too when given.
    pub fn search_full_with(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
        meaning: Option<&MeaningIndex>,
    ) -> Result<SearchResults> {
        let query_meaning = meaning.and_then(|meaning| meaning.query(query));
        self.searcher.search_meaning(
            query,
            limit,
            &self.rank,
            options,
            query_meaning
                .as_ref()
                .map(|m| m as &dyn plumb_index::Meaning),
        )
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
        let meaning = self.meaning.get();
        self.search_full_with(query, limit, options, meaning.as_deref())
    }

    fn num_docs(&self) -> u64 {
        self.searcher.num_docs()
    }

    fn places(
        &self,
        query: &str,
        home: Option<&str>,
        country: Option<&str>,
    ) -> Option<plumb_index::places::PlaceResults> {
        let places = self.places.as_ref()?;
        places
            .search(query, home, country, 8)
            .unwrap_or_else(|err| {
                error!("searching places: {err:#}");
                None
            })
    }
}

/// What a long-running node tells its web pages about itself, and what its
/// panel (`/app`) can change.
pub trait StatusSource: Send + Sync {
    /// The node's status, as `GET /api/status` returns it.
    fn status(&self) -> Status;
    /// The network side, for a node that joined the Plumb network.
    fn network(&self) -> Option<Arc<NetHandle>> {
        None
    }
    /// How the node ranks, which it also uses for what other nodes send.
    fn rank(&self) -> RankConfig {
        RankConfig::default()
    }
    /// Names the bucket table private search fetches from, when the node
    /// serves private search and its index has buckets.
    fn bucket_table(&self) -> Option<String> {
        None
    }
    /// Bucket `bucket` of table `table`, its records as JSON; `None` when
    /// that table is not the one served (any more).
    fn bucket(&self, _table: &str, _bucket: u32) -> Option<Result<Vec<String>>> {
        None
    }
    /// Whether the node notes which result is opened for a search and
    /// reports it anonymously; its result links then go through `/go`.
    fn shares_popularity(&self) -> bool {
        false
    }
    /// Notes that `domain` was opened from the results for `query`.
    fn record_pick(&self, query: &str, domain: &str) {
        let _ = (query, domain);
    }
    /// Keeps signed crawls a network search found ([`FoundSite::keeps`]:
    /// trusted or confirmed ones) of sites this node already holds, folded into its records as a
    /// shared crawl is. Blocking.
    /// Whether safe search leaves out `domain` because it is on the
    /// node's adult blocklist.
    fn blocks_adult(&self, _domain: &str) -> bool {
        false
    }

    fn keep_from_network(&self, records: Vec<SiteRecord>) {
        let _ = records;
    }

    /// The icon of `domain` as a small PNG, when a crawl found one.
    fn icon(&self, _domain: &str) -> Option<Vec<u8>> {
        None
    }

    /// The "Recent" block for `query`, whose best result is `top` (its
    /// domain, and whether the query names it); see
    /// [`crate::news::NewsStore::recent`].
    fn recent(&self, _query: &str, _top: Option<(&str, bool)>) -> Option<crate::news::Recent> {
        None
    }

    /// Active and next-start feature choices, shared by desktop and Docker.
    fn features(&self) -> crate::node::features::FeatureSettings {
        Default::default()
    }
    fn saved_features(&self) -> Result<crate::node::features::FeatureSettings> {
        Ok(self.features())
    }
    fn change_features(&self, _features: crate::node::features::FeatureSettings) -> Result<()> {
        anyhow::bail!("this node has no feature settings")
    }

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

    /// Restarts the node, to apply saved feature changes; only a node whose
    /// [`Status::can_restart`] says so can.
    fn restart(&self) -> Result<()> {
        anyhow::bail!("This node cannot restart itself. Restart it where it runs.")
    }

    /// What the node did lately, newest first.
    fn activity_log(&self) -> Vec<crate::node::LogEntry> {
        Vec::new()
    }

    /// Tries failed work again now.
    fn retry(&self, _what: crate::node::Retry) -> Result<()> {
        anyhow::bail!("This node cannot retry work from the panel.")
    }

    /// Saves a backup in the data folder's `backups/`.
    fn make_backup(&self) -> Result<crate::node::backup::BackupInfo> {
        anyhow::bail!("This node has no data folder to back up.")
    }

    /// Restores `backup` into the data folder, then restarts if it can.
    fn restore_backup(&self, _backup: &crate::node::backup::Backup) -> Result<()> {
        anyhow::bail!("This node has no data folder to restore into.")
    }

    /// Tries the network's bootstrap nodes again now.
    fn reconnect_network(&self) -> Result<()> {
        match self.network() {
            Some(net) => net.reconnect(),
            None => anyhow::bail!("The Plumb network is off on this node."),
        }
    }

    /// Where the node keeps its data, to show on the panel. Its remote
    /// control file is there too: without a data folder, the node cannot be
    /// controlled remotely.
    fn data_dir(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// The address the node was told to listen on.
    fn bind(&self) -> Option<std::net::SocketAddr> {
        None
    }

    /// Where the node serves HTTPS for remote control, if it does.
    fn https_bind(&self) -> Option<std::net::SocketAddr> {
        None
    }

    /// Whether the panel may list and control other nodes.
    fn manages_other_nodes(&self) -> bool {
        false
    }

    /// Where the node keeps each browser's search history; `None` when it
    /// keeps none.
    fn search_history(&self) -> Option<crate::history::HistoryStore> {
        None
    }
}

#[derive(Clone)]
struct AppState {
    backend: Arc<dyn SearchBackend>,
    /// Set for a long-running node, `None` for `plumb serve`.
    node: Option<Arc<dyn StatusSource>>,
    /// The home country and the web search link.
    settings: WebSettings,
    /// Currency rates for instant answers.
    rates: Arc<answers::RatesCache>,
    /// How many tool calls each client may still make to `/mcp`.
    mcp_limiter: Arc<mcp::Limiter>,
    /// Fetches pages for `/mcp`'s `read_page`.
    page_reader: Arc<mcp::SharedReader>,
    /// What agents found, for `/mcp`'s `report_finding`; opened from the
    /// node's data directory when first needed.
    findings: Arc<std::sync::OnceLock<Option<Arc<crate::findings::Findings>>>>,
}

impl AppState {
    /// The node's status while it is still setting up; `None` once it is
    /// ready, and always for `plumb serve`.
    fn setting_up(&self) -> Option<Status> {
        let status = self.node.as_ref()?.status();
        (status.phase != Phase::Ready).then_some(status)
    }

    /// The node's findings; `None` for `plumb serve`, which keeps no data.
    fn findings(&self) -> Option<Arc<crate::findings::Findings>> {
        self.findings
            .get_or_init(|| {
                let dir = self.node.as_ref()?.data_dir()?;
                match crate::findings::Findings::in_dir(&dir) {
                    Ok(findings) => Some(Arc::new(findings)),
                    Err(err) => {
                        error!("opening findings: {err:#}");
                        None
                    }
                }
            })
            .clone()
    }

    fn network(&self) -> Option<Arc<NetHandle>> {
        self.node.as_ref()?.network()
    }

    fn shares_popularity(&self) -> bool {
        self.node
            .as_ref()
            .is_some_and(|node| node.shares_popularity())
    }

    /// The "Recent" block for `query`, whose results are `results`; `None`
    /// for `plumb serve`, which keeps no headlines. Headlines are matched
    /// with the query as typed: spelling corrections come from site names,
    /// and would turn news words into them.
    fn recent(&self, query: &str, results: &SearchResults) -> Option<Recent> {
        let top = results
            .hits
            .first()
            .map(|hit| (hit.domain.as_str(), hit.named));
        self.node.as_ref()?.recent(query, top)
    }

    /// What the node's plugins find for `query`.
    async fn plugin_results(&self, query: &str, options: &SearchOptions) -> Vec<PluginResults> {
        self.settings
            .plugins
            .search(query, options.safe, options.language.as_deref())
            .await
    }

    /// The icons of `domains` that this node has, for [`render_hit`]. Read
    /// off the async threads: each is a small file.
    async fn icons(&self, domains: Vec<String>) -> Icons {
        let Some(node) = self.node.clone() else {
            return Icons::default();
        };
        tokio::task::spawn_blocking(move || Icons::load(node.as_ref(), domains))
            .await
            .unwrap_or_default()
    }
}

/// Site icons for a results page, as `data:` URLs: the page carries them,
/// so the browser asks nobody else for them.
#[derive(Debug, Default)]
struct Icons(HashMap<String, String>);

impl Icons {
    fn load(node: &dyn StatusSource, domains: Vec<String>) -> Self {
        let mut icons = HashMap::new();
        for domain in domains {
            if icons.contains_key(&domain) {
                continue;
            }
            if let Some(png) = node.icon(&domain) {
                let url = format!("data:image/png;base64,{}", BASE64.encode(png));
                icons.insert(domain, url);
            }
        }
        Icons(icons)
    }

    fn get(&self, domain: &str) -> Option<&str> {
        self.0.get(domain).map(String::as_str)
    }
}

/// The web app: `/`, `/search` and `/api/search`.
pub fn router(backend: Arc<dyn SearchBackend>) -> Router {
    router_with(backend, HomeCountry::Auto)
}

/// [`router`] with [`WebSettings`], or just a [`HomeCountry`].
pub fn router_with(backend: Arc<dyn SearchBackend>, settings: impl Into<WebSettings>) -> Router {
    app(AppState {
        backend,
        node: None,
        settings: settings.into(),
        rates: Arc::default(),
        mcp_limiter: Arc::default(),
        page_reader: Arc::default(),
        findings: Arc::default(),
    })
}

/// The web app of a long-running node: what [`router`] serves, plus
/// `GET /api/status`. Until `status` reports [`Phase::Ready`], `/` and
/// `/search` show the setup page and `/api/search` answers 503.
pub fn node_router(backend: Arc<dyn SearchBackend>, status: Arc<dyn StatusSource>) -> Router {
    node_router_with(backend, status, HomeCountry::Auto)
}

/// [`node_router`] with [`WebSettings`], or just a [`HomeCountry`].
pub fn node_router_with(
    backend: Arc<dyn SearchBackend>,
    status: Arc<dyn StatusSource>,
    settings: impl Into<WebSettings>,
) -> Router {
    app(AppState {
        backend,
        node: Some(status),
        settings: settings.into(),
        rates: Arc::default(),
        mcp_limiter: Arc::default(),
        page_reader: Arc::default(),
        findings: Arc::default(),
    })
}

fn app(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/", get(home))
        .route("/search", get(search_page))
        .route("/api/search", get(api_search))
        .route("/api/websearch", post(searxng::external))
        .route("/opensearch.xml", get(opensearch));
    router = mcp::routes(router);
    if state.node.is_some() {
        router = router
            .route("/api/status", get(api_status))
            .route("/go", get(go))
            .route("/network", get(network_page))
            .route("/api/network/search", get(api_network_search))
            .route("/api/recent", get(api_recent))
            .route("/app", get(panel::panel))
            .route("/app/settings", post(panel::save_settings))
            .route("/app/setup", post(setup::save_setup))
            .route("/app/features", post(panel::save_features))
            .route("/app/refresh", post(panel::refresh))
            .route("/app/network/retry", post(panel::retry_network))
            .route("/app/pause", post(panel::pause))
            .route("/app/retry", post(panel::retry))
            .route("/app/backup", post(panel::backup))
            .route("/app/backups/restore", post(panel::restore_saved))
            .route("/app/restore", post(panel::restore_upload))
            .route("/app/backups/{name}", get(panel::download_backup))
            .route("/app/restart", post(panel::restart))
            .route("/app/remote-control", post(panel::save_remote_control))
            .route(panel::ADD_TO_FIREFOX_PATH, get(panel::add_to_firefox));
        router = private::routes(router);
        router = relay::routes(router);
        router = control::routes(router);
        router = nodes::routes(router);
        router = history::routes(router);
    }
    router.with_state(state)
}

/// Opens the index and serves it until Ctrl-C or SIGTERM.
pub fn run(args: ServeArgs) -> Result<()> {
    let searcher = Searcher::open(&args.index)
        .with_context(|| format!("opening the index in {}", args.index.display()))?;
    let docs = searcher.num_docs();
    let meaning = SharedMeaning::new(MeaningIndex::from_args(&args.meaning)?);
    let mut backend = IndexBackend::new(searcher, rank_config(args.alpha)).with_meaning(meaning);
    if let Some(places) = &args.places {
        backend = backend.with_places(crate::places::open_file(places)?);
    }
    let app = router_with(
        Arc::new(backend),
        WebSettings {
            home: args.country.clone(),
            web_search: args.web_search.0,
            read_pages_for_all: args.mcp_read_pages,
            plugins: args
                .plugins
                .as_deref()
                .map(crate::plugins::Plugins::load_dir)
                .unwrap_or_default(),
        },
    );
    block_on(async move {
        let listener = tokio::net::TcpListener::bind(args.bind)
            .await
            .with_context(|| format!("listening on {}", args.bind))?;
        let addr = listener.local_addr().context("reading the bound address")?;
        info!("serving {docs} sites on http://{addr}/ (Ctrl-C to stop)");
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
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
    /// `1` (or `on`, `true`): `/search` asks the Plumb network too.
    net: Option<String>,
    /// `1` (or `on`, `true`): search for the query as typed, without
    /// correcting typos.
    exact: Option<String>,
    /// `1`: the settings gear's form sent the history choices below.
    hist: Option<String>,
    /// `1`: show past searches.
    hs: Option<String>,
    /// `1`: rank sites opened before higher.
    hr: Option<String>,
    /// Safe search: `off`, `moderate` (the default) or `strict`.
    safe: Option<String>,
    /// Only sites in this language (a language code); empty for any.
    lang: Option<String>,
    /// The "Recent" headlines: `collapsed` (the default), `expanded` or
    /// `off`.
    news: Option<String>,
    /// `json`: `/search` answers in SearXNG's JSON format, for AI apps
    /// that take a SearXNG address (see [`searxng`]).
    format: Option<String>,
    /// SearXNG's page of results, from 1.
    pageno: Option<usize>,
    /// SearXNG's safe search: 0, 1 or 2.
    safesearch: Option<String>,
    /// SearXNG's categories, comma separated: `news` alone asks for
    /// recent headlines only.
    categories: Option<String>,
    /// SearXNG's time range (`day`, `week`, ...): recent headlines first.
    time_range: Option<String>,
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

    /// The history choices the settings gear's form sent, if it sent them.
    fn history_prefs(&self) -> Option<history::Prefs> {
        history::prefs_from_form(&self.hist, &self.hs, &self.hr)
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
            exact: flag(&self.exact),
            safe: self
                .safe
                .as_deref()
                .and_then(SafeSearch::parse)
                .unwrap_or_default(),
            language: self.lang.as_deref().and_then(language_code),
            recent: self
                .news
                .as_deref()
                .and_then(RecentNews::parse)
                .unwrap_or_default(),
        }
    }
}

async fn home(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    home_or_setup(&state, &params, &headers)
}

/// The home page, or the setup page while a node is still setting up. Its
/// settings gear shows what `params` and `headers` ask for.
fn home_or_setup(state: &AppState, params: &SearchParams, headers: &HeaderMap) -> Response {
    let status = state.node.as_ref().map(|node| node.status());
    let now = now_unix();
    match &status {
        Some(status) if status.phase != Phase::Ready => setup_response(status, now),
        _ => {
            let visitor = history::Visitor::of(state, headers, params.history_prefs());
            let settings = Settings {
                options: params.options(&state.settings.home, headers),
                network: state.net_setting(params),
                scope: state.search_scope(),
                private: state.private_search(),
                history: visitor.as_ref().map(history::Visitor::view),
            };
            let response = html_response(
                StatusCode::OK,
                render_home(state.backend.num_docs(), status.as_ref(), now, &settings),
            );
            match &visitor {
                Some(visitor) => visitor.send_cookies(response),
                None => response,
            }
        }
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

/// Whether a search asks the Plumb network, as the settings gear shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetSetting {
    /// This node has not joined the network: its own index only.
    Unavailable,
    Off,
    On,
}

/// What the settings gear holds.
struct Settings {
    options: SearchOptions,
    network: NetSetting,
    /// Which nodes the network part of a search asks.
    scope: plumb_net::SearchScope,
    /// This node offers private search (`/private`).
    private: bool,
    /// The searcher's history, on a node that keeps one.
    history: Option<history::HistoryView>,
}

impl AppState {
    /// Which nodes this node's network searches ask, set by its owner.
    fn search_scope(&self) -> plumb_net::SearchScope {
        self.network()
            .map(|net| net.status().search_scope)
            .unwrap_or_default()
    }

    fn net_setting(&self, params: &SearchParams) -> NetSetting {
        match (self.network().is_some(), flag(&params.net)) {
            (false, _) => NetSetting::Unavailable,
            (true, false) => NetSetting::Off,
            (true, true) => NetSetting::On,
        }
    }
}

/// How the network part of a search went, for the line that says where
/// the results came from.
enum NetOutcome {
    /// The network was not asked.
    NotAsked,
    Answered(NetworkResults),
    Failed,
}

async fn search_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    if params
        .format
        .as_deref()
        .is_some_and(|format| format.eq_ignore_ascii_case("json"))
    {
        return searxng::search(state, headers, params).await;
    }
    // A bang leaves Plumb, even while it sets up.
    if let Some(url) = bang_url(&params.q) {
        return (security_headers(), Redirect::to(&url)).into_response();
    }
    if let Some(status) = state.setting_up() {
        // Reloading keeps the query, so the results show up once the index is ready.
        return setup_response(&status, now_unix());
    }
    let query = params.query();
    if query.is_empty() {
        return home_or_setup(&state, &params, &headers);
    }
    let mut visitor = history::Visitor::of(&state, &headers, params.history_prefs());
    let mut settings = Settings {
        options: params.options(&state.settings.home, &headers),
        network: state.net_setting(&params),
        scope: state.search_scope(),
        private: state.private_search(),
        history: None,
    };
    let limit = params.limit();
    let (local, plugins) = tokio::join!(
        run_search(&state, &query, limit, &settings.options),
        state.plugin_results(&query, &settings.options)
    );
    let mut extras = match &local {
        Ok(results) => extras(&state, &query, results, &settings.options).await,
        Err(_) => answers::Extras::default(),
    };
    extras.plugins = plugins;
    let (local, network) = if settings.network == NetSetting::On {
        let network = network_search(&state, &query, limit, &settings.options).await;
        let network = match network {
            Ok(results) => NetOutcome::Answered(results),
            Err(_) => {
                // The page still has this node's own results.
                error!("search page network lookup failed");
                NetOutcome::Failed
            }
        };
        // Sites the searcher never wants to see stay out, wherever they
        // came from.
        let network = match (network, &visitor) {
            (NetOutcome::Answered(mut found), Some(visitor)) => {
                found
                    .hits
                    .retain(|result| !visitor.about.hides(&result.hit.domain));
                NetOutcome::Answered(found)
            }
            (network, _) => network,
        };
        (local, network)
    } else {
        (local, NetOutcome::NotAsked)
    };
    // "Near me" goes by the town the searcher gave.
    let found_places = run_places(
        &state,
        &query,
        visitor.as_ref().and_then(|v| v.about.town()),
        settings.options.country.as_deref(),
    )
    .await;
    let response = match local {
        Ok(mut results) => {
            if let Some(visitor) = &mut visitor {
                visitor.rank(&query, &mut results.hits);
                visitor.note_search(&query);
                settings.history = Some(visitor.view());
            }
            let mut domains: Vec<String> =
                results.hits.iter().map(|hit| hit.domain.clone()).collect();
            if let NetOutcome::Answered(found) = &network {
                domains.extend(found.hits.iter().map(|result| result.hit.domain.clone()));
            }
            for placed in &results.pages {
                let domain = placed.hit.page.set_domain();
                if !domains.iter().any(|d| d == domain) {
                    domains.push(domain.to_string());
                }
            }
            if let Some(profile) = &extras.profile {
                domains.extend(plumb_core::registrable_domain(&profile.url));
            }
            if let Some(found) = &found_places {
                domains.extend(places::website_domains(found));
            }
            let icons = state.icons(domains).await;
            let recent = state.recent(&query, &results);
            let mut page = render_results_with(
                &query,
                &results,
                Some(&extras),
                &network,
                &settings,
                state.settings.web_search,
                limit,
                state.shares_popularity(),
                &icons,
                recent.as_ref(),
            );
            if let Some(found) = &found_places {
                // Above the sites.
                let html = places::render_places(
                    found,
                    visitor.is_some(),
                    settings.options.country.as_deref(),
                    &icons,
                );
                if let Some(at) = page.find("<main>\n") {
                    page.insert_str(
                        at + "<main>\n".len(),
                        &format!("<style>{}</style>\n{html}", places::STYLE),
                    );
                }
            }
            html_response(StatusCode::OK, page)
        }
        Err(_) => {
            error!("search page local lookup failed");
            html_response(StatusCode::INTERNAL_SERVER_ERROR, render_error(&query))
        }
    };
    match &visitor {
        Some(visitor) => visitor.send_cookies(response),
        None => response,
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
    let options = params.options(&state.settings.home, &headers);
    let (found, plugins) = tokio::join!(
        run_search(&state, &query, params.limit(), &options),
        async {
            if full {
                state.plugin_results(&query, &options).await
            } else {
                Vec::new()
            }
        }
    );
    match found {
        Ok(results) if full => {
            let extras = extras(&state, &query, &results, &options).await;
            let info = match &extras.profile {
                Some(profile) => answers::info_from_page(&profile.page, &results.hits),
                None => {
                    let placed = place_pages(
                        &query,
                        &results.hits,
                        results.pages.iter().map(|p| p.hit.clone()).collect(),
                    );
                    answers::info_box(&results.hits, &placed)
                }
            };
            let places = run_places(&state, &query, None, options.country.as_deref()).await;
            let body = FullResults {
                results: &results,
                answer: extras.answer,
                profile: extras.profile,
                info,
                places,
                plugins,
            };
            (StatusCode::OK, security_headers(), Json(body)).into_response()
        }
        Ok(results) => (StatusCode::OK, security_headers(), Json(results.hits)).into_response(),
        Err(_) => {
            error!("search API local lookup failed");
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

/// `GET /api/recent?q=...`: the "Recent" block the results page shows
/// for the query, as JSON; `{"headlines":[]}` when it shows none.
async fn api_recent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    let query = params.query();
    let recent = if query.is_empty() || state.setting_up().is_some() {
        None
    } else {
        let options = params.options(&state.settings.home, &headers);
        match run_search(&state, &query, params.limit(), &options).await {
            Ok(results) => state.recent(&query, &results),
            Err(_) => None,
        }
    };
    (
        StatusCode::OK,
        security_headers(),
        Json(recent.unwrap_or_default()),
    )
        .into_response()
}

/// `/api/search?full=1`: the results, and the instant answer and info box
/// the results page shows with them.
#[derive(Serialize)]
struct FullResults<'a> {
    #[serde(flatten)]
    results: &'a SearchResults,
    #[serde(skip_serializing_if = "Option::is_none")]
    answer: Option<plumb_answer::Answer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<answers::ProfileAnswer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    info: Option<answers::InfoBox>,
    #[serde(skip_serializing_if = "Option::is_none")]
    places: Option<plumb_index::places::PlaceResults>,
    /// What the node's plugins found; never from other nodes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    plugins: Vec<PluginResults>,
}

/// The instant answer and the official profile asked for, for `query`
/// whose own results are `results`. The profile is looked up by searching
/// again for the words before the service ("mrbeast" of "mrbeast
/// youtube"), unless the whole query already names a page.
async fn extras(
    state: &AppState,
    query: &str,
    results: &SearchResults,
    options: &SearchOptions,
) -> answers::Extras {
    let answer = instant_answer(state, query).await;
    let names_a_page = results.pages.iter().any(|placed| placed.hit.named);
    let profile = match plumb_core::profiles::services_asked(query) {
        Some((_, name)) if !names_a_page => run_search(state, &name, PROFILE_SEARCH_LIMIT, options)
            .await
            .ok()
            .and_then(|found| answers::profile_answer(query, &found.pages)),
        _ => None,
    };
    answers::Extras {
        answer,
        profile,
        plugins: Vec::new(),
    }
}

/// Results asked for when looking up whose profile a query asks for.
const PROFILE_SEARCH_LIMIT: usize = 5;

/// The instant answer to `query`, with currency rates when it needs them.
async fn instant_answer(state: &AppState, query: &str) -> Option<plumb_answer::Answer> {
    let rates = state.rates.for_query(query).await;
    let now = i64::try_from(now_unix()).unwrap_or(i64::MAX);
    plumb_answer::answer(query, now, rates.as_ref())
}

#[derive(Debug, Default, Deserialize)]
struct GoParams {
    #[serde(default)]
    q: String,
    /// The domain picked.
    #[serde(default)]
    d: String,
    country: Option<String>,
    only: Option<String>,
    exact: Option<String>,
    safe: Option<String>,
    lang: Option<String>,
    news: Option<String>,
}

/// `GET /go?q=&d=`: notes that `d` was picked for the query, when the node
/// shares popularity, and redirects to it. Only a site the same search
/// returns is redirected to; anything else goes back to the results.
async fn go(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<GoParams>,
) -> Response {
    let search = SearchParams {
        q: params.q,
        limit: Some(MAX_LIMIT),
        country: params.country,
        only: params.only,
        full: None,
        net: None,
        exact: params.exact,
        hist: None,
        hs: None,
        hr: None,
        safe: params.safe,
        lang: params.lang,
        news: params.news,
        format: None,
        pageno: None,
        safesearch: None,
        categories: None,
        time_range: None,
    };
    let query = search.query();
    let back = {
        let encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
        format!("/search?q={encoded}")
    };
    let found = if query.is_empty() || state.setting_up().is_some() {
        None
    } else {
        let options = search.options(&state.settings.home, &headers);
        match run_search(&state, &query, MAX_LIMIT, &options).await {
            Ok(results) => results.hits.into_iter().find(|hit| hit.domain == params.d),
            Err(_) => {
                error!("result redirect search failed");
                None
            }
        }
    };
    let Some((hit, href)) = found.and_then(|hit| safe_href(&hit).map(|href| (hit, href))) else {
        return redirect(&back);
    };
    if let Some(mut visitor) = history::Visitor::of(&state, &headers, None) {
        visitor.note_opened(&query, &hit.domain);
    }
    if state.shares_popularity() {
        if let Some(node) = state.node.clone() {
            let _ =
                tokio::task::spawn_blocking(move || node.record_pick(&query, &hit.domain)).await;
        }
    }
    redirect(&href)
}

fn redirect(location: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        security_headers(),
        [
            (header::LOCATION, location.to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
    )
        .into_response()
}

/// `GET /network?q=`: only what other nodes answer, for a node in the network.
async fn network_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    if state.network().is_none() {
        return html_response(StatusCode::NOT_FOUND, render_no_network());
    }
    let query = params.query();
    if query.is_empty() {
        return home_or_setup(&state, &params, &headers);
    }
    let options = params.options(&state.settings.home, &headers);
    match network_search(&state, &query, params.limit(), &options).await {
        Ok(results) => {
            let domains = results.hits.iter().map(|r| r.hit.domain.clone()).collect();
            let icons = state.icons(domains).await;
            html_response(StatusCode::OK, render_network(&query, &results, &icons))
        }
        Err(_) => {
            error!("network search page lookup failed");
            html_response(StatusCode::INTERNAL_SERVER_ERROR, render_error(&query))
        }
    }
}

/// `GET /api/network/search?q=&limit=`: [`NetworkResults`] as JSON.
async fn api_network_search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    if state.network().is_none() {
        let body = serde_json::json!({ "error": "this node has not joined the Plumb network" });
        return (StatusCode::NOT_FOUND, security_headers(), Json(body)).into_response();
    }
    let query = params.query();
    if query.is_empty() {
        return (
            StatusCode::OK,
            security_headers(),
            Json(NetworkResults::default()),
        )
            .into_response();
    }
    let options = params.options(&state.settings.home, &headers);
    match network_search(&state, &query, params.limit(), &options).await {
        Ok(results) => (StatusCode::OK, security_headers(), Json(results)).into_response(),
        Err(_) => {
            error!("network search API lookup failed");
            let body = serde_json::json!({ "error": "network search failed" });
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                security_headers(),
                Json(body),
            )
                .into_response()
        }
    }
}

/// What a network search found, ranked by this node.
#[derive(Debug, Default, Serialize)]
pub struct NetworkResults {
    /// Buckets asked for, padding included.
    pub buckets: usize,
    /// Requests sent, each to one node under its own throwaway identity.
    pub asked: usize,
    pub answered: usize,
    /// Answered sealed through another node, so the node answering never
    /// saw this node's IP address.
    pub relayed: usize,
    /// Sent straight to the node answering, because no other node could
    /// relay; that node saw this node's IP address.
    pub direct: usize,
    /// Answers dropped because a proof in them did not check out.
    pub rejected: usize,
    /// The query's buckets answered from this node's copies of ones it
    /// fetched lately, without asking the network again.
    pub cached: usize,
    /// Needed buckets waiting for a scheduled background download.
    pub pending: usize,
    /// Retained buckets used while their background refresh is due.
    pub stale: usize,
    pub hits: Vec<NetworkResult>,
}

/// One site from other nodes, ranked here.
#[derive(Debug, Serialize)]
pub struct NetworkResult {
    #[serde(flatten)]
    pub hit: Hit,
    /// The text comes from a signed crawl whose proof checked out.
    pub verified: bool,
    /// The node that signed that crawl.
    pub crawler: Option<String>,
    /// Signed crawls from two or more different nodes agree on the text.
    pub confirmed: bool,
    /// How many answers held the site.
    pub answers: usize,
    /// The signed crawl this node may keep, see [`FoundSite::shared`].
    #[serde(skip)]
    pub shared: Option<SiteRecord>,
}

/// Fetches the query's buckets from other nodes and ranks the sites that
/// match with this node's own ranking and the searcher's choices, in a
/// small index built for the purpose and deleted after.
async fn network_search(
    state: &AppState,
    query: &str,
    limit: usize,
    options: &SearchOptions,
) -> Result<NetworkResults> {
    let net = state.network().context("not in the network")?;
    let rank = state
        .node
        .as_ref()
        .map_or_else(RankConfig::default, |node| node.rank());
    // Search operators narrow the ranking below; only words pick buckets.
    let lookup = plumb_core::Operators::parse(query).lookup_text();
    let found = net.search(&lookup, NETWORK_SEARCH_WAIT).await?;
    let query = query.to_string();
    let options = options.clone();
    let node = state.node.clone();
    tokio::task::spawn_blocking(move || {
        // Signed crawls of sites this node holds fill in what its own
        // records lack, for its next index and search by meaning.
        if let Some(node) = &node {
            let shared: Vec<SiteRecord> = found
                .found
                .iter()
                .filter_map(|site| site.keeps().cloned())
                .collect();
            if !shared.is_empty() {
                node.keep_from_network(shared);
            }
        }
        let mut results = rank_found(found, &query, limit, &rank, &options)?;
        // Adult sites stay out of network results too.
        if let (Some(node), true) = (&node, options.safe != SafeSearch::Off) {
            results
                .hits
                .retain(|result| !node.blocks_adult(&result.hit.domain));
        }
        Ok(results)
    })
    .await
    .context("the ranking task failed")?
}

fn rank_found(
    found: NetSearch,
    query: &str,
    limit: usize,
    rank: &RankConfig,
    options: &SearchOptions,
) -> Result<NetworkResults> {
    let mut results = NetworkResults {
        buckets: found.buckets,
        asked: found.asked,
        answered: found.answered,
        relayed: found.relayed,
        direct: found.direct,
        rejected: found.rejected,
        cached: found.cached,
        pending: found.pending,
        stale: found.stale,
        hits: Vec::new(),
    };
    if found.found.is_empty() || limit == 0 {
        return Ok(results);
    }
    let dir = tempfile::tempdir().context("making a folder for ranking")?;
    let index = dir.path().join("index");
    let records: Vec<SiteRecord> = found.found.iter().map(|f| f.record.clone()).collect();
    build_index(&index, &records)?;
    // The few sites found are no dictionary to correct typos against: the
    // query was corrected, if at all, by this node's own index.
    let options = SearchOptions {
        exact: true,
        ..options.clone()
    };
    let hits = Searcher::open(&index)?
        .search_full(query, limit, rank, &options)?
        .hits;
    let by_domain: std::collections::HashMap<&str, &FoundSite> = found
        .found
        .iter()
        .map(|f| (f.record.domain.as_str(), f))
        .collect();
    for hit in hits {
        let Some(site) = by_domain.get(hit.domain.as_str()) else {
            continue;
        };
        results.hits.push(NetworkResult {
            verified: site.verified,
            crawler: site.crawler.clone(),
            confirmed: site.confirmed,
            answers: site.answers,
            shared: site.shared.clone(),
            hit,
        });
    }
    Ok(results)
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
         <Description>Plumb Search</Description>\n\
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
/// Callers log only the failed operation: backend error messages can include
/// search terms, so neither those messages nor queries belong in diagnostics.
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
    debug!("local search completed: {} hits", results.hits.len());
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
pub(crate) fn time_ago(at: u64, now: u64) -> String {
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
--url:#0d652d;--line:#dadce0;--accent:#1a73e8;--err:#b3261e;--net:#f2effb;--chip:#fff;\
--seen:#681da8}\
@media (prefers-color-scheme:dark){:root{--bg:#1f1f1f;--fg:#e8eaed;--muted:#9aa0a6;\
--link:#8ab4f8;--url:#81c995;--line:#3c4043;--accent:#8ab4f8;--err:#f2b8b5;--net:#29263a;\
--chip:#f1f3f4;--seen:#c58af9}}\
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
main>ol{margin-top:.5rem}\
li{padding:.85rem 0;margin:.25rem 0}\
.r{display:block;color:inherit;text-decoration:none}\
.site{display:flex;align-items:center;gap:.7rem;min-width:0;margin-bottom:.35rem}\
.ic{flex:none;display:grid;place-items:center;width:1.85rem;height:1.85rem;border-radius:50%;\
background:var(--chip);border:1px solid var(--line);overflow:hidden}\
.ic img{display:block;width:18px;height:18px}\
.ic.l0,.ic.l1,.ic.l2,.ic.l3,.ic.l4,.ic.l5,.ic.l6,.ic.l7{border:0;color:#fff;\
font-size:.85rem;font-weight:600;line-height:1}\
.l0{background:#1a73e8}.l1{background:#d93025}.l2{background:#188038}.l3{background:#e37400}\
.l4{background:#9334e6}.l5{background:#007b83}.l6{background:#c5221f}.l7{background:#5f6368}\
.sn{display:flex;flex-direction:column;min-width:0;line-height:1.3}\
.dn{font-size:.875rem;color:var(--fg);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}\
.u{font-size:.75rem;color:var(--muted);white-space:nowrap;overflow:hidden;text-overflow:ellipsis}\
.t{display:block;font-size:1.25rem;line-height:1.3;color:var(--link);overflow-wrap:anywhere}\
a.r:hover .t,a.r:focus-visible .t{text-decoration:underline}\
a.r:visited .t{color:var(--seen)}\
.d{margin:.3rem 0 0;line-height:1.55;overflow-wrap:anywhere}\
.pk code{font-size:.85em;padding:0 .3em;border-radius:4px;background:rgba(127,127,127,.15)}\
.pk a{color:var(--link)}\
.sub{margin:.35rem 0 0;font-size:.9rem;line-height:1.5;overflow-wrap:anywhere}\
.sub a{color:var(--link)}\
.kp{display:flex;flex-wrap:wrap;gap:.3rem 1.25rem;margin:.45rem 0 0;font-size:.9rem}\
.kp li{padding:0;margin:0}\
.kp a{color:var(--link)}\
.tag,.m,.s{color:var(--muted)}\
.m,.s{font-size:.8rem}\
.m{margin-top:.25rem}\
.s{margin-top:1.5rem}\
.none{margin:1.5rem 0}\
header form{flex-wrap:wrap}\
form[role=search]{position:relative}\
.gear{flex:none}\
.gear>summary{list-style:none;cursor:pointer;padding:.55rem .7rem;border:1px solid var(--line);\
border-radius:.5rem;color:var(--muted);user-select:none}\
.gear>summary::-webkit-details-marker{display:none}\
.gear[open]>summary,.gear>summary:hover{color:var(--fg);border-color:var(--accent)}\
.panel{position:absolute;right:0;top:calc(100% + .4rem);z-index:2;width:min(20rem,calc(100vw - 2rem));\
display:grid;gap:.6rem;padding:.8rem .9rem;text-align:left;font-size:.875rem;\
background:var(--bg);border:1px solid var(--line);border-radius:.6rem;\
box-shadow:0 6px 20px rgba(0,0,0,.18)}\
.panel label{display:flex;gap:.45rem;align-items:center}\
.panel input{flex:none;margin:0}\
.panel .hint{margin:-.35rem 0 0 1.45rem;color:var(--muted);font-size:.8rem}\
.panel label.off{color:var(--muted)}\
.panel button{justify-self:end;padding:.35rem .9rem}\
.pv{margin:0;padding-top:.5rem;border-top:1px solid var(--line)}\
.src a,.err a{color:var(--link)}\
.pv{display:grid;gap:.6rem}\
.panel .pv .hint{margin-left:2.65rem}\
.panel a.tg{display:flex;gap:.6rem;align-items:center;color:var(--fg);text-decoration:none}\
.knob{flex:none;position:relative;width:2rem;height:1.1rem;border-radius:1rem;\
background:var(--line);transition:background .15s}\
.knob::after{content:\"\";position:absolute;top:.15rem;left:.15rem;width:.8rem;height:.8rem;\
border-radius:50%;background:var(--bg);transition:left .15s}\
.tg[aria-checked=true] .knob{background:var(--accent)}\
.tg[aria-checked=true] .knob::after{left:1.05rem}\
.tg:hover .knob,.tg:focus-visible .knob{outline:2px solid var(--accent);outline-offset:1px}\
.src{margin:.75rem 0 0;font-size:.8rem;color:var(--muted)}\
.src a{color:var(--link)}\
.sw{display:inline-block;width:.8em;height:.8em;margin:0 .2em -.1em 0;border-radius:.2em;\
background:var(--net);border:1px solid var(--muted)}\
li.net{background:var(--net);margin:.25rem -.75rem;padding:.85rem .75rem;border-radius:.75rem}\
select{font:inherit;padding:.15rem .3rem;border:1px solid var(--line);border-radius:.35rem;\
background:var(--bg);color:var(--fg)}\
.ss{margin:1rem 0 .25rem;padding:.6rem .8rem;border:1px solid var(--line);border-radius:.5rem}\
.ss a{color:var(--link)}\
.sp{margin:1rem 0 .25rem}.sp a{color:var(--link)}\
li.news{padding:.6rem .9rem;border:1px solid var(--line);border-radius:.6rem}\
.news summary{cursor:pointer}\
.news .nh{font-size:.875rem;font-weight:600}\
.news details[open] summary{margin-bottom:.2rem}\
.news ol li{padding:.3rem 0;margin:0}\
.news a{color:var(--link);text-decoration:none;overflow-wrap:anywhere}\
.news a:hover,.news a:focus-visible{text-decoration:underline}\
.news .m{margin:0}\
.plugin .nh{margin:0 0 .2rem}\
.plugin .d{margin:.1rem 0;font-size:.875rem}\
.web{margin:.25rem 0;font-size:.9rem}.web a{color:var(--muted)}\
.setup{max-width:36rem}\
.step{margin:2rem 0 .5rem;font-size:1.1rem}\
progress{width:100%;height:.75rem;accent-color:var(--accent)}\
.err{margin-top:1.5rem;padding:.25rem 1rem;border:1px solid var(--err);border-radius:.5rem;\
text-align:left}\
.err strong{color:var(--err)}\
.msg{white-space:pre-wrap;overflow-wrap:anywhere;font:.85rem/1.4 ui-monospace,monospace}\
.op{color:var(--url);font-weight:600}\
.panel a{color:var(--link)}\
.recent{margin:1rem auto 0;max-width:36rem;display:flex;flex-wrap:wrap;gap:.4rem;\
justify-content:center;align-items:center;font-size:.875rem}\
.recent ul{display:contents;list-style:none}\
.recent li{margin:0;padding:0}\
.recent li a{display:inline-block;padding:.2rem .7rem;border:1px solid var(--line);\
border-radius:1rem;color:var(--fg);text-decoration:none}\
.recent li a:hover{border-color:var(--accent)}\
.recent .all{color:var(--muted)}\
.hist h1{font-size:1.6rem;margin-top:1.25rem}.hist h2{font-size:1.05rem;margin:1.75rem 0 .5rem}\
.hist ul{padding-left:1.1rem}.hist li{padding:.2rem 0;margin:0}.hist li a{color:var(--link)}\
.hist form{margin-top:1.5rem}\
.about label{display:block;margin-top:1.25rem}.about .m{margin:.2rem 0 .4rem}\
.about textarea,.about #town{width:100%;box-sizing:border-box;font:inherit;padding:.4rem;\
background:var(--bg);color:var(--fg);border:1px solid var(--line);border-radius:6px}\
.ia{margin:1rem 0 .5rem;padding:.85rem 1rem;border:1px solid var(--line);border-radius:.75rem}\
.ia p{margin:0}.iaq{color:var(--muted);font-size:.9rem;overflow-wrap:anywhere}\
.iaa{font-size:1.75rem;line-height:1.3;overflow-wrap:anywhere}.ia .m{margin-top:.2rem}\
.wide{max-width:74rem}.wide header form{max-width:42rem}\
.cols{display:flex;flex-direction:column}.cols>main{min-width:0}\
.ib{order:-1;margin:1rem 0 .25rem;padding:1rem 1.1rem;border:1px solid var(--line);\
border-radius:.75rem;overflow-wrap:anywhere}\
.ib h2{margin:0;font-size:1.35rem;line-height:1.3}\
.ibd{margin:.2rem 0 0;color:var(--muted)}\
.ib dl{display:grid;grid-template-columns:auto 1fr;gap:.25rem .9rem;margin:.8rem 0 0;font-size:.9rem}\
.ib dt{color:var(--muted)}.ib dd{margin:0;min-width:0}\
.ib a{color:var(--link)}.ibl{margin:.8rem 0 0;font-size:.9rem}\
.pf{margin:1rem 0 .5rem;padding:.85rem 1rem;border:1px solid var(--accent);border-radius:.75rem}\
.pf .m{margin:.3rem 0 0}\
.ibp{display:flex;flex-wrap:wrap;gap:.4rem;margin:.8rem 0 0;padding:0;list-style:none;font-size:.85rem}\
.ibp li{margin:0;padding:0}.ibp a{display:inline-block;padding:.15rem .65rem;\
border:1px solid var(--line);border-radius:1rem;text-decoration:none}\
.ibp a:hover{border-color:var(--accent)}.pfirst .ib{order:0}\
@media (min-width:64rem){.cols{display:grid;grid-template-columns:minmax(0,44rem) minmax(0,22rem);\
gap:0 3rem;align-items:start}.ib{order:0;margin-top:1.25rem}}";

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

/// The search form with its settings tucked behind a gear: the home
/// country, whether to leave out other countries' sites, and whether to
/// ask the Plumb network too. A `<details>` opens the gear, so it needs no
/// script; the settings travel with the search as query parameters.
/// The languages the settings gear offers, by their own names.
const LANGUAGE_CHOICES: &[(&str, &str)] = &[
    ("en", "English"),
    ("de", "Deutsch"),
    ("es", "Español"),
    ("fr", "Français"),
    ("it", "Italiano"),
    ("nl", "Nederlands"),
    ("pl", "Polski"),
    ("pt", "Português"),
    ("sv", "Svenska"),
    ("tr", "Türkçe"),
    ("ru", "Русский"),
    ("uk", "Українська"),
    ("ar", "العربية"),
    ("hi", "हिन्दी"),
    ("ja", "日本語"),
    ("ko", "한국어"),
    ("zh", "中文"),
];

fn settings_form(query: &str, autofocus: bool, settings: &Settings) -> String {
    let options = &settings.options;
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
    let safe_choices: String = [
        (SafeSearch::Off, "Off"),
        (SafeSearch::Moderate, "Moderate"),
        (SafeSearch::Strict, "Strict"),
    ]
    .into_iter()
    .map(|(level, name)| {
        let selected = if options.safe == level {
            " selected"
        } else {
            ""
        };
        format!(
            "<option value=\"{}\"{selected}>{name}</option>",
            level.as_str()
        )
    })
    .collect();
    let news_choices: String = [
        (RecentNews::Collapsed, "Folded"),
        (RecentNews::Expanded, "Open"),
        (RecentNews::Off, "Off"),
    ]
    .into_iter()
    .map(|(view, name)| {
        let selected = if options.recent == view {
            " selected"
        } else {
            ""
        };
        format!(
            "<option value=\"{}\"{selected}>{name}</option>",
            view.as_str()
        )
    })
    .collect();
    let language = options.language.as_deref();
    let mut language_choices = format!(
        "<option value=\"\"{}>Any language</option>",
        if language.is_none() { " selected" } else { "" }
    );
    if let Some(code) = language.filter(|c| !LANGUAGE_CHOICES.iter().any(|(l, _)| l == c)) {
        let _ = write!(
            language_choices,
            "<option value=\"{0}\" selected>{0}</option>",
            escape_html(code)
        );
    }
    for (code, name) in LANGUAGE_CHOICES {
        let selected = if language == Some(*code) {
            " selected"
        } else {
            ""
        };
        let _ = write!(
            language_choices,
            "<option value=\"{code}\" lang=\"{code}\"{selected}>{name}</option>"
        );
    }
    let network = match settings.network {
        NetSetting::Unavailable => {
            "<label class=\"off\"><input type=\"checkbox\" disabled> Use the Plumb network \
             for search</label><p class=\"hint\">Not on this site: it has not joined the Plumb \
             network, so results come only from its own index.</p>"
        }
        NetSetting::Off | NetSetting::On => {
            if settings.network == NetSetting::On {
                "<label><input type=\"checkbox\" name=\"net\" value=\"1\" checked> Use the \
                 Plumb network for search</label>"
            } else {
                "<label><input type=\"checkbox\" name=\"net\" value=\"1\"> Use the Plumb \
                 network for search</label>"
            }
        }
    };
    let network_hint = match settings.network {
        NetSetting::Unavailable => String::new(),
        NetSetting::Off | NetSetting::On => format!(
            "<p class=\"hint\">Uses data from other Plumb nodes without sending query text. \
             Results only they found are tinted. Missing data may wait for a background \
             download.</p><p class=\"hint\">{} Set in this node's panel.</p>",
            settings.scope.explain()
        ),
    };
    let history = settings
        .history
        .as_ref()
        .map(history::HistoryView::settings_html)
        .unwrap_or_default();
    let private = if settings.private {
        private_toggle(false)
    } else {
        String::new()
    };
    format!(
        "<form action=\"/search\" method=\"get\" role=\"search\">\
         <input type=\"search\" name=\"q\" value=\"{}\" placeholder=\"A site's name, e.g. us bank\" \
         aria-label=\"Search\" autocomplete=\"off\"{}>\
         <details class=\"gear\"><summary title=\"Settings\" aria-label=\"Settings\">\
         &#9881;&#xFE0E;</summary><div class=\"panel\">\
         <label>Country <select name=\"country\">{choices}</select></label>\
         <label><input type=\"checkbox\" name=\"only\" value=\"1\"{}> Only this country</label>\
         <label>Language <select name=\"lang\">{language_choices}</select></label>\
         <label>Safe search <select name=\"safe\">{safe_choices}</select></label>\
         <label>Recent news <select name=\"news\">{news_choices}</select></label>\
         {network}{network_hint}{history}{private}<button type=\"submit\">Apply</button></div></details>\
         <button type=\"submit\">Search</button></form>",
        escape_html(query),
        if autofocus { " autofocus" } else { "" },
        if options.only_country { " checked" } else { "" }
    )
}

/// The gear's "Private search" switch, shown only where the node serves
/// private search. It is a link, not a form field, so turning it on never
/// sends what is typed in the search box: it opens `/private`, and turning
/// it off there goes back to normal search.
fn private_toggle(on: bool) -> String {
    let (href, checked) = if on {
        ("/", "true")
    } else {
        ("/private", "false")
    };
    format!(
        "<div class=\"pv\"><a class=\"tg\" href=\"{href}\" role=\"switch\" \
         aria-checked=\"{checked}\"><span class=\"knob\" aria-hidden=\"true\"></span>\
         Private search</a><p class=\"hint\">Your browser looks up the results itself, so \
         this site never sees what you search for. Needs JavaScript.</p></div>"
    )
}

/// The home page; a node's `status` adds what it is doing to the count of sites.
fn render_home(docs: u64, status: Option<&Status>, now: u64, settings: &Settings) -> String {
    let note = status
        .and_then(|status| node_note(status, now))
        .map(|note| format!(" &middot; {}", escape_html(&note)))
        .unwrap_or_default();
    let wikidata = status
        .and_then(|status| wikidata_note(status, now))
        .map(|note| format!("\n<p class=\"s\">{}</p>", escape_html(&note)))
        .unwrap_or_default();
    let recent = settings
        .history
        .as_ref()
        .map(|history| history.recent_html(&settings.options))
        .unwrap_or_default();
    let body = format!(
        "<main class=\"wrap home\">\n<h1>Plumb</h1>\n\
         {}{recent}\n<p class=\"s\">{} sites indexed{note}</p>{wikidata}\n\
         <p class=\"s\">Not looking for a site? Add !g, !ddg or !b to search Google, \
         DuckDuckGo or Bing.</p>\n</main>",
        settings_form("", true, settings),
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

/// The results page's header: the logo and the search form with its
/// settings gear.
fn results_form(query: &str, settings: &Settings) -> String {
    format!(
        "<header><a class=\"logo\" href=\"/\">Plumb</a>{}</header>",
        settings_form(query, false, settings)
    )
}

/// `path?q=...` with the searcher's choices, for links between pages.
fn search_link(path: &str, query: &str, options: &SearchOptions, net: bool) -> String {
    let mut params = url::form_urlencoded::Serializer::new(String::new());
    params.append_pair("q", query);
    if let Some(country) = &options.country {
        params.append_pair("country", country);
    }
    if options.only_country {
        params.append_pair("only", "1");
    }
    if net {
        params.append_pair("net", "1");
    }
    if options.exact {
        params.append_pair("exact", "1");
    }
    append_filters(&mut params, options);
    format!("{path}?{}", params.finish())
}

/// "Did you mean amazon?" above the results, which are for the query as
/// typed.
fn render_spelling(out: &mut String, spelling: &Spelling, options: &SearchOptions) {
    let fixed_options = SearchOptions {
        exact: false,
        ..options.clone()
    };
    let fixed = format!(
        "<a href=\"{}\"><strong>{}</strong></a>",
        escape_html(&search_link(
            "/search",
            &spelling.query,
            &fixed_options,
            false
        )),
        escape_html(&truncate_chars(&spelling.query, 150))
    );
    let _ = writeln!(out, "<p class=\"sp\">Did you mean {fixed}?</p>");
}

/// One result as shown: a hit, and what the network said about it when only
/// the network found it.
struct Shown<'a> {
    hit: Cow<'a, Hit>,
    network: Option<&'a NetworkResult>,
}

/// This node's hits and the network's, best score first, at most `limit`.
/// A site both found is shown once, as this node's: only sites this node
/// did not find count as from the network. Both lists are ranked with this
/// node's ranking and the same choices, and scores are normalized per
/// search, so they compare.
///
/// When the network holds a signed crawl of a site both found, that crawl
/// fills the title and description this node's copy lacks, and the site
/// keeps the better of the two scores: the network's text may match the
/// search where this node's copy, without text, barely does.
fn merge_results<'a>(local: &'a [Hit], network: &'a NetOutcome, limit: usize) -> Vec<Shown<'a>> {
    let signed: HashMap<&str, &NetworkResult> = match network {
        NetOutcome::Answered(results) => results
            .hits
            .iter()
            .filter(|result| result.shared.is_some())
            .map(|result| (result.hit.domain.as_str(), result))
            .collect(),
        _ => HashMap::new(),
    };
    let mut shown: Vec<Shown> = local
        .iter()
        .map(|hit| Shown {
            hit: match signed.get(hit.domain.as_str()) {
                Some(result) => Cow::Owned(fill_from_network(hit, result)),
                None => Cow::Borrowed(hit),
            },
            network: None,
        })
        .collect();
    if let NetOutcome::Answered(results) = network {
        let seen: HashSet<&str> = local.iter().map(|hit| hit.domain.as_str()).collect();
        shown.extend(
            results
                .hits
                .iter()
                .filter(|result| !seen.contains(result.hit.domain.as_str()))
                .map(|result| Shown {
                    hit: Cow::Borrowed(&result.hit),
                    network: Some(result),
                }),
        );
        // Stable, so ties keep this node's hits first.
        shown.sort_by(|a, b| b.hit.score.total_cmp(&a.hit.score));
    }
    shown.truncate(limit);
    shown
}

/// This node's `hit` with what the network's signed crawl of the site adds:
/// the title and description it lacks, and the network's score when better.
fn fill_from_network(hit: &Hit, result: &NetworkResult) -> Hit {
    let mut hit = hit.clone();
    let blank = |text: &Option<String>| text.as_deref().is_none_or(|t| t.trim().is_empty());
    if let Some(shared) = &result.shared {
        if blank(&hit.title) && !blank(&shared.title) {
            hit.title.clone_from(&shared.title);
        }
        if blank(&hit.description) && !blank(&shared.description) {
            hit.description.clone_from(&shared.description);
        }
        if hit.key_pages.is_empty() {
            hit.key_pages.clone_from(&shared.key_pages);
        }
    }
    hit.score = hit.score.max(result.hit.score);
    hit
}

/// The line above the results that says where they came from.
fn render_source(
    out: &mut String,
    query: &str,
    settings: &Settings,
    network: &NetOutcome,
    from_network: usize,
) {
    let line = match (settings.network, network) {
        (NetSetting::Unavailable, _) => "From this site's own index.".to_string(),
        (_, NetOutcome::NotAsked) => format!(
            "From this site's own index. <a href=\"{}\">Use the Plumb network too</a>",
            escape_html(&search_link("/search", query, &settings.options, true))
        ),
        (_, NetOutcome::Failed) => {
            "From this site's own index: the Plumb network did not answer this time.".to_string()
        }
        (_, NetOutcome::Answered(results)) if results.pending > 0 => {
            let mut line = "From this site's index and any saved Plumb results. More results \
                            are waiting for the next background download; search again later."
                .to_string();
            if results.stale > 0 {
                line.push_str(" Some saved results may be out of date.");
            }
            if from_network > 0 {
                line.push_str(
                    " <span class=\"sw\"></span>Tinted results came from saved Plumb data.",
                );
            }
            line
        }
        (_, NetOutcome::Answered(results)) if results.asked == 0 && results.cached > 0 => {
            let mut line =
                "From this site's index and saved Plumb results, read locally.".to_string();
            if results.stale > 0 {
                line.push_str(
                    " Some saved results may be out of date and will refresh in the background.",
                );
            }
            if from_network > 0 {
                line.push_str(
                    " <span class=\"sw\"></span>Tinted results came only from the network.",
                );
            }
            line
        }
        (_, NetOutcome::Answered(results)) if results.asked == 0 => {
            "From this site's own index: none of the Plumb nodes it searches are connected right \
             now."
                .to_string()
        }
        (_, NetOutcome::Answered(results)) => {
            let mut line = format!(
                "From this site's index and the Plumb network: {} of {} requests to other nodes \
                 answered, without sending them your search.",
                results.answered, results.asked
            );
            if results.rejected > 0 {
                let _ = write!(
                    line,
                    " {} answers were dropped because their proofs did not check out.",
                    results.rejected
                );
            }
            if results.cached > 0 {
                line.push_str(" Some results came from saved Plumb data.");
            }
            if from_network > 0 {
                line.push_str(
                    " <span class=\"sw\"></span>Tinted results came only from the network.",
                );
            }
            line
        }
    };
    let _ = writeln!(out, "<p class=\"src\">{line}</p>");
}

/// [`render_results_with`] without a "Recent" block.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn render_results(
    query: &str,
    results: &SearchResults,
    extras: Option<&answers::Extras>,
    network: &NetOutcome,
    settings: &Settings,
    web_search: Option<Engine>,
    limit: usize,
    share_picks: bool,
    icons: &Icons,
) -> String {
    render_results_with(
        query,
        results,
        extras,
        network,
        settings,
        web_search,
        limit,
        share_picks,
        icons,
        None,
    )
}

/// [`render_results`] with a "Recent" block after the first result.
#[allow(clippy::too_many_arguments)]
fn render_results_with(
    query: &str,
    results: &SearchResults,
    extras: Option<&answers::Extras>,
    network: &NetOutcome,
    settings: &Settings,
    web_search: Option<Engine>,
    limit: usize,
    share_picks: bool,
    icons: &Icons,
    recent: Option<&Recent>,
) -> String {
    let shown = merge_results(&results.hits, network, limit);
    let from_network = shown.iter().filter(|s| s.network.is_some()).count();
    let mut body = String::from("<main>\n");
    if let Some(profile) = extras.and_then(|e| e.profile.as_ref()) {
        let domain = plumb_core::registrable_domain(&profile.url).unwrap_or_default();
        answers::render_profile(&mut body, profile, icons.get(&domain));
    }
    if let Some(answer) = extras.and_then(|e| e.answer.as_ref()) {
        answers::render_answer(&mut body, answer);
    }
    render_source(&mut body, query, settings, network, from_network);
    if let Some(spelling) = &results.spelling {
        render_spelling(&mut body, spelling, &settings.options);
    }
    // Picks are noted for the query: shared, or kept in the searcher's
    // history.
    let notes_picks = share_picks
        || settings
            .history
            .as_ref()
            .is_some_and(|history| history.prefs.on());
    if let Some(site_search) = &results.site_search {
        render_site_search(&mut body, site_search);
    }
    if let Some(engine) = web_search {
        let _ = writeln!(
            body,
            "<p class=\"web\"><a href=\"{}\" rel=\"noreferrer\">Search the web with {} for <strong>{}</strong></a></p>",
            escape_html(&engine.url(query)),
            escape_html(engine.name()),
            escape_html(&truncate_chars(query, 150))
        );
    }
    let shown_hits: Vec<Hit> = shown
        .iter()
        .map(|item| item.hit.clone().into_owned())
        .collect();
    let found_pages = results
        .pages
        .iter()
        .map(|placed| placed.hit.clone())
        .collect();
    let ops = plumb_core::Operators::parse(query);
    let pages = if ops.any() {
        place_operator_pages(&ops, &shown_hits, found_pages)
    } else {
        place_pages(query, &shown_hits, found_pages)
    };
    // An info box is about what the whole query names, which operators
    // ("site:", "-word") change.
    let info = if let Some(profile) = extras.and_then(|e| e.profile.as_ref()) {
        answers::info_from_page(&profile.page, &shown_hits)
    } else if ops.any() {
        None
    } else {
        answers::info_box(&shown_hits, &pages)
    };
    let shown_count = shown.len();
    let pages = &pages;
    let listed_pages = move |at: usize| {
        pages.iter().filter(move |p| {
            p.under.is_none() && (p.at == at || (at == usize::MAX && p.at >= shown_count))
        })
    };
    let news = recent
        .filter(|_| settings.options.recent != RecentNews::Off)
        .map(|recent| render_recent(recent, settings.options.recent, now_unix()));
    let now = now_unix();
    let from_plugins: String = extras
        .map(|e| e.plugins.iter().map(|p| render_plugin(p, now)).collect())
        .unwrap_or_default();
    let news = match (news, from_plugins.is_empty()) {
        (news, true) => news,
        (news, false) => Some(news.unwrap_or_default() + &from_plugins),
    };
    if shown.is_empty() && pages.is_empty() {
        let _ = writeln!(
            body,
            "<p class=\"none\">No sites match <strong>{}</strong>.</p>",
            escape_html(query)
        );
        if let Some(news) = &news {
            let _ = writeln!(body, "<ol>\n{news}</ol>");
        }
    } else {
        body.push_str("<ol>\n");
        for (position, item) in shown.iter().enumerate() {
            for page in listed_pages(position) {
                render_page(&mut body, &page.hit, icons.get(page.hit.page.set_domain()));
            }
            // `/go` only follows this node's own results, so sites from other
            // nodes link straight to themselves.
            let go = (notes_picks && item.network.is_none())
                .then(|| go_link(query, &settings.options, &item.hit.domain));
            let icon = icons.get(&item.hit.domain);
            let notes = settings
                .history
                .as_ref()
                .map(|history| history.notes(&item.hit))
                .unwrap_or_default();
            let mut rendered = String::new();
            render_hit(
                &mut rendered,
                &item.hit,
                item.network,
                go.as_deref(),
                icon,
                &notes,
            );
            let carried = pages
                .iter()
                .find(|p| p.under.as_deref() == Some(item.hit.domain.as_str()));
            if let Some(page) = carried {
                if let Some(end) = rendered.rfind("</li>") {
                    rendered.insert_str(end, &page_line(&page.hit));
                }
            }
            let product = carried.and_then(|page| product_of(&page.hit, &item.hit.domain, query));
            if position == 0 || product.is_some() {
                if let Some(end) = rendered.rfind("<div class=\"m\">") {
                    let line = key_pages_line(&item.hit, query, product.as_ref());
                    rendered.insert_str(end, &line);
                }
            }
            body.push_str(&rendered);
            if position == 0 {
                if let Some(news) = &news {
                    body.push_str(news);
                }
            }
        }
        if shown.is_empty() {
            if let Some(news) = &news {
                body.push_str(news);
            }
        }
        for page in listed_pages(usize::MAX) {
            render_page(&mut body, &page.hit, icons.get(page.hit.page.set_domain()));
        }
        body.push_str("</ol>\n");
    }
    if share_picks {
        body.push_str(
            "<p class=\"s\">This node shares which result is opened for a search: it notes \
             the pick here and reports it to other Plumb nodes, encrypted so that no node can \
             read it until many nodes report the same pick, and never with who made it.</p>\n",
        );
    }
    let options = &settings.options;
    let api = escape_html(&search_link("/api/search", query, options, false));
    let mut json = format!("<a href=\"{api}\">{api}</a>");
    if matches!(network, NetOutcome::Answered(_)) {
        let api = escape_html(&search_link("/api/network/search", query, options, false));
        let _ = write!(json, " and <a href=\"{api}\">{api}</a>");
    }
    let _ = write!(body, "<p class=\"s\">As JSON: {json}</p>\n</main>\n");
    // With an info box the page is wider, with the box beside the results
    // (above them on a narrow screen, unless a profile asked for leads).
    let form = results_form(query, settings);
    let body = match &info {
        Some(info) => {
            let mut aside = String::new();
            answers::render_info_box(&mut aside, info);
            let cols = if extras.is_some_and(|e| e.profile.is_some()) {
                "cols pfirst"
            } else {
                "cols"
            };
            format!("<div class=\"wrap wide\">\n{form}\n<div class=\"{cols}\">\n{body}{aside}</div>\n</div>")
        }
        None => format!("<div class=\"wrap\">\n{form}\n{body}</div>"),
    };
    page(&format!("{query} - Plumb Search"), &body)
}

/// What other nodes answered, and nothing from this node. Their text is as
/// untrusted as any record's, and is escaped the same way.
fn render_network(query: &str, results: &NetworkResults, icons: &Icons) -> String {
    let mut body = format!("<div class=\"wrap\">\n{}\n<main>\n", results_header(query));
    if results.asked == 0 && (results.cached > 0 || results.pending > 0) {
        body.push_str(
            "<p class=\"s\">Plumb results are read from saved data and ranked on this node.</p>\n",
        );
    } else {
        let _ = writeln!(
            body,
            "<p class=\"s\">From the Plumb network, without sending your search: {} buckets of \
         sites asked of other nodes under throwaway identities, {} of {} requests answered{}{}. \
         Ranked on this node.</p>",
            results.buckets,
            results.answered,
            results.asked,
            if results.direct > 0 {
                format!(
                    "; {} sent straight to a node, which saw this node's address, because no \
                 other node could pass them on",
                    results.direct
                )
            } else if results.asked > 0 {
                "; each through another node, so the node answering never saw this node's \
             address"
                    .to_string()
            } else {
                String::new()
            },
            if results.rejected > 0 {
                format!(
                    "; {} answers were dropped because their proofs did not check out",
                    results.rejected
                )
            } else {
                String::new()
            }
        );
    }
    if results.cached > 0 {
        let _ = writeln!(
            body,
            "<p class=\"s\">{} of this search's buckets came from saved Plumb data, so the \
             network was not asked for them again.</p>",
            results.cached
        );
    }
    if results.pending > 0 {
        body.push_str(
            "<p class=\"s\">More results are waiting for the next background download. \
             Search again later.</p>\n",
        );
    }
    if results.stale > 0 {
        body.push_str(
            "<p class=\"s\">Some saved results may be out of date and will refresh \
             in the background.</p>\n",
        );
    }
    if results.asked == 0 && results.cached == 0 && results.pending == 0 {
        body.push_str(
            "<p class=\"none\">None of the nodes this node searches are connected yet. It \
             keeps looking for them.</p>\n",
        );
    } else if results.hits.is_empty() && results.pending == 0 {
        let _ = writeln!(
            body,
            "<p class=\"none\">No node had a site matching <strong>{}</strong>.</p>",
            escape_html(query)
        );
    } else if !results.hits.is_empty() {
        body.push_str("<ol>\n");
        for result in &results.hits {
            let icon = icons.get(&result.hit.domain);
            render_hit(&mut body, &result.hit, Some(result), None, icon, &[]);
        }
        body.push_str("</ol>\n");
    }
    let encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let local = escape_html(&format!("/search?q={encoded}"));
    let api = escape_html(&format!("/api/network/search?q={encoded}"));
    let _ = write!(
        body,
        "<p class=\"s\"><a href=\"{local}\">Back to this node's results</a> &middot; \
         As JSON: <a href=\"{api}\">{api}</a></p>\n</main>\n</div>"
    );
    page(&format!("{query} - Plumb network"), &body)
}

fn render_no_network() -> String {
    let body = format!(
        "<div class=\"wrap\">\n{}\n<main>\n<p class=\"none\">This node has not joined the \
         Plumb network. Start it with <code>plumb run --network</code> to search other \
         nodes.</p>\n</main>\n</div>",
        results_header("")
    );
    page("Plumb network", &body)
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

/// The `/go` link that notes a pick of `domain` for `query`.
fn go_link(query: &str, options: &SearchOptions, domain: &str) -> String {
    let mut link = url::form_urlencoded::Serializer::new(String::new());
    link.append_pair("q", query);
    link.append_pair("d", domain);
    link.append_pair("country", options.country.as_deref().unwrap_or("any"));
    if options.only_country {
        link.append_pair("only", "1");
    }
    if options.exact {
        link.append_pair("exact", "1");
    }
    append_filters(&mut link, options);
    format!("/go?{}", link.finish())
}

/// Adds safe search, when not the default, and the language filter to a
/// link's parameters.
fn append_filters(params: &mut url::form_urlencoded::Serializer<String>, options: &SearchOptions) {
    if options.safe != SafeSearch::default() {
        params.append_pair("safe", options.safe.as_str());
    }
    if let Some(language) = &options.language {
        params.append_pair("lang", language);
    }
    if options.recent != RecentNews::default() {
        params.append_pair("news", options.recent.as_str());
    }
}

/// The round badge before a result: the site's icon, or else the first
/// letter of its name on a color of its own. `icon` is a `data:` URL.
fn site_badge(domain: &str, icon: Option<&str>) -> String {
    match icon {
        Some(icon) => format!(
            "<span class=\"ic\"><img src=\"{}\" alt=\"\" width=\"18\" height=\"18\"></span>",
            escape_html(icon)
        ),
        None => {
            let (letter, color) = site_initial(domain);
            format!(
                "<span class=\"ic l{color}\" aria-hidden=\"true\">{}</span>",
                escape_html(&letter.to_string())
            )
        }
    }
}

/// One result: the site's badge, domain and address above its name, then
/// its description. A site that came from other nodes (`network`) is
/// tinted and says so. `go` is the `/go` link to send the click through
/// instead of linking to the site directly; `icon` is the site's icon as a
/// `data:` URL.
/// A single page (a Wikipedia article, a GitHub repository) listed among
/// the sites, with its set's icon.
fn render_page(out: &mut String, hit: &PageHit, icon: Option<&str>) {
    let Some(href) = http_url(&hit.page.url) else {
        return;
    };
    let badge = site_badge(hit.page.set_domain(), icon);
    let _ = write!(
        out,
        "<li class=\"pg\"><a class=\"r\" href=\"{}\" rel=\"noreferrer\"><span class=\"site\">{badge}\
         <span class=\"sn\"><span class=\"dn\">{}</span><span class=\"u\">{}</span></span></span>\
         <span class=\"t\">{}</span></a>",
        escape_html(&href),
        escape_html(hit.page.set_name()),
        escape_html(&display_url(&href)),
        escape_html(&truncate_chars(&hit.page.title, 150)),
    );
    if let Some(description) = hit
        .page
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
    {
        let _ = write!(out, "<p class=\"d\">{}</p>", escape_html(description));
    }
    if let Some(package) = &hit.page.package {
        render_package(out, package);
    }
    let _ = writeln!(
        out,
        "<div class=\"m\"><span title=\"{} {}\">score {:.3}</span></div></li>",
        hit.page.views,
        match hit.page.set.as_str() {
            plumb_index::pages::GITHUB_SET => "stars",
            plumb_index::pages::BOOKS_SET => "readers",
            plumb_index::pages::PAPERS_SET => "citations",
            plumb_index::pages::PACKAGES_SET => "use (share of the registry's most, in billionths)",
            _ => "views",
        },
        hit.score
    );
}

/// A package's card under its result: its latest version, the command
/// that installs it, and links to its docs and code.
fn render_package(out: &mut String, package: &plumb_core::packages::PackageInfo) {
    let mut parts: Vec<String> = Vec::new();
    if let Some(version) = &package.version {
        let mut latest = format!("Latest {}", escape_html(version));
        if let Some(released) = &package.released {
            let _ = write!(latest, " ({})", escape_html(released));
        }
        parts.push(latest);
    }
    if let Some(license) = &package.license {
        parts.push(escape_html(license));
    }
    if let Some(install) = package.install() {
        parts.push(format!("<code>{}</code>", escape_html(&install)));
    }
    for (label, url) in [
        ("Docs", package.docs()),
        ("Code", package.repo.clone()),
        ("Home", package.homepage.clone()),
    ] {
        if let Some(href) = url.as_deref().and_then(http_url) {
            parts.push(format!(
                "<a href=\"{}\" rel=\"noreferrer\">{label}</a>",
                escape_html(&href)
            ));
        }
    }
    if !parts.is_empty() {
        let _ = write!(out, "<p class=\"d pk\">{}</p>", parts.join(" &middot; "));
    }
}

/// The "Recent" block, as an item of the results list: the latest posts
/// of the site the query names, or recent headlines about its words, each
/// with its site and age, folded behind a one-line summary unless `view`
/// says open (no script needed: `<details>`). Feed text is as untrusted
/// as any record's, and is escaped the same way.
fn render_recent(recent: &Recent, view: RecentNews, now: u64) -> String {
    let heading = match &recent.site {
        Some(site) => format!("Latest from {}", escape_html(site)),
        None => "Recent".to_string(),
    };
    let mut items = String::new();
    let mut shown = 0;
    for headline in &recent.headlines {
        let Some(href) = http_url(&headline.url) else {
            continue;
        };
        shown += 1;
        let _ = write!(
            items,
            "<li><a href=\"{}\" rel=\"noreferrer\">{}</a><div class=\"m\">{} &middot; {}</div></li>",
            escape_html(&href),
            escape_html(&truncate_chars(&headline.title, 150)),
            escape_html(&headline.domain),
            time_ago(headline.at, now)
        );
    }
    let newest = recent.headlines.iter().map(|h| h.at).max().unwrap_or(now);
    let count = if shown == 1 {
        "1 headline".to_string()
    } else {
        format!("{shown} headlines")
    };
    format!(
        "<li class=\"news\"><details{}><summary><span class=\"nh\">{heading}</span> \
         <span class=\"m\">{count}, newest {}</span></summary><ol>{items}</ol></details></li>\n",
        if view == RecentNews::Expanded {
            " open"
        } else {
            ""
        },
        time_ago(newest, now)
    )
}

/// What one of the node's plugins found, as an item of the results
/// list after the first result, named for the plugin so that nobody
/// takes it for Plumb's own. Plugin text is as untrusted as any record's,
/// and is escaped the same way.
fn render_plugin(found: &PluginResults, now: u64) -> String {
    let mut items = String::new();
    for item in &found.results {
        let Some(href) = http_url(&item.url) else {
            continue;
        };
        let mut meta = escape_html(&item.site);
        if let Some(at) = item.published {
            let _ = write!(meta, " &middot; {}", time_ago(at, now));
        }
        let snippet = item
            .snippet
            .as_deref()
            .map(|s| format!("<p class=\"d\">{}</p>", escape_html(s)))
            .unwrap_or_default();
        let _ = write!(
            items,
            "<li><a href=\"{}\" rel=\"noreferrer\">{}</a>{snippet}<div class=\"m\">{meta}</div></li>",
            escape_html(&href),
            escape_html(&item.title),
        );
    }
    if items.is_empty() {
        return String::new();
    }
    format!(
        "<li class=\"news plugin\"><p class=\"nh\">From {} <span class=\"m\">plugin on this node</span></p>\
         <ol>{items}</ol></li>\n",
        escape_html(&found.name)
    )
}

/// Most key pages listed under a result.
const SHOWN_KEY_PAGES: usize = 6;

/// The part of the site `domain` that the query names, from the article
/// carried under its result: YouTube Music's `https://music.youtube.com/`
/// for "youtube music" or "music youtube", when that article's item has
/// it as its official website. The query must have the words of the
/// article's title or one of its other names, in any order.
fn product_of(page: &PageHit, domain: &str, query: &str) -> Option<KeyPage> {
    let website = page.page.website.as_deref()?;
    let href = http_url(website)?;
    if plumb_core::registrable_domain(&href)? != domain {
        return None;
    }
    let words = |text: &str| {
        let mut words: Vec<String> = plumb_core::normalize_text(text)
            .split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect();
        words.sort_unstable();
        words
    };
    let asked = words(query);
    let title = page.page.title.as_str();
    let base = match title.rfind(" (") {
        Some(i) if title.ends_with(')') && i > 0 => &title[..i],
        _ => title,
    };
    let named = std::iter::once(base)
        .chain(page.page.aliases.iter().map(String::as_str))
        .any(|name| !asked.is_empty() && words(name) == asked);
    named.then(|| KeyPage {
        label: base.to_string(),
        url: href,
    })
}

/// The site's key pages (sign in, docs, pricing) under the top result,
/// when the query names that site: on its own ("paypal"), or followed by
/// what is wanted from it ("paypal login"), whose page then comes first,
/// in bold. A part of the site the query names (`product`, from
/// [`product_of`]) comes first, in bold, under any result. Empty
/// otherwise, and when the site has fewer than two key pages and none for
/// what was asked.
fn key_pages_line(hit: &Hit, query: &str, product: Option<&KeyPage>) -> String {
    if !hit.named && product.is_none() {
        return String::new();
    }
    let mut pages: Vec<(&KeyPage, String)> = hit
        .key_pages
        .iter()
        .filter(|_| hit.named)
        .filter(|page| page.is_valid_for(&hit.domain))
        .filter_map(|page| Some((page, http_url(&page.url)?)))
        .take(SHOWN_KEY_PAGES)
        .collect();
    let wanted = PageIntent::of_query_end(query).map(|(intent, _)| intent);
    let mut matched = wanted.and_then(|wanted| {
        pages
            .iter()
            .position(|(page, _)| page.intent() == Some(wanted))
    });
    if let Some(at) = matched {
        let page = pages.remove(at);
        pages.insert(0, page);
    }
    if let Some(product) = product {
        pages.retain(|(_, href)| *href != product.url);
        pages.insert(0, (product, product.url.clone()));
        pages.truncate(SHOWN_KEY_PAGES);
        matched = Some(0);
    }
    if pages.len() < 2 && matched.is_none() {
        return String::new();
    }
    let mut line = String::from("<ul class=\"kp\">");
    for (i, (page, href)) in pages.iter().enumerate() {
        let label = escape_html(&truncate_chars(&page.label, 40));
        let label = if i == 0 && matched.is_some() {
            format!("<strong>{label}</strong>")
        } else {
            label
        };
        let _ = write!(
            line,
            "<li><a href=\"{}\" rel=\"noreferrer\">{label}</a></li>",
            escape_html(href)
        );
    }
    line.push_str("</ul>");
    line
}

/// "Wikipedia: Python (programming language)", under the result for the
/// site the page is about.
fn page_line(hit: &PageHit) -> String {
    let Some(href) = http_url(&hit.page.url) else {
        return String::new();
    };
    let description = hit
        .page
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .map(|d| format!(" &middot; {}", escape_html(d)))
        .unwrap_or_default();
    format!(
        "<p class=\"sub\">{}: <a href=\"{}\" rel=\"noreferrer\">{}</a>{description}</p>",
        escape_html(hit.page.set_name()),
        escape_html(&href),
        escape_html(&truncate_chars(&hit.page.title, 150)),
    )
}

fn render_hit(
    out: &mut String,
    hit: &Hit,
    network: Option<&NetworkResult>,
    go: Option<&str>,
    icon: Option<&str>,
    notes: &[String],
) {
    let name = hit
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(&hit.domain);
    let name = escape_html(&truncate_chars(name, 150));
    out.push_str(if network.is_some() {
        "<li class=\"net\">"
    } else {
        "<li>"
    });
    let badge = site_badge(&hit.domain, icon);
    let domain = escape_html(&hit.domain);
    match safe_href(hit) {
        Some(href) => {
            // The address, unless it says no more than the domain.
            let shown = display_url(&href);
            let url = if shown == hit.domain {
                String::new()
            } else {
                format!("<span class=\"u\">{}</span>", escape_html(&shown))
            };
            let _ = write!(
                out,
                "<a class=\"r\" href=\"{}\" rel=\"noreferrer\"><span class=\"site\">{badge}\
                 <span class=\"sn\"><span class=\"dn\">{domain}</span>{url}</span></span>\
                 <span class=\"t\">{name}</span></a>",
                escape_html(go.unwrap_or(&href)),
            );
        }
        None => {
            let _ = write!(
                out,
                "<div class=\"r\"><span class=\"site\">{badge}<span class=\"sn\">\
                 <span class=\"dn\">{domain}</span></span></span><span class=\"t\">{name}</span></div>"
            );
        }
    }
    if let Some(description) = hit.description.as_deref().filter(|d| !d.trim().is_empty()) {
        let _ = write!(out, "<p class=\"d\">{}</p>", escape_html(description));
    }
    // Already HTML.
    let mut meta: Vec<String> = notes.to_vec();
    if let Some(code) = hit.country.as_deref() {
        meta.push(escape_html(country_name(code)));
    }
    if let Some(result) = network {
        let crawl = if result.confirmed {
            "signed crawls, two nodes agree"
        } else if result.verified {
            "signed crawl, checked"
        } else {
            "unsigned (seed data)"
        };
        let answers = if result.answers == 1 {
            "1 answer".to_string()
        } else {
            format!("{} answers", result.answers)
        };
        meta.push(format!("from the Plumb network ({crawl}, in {answers})"));
    }
    meta.push(format!(
        "<span title=\"text {:.3}, link {:.3}\">score {:.3}</span>",
        hit.text_score, hit.link_score, hit.score
    ));
    let _ = writeln!(
        out,
        "<div class=\"m\">{}</div></li>",
        meta.join(" &middot; ")
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
            named: false,
            official: false,
            key_pages: Vec::new(),
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

    #[tokio::test]
    async fn plugin_results_show_on_the_page_and_in_json_but_only_when_asked() {
        let plugins = crate::plugins::answering_plugins(
            "Test News",
            "tn",
            r#"{"results":[{"title":"Story <b>","url":"https://news.example.com/1","snippet":"Hot take"}]}"#,
        );
        let app = router_with(
            backend(bank_hits()),
            WebSettings {
                home: HomeCountry::Off,
                plugins,
                ..WebSettings::default()
            },
        );
        let (_, _, page) = send(app.clone(), "/search?q=tn+us+bank").await;
        let block = page
            .find("<li class=\"news plugin\">")
            .expect("the plugin's block");
        assert!(
            page[..block].contains("usbank.com"),
            "after the first result"
        );
        assert!(page[block..].contains("From Test News"));
        assert!(page[block..].contains("Story &lt;b&gt;"));
        assert!(page[block..].contains("href=\"https://news.example.com/1\""));
        // Its keyword picks it.
        let (_, _, page) = send(app.clone(), "/search?q=us+bank").await;
        assert!(!page.contains("From Test News"));

        let (_, _, body) = send(app.clone(), "/api/search?q=tn+us+bank&full=1").await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["plugins"][0]["name"], "Test News");
        assert_eq!(json["plugins"][0]["results"][0]["site"], "example.com");

        let (_, _, body) = send(app, "/search?q=tn+us+bank&format=json").await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let results = json["results"].as_array().unwrap();
        assert_eq!(results[1]["url"], "https://news.example.com/1");
        assert_eq!(results[1]["engine"], "test-news");
        assert_eq!(results[0]["engine"], "plumb");
    }

    #[tokio::test]
    async fn answers_searxng_json_for_ai_apps() {
        let app = router_with(backend(bank_hits()), HomeCountry::Off);
        let (status, headers, body) = send(
            app.clone(),
            "/search?q=us+bank&format=json&pageno=1&safesearch=0",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json"));
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["query"], "us bank");
        assert_eq!(body["number_of_results"], 2);
        let first = &body["results"][0];
        assert_eq!(first["url"], "https://www.usbank.com/");
        assert_eq!(first["title"], "U.S. Bank | Personal & Business Banking");
        assert_eq!(first["content"], "Checking, savings & loans.");
        assert_eq!(first["engine"], "plumb");
        assert_eq!(first["parsed_url"][1], "www.usbank.com");
        // A site without a title goes by its domain.
        assert_eq!(body["results"][1]["title"], "usbank-login-help.com");

        // The second page.
        let (_, _, body) = send(
            app.clone(),
            "/search?q=us+bank&format=json&pageno=2&limit=1",
        )
        .await;
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["results"].as_array().unwrap().len(), 1);
        assert_eq!(body["results"][0]["url"], "https://usbank-login-help.com/");

        // Instant answers go where SearXNG puts its own.
        let (_, _, body) = send(app.clone(), "/search?q=12*7&format=json").await;
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["answers"][0]["answer"], "12 × 7 = 84");
        // And leads the first snippet, for apps that read only snippets.
        assert!(body["results"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("Answer: 12 × 7 = 84. Checking"));

        // News only: this server keeps no headlines.
        let (_, _, body) = send(app.clone(), "/search?q=us+bank&format=json&categories=news").await;
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["results"], serde_json::json!([]));

        // Open WebUI's external search engine.
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/websearch")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"us bank","count":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body,
            serde_json::json!([{
                "link": "https://www.usbank.com/",
                "title": "U.S. Bank | Personal & Business Banking",
                "snippet": "Checking, savings & loans.",
            }])
        );

        // A bang stays a search: an AI app wants results, not a redirect.
        let (status, _, _) = send(app, "/search?q=!g+rust&format=json").await;
        assert_eq!(status, StatusCode::OK);
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
        let (status, _, body) = send(
            router_with(fake.clone(), HomeCountry::Off),
            "/search?q=us+bank",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(
                "<a class=\"r\" href=\"https://www.usbank.com/\" rel=\"noreferrer\">\
                 <span class=\"site\"><span class=\"ic l"
            ),
            "{body}"
        );
        assert!(body.contains(
            "<span class=\"dn\">usbank.com</span><span class=\"u\">www.usbank.com</span>\
             </span></span><span class=\"t\">U.S. Bank | Personal &amp; Business Banking</span></a>"
        ));
        assert!(body.contains("<p class=\"d\">Checking, savings &amp; loans.</p>"));
        // A hit without a title is shown by its domain.
        assert!(body.contains("<span class=\"t\">usbank-login-help.com</span></a>"));
        // Without an icon, a site gets its first letter.
        assert!(body.contains("aria-hidden=\"true\">U</span>"));
        assert!(body.contains("value=\"us bank\""));
        assert!(body.contains("href=\"/api/search?q=us+bank\""));
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec![("us bank".to_string(), DEFAULT_LIMIT)]
        );
    }

    #[tokio::test]
    async fn the_site_searched_for_lists_its_key_pages() {
        let page = |label: &str, url: &str| KeyPage {
            label: label.into(),
            url: url.into(),
        };
        let mut paypal = hit(
            "paypal.com",
            "https://www.paypal.com/",
            Some("PayPal"),
            None,
        );
        paypal.named = true;
        paypal.key_pages = vec![
            page("Sign Up", "https://www.paypal.com/signup"),
            page("Log In", "https://www.paypal.com/signin"),
            page("Help", "https://www.paypal.com/help"),
            page("Phish", "https://paypal-login.example/"),
        ];
        let mut other = hit(
            "paypal-login.example",
            "https://paypal-login.example/",
            None,
            None,
        );
        other.key_pages = paypal.key_pages.clone();
        let fake = backend(vec![paypal.clone(), other]);
        let search = |q: &'static str| {
            let fake = fake.clone();
            async move { send(router_with(fake, HomeCountry::Off), q).await.2 }
        };

        let body = search("/search?q=paypal").await;
        assert!(
            body.contains(
                "<ul class=\"kp\"><li><a href=\"https://www.paypal.com/signup\" \
                 rel=\"noreferrer\">Sign Up</a></li><li><a href=\"https://www.paypal.com/signin\" \
                 rel=\"noreferrer\">Log In</a></li><li><a href=\"https://www.paypal.com/help\" \
                 rel=\"noreferrer\">Help</a></li></ul>"
            ),
            "{body}"
        );
        assert_eq!(
            body.matches("class=\"kp\"").count(),
            1,
            "only the top result"
        );
        assert!(!body.contains("paypal-login.example/\" rel=\"noreferrer\">Phish"));

        // What the query asks for comes first.
        let body = search("/search?q=paypal+login").await;
        assert!(body.contains(
            "<ul class=\"kp\"><li><a href=\"https://www.paypal.com/signin\" \
             rel=\"noreferrer\"><strong>Log In</strong></a></li>"
        ));

        // Not for a top result the query does not name.
        paypal.named = false;
        let body = send(
            router_with(backend(vec![paypal]), HomeCountry::Off),
            "/search?q=pay",
        )
        .await
        .2;
        assert!(!body.contains("class=\"kp\""));
    }

    #[test]
    fn the_part_of_a_site_the_query_names_comes_first() {
        use plumb_index::pages::Page;
        let article = PageHit {
            page: Page {
                set: "wikipedia-en".into(),
                url: "https://en.wikipedia.org/wiki/YouTube_Music".into(),
                title: "YouTube Music".into(),
                description: Some("Music streaming service".into()),
                site: Some("youtube.com".into()),
                views: 1000,
                aliases: vec!["YT Music".into()],
                item: Some("Q28404534".into()),
                profiles: Vec::new(),
                website: Some("https://music.youtube.com/".into()),
                package: None,
            },
            score: 1.0,
            named: true,
            popularity: 0.5,
            whole: false,
        };
        let youtube = hit(
            "youtube.com",
            "https://www.youtube.com/",
            Some("YouTube"),
            None,
        );
        for query in ["youtube music", "Music YouTube", "yt music"] {
            let product = product_of(&article, "youtube.com", query).unwrap();
            assert_eq!(product.url, "https://music.youtube.com/");
            // Under any result, named in full or not.
            assert_eq!(
                key_pages_line(&youtube, query, Some(&product)),
                "<ul class=\"kp\"><li><a href=\"https://music.youtube.com/\" \
                 rel=\"noreferrer\"><strong>YouTube Music</strong></a></li></ul>"
            );
        }
        // Not for other words, nor on another site.
        assert!(product_of(&article, "youtube.com", "youtube").is_none());
        assert!(product_of(&article, "youtube.com", "youtube music charts").is_none());
        assert!(product_of(&article, "you-tubemusic.com", "youtube music").is_none());
        let mut elsewhere = article.clone();
        elsewhere.page.website = Some("https://youtubemusic.example/".into());
        assert!(product_of(&elsewhere, "youtube.com", "youtube music").is_none());
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
            network: None,
            fill: None,
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

    /// A ready node that shares popularity and remembers the picks.
    struct SharingNode {
        status: Status,
        picks: Mutex<Vec<(String, String)>>,
    }

    impl StatusSource for SharingNode {
        fn status(&self) -> Status {
            self.status.clone()
        }
        fn shares_popularity(&self) -> bool {
            true
        }
        fn record_pick(&self, query: &str, domain: &str) {
            self.picks
                .lock()
                .unwrap()
                .push((query.to_string(), domain.to_string()));
        }
    }

    #[tokio::test]
    async fn a_node_sharing_popularity_notes_the_result_opened_and_says_so() {
        let fake = backend(bank_hits());
        let node = Arc::new(SharingNode {
            status: node_status(Phase::Ready, Step::Idle),
            picks: Mutex::new(Vec::new()),
        });
        let app = || node_router(fake.clone(), node.clone());
        let (code, _, body) = send(app(), "/search?q=us+bank&country=any").await;
        assert_eq!(code, StatusCode::OK);
        assert!(
            body.contains("href=\"/go?q=us+bank&amp;d=usbank.com&amp;country=any\""),
            "{body}"
        );
        // The address shown is still the site's own.
        assert!(
            body.contains("<span class=\"u\">www.usbank.com</span>"),
            "{body}"
        );
        assert!(
            body.contains("This node shares which result is opened"),
            "{body}"
        );

        let (code, headers, _) = send(app(), "/go?q=us+bank&d=usbank.com&country=any").await;
        assert_eq!(code, StatusCode::SEE_OTHER);
        assert_eq!(headers[header::LOCATION], "https://www.usbank.com/");
        assert_eq!(
            *node.picks.lock().unwrap(),
            vec![("us bank".to_string(), "usbank.com".to_string())]
        );

        // Only to a site the search returns: anything else goes back.
        for uri in [
            "/go?q=us+bank&d=evil.example",
            "/go?q=&d=usbank.com",
            "/go?d=usbank.com",
        ] {
            let (code, headers, _) = send(app(), uri).await;
            assert_eq!(code, StatusCode::SEE_OTHER, "{uri}");
            assert!(
                headers[header::LOCATION]
                    .to_str()
                    .unwrap()
                    .starts_with("/search?q="),
                "{uri}"
            );
        }
        assert_eq!(node.picks.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_node_not_sharing_popularity_links_straight_to_sites() {
        let fake = backend(bank_hits());
        let node = node(node_status(Phase::Ready, Step::Idle));
        let (_, _, body) = send(node_router(fake, node), "/search?q=us+bank").await;
        assert!(body.contains("href=\"https://www.usbank.com/\""), "{body}");
        assert!(!body.contains("/go?"), "{body}");
        assert!(!body.contains("shares which result"), "{body}");
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
            render_home(12, Some(&status), now, &no_settings()).contains(
                "Plumb is still downloading Wikidata&#39;s list of official websites and more \
                 rankings. Search works now, and results get better once those are in."
            ),
            "{}",
            render_home(12, Some(&status), now, &no_settings())
        );

        // Once Wikidata is in, nothing is said.
        status.wikidata_missing = false;
        status.wikidata_error = None;
        assert!(!render_setup(&status, now).contains("Wikidata"));
        assert!(!render_home(12, Some(&status), now, &no_settings()).contains("Wikidata"));
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
            "\n<Description>Plumb Search</Description>\n",
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
                pages: Vec::new(),
                hits: vec![github],
                site_search: Some(SiteSearch {
                    domain: "github.com".into(),
                    terms: query.trim_start_matches("github ").into(),
                    url: self.link.clone(),
                }),
                spelling: None,
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
        assert!(body.contains("<div class=\"m\">United States &middot; <span title=\"text"));
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

    fn no_settings() -> Settings {
        Settings {
            history: None,
            options: SearchOptions::default(),
            network: NetSetting::Unavailable,
            scope: plumb_net::SearchScope::default(),
            private: false,
        }
    }

    fn scored(domain: &str, score: f32) -> Hit {
        let mut hit = hit(domain, &format!("https://{domain}/"), None, None);
        hit.score = score;
        hit
    }

    fn from_network(hit: Hit) -> NetworkResult {
        NetworkResult {
            hit,
            verified: true,
            crawler: Some("12D3KooWexample".into()),
            confirmed: false,
            answers: 2,
            shared: None,
        }
    }

    fn answered(hits: Vec<Hit>) -> NetOutcome {
        NetOutcome::Answered(NetworkResults {
            buckets: 4,
            asked: 8,
            answered: 6,
            relayed: 6,
            direct: 0,
            rejected: 0,
            cached: 0,
            pending: 0,
            stale: 0,
            hits: hits.into_iter().map(from_network).collect(),
        })
    }

    #[tokio::test]
    async fn settings_sit_behind_a_gear() {
        for uri in ["/", "/search?q=us+bank&net=1"] {
            let (code, _, body) = get(backend(bank_hits()), uri).await;
            assert_eq!(code, StatusCode::OK);
            assert!(body.contains("<details class=\"gear\">"), "{uri}: {body}");
            assert!(body.contains("name=\"country\""), "{uri}");
            assert!(body.contains("name=\"only\""), "{uri}");
            assert!(body.contains("<select name=\"lang\">"), "{uri}");
            assert!(
                body.contains("<option value=\"moderate\" selected>Moderate</option>"),
                "{uri}"
            );
            // `plumb serve` is in no network: the setting is off, and says why.
            assert!(
                body.contains("<input type=\"checkbox\" disabled> Use the Plumb network"),
                "{uri}"
            );
            assert!(body.contains("has not joined the Plumb network"), "{uri}");
            assert!(!body.contains("name=\"net\""), "{uri}");
        }
        let (_, _, body) = get(backend(bank_hits()), "/search?q=us+bank&net=1").await;
        assert!(body.contains("<p class=\"src\">From this site's own index.</p>"));
        assert!(!body.contains("class=\"net\""));
    }

    #[tokio::test]
    async fn safe_search_and_language_stay_with_the_search() {
        let (_, _, body) = get(
            backend(bank_hits()),
            "/search?q=us+bank&safe=strict&lang=de",
        )
        .await;
        assert!(body.contains("<option value=\"strict\" selected>Strict</option>"));
        assert!(body.contains("<option value=\"de\" lang=\"de\" selected>Deutsch</option>"));
        let options = SearchOptions {
            safe: SafeSearch::Off,
            language: Some("de".into()),
            ..SearchOptions::default()
        };
        assert_eq!(
            search_link("/search", "x", &options, false),
            "/search?q=x&safe=off&lang=de"
        );
        assert!(go_link("x", &options, "a.com").ends_with("&safe=off&lang=de"));
        // The default needs no parameter.
        assert_eq!(
            search_link("/search", "x", &SearchOptions::default(), false),
            "/search?q=x"
        );
    }

    #[test]
    fn the_network_setting_shows_its_state() {
        let mut settings = no_settings();
        settings.network = NetSetting::Off;
        let off = settings_form("x", false, &settings);
        assert!(off.contains("name=\"net\" value=\"1\"> Use the Plumb network"));
        settings.network = NetSetting::On;
        let on = settings_form("x", false, &settings);
        assert!(on.contains("name=\"net\" value=\"1\" checked> Use the Plumb network"));
        assert!(on.contains("without sending query text"));
        assert!(on.contains("Missing data may wait for a background download"));
    }

    #[test]
    fn the_private_switch_shows_only_where_private_search_runs() {
        let mut settings = no_settings();
        let off = settings_form("x", false, &settings);
        assert!(!off.contains("Private search"), "{off}");
        settings.private = true;
        let on = settings_form("x", false, &settings);
        // A link, so turning it on never submits what was typed.
        assert!(
            on.contains("href=\"/private\" role=\"switch\" aria-checked=\"false\""),
            "{on}"
        );
        assert!(!on.contains("name=\"private\""));
        assert!(private_toggle(true).contains("href=\"/\" role=\"switch\" aria-checked=\"true\""));
    }

    #[test]
    fn a_signed_network_crawl_fills_in_this_nodes_copy() {
        // This node holds b.com without text; the network has a signed
        // crawl of it whose text matches the search better.
        let local = vec![scored("a.com", 0.9), scored("b.com", 0.3)];
        let mut signed = SiteRecord::new("b.com");
        signed.title = Some("B Shoes".into());
        signed.description = Some("Handmade shoes".into());
        let mut network = from_network(scored("b.com", 0.95));
        network.shared = Some(signed);
        let unsigned = from_network(scored("a.com", 1.0));
        let network = NetOutcome::Answered(NetworkResults {
            hits: vec![network, unsigned],
            ..NetworkResults::default()
        });
        let shown = merge_results(&local, &network, 10);
        let order: Vec<(&str, bool)> = shown
            .iter()
            .map(|s| (s.hit.domain.as_str(), s.network.is_some()))
            .collect();
        // Still this node's, untinted, but with the network's text and
        // score; a.com has no signed crawl, so it keeps its own score.
        assert_eq!(order, [("b.com", false), ("a.com", false)]);
        assert_eq!(shown[0].hit.title.as_deref(), Some("B Shoes"));
        assert_eq!(shown[0].hit.description.as_deref(), Some("Handmade shoes"));
        assert!((shown[1].hit.score - 0.9).abs() < f32::EPSILON);

        // Text this node has stays.
        let mut own = scored("b.com", 0.3);
        own.description = Some("Our own words".into());
        let shown = merge_results(std::slice::from_ref(&own), &network, 10);
        let b = shown.iter().find(|s| s.hit.domain == "b.com").unwrap();
        assert_eq!(b.hit.description.as_deref(), Some("Our own words"));
        assert_eq!(b.hit.title.as_deref(), Some("B Shoes"));
    }

    #[test]
    fn network_only_sites_are_merged_by_score_and_tinted() {
        let local = vec![scored("a.com", 0.9), scored("b.com", 0.5)];
        let network = answered(vec![scored("b.com", 0.8), scored("c.com", 0.7)]);
        let shown = merge_results(&local, &network, 10);
        let order: Vec<(&str, bool)> = shown
            .iter()
            .map(|s| (s.hit.domain.as_str(), s.network.is_some()))
            .collect();
        // b.com is this node's own, so it keeps its own score and no tint.
        assert_eq!(order, [("a.com", false), ("c.com", true), ("b.com", false)]);
        assert_eq!(merge_results(&local, &network, 2).len(), 2);

        let mut settings = no_settings();
        settings.network = NetSetting::On;
        let results = SearchResults {
            pages: Vec::new(),
            hits: local.clone(),
            site_search: None,
            spelling: None,
        };
        let page = render_results(
            "q",
            &results,
            None,
            &network,
            &settings,
            None,
            10,
            false,
            &Icons::default(),
        );
        assert!(page.contains("<li class=\"net\"><a class=\"r\" href=\"https://c.com/\""));
        assert_eq!(page.matches("<li class=\"net\">").count(), 1);
        assert!(page.contains("6 of 8 requests to other nodes answered"));
        assert!(page.contains("Tinted results came only from the network."));
        assert!(page.contains("from the Plumb network (signed crawl, checked, in 2 answers)"));
        assert!(page.contains("/api/network/search?q=q"));
    }

    #[tokio::test]
    async fn instant_answers_go_above_the_results() {
        let fake = backend(bank_hits());
        let (status, _, body) = get(fake.clone(), "/search?q=12*(3%2B4)").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("<p class=\"iaq\">12 × (3 + 4) =</p><p class=\"iaa\">84</p>"),
            "{body}"
        );
        let (_, _, body) = get(fake.clone(), "/search?q=10+km+in+miles").await;
        assert!(body.contains("6.21371 miles"), "{body}");
        let (_, _, body) = get(fake.clone(), "/search?q=us+bank").await;
        assert!(!body.contains("class=\"ia\""));
        let (_, _, body) = get(fake, "/api/search?q=2%2B2&full=1").await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["answer"]["answer"], "4");
        assert!(json["hits"].is_array());
    }

    /// Finds the article on MrBeast, named by "mrbeast", with his profiles.
    struct BeastBackend;

    impl SearchBackend for BeastBackend {
        fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Hit>> {
            Ok(Vec::new())
        }

        fn search_full(
            &self,
            query: &str,
            _limit: usize,
            _options: &SearchOptions,
        ) -> Result<SearchResults> {
            use plumb_core::profiles::Profile;
            use plumb_index::pages::{Page, PageHit, PlacedPage};
            let mut results = SearchResults::default();
            if query.contains("mrbeast") {
                results.pages.push(PlacedPage {
                    hit: PageHit {
                        page: Page {
                            set: "wikipedia-en".into(),
                            url: "https://en.wikipedia.org/wiki/MrBeast".into(),
                            title: "MrBeast".into(),
                            description: Some("American YouTuber".into()),
                            site: None,
                            views: 900_000,
                            aliases: Vec::new(),
                            item: Some("Q19897578".into()),
                            profiles: vec![Profile {
                                service: "youtube-handle".into(),
                                id: "MrBeast".into(),
                            }],
                            website: None,
                            package: None,
                        },
                        score: 1.0,
                        named: query == "mrbeast",
                        popularity: 0.9,
                        whole: false,
                    },
                    under: None,
                    at: 0,
                });
            }
            Ok(results)
        }

        fn num_docs(&self) -> u64 {
            1
        }
    }

    #[tokio::test]
    async fn a_profile_asked_for_comes_first() {
        let app = router_with(Arc::new(BeastBackend), HomeCountry::Off);
        let (status, _, body) = send(app.clone(), "/search?q=mrbeast+youtube").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<section class=\"pf\""), "{body}");
        assert!(body.contains("href=\"https://www.youtube.com/@MrBeast\""));
        assert!(
            body.contains("<h2>MrBeast</h2>"),
            "the info box is about him"
        );
        let (_, _, body) = send(app.clone(), "/search?q=mrbeast+twitch").await;
        assert!(!body.contains("class=\"pf\""), "no Twitch channel known");
        let (_, _, body) = send(app, "/api/search?q=mrbeast+youtube&full=1").await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["profile"]["url"], "https://www.youtube.com/@MrBeast");
        assert_eq!(json["info"]["profiles"][0]["service"], "YouTube");
    }

    #[test]
    fn an_article_the_query_names_gets_an_info_box() {
        use plumb_index::pages::{Page, PageHit, PlacedPage};
        let article = PageHit {
            page: Page {
                set: "wikipedia-en".into(),
                url: "https://en.wikipedia.org/wiki/Marie_Curie".into(),
                title: "Marie Curie".into(),
                description: Some("Polish-French physicist and chemist (1867–1934)".into()),
                site: None,
                views: 100_000,
                aliases: Vec::new(),
                item: Some("Q7186".into()),
                profiles: Vec::new(),
                website: None,
                package: None,
            },
            score: 1.0,
            named: true,
            popularity: 0.9,
            whole: false,
        };
        let results = SearchResults {
            pages: vec![PlacedPage {
                hit: article,
                under: None,
                at: 0,
            }],
            hits: vec![scored("mariecurie.org.uk", 0.5)],
            site_search: None,
            spelling: None,
        };
        let page = render_results(
            "marie curie",
            &results,
            None,
            &NetOutcome::NotAsked,
            &no_settings(),
            None,
            10,
            false,
            &Icons::default(),
        );
        assert!(page.contains("<div class=\"wrap wide\">"), "{page}");
        assert!(page
            .contains("<aside class=\"ib\" aria-label=\"About Marie Curie\"><h2>Marie Curie</h2>"));
        assert!(page.contains("href=\"https://www.wikidata.org/wiki/Q7186\""));
        // The article is still listed with the results.
        assert!(page.contains("<li class=\"pg\">"));

        let mut plain = results.clone();
        plain.pages.clear();
        let page = render_results(
            "marie curie",
            &plain,
            None,
            &NetOutcome::NotAsked,
            &no_settings(),
            None,
            10,
            false,
            &Icons::default(),
        );
        assert!(!page.contains("class=\"ib\"") && !page.contains("wrap wide"));
    }

    #[test]
    fn typos_are_searched_as_typed_with_a_suggestion() {
        let mut results = SearchResults {
            pages: Vec::new(),
            hits: vec![scored("amazom.com", 0.9)],
            site_search: None,
            spelling: Some(Spelling {
                query: "amazon".into(),
            }),
        };
        let mut settings = no_settings();
        settings.options.country = Some("DE".into());
        let page = render_results(
            "amazom",
            &results,
            None,
            &NetOutcome::NotAsked,
            &settings,
            None,
            10,
            true,
            &Icons::default(),
        );
        assert!(
            page.contains(
                "<p class=\"sp\">Did you mean <a href=\"/search?q=amazon&amp;country=DE\">\
             <strong>amazon</strong></a>?</p>"
            ),
            "{page}"
        );
        assert!(!page.contains("Showing results for"), "{page}");
        // Picks are noted for the query as typed.
        assert!(page.contains("/go?q=amazom&amp;d=amazom.com"), "{page}");

        // Searching as typed carries on through the picks.
        let mut settings = no_settings();
        settings.options.exact = true;
        results.spelling = None;
        let page = render_results(
            "gogle",
            &results,
            None,
            &NetOutcome::NotAsked,
            &settings,
            None,
            10,
            true,
            &Icons::default(),
        );
        assert!(page.contains("&amp;exact=1"), "{page}");
        assert!(!page.contains("class=\"sp\""), "{page}");
    }

    #[test]
    fn site_queries_list_every_page_on_the_site() {
        let pages = ["Albert Einstein", "Einstein family", "Einstein (crater)"]
            .into_iter()
            .map(|title| plumb_index::pages::PlacedPage {
                hit: PageHit {
                    page: plumb_index::pages::Page::from_article(
                        "en",
                        plumb_core::Article {
                            title: title.into(),
                            ..Default::default()
                        },
                    ),
                    score: 0.5,
                    named: false,
                    popularity: 0.1,
                    whole: false,
                },
                under: None,
                at: 0,
            })
            .collect();
        let results = SearchResults {
            pages,
            hits: Vec::new(),
            site_search: None,
            spelling: None,
        };
        let page = render_results(
            "einstein site:wikipedia.org",
            &results,
            None,
            &NetOutcome::NotAsked,
            &no_settings(),
            None,
            10,
            false,
            &Icons::default(),
        );
        assert!(!page.contains("No sites match"));
        for title in ["Albert Einstein", "Einstein family", "Einstein (crater)"] {
            assert!(page.contains(title), "{title}");
        }
    }

    #[test]
    fn the_source_line_says_where_results_came_from() {
        let results = SearchResults {
            pages: Vec::new(),
            hits: vec![scored("a.com", 0.9)],
            site_search: None,
            spelling: None,
        };
        let mut settings = no_settings();
        settings.network = NetSetting::Off;
        settings.options.country = Some("DE".into());
        let page = render_results(
            "q",
            &results,
            None,
            &NetOutcome::NotAsked,
            &settings,
            None,
            10,
            false,
            &Icons::default(),
        );
        assert!(page.contains(
            "From this site's own index. <a href=\"/search?q=q&amp;country=DE&amp;net=1\">"
        ));

        settings.network = NetSetting::On;
        let page = render_results(
            "q",
            &results,
            None,
            &NetOutcome::Failed,
            &settings,
            None,
            10,
            false,
            &Icons::default(),
        );
        assert!(page.contains("the Plumb network did not answer this time"));
        assert!(page.contains("a.com"));

        let page = render_results(
            "q",
            &results,
            None,
            &answered(Vec::new()),
            &settings,
            None,
            10,
            false,
            &Icons::default(),
        );
        assert!(!page.contains("Tinted"));
        let none = NetOutcome::Answered(NetworkResults::default());
        let page = render_results(
            "q",
            &results,
            None,
            &none,
            &settings,
            None,
            10,
            false,
            &Icons::default(),
        );
        assert!(page.contains("none of the Plumb nodes it searches are connected right"));
    }

    #[test]
    fn scheduled_downloads_and_stale_results_have_clear_status() {
        let pending = NetworkResults {
            pending: 2,
            ..Default::default()
        };
        let page = render_network("us bank", &pending, &Icons::default());
        assert!(
            page.contains("waiting for the next background download"),
            "{page}"
        );
        assert!(!page.contains("No other nodes are connected"), "{page}");
        assert!(!page.contains("No node had a site matching"), "{page}");

        let stale = NetworkResults {
            cached: 1,
            stale: 1,
            ..Default::default()
        };
        let mut line = String::new();
        let mut settings = no_settings();
        settings.network = NetSetting::On;
        render_source(
            &mut line,
            "us bank",
            &settings,
            &NetOutcome::Answered(stale),
            0,
        );
        assert!(line.contains("saved Plumb results, read locally"), "{line}");
        assert!(line.contains("may be out of date"), "{line}");
        assert!(!line.contains("no other Plumb nodes"), "{line}");
    }

    #[test]
    fn results_show_site_icons_inline_and_letters_otherwise() {
        let results = SearchResults {
            pages: Vec::new(),
            spelling: None,
            hits: vec![
                hit(
                    "usbank.com",
                    "https://www.usbank.com/",
                    Some("U.S. Bank"),
                    None,
                ),
                hit(
                    "chase.com",
                    "https://www.chase.com/personal",
                    Some("Chase"),
                    None,
                ),
            ],
            site_search: None,
        };
        let mut icons = Icons::default();
        icons
            .0
            .insert("usbank.com".into(), "data:image/png;base64,iVBORw0K".into());
        let page = render_results(
            "bank",
            &results,
            None,
            &NetOutcome::NotAsked,
            &no_settings(),
            None,
            10,
            false,
            &icons,
        );
        assert!(page.contains(
            "<span class=\"ic\"><img src=\"data:image/png;base64,iVBORw0K\" alt=\"\" \
             width=\"18\" height=\"18\"></span>"
        ));
        let (letter, color) = site_initial("chase.com");
        assert_eq!(letter, 'C');
        assert!(page.contains(&format!(
            "<span class=\"ic l{color}\" aria-hidden=\"true\">C</span>"
        )));
        assert!(page.contains("<span class=\"u\">www.chase.com/personal</span>"));
        // An address that is just the domain is not repeated.
        let results = SearchResults {
            pages: Vec::new(),
            spelling: None,
            hits: vec![hit("jsr.io", "https://jsr.io/", Some("JSR"), None)],
            site_search: None,
        };
        let page = render_results(
            "jsr",
            &results,
            None,
            &NetOutcome::NotAsked,
            &no_settings(),
            None,
            10,
            false,
            &Icons::default(),
        );
        assert!(
            page.contains("<span class=\"dn\">jsr.io</span></span>"),
            "{page}"
        );
        // The page loads no image from anywhere: icons ride inside it.
        assert!(!page.contains("src=\"http"));
        assert!(CONTENT_SECURITY_POLICY.contains("img-src data:;"));
    }

    #[test]
    fn recent_headlines_follow_the_first_result_escaped() {
        let results = SearchResults {
            pages: Vec::new(),
            spelling: None,
            hits: vec![
                hit("news.com", "https://news.com/", Some("News"), None),
                hit("other.com", "https://other.com/", Some("Other"), None),
            ],
            site_search: None,
        };
        let now = now_unix();
        let headline = |title: &str, url: &str| crate::news::RecentHeadline {
            domain: "news.com".into(),
            title: title.into(),
            url: url.into(),
            at: now - 2 * 3600,
        };
        let recent = Recent {
            site: Some("news.com".into()),
            headlines: vec![
                headline(
                    "<script>alert(1)</script> wins",
                    "https://news.com/a?x=1&y=2",
                ),
                headline("Sneaky", "javascript:alert(1)"),
            ],
        };
        let page = render_results_with(
            "news",
            &results,
            None,
            &NetOutcome::NotAsked,
            &no_settings(),
            None,
            10,
            false,
            &Icons::default(),
            Some(&recent),
        );
        let block = page.find("<li class=\"news\">").expect("a Recent block");
        assert!(
            page.find("news.com/").unwrap() < block,
            "after the first result"
        );
        assert!(block < page.find("other.com").unwrap(), "before the second");
        // Folded by default, behind a one-line summary.
        assert!(page.contains(
            "<details><summary><span class=\"nh\">Latest from news.com</span> \
             <span class=\"m\">1 headline, newest 2 hours ago</span></summary>"
        ));
        assert!(page.contains(
            "<a href=\"https://news.com/a?x=1&amp;y=2\" rel=\"noreferrer\">\
             &lt;script&gt;alert(1)&lt;/script&gt; wins</a>"
        ));
        assert!(page.contains("news.com &middot; 2 hours ago"));
        assert!(!page.contains("Sneaky"));
        assert!(!page.contains("<script>"));

        // The gear's "Recent news" choice opens it, or leaves it out.
        let render = |view: RecentNews| {
            let mut settings = no_settings();
            settings.options.recent = view;
            render_results_with(
                "news",
                &results,
                None,
                &NetOutcome::NotAsked,
                &settings,
                None,
                10,
                false,
                &Icons::default(),
                Some(&recent),
            )
        };
        let open = render(RecentNews::Expanded);
        assert!(open.contains("<li class=\"news\"><details open>"));
        assert!(open.contains("<option value=\"expanded\" selected>Open</option>"));
        assert!(!render(RecentNews::Off).contains("class=\"news\""));
        // The choice rides along in the page's links, unless it is the default.
        let mut options = SearchOptions::default();
        assert!(!search_link("/search", "x", &options, false).contains("news="));
        options.recent = RecentNews::Off;
        assert!(search_link("/search", "x", &options, false).ends_with("&news=off"));
    }

    /// A node that keeps search history in a folder.
    struct HistoryNode(std::path::PathBuf);

    impl StatusSource for HistoryNode {
        fn status(&self) -> Status {
            node_status(Phase::Ready, Step::Idle)
        }
        fn search_history(&self) -> Option<crate::history::HistoryStore> {
            Some(crate::history::HistoryStore::new(&self.0))
        }
    }

    /// The `name=value` of the cookie `name` that `headers` set.
    fn set_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(&format!("{name}=")))
            .map(|v| v.split(';').next().unwrap().to_string())
    }

    #[tokio::test]
    async fn each_browser_keeps_its_own_history_and_opened_sites_come_first() {
        let dir = tempfile::tempdir().unwrap();
        let node = Arc::new(HistoryNode(dir.path().join("history")));
        let fake = backend(bank_hits());
        let app = || node_router(fake.clone(), node.clone());

        // The first search gives the browser a profile and links through /go.
        let (code, headers, body) = send(app(), "/search?q=us+bank&country=any").await;
        assert_eq!(code, StatusCode::OK);
        let profile = set_cookie(&headers, "plumb_profile").expect("a profile cookie");
        assert!(
            body.contains("href=\"/go?q=us+bank&amp;d=usbank-login-help.com&amp;country=any\""),
            "{body}"
        );
        assert!(!body.contains("You opened this before"), "{body}");
        assert!(body.contains("name=\"hist\""), "{body}");
        let first = |body: &str| {
            body.find("usbank.com</span>").unwrap()
                < body.find("usbank-login-help.com</span>").unwrap()
        };
        assert!(first(&body));

        // Opening the second result puts it first next time, labelled.
        let me = [("cookie", profile.as_str())];
        let (code, _, _) = send_with_headers(
            app(),
            "/go?q=us+bank&d=usbank-login-help.com&country=any",
            &me,
        )
        .await;
        assert_eq!(code, StatusCode::SEE_OTHER);
        let (_, _, body) = send_with_headers(app(), "/search?q=us+bank&country=any", &me).await;
        assert!(!first(&body), "{body}");
        assert!(body.contains("You opened this before"), "{body}");

        // The home page lists past searches; the history page lists both.
        let (_, _, home) = send_with_headers(app(), "/", &me).await;
        assert!(home.contains("class=\"recent\""), "{home}");
        assert!(home.contains(">us bank</a>"), "{home}");
        let (code, _, page) = send_with_headers(app(), "/history", &me).await;
        assert_eq!(code, StatusCode::OK);
        assert!(page.contains("usbank-login-help.com"), "{page}");

        // Another browser sees none of it.
        let (_, headers, other) = send(app(), "/search?q=us+bank&country=any").await;
        assert!(set_cookie(&headers, "plumb_profile").is_some_and(|c| c != profile));
        assert!(!other.contains("You opened this before"), "{other}");
        assert!(first(&other));

        // Turning both choices off: no labels, no reordering, no list.
        let (_, headers, body) =
            send_with_headers(app(), "/search?q=us+bank&country=any&hist=1", &me).await;
        let prefs = set_cookie(&headers, "plumb_history").unwrap();
        assert_eq!(prefs, "plumb_history=s0r0");
        assert!(first(&body), "{body}");
        assert!(!body.contains("You opened this before"), "{body}");
        let both = format!("{profile}; {prefs}");
        let (_, _, home) = send_with_headers(app(), "/", &[("cookie", both.as_str())]).await;
        assert!(!home.contains("class=\"recent\""), "{home}");

        // Clearing empties it.
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/history/clear")
                    .header("cookie", profile.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let (_, _, page) = send_with_headers(app(), "/history", &me).await;
        assert!(page.contains("No searches yet."), "{page}");
    }

    #[tokio::test]
    async fn the_about_page_moves_and_hides_results_for_this_browser_only() {
        let dir = tempfile::tempdir().unwrap();
        let node = Arc::new(HistoryNode(dir.path().join("history")));
        let fake = backend(bank_hits());
        let app = || node_router(fake.clone(), node.clone());
        let post = |cookie: Option<&str>, form: &'static str| {
            let mut request = Request::builder()
                .method("POST")
                .uri("/about")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if let Some(cookie) = cookie {
                request = request.header("cookie", cookie);
            }
            app().oneshot(request.body(Body::from(form)).unwrap())
        };

        let (code, _, page) = send(app(), "/about").await;
        assert_eq!(code, StatusCode::OK);
        assert!(page.contains("name=\"interests\""), "{page}");

        // Saving gives the browser a profile.
        let response = post(None, "pinned=usbank-login-help.com&interests=")
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let profile = set_cookie(response.headers(), "plumb_profile").expect("a profile");
        let me = [("cookie", profile.as_str())];
        let (_, _, body) = send_with_headers(app(), "/search?q=us+bank&country=any", &me).await;
        let first = |body: &str| {
            body.find("usbank.com</span>").unwrap()
                < body.find("usbank-login-help.com</span>").unwrap()
        };
        assert!(!first(&body), "{body}");
        assert!(body.contains("One of your sites"), "{body}");

        // Another browser sees none of it.
        let (_, _, other) = send(app(), "/search?q=us+bank&country=any").await;
        assert!(first(&other));
        assert!(!other.contains("One of your sites"), "{other}");

        // Hidden sites are left out; forgetting brings them back.
        post(Some(&profile), "hidden=www.usbank-login-help.com")
            .await
            .unwrap();
        let (_, _, body) = send_with_headers(app(), "/search?q=us+bank&country=any", &me).await;
        assert!(!body.contains("usbank-login-help.com</span>"), "{body}");
        let response = post(Some(&profile), "clear=1").await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let (_, _, body) = send_with_headers(app(), "/search?q=us+bank&country=any", &me).await;
        assert!(first(&body), "{body}");
    }

    #[tokio::test]
    async fn nodes_without_history_set_no_cookies() {
        let app = node_router(
            backend(bank_hits()),
            node(node_status(Phase::Ready, Step::Idle)),
        );
        let (_, headers, body) = send(app.clone(), "/search?q=us+bank").await;
        assert!(headers.get(header::SET_COOKIE).is_none());
        assert!(!body.contains("name=\"hist\""), "{body}");
        let (code, _, _) = send(app.clone(), "/history").await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let (code, _, _) = send(app, "/about").await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    struct IconNode;

    impl StatusSource for IconNode {
        fn status(&self) -> Status {
            node_status(Phase::Ready, Step::Idle)
        }
        fn icon(&self, domain: &str) -> Option<Vec<u8>> {
            (domain == "usbank.com").then(|| b"\x89PNG".to_vec())
        }
    }

    #[tokio::test]
    async fn a_node_puts_the_icons_it_has_into_the_page() {
        let app = node_router(backend(bank_hits()), Arc::new(IconNode));
        let (code, _, body) = send(app, "/search?q=us+bank").await;
        assert_eq!(code, StatusCode::OK);
        assert!(
            body.contains("<img src=\"data:image/png;base64,iVBORw==\""),
            "{body}"
        );
        assert_eq!(body.matches("<img ").count(), 1);
    }
}
