//! `plumb serve`: a small web front end and JSON API.
//!
//! - `GET /` shows a search box,
//! - `GET /search?q=` shows results as server-rendered HTML,
//! - `GET /api/search?q=&limit=` returns a JSON list of [`Hit`]s.
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
use axum::http::{header, HeaderName, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use plumb_core::{collapse_whitespace, truncate_chars};
use plumb_index::{Hit, RankConfig, Searcher};
use serde::Deserialize;
use tracing::{debug, error, info};
use url::Url;

use crate::cli::ServeArgs;
use crate::{block_on, rank_config};

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

/// Answers queries for the web handlers. [`IndexBackend`] is the real one;
/// tests can plug in their own.
pub trait SearchBackend: Send + Sync {
    /// Best `limit` hits for `query`, best first.
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>>;
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

    fn num_docs(&self) -> u64 {
        self.searcher.num_docs()
    }
}

#[derive(Clone)]
struct AppState {
    backend: Arc<dyn SearchBackend>,
}

/// The web app: `/`, `/search` and `/api/search`.
pub fn router(backend: Arc<dyn SearchBackend>) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/search", get(search_page))
        .route("/api/search", get(api_search))
        .with_state(AppState { backend })
}

/// Opens the index and serves it until Ctrl-C or SIGTERM.
pub fn run(args: ServeArgs) -> Result<()> {
    let searcher = Searcher::open(&args.index)
        .with_context(|| format!("opening the index in {}", args.index.display()))?;
    let docs = searcher.num_docs();
    let app = router(Arc::new(IndexBackend::new(
        searcher,
        rank_config(args.alpha),
    )));
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
async fn shutdown_signal() {
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
}

impl SearchParams {
    /// The query with whitespace collapsed, cut to [`MAX_QUERY_CHARS`].
    fn query(&self) -> String {
        truncate_chars(&collapse_whitespace(&self.q), MAX_QUERY_CHARS)
    }

    fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
    }
}

async fn home(State(state): State<AppState>) -> Response {
    html_response(StatusCode::OK, render_home(state.backend.num_docs()))
}

async fn search_page(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Response {
    let query = params.query();
    if query.is_empty() {
        return html_response(StatusCode::OK, render_home(state.backend.num_docs()));
    }
    match run_search(&state, &query, params.limit()).await {
        Ok(hits) => html_response(StatusCode::OK, render_results(&query, &hits)),
        Err(err) => {
            error!("search for {query:?} failed: {err:#}");
            html_response(StatusCode::INTERNAL_SERVER_ERROR, render_error(&query))
        }
    }
}

async fn api_search(State(state): State<AppState>, Query(params): Query<SearchParams>) -> Response {
    let query = params.query();
    if query.is_empty() {
        return (StatusCode::OK, security_headers(), Json(Vec::<Hit>::new())).into_response();
    }
    match run_search(&state, &query, params.limit()).await {
        Ok(hits) => (StatusCode::OK, security_headers(), Json(hits)).into_response(),
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

/// Runs a search on the blocking thread pool, since searching is CPU and
/// disk work. A panicking backend becomes an error, not a dropped connection.
async fn run_search(state: &AppState, query: &str, limit: usize) -> Result<Vec<Hit>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let backend = Arc::clone(&state.backend);
    let owned_query = query.to_string();
    let hits = tokio::task::spawn_blocking(move || backend.search(&owned_query, limit))
        .await
        .context("the search task failed")??;
    debug!("{query:?}: {} hits", hits.len());
    Ok(hits)
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
fn group_thousands(n: u64) -> String {
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

const STYLE: &str = "\
:root{color-scheme:light dark;--bg:#fff;--fg:#202124;--muted:#5f6368;--link:#1a0dab;\
--url:#0d652d;--line:#dadce0;--accent:#1a73e8}\
@media (prefers-color-scheme:dark){:root{--bg:#1f1f1f;--fg:#e8eaed;--muted:#9aa0a6;\
--link:#8ab4f8;--url:#81c995;--line:#3c4043;--accent:#8ab4f8}}\
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
.none{margin:1.5rem 0}";

/// A whole HTML document; `body` must already be escaped.
fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n\
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

fn render_home(docs: u64) -> String {
    let body = format!(
        "<main class=\"wrap home\">\n<h1>Plumb</h1>\n<p class=\"tag\">Find a site by its name.</p>\n\
         {}\n<p class=\"s\">{} sites indexed</p>\n</main>",
        search_form("", true),
        group_thousands(docs)
    );
    page("Plumb Search", &body)
}

fn results_header(query: &str) -> String {
    format!(
        "<header><a class=\"logo\" href=\"/\">Plumb</a>{}</header>",
        search_form(query, false)
    )
}

fn render_results(query: &str, hits: &[Hit]) -> String {
    let mut body = format!("<div class=\"wrap\">\n{}\n<main>\n", results_header(query));
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
    let api: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let api = escape_html(&format!("/api/search?q={api}"));
    let _ = write!(
        body,
        "<p class=\"s\">As JSON: <a href=\"{api}\">{api}</a></p>\n</main>\n</div>"
    );
    page(&format!("{query} - Plumb Search"), &body)
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
    let _ = writeln!(
        out,
        "<div class=\"m\">{} &middot; score {:.3} (text {:.3}, link {:.3})</div></li>",
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
        let app = router(backend);
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let response = app.oneshot(request).await.unwrap();
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

    #[test]
    fn long_queries_are_cut() {
        let params = SearchParams {
            q: format!("  us \n bank {}", "x".repeat(500)),
            limit: None,
        };
        let query = params.query();
        assert!(query.starts_with("us bank x"));
        assert_eq!(query.chars().count(), MAX_QUERY_CHARS);
        assert_eq!(params.limit(), DEFAULT_LIMIT);
    }
}
