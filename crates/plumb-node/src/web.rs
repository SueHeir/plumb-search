//! `plumb serve`: a small web front end and JSON API.
//!
//! - `GET /` shows a search box,
//! - `GET /search?q=` shows results as server-rendered HTML, ten at first;
//!   "More results" under them asks for ten more (`limit=`),
//! - `GET /api/search?q=&limit=` returns a JSON list of [`Hit`](plumb_index::Hit)s, or with
//!   `full=1` a [`SearchResults`] object that also holds the site search link,
//!
//! Both searches take `country=XX` (a two-letter code, or `any` for none)
//! and `only=1` (leave out other countries' sites). Without `country`, the
//! server's [`HomeCountry`] setting decides, by default from the browser's
//! `Accept-Language`, then this computer's region settings, then the
//! United States. They also take `lang=XX` (only sites in that language, or
//! `any`); without it, the server's language when it has one, else the
//! browser's first language when the settings gear offers it, else English.
//! A server run with `--only-country` starts the search pages with "Only
//! this country" on when the address names no country.
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

pub mod answers;
mod api;
mod backend;
mod control;
mod go;
mod history;
mod hit;
mod home;
mod html;
mod link;
mod mcp;
mod network;
mod nodes;
mod panel;
mod params;
mod places;
mod plugin_results;
mod plugins;
pub(crate) mod private;
mod relay;
mod results;
mod search;
mod searxng;
mod setup;
mod tune;
mod weather;
mod welcome;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::routing::{get, post};
use axum::Router;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use plumb_index::{SearchOptions, SearchResults, Searcher};
use plumb_net::NetHandle;
use tracing::{error, info};

use self::api::{api_network_search, api_recent, api_search, api_status};
pub use self::api::{NetworkResult, NetworkResults};
pub use self::backend::{IndexBackend, SearchBackend, StatusSource};
use self::go::{go, redirect};
use self::hit::site_badge;
use self::home::{home, not_found, opensearch, request_origin, SETUP_RELOAD_SECONDS};
use self::html::{
    app_bar, app_bar_for, homepage_url, html_response, page, page_with_head, private_toggle,
    search_link, security_headers, time_until,
};
pub(crate) use self::html::{duration_words, http_url, time_ago};
pub use self::html::{escape_html, group_thousands};
use self::network::network_page;
pub use self::panel::ADD_TO_FIREFOX_PATH;
use self::params::{flag, SearchParams};
use self::search::{
    extras, place_sites, route_sources, run_places, run_search, search_about, search_page,
};
use crate::cli::ServeArgs;
use crate::country::{HomeCountry, COUNTRY_CHOICES};
use crate::meaning::{MeaningIndex, SharedMeaning};
use crate::news::Recent;
use crate::node::{Phase, Status};
use crate::plugins::PluginResults;
use crate::websearch::WebSettings;
use crate::{block_on, rank_config};

/// Results returned when a request does not say how many.
pub const DEFAULT_LIMIT: usize = 10;

/// Most results one request can get.
pub const MAX_LIMIT: usize = 100;

/// Longer queries are cut to this many characters.
pub const MAX_QUERY_CHARS: usize = 200;

/// `text` as a search: garbled text read as it was written
/// ([`plumb_core::unmangled`]), whitespace collapsed, and cut to
/// [`MAX_QUERY_CHARS`].
pub fn query_text(text: &str) -> String {
    plumb_core::truncate_chars(
        &plumb_core::collapse_whitespace(&plumb_core::unmangled(text)),
        MAX_QUERY_CHARS,
    )
}

/// No scripts, no external resources, forms only to this server. Inline
/// styles are allowed for the page's own `<style>` element.
const CONTENT_SECURITY_POLICY: &str =
    "default-src 'none'; style-src 'unsafe-inline'; img-src data:; \
     form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

/// How long a network search waits for other nodes to answer.
const NETWORK_SEARCH_WAIT: Duration = Duration::from_secs(4);

/// Network searches one client may start at once...
const NET_BURST: f64 = 20.0;

/// ...and per minute after that.
const NET_PER_MINUTE: f64 = 20.0;

/// The media type of an OpenSearch description.
const OPENSEARCH_TYPE: &str = "application/opensearchdescription+xml";

/// In the `<head>` of every page, so that browsers offer to add Plumb as a
/// search engine.
const OPENSEARCH_LINK: &str = "<link rel=\"search\" \
     type=\"application/opensearchdescription+xml\" title=\"Plumb Search\" \
     href=\"/opensearch.xml\">\n";

#[derive(Clone)]
struct AppState {
    backend: Arc<dyn SearchBackend>,
    /// Set for a long-running node, `None` for `plumb serve`.
    node: Option<Arc<dyn StatusSource>>,
    /// The home country and the web search link.
    settings: WebSettings,
    /// Currency rates for instant answers.
    rates: Arc<answers::RatesCache>,
    /// Forecasts for weather answers.
    weather: Arc<weather::WeatherCache>,
    /// How many tool calls each client may still make to `/mcp`.
    mcp_limiter: Arc<mcp::Limiter>,
    /// How many network searches each client may still start: a node runs
    /// only a few at once, so one client must not take them all.
    net_limiter: Arc<mcp::Limiter>,
    /// Fetches pages for `/mcp`'s `read_page`.
    page_reader: Arc<mcp::SharedReader>,
    /// How `/mcp`'s `read_page` reads for others, with `--mcp-read-pages`.
    public_reads: Arc<mcp::PublicReads>,
    /// What agents found, for `/mcp`'s `report_finding`; opened from the
    /// node's data directory when first needed.
    findings: Arc<std::sync::OnceLock<Option<Arc<crate::findings::Findings>>>>,
    /// The ranking experiments set up in the node's data directory;
    /// opened when first needed.
    experiments: Arc<std::sync::OnceLock<Option<Arc<crate::experiments::Lab>>>>,
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

    /// The node's ranking experiments; `None` for `plumb serve`, and for
    /// a node that sets up none.
    fn experiments(&self) -> Option<Arc<crate::experiments::Lab>> {
        self.experiments
            .get_or_init(|| {
                let dir = self.node.as_ref()?.data_dir()?;
                match crate::experiments::Lab::in_dir(&dir) {
                    Ok(lab) => lab.map(Arc::new),
                    Err(err) => {
                        error!("no experiments run: {err:#}");
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

    /// The "Recent" block for `query`, whose results are `results`, searched
    /// with `options`; `None` for `plumb serve`, which keeps no headlines.
    /// Headlines are matched with the query as typed: spelling corrections
    /// come from site names, and would turn news words into them.
    fn recent(
        &self,
        query: &str,
        results: &SearchResults,
        options: &SearchOptions,
    ) -> Option<Recent> {
        let top = results
            .hits
            .first()
            .map(|hit| (hit.domain.as_str(), hit.named));
        self.node
            .as_ref()?
            .recent(query, top, options.language.as_deref())
    }

    /// What the node's plugins find for `query`, which the node took to
    /// be `about` one thing.
    async fn plugin_results(
        &self,
        query: &str,
        options: &SearchOptions,
        about: Option<&plumb_plugin::About>,
        run: Option<&str>,
    ) -> Vec<PluginResults> {
        let found = self
            .settings
            .plugins
            .search_running(query, options.safe, options.language.as_deref(), about, run)
            .await;
        self.without_denied(found)
    }

    /// `found` less the results the node's denylist takes off, as for
    /// the node's own results.
    fn without_denied(&self, mut found: Vec<PluginResults>) -> Vec<PluginResults> {
        let Some(node) = &self.node else {
            return found;
        };
        for plugin in &mut found {
            plugin.results.retain(|item| !node.denies(&item.url));
        }
        found.retain(|plugin| !plugin.results.is_empty());
        found
    }

    /// The icons of `domains` that this node has, for
    /// [`render_hit`](hit::render_hit). Read off the async threads: each is a
    /// small file.
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
        weather: Arc::default(),
        mcp_limiter: Arc::default(),
        net_limiter: Arc::new(mcp::Limiter::new(NET_BURST, NET_PER_MINUTE)),
        page_reader: Arc::default(),
        public_reads: Arc::default(),
        findings: Arc::default(),
        experiments: Arc::default(),
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
        weather: Arc::default(),
        mcp_limiter: Arc::default(),
        net_limiter: Arc::new(mcp::Limiter::new(NET_BURST, NET_PER_MINUTE)),
        page_reader: Arc::default(),
        public_reads: Arc::default(),
        findings: Arc::default(),
        experiments: Arc::default(),
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
    router = plugins::routes(router);
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
            .route("/app/plugins", post(panel::save_plugins))
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
        router = link::routes(router);
        router = tune::routes(router);
        router = welcome::routes(router);
    }
    router.fallback(not_found).with_state(state)
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
    if let Some(map) = &args.map {
        backend = backend.with_map(map.clone());
    }
    let app = router_with(
        Arc::new(backend),
        WebSettings {
            home: args.country.clone(),
            only_country: args.only_country,
            language: args.lang.clone(),
            web_search: args.web_search.0,
            read_pages_for_all: args.mcp_read_pages,
            findings_for_all: args.mcp_findings,
            page_reader: plumb_crawl::ReadConfig::default(),
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

#[cfg(test)]
mod tests;
