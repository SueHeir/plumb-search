//! Fetching homepages: robots.txt for every origin, politeness delays,
//! redirects followed hop by hop, fallback URLs, and size limits.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{self, FuturesUnordered, StreamExt};
use plumb_core::{is_bot_check_page, now_unix, registrable_domain};
use reqwest::header::{
    ACCEPT, CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, LOCATION,
};
use reqwest::{redirect, Client, ClientBuilder, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use texting_robots::Robot;
use tokio::time::Instant;
use tracing::{debug, info, warn};
use url::{Origin, Url};

use crate::{
    dns, normalize_icon, CrawlConfig, CrawlOutcome, CrawlResult, CrawlTarget, CrawledPage,
    ROBOTS_TOKEN,
};

const ROBOTS_PATH: &str = "/robots.txt";
/// robots.txt is parsed up to this size; RFC 9309 asks for at least 500 KiB.
const ROBOTS_MAX_BYTES: usize = 500 * 1024;
/// A robots.txt whose rules would take more memory than this, by the
/// estimate of [`regex_cost`], is not parsed. Every rule with a `$` needs a
/// compiled regular expression, so 500 KiB of them can take a quarter of a
/// gigabyte. This allows about a thousand short ones; real files have a
/// handful.
const MAX_ROBOTS_REGEX_BYTES: usize = 16 << 20;
/// Longest robots.txt `Crawl-delay` honoured; longer ones are cut to this.
const MAX_CRAWL_DELAY: Duration = Duration::from_secs(30);
/// Where browsers look for a site's icon when its pages name none.
const FAVICON_PATH: &str = "/favicon.ico";
/// Icon files tried per site, `/favicon.ico` included.
const MAX_ICON_TRIES: usize = 3;
/// Icon files are read up to this size; a larger one fails to decode.
const ICON_MAX_BYTES: usize = 256 * 1024;
/// `Accept` header for icon requests.
const ACCEPT_ICON: &str = "image/png,image/x-icon,image/*;q=0.8,*/*;q=0.5";
/// `Accept` header for page requests.
const ACCEPT_HTML: &str = "text/html,application/xhtml+xml;q=0.9,*/*;q=0.8";

/// Fetches each target's homepage, at most `cfg.concurrency` targets at a
/// time, returning one result per target (in any order).
///
/// For each target the crawler starts at `target.url` (by default
/// `https://<domain>/`) and follows redirects itself, one hop at a time:
///
/// 1. Before requesting a URL it checks the robots.txt of the URL's origin
///    (scheme, host and port: RFC 9309 scopes the file to one origin),
///    fetching it on first use. So a redirect into a disallowed path, or to
///    a host whose own robots.txt disallows the page, is never followed:
///    [`CrawlOutcome::RobotsDisallowed`]. robots.txt answers:
///    - 2xx: the first 500 KiB are parsed and obeyed for [`ROBOTS_TOKEN`].
///      A file that cannot be parsed gives [`CrawlOutcome::Failed`], and
///      so does one whose rules would take more than 16 MiB: each rule with
///      a `$` needs a compiled regular expression, so that allows about a
///      thousand of them.
///    - 4xx other than 429: no rules, everything is allowed.
///    - 5xx or 429, or no answer: not crawled, [`CrawlOutcome::Failed`].
///    - A redirect, to any host (RFC 9309 says to follow at least five):
///      followed, and the file it ends at is the origin's. A hop to a
///      private address gives [`CrawlOutcome::Failed`]; more than
///      `cfg.max_redirects` redirects, or one without a usable
///      `Location`, mean no rules.
///
///    Failed messages about robots.txt start with `robots.txt`.
/// 2. A request to a host waits until `cfg.per_host_delay` has passed since
///    that host last answered, or the `Crawl-delay` in the robots.txt of the
///    URL's origin when that is longer (capped at 30 seconds).
/// 3. Redirects are followed, up to `cfg.max_redirects`, while they stay on
///    the site the chain started on: the same host, or a host with the same
///    registrable domain, like `usbank.com` and `www.usbank.com`. A redirect
///    to another site gives [`CrawlOutcome::OffsiteRedirect`] with the URL
///    it points to, which is not requested; one redirect too many gives
///    [`CrawlOutcome::Failed`].
/// 4. The final response must be a 2xx ([`CrawlOutcome::HttpStatus`]
///    otherwise) and HTML or of no stated type ([`CrawlOutcome::NotHtml`]
///    otherwise). At most `cfg.max_bytes` of its body are read and parsed
///    with [`extract_page_meta`] into [`CrawlOutcome::Fetched`], or into
///    [`CrawlOutcome::BotCheck`] when the page is a bot check standing in
///    for the homepage.
///
/// When the start URL gets no answer at all (a failure with `network` set,
/// such as a name that does not resolve or a certificate that does not
/// cover it), the crawler tries `target.known_url` (when it is on the
/// start URL's site), then, for a start URL on the domain itself, the same
/// URL on the `www.` host and over plain http (for the default start URL:
/// `https://www.<domain>/`, then `http://<domain>/`). Any other outcome,
/// an HTTP error status included, is final. When none of them answers, the
/// result is the start URL's failure.
///
/// Unless `cfg.allow_private_addresses` is set, host names are only
/// connected to on globally routable addresses: a host whose name resolves
/// only to loopback, private, link-local or other special addresses is
/// never contacted, as a target or as a redirect hop, so a hostile domain
/// cannot point the crawler at the operator's own network. That relies on
/// connecting to each site directly, which the crawler does unless
/// `cfg.use_system_proxy` is set: a proxy looks up target names itself.
///
/// Errors never stop the batch; each becomes that target's
/// [`CrawlOutcome::Failed`], and so does a target not done within
/// `cfg.target_deadline`. Must run inside a Tokio runtime.
pub async fn crawl_homepages(targets: Vec<CrawlTarget>, cfg: &CrawlConfig) -> Vec<CrawlResult> {
    if targets.is_empty() {
        return Vec::new();
    }
    match build_client(cfg) {
        Ok(client) => crawl_with(&client, targets, cfg).await,
        Err(err) => {
            let error = format!("building the HTTP client: {}", error_text(err));
            warn!("cannot crawl {} homepages: {error}", targets.len());
            targets
                .into_iter()
                .map(|target| CrawlResult {
                    domain: target.domain,
                    outcome: Failure::other(error.clone()).into(),
                })
                .collect()
        }
    }
}

/// Fetches homepages like [`crawl_homepages`], but keeps
/// `cfg.concurrency` of them in flight as targets are added, instead of
/// waiting for the slowest homepage of a batch before starting the next:
/// a site that never answers holds one slot, not the whole crawl.
/// [`HomepageCrawler::next`] returns results in the order they finish.
pub struct HomepageCrawler {
    client: Result<Client, String>,
    cfg: Arc<CrawlConfig>,
    queued: std::collections::VecDeque<CrawlTarget>,
    running: FuturesUnordered<BoxFuture<'static, CrawlResult>>,
}

impl HomepageCrawler {
    pub fn new(cfg: CrawlConfig) -> Self {
        let client = build_client(&cfg)
            .map_err(|err| format!("building the HTTP client: {}", error_text(err)));
        if let Err(error) = &client {
            warn!("cannot crawl homepages: {error}");
        }
        HomepageCrawler {
            client,
            cfg: Arc::new(cfg),
            queued: std::collections::VecDeque::new(),
            running: FuturesUnordered::new(),
        }
    }

    /// Adds homepages to fetch after those added before.
    pub fn push(&mut self, targets: impl IntoIterator<Item = CrawlTarget>) {
        self.queued.extend(targets);
    }

    /// Homepages added and not yet returned by [`Self::next`].
    pub fn pending(&self) -> usize {
        self.queued.len() + self.running.len()
    }

    /// The next homepage to finish; `None` once none is pending. Fetches
    /// only make progress while this is awaited.
    pub async fn next(&mut self) -> Option<CrawlResult> {
        self.start_queued();
        let result = self.running.next().await?;
        self.start_queued();
        Some(result)
    }

    fn start_queued(&mut self) {
        while self.running.len() < self.cfg.concurrency.max(1) {
            let Some(target) = self.queued.pop_front() else {
                return;
            };
            let cfg = Arc::clone(&self.cfg);
            let future: BoxFuture<'static, CrawlResult> = match &self.client {
                Ok(client) => {
                    let client = client.clone();
                    Box::pin(async move { crawl_target(&client, &cfg, target).await })
                }
                Err(error) => {
                    let outcome = Failure::other(error.clone()).into();
                    Box::pin(async move {
                        CrawlResult {
                            domain: target.domain,
                            outcome,
                        }
                    })
                }
            };
            self.running.push(future);
        }
    }
}

/// [`crawl_homepages`] with the client built.
async fn crawl_with(
    client: &Client,
    targets: Vec<CrawlTarget>,
    cfg: &CrawlConfig,
) -> Vec<CrawlResult> {
    // buffer_unordered(0) would never start anything.
    let concurrency = cfg.concurrency.max(1);
    info!(
        "crawling {} homepages, {concurrency} at a time",
        targets.len()
    );
    let results: Vec<CrawlResult> = stream::iter(targets)
        .map(|target| crawl_target(client, cfg, target))
        .buffer_unordered(concurrency)
        .collect()
        .await;
    log_summary(&results);
    results
}

/// Fetches only the icon of each target's site, not its homepage: the
/// `/favicon.ico` of its start URL's origin (`target.url`, or
/// `https://<domain>/`), with robots.txt and the per-host delay obeyed as in
/// a crawl ([`crawl_homepages`]). For a site whose homepage was crawled but
/// whose icon was never kept. `None` for a site with no icon this crawler
/// can read, or no answer. Must run inside a Tokio runtime.
pub async fn fetch_site_icons(
    targets: Vec<CrawlTarget>,
    cfg: &CrawlConfig,
) -> Vec<(String, Option<Vec<u8>>)> {
    let Ok(client) = build_client(cfg) else {
        return Vec::new();
    };
    let client = &client;
    stream::iter(targets)
        .map(|target| async move {
            let icon = match start_url(&target) {
                Ok(url) => Visit::new(client, cfg).fetch_icon_of(&url, &[]).await,
                Err(_) => None,
            };
            (target.domain, icon)
        })
        .buffer_unordered(cfg.concurrency.max(1))
        .collect()
        .await
}

/// A site whose feed to check ([`check_feeds`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedTarget {
    /// The site's homepage, which names its feed when `feed` is unknown.
    pub homepage: CrawlTarget,
    /// The feed's address, when a crawl found it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed: Option<String>,
    /// What the feed's last answer said about its version (`ETag` and
    /// `Last-Modified`), sent back so an unchanged feed is not sent again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

/// What a check of one site's feed found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedCheck {
    pub domain: String,
    pub outcome: FeedOutcome,
}

/// See [`check_feeds`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedOutcome {
    /// The feed's recent posts on the site, newest first (maybe none), with
    /// its address and version.
    Read {
        feed: String,
        headlines: Vec<plumb_core::Headline>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    /// The feed has not changed since the version the target named.
    NotModified,
    /// The site has no feed this crawler may read: its homepage names
    /// none, robots.txt keeps the crawler out, or the feed is gone (404 or
    /// 410) or is not RSS or Atom.
    NoFeed,
    /// No answer, or an error worth trying again later.
    Failed(String),
}

/// Feeds are read up to this size; most are under 200 KB.
const FEED_MAX_BYTES: usize = 2 * 1024 * 1024;
/// `Accept` header for feed requests.
const ACCEPT_FEED: &str =
    "application/rss+xml,application/atom+xml,application/xml;q=0.9,text/xml;q=0.9,*/*;q=0.5";

/// Checks each target's feed, at most `cfg.concurrency` at a time, for its
/// posts of the last week ([`crate::read_feed`]). A target with no known
/// feed has its homepage fetched first, as [`crawl_homepages`] would (no
/// icon, no fallback URLs), for the feed it names ([`crate::PageMeta::feed`]). The feed is
/// fetched with the same rules as a homepage: robots.txt of each origin
/// asked, per-host delays, at most `cfg.max_redirects` redirects (to any
/// site, each allowed by its robots.txt), no private addresses. A known
/// feed is asked for only if changed since the version the target names.
/// A check not done within `cfg.target_deadline` fails. Must run inside a
/// Tokio runtime.
pub async fn check_feeds(targets: Vec<FeedTarget>, cfg: &CrawlConfig) -> Vec<FeedCheck> {
    let cfg = CrawlConfig {
        fetch_icons: false,
        ..cfg.clone()
    };
    let client = match build_client(&cfg) {
        Ok(client) => client,
        Err(err) => {
            let error = format!("building the HTTP client: {}", error_text(err));
            return targets
                .into_iter()
                .map(|target| FeedCheck {
                    domain: target.homepage.domain,
                    outcome: FeedOutcome::Failed(error.clone()),
                })
                .collect();
        }
    };
    check_feeds_with(&client, targets, &cfg).await
}

/// [`check_feeds`] with the client built.
async fn check_feeds_with(
    client: &Client,
    targets: Vec<FeedTarget>,
    cfg: &CrawlConfig,
) -> Vec<FeedCheck> {
    stream::iter(targets)
        .map(|target| async move {
            let outcome =
                match tokio::time::timeout(cfg.target_deadline, check_feed(client, cfg, &target))
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => FeedOutcome::Failed(past_deadline(cfg)),
                };
            debug!("{} feed: {outcome:?}", target.homepage.domain);
            FeedCheck {
                domain: target.homepage.domain,
                outcome,
            }
        })
        .buffer_unordered(cfg.concurrency.max(1))
        .collect()
        .await
}

async fn check_feed(client: &Client, cfg: &CrawlConfig, target: &FeedTarget) -> FeedOutcome {
    let domain = &target.homepage.domain;
    let mut visit = Visit::new(client, cfg);
    let (feed, known) = match target.feed.as_deref().map(http_url) {
        Some(Ok(feed)) => (feed, true),
        _ => {
            let start = match start_url(&target.homepage) {
                Ok(start) => start,
                Err(error) => return FeedOutcome::Failed(error),
            };
            match visit.fetch_homepage(domain, &start).await {
                CrawlOutcome::Fetched(page) => match page.meta.feed.as_deref().map(http_url) {
                    Some(Ok(feed)) => (feed, false),
                    _ => return FeedOutcome::NoFeed,
                },
                CrawlOutcome::Failed { error, .. } => return FeedOutcome::Failed(error),
                _ => return FeedOutcome::NoFeed,
            }
        }
    };
    let mut url = feed.clone();
    for _ in 0..=cfg.max_redirects {
        let crawl_delay = match visit.robots(&url).await {
            Robots::DoNotCrawl(failure) => return FeedOutcome::Failed(failure.error),
            Robots::NoRules => None,
            Robots::Rules(robot) => {
                if !allowed(&robot, &url).await {
                    return FeedOutcome::NoFeed;
                }
                robot.delay
            }
        };
        let delay = page_delay(cfg.per_host_delay, crawl_delay);
        let mut request = client.get(url.clone()).header(ACCEPT, ACCEPT_FEED);
        if known {
            if let Some(etag) = &target.etag {
                request = request.header(IF_NONE_MATCH, etag);
            }
            if let Some(modified) = &target.last_modified {
                request = request.header(IF_MODIFIED_SINCE, modified);
            }
        }
        let response = match visit.send(&url, delay, request).await {
            Ok(response) => response,
            Err(err) => return FeedOutcome::Failed(error_text(err)),
        };
        if let Some(next) = redirect_target(&response) {
            url = next;
            continue;
        }
        let status = response.status();
        if status == StatusCode::NOT_MODIFIED {
            return FeedOutcome::NotModified;
        }
        if matches!(status.as_u16(), 404 | 410) {
            return FeedOutcome::NoFeed;
        }
        if !status.is_success() {
            return FeedOutcome::Failed(format!("{url}: HTTP {status}"));
        }
        let header = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v: &reqwest::header::HeaderValue| v.to_str().ok())
                .filter(|v| !v.is_empty() && v.len() <= 200)
                .map(str::to_string)
        };
        let (etag, last_modified) = (header(ETAG), header(LAST_MODIFIED));
        let body = match read_body(response, FEED_MAX_BYTES, &cfg.downloaded).await {
            Ok(body) => body,
            Err(err) => return FeedOutcome::Failed(error_text(err)),
        };
        let (domain, base) = (domain.clone(), url.clone());
        let read = tokio::task::spawn_blocking(move || {
            crate::read_feed(&domain, &base, &decode_html(&body), now_unix())
        })
        .await;
        return match read {
            Ok(Some(headlines)) => FeedOutcome::Read {
                feed: feed.into(),
                headlines,
                etag,
                last_modified,
            },
            Ok(None) => FeedOutcome::NoFeed,
            Err(err) => FeedOutcome::Failed(format!("reading the feed: {err}")),
        };
    }
    FeedOutcome::Failed(format!("{feed}: more than {} redirects", cfg.max_redirects))
}

/// One client for the whole batch, so connections are reused.
pub(crate) fn build_client(cfg: &CrawlConfig) -> reqwest::Result<Client> {
    client_builder(cfg).build()
}

/// The client's settings. It follows no redirects: [`Visit`] follows them
/// itself, checking robots.txt before each hop.
fn client_builder(cfg: &CrawlConfig) -> ClientBuilder {
    let builder = Client::builder()
        .user_agent(cfg.user_agent.as_str())
        .timeout(cfg.timeout)
        .gzip(true)
        .redirect(redirect::Policy::none())
        .dns_resolver(dns::Resolver::new(
            cfg.allow_private_addresses,
            cfg.dns_lookups,
        ));
    // reqwest uses the system proxy unless told not to; behind a proxy the
    // resolver above would see only the proxy's name, not the targets'.
    if cfg.use_system_proxy {
        builder
    } else {
        builder.no_proxy()
    }
}

/// Not finished within [`CrawlConfig::target_deadline`].
fn past_deadline(cfg: &CrawlConfig) -> String {
    format!(
        "not done within {} seconds",
        cfg.target_deadline.as_secs_f32()
    )
}

async fn crawl_target(client: &Client, cfg: &CrawlConfig, target: CrawlTarget) -> CrawlResult {
    let outcome = match tokio::time::timeout(
        cfg.target_deadline,
        crawl_outcome(client, cfg, &target),
    )
    .await
    {
        Ok(outcome) => outcome,
        // A slow site, not a sign the node is offline.
        Err(_) => Failure::other(past_deadline(cfg)).into(),
    };
    debug!("{} ({}): {}", target.domain, target.url, describe(&outcome));
    CrawlResult {
        domain: target.domain,
        outcome,
    }
}

async fn crawl_outcome(client: &Client, cfg: &CrawlConfig, target: &CrawlTarget) -> CrawlOutcome {
    let start = match start_url(target) {
        Ok(url) => url,
        Err(error) => return Failure::other(error).into(),
    };
    let mut visit = Visit::new(client, cfg);
    let mut first_failure = None;
    let mut also_tried = Vec::new();
    for url in candidate_urls(&start, target) {
        match visit.fetch_homepage(&target.domain, &url).await {
            CrawlOutcome::Failed {
                error,
                network: true,
            } => {
                debug!("{}: no answer at {url}: {error}", target.domain);
                if first_failure.is_none() {
                    first_failure = Some(error);
                } else {
                    also_tried.push(url.to_string());
                }
            }
            outcome => return outcome,
        }
    }
    let mut error = first_failure.unwrap_or_default();
    if !also_tried.is_empty() {
        error = format!("{error} (no answer at {} either)", also_tried.join(", "));
    }
    CrawlOutcome::Failed {
        error,
        network: true,
    }
}

/// The target's start URL: `target.url`, or `https://<domain>/` when that
/// is empty.
fn start_url(target: &CrawlTarget) -> Result<Url, String> {
    match target.url.trim() {
        "" => http_url(&format!("https://{}/", target.domain.trim())),
        url => http_url(url),
    }
}

/// `raw` as an absolute http(s) URL with a host.
pub(crate) fn http_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|err| format!("invalid URL {raw:?}: {err}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("not an http(s) URL: {raw:?}"));
    }
    Ok(url)
}

/// The URLs to try for a target, in order and without repeats: the start
/// URL; then, for when that gets no answer, the known URL if it is on the
/// start URL's site and, for a start URL on the domain itself, the same URL
/// on the `www.` host and over plain http.
fn candidate_urls(start: &Url, target: &CrawlTarget) -> Vec<Url> {
    let mut urls = vec![start.clone()];
    if let Some(known) = target.known_url.as_deref() {
        match http_url(known) {
            // Another site, or an IP address, which the private-address
            // check would not see: not this target's homepage.
            Ok(url) if !same_site(start, &url) => {
                debug!(
                    "{}: ignoring the known URL {url}: another site",
                    target.domain
                );
            }
            Ok(url) => push_new(&mut urls, url),
            Err(error) => debug!("{}: ignoring the known URL: {error}", target.domain),
        }
    }
    // The domain spelled as a URL host: lowercase, IDNA as ASCII.
    let Ok(home) = http_url(&format!("https://{}/", target.domain.trim())) else {
        return urls;
    };
    let Some(host) = home
        .host_str()
        .filter(|&host| start.host_str() == Some(host))
    else {
        return urls;
    };
    if !host.starts_with("www.") {
        let mut www = start.clone();
        if www.set_host(Some(&format!("www.{host}"))).is_ok() {
            push_new(&mut urls, www);
        }
    }
    if start.scheme() == "https" {
        let mut http = start.clone();
        if http.set_scheme("http").is_ok() {
            push_new(&mut urls, http);
        }
    }
    urls
}

fn push_new(urls: &mut Vec<Url>, url: Url) {
    if !urls.contains(&url) {
        urls.push(url);
    }
}

/// A failure, to report as [`CrawlOutcome::Failed`].
#[derive(Debug, Clone)]
pub(crate) struct Failure {
    error: String,
    network: bool,
}

impl Failure {
    /// A request that failed; `what` (like `robots.txt`) starts the message.
    fn request(what: Option<&str>, err: reqwest::Error) -> Self {
        let network = is_network_error(&err);
        let error = match what {
            Some(what) => format!("{what}: {}", error_text(err)),
            None => error_text(err),
        };
        Failure { error, network }
    }

    /// A failure that is not about the network.
    fn other(error: String) -> Self {
        Failure {
            error,
            network: false,
        }
    }
}

impl From<Failure> for CrawlOutcome {
    fn from(failure: Failure) -> Self {
        CrawlOutcome::Failed {
            error: failure.error,
            network: failure.network,
        }
    }
}

/// Whether `err` means that no usable answer came back: the host name did
/// not resolve (or was refused by the resolver), connecting or the TLS
/// handshake failed, the request timed out, the connection broke off
/// before the response was complete, or the answer was not HTTP. Errors
/// building the request or decoding the body are not network errors.
fn is_network_error(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout() || err.is_request() || err.is_body()
}

/// What robots.txt says about crawling an origin.
#[derive(Clone)]
pub(crate) enum Robots {
    /// A robots.txt was fetched and parsed; obey it.
    Rules(Arc<Robot>),
    /// There is no usable robots.txt (4xx, or a redirect chain that does
    /// not end), so there are no restrictions.
    NoRules,
    /// Do not crawl: robots.txt answered 5xx or 429, could not be fetched,
    /// or could not be parsed. The message says which.
    DoNotCrawl(Failure),
}

/// One target's crawl: the robots.txt of each origin it visits, and when
/// each host it asked last answered.
pub(crate) struct Visit<'a> {
    client: &'a Client,
    cfg: &'a CrawlConfig,
    robots: HashMap<Origin, Robots>,
    last_answer: HashMap<String, Instant>,
    extraction: crate::InnerPageExtraction,
}

impl<'a> Visit<'a> {
    /// The client requests are sent with.
    pub(crate) fn client(&self) -> &'a Client {
        self.client
    }

    pub(crate) fn new(client: &'a Client, cfg: &'a CrawlConfig) -> Self {
        Visit {
            client,
            cfg,
            robots: HashMap::new(),
            last_answer: HashMap::new(),
            extraction: crate::InnerPageExtraction::Compact,
        }
    }

    pub(crate) fn set_inner_page_extraction(&mut self, extraction: crate::InnerPageExtraction) {
        self.extraction = extraction;
    }

    /// Fetches the page at `start`, following redirects that stay on its
    /// site and checking every URL against the robots.txt of its origin
    /// before requesting it.
    pub(crate) async fn fetch_homepage(&mut self, domain: &str, start: &Url) -> CrawlOutcome {
        let mut url = start.clone();
        let mut redirects = 0;
        loop {
            let crawl_delay = match self.robots(&url).await {
                Robots::DoNotCrawl(failure) => return failure.into(),
                Robots::NoRules => None,
                Robots::Rules(robot) => {
                    if !allowed(&robot, &url).await {
                        return CrawlOutcome::RobotsDisallowed;
                    }
                    robot.delay
                }
            };
            let delay = page_delay(self.cfg.per_host_delay, crawl_delay);
            let request = self.client.get(url.clone()).header(ACCEPT, ACCEPT_HTML);
            let response = match self.send(&url, delay, request).await {
                Ok(response) => response,
                Err(err) => return Failure::request(None, err).into(),
            };
            let Some(next) = redirect_target(&response) else {
                let mut outcome = self.read_page(domain, response).await;
                if let CrawlOutcome::Fetched(page) = &mut outcome {
                    if self.cfg.fetch_icons {
                        page.icon = self.fetch_icon(page).await;
                    }
                }
                return outcome;
            };
            if !same_site(start, &next) {
                return CrawlOutcome::OffsiteRedirect {
                    final_url: next.into(),
                };
            }
            if redirects == self.cfg.max_redirects {
                let error = format!(
                    "more than {} redirects, starting at {start}",
                    self.cfg.max_redirects
                );
                return Failure::other(error).into();
            }
            redirects += 1;
            debug!("{url} redirects to {next}");
            url = next;
        }
    }

    /// The site's icon for `page`, normalized: the best two icons the page
    /// links to, then `/favicon.ico` of the page's origin,
    /// until one reads as a bitmap. Each URL, and each redirect, must be
    /// allowed by its origin's robots.txt; icons may be on other sites,
    /// such as a CDN. A failure only means no icon.
    async fn fetch_icon(&mut self, page: &CrawledPage) -> Option<Vec<u8>> {
        let page_url = Url::parse(&page.final_url).ok()?;
        self.fetch_icon_of(&page_url, &page.meta.icons).await
    }

    /// [`Visit::fetch_icon`] for the page at `page_url` that links to
    /// `named` icons.
    async fn fetch_icon_of(&mut self, page_url: &Url, named: &[String]) -> Option<Vec<u8>> {
        let mut urls: Vec<Url> = named
            .iter()
            .filter_map(|icon| http_url(icon).ok())
            .take(MAX_ICON_TRIES - 1)
            .collect();
        if let Ok(favicon) = page_url.join(FAVICON_PATH) {
            urls.push(favicon);
        }
        let mut seen = Vec::new();
        urls.retain(|url| {
            let new = !seen.contains(url);
            seen.push(url.clone());
            new
        });
        for start in urls {
            let Some(body) = self.fetch_icon_file(start.clone()).await else {
                continue;
            };
            let icon = tokio::task::spawn_blocking(move || normalize_icon(&body))
                .await
                .ok()
                .flatten();
            if icon.is_some() {
                return icon;
            }
            debug!("{start}: not an icon this crawler can read");
        }
        None
    }

    /// The bytes of an icon file at `url`, following redirects anywhere
    /// that robots.txt allows. `None` for anything but a 2xx answer that
    /// is not a web page.
    async fn fetch_icon_file(&mut self, mut url: Url) -> Option<Vec<u8>> {
        for _ in 0..=self.cfg.max_redirects {
            let crawl_delay = match self.robots(&url).await {
                Robots::DoNotCrawl(_) => return None,
                Robots::NoRules => None,
                Robots::Rules(robot) => {
                    if !allowed(&robot, &url).await {
                        return None;
                    }
                    robot.delay
                }
            };
            let delay = page_delay(self.cfg.per_host_delay, crawl_delay);
            let request = self.client.get(url.clone()).header(ACCEPT, ACCEPT_ICON);
            let response = self.send(&url, delay, request).await.ok()?;
            if let Some(next) = redirect_target(&response) {
                url = next;
                continue;
            }
            if !response.status().is_success() {
                return None;
            }
            if content_type(&response).is_some_and(|kind| is_html(&kind)) {
                return None;
            }
            return read_body(response, ICON_MAX_BYTES, &self.cfg.downloaded)
                .await
                .ok();
        }
        None
    }

    /// The robots.txt rules for `url`'s origin, fetched on first use.
    /// Every URL is asked about here before it is requested, so this is
    /// also where hosts written as private IP addresses are refused: the
    /// resolver only checks names, and icons, feeds and redirects can
    /// point anywhere.
    pub(crate) async fn robots(&mut self, url: &Url) -> Robots {
        if !self.cfg.allow_private_addresses && crate::read::names_private_ip(url) {
            return Robots::DoNotCrawl(Failure::other(format!("{url}: a private address")));
        }
        let origin = url.origin();
        if let Some(robots) = self.robots.get(&origin) {
            return robots.clone();
        }
        let robots = self.fetch_robots(url).await;
        self.robots.insert(origin, robots.clone());
        robots
    }

    /// Fetches and parses the robots.txt of `url`'s origin, following
    /// redirects to any host, as RFC 9309 asks; the file the chain ends at
    /// holds the origin's rules. Hops to private IP addresses are refused
    /// here (the resolver refuses names that resolve to them).
    async fn fetch_robots(&mut self, url: &Url) -> Robots {
        let first = robots_url(url);
        let mut robots = first.clone();
        for _ in 0..=self.cfg.max_redirects {
            if !self.cfg.allow_private_addresses && crate::read::names_private_ip(&robots) {
                return Robots::DoNotCrawl(Failure::other(format!(
                    "robots.txt: redirects to {robots}, a private address"
                )));
            }
            let request = self.client.get(robots.clone());
            let response = match self.send(&robots, self.cfg.per_host_delay, request).await {
                Ok(response) => response,
                Err(err) => return Robots::DoNotCrawl(Failure::request(Some("robots.txt"), err)),
            };
            let status = response.status();
            if status.is_redirection() {
                match redirect_target(&response) {
                    Some(next) => {
                        robots = next;
                        continue;
                    }
                    None => {
                        debug!("{robots}: a redirect without a usable Location; no rules apply");
                        return Robots::NoRules;
                    }
                }
            }
            if status.is_success() {
                return match read_body(response, ROBOTS_MAX_BYTES, &self.cfg.downloaded).await {
                    Ok(body) => parse_robots(body).await,
                    Err(err) => Robots::DoNotCrawl(Failure::request(Some("robots.txt"), err)),
                };
            }
            if status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS {
                return Robots::NoRules;
            }
            return Robots::DoNotCrawl(Failure::other(format!("robots.txt: HTTP {status}")));
        }
        debug!(
            "{first}: more than {} redirects; no rules apply",
            self.cfg.max_redirects
        );
        Robots::NoRules
    }

    /// Sends `request` to `url` once `delay` has passed since `url`'s host
    /// last answered (or failed to), and notes when this request is done.
    /// Counting from the answer rather than the request keeps slow hosts
    /// from being asked again right away.
    pub(crate) async fn send(
        &mut self,
        url: &Url,
        delay: Duration,
        request: RequestBuilder,
    ) -> reqwest::Result<Response> {
        let host = url.host_str().unwrap_or_default().to_string();
        if let Some(last) = self.last_answer.get(&host) {
            let wait = delay.saturating_sub(last.elapsed());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
        let response = request.send().await;
        self.last_answer.insert(host, Instant::now());
        response
    }

    /// The outcome for the response that ends the redirect chain.
    async fn read_page(&self, domain: &str, response: Response) -> CrawlOutcome {
        let final_url = response.url().clone();
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
        let body = match read_body(response, self.cfg.max_bytes, &self.cfg.downloaded).await {
            Ok(body) => body,
            Err(err) => return Failure::request(None, err).into(),
        };
        let fetched_at = now_unix();
        // Parsing is CPU work; keep it off the threads driving the other fetches.
        let base_url = final_url.clone();
        let extraction = self.extraction;
        let parsed = tokio::task::spawn_blocking(move || {
            crate::extract_inner_page_meta(&base_url, &decode_html(&body), extraction)
        })
        .await;
        match parsed {
            Ok(meta)
                if is_bot_check_page(
                    domain,
                    meta.title.as_deref(),
                    meta.description.as_deref(),
                    &meta.headings,
                    meta.body_text.as_deref(),
                ) =>
            {
                debug!("{final_url}: a bot check, not the homepage");
                CrawlOutcome::BotCheck { title: meta.title }
            }
            Ok(meta) => CrawlOutcome::Fetched(CrawledPage {
                domain: domain.to_string(),
                final_url: final_url.into(),
                status: status.as_u16(),
                fetched_at,
                meta,
                icon: None,
            }),
            Err(err) => Failure::other(format!("parsing the page: {err}")).into(),
        }
    }
}

/// Parses a robots.txt body, away from the threads driving the fetches.
async fn parse_robots(body: Vec<u8>) -> Robots {
    match tokio::task::spawn_blocking(move || robots_rules(&body)).await {
        Ok(Ok(robot)) => Robots::Rules(Arc::new(robot)),
        Ok(Err(error)) => Robots::DoNotCrawl(Failure::other(error)),
        Err(err) => {
            Robots::DoNotCrawl(Failure::other(format!("robots.txt: parsing failed: {err}")))
        }
    }
}

/// The rules of a robots.txt for [`ROBOTS_TOKEN`], unless they would take
/// more than [`MAX_ROBOTS_REGEX_BYTES`] or cannot be parsed.
fn robots_rules(body: &[u8]) -> Result<Robot, String> {
    let cost = regex_cost(body);
    if cost > MAX_ROBOTS_REGEX_BYTES {
        return Err(format!(
            "robots.txt: its rules with `$` would take about {} MiB, more than the {} MiB \
             this crawler allows",
            cost.div_ceil(1 << 20),
            MAX_ROBOTS_REGEX_BYTES >> 20
        ));
    }
    Robot::new(ROBOTS_TOKEN, &product_tokens(body))
        .map_err(|err| format!("robots.txt: cannot parse it: {err:#}"))
}

/// `body` with each `User-agent` value cut to its product token, the leading
/// run of letters, `_` and `-` (RFC 9309 2.2.1), as Google's parser matches
/// it. texting_robots compares the whole value, so `User-agent:
/// PlumbSearch/1.0`, copied from our User-Agent header, would not count as
/// meaning us and its rules would be skipped for the `*` group's.
fn product_tokens(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body);
    while !rest.is_empty() {
        let end = rest
            .iter()
            .position(|&byte| matches!(byte, b'\n' | b'\r'))
            .unwrap_or(rest.len());
        let (line, after) = rest.split_at(end);
        match user_agent_value(line) {
            Some(value) => {
                let token_len = value
                    .iter()
                    .take_while(|&&byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'-'))
                    .count();
                if token_len == 0 {
                    out.extend_from_slice(line);
                } else {
                    out.extend_from_slice(b"User-agent: ");
                    out.extend_from_slice(&value[..token_len]);
                }
            }
            None => out.extend_from_slice(line),
        }
        let newlines = after
            .iter()
            .take_while(|&&byte| matches!(byte, b'\n' | b'\r'))
            .count();
        out.extend_from_slice(&after[..newlines]);
        rest = &after[newlines..];
    }
    out
}

/// The value of a `User-agent` line, spelled as texting_robots accepts it,
/// without any comment or surrounding space.
fn user_agent_value(line: &[u8]) -> Option<&[u8]> {
    let line = line.trim_ascii_start();
    let key_len = ["user-agent", "user agent", "useragent"]
        .into_iter()
        .find(|key| {
            line.get(..key.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(key.as_bytes()))
        })?
        .len();
    let after_key = &line[key_len..];
    let value = match after_key.trim_ascii_start().strip_prefix(b":") {
        Some(value) => value,
        None if after_key.first().is_some_and(|&b| b == b' ' || b == b'\t') => after_key,
        None => return None,
    };
    let value = value.split(|&byte| byte == b'#').next().unwrap_or_default();
    Some(value.trim_ascii())
}

/// An estimate, on the high side, of the memory texting_robots needs for
/// the rules in `body` that hold a `$`, each of which it compiles into a
/// regular expression: 6 KiB per rule, 2 KiB per run of `*` and 64 bytes
/// per byte of the pattern, three for a non-ASCII byte since it gets
/// percent-encoded. (Measured with texting_robots 0.2 and regex 1: about
/// 5.8 KB, 1.8 KB and 51 bytes.) Other rules cost little more than their
/// text. Every line with a `$` before any `#` counts, whatever its group.
fn regex_cost(body: &[u8]) -> usize {
    // Split as texting_robots does: at line ends, then at the first `#`.
    body.split(|&byte| matches!(byte, b'\n' | b'\r'))
        .map(|line| line.split(|&byte| byte == b'#').next().unwrap_or_default())
        .filter(|rule| rule.contains(&b'$'))
        .map(|rule| {
            let stars = rule.iter().filter(|&&byte| byte == b'*').count();
            let runs = stars - rule.windows(2).filter(|pair| pair == b"**").count();
            let bytes: usize = rule
                .iter()
                .map(|byte| if byte.is_ascii() { 1 } else { 3 })
                .sum();
            6 * 1024 + runs * 2 * 1024 + bytes * 64
        })
        .sum()
}

/// Whether `robot` allows `url`. A big robots.txt has many rules to try, so
/// the matching runs away from the threads driving the fetches; if it
/// fails, the URL counts as disallowed.
pub(crate) async fn allowed(robot: &Arc<Robot>, url: &Url) -> bool {
    let (robot, url) = (Arc::clone(robot), url.to_string());
    tokio::task::spawn_blocking(move || robot.allowed(&url))
        .await
        .unwrap_or(false)
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

/// How long to wait before a page request after the last request to the
/// same host: `per_host_delay`, or the robots.txt `Crawl-delay` (seconds,
/// capped at [`MAX_CRAWL_DELAY`]) when that is longer.
pub(crate) fn page_delay(per_host_delay: Duration, crawl_delay: Option<f32>) -> Duration {
    let robots_delay = match crawl_delay {
        Some(secs) if secs > 0.0 => {
            Duration::from_secs_f32(secs.min(MAX_CRAWL_DELAY.as_secs_f32()))
        }
        // Absent, zero, negative or NaN.
        _ => Duration::ZERO,
    };
    per_host_delay.max(robots_delay)
}

/// Where a redirect points: its `Location`, resolved against the response's
/// URL, without credentials or fragment. `None` unless the status is 301,
/// 302, 303, 307 or 308 and the `Location` makes an http(s) URL.
pub(crate) fn redirect_target(response: &Response) -> Option<Url> {
    if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let location = response.headers().get(LOCATION)?;
    let location = String::from_utf8_lossy(location.as_bytes());
    let mut url = response.url().join(&location).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_fragment(None);
    // Only fails for URLs that cannot have credentials, which have none to strip.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Some(url)
}

/// Same host, or two hosts with the same registrable domain
/// (`usbank.com` and `www.usbank.com`). Ports and schemes do not matter.
pub(crate) fn same_site(a: &Url, b: &Url) -> bool {
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
/// limit applies to the decompressed bytes. Every byte read is added to
/// `downloaded`.
pub(crate) async fn read_body(
    mut response: Response,
    max_bytes: usize,
    downloaded: &AtomicU64,
) -> reqwest::Result<Vec<u8>> {
    let mut body = Vec::new();
    while body.len() < max_bytes {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        downloaded.fetch_add(chunk.len() as u64, Ordering::Relaxed);
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
pub(crate) fn error_text(err: reqwest::Error) -> String {
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
        CrawlOutcome::BotCheck { title } => {
            format!("a bot check ({})", title.as_deref().unwrap_or("no title"))
        }
        CrawlOutcome::Failed {
            error,
            network: true,
        } => format!("no answer: {error}"),
        CrawlOutcome::Failed { error, .. } => format!("failed: {error}"),
    }
}

/// Logs how many of `results` ended which way, as one "crawled N homepages:" line.
pub fn log_summary(results: &[CrawlResult]) {
    let (mut fetched, mut disallowed, mut offsite, mut http, mut not_html) = (0, 0, 0, 0, 0);
    let (mut bot_checks, mut failed, mut no_answer) = (0, 0, 0);
    for result in results {
        match result.outcome {
            CrawlOutcome::Fetched(_) => fetched += 1,
            CrawlOutcome::RobotsDisallowed => disallowed += 1,
            CrawlOutcome::OffsiteRedirect { .. } => offsite += 1,
            CrawlOutcome::HttpStatus { .. } => http += 1,
            CrawlOutcome::NotHtml { .. } => not_html += 1,
            CrawlOutcome::BotCheck { .. } => bot_checks += 1,
            CrawlOutcome::Failed { network, .. } => {
                failed += 1;
                no_answer += usize::from(network);
            }
        }
    }
    info!(
        "crawled {} homepages: {fetched} fetched, {disallowed} disallowed by robots.txt, \
         {offsite} redirected to other sites, {http} HTTP errors, {not_html} not HTML, \
         {bot_checks} bot checks, {failed} failed ({no_answer} with no answer)",
        results.len()
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
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
    use crate::test_alloc::peak_bytes;
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
            // Only the icon tests turn this on, so the others see just the
            // requests for robots.txt and pages.
            fetch_icons: false,
            // The test server is on 127.0.0.1 (or `localhost`).
            allow_private_addresses: true,
            ..CrawlConfig::default()
        }
    }

    fn target(port: u16, path: &str) -> CrawlTarget {
        CrawlTarget {
            domain: "example.test".into(),
            url: format!("http://127.0.0.1:{port}{path}"),
            known_url: None,
        }
    }

    async fn crawl_one(target: CrawlTarget, cfg: &CrawlConfig) -> CrawlOutcome {
        let mut results = crawl_homepages(vec![target.clone()], cfg).await;
        assert_eq!(results.len(), 1);
        let result = results.pop().unwrap();
        assert_eq!(result.domain, target.domain);
        result.outcome
    }

    /// Crawls `target` with a client that sends each `(host, port)` host
    /// name to 127.0.0.1 at that port (when the URL names no port), so tests
    /// can use real-looking host names without DNS.
    async fn crawl_with_hosts(
        target: CrawlTarget,
        cfg: &CrawlConfig,
        hosts: &[(&str, u16)],
    ) -> CrawlOutcome {
        let mut client = client_builder(cfg);
        for &(host, port) in hosts {
            client = client.resolve(host, ([127, 0, 0, 1], port).into());
        }
        let mut results = crawl_with(&client.build().unwrap(), vec![target], cfg).await;
        assert_eq!(results.len(), 1);
        results.pop().unwrap().outcome
    }

    /// A server that accepts connections and closes them without a word,
    /// counting them: a host that does not answer.
    async fn silent_server() -> (u16, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        (port, connections)
    }

    fn expect_fetched(outcome: CrawlOutcome) -> CrawledPage {
        match outcome {
            CrawlOutcome::Fetched(page) => page,
            other => panic!("expected a fetched page, got {other:?}"),
        }
    }

    fn expect_failed(outcome: CrawlOutcome) -> String {
        expect_failure(outcome).0
    }

    /// The error message and whether the failure is a network one.
    fn expect_failure(outcome: CrawlOutcome) -> (String, bool) {
        match outcome {
            CrawlOutcome::Failed { error, network } => (error, network),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn checks_a_feed_the_homepage_names() {
        let (port, hits) = serve(|_| {
            Router::new()
                .route(
                    "/",
                    get(|| async {
                        Html(r#"<link rel="alternate" type="application/rss+xml" href="/rss">"#)
                    }),
                )
                .route(
                    "/rss",
                    get(|request: Request| async move {
                        if request.headers().get(header::IF_NONE_MATCH).is_some() {
                            return Response::builder()
                                .status(StatusCode::NOT_MODIFIED)
                                .body(axum::body::Body::empty())
                                .unwrap();
                        }
                        let now = now_unix();
                        let body = format!(
                            "<rss><channel><item><title>Fresh</title>\
                             <link>http://news.example.com/fresh</link>\
                             <pubDate>{}</pubDate></item></channel></rss>",
                            rfc2822(now - 60)
                        );
                        Response::builder()
                            .header(header::ETAG, "\"v1\"")
                            .body(axum::body::Body::from(body))
                            .unwrap()
                    }),
                )
                .route(
                    "/robots.txt",
                    get(|| async { "User-agent: *\nDisallow: /private\n" }),
                )
        })
        .await;
        let cfg = config();
        let client = client_builder(&cfg)
            .resolve("news.example.com", ([127, 0, 0, 1], port).into())
            .build()
            .unwrap();
        let mut target = FeedTarget {
            homepage: CrawlTarget {
                domain: "example.com".into(),
                url: format!("http://news.example.com:{port}/"),
                known_url: None,
            },
            ..FeedTarget::default()
        };
        let checks = check_feeds_with(&client, vec![target.clone()], &cfg).await;
        let FeedOutcome::Read {
            feed,
            headlines,
            etag,
            ..
        } = checks[0].outcome.clone()
        else {
            panic!("expected the feed read, got {:?}", checks[0].outcome);
        };
        assert_eq!(feed, format!("http://news.example.com:{port}/rss"));
        assert_eq!(headlines.len(), 1);
        assert_eq!(headlines[0].title, "Fresh");
        assert_eq!(etag.as_deref(), Some("\"v1\""));
        assert_eq!(hits.paths(), ["/robots.txt", "/", "/rss"]);

        // Known now: straight to the feed, which has not changed.
        target.feed = Some(feed);
        target.etag = etag;
        let checks = check_feeds_with(&client, vec![target.clone()], &cfg).await;
        assert_eq!(checks[0].outcome, FeedOutcome::NotModified);

        // A feed robots.txt keeps the crawler out of is no feed for it.
        target.feed = Some(format!("http://news.example.com:{port}/private/rss"));
        let checks = check_feeds_with(&client, vec![target], &cfg).await;
        assert_eq!(checks[0].outcome, FeedOutcome::NoFeed);
    }

    /// `at` as an RSS date, in UTC.
    fn rfc2822(at: u64) -> String {
        const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let days = (at / 86_400) as i64;
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        let secs = at % 86_400;
        format!(
            "{}, {day:02} {} {year} {:02}:{:02}:{:02} GMT",
            DAYS[(days % 7) as usize],
            MONTHS[(month - 1) as usize],
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        )
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

    fn icon_config() -> CrawlConfig {
        CrawlConfig {
            fetch_icons: true,
            ..config()
        }
    }

    fn png_response() -> Response {
        Response::builder()
            .header(header::CONTENT_TYPE, "image/png")
            .body(crate::icon::tests::png(48, 48, 255).into())
            .unwrap()
    }

    const ICON_HOME: &str = r#"<!doctype html><html><head><title>Icons</title>
        <link rel="icon" href="/static/icon.svg" type="image/svg+xml">
        <link rel="icon" href="/static/icon-32.png" sizes="32x32">
        </head><body>Hi</body></html>"#;

    #[tokio::test]
    async fn fetches_the_icon_a_homepage_names() {
        let (port, hits) = serve(|_| {
            Router::new()
                .route("/", get(|| async { Html(ICON_HOME) }))
                .route("/static/icon-32.png", get(|| async { png_response() }))
        })
        .await;
        let page = expect_fetched(crawl_one(target(port, "/"), &icon_config()).await);
        let icon = page.icon.expect("an icon");
        assert!(icon.starts_with(b"\x89PNG"));
        assert_eq!(
            page.meta.icons,
            [format!("http://127.0.0.1:{port}/static/icon-32.png")]
        );
        assert_eq!(hits.paths(), ["/robots.txt", "/", "/static/icon-32.png"]);
    }

    #[tokio::test]
    async fn private_ip_addresses_are_never_requested() {
        // A public page could name an icon, a feed or a redirect target on
        // the node's own network; the resolver only checks names.
        let (port, hits) = serve(|_| {
            Router::new()
                .route("/", get(|| async { Html(ICON_HOME) }))
                .route("/favicon.ico", get(|| async { png_response() }))
        })
        .await;
        let cfg = CrawlConfig {
            allow_private_addresses: false,
            ..icon_config()
        };
        let outcome = crawl_one(target(port, "/"), &cfg).await;
        assert!(
            matches!(&outcome, CrawlOutcome::Failed { error, .. } if error.contains("private address")),
            "{outcome:?}"
        );
        let found = fetch_site_icons(vec![target(port, "/")], &cfg).await;
        assert!(found.iter().all(|(_, icon)| icon.is_none()));
        assert!(hits.paths().is_empty());
    }

    #[tokio::test]
    async fn icons_obey_robots_txt_and_fall_back_to_favicon_ico() {
        let (port, hits) = serve(|_| {
            Router::new()
                .route("/", get(|| async { Html(ICON_HOME) }))
                .route(
                    "/robots.txt",
                    get(|| async { "User-agent: *\nDisallow: /static/\n" }),
                )
                .route("/static/icon-32.png", get(|| async { png_response() }))
                .route("/favicon.ico", get(|| async { png_response() }))
        })
        .await;
        let page = expect_fetched(crawl_one(target(port, "/"), &icon_config()).await);
        assert!(page.icon.is_some());
        assert_eq!(hits.paths(), ["/robots.txt", "/", "/favicon.ico"]);
    }

    #[tokio::test]
    async fn fetches_just_a_sites_icon() {
        let (port, hits) = serve(|_| {
            Router::new()
                .route("/", get(|| async { Html(ICON_HOME) }))
                .route("/favicon.ico", get(|| async { png_response() }))
        })
        .await;
        let found = fetch_site_icons(vec![target(port, "/")], &config()).await;
        assert_eq!(found.len(), 1);
        assert!(found[0]
            .1
            .as_ref()
            .expect("an icon")
            .starts_with(b"\x89PNG"));
        // The homepage itself is not fetched.
        assert_eq!(hits.paths(), ["/robots.txt", "/favicon.ico"]);
    }

    #[tokio::test]
    async fn a_site_without_a_readable_icon_has_none() {
        let (port, hits) = serve(|_| {
            home().route(
                "/favicon.ico",
                get(|| async { Html("<!doctype html><p>Not here") }),
            )
        })
        .await;
        let page = expect_fetched(crawl_one(target(port, "/"), &icon_config()).await);
        assert_eq!(page.icon, None);
        assert_eq!(hits.paths(), ["/robots.txt", "/", "/favicon.ico"]);
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
            let (error, network) = expect_failure(crawl_one(target(port, "/"), &config()).await);
            assert!(error.starts_with("robots.txt"), "{error}");
            assert!(error.contains(status.as_str()), "{error}");
            // The site answered: not a network failure.
            assert!(!network);
            assert_eq!(hits.paths(), ["/robots.txt"]);
        }
    }

    /// A robots.txt that starts with `rules` (after `User-agent: *`) and is
    /// padded with comment lines to at least `bytes`.
    fn robots_txt(rules: &str, bytes: usize) -> String {
        let mut txt = format!("User-agent: *\n{rules}");
        while txt.len() < bytes {
            txt.push_str("# padding padding padding padding padding\n");
        }
        txt
    }

    #[tokio::test]
    async fn robots_txt_is_read_up_to_500_kib() {
        // A group that starts within the first 500 KiB is obeyed...
        let padded = robots_txt("", ROBOTS_MAX_BYTES - 1024) + "User-agent: *\nDisallow: /\n";
        let (port, _) = serve(move |_| home().route("/robots.txt", get(|| async { padded }))).await;
        let outcome = crawl_one(target(port, "/"), &config()).await;
        assert_eq!(outcome, CrawlOutcome::RobotsDisallowed);

        // ...and what comes after them is not read.
        let padded = robots_txt("", ROBOTS_MAX_BYTES) + "User-agent: *\nDisallow: /\n";
        let (port, hits) =
            serve(move |_| home().route("/robots.txt", get(|| async { padded }))).await;
        expect_fetched(crawl_one(target(port, "/"), &config()).await);
        assert_eq!(hits.paths(), ["/robots.txt", "/"]);
    }

    #[tokio::test]
    async fn robots_txt_too_costly_to_parse_means_no_crawl() {
        let txt = robots_txt(&rules(2000, "*a*b*c$"), 0);
        let (port, hits) = serve(move |_| home().route("/robots.txt", get(|| async { txt }))).await;
        let (error, network) = expect_failure(crawl_one(target(port, "/"), &config()).await);
        assert_eq!(
            error,
            "robots.txt: its rules with `$` would take about 27 MiB, more than the 16 MiB \
             this crawler allows"
        );
        assert!(!network);
        assert_eq!(hits.paths(), ["/robots.txt"]);
    }

    #[tokio::test]
    async fn unreachable_host_is_not_crawled() {
        // Nothing can listen on port 0, so connecting fails right away.
        let (error, network) = expect_failure(crawl_one(target(0, "/"), &config()).await);
        assert!(error.starts_with("robots.txt"), "{error}");
        assert!(network, "{error}");

        // Nor can a host that hangs up without answering be.
        let (port, connections) = silent_server().await;
        let (error, network) = expect_failure(crawl_one(target(port, "/"), &config()).await);
        assert!(error.starts_with("robots.txt"), "{error}");
        assert!(network, "{error}");
        assert_eq!(connections.load(Ordering::SeqCst), 1);
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
            let (error, network) = expect_failure(result.outcome);
            assert!(error.starts_with("building the HTTP client"), "{error}");
            assert!(!network);
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
        // The name "localhost" is looked up, and it resolves to loopback.
        let (port, hits) = serve(|_| home()).await;
        let by_name = CrawlTarget {
            domain: "example.test".into(),
            url: format!("http://localhost:{port}/"),
            known_url: None,
        };

        assert!(!CrawlConfig::default().allow_private_addresses);
        let refusing = CrawlConfig {
            allow_private_addresses: false,
            ..config()
        };
        let (error, network) = expect_failure(crawl_one(by_name.clone(), &refusing).await);
        assert!(network, "{error}");
        assert!(error.starts_with("robots.txt"), "{error}");
        assert!(
            error.contains("localhost resolves only to non-public addresses"),
            "{error}"
        );
        assert!(hits.paths().is_empty(), "{:?}", hits.paths());

        let page = expect_fetched(crawl_one(by_name, &config()).await);
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
            allow_private_addresses: false,
            ..config()
        };
        let target = CrawlTarget {
            domain: "example.test".into(),
            url,
            known_url: None,
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
                known_url: None,
            };
            let (error, network) = expect_failure(crawl_one(bad, &config()).await);
            assert!(error.contains(url), "{error}");
            assert!(!network);
        }
    }

    #[tokio::test]
    async fn empty_url_means_the_domain_homepage() {
        assert_eq!(
            start_url(&CrawlTarget {
                domain: "example.com".into(),
                ..CrawlTarget::default()
            }),
            Url::parse("https://example.com/").map_err(|err| err.to_string())
        );
        assert_eq!(
            CrawlTarget::homepage("example.com"),
            CrawlTarget::new("example.com")
        );
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
    async fn slow_targets_fail_at_the_deadline() {
        let slow = || async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Html(HOME)
        };
        let (port, _) = serve(move |_| Router::new().route("/", get(slow))).await;
        let cfg = CrawlConfig {
            target_deadline: Duration::from_millis(300),
            ..config()
        };
        let started = Instant::now();
        let (error, network) = expect_failure(crawl_one(target(port, "/"), &cfg).await);
        assert!(error.starts_with("not done within 0.3 seconds"), "{error}");
        assert!(!network);
        let feed = FeedTarget {
            homepage: target(port, "/"),
            ..FeedTarget::default()
        };
        let checks = check_feeds(vec![feed], &cfg).await;
        assert_eq!(
            checks[0].outcome,
            FeedOutcome::Failed("not done within 0.3 seconds".into())
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn too_many_redirects_fail() {
        let (port, hits) = serve(|_| {
            Router::new().route(
                "/",
                get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/")]) }),
            )
        })
        .await;
        let cfg = CrawlConfig {
            max_redirects: 2,
            ..config()
        };
        let (error, network) = expect_failure(crawl_one(target(port, "/"), &cfg).await);
        assert!(error.starts_with("more than 2 redirects"), "{error}");
        assert!(!network);
        assert_eq!(hits.paths(), ["/robots.txt", "/", "/", "/"]);
    }

    #[tokio::test]
    async fn redirects_into_disallowed_paths_are_never_requested() {
        let (port, hits) = serve(|_| {
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
        assert_eq!(hits.paths(), ["/robots.txt", "/"]);
    }

    /// Answers `/` with a redirect to `http://www.site.test/`.
    fn moved_to_www(_port: u16) -> Router {
        Router::new().route(
            "/",
            get(|| async {
                let to = "http://www.site.test/";
                (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, to)])
            }),
        )
    }

    #[tokio::test]
    async fn each_origin_is_checked_against_its_own_robots_txt() {
        // site.test has no robots.txt and sends visitors to www.site.test,
        // whose own robots.txt disallows everything.
        let (bare_port, bare_hits) = serve(moved_to_www).await;
        let (www_port, www_hits) = serve(|_| {
            home().route(
                "/robots.txt",
                get(|| async { "User-agent: *\nDisallow: /\n" }),
            )
        })
        .await;
        let start = CrawlTarget {
            domain: "site.test".into(),
            url: "http://site.test/".into(),
            known_url: None,
        };
        let hosts = [("site.test", bare_port), ("www.site.test", www_port)];
        let outcome = crawl_with_hosts(start.clone(), &config(), &hosts).await;
        assert_eq!(outcome, CrawlOutcome::RobotsDisallowed);
        assert_eq!(bare_hits.paths(), ["/robots.txt", "/"]);
        assert_eq!(www_hits.paths(), ["/robots.txt"]);

        // When www allows it, the page is fetched after www's Crawl-delay.
        let (bare_port, _) = serve(moved_to_www).await;
        let (www_port, www_hits) = serve(|_| {
            home().route(
                "/robots.txt",
                get(|| async { "User-agent: *\nCrawl-delay: 0.3\n" }),
            )
        })
        .await;
        let hosts = [("site.test", bare_port), ("www.site.test", www_port)];
        let page = expect_fetched(crawl_with_hosts(start, &config(), &hosts).await);
        assert_eq!(page.final_url, "http://www.site.test/");
        assert_eq!(www_hits.paths(), ["/robots.txt", "/"]);
        let gap = www_hits
            .get("/")
            .at
            .duration_since(www_hits.get("/robots.txt").at);
        assert!(gap >= Duration::from_millis(300), "{gap:?}");
    }

    #[tokio::test]
    async fn start_urls_that_get_no_answer_fall_back() {
        let (silent, silent_connections) = silent_server().await;
        let connections = || silent_connections.load(Ordering::SeqCst);

        // A www-only site: the bare name does not answer.
        let (port, hits) = serve(|_| home()).await;
        let www_only = CrawlTarget {
            domain: "www-only.test".into(),
            url: "http://www-only.test/".into(),
            known_url: None,
        };
        let hosts = [("www-only.test", silent), ("www.www-only.test", port)];
        let page = expect_fetched(crawl_with_hosts(www_only, &config(), &hosts).await);
        assert_eq!(page.final_url, "http://www.www-only.test/");
        assert_eq!(hits.paths(), ["/robots.txt", "/"]);

        // No TLS anywhere: https fails on both names, plain http works.
        let (port, hits) = serve(|_| home()).await;
        let hosts = [("http-only.test", port), ("www.http-only.test", silent)];
        let target = CrawlTarget::new("http-only.test");
        let page = expect_fetched(crawl_with_hosts(target, &config(), &hosts).await);
        assert_eq!(page.final_url, "http://http-only.test/");
        assert_eq!(hits.paths(), ["/robots.txt", "/"]);

        // The known URL comes before www and http.
        let (port, hits) = serve(|_| home().route("/home", get(|| async { Html(HOME) }))).await;
        let before = connections();
        let known = CrawlTarget {
            known_url: Some("http://known.test/home".into()),
            ..CrawlTarget::new("known.test")
        };
        let hosts = [("known.test", port), ("www.known.test", silent)];
        let page = expect_fetched(crawl_with_hosts(known, &config(), &hosts).await);
        assert_eq!(page.final_url, "http://known.test/home");
        assert_eq!(hits.paths(), ["/robots.txt", "/home"]);
        assert_eq!(connections(), before, "www was tried");

        // An HTTP error is an answer: nothing else is tried.
        let (port, _) =
            serve(|_| Router::new().route("/", get(|| async { StatusCode::SERVICE_UNAVAILABLE })))
                .await;
        let before = connections();
        let status = CrawlTarget {
            domain: "status.test".into(),
            url: "http://status.test/".into(),
            known_url: None,
        };
        let hosts = [("status.test", port), ("www.status.test", silent)];
        let outcome = crawl_with_hosts(status, &config(), &hosts).await;
        assert_eq!(outcome, CrawlOutcome::HttpStatus { status: 503 });
        assert_eq!(connections(), before, "www was tried");

        // Nothing answers: the start URL's failure, naming the others.
        let before = connections();
        let hosts = [("down.test", silent), ("www.down.test", silent)];
        let target = CrawlTarget::new("down.test");
        let (error, network) = expect_failure(crawl_with_hosts(target, &config(), &hosts).await);
        assert!(network, "{error}");
        assert!(
            error.starts_with("robots.txt: error sending request for url (https://down.test/"),
            "{error}"
        );
        assert!(
            error.ends_with("(no answer at https://www.down.test/, http://down.test/ either)"),
            "{error}"
        );
        assert_eq!(connections(), before + 3);
    }

    #[tokio::test]
    async fn offsite_redirects_are_reported_not_followed() {
        // "localhost" is another host than "127.0.0.1" with no registrable
        // domain, so it counts as another site, yet with private addresses
        // allowed it would reach this server: a request for /new would show
        // up in the hits.
        let cfg = config();
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
    async fn robots_txt_redirects_are_followed_anywhere() {
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

        // Another host: followed too (RFC 9309), and its file obeyed.
        let elsewhere = |port: u16| {
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
        };
        let (port, hits) = serve(elsewhere).await;
        let outcome = crawl_one(target(port, "/"), &config()).await;
        assert_eq!(outcome, CrawlOutcome::RobotsDisallowed);
        assert_eq!(hits.paths(), ["/robots.txt", "/elsewhere.txt"]);

        // A hop to a private address is refused when those are.
        let (port, hits) = serve(|port| {
            home().route(
                "/robots.txt",
                get(move || async move {
                    let to = format!("http://127.0.0.1:{port}/elsewhere.txt");
                    (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, to)])
                }),
            )
        })
        .await;
        let start = CrawlTarget {
            domain: "site.test".into(),
            url: format!("http://site.test:{port}/"),
            known_url: None,
        };
        let cfg = CrawlConfig {
            allow_private_addresses: false,
            ..config()
        };
        let outcome = crawl_with_hosts(start, &cfg, &[("site.test", port)]).await;
        assert!(
            matches!(&outcome, CrawlOutcome::Failed { error, .. } if error.contains("private address")),
            "{outcome:?}"
        );
        assert_eq!(hits.paths(), ["/robots.txt"]);

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
    async fn bot_checks_are_not_homepages() {
        let check = r#"<html><head><title>KillBot Verification</title></head><body>
            <a href="https://killbot.example.net/">Protected by KillBot</a></body></html>"#;
        let (port, _) =
            serve(|_| Router::new().route("/", get(move || async move { Html(check) }))).await;
        let outcome = crawl_one(target(port, "/"), &config()).await;
        assert_eq!(
            outcome,
            CrawlOutcome::BotCheck {
                title: Some("KillBot Verification".into())
            }
        );
        let records = to_records(&[CrawlResult {
            domain: "example.test".into(),
            outcome,
        }]);
        assert!(records.is_empty(), "{records:?}");
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
            let downloaded = AtomicU64::new(0);
            let body = read_body(response, limit, &downloaded).await.unwrap();
            assert_eq!(body.len(), expected, "limit {limit}");
            // What was read, which can be a chunk more than what was kept.
            let read = downloaded.load(Ordering::Relaxed);
            assert!(
                read >= expected as u64 && read <= 200_000,
                "limit {limit}: {read}"
            );
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
                known_url: None,
            },
            CrawlTarget {
                domain: "example.com".into(),
                url: format!("http://127.0.0.1:{port}/beta"),
                known_url: None,
            },
            CrawlTarget {
                domain: "gone.example".into(),
                url: format!("http://127.0.0.1:{port}/gone"),
                known_url: None,
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
    fn robots_txt_groups_match_on_the_product_token() {
        // A group for us, named as our User-Agent header spells it.
        let body = "User-agent: *\nAllow: /\n\nUser-agent: PlumbSearch/1.0 (+https://x.test)\nDisallow: /\n";
        let robot = robots_rules(body.as_bytes()).unwrap();
        assert!(!robot.allowed("https://a.com/"));
        for body in [
            "\u{feff}user agent   plumbsearch # us\r\nDisallow: /\r\n",
            "User-agent: Other\nUser-Agent : PlumbSearch/2\nDisallow: /\n",
        ] {
            let robot = robots_rules(body.as_bytes()).unwrap();
            assert!(!robot.allowed("https://a.com/"), "{body:?}");
        }
        // Other crawlers' groups still do not apply.
        let body = "User-agent: PlumbSearchBot/1.0\nDisallow: /\nUser-agent: Plumb\nDisallow: /\n";
        assert!(robots_rules(body.as_bytes())
            .unwrap()
            .allowed("https://a.com/"));
        // Everything else is left as it was.
        let body = b"User-agent: *\r\nDisallow: /a # x\n\nSitemap: https://a.com/s.xml";
        assert_eq!(product_tokens(body), body);
    }

    #[test]
    fn robots_txt_lives_at_the_origin_root() {
        let url = Url::parse("https://user:pw@www.example.com:8443/a/b?q=1#f").unwrap();
        assert_eq!(
            robots_url(&url).as_str(),
            "https://www.example.com:8443/robots.txt"
        );
    }

    /// `lines` lines of `Disallow: /<i><pattern>`.
    fn rules(lines: usize, pattern: &str) -> String {
        (0..lines)
            .map(|i| format!("Disallow: /{i}{pattern}\n"))
            .collect()
    }

    #[test]
    fn robots_txt_parsing_is_bounded() {
        let check = |txt: String| {
            let started = Instant::now();
            let (parsed, peak) = peak_bytes(|| robots_rules(txt.as_bytes()));
            (parsed, peak, started.elapsed())
        };

        // Each of these would take 30 to 250 MB, and up to 1.3 s in a
        // release build. They are refused instead.
        let mut bomb = String::new();
        let mut count = 0;
        while bomb.len() < ROBOTS_MAX_BYTES - 40 {
            bomb.push_str(&format!("Disallow: /*a{count}*b*c$\n"));
            count += 1;
        }
        let costly = [
            // 500 KiB of rules that need a regular expression.
            bomb,
            // Fewer rules with more wildcards, or long ones.
            rules(1000, &format!("{}$", "x*".repeat(38))),
            rules(1000, &format!("{}$", "y".repeat(480))),
            rules(1000, &format!("{}$", "é".repeat(80))),
        ];
        for rules in costly {
            let (parsed, peak, took) = check(robots_txt(&rules, 0));
            let error = parsed.expect_err("parsed");
            assert!(
                error.ends_with("more than the 16 MiB this crawler allows"),
                "{error}"
            );
            assert!(peak < 1 << 20, "{peak} bytes at peak");
            assert!(took < Duration::from_secs(1), "took {took:?}");
        }

        // Close to the limit, then wildcard rules (which need no regular
        // expression) up to 500 KiB.
        let mut big = rules(1150, "*a*b*c$");
        assert!(regex_cost(big.as_bytes()) > MAX_ROBOTS_REGEX_BYTES * 9 / 10);
        let mut i = 0;
        while big.len() < ROBOTS_MAX_BYTES - 40 {
            big.push_str(&format!("Disallow: /*d{i}*e*f\n"));
            i += 1;
        }
        let (parsed, peak, took) = check(robots_txt(&big, 0));
        let robot = parsed.unwrap();
        // 19 to 21 MB, depending on the machine, of which about 3 MB for
        // the wildcard rules and some for regex-automata's full DFAs (see
        // the dev-dependency on it).
        assert!(
            peak < MAX_ROBOTS_REGEX_BYTES + (8 << 20),
            "{peak} bytes at peak"
        );
        // About 1 s in a debug build.
        assert!(took < Duration::from_secs(20), "took {took:?}");
        assert!(robot.allowed("https://example.com/"));
        assert!(!robot.allowed("https://example.com/999abc"));
        assert!(!robot.allowed("https://example.com/xd5ef"));
    }

    #[test]
    fn regex_cost_counts_rules_with_a_dollar() {
        let rule = |bytes: usize, runs: usize| 6 * 1024 + runs * 2 * 1024 + bytes * 64;
        let txt = "Disallow: /a$\r\nAllow: /b # $ in a comment\n# $\n\n\
                   Disallow: /*.pdf$ # pdf\nDisallow: /**x*$\nDisallow: /*y\nAllow: /é$\n";
        assert_eq!(
            regex_cost(txt.as_bytes()),
            rule(13, 0) + rule(18, 1) + rule(16, 2) + rule(15, 0)
        );
        assert_eq!(regex_cost(b""), 0);
    }

    #[test]
    fn fallback_urls_follow_the_start_url() {
        let urls = |target: &CrawlTarget| -> Vec<String> {
            let start = start_url(target).unwrap();
            candidate_urls(&start, target)
                .iter()
                .map(Url::to_string)
                .collect()
        };
        let mut target = CrawlTarget::new("example.com");
        assert_eq!(
            urls(&target),
            [
                "https://example.com/",
                "https://www.example.com/",
                "http://example.com/"
            ]
        );

        // The known URL comes next; URLs are not repeated.
        target.known_url = Some("https://www.example.com/en/".into());
        assert_eq!(
            urls(&target),
            [
                "https://example.com/",
                "https://www.example.com/en/",
                "https://www.example.com/",
                "http://example.com/"
            ]
        );
        target.known_url = Some("https://www.example.com/".into());
        assert_eq!(urls(&target).len(), 3);
        // One on another site, or not an http(s) URL, is skipped.
        for elsewhere in [
            "https://example.org/",
            "http://10.0.0.1/",
            "mailto:info@example.com",
        ] {
            target.known_url = Some(elsewhere.into());
            assert_eq!(urls(&target).len(), 3, "{elsewhere}");
        }

        // The start URL's path, port and scheme are kept.
        let target = CrawlTarget {
            domain: "example.com".into(),
            url: "http://example.com:8080/home?lang=en".into(),
            known_url: None,
        };
        assert_eq!(
            urls(&target),
            [
                "http://example.com:8080/home?lang=en",
                "http://www.example.com:8080/home?lang=en"
            ]
        );

        // Domains are compared as URL hosts.
        let target = CrawlTarget::new("MÜNCHEN.de");
        assert_eq!(
            urls(&target),
            [
                "https://xn--mnchen-3ya.de/",
                "https://www.xn--mnchen-3ya.de/",
                "http://xn--mnchen-3ya.de/"
            ]
        );

        // A start URL on another host, or on www already, has no variants.
        let other_host = CrawlTarget {
            domain: "example.com".into(),
            url: "http://shop.example.com/".into(),
            known_url: None,
        };
        assert_eq!(urls(&other_host), ["http://shop.example.com/"]);
        let www = CrawlTarget {
            domain: "www.example.com".into(),
            url: "http://www.example.com/".into(),
            known_url: None,
        };
        assert_eq!(urls(&www), ["http://www.example.com/"]);
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
