//! Fetching a site's inner pages: the pages its sitemaps (or its index
//! pages) list under the addresses asked for, each fetched like a homepage
//! ([`crate::crawl_homepages`]): robots.txt obeyed for every URL, one
//! request at a time per host with the per-host delay (or the robots.txt
//! `Crawl-delay`) between them, redirects followed only on the same site.
//!
//! Page sets of good sites' inner pages (software docs, universities) are
//! made from what this finds: each page's title and description.

use std::collections::HashSet;
use std::io::Read;

use reqwest::header::ACCEPT;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use url::Url;

use crate::crawl::{
    allowed, build_client, error_text, http_url, page_delay, read_body, redirect_target, same_site,
    Robots, Visit,
};
use crate::{CrawlConfig, CrawlOutcome, PageMeta};

/// Sitemap files read per site, sitemap indexes included.
pub const MAX_SITEMAPS: usize = 200;
/// A sitemap or index page is read up to this size, unpacked.
const MAX_LIST_BYTES: usize = 50 * 1024 * 1024;
/// `Accept` header for sitemaps and index pages.
const ACCEPT_LIST: &str = "application/xml,text/xml,text/html;q=0.9,*/*;q=0.5";
/// A site's pages found are kept up to this many times the pages asked for
/// before the shallowest are picked, so a huge sitemap is not held whole.
const FOUND_FACTOR: usize = 20;

/// A site whose inner pages to fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitePagesTarget {
    /// Registrable domain of the site: `python.org`.
    pub domain: String,
    /// Where the pages wanted are: only pages whose address starts with
    /// one of these are taken (`https://docs.python.org/3/`).
    pub roots: Vec<String>,
    /// Sitemaps to read. When there are none, those the robots.txt of each
    /// root's origin names are read, else `/sitemap.xml` of the origin.
    #[serde(default)]
    pub sitemaps: Vec<String>,
    /// Pages that link to the pages wanted (a docs site's table of contents
    /// or index), read for links under the roots, besides the sitemaps.
    #[serde(default)]
    pub index_pages: Vec<String>,
    /// Most pages fetched; the shallowest found first.
    pub max_pages: usize,
}

/// A page fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitePage {
    /// Where the page was, after redirects.
    pub url: String,
    pub meta: PageMeta,
}

/// What [`fetch_site_pages`] found for one site.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitePagesResult {
    pub domain: String,
    /// The pages fetched, in the order they were asked for.
    pub pages: Vec<SitePage>,
    /// Pages found under the roots, before the cut to `max_pages`.
    pub found: usize,
    /// Pages asked for that robots.txt disallowed, did not answer with a
    /// web page, or failed.
    pub skipped: usize,
    /// Per requested URL, including skipped pages. Reasons are bounded
    /// categories, rather than upstream bodies or sensitive error strings.
    #[serde(default)]
    pub outcomes: Vec<SitePageOutcome>,
    /// Reached the end of the bounded page list (not transfer completeness).
    #[serde(default)]
    pub finished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitePageOutcome {
    pub url: String,
    pub final_url: Option<String>,
    pub status: String,
}

/// Fetches the inner pages of `target`: those its sitemaps and index pages
/// list under its roots, the shallowest first (fewest `/` in the path, then
/// the order they were listed in), up to `target.max_pages`. Pages are
/// fetched one at a time, with robots.txt and the per-host delay obeyed as
/// in a homepage crawl; icons are not fetched and `cfg.target_deadline`
/// does not apply (each request has `cfg.timeout`). A redirect off the
/// roots, a repeat, a bot check or a page that is not HTML is skipped.
/// Must run inside a Tokio runtime.
pub async fn fetch_site_pages(target: &SitePagesTarget, cfg: &CrawlConfig) -> SitePagesResult {
    fetch_site_pages_with_extraction(target, cfg, crate::InnerPageExtraction::Compact).await
}

/// Opt-in inner-page content; request, body, robots and redirect bounds
/// are identical to [`fetch_site_pages`].
pub async fn fetch_site_pages_with_extraction(
    target: &SitePagesTarget,
    cfg: &CrawlConfig,
    extraction: crate::InnerPageExtraction,
) -> SitePagesResult {
    let mut result = SitePagesResult {
        domain: target.domain.clone(),
        ..SitePagesResult::default()
    };
    let cfg = CrawlConfig {
        fetch_icons: false,
        ..cfg.clone()
    };
    let client = match build_client(&cfg) {
        Ok(client) => client,
        Err(err) => {
            info!("{}: cannot fetch pages: {}", target.domain, error_text(err));
            return result;
        }
    };
    let mut roots: Vec<Url> = target
        .roots
        .iter()
        .filter_map(|root| http_url(root).ok())
        .collect();
    if roots.is_empty() {
        info!("{}: no usable roots", target.domain);
        return result;
    }
    let mut visit = Visit::new(&client, &cfg);
    visit.set_inner_page_extraction(extraction);
    let mut indexes = Vec::new();
    for index in &target.index_pages {
        let Ok(url) = http_url(index) else { continue };
        match fetch_list(&mut visit, &cfg, url.clone()).await {
            Ok((at, body)) => {
                result.outcomes.push(SitePageOutcome {
                    url: url.to_string(),
                    final_url: Some(at.to_string()),
                    status: "discovery-fetched".into(),
                });
                // "en/stable/contents/" sends you on to "en/5.2/contents/":
                // the pages are under "en/5.2/" too.
                if let Some(root) = moved_root(&url, &at, &roots) {
                    info!("{}: {url} is now under {root}", target.domain);
                    roots.push(root);
                }
                indexes.push((at, body));
            }
            Err(error) => {
                result.outcomes.push(SitePageOutcome {
                    url: url.to_string(),
                    final_url: None,
                    status: "discovery-failed".into(),
                });
                info!("{}: index page {url}: {error}", target.domain);
            }
        }
    }
    let wanted = target.max_pages.max(1);
    let mut found = Found::new(&roots, wanted.saturating_mul(FOUND_FACTOR));
    for (at, body) in &indexes {
        if crate::extract::extract_page_meta(at, &String::from_utf8_lossy(body))
            .title
            .is_some()
        {
            found.add(at.clone());
        }
        for link in page_links(at, &String::from_utf8_lossy(body)) {
            found.add(link);
        }
    }
    let sitemaps = sitemaps_of(&mut visit, target, &roots).await;
    result
        .outcomes
        .extend(read_sitemaps(&mut visit, &cfg, &target.domain, sitemaps, &mut found).await);
    result.found = found.count;
    let urls = found.balanced(wanted);
    info!(
        "{}: {} pages found under {}, fetching {}",
        target.domain,
        result.found,
        target.roots.join(", "),
        urls.len()
    );
    let mut seen = HashSet::new();
    let mut why = std::collections::BTreeMap::<String, usize>::new();
    for url in urls {
        let mut outcome = visit.fetch_homepage(&target.domain, &url).await;
        // A page that only sends you on, with a `<meta>` refresh or a
        // script ("Redirecting…"): the page it sends you to, on its host.
        if let CrawlOutcome::Fetched(page) = &outcome {
            if is_stub(page.meta.title.as_deref()) {
                if let Ok(stub) = Url::parse(&page.final_url) {
                    if let Ok((at, body)) = fetch_list(&mut visit, &cfg, stub).await {
                        if let Some(next) = refresh_target(&at, &String::from_utf8_lossy(&body))
                            .filter(|next| next.host_str() == at.host_str() && *next != at)
                        {
                            outcome = visit.fetch_homepage(&target.domain, &next).await;
                        }
                    }
                }
            }
        }
        let mut final_url = None;
        let skipped = match outcome {
            CrawlOutcome::Fetched(page) => {
                let at = Url::parse(&page.final_url).ok().map(|mut at| {
                    at.set_fragment(None);
                    at
                });
                match at {
                    // A page under the roots, or one it redirects to on the
                    // same host ("stable/" to "2.9/").
                    Some(at)
                        if (under_roots(&at, &roots) || at.host_str() == url.host_str())
                            && !seen.contains(at.as_str()) =>
                    {
                        seen.insert(at.to_string());
                        final_url = Some(at.to_string());
                        result.pages.push(SitePage {
                            url: at.into(),
                            meta: page.meta,
                        });
                        None
                    }
                    Some(at) if seen.contains(at.as_str()) => Some("a page fetched before".into()),
                    _ => Some("a redirect off the site's host".into()),
                }
            }
            CrawlOutcome::RobotsDisallowed => Some("disallowed by robots.txt".into()),
            CrawlOutcome::OffsiteRedirect { .. } => Some("a redirect to another site".into()),
            CrawlOutcome::HttpStatus { status } => Some(format!("HTTP {status}")),
            CrawlOutcome::NotHtml { .. } => Some("not a web page".into()),
            CrawlOutcome::BotCheck { .. } => Some("a bot check".into()),
            CrawlOutcome::Failed { error, .. } => {
                debug!("{url}: {error}");
                Some(if error.starts_with("robots.txt") {
                    "robots.txt could not be read".into()
                } else {
                    "failed".into()
                })
            }
        };
        result.outcomes.push(SitePageOutcome {
            url: url.to_string(),
            final_url,
            status: skipped.clone().unwrap_or_else(|| "fetched".into()),
        });
        if let Some(reason) = skipped {
            result.skipped += 1;
            *why.entry(reason).or_default() += 1;
        }
    }
    let why: Vec<String> = why
        .into_iter()
        .map(|(reason, count)| format!("{count} {reason}"))
        .collect();
    info!(
        "{}: fetched {} pages, skipped {}{}",
        target.domain,
        result.pages.len(),
        result.skipped,
        if why.is_empty() {
            String::new()
        } else {
            format!(" ({})", why.join(", "))
        }
    );
    result.finished = true;
    result
}

/// The sitemaps to read for `target`: its own, else those the robots.txt
/// of each root's origin names, else each origin's `/sitemap.xml`.
async fn sitemaps_of(visit: &mut Visit<'_>, target: &SitePagesTarget, roots: &[Url]) -> Vec<Url> {
    if !target.sitemaps.is_empty() {
        return target
            .sitemaps
            .iter()
            .filter_map(|sitemap| http_url(sitemap).ok())
            .collect();
    }
    let mut sitemaps = Vec::new();
    let mut origins = HashSet::new();
    for root in roots {
        if !origins.insert(root.origin()) {
            continue;
        }
        let named: Vec<Url> = match visit.robots(root).await {
            Robots::Rules(robot) => robot
                .sitemaps
                .iter()
                .filter_map(|sitemap| http_url(sitemap).ok())
                .collect(),
            _ => Vec::new(),
        };
        if named.is_empty() {
            if let Ok(default) = root.join("/sitemap.xml") {
                sitemaps.push(default);
            }
        }
        sitemaps.extend(named);
    }
    sitemaps
}

/// Reads `sitemaps` and the sitemaps their indexes list, breadth first, up
/// to [`MAX_SITEMAPS`] files, adding the pages they list to `found`.
async fn read_sitemaps(
    visit: &mut Visit<'_>,
    cfg: &CrawlConfig,
    domain: &str,
    sitemaps: Vec<Url>,
    found: &mut Found<'_>,
) -> Vec<SitePageOutcome> {
    let mut outcomes = Vec::new();
    let mut queue: std::collections::VecDeque<Url> = sitemaps.into();
    let mut read = HashSet::new();
    while let Some(sitemap) = queue.pop_front() {
        if read.len() == MAX_SITEMAPS || found.full() {
            break;
        }
        if !read.insert(sitemap.clone()) {
            continue;
        }
        let body = match fetch_list(visit, cfg, sitemap.clone()).await {
            Ok((_, body)) => body,
            Err(error) => {
                outcomes.push(SitePageOutcome {
                    url: sitemap.to_string(),
                    final_url: None,
                    status: if error.contains("404") {
                        "discovery-unavailable"
                    } else {
                        "discovery-failed"
                    }
                    .into(),
                });
                info!("{domain}: sitemap {sitemap}: {error}");
                continue;
            }
        };
        let Some(listed) = parse_sitemap(&body) else {
            outcomes.push(SitePageOutcome {
                url: sitemap.to_string(),
                final_url: None,
                status: "discovery-unavailable".into(),
            });
            info!("{domain}: sitemap {sitemap} does not read");
            continue;
        };
        outcomes.push(SitePageOutcome {
            url: sitemap.to_string(),
            final_url: None,
            status: "discovery-fetched".into(),
        });
        for nested in listed.sitemaps {
            if let Ok(url) = http_url(&nested) {
                // Only a site's own sitemaps: robots.txt may name another
                // site's, and a sitemap index another host's.
                if same_site(&sitemap, &url) {
                    queue.push_back(url);
                }
            }
        }
        for page in listed.pages {
            if let Ok(url) = http_url(&page) {
                found.add(url);
            }
        }
    }
    info!(
        "{domain}: read {} sitemaps, found {} pages",
        read.len(),
        found.count
    );
    outcomes
}

/// Fetches a sitemap or index page at `url`, following redirects on the
/// same site, each URL allowed by its robots.txt. A packed sitemap
/// (`.xml.gz`) is unpacked. Returns where it was and its body.
async fn fetch_list(
    visit: &mut Visit<'_>,
    cfg: &CrawlConfig,
    start: Url,
) -> Result<(Url, Vec<u8>), String> {
    let mut url = start.clone();
    for _ in 0..=cfg.max_redirects {
        let crawl_delay = match visit.robots(&url).await {
            Robots::DoNotCrawl(_) => return Err("robots.txt could not be read".into()),
            Robots::NoRules => None,
            Robots::Rules(robot) => {
                if !allowed(&robot, &url).await {
                    return Err("robots.txt disallows it".into());
                }
                robot.delay
            }
        };
        let delay = page_delay(cfg.per_host_delay, crawl_delay);
        let request = visit.client().get(url.clone()).header(ACCEPT, ACCEPT_LIST);
        let response = visit.send(&url, delay, request).await.map_err(error_text)?;
        if let Some(next) = redirect_target(&response) {
            if !same_site(&start, &next) {
                return Err(format!("redirects off the site, to {next}"));
            }
            url = next;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let at = response.url().clone();
        let body = read_body(response, MAX_LIST_BYTES, &cfg.downloaded)
            .await
            .map_err(error_text)?;
        return Ok((at, unpack(body)?));
    }
    Err("too many redirects".into())
}

/// `body` unpacked when it is gzip (a `.xml.gz` sitemap), else as it is.
fn unpack(body: Vec<u8>) -> Result<Vec<u8>, String> {
    if !body.starts_with(&[0x1f, 0x8b]) {
        return Ok(body);
    }
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body.as_slice())
        .take(MAX_LIST_BYTES as u64)
        .read_to_end(&mut out)
        .map_err(|err| format!("unpacking: {err}"))?;
    Ok(out)
}

/// What a sitemap lists.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Listed {
    /// Pages, from a `<urlset>`.
    pub pages: Vec<String>,
    /// Other sitemaps, from a `<sitemapindex>`.
    pub sitemaps: Vec<String>,
}

/// The pages or sitemaps a sitemap lists (the `<loc>` of each `<url>` or
/// `<sitemap>`), or a plain-text sitemap's lines. `None` when it reads as
/// neither.
pub(crate) fn parse_sitemap(body: &[u8]) -> Option<Listed> {
    let text = String::from_utf8_lossy(body);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    if !text.starts_with('<') {
        // A text sitemap: one address a line.
        let pages: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("http://") || line.starts_with("https://"))
            .map(String::from)
            .collect();
        return (!pages.is_empty()).then_some(Listed {
            pages,
            sitemaps: Vec::new(),
        });
    }
    let options = roxmltree::ParsingOptions {
        allow_dtd: false,
        ..Default::default()
    };
    let doc = roxmltree::Document::parse_with_options(text, options).ok()?;
    let mut listed = Listed::default();
    for node in doc.descendants().filter(|n| n.tag_name().name() == "loc") {
        let Some(loc) = node.text().map(str::trim).filter(|loc| !loc.is_empty()) else {
            continue;
        };
        match node.parent_element().map(|p| p.tag_name().name()) {
            Some("url") => listed.pages.push(loc.to_string()),
            Some("sitemap") => listed.sitemaps.push(loc.to_string()),
            _ => {}
        }
    }
    Some(listed)
}

/// Whether a page titled `title` only sends you on to another.
fn is_stub(title: Option<&str>) -> bool {
    title.is_some_and(|title| {
        let title = title.trim().to_lowercase();
        title.starts_with("redirecting") || title == "redirect" || title == "moved"
    })
}

/// Where the page `html` at `base` sends you: its `<meta
/// http-equiv="refresh">` address, else the first address a script sets
/// `location` to.
pub(crate) fn refresh_target(base: &Url, html: &str) -> Option<Url> {
    let lower = html.to_ascii_lowercase();
    let quoted = |from: usize| -> Option<&str> {
        let rest = html.get(from..)?.trim_start();
        let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let rest = &rest[1..];
        Some(&rest[..rest.find(quote)?])
    };
    if let Some(at) = lower
        .find("http-equiv=\"refresh\"")
        .or_else(|| lower.find("http-equiv='refresh'"))
        .or_else(|| lower.find("http-equiv=refresh"))
    {
        let start = lower[..at].rfind('<').unwrap_or(0);
        let end = at + lower[at..].find('>').unwrap_or(lower.len() - at);
        let tag = &lower[start..end];
        if let Some(url_at) = tag.find("url=") {
            let from = start + url_at + "url=".len();
            let rest = &html[from..end];
            let target = rest
                .trim_start_matches(['\'', '"'])
                .split(['"', '\'', ';'])
                .next()
                .unwrap_or("")
                .trim();
            if let Ok(url) = base.join(&html_unescape(target)) {
                return Some(url);
            }
        }
    }
    for key in [
        "location.href",
        "location.replace(",
        "location.assign(",
        "location =",
    ] {
        let Some(at) = lower.find(key) else { continue };
        let mut from = at + key.len();
        // "location.href = '…'"
        let rest = &lower[from..];
        let skipped = rest.len() - rest.trim_start_matches([' ', '=']).len();
        from += skipped;
        if let Some(target) = quoted(from) {
            if let Ok(url) = base.join(target) {
                return Some(url);
            }
        }
    }
    None
}

/// The addresses `html` links to (`href` attributes), resolved against
/// `base`, without fragments.
pub(crate) fn page_links(base: &Url, html: &str) -> Vec<Url> {
    let mut links = Vec::new();
    let mut rest = html;
    while let Some(at) = find_href(rest) {
        rest = &rest[at..];
        let href = match rest.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                rest = &rest[1..];
                let Some(end) = rest.find(quote) else { break };
                let href = html_unescape(&rest[..end]);
                rest = &rest[end + 1..];
                href
            }
            // Minified pages leave values unquoted: `<a href=fs.html>`.
            Some(c) if !c.is_whitespace() && c != '>' => {
                let end = rest
                    .find(|c: char| c.is_whitespace() || c == '>')
                    .unwrap_or(rest.len());
                let href = html_unescape(&rest[..end]);
                rest = &rest[end..];
                href
            }
            _ => continue,
        };
        if let Ok(mut url) = base.join(href.trim()) {
            if matches!(url.scheme(), "http" | "https") {
                url.set_fragment(None);
                links.push(url);
            }
        }
    }
    links
}

/// Where the value after the next `href=` starts in `html`, ignoring case.
fn find_href(html: &str) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut from = 0;
    while let Some(i) = html[from..].find('=').map(|i| i + from) {
        let name = &bytes[i.saturating_sub(4)..i];
        let after_space = i >= 5 && bytes[i - 5].is_ascii_whitespace();
        if name.eq_ignore_ascii_case(b"href") && after_space {
            return Some(i + 1);
        }
        from = i + 1;
    }
    None
}

/// `&amp;` and the like in an attribute value, as plain text.
fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// Where the root that `asked`, a page under one of `roots`, was under has
/// moved to, when the page sent you on to `at` on the same host with the
/// same path after the root: "https://docs.a.org/en/5.2/" for
/// "https://docs.a.org/en/stable/contents/" sending you to
/// "https://docs.a.org/en/5.2/contents/". `None` when it did not move or
/// moved somewhere already under the roots.
fn moved_root(asked: &Url, at: &Url, roots: &[Url]) -> Option<Url> {
    if at.host_str() != asked.host_str() || under_roots(at, roots) {
        return None;
    }
    let root = roots
        .iter()
        .find(|root| under_roots(asked, std::slice::from_ref(root)))?;
    let rest = &asked.path()[root.path().len()..];
    let prefix = at.path().strip_suffix(rest)?;
    if prefix.is_empty() || !prefix.ends_with('/') {
        return None;
    }
    let mut moved = at.clone();
    moved.set_path(prefix);
    moved.set_query(None);
    moved.set_fragment(None);
    Some(moved)
}

/// Whether `url` is under one of `roots`: on the same scheme and host, and
/// its path starts with the root's.
pub(crate) fn under_roots(url: &Url, roots: &[Url]) -> bool {
    roots.iter().any(|root| {
        url.host_str() == root.host_str()
            && (url.scheme() == root.scheme() || url.scheme() == "https")
            && url.path().starts_with(root.path())
    })
}

/// Pages found under the roots, each once, in the order found.
struct Found<'a> {
    roots: &'a [Url],
    urls: Vec<Url>,
    seen: HashSet<String>,
    /// Pages found in all, kept or not.
    count: usize,
    most: usize,
}

impl<'a> Found<'a> {
    fn new(roots: &'a [Url], most: usize) -> Self {
        Found {
            roots,
            urls: Vec::new(),
            seen: HashSet::new(),
            count: 0,
            most,
        }
    }

    fn add(&mut self, mut url: Url) {
        url.set_fragment(None);
        // Sitemaps often list a site's pages as http://.
        if url.scheme() == "http"
            && self
                .roots
                .iter()
                .any(|root| root.scheme() == "https" && root.host_str() == url.host_str())
        {
            let _ = url.set_scheme("https");
        }
        if !under_roots(&url, self.roots) || is_not_a_page(&url) {
            return;
        }
        if !self.seen.insert(url.to_string()) {
            return;
        }
        self.count += 1;
        if self.urls.len() < self.most {
            self.urls.push(url);
        }
    }

    fn full(&self) -> bool {
        self.urls.len() >= self.most
    }

    /// Reserve slots across configured roots (or top-level sections of a
    /// whole-site root), then take shallow pages within each section.
    fn balanced(self, wanted: usize) -> Vec<Url> {
        if self.urls.len() <= wanted {
            return self.shallowest(wanted);
        }
        let mut groups = std::collections::BTreeMap::<String, Vec<Url>>::new();
        for url in self.urls {
            let root = self
                .roots
                .iter()
                .filter(|root| under_roots(&url, std::slice::from_ref(root)))
                .max_by_key(|root| root.path().len());
            let Some(root) = root else { continue };
            let key = if self.roots.len() > 1 {
                root.as_str().to_string()
            } else {
                let relative = url.path().strip_prefix(root.path()).unwrap_or(url.path());
                let section = relative
                    .trim_start_matches('/')
                    .split('/')
                    .next()
                    .unwrap_or("");
                if section.contains('.') || section.is_empty() {
                    "overview".into()
                } else {
                    section.into()
                }
            };
            groups.entry(key).or_default().push(url);
        }
        let mut groups: Vec<_> = groups
            .into_values()
            .map(|mut urls| {
                urls.sort_by_key(depth);
                std::collections::VecDeque::from(urls)
            })
            .collect();
        groups.sort_by_key(|g| g.front().map_or(usize::MAX, depth));
        let mut chosen = Vec::new();
        while chosen.len() < wanted {
            let before = chosen.len();
            for group in &mut groups {
                if let Some(url) = group.pop_front() {
                    chosen.push(url);
                }
                if chosen.len() == wanted {
                    break;
                }
            }
            if chosen.len() == before {
                break;
            }
        }
        chosen
    }

    /// The `wanted` shallowest pages, those listed first among equals.
    fn shallowest(self, wanted: usize) -> Vec<Url> {
        let mut urls: Vec<(usize, usize, Url)> = self
            .urls
            .into_iter()
            .enumerate()
            .map(|(i, url)| (depth(&url), i, url))
            .collect();
        urls.sort_by_key(|(depth, i, _)| (*depth, *i));
        urls.truncate(wanted);
        urls.into_iter().map(|(_, _, url)| url).collect()
    }
}

/// How deep `url` is: the non-empty segments of its path.
fn depth(url: &Url) -> usize {
    url.path().split('/').filter(|s| !s.is_empty()).count()
}

/// Files that are not web pages, by their extension.
fn is_not_a_page(url: &Url) -> bool {
    const FILES: &[&str] = &[
        ".pdf", ".zip", ".gz", ".tgz", ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".css",
        ".js", ".json", ".xml", ".txt", ".mp4", ".mp3", ".ico", ".woff", ".woff2", ".tar", ".epub",
    ];
    let path = url.path().to_ascii_lowercase();
    FILES.iter().any(|ext| path.ends_with(ext)) || url.query().is_some()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn bounded_selection_reserves_slots_across_roots() {
        let roots = [
            Url::parse("https://example.org/forms/").unwrap(),
            Url::parse("https://example.org/benefits/").unwrap(),
        ];
        let mut found = Found::new(&roots, 100);
        for i in 0..20 {
            found.add(Url::parse(&format!("https://example.org/forms/{i}")).unwrap());
        }
        found.add(Url::parse("https://example.org/benefits/retirement/apply").unwrap());
        let chosen = found.balanced(4);
        assert_eq!(chosen.len(), 4);
        assert!(chosen.iter().any(|u| u.path().starts_with("/benefits/")));
    }

    #[tokio::test]
    async fn fetches_the_pages_sitemaps_and_index_pages_list() {
        use axum::response::{Html, Redirect};
        use axum::routing::get;
        use axum::Router;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let at = move |path: &str| format!("http://127.0.0.1:{port}{path}");
        let page = |title: &'static str| {
            move || async move {
                Html(format!(
                    "<html><head><title>{title}</title></head><body><p>Text.</p></body></html>"
                ))
            }
        };
        let robots = format!(
            "User-agent: *\nDisallow: /docs/secret\nSitemap: {}\n",
            at("/sitemap_index.xml")
        );
        let index = format!(
            "<sitemapindex><sitemap><loc>{}</loc></sitemap></sitemapindex>",
            at("/s1.xml")
        );
        let urlset = format!(
            "<urlset>{}</urlset>",
            [
                "/docs/a.html",
                "/docs/deep/b.html",
                "/docs/secret.html",
                "/other/c.html",
                "/docs/moved.html"
            ]
            .iter()
            .map(|path| format!("<url><loc>{}</loc></url>", at(path)))
            .collect::<String>()
        );
        let app = Router::new()
            .route("/robots.txt", get(move || async move { robots }))
            .route("/sitemap_index.xml", get(move || async move { index }))
            .route("/s1.xml", get(move || async move { urlset }))
            .route(
                "/docs/toc.html",
                get(|| async { Html(r#"<a href="c.html">C</a> <a href="/other/d.html">D</a>"#) }),
            )
            .route("/docs/a.html", get(page("A - Docs")))
            .route("/docs/c.html", get(page("C - Docs")))
            .route("/docs/deep/b.html", get(page("B - Docs")))
            .route("/docs/secret.html", get(page("Secret")))
            .route(
                "/docs/moved.html",
                get(|| async { Redirect::permanent("/docs/a.html") }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cfg = CrawlConfig {
            per_host_delay: Duration::from_millis(5),
            timeout: Duration::from_secs(3),
            allow_private_addresses: true,
            ..CrawlConfig::default()
        };
        let target = SitePagesTarget {
            domain: "example.test".into(),
            roots: vec![at("/docs/")],
            sitemaps: Vec::new(),
            index_pages: vec![at("/docs/toc.html")],
            max_pages: 10,
        };
        let result = fetch_site_pages(&target, &cfg).await;
        let fetched: Vec<(String, Option<String>)> = result
            .pages
            .iter()
            .map(|page| (page.url.clone(), page.meta.title.clone()))
            .collect();
        assert_eq!(
            fetched,
            [
                (at("/docs/c.html"), Some("C - Docs".to_string())),
                (at("/docs/a.html"), Some("A - Docs".to_string())),
                (at("/docs/deep/b.html"), Some("B - Docs".to_string())),
            ]
        );
        // c, a, deep/b, secret and moved were found; secret is disallowed
        // and moved is a page already fetched.
        assert_eq!(result.found, 5);
        assert_eq!(result.skipped, 2);
        assert!(result.pages.iter().all(|page| page.meta.search.is_none()));
        assert!(result.finished);
        assert!(result
            .outcomes
            .iter()
            .any(|o| o.url.ends_with("secret.html") && o.status == "disallowed by robots.txt"));

        let few = fetch_site_pages(
            &SitePagesTarget {
                max_pages: 1,
                ..target
            },
            &cfg,
        )
        .await;
        assert_eq!(few.pages.len(), 1);
    }

    #[tokio::test]
    async fn rich_fetch_keeps_robots_and_body_bounds() {
        use axum::{response::Html, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let sitemap = format!("<urlset><url><loc>{base}/docs/allowed</loc></url><url><loc>{base}/docs/secret</loc></url></urlset>");
        let html = format!("<title>Allowed</title><h2 id='early'>API</h2><pre>early_symbol</pre><p>{}</p><pre>late_symbol</pre>", "filler ".repeat(200));
        let app = Router::new()
            .route(
                "/robots.txt",
                get(|| async { "User-agent: *\nDisallow: /docs/secret\n" }),
            )
            .route("/sitemap.xml", get(move || async { sitemap }))
            .route("/docs/allowed", get(move || async { Html(html) }))
            .route(
                "/docs/secret",
                get(|| async { Html("<pre>secret_symbol</pre>") }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cfg = CrawlConfig {
            max_bytes: 256,
            per_host_delay: Duration::ZERO,
            allow_private_addresses: true,
            ..CrawlConfig::default()
        };
        let target = SitePagesTarget {
            domain: "example.test".into(),
            roots: vec![format!("{base}/docs/")],
            max_pages: 10,
            ..SitePagesTarget::default()
        };
        let result =
            fetch_site_pages_with_extraction(&target, &cfg, crate::InnerPageExtraction::Docs).await;
        assert_eq!(result.pages.len(), 1);
        assert_eq!(result.skipped, 1);
        let search = result.pages[0].meta.search.as_ref().unwrap();
        assert!(search.text().contains("early_symbol"));
        assert!(!search.text().contains("late_symbol"));
        assert!(!search.text().contains("secret_symbol"));
        server.abort();
    }

    #[test]
    fn reads_sitemaps_and_indexes() {
        let urlset = br#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc> https://docs.a.org/3/library/functions.html </loc><priority>0.5</priority></url>
  <url><loc>https://docs.a.org/3/howto/sorting.html</loc></url>
</urlset>"#;
        let listed = parse_sitemap(urlset).unwrap();
        assert_eq!(
            listed.pages,
            [
                "https://docs.a.org/3/library/functions.html",
                "https://docs.a.org/3/howto/sorting.html"
            ]
        );
        assert!(listed.sitemaps.is_empty());

        let index = br#"<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <sitemap><loc>https://a.org/s1.xml.gz</loc></sitemap></sitemapindex>"#;
        let listed = parse_sitemap(index).unwrap();
        assert_eq!(listed.sitemaps, ["https://a.org/s1.xml.gz"]);
        assert!(listed.pages.is_empty());

        let text = b"https://a.org/x\n\nhttps://a.org/y\n";
        assert_eq!(parse_sitemap(text).unwrap().pages.len(), 2);
        assert!(parse_sitemap(b"<html><oops").is_none());
        assert!(parse_sitemap(b"nothing here").is_none());
    }

    #[test]
    fn unpacks_packed_sitemaps() {
        use std::io::Write;
        let mut packed = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        packed.write_all(b"<urlset/>").unwrap();
        let packed = packed.finish().unwrap();
        assert_eq!(unpack(packed).unwrap(), b"<urlset/>");
        assert_eq!(unpack(b"<urlset/>".to_vec()).unwrap(), b"<urlset/>");
    }

    #[test]
    fn finds_links_on_index_pages() {
        let base = Url::parse("https://docs.a.org/3/contents.html").unwrap();
        let html = r#"<a class="x" href="library/index.html">L</a>
            <A HREF='howto/sorting.html#sortinghowto'>S</A>
            <a href="https://other.org/">O</a> <a data-href="nope.html">N</a>
            <a href="mailto:x@a.org">M</a> <a href="q.html?a=1&amp;b=2">Q</a>
            <a href=fs.html>F</a><a class=x href=path.html#p>P</a>"#;
        let links: Vec<String> = page_links(&base, html)
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            links,
            [
                "https://docs.a.org/3/library/index.html",
                "https://docs.a.org/3/howto/sorting.html",
                "https://other.org/",
                "https://docs.a.org/3/q.html?a=1&b=2",
                "https://docs.a.org/3/fs.html",
                "https://docs.a.org/3/path.html",
            ]
        );
    }

    #[test]
    fn finds_where_a_page_sends_you() {
        let base = Url::parse("https://docs.a.org/docs/stable/amp.html").unwrap();
        let at = |html: &str| refresh_target(&base, html).map(String::from);
        assert_eq!(
            at(r#"<meta http-equiv="refresh" content="0; url=../2.9/amp.html">"#).as_deref(),
            Some("https://docs.a.org/docs/2.9/amp.html")
        );
        assert_eq!(
            at(r#"<META HTTP-EQUIV="Refresh" CONTENT="0;URL='/docs/2.9/amp.html'">"#).as_deref(),
            Some("https://docs.a.org/docs/2.9/amp.html")
        );
        assert_eq!(
            at(r#"<script>window.location.href = "/docs/2.9/amp.html";</script>"#).as_deref(),
            Some("https://docs.a.org/docs/2.9/amp.html")
        );
        assert_eq!(
            at(r#"<script>window.location.replace('/docs/2.9/amp.html')</script>"#).as_deref(),
            Some("https://docs.a.org/docs/2.9/amp.html")
        );
        assert_eq!(at("<p>Just a page</p>"), None);
        assert!(is_stub(Some("Redirecting…")));
        assert!(!is_stub(Some("Redirects in nginx")));
    }

    #[test]
    fn follows_a_root_that_moved() {
        let url = |s: &str| Url::parse(s).unwrap();
        let roots = [url("https://docs.a.org/en/stable/")];
        assert_eq!(
            moved_root(
                &url("https://docs.a.org/en/stable/contents/"),
                &url("https://docs.a.org/en/5.2/contents/"),
                &roots
            ),
            Some(url("https://docs.a.org/en/5.2/"))
        );
        // Not moved, moved to another site, or to another page.
        assert_eq!(
            moved_root(
                &url("https://docs.a.org/en/stable/contents/"),
                &url("https://docs.a.org/en/stable/contents/"),
                &roots
            ),
            None
        );
        assert_eq!(
            moved_root(
                &url("https://docs.a.org/en/stable/contents/"),
                &url("https://b.org/en/5.2/contents/"),
                &roots
            ),
            None
        );
        assert_eq!(
            moved_root(
                &url("https://docs.a.org/en/stable/contents/"),
                &url("https://docs.a.org/en/5.2/"),
                &roots
            ),
            None
        );
    }

    #[test]
    fn keeps_pages_under_the_roots_shallowest_first() {
        let roots = [Url::parse("https://docs.a.org/3/").unwrap()];
        let mut found = Found::new(&roots, 100);
        for url in [
            "https://docs.a.org/3/library/os.path.html",
            "https://docs.a.org/2/library/os.html",
            "https://docs.a.org/3/tutorial.html",
            "https://docs.a.org/3/tutorial.html#intro",
            "https://other.a.org/3/x.html",
            "https://docs.a.org/3/_static/logo.png",
            "https://docs.a.org/3/search.html?q=x",
            "http://docs.a.org/3/glossary.html",
        ] {
            found.add(Url::parse(url).unwrap());
        }
        assert_eq!(found.count, 3);
        let urls: Vec<String> = found.shallowest(2).into_iter().map(String::from).collect();
        assert_eq!(
            urls,
            [
                "https://docs.a.org/3/tutorial.html",
                "https://docs.a.org/3/glossary.html"
            ]
        );
    }
}
