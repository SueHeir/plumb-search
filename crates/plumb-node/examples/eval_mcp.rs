//! Bounded scratch-only server for surface contracts, using production routes.
//! Requires the combined candidate's entity/language retrieval APIs. Opens
//! existing indexes only; never constructs a Node or rebuilds page/place sets.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use clap::Parser;
use plumb_core::Operators;
use plumb_index::pages::{
    add_named_site, drop_namesakes_of_words, lift_named_sites, operators_allow, options_allow,
    place_operator_pages, place_pages, Page, PageHit, PageSearcher, OPERATOR_PAGES,
};
use plumb_index::{Hit, RankConfig, SearchOptions, SearchResults, Searcher};
use plumb_node::cli::MeaningArgs;
use plumb_node::country::HomeCountry;
use plumb_node::mcp::Mcp;
use plumb_node::meaning::{MeaningIndex, SharedMeaning};
use plumb_node::web::{self, IndexBackend, SearchBackend};
use plumb_node::websearch::WebSettings;
use serde_json::{json, Value};

const TYPED_CANDIDATES: usize = 200;
const DISPLAY_CANDIDATES: usize = 10;
const MAX_BODY: usize = 64 * 1024;
// Production's shared-client limiter permits 60 tools/minute. Pace this
// sequential evaluator through that limiter; report latency in the core run.
const TOOL_GAP: Duration = Duration::from_millis(1050);

#[derive(Parser)]
#[command(about = "Serve existing scratch indexes through production MCP/web routes; no node jobs")]
struct Args {
    /// Every index/model/manifest must already exist inside this scratch tree.
    #[arg(long)]
    scratch_root: PathBuf,
    #[arg(long)]
    index: PathBuf,
    /// The directory containing pages.json, inside the retained eval page cache.
    #[arg(long)]
    page_index: PathBuf,
    /// Optional prebuilt small place index; never a places gzip source.
    #[arg(long)]
    place_index: Option<PathBuf>,
    #[command(flatten)]
    meaning: MeaningArgs,
    #[arg(long, default_value = "{}")]
    rank: String,
    /// Fixed home country or any; auto is refused to avoid host locale effects.
    #[arg(long, default_value = "any")]
    country: String,
    #[arg(long, default_value = "en")]
    language: String,
    #[arg(long, default_value = "127.0.0.1:18081")]
    bind: SocketAddr,
    #[arg(long, default_value_t = 900)]
    seconds: u64,
    #[arg(long)]
    snapshot_manifest: Option<PathBuf>,
}

/// Mirrors the bounded public retrieval/placement primitives in node/pages.
/// Changes to production assembly must be reviewed here before treating these
/// surface checks as deployment evidence; it is not a live Node equivalence test.
struct FrozenBackend {
    sites: IndexBackend,
    pages: PageSearcher,
    rank: RankConfig,
}

impl FrozenBackend {
    fn add_pages(
        &self,
        query: &str,
        options: &SearchOptions,
        rank: &RankConfig,
        results: &mut SearchResults,
    ) -> Result<()> {
        let pages = self.pages.in_language(options.language.as_deref());
        let ops = Operators::parse(query);
        if ops.any() {
            if !ops.words.is_empty() {
                let mut found =
                    pages.search_naming_docs(&ops.words, &ops, false, OPERATOR_PAGES)?;
                found.retain(|hit| options_allow(options, &hit.page));
                results.pages = place_operator_pages(&ops, &results.hits, found);
            }
            return Ok(());
        }
        let applied = results
            .spelling
            .as_ref()
            .filter(|spelling| spelling.applied)
            .map(|spelling| spelling.query.clone());
        let query = applied.as_deref().unwrap_or(query);
        let mut found = pages.search(query, DISPLAY_CANDIDATES)?;
        pages.add_other_number(query, &results.hits, &mut found, DISPLAY_CANDIDATES)?;
        found.retain(|hit| options_allow(options, &hit.page));
        if rank.add_named_site {
            add_named_site(&mut results.hits, &found, |domain| self.sites.site(domain));
        }
        if rank.drop_namesakes {
            drop_namesakes_of_words(&mut results.hits, &found);
        }
        lift_named_sites(&mut results.hits, &found);
        pages.note_demand(&mut results.hits)?;
        let spelled_right = found
            .iter()
            .any(|hit| hit.page.package.is_some() || hit.named || hit.whole);
        if spelled_right && applied.is_none() {
            results.spelling = None;
        }
        if let Some(link) = &results.site_search {
            if found
                .iter()
                .any(|hit| hit.named && hit.page.site.as_deref() != Some(link.domain.as_str()))
            {
                results.site_search = None;
            }
        }
        if let Some(spelling) = results.spelling.take_if(|_| applied.is_none()) {
            results.spelling = pages.check_spelling(query, spelling)?;
        }
        if results.spelling.is_none() && !spelled_right {
            let sites = self.sites.searcher();
            results.spelling = pages.suggest_spelling(query, sites.spelling_model(), &|word| {
                sites.word_sites(word) >= plumb_index::KNOWN_WORD_SITES
            })?;
        }
        results.pages = place_pages(query, &results.hits, found);
        if rank.learned {
            plumb_index::learned::reorder(
                plumb_index::learned::Model::builtin(),
                query,
                &mut results.hits,
                &mut results.pages,
            );
        }
        if let Some(site) = results.spelling.as_ref().and_then(|s| s.site.as_deref()) {
            plumb_index::suggested_site_second(&mut results.hits, site);
        }
        pages.title_untitled(&mut results.hits)?;
        Ok(())
    }
}

impl SearchBackend for FrozenBackend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.sites.search(query, limit)
    }

    fn search_full(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        let mut results = self.sites.search_full(query, limit, options)?;
        self.add_pages(query, options, &self.rank, &mut results)?;
        Ok(results)
    }

    fn search_ranked(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
        rank: &RankConfig,
    ) -> Result<SearchResults> {
        let mut results = self.sites.search_ranked(query, limit, options, rank)?;
        self.add_pages(query, options, rank, &mut results)?;
        Ok(results)
    }

    fn num_docs(&self) -> u64 {
        self.sites.num_docs()
    }

    fn site(&self, domain: &str) -> Option<Hit> {
        SearchBackend::site(&self.sites, domain)
    }

    fn places(
        &self,
        query: &str,
        home: Option<&str>,
        country: Option<&str>,
    ) -> Option<plumb_index::places::PlaceResults> {
        self.sites.places(query, home, country)
    }

    fn locate(&self, text: &str, country: Option<&str>) -> Option<plumb_core::place::Place> {
        self.sites.locate(text, country)
    }

    fn known_song(&self, query: &str, options: &SearchOptions) -> Option<Page> {
        self.pages
            .in_language(options.language.as_deref())
            .known_song(query)
            .ok()
            .flatten()
            .filter(|page| options_allow(options, page))
    }

    fn definition(&self, name: &str) -> Option<Page> {
        self.pages.definition(name).ok().flatten()
    }

    fn entities(&self, query: &str, limit: usize, options: &SearchOptions) -> Result<Vec<PageHit>> {
        let ops = Operators::parse(query);
        let words = if ops.any() { ops.words.as_str() } else { query };
        Ok(self
            .pages
            .in_language(options.language.as_deref())
            .entities(words, TYPED_CANDIDATES)?
            .into_iter()
            .filter(|hit| options_allow(options, &hit.page) && operators_allow(&ops, &hit.page))
            .take(limit.min(TYPED_CANDIDATES))
            .collect())
    }

    fn pages_of(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
        docs: bool,
        keep: &dyn Fn(&Page) -> bool,
    ) -> Vec<PageHit> {
        if limit == 0 {
            return Vec::new();
        }
        let ops = Operators::parse(query);
        let words = if ops.any() { ops.words.as_str() } else { query };
        self.pages
            .in_language(options.language.as_deref())
            .search_naming_docs(words, &ops, docs, TYPED_CANDIDATES)
            .unwrap_or_else(|err| {
                eprintln!("scratch typed retrieval: {err:#}");
                Vec::new()
            })
            .into_iter()
            .filter(|hit| {
                options_allow(options, &hit.page)
                    && operators_allow(&ops, &hit.page)
                    && keep(&hit.page)
            })
            .take(limit.min(TYPED_CANDIDATES))
            .collect()
    }
}

fn scratch_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("opening {}", path.display()))?;
    ensure!(
        path.starts_with(root),
        "{} is outside scratch root {}",
        path.display(),
        root.display()
    );
    Ok(path)
}

fn offline_query(query: &str) -> bool {
    !plumb_answer::may_need_rates(query)
        && plumb_answer::weather::asked(query).is_none()
        && plumb_node::websearch::bang_url(query).is_none()
}

fn rejected(id: Value, reason: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32600, "message": reason}
        })),
    )
        .into_response()
}

/// The production router includes external-search, rates and weather paths.
/// Reject them before entering it. Do not install ConnectInfo: production MCP
/// then treats all requests as remote, disabling read_page/findings/leads.
async fn offline_only(
    State(next_tool): State<Arc<tokio::sync::Mutex<tokio::time::Instant>>>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let allowed = matches!(
        path,
        "/" | "/search" | "/api/search" | "/mcp" | "/opensearch.xml" | "/eval/status"
    );
    if !allowed
        || (request.method() != Method::GET
            && !(path == "/mcp" && request.method() == Method::POST))
    {
        return rejected(
            Value::Null,
            "route excluded from the offline scratch evaluation",
        );
    }
    if path == "/mcp" && request.method() == Method::POST {
        let (parts, body) = request.into_parts();
        let Ok(bytes) = to_bytes(body, MAX_BODY).await else {
            return rejected(Value::Null, "scratch request body exceeds 64 KiB");
        };
        let Ok(message) = serde_json::from_slice::<Value>(&bytes) else {
            return rejected(Value::Null, "scratch request must be a JSON-RPC object");
        };
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message["method"].as_str().unwrap_or_default();
        if !matches!(
            method,
            "initialize" | "ping" | "notifications/initialized" | "tools/list" | "tools/call"
        ) {
            return rejected(id, "method excluded from the offline scratch evaluation");
        }
        if method == "tools/call" {
            let name = message["params"]["name"].as_str().unwrap_or_default();
            if !matches!(
                name,
                "search" | "official_site" | "check_lookalike" | "site_info" | "facts" | "package"
            ) {
                return rejected(id, "tool excluded from the offline scratch evaluation");
            }
        }
        if Mcp::search_query(&message).is_some_and(|q| !offline_query(&q)) {
            return rejected(
                id,
                "rates, weather and external redirects are excluded from this offline evaluation",
            );
        }
        if method == "tools/call" {
            let mut due = next_tool.lock().await;
            tokio::time::sleep_until(*due).await;
            *due = tokio::time::Instant::now() + TOOL_GAP;
        }
        request = Request::from_parts(parts, Body::from(bytes));
    } else if matches!(path, "/search" | "/api/search") {
        let query =
            url::form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
                .find(|(key, _)| key == "q")
                .map(|(_, query)| query.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
        let query: String = query.chars().take(web::MAX_QUERY_CHARS).collect();
        if !offline_query(&query) {
            return rejected(
                Value::Null,
                "rates, weather and external redirects are excluded from this offline evaluation",
            );
        }
    }
    next.run(request).await
}

fn router(backend: Arc<dyn SearchBackend>, settings: WebSettings, status: Value) -> axum::Router {
    web::router_with(backend, settings)
        .route(
            "/eval/status",
            get(move || async move { Json(status.clone()) }),
        )
        .layer(middleware::from_fn_with_state(
            Arc::new(tokio::sync::Mutex::new(tokio::time::Instant::now())),
            offline_only,
        ))
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();
    ensure!(args.bind.ip().is_loopback(), "--bind must be loopback");
    ensure!(
        (1..=3600).contains(&args.seconds),
        "--seconds must be between 1 and 3600"
    );
    let home = HomeCountry::parse(&args.country).map_err(anyhow::Error::msg)?;
    ensure!(
        home != HomeCountry::Auto,
        "--country must be fixed or any, never auto"
    );
    let language = plumb_core::language_code(&args.language).context("invalid --language")?;
    let root = args
        .scratch_root
        .canonicalize()
        .context("opening --scratch-root")?;
    ensure!(
        root.parent().is_some(),
        "--scratch-root cannot be the filesystem root"
    );
    let index = scratch_path(&root, &args.index)?;
    let page_index = scratch_path(&root, &args.page_index)?;
    if let Some(model) = &args.meaning.model {
        let model = scratch_path(&root, model)?;
        ensure!(
            !model.join(plumb_embed::SERVER_FILE).exists(),
            "embedding servers are excluded; use local weights"
        );
        for file in plumb_embed::MODEL_FILES {
            scratch_path(&root, &model.join(file))?;
        }
        args.meaning.model = Some(model);
    }
    if let Some(vectors) = &args.meaning.vectors {
        args.meaning.vectors = Some(scratch_path(&root, vectors)?);
    }
    let rank: RankConfig = serde_json::from_str(&args.rank).context("reading --rank JSON")?;
    let meaning = SharedMeaning::new(MeaningIndex::from_args(&args.meaning)?);
    let mut sites = IndexBackend::new(Searcher::open(&index)?, rank.clone()).with_meaning(meaning);
    let mut place_count = None;
    if let Some(path) = &args.place_index {
        let path = scratch_path(&root, path)?;
        let places = plumb_index::places::PlaceSearcher::open(&path)?;
        place_count = Some(places.num_places());
        sites = sites.with_places(places);
        args.place_index = Some(path);
    }
    let pages = PageSearcher::open(&page_index)?;
    let snapshot = args
        .snapshot_manifest
        .as_ref()
        .map(|path| -> Result<Value> {
            let path = scratch_path(&root, path)?;
            ensure!(
                std::fs::metadata(&path)?.len() <= 1024 * 1024,
                "snapshot manifest exceeds 1 MiB"
            );
            Ok(serde_json::from_slice(&std::fs::read(path)?)?)
        })
        .transpose()?;
    let status = json!({
        "schema": 1, "build": plumb_node::build_info::current(),
        "adapter": "candidate-only frozen public retrieval primitives", "scratch_root": root,
        "index": index, "page_index": page_index, "sites": sites.num_docs(), "pages": pages.num_pages(),
        "place_index": args.place_index, "places": place_count, "model": args.meaning.model,
        "vectors": args.meaning.vectors, "query_instruction": format!("{:?}", args.meaning.query_instruction),
        "rank": rank, "country": args.country, "language": language, "snapshot": snapshot,
        "clock": "production wall clock; temporal contracts require separate fixed-clock core reports",
        "tool_gap_ms": TOOL_GAP.as_millis(),
        "seconds": args.seconds, "features": {"node_jobs": false, "findings": false,
            "personalization": false, "plugins": false, "page_reads": false, "external_results": false,
            "weather": false, "currency_rates": false},
    });
    let backend = Arc::new(FrozenBackend { sites, pages, rank });
    let settings = WebSettings {
        home,
        language: Some(language),
        ..WebSettings::default()
    };
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    eprintln!(
        "scratch MCP at http://{}/mcp; provenance at /eval/status; expires after {}s",
        listener.local_addr()?,
        args.seconds
    );
    // Without into_make_service_with_connect_info, no request gains production
    // local-only privileges, even when sent from localhost with a local Host.
    axum::serve(listener, router(backend, settings, status))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = tokio::time::sleep(Duration::from_secs(args.seconds)) => {},
            }
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_core::article::Article;
    use plumb_core::facts::{Fact, FactKind};
    use plumb_core::packages::PackageInfo;
    use plumb_core::SiteRecord;
    use plumb_index::pages::{build_page_index, DOCS_SET};
    use tower::ServiceExt;

    fn fixture() -> (tempfile::TempDir, Arc<FrozenBackend>) {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("sites");
        plumb_index::build_index(
            &index,
            &[SiteRecord {
                domain: "python.org".into(),
                title: Some("Python".into()),
                description: Some("Python programming language".into()),
                ..SiteRecord::default()
            }],
        )
        .unwrap();
        let pages_dir = dir.path().join("pages");
        let pages = vec![
            Page::from_article(
                "en",
                Article {
                    title: "Japan".into(),
                    item: Some("Q17".into()),
                    views: 100,
                    description: Some("country in East Asia".into()),
                    facts: vec![Fact {
                        kind: FactKind::Population,
                        value: "123802000;2024".into(),
                    }],
                    ..Article::default()
                },
            ),
            Page::from_reference(Article {
                title: "Japan".into(),
                item: Some("https://reference.example/japan".into()),
                views: 100000,
                ..Article::default()
            })
            .unwrap(),
            Page::from_docs(Article {
                title: "tomllib — Parse TOML files".into(),
                item: Some("https://docs.python.org/3/library/tomllib.html".into()),
                description: Some("Python tomllib TOML parser".into()),
                language: Some("en".into()),
                ..Article::default()
            })
            .unwrap(),
            Page::from_package(Article {
                title: "serde".into(),
                views: 100000,
                description: Some("Serialization framework for Rust".into()),
                package: Some(PackageInfo {
                    registry: "crates".into(),
                    name: "serde".into(),
                    version: Some("1.0.228".into()),
                    docs: Some("https://docs.rs/serde/".into()),
                    ..PackageInfo::default()
                }),
                ..Article::default()
            })
            .unwrap(),
        ];
        build_page_index(&pages_dir, pages).unwrap();
        let rank = RankConfig::default();
        let backend = Arc::new(FrozenBackend {
            sites: IndexBackend::new(Searcher::open(&index).unwrap(), rank.clone()),
            pages: PageSearcher::open(&pages_dir).unwrap(),
            rank,
        });
        (dir, backend)
    }

    async fn rpc(app: axum::Router, method: &str, params: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri("/mcp?findings=off")
            .header("host", "localhost:18081")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}).to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn production_mcp_uses_entities_docs_packages_and_reports_build() {
        let (_dir, backend) = fixture();
        let options = SearchOptions::default();
        let docs = backend.pages_of("tomllib site:docs.python.org", 10, &options, true, &|p| {
            p.set == DOCS_SET
        });
        assert_eq!(docs.len(), 1);
        assert!(backend
            .pages_of("tomllib -site:python.org", 10, &options, true, &|_| true)
            .is_empty());
        assert!(backend
            .pages_of("tomllib", 0, &options, true, &|_| true)
            .is_empty());
        assert!(!backend
            .search_full("serde package rust", 10, &options)
            .unwrap()
            .pages
            .is_empty());
        let app = router(
            backend,
            WebSettings::from(HomeCountry::Off),
            json!({"schema": 1}),
        );
        let (status, init) = rpc(app.clone(), "initialize", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(init["result"]["_meta"]["plumb.build"]["revision"].is_string());
        let (_, tools) = rpc(app.clone(), "tools/list", json!({})).await;
        let names: Vec<_> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"read_page"));
        assert!(!names.contains(&"report_finding"));
        let (_, docs) = rpc(app.clone(), "tools/call", json!({"name": "search", "arguments": {"query": "tomllib site:docs.python.org", "kind": "docs"}})).await;
        let pages = docs["result"]["structuredContent"]["pages"]
            .as_array()
            .unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(
            pages[0]["url"],
            "https://docs.python.org/3/library/tomllib.html"
        );
        let (_, facts) = rpc(
            app.clone(),
            "tools/call",
            json!({"name": "facts", "arguments": {"subject": "Japan", "about": "population"}}),
        )
        .await;
        let facts = &facts["result"]["structuredContent"];
        assert_eq!(facts["item"], "Q17");
        assert_eq!(facts["facts"][0]["observation_year"], 2024);
        let (_, package) = rpc(
            app,
            "tools/call",
            json!({"name": "package", "arguments": {"name": "serde", "registry": "crates"}}),
        )
        .await;
        assert_eq!(
            package["result"]["structuredContent"]["found"], true,
            "{package}"
        );
    }

    #[tokio::test]
    async fn production_web_works_and_outbound_routes_are_rejected() {
        let (_dir, backend) = fixture();
        let app = router(
            backend,
            WebSettings::from(HomeCountry::Off),
            json!({"schema": 1}),
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/search?q=tomllib+site%3Adocs.python.org&full=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert!(!body["pages"].as_array().unwrap().is_empty(), "{body}");
        for uri in [
            "/api/search?q=weather+in+Denver",
            "/search?q=100+USD+in+EUR",
            "/search?q=!g+rust",
            "/api/websearch",
            "/api/status",
            "/app",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
        }
        for (name, args) in [
            ("read_page", json!({"url": "https://example.com"})),
            ("report_finding", json!({})),
            ("search", json!({"query": "100 USD in EUR"})),
            ("search", json!({"query": "weather in Denver"})),
        ] {
            let (status, error) = rpc(
                app.clone(),
                "tools/call",
                json!({"name": name, "arguments": args}),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
            assert_eq!(error["id"], 7);
        }
    }

    #[test]
    fn paths_must_be_in_scratch_and_cli_cannot_bind_publicly_by_default() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let root = a.path().canonicalize().unwrap();
        assert!(scratch_path(&root, a.path()).is_ok());
        assert!(scratch_path(&root, b.path()).is_err());
        assert!(scratch_path(&root, &a.path().join("missing")).is_err());
        let args = Args::try_parse_from([
            "eval_mcp",
            "--scratch-root",
            "/scratch",
            "--index",
            "/scratch/sites",
            "--page-index",
            "/scratch/pages",
        ])
        .unwrap();
        assert!(args.bind.ip().is_loopback());
        assert_eq!(args.seconds, 900);
    }
}
