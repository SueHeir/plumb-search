//! Fetching homepages: robots.txt, politeness delays, redirects and size limits.

use std::borrow::Cow;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use plumb_core::{now_unix, registrable_domain};
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION};
use reqwest::{redirect, Client, Response, StatusCode};
use texting_robots::Robot;
use tracing::{debug, info, warn};
use url::Url;

use crate::{
    dns, extract_page_meta, CrawlConfig, CrawlOutcome, CrawlResult, CrawlTarget, CrawledPage,
    ROBOTS_TOKEN,
};

const ROBOTS_PATH: &str = "/robots.txt";
/// robots.txt is parsed up to this size; RFC 9309 asks for at least 500 KiB.
const ROBOTS_MAX_BYTES: usize = 500 * 1024;
/// Longest robots.txt `Crawl-delay` honoured; longer ones are cut to this.
const MAX_CRAWL_DELAY: Duration = Duration::from_secs(30);
/// `Accept` header for page requests.
const ACCEPT_HTML: &str = "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8";

/// Fetches each target's homepage, at most `cfg.concurrency` at a time,
/// returning one result per target (in any order). robots.txt rules:
/// a 2xx robots.txt is obeyed for [`ROBOTS_TOKEN`]; a 4xx means no rules;
/// a 5xx or an unreachable robots.txt means do not crawl (RFC 9309).
///
/// For each target:
///
/// 1. `robots.txt` is fetched from the start URL's origin:
///    - 2xx: the first 500 KiB are parsed and obeyed. A disallowed start URL
///      gives [`CrawlOutcome::RobotsDisallowed`]; a file that cannot be
///      parsed gives [`CrawlOutcome::Failed`].
///    - 4xx other than 429: no rules, everything is allowed.
///    - 5xx or 429: not crawled, [`CrawlOutcome::Failed`].
///    - Network error or timeout: not crawled, [`CrawlOutcome::Failed`].
///    - A redirect to another site, or more than `cfg.max_redirects`
///      redirects: no rules, as RFC 9309 allows for redirects it cannot
///      follow.
///
///    Failed messages about robots.txt start with `robots.txt`.
/// 2. Waits `cfg.per_host_delay`, or the robots.txt `Crawl-delay` when that
///    is longer (capped at 30 seconds).
/// 3. Fetches the start URL. A redirect to another registrable domain gives
///    [`CrawlOutcome::OffsiteRedirect`] with the URL it points to. Ending on
///    a path robots.txt disallows gives [`CrawlOutcome::RobotsDisallowed`],
///    a non-2xx status gives [`CrawlOutcome::HttpStatus`], and a
///    `Content-Type` that is present and not HTML gives
///    [`CrawlOutcome::NotHtml`]. Otherwise at most `cfg.max_bytes` of the
///    body are read and parsed with [`extract_page_meta`] into
///    [`CrawlOutcome::Fetched`].
///
/// Redirects, for robots.txt and pages alike, are followed (up to
/// `cfg.max_redirects`) only while they stay on the start URL's site: the
/// same host, or a host with the same registrable domain, like `usbank.com`
/// and `www.usbank.com`. Nothing is ever requested from another site, whose
/// robots.txt has not been checked.
///
/// Unless `cfg.allow_private_addresses` is set, host names are only
/// connected to on globally routable addresses: a target whose name
/// resolves only to loopback, private, link-local or other special
/// addresses fails with [`CrawlOutcome::Failed`] (at its robots.txt
/// request) without anything being sent, so a hostile domain cannot point
/// the crawler at the operator's own network. That relies on connecting to
/// each site directly, which the crawler does unless `cfg.use_system_proxy`
/// is set: a proxy looks up target names itself.
///
/// Errors never stop the batch; each becomes that target's
/// [`CrawlOutcome::Failed`]. Must run inside a Tokio runtime.
pub async fn crawl_homepages(targets: Vec<CrawlTarget>, cfg: &CrawlConfig) -> Vec<CrawlResult> {
    if targets.is_empty() {
        return Vec::new();
    }
    let client = match build_client(cfg) {
        Ok(client) => client,
        Err(err) => {
            let error = format!("building the HTTP client: {}", error_text(err));
            warn!("cannot crawl {} homepages: {error}", targets.len());
            return targets
                .into_iter()
                .map(|target| CrawlResult {
                    domain: target.domain,
                    outcome: CrawlOutcome::Failed {
                        error: error.clone(),
                    },
                })
                .collect();
        }
    };
    // buffer_unordered(0) would never start anything.
    let concurrency = cfg.concurrency.max(1);
    info!(
        "crawling {} homepages, {concurrency} at a time",
        targets.len()
    );
    let results: Vec<CrawlResult> = stream::iter(targets)
        .map(|target| crawl_target(&client, cfg, target))
        .buffer_unordered(concurrency)
        .collect()
        .await;
    log_summary(&results);
    results
}

/// One client for the whole batch, so connections are reused.
fn build_client(cfg: &CrawlConfig) -> reqwest::Result<Client> {
    let builder = Client::builder()
        .user_agent(cfg.user_agent.as_str())
        .timeout(cfg.timeout)
        .gzip(true)
        .redirect(redirect_policy(cfg.max_redirects))
        .dns_resolver(dns::Resolver {
            allow_private: cfg.allow_private_addresses,
        });
    // reqwest uses the system proxy unless told not to; behind a proxy the
    // resolver above would see only the proxy's name, not the targets'.
    let builder = if cfg.use_system_proxy {
        builder
    } else {
        builder.no_proxy()
    };
    builder.build()
}

/// [`redirect::Policy::limited`], except that no request follows a redirect
/// to another site (see [`same_site`]): the 3xx comes back as the response,
/// and nothing is sent to that site. [`redirect_target`] reads where it
/// pointed.
fn redirect_policy(max_redirects: usize) -> redirect::Policy {
    let limited = redirect::Policy::limited(max_redirects);
    redirect::Policy::custom(move |attempt| {
        // The first URL of the chain is the one the request was made for.
        let leaves_site = attempt
            .previous()
            .first()
            .is_some_and(|first| !same_site(first, attempt.url()));
        if leaves_site {
            attempt.stop()
        } else {
            limited.redirect(attempt)
        }
    })
}

async fn crawl_target(client: &Client, cfg: &CrawlConfig, target: CrawlTarget) -> CrawlResult {
    let outcome = crawl_outcome(client, cfg, &target).await;
    debug!("{} ({}): {}", target.domain, target.url, describe(&outcome));
    CrawlResult {
        domain: target.domain,
        outcome,
    }
}

async fn crawl_outcome(client: &Client, cfg: &CrawlConfig, target: &CrawlTarget) -> CrawlOutcome {
    let start = match start_url(&target.url) {
        Ok(url) => url,
        Err(error) => return CrawlOutcome::Failed { error },
    };
    let robot = match fetch_robots(client, &start).await {
        Robots::Rules(robot) if !robot.allowed(start.as_str()) => {
            return CrawlOutcome::RobotsDisallowed
        }
        Robots::Rules(robot) => Some(robot),
        Robots::NoRules => None,
        Robots::DoNotCrawl(error) => return CrawlOutcome::Failed { error },
    };
    let delay = page_delay(cfg.per_host_delay, robot.as_ref().and_then(|r| r.delay));
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    fetch_page(client, cfg, target, &start, robot.as_ref()).await
}

fn start_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|err| format!("invalid URL {raw:?}: {err}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("not an http(s) URL: {raw:?}"));
    }
    Ok(url)
}

/// What robots.txt says about crawling a host.
enum Robots {
    /// A robots.txt was fetched and parsed; obey it.
    Rules(Robot),
    /// There is no usable robots.txt (4xx, or a redirect we do not
    /// follow), so there are no restrictions.
    NoRules,
    /// Do not crawl: robots.txt answered 5xx or 429, could not be fetched,
    /// or could not be parsed. The message says which.
    DoNotCrawl(String),
}

async fn fetch_robots(client: &Client, start: &Url) -> Robots {
    let url = robots_url(start);
    let response = match client.get(url.clone()).send().await {
        Ok(response) => response,
        // Too many redirects: RFC 9309 lets us treat robots.txt as unavailable.
        Err(err) if err.is_redirect() => return Robots::NoRules,
        Err(err) => return Robots::DoNotCrawl(format!("robots.txt: {}", error_text(err))),
    };
    let status = response.status();
    if status.is_success() {
        let body = match read_body(response, ROBOTS_MAX_BYTES).await {
            Ok(body) => body,
            Err(err) => return Robots::DoNotCrawl(format!("robots.txt: {}", error_text(err))),
        };
        return match Robot::new(ROBOTS_TOKEN, &body) {
            Ok(robot) => Robots::Rules(robot),
            Err(err) => Robots::DoNotCrawl(format!("robots.txt: cannot parse it: {err:#}")),
        };
    }
    if status.is_redirection() {
        // The redirect policy stopped at a hop to another site, or the
        // redirect had no usable Location.
        let to = redirect_target(&response).map(String::from);
        debug!("{url}: robots.txt redirects to {to:?}, not followed; no rules apply");
        return Robots::NoRules;
    }
    if status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS {
        return Robots::NoRules;
    }
    Robots::DoNotCrawl(format!("robots.txt: HTTP {status}"))
}

/// `/robots.txt` on the URL's origin, without credentials.
fn robots_url(url: &Url) -> Url {
    let mut robots = url.clone();
    robots.set_path(ROBOTS_PATH);
    robots.set_query(None);
    robots.set_fragment(None);
    // Only fails for URLs that cannot have credentials, which have none to strip.
    let _ = robots.set_username("");
    let _ = robots.set_password(None);
    robots
}

/// How long to wait between the robots.txt request and the page request:
/// `per_host_delay`, or the robots.txt `Crawl-delay` (seconds, capped at
/// [`MAX_CRAWL_DELAY`]) when that is longer.
fn page_delay(per_host_delay: Duration, crawl_delay: Option<f32>) -> Duration {
    let robots_delay = match crawl_delay {
        Some(secs) if secs > 0.0 => {
            Duration::from_secs_f32(secs.min(MAX_CRAWL_DELAY.as_secs_f32()))
        }
        // Absent, zero, negative or NaN.
        _ => Duration::ZERO,
    };
    per_host_delay.max(robots_delay)
}

async fn fetch_page(
    client: &Client,
    cfg: &CrawlConfig,
    target: &CrawlTarget,
    start: &Url,
    robot: Option<&Robot>,
) -> CrawlOutcome {
    let response = match client
        .get(start.clone())
        .header(ACCEPT, ACCEPT_HTML)
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            return CrawlOutcome::Failed {
                error: error_text(err),
            }
        }
    };
    // The redirect policy hands back a redirect to another site unfollowed.
    if let Some(to) = redirect_target(&response).filter(|to| !same_site(start, to)) {
        return CrawlOutcome::OffsiteRedirect {
            final_url: to.into(),
        };
    }
    let final_url = response.url().clone();
    // Only reachable if the redirect policy changes: every hop it follows
    // stays on the site.
    if !same_site(start, &final_url) {
        return CrawlOutcome::OffsiteRedirect {
            final_url: final_url.into(),
        };
    }
    if final_url != *start && robot.is_some_and(|robot| !robot.allowed(final_url.as_str())) {
        return CrawlOutcome::RobotsDisallowed;
    }
    let status = response.status();
    if !status.is_success() {
        return CrawlOutcome::HttpStatus {
            status: status.as_u16(),
        };
    }
    if let Some(content_type) = content_type(&response) {
        if !is_html(&content_type) {
            return CrawlOutcome::NotHtml { content_type };
        }
    }
    let body = match read_body(response, cfg.max_bytes).await {
        Ok(body) => body,
        Err(err) => {
            return CrawlOutcome::Failed {
                error: error_text(err),
            }
        }
    };
    let fetched_at = now_unix();
    // Parsing is CPU work; keep it off the threads driving the other fetches.
    let base_url = final_url.clone();
    let parsed =
        tokio::task::spawn_blocking(move || extract_page_meta(&base_url, &decode_html(&body)))
            .await;
    match parsed {
        Ok(meta) => CrawlOutcome::Fetched(CrawledPage {
            domain: target.domain.clone(),
            final_url: final_url.into(),
            status: status.as_u16(),
            fetched_at,
            meta,
        }),
        Err(err) => CrawlOutcome::Failed {
            error: format!("parsing the page: {err}"),
        },
    }
}

/// Where a 3xx response points: its `Location`, resolved against the
/// response's URL the way the redirect machinery resolves it. `None` for
/// other statuses or a missing or unusable `Location`.
fn redirect_target(response: &Response) -> Option<Url> {
    if !response.status().is_redirection() {
        return None;
    }
    let location = response.headers().get(LOCATION)?;
    let location = String::from_utf8_lossy(location.as_bytes());
    response.url().join(&location).ok()
}

/// Same host, or two hosts with the same registrable domain
/// (`usbank.com` and `www.usbank.com`). Ports and schemes do not matter.
fn same_site(a: &Url, b: &Url) -> bool {
    if a.host_str().is_some() && a.host_str() == b.host_str() {
        return true;
    }
    match (
        registrable_domain(a.as_str()),
        registrable_domain(b.as_str()),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The `Content-Type` header, if present and not blank.
fn content_type(response: &Response) -> Option<String> {
    let value = response.headers().get(CONTENT_TYPE)?;
    let value = String::from_utf8_lossy(value.as_bytes()).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn is_html(content_type: &str) -> bool {
    let mime = content_type.split(';').next().unwrap_or_default().trim();
    mime.eq_ignore_ascii_case("text/html") || mime.eq_ignore_ascii_case("application/xhtml+xml")
}

/// Reads the body chunk by chunk, keeping at most `max_bytes`. Stops
/// reading once the limit is reached and drops the rest. With gzip the
/// limit applies to the decompressed bytes.
async fn read_body(mut response: Response, max_bytes: usize) -> reqwest::Result<Vec<u8>> {
    let mut body = Vec::new();
    while body.len() < max_bytes {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let take = chunk.len().min(max_bytes - body.len());
        body.extend_from_slice(&chunk[..take]);
    }
    Ok(body)
}

/// Decodes a page as UTF-8, replacing invalid bytes.
///
/// Known gap: the charset from `Content-Type` or `<meta charset>` is
/// ignored, so pages in legacy encodings (windows-1252, Shift_JIS, GBK,
/// ...) come out partly garbled. Fixing that needs a decoder such as
/// `encoding_rs`.
fn decode_html(body: &[u8]) -> Cow<'_, str> {
    let body = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body);
    String::from_utf8_lossy(body)
}

/// An error and its causes on one line, like `a: b: c`.
fn error_text(err: reqwest::Error) -> String {
    format!("{:#}", anyhow::Error::new(err))
}

fn describe(outcome: &CrawlOutcome) -> String {
    match outcome {
        CrawlOutcome::Fetched(page) => format!(
            "fetched {} ({} outbound links)",
            page.final_url,
            page.meta.links.len()
        ),
        CrawlOutcome::RobotsDisallowed => "disallowed by robots.txt".to_string(),
        CrawlOutcome::OffsiteRedirect { final_url } => {
            format!("redirected to another site: {final_url}")
        }
        CrawlOutcome::HttpStatus { status } => format!("HTTP {status}"),
        CrawlOutcome::NotHtml { content_type } => format!("not HTML ({content_type})"),
        CrawlOutcome::Failed { error } => format!("failed: {error}"),
    }
}

fn log_summary(results: &[CrawlResult]) {
    let (mut fetched, mut disallowed, mut offsite, mut http, mut not_html, mut failed) =
        (0, 0, 0, 0, 0, 0);
    for result in results {
        match result.outcome {
            CrawlOutcome::Fetched(_) => fetched += 1,
            CrawlOutcome::RobotsDisallowed => disallowed += 1,
            CrawlOutcome::OffsiteRedirect { .. } => offsite += 1,
            CrawlOutcome::HttpStatus { .. } => http += 1,
            CrawlOutcome::NotHtml { .. } => not_html += 1,
            CrawlOutcome::Failed { .. } => failed += 1,
        }
    }
    info!(
        "crawled {} homepages: {fetched} fetched, {disallowed} disallowed by robots.txt, \
         {offsite} redirected to other sites, {http} HTTP errors, {not_html} not HTML, \
         {failed} failed",
        results.len()
    );
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use axum::extract::{Request, State};
    use axum::http::{header, StatusCode};
    use axum::middleware::{self, Next};
    use axum::response::{Html, Response};
    use axum::routing::get;
    use axum::Router;
    use plumb_core::SiteRecord;

    use super::*;
    use crate::{to_records, OutLink, USER_AGENT};

    /// A record's link texts as (text, number of linking sites).
    fn link_texts_of(record: &SiteRecord) -> Vec<(&str, u32)> {
        record
            .link_texts
            .iter()
            .map(|lt| (lt.text.as_str(), lt.count))
            .collect()
    }

    /// A request a test server received.
    #[derive(Debug, Clone)]
    struct Hit {
        path: String,
        user_agent: String,
        at: Instant,
    }

    /// The requests a test server received, in arrival order.
    #[derive(Debug, Clone, Default)]
    struct Hits(Arc<Mutex<Vec<Hit>>>);

    impl Hits {
        fn paths(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|hit| hit.path.clone())
                .collect()
        }

        fn get(&self, path: &str) -> Hit {
            let hits = self.0.lock().unwrap();
            let hit = hits.iter().find(|hit| hit.path == path);
            hit.cloned()
                .unwrap_or_else(|| panic!("{path} was never requested"))
        }
    }

    async fn record_hit(State(hits): State<Hits>, request: Request, next: Next) -> Response {
        let user_agent = request
            .headers()
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        hits.0.lock().unwrap().push(Hit {
            path: request.uri().path().to_string(),
            user_agent,
            at: Instant::now(),
        });
        next.run(request).await
    }

    /// Serves the router built for the chosen port on 127.0.0.1 for the
    /// rest of the test, recording every request.
    async fn serve(app: impl FnOnce(u16) -> Router) -> (u16, Hits) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Hits::default();
        let app = app(port).layer(middleware::from_fn_with_state(hits.clone(), record_hit));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, hits)
    }

    fn config() -> CrawlConfig {
        CrawlConfig {
            concurrency: 4,
            per_host_delay: Duration::from_millis(10),
            timeout: Duration::from_secs(3),
            ..CrawlConfig::default()
        }
    }

    /// [`config`] for tests that reach the local server by the name
    /// `localhost`, which resolves to a loopback address.
    fn config_for_localhost() -> CrawlConfig {
        CrawlConfig {
            allow_private_addresses: true,
            ..config()
        }
    }

    fn target(port: u16, path: &str) -> CrawlTarget {
        CrawlTarget {
            domain: "example.test".into(),
            url: format!("http://127.0.0.1:{port}{path}"),
        }
    }

    async fn crawl_one(target: CrawlTarget, cfg: &CrawlConfig) -> CrawlOutcome {
        let mut results = crawl_homepages(vec![target.clone()], cfg).await;
        assert_eq!(results.len(), 1);
        let result = results.pop().unwrap();
        assert_eq!(result.domain, target.domain);
        result.outcome
    }

    fn expect_fetched(outcome: CrawlOutcome) -> CrawledPage {
        match outcome {
            CrawlOutcome::Fetched(page) => page,
            other => panic!("expected a fetched page, got {other:?}"),
        }
    }

    fn expect_failed(outcome: CrawlOutcome) -> String {
        match outcome {
            CrawlOutcome::Failed { error } => error,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    const HOME: &str = r#"<!doctype html>
        <html><head>
          <title>
            Example   Bank | Home
          </title>
          <meta name="description" content="Checking, savings and loans.">
          <meta property="og:site_name" content="Example Bank">
        </head><body>
          <a href="/about">About us</a>
          <a href="https://www.partner.org/?ref=home#top">Our Partner</a>
          <a href="https://social.example.net/examplebank"><img src="f.png" alt="Follow us"></a>
        </body></html>"#;

    fn home() -> Router {
        Router::new().route("/", get(|| async { Html(HOME) }))
    }

    #[tokio::test]
    async fn fetches_a_homepage() {
        let (port, hits) = serve(|_| {
            home().route(
                "/robots.txt",
                get(|| async { "User-agent: *\nDisallow: /private/\n" }),
            )
        })
        .await;
        let before = now_unix();
        let page = expect_fetched(crawl_one(target(port, "/"), &config()).await);

        assert_eq!(page.domain, "example.test");
        assert_eq!(page.final_url, format!("http://127.0.0.1:{port}/"));
        assert_eq!(page.status, 200);
        assert!(page.fetched_at >= before);
        assert_eq!(page.meta.title.as_deref(), Some("Example Bank | Home"));
        assert_eq!(
            page.meta.description.as_deref(),
            Some("Checking, savings and loans.")
        );
        assert_eq!(page.meta.site_name.as_deref(), Some("Example Bank"));
        assert_eq!(
            page.meta.links,
            [
                OutLink {
                    url: "https://www.partner.org/?ref=home".into(),
                    target_domain: "partner.org".into(),
                    text: "our partner".into(),
                },
                OutLink {
                    url: "https://social.example.net/examplebank".into(),
                    target_domain: "example.net".into(),
                    text: "follow us".into(),
                },
            ]
        );
        assert_eq!(hits.paths(), ["/robots.txt", "/"]);
        assert_eq!(hits.get("/").user_agent, USER_AGENT);
        assert_eq!(hits.get("/robots.txt").user_agent, USER_AGENT);
    }

    #[tokio::test]
    async fn obeys_robots_txt() {
        let cases = [
            (
                "User-agent: PlumbSearch\nDisallow: /\n\nUser-agent: *\nAllow: /\n",
                false,
            ),
            ("user-agent: plumbsearch\ndisallow: /\n", false),
            ("User-agent: *\nDisallow: /\n", false),
            ("User-agent: *\nDisallow: /\nAllow: /$\n", true),
            ("User-agent: OtherBot\nDisallow: /\n", true),
            (
                "User-agent: PlumbSearch\nDisallow: /private/\n\nUser-agent: *\nDisallow: /\n",
                true,
            ),
        ];
        for (robots, allowed) in cases {
            let (port, hits) =
                serve(|_| home().route("/robots.txt", get(move || async move { robots }))).await;
            let outcome = crawl_one(target(port, "/"), &config()).await;
            if allowed {
                expect_fetched(outcome);
                assert_eq!(hits.paths(), ["/robots.txt", "/"], "{robots}");
            } else {
                assert_eq!(outcome, CrawlOutcome::RobotsDisallowed, "{robots}");
                assert_eq!(hits.paths(), ["/robots.txt"], "{robots}");
            }
        }
    }

    #[tokio::test]
    async fn robots_txt_4xx_allows_everything() {
        // No /robots.txt route: 404.
        let (port, hits) = serve(|_| home()).await;
        expect_fetched(crawl_one(target(port, "/"), &config()).await);
        assert_eq!(hits.paths(), ["/robots.txt", "/"]);

        let (port, _) =
            serve(|_| home().route("/robots.txt", get(|| async { StatusCode::FORBIDDEN }))).await;
        expect_fetched(crawl_one(target(port, "/"), &config()).await);
    }

    #[tokio::test]
    async fn robots_txt_5xx_or_429_means_no_crawl() {
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            let (port, hits) = serve(|_| {
                home().route(
                    "/robots.txt",
                    get(move || async move { (status, "try again later") }),
                )
            })
            .await;
            let error = expect_failed(crawl_one(target(port, "/"), &config()).await);
            assert!(error.starts_with("robots.txt"), "{error}");
            assert!(error.contains(status.as_str()), "{error}");
            assert_eq!(hits.paths(), ["/robots.txt"]);
        }
    }

    #[tokio::test]
    async fn unreachable_host_is_not_crawled() {
        // Nothing can listen on port 0, so connecting fails right away.
        let error = expect_failed(crawl_one(target(0, "/"), &config()).await);
        assert!(error.starts_with("robots.txt"), "{error}");
    }

    #[tokio::test]
    async fn client_errors_fail_every_target() {
        let cfg = CrawlConfig {
            user_agent: "bad\nagent".into(),
            ..config()
        };
        let mut other = target(0, "/");
        other.domain = "other.test".into();
        let results = crawl_homepages(vec![target(0, "/"), other], &cfg).await;
        assert_eq!(results.len(), 2);
        for result in results {
            let error = expect_failed(result.outcome);
            assert!(error.starts_with("building the HTTP client"), "{error}");
        }
    }

    #[tokio::test]
    async fn zero_concurrency_still_crawls() {
        let cfg = CrawlConfig {
            concurrency: 0,
            ..config()
        };
        let results = crawl_homepages(vec![target(0, "/")], &cfg).await;
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn private_addresses_are_refused_unless_allowed() {
        // Unlike the IP address 127.0.0.1 the other tests use, the name
        // "localhost" is looked up, and it resolves to loopback.
        let (port, hits) = serve(|_| home()).await;
        let by_name = CrawlTarget {
            domain: "example.test".into(),
            url: format!("http://localhost:{port}/"),
        };

        assert!(!CrawlConfig::default().allow_private_addresses);
        let error = expect_failed(crawl_one(by_name.clone(), &config()).await);
        assert!(error.starts_with("robots.txt"), "{error}");
        assert!(
            error.contains("localhost resolves only to non-public addresses"),
            "{error}"
        );
        assert!(hits.paths().is_empty(), "{:?}", hits.paths());

        let page = expect_fetched(crawl_one(by_name, &config_for_localhost()).await);
        assert_eq!(page.final_url, format!("http://localhost:{port}/"));
        assert_eq!(hits.paths(), ["/robots.txt", "/"]);
    }

    #[tokio::test]
    async fn system_proxy_is_used_only_when_asked() {
        let (port, hits) = serve(|_| home()).await;
        let (proxy_port, proxy_hits) = serve(|_| home()).await;
        let url = format!("http://localhost:{port}/");

        // HTTP_PROXY is ignored by default, so the crawler looks up
        // "localhost" itself and refuses it.
        assert!(!CrawlConfig::default().use_system_proxy);
        let error = expect_failed(crawl_behind_proxy(&url, proxy_port, false).await);
        assert!(
            error.contains("localhost resolves only to non-public addresses"),
            "{error}"
        );
        assert!(proxy_hits.paths().is_empty(), "{:?}", proxy_hits.paths());

        // With the system proxy on, every request goes to the proxy, which
        // looks the name up itself: the private-address check never sees it.
        let page = expect_fetched(crawl_behind_proxy(&url, proxy_port, true).await);
        assert_eq!(page.final_url, url);
        assert_eq!(proxy_hits.paths(), ["/robots.txt", "/"]);
        assert!(hits.paths().is_empty(), "{:?}", hits.paths());
    }

    /// Holds the job of [`crawl_in_child_process`].
    const CHILD_JOB: &str = "PLUMB_CRAWL_TEST_CHILD_JOB";
    /// Precedes the outcome [`crawl_in_child_process`] prints.
    const CHILD_OUTCOME: &str = "child outcome: ";

    /// Crawls `url` with [`config`] and `use_system_proxy` in a child process
    /// whose `HTTP_PROXY` is 127.0.0.1:`proxy_port`. reqwest reads proxy
    /// settings from the environment, which a test must not change in its
    /// own process while other threads may be reading it.
    async fn crawl_behind_proxy(
        url: &str,
        proxy_port: u16,
        use_system_proxy: bool,
    ) -> CrawlOutcome {
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
        child.args([
            "--exact",
            "crawl::tests::crawl_in_child_process",
            "--nocapture",
        ]);
        for name in [
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
            "REQUEST_METHOD",
        ] {
            child.env_remove(name);
        }
        child
            .env("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}"))
            .env(
                CHILD_JOB,
                serde_json::to_string(&(url, use_system_proxy)).unwrap(),
            );

        let output = child.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let outcome = stdout
            .lines()
            .find_map(|line| line.split_once(CHILD_OUTCOME).map(|(_, json)| json));
        match outcome {
            Some(json) if output.status.success() => serde_json::from_str(json).unwrap(),
            _ => panic!(
                "child process failed ({}):\n{stdout}{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        }
    }

    /// The child process of [`crawl_behind_proxy`]; does nothing in a normal
    /// test run.
    #[tokio::test]
    async fn crawl_in_child_process() {
        let Ok(job) = std::env::var(CHILD_JOB) else {
            return;
        };
        let (url, use_system_proxy): (String, bool) = serde_json::from_str(&job).unwrap();
        let cfg = CrawlConfig {
            use_system_proxy,
            ..config()
        };
        let target = CrawlTarget {
            domain: "example.test".into(),
            url,
        };
        let outcome = crawl_one(target, &cfg).await;
        println!(
            "{CHILD_OUTCOME}{}",
            serde_json::to_string(&outcome).unwrap()
        );
    }

    #[tokio::test]
    async fn bad_target_urls_fail_without_fetching() {
        for url in [
            "not a url",
            "example.com",
            "ftp://example.com/",
            "mailto:a@b.c",
        ] {
            let bad = CrawlTarget {
                domain: "example.com".into(),
                url: url.into(),
            };
            let error = expect_failed(crawl_one(bad, &config()).await);
            assert!(error.contains(url), "{error}");
        }
    }

    #[tokio::test]
    async fn follows_same_host_redirects() {
        let (port, hits) = serve(|_| {
            Router::new()
                .route(
                    "/",
                    get(|| async {
                        (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, "/home")])
                    }),
                )
                .route("/home", get(|| async { Html(HOME) }))
        })
        .await;
        let page = expect_fetched(crawl_one(target(port, "/"), &config()).await);
        assert_eq!(page.final_url, format!("http://127.0.0.1:{port}/home"));
        assert_eq!(page.meta.title.as_deref(), Some("Example Bank | Home"));
        assert_eq!(hits.paths(), ["/robots.txt", "/", "/home"]);
    }

    #[tokio::test]
    async fn redirect_to_a_disallowed_path_is_not_used() {
        let (port, _) = serve(|_| {
            Router::new()
                .route(
                    "/robots.txt",
                    get(|| async { "User-agent: *\nDisallow: /members/\n" }),
                )
                .route(
                    "/",
                    get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/members/")]) }),
                )
                .route("/members/", get(|| async { Html(HOME) }))
        })
        .await;
        let outcome = crawl_one(target(port, "/"), &config()).await;
        assert_eq!(outcome, CrawlOutcome::RobotsDisallowed);
    }

    #[tokio::test]
    async fn offsite_redirects_are_reported_not_followed() {
        // "localhost" is another host than "127.0.0.1" with no registrable
        // domain, so it counts as another site, yet with private addresses
        // allowed it would reach this server: a request for /new would show
        // up in the hits.
        let cfg = config_for_localhost();
        let moved = |port: u16| {
            Router::new()
                .route(
                    "/old",
                    get(move || async move {
                        let to = format!("http://localhost:{port}/new?from=old");
                        (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, to)])
                    }),
                )
                .route(
                    "/chain",
                    get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/hop")]) }),
                )
                .route(
                    "/hop",
                    get(move || async move {
                        // Protocol-relative, resolved against /hop's URL.
                        let to = format!("//localhost:{port}/new");
                        (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, to)])
                    }),
                )
                .route("/new", get(|| async { Html(HOME) }))
        };

        let (port, hits) = serve(moved).await;
        let outcome = crawl_one(target(port, "/old"), &cfg).await;
        assert_eq!(
            outcome,
            CrawlOutcome::OffsiteRedirect {
                final_url: format!("http://localhost:{port}/new?from=old")
            }
        );
        assert_eq!(hits.paths(), ["/robots.txt", "/old"]);

        // Same-site hops are followed up to the one that leaves the site.
        let (port, hits) = serve(moved).await;
        let outcome = crawl_one(target(port, "/chain"), &cfg).await;
        assert_eq!(
            outcome,
            CrawlOutcome::OffsiteRedirect {
                final_url: format!("http://localhost:{port}/new")
            }
        );
        assert_eq!(hits.paths(), ["/robots.txt", "/chain", "/hop"]);
    }

    #[tokio::test]
    async fn robots_txt_redirects_stay_on_the_site() {
        // Same host: followed and obeyed.
        let (port, hits) = serve(|_| {
            home()
                .route(
                    "/robots.txt",
                    get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/robots-v2.txt")]) }),
                )
                .route(
                    "/robots-v2.txt",
                    get(|| async { "User-agent: *\nDisallow: /\n" }),
                )
        })
        .await;
        let outcome = crawl_one(target(port, "/"), &config()).await;
        assert_eq!(outcome, CrawlOutcome::RobotsDisallowed);
        assert_eq!(hits.paths(), ["/robots.txt", "/robots-v2.txt"]);

        // Another site: not followed (though allowed to reach this server),
        // so there are no rules.
        let (port, hits) = serve(|port| {
            home()
                .route(
                    "/robots.txt",
                    get(move || async move {
                        let to = format!("http://localhost:{port}/elsewhere.txt");
                        (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, to)])
                    }),
                )
                .route(
                    "/elsewhere.txt",
                    get(|| async { "User-agent: *\nDisallow: /\n" }),
                )
        })
        .await;
        expect_fetched(crawl_one(target(port, "/"), &config_for_localhost()).await);
        assert_eq!(hits.paths(), ["/robots.txt", "/"]);

        // A redirect loop: more than max_redirects also means no rules.
        let (port, _) = serve(|_| {
            home().route(
                "/robots.txt",
                get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/robots.txt")]) }),
            )
        })
        .await;
        expect_fetched(crawl_one(target(port, "/"), &config()).await);
    }

    #[tokio::test]
    async fn non_html_pages_are_skipped() {
        let (port, hits) = serve(|_| {
            Router::new().route(
                "/",
                get(|| async { ([(header::CONTENT_TYPE, "application/json")], "{}") }),
            )
        })
        .await;
        let outcome = crawl_one(target(port, "/"), &config()).await;
        assert_eq!(
            outcome,
            CrawlOutcome::NotHtml {
                content_type: "application/json".into()
            }
        );
        assert_eq!(hits.paths(), ["/robots.txt", "/"]);

        // XHTML, and no Content-Type at all, are parsed.
        let (port, _) = serve(|_| {
            Router::new().route(
                "/",
                get(|| async {
                    let body = "<html><head><title>XHTML</title></head></html>";
                    ([(header::CONTENT_TYPE, "application/xhtml+xml")], body)
                }),
            )
        })
        .await;
        let page = expect_fetched(crawl_one(target(port, "/"), &config()).await);
        assert_eq!(page.meta.title.as_deref(), Some("XHTML"));
        let (port, _) = serve(|_| {
            Router::new().route(
                "/",
                get(|| async {
                    let body = axum::body::Body::from("<title>No type</title>");
                    Response::new(body)
                }),
            )
        })
        .await;
        let page = expect_fetched(crawl_one(target(port, "/"), &config()).await);
        assert_eq!(page.meta.title.as_deref(), Some("No type"));
    }

    #[tokio::test]
    async fn error_statuses_are_reported() {
        let (port, _) = serve(|_| {
            Router::new().route(
                "/",
                get(|| async { (StatusCode::SERVICE_UNAVAILABLE, Html("<title>Down</title>")) }),
            )
        })
        .await;
        let outcome = crawl_one(target(port, "/"), &config()).await;
        assert_eq!(outcome, CrawlOutcome::HttpStatus { status: 503 });

        let (port, _) = serve(|_| Router::new()).await;
        let outcome = crawl_one(target(port, "/"), &config()).await;
        assert_eq!(outcome, CrawlOutcome::HttpStatus { status: 404 });

        // A redirect without a Location goes nowhere.
        let (port, _) =
            serve(|_| Router::new().route("/", get(|| async { StatusCode::FOUND }))).await;
        let outcome = crawl_one(target(port, "/"), &config()).await;
        assert_eq!(outcome, CrawlOutcome::HttpStatus { status: 302 });
    }

    #[tokio::test]
    async fn bodies_are_cut_at_max_bytes() {
        let filler = "x".repeat(100_000);
        let body = format!(
            "<html><head><title>Big page</title></head><body><p>{filler}</p>\
             <a href=\"https://late.example.org/\">Late link</a></body></html>"
        );
        let (port, _) =
            serve(|_| Router::new().route("/", get(move || async move { Html(body) }))).await;

        let cfg = CrawlConfig {
            max_bytes: 4096,
            ..config()
        };
        let page = expect_fetched(crawl_one(target(port, "/"), &cfg).await);
        assert_eq!(page.meta.title.as_deref(), Some("Big page"));
        assert!(page.meta.links.is_empty(), "{:?}", page.meta.links);

        // With room for the whole page the late link is there.
        let page = expect_fetched(crawl_one(target(port, "/"), &config()).await);
        assert_eq!(page.meta.links.len(), 1);
    }

    #[tokio::test]
    async fn gzip_bodies_are_decoded() {
        /// `<title>Gzipped page</title>`, compressed with `gzip -9 -n`.
        const GZIPPED: &[u8] = &[
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0xb3, 0x29, 0xc9, 0x2c,
            0xc9, 0x49, 0xb5, 0x73, 0xaf, 0xca, 0x2c, 0x28, 0x48, 0x4d, 0x51, 0x28, 0x48, 0x4c,
            0x4f, 0xb5, 0xd1, 0x87, 0x88, 0x01, 0x00, 0xe0, 0xd1, 0x11, 0xfc, 0x1b, 0x00, 0x00,
            0x00,
        ];
        let (port, _) = serve(|_| {
            Router::new().route(
                "/",
                get(|| async {
                    let headers = [
                        (header::CONTENT_TYPE, "text/html"),
                        (header::CONTENT_ENCODING, "gzip"),
                    ];
                    (headers, GZIPPED)
                }),
            )
        })
        .await;
        let page = expect_fetched(crawl_one(target(port, "/"), &config()).await);
        assert_eq!(page.meta.title.as_deref(), Some("Gzipped page"));
    }

    #[tokio::test]
    async fn read_body_stops_at_the_limit() {
        let (port, _) =
            serve(|_| Router::new().route("/big", get(|| async { "y".repeat(200_000) }))).await;
        let client = build_client(&config()).unwrap();
        let url = format!("http://127.0.0.1:{port}/big");
        for (limit, expected) in [(0, 0), (1000, 1000), (200_000, 200_000), (1 << 20, 200_000)] {
            let response = client.get(&url).send().await.unwrap();
            let body = read_body(response, limit).await.unwrap();
            assert_eq!(body.len(), expected, "limit {limit}");
            assert!(body.iter().all(|&byte| byte == b'y'));
        }
    }

    #[tokio::test]
    async fn waits_between_robots_txt_and_the_page() {
        // robots.txt Crawl-delay, when longer than per_host_delay.
        let (port, hits) = serve(|_| {
            home().route(
                "/robots.txt",
                get(|| async { "User-agent: *\nCrawl-delay: 0.3\n" }),
            )
        })
        .await;
        let cfg = CrawlConfig {
            per_host_delay: Duration::ZERO,
            ..config()
        };
        expect_fetched(crawl_one(target(port, "/"), &cfg).await);
        let gap = hits.get("/").at.duration_since(hits.get("/robots.txt").at);
        assert!(gap >= Duration::from_millis(300), "{gap:?}");

        // per_host_delay alone.
        let (port, hits) = serve(|_| home()).await;
        let cfg = CrawlConfig {
            per_host_delay: Duration::from_millis(200),
            ..config()
        };
        expect_fetched(crawl_one(target(port, "/"), &cfg).await);
        let gap = hits.get("/").at.duration_since(hits.get("/robots.txt").at);
        assert!(gap >= Duration::from_millis(200), "{gap:?}");
    }

    #[tokio::test]
    async fn crawl_results_become_records() {
        let (port, _) = serve(|_| {
            Router::new()
                .route(
                    "/alpha",
                    get(|| async {
                        Html(
                            r#"<title>Alpha Co</title>
                            <meta property="og:site_name" content="Alpha">
                            <a href="https://beta.example.com/">Beta Corp</a>
                            <a href="https://www.partner.org/">Partner</a>
                            <a href="https://partner.org/about">About our partner</a>"#,
                        )
                    }),
                )
                .route(
                    "/beta",
                    get(|| async {
                        Html(
                            r#"<title>Beta Corp</title>
                            <a href="https://alpha.example.org/">Alpha!</a>
                            <a href="https://partner.org/">PARTNER</a>"#,
                        )
                    }),
                )
                .route("/gone", get(|| async { StatusCode::GONE }))
        })
        .await;
        let targets = vec![
            CrawlTarget {
                domain: "example.org".into(),
                url: format!("http://127.0.0.1:{port}/alpha"),
            },
            CrawlTarget {
                domain: "example.com".into(),
                url: format!("http://127.0.0.1:{port}/beta"),
            },
            CrawlTarget {
                domain: "gone.example".into(),
                url: format!("http://127.0.0.1:{port}/gone"),
            },
        ];
        let cfg = CrawlConfig {
            concurrency: 2,
            ..config()
        };
        let results = crawl_homepages(targets, &cfg).await;
        let mut domains: Vec<&str> = results.iter().map(|r| r.domain.as_str()).collect();
        domains.sort_unstable();
        assert_eq!(domains, ["example.com", "example.org", "gone.example"]);

        let records = to_records(&results);
        let domains: Vec<&str> = records.iter().map(|r| r.domain.as_str()).collect();
        assert_eq!(domains, ["example.com", "example.org", "partner.org"]);

        let beta = &records[0];
        assert_eq!(beta.title.as_deref(), Some("Beta Corp"));
        assert_eq!(beta.url, Some(format!("http://127.0.0.1:{port}/beta")));
        assert!(beta.crawled_at.is_some());
        assert_eq!(link_texts_of(beta), [("beta corp", 1)]);
        assert_eq!(beta.signals.linking_domains, 1);

        let alpha = &records[1];
        assert_eq!(alpha.title.as_deref(), Some("Alpha Co"));
        assert_eq!(alpha.aliases, ["Alpha"]);
        assert_eq!(link_texts_of(alpha), [("alpha", 1)]);

        // Discovered. "about our partner" is on a deep link, so it is left out.
        let partner = &records[2];
        assert_eq!((partner.title.as_deref(), partner.crawled_at), (None, None));
        assert_eq!(link_texts_of(partner), [("partner", 2)]);
        assert_eq!(partner.signals.linking_domains, 2);
    }

    #[tokio::test]
    async fn no_targets_no_results() {
        assert!(crawl_homepages(Vec::new(), &config()).await.is_empty());
    }

    #[test]
    fn crawl_future_is_send() {
        // Callers spawn crawls on multi-threaded runtimes.
        fn assert_send<T: Send>(_: T) {}
        let cfg = CrawlConfig::default();
        assert_send(crawl_homepages(Vec::new(), &cfg));
    }

    #[test]
    fn same_site_compares_hosts_then_registrable_domains() {
        let url = |s: &str| Url::parse(s).unwrap();
        let same = |a: &str, b: &str| same_site(&url(a), &url(b));
        assert!(same("https://usbank.com/", "https://www.usbank.com/home"));
        assert!(same("http://usbank.com/", "https://usbank.com:8443/"));
        assert!(same("https://news.bbc.co.uk/", "https://www.bbc.co.uk/"));
        assert!(same("http://127.0.0.1:1/", "http://127.0.0.1:2/x"));
        assert!(same("http://intranet/", "http://intranet/home"));
        assert!(!same("https://fb.com/", "https://www.facebook.com/"));
        assert!(!same("https://a.co.uk/", "https://b.co.uk/"));
        assert!(!same("http://127.0.0.1/", "http://localhost/"));
        assert!(!same("http://127.0.0.1/", "http://127.0.0.2/"));
        assert!(!same("http://intranet/", "http://wiki/"));
    }

    #[test]
    fn page_delay_takes_the_longer_wait() {
        let ms = Duration::from_millis;
        assert_eq!(page_delay(ms(1000), None), ms(1000));
        assert_eq!(page_delay(ms(1000), Some(0.5)), ms(1000));
        assert_eq!(page_delay(ms(1000), Some(2.5)), ms(2500));
        assert_eq!(page_delay(ms(0), Some(0.25)), ms(250));
        assert_eq!(page_delay(ms(1000), Some(3600.0)), MAX_CRAWL_DELAY);
        assert_eq!(page_delay(ms(1000), Some(f32::INFINITY)), MAX_CRAWL_DELAY);
        assert_eq!(page_delay(ms(1000), Some(f32::NAN)), ms(1000));
        assert_eq!(page_delay(ms(1000), Some(-5.0)), ms(1000));
        assert_eq!(page_delay(ms(45_000), Some(60.0)), ms(45_000));
    }

    #[test]
    fn html_content_types() {
        for html in [
            "text/html",
            "text/html; charset=UTF-8",
            "TEXT/HTML;charset=utf-8",
            " application/xhtml+xml ",
        ] {
            assert!(is_html(html), "{html}");
        }
        for other in [
            "application/json",
            "text/plain",
            "text/htmlx",
            "image/png",
            "",
        ] {
            assert!(!is_html(other), "{other}");
        }
    }

    #[test]
    fn robots_txt_lives_at_the_origin_root() {
        let url = Url::parse("https://user:pw@www.example.com:8443/a/b?q=1#f").unwrap();
        assert_eq!(
            robots_url(&url).as_str(),
            "https://www.example.com:8443/robots.txt"
        );
    }

    #[test]
    fn decoding_is_lossy_utf8_without_bom() {
        assert_eq!(
            decode_html(b"\xEF\xBB\xBF<title>Caf\xC3\xA9</title>"),
            "<title>Café</title>"
        );
        assert_eq!(
            decode_html(b"<title>Caf\xE9</title>"),
            "<title>Caf\u{FFFD}</title>"
        );
    }
}
