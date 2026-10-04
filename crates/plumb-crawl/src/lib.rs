//! Polite homepage crawler for Plumb Search.
//!
//! Plumb only needs a site's homepage `<head>` (title, description,
//! `og:site_name`) and the links on that page, so this crawler fetches one
//! page per domain, after checking robots.txt. Outbound links are how the
//! index grows on its own: every crawl reports link text for, and
//! discovers, other domains.
//!
//! - [`crawl_homepages`] fetches homepages (robots.txt, delays, redirect,
//!   size and time limits).
//! - [`extract_page_meta`] reads names and outbound links out of a page.
//! - [`to_records`] turns crawl results into [`plumb_core::SiteRecord`]s
//!   to merge into a [`plumb_core::RecordSet`].

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

mod crawl;
mod dns;
mod extract;
mod icon;
mod records;
#[cfg(test)]
mod test_alloc;

pub use crawl::{crawl_homepages, fetch_site_icons, log_summary, HomepageCrawler};
pub use extract::{extract_page_meta, MAX_BODY_WORDS, MAX_ICONS, MAX_OUT_LINKS};
pub use icon::{normalize_icon, ICON_SIZE};
pub use records::to_records;

/// Sent with every request so site owners can see who is crawling and why.
pub const USER_AGENT: &str = concat!(
    "PlumbSearch/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/SueHeir/plumb-search)"
);

/// The product token matched against robots.txt `User-agent` lines.
pub const ROBOTS_TOKEN: &str = "PlumbSearch";

/// How [`crawl_homepages`] fetches pages.
#[derive(Debug, Clone)]
pub struct CrawlConfig {
    pub user_agent: String,
    /// Fetches in flight at once across all hosts.
    pub concurrency: usize,
    /// Host name lookups in flight at once. Home routers forward DNS for the
    /// whole house, and a burst of hundreds of lookups makes many of them
    /// fail ("temporary failure in name resolution"), which looks like the
    /// sites being down. Lookups beyond this wait their turn, so
    /// `concurrency` can stay high for slow sites without flooding the
    /// resolver. Zero counts as one.
    pub dns_lookups: usize,
    /// Pause between a host's answer and the next request to that host
    /// (robots.txt, then the page, then any redirects).
    pub per_host_delay: Duration,
    /// Timeout for each request, including reading the body.
    pub timeout: Duration,
    /// Bodies are cut off after this many bytes.
    pub max_bytes: usize,
    /// Redirects followed per request.
    pub max_redirects: usize,
    /// After a homepage, also fetch the site's icon for results pages (see
    /// [`CrawledPage::icon`]): at most [`MAX_ICONS`] icons the page links
    /// to and `/favicon.ico`, each allowed by robots.txt, until one reads
    /// as a bitmap.
    pub fetch_icons: bool,
    /// Also connect to host names that resolve to addresses off the public
    /// internet: loopback, private, link-local, CGNAT and other special
    /// ranges. Off by default, so a hostile domain whose DNS points at, say,
    /// 192.168.1.1 cannot make the crawler fetch pages on the operator's own
    /// network; such a target fails instead. Turn it on only to crawl an
    /// intranet on purpose. The check covers the names the crawler looks up
    /// itself, so neither hosts written as IP addresses nor targets reached
    /// through a proxy ([`CrawlConfig::use_system_proxy`]).
    pub allow_private_addresses: bool,
    /// Send requests through the system proxy, for machines that reach the
    /// internet only through one: the proxy named by the `HTTP_PROXY`,
    /// `HTTPS_PROXY` or `ALL_PROXY` environment variable, except for hosts
    /// listed in `NO_PROXY`. Off by default: the crawler then ignores those
    /// variables and connects to every site directly. With it on, the proxy
    /// looks up target host names itself, so the crawler never sees their
    /// addresses and [`CrawlConfig::allow_private_addresses`] cannot keep a
    /// hostile domain off the networks the proxy can reach. The proxy's own
    /// host name is still checked, so give a proxy on a private network as an
    /// IP address.
    pub use_system_proxy: bool,
    /// Counts the bytes of response bodies read (robots.txt files and
    /// pages, after decompression), so that a caller can keep track of
    /// the crawl's downloads. Clones of a config share the count.
    pub downloaded: Arc<AtomicU64>,
}

impl Default for CrawlConfig {
    fn default() -> Self {
        CrawlConfig {
            user_agent: USER_AGENT.to_string(),
            concurrency: 16,
            dns_lookups: 32,
            per_host_delay: Duration::from_secs(1),
            timeout: Duration::from_secs(15),
            max_bytes: 512 * 1024,
            max_redirects: 5,
            fetch_icons: true,
            allow_private_addresses: false,
            use_system_proxy: false,
            downloaded: Arc::default(),
        }
    }
}

/// A homepage to fetch. [`CrawlTarget::new`] makes one for a domain; set
/// [`known_url`](CrawlTarget::known_url) when the site is known to answer
/// at some URL.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlTarget {
    /// Registrable domain the results are filed under.
    pub domain: String,
    /// The URL to start at. Empty means `https://<domain>/`.
    #[serde(default)]
    pub url: String,
    /// A URL the site was reached at before (a record's `url`), tried when
    /// `url` cannot be reached at all; see [`crawl_homepages`]. Ignored
    /// unless it is on the same site as `url`: the same host, or one with
    /// the same registrable domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_url: Option<String>,
}

impl CrawlTarget {
    /// Starts at `https://<domain>/`, with no known URL.
    pub fn new(domain: &str) -> Self {
        CrawlTarget {
            domain: domain.to_string(),
            url: format!("https://{domain}/"),
            known_url: None,
        }
    }

    /// The same as [`CrawlTarget::new`].
    pub fn homepage(domain: &str) -> Self {
        Self::new(domain)
    }
}

/// What a page's HTML says about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageMeta {
    /// Text of the first `<title>`.
    pub title: Option<String>,
    /// `<meta name="description">`, else `og:description`.
    pub description: Option<String>,
    /// `og:site_name`.
    pub site_name: Option<String>,
    /// The site's search address, with `{searchTerms}` where the words go,
    /// from the first GET form on the page with a search box that submits
    /// to the same site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_url: Option<String>,
    /// The site's icons from `<link rel="icon">` and Apple touch icon
    /// links, best for a results page first, at most [`MAX_ICONS`]. SVG
    /// icons are left out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub icons: Vec<String>,
    /// Visible `<h1>` and `<h2>` texts, in page order, each once, at most
    /// [`plumb_core::MAX_HEADINGS`] and [`plumb_core::MAX_HEADING_WORDS`]
    /// words in all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headings: Vec<String>,
    /// The page's visible text in reading order, without scripts, menus,
    /// headers, footers, forms and headings, cut to [`MAX_BODY_WORDS`]
    /// words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_text: Option<String>,
    /// Links to other registrable domains, in page order.
    pub links: Vec<OutLink>,
}

/// A link from the page to another registrable domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutLink {
    /// Absolute URL.
    pub url: String,
    /// Registrable domain of `url`.
    pub target_domain: String,
    /// Normalized link text (may be empty).
    pub text: String,
}

/// A homepage that was fetched and parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawledPage {
    /// The target's registrable domain.
    pub domain: String,
    /// URL of the page after redirects.
    pub final_url: String,
    /// HTTP status of the final response (always 2xx).
    pub status: u16,
    /// Unix seconds.
    pub fetched_at: u64,
    pub meta: PageMeta,
    /// The site's icon as a [`ICON_SIZE`]-pixel square PNG, made by
    /// [`normalize_icon`], when [`CrawlConfig::fetch_icons`] is on and one
    /// was found. Left out of the JSON form.
    #[serde(skip)]
    pub icon: Option<Vec<u8>>,
}

/// What happened to one target. See [`crawl_homepages`] for when each
/// outcome is produced.
// A fetched page is the common outcome, so boxing it would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CrawlOutcome {
    Fetched(CrawledPage),
    /// The robots.txt of the homepage's origin, or of a same-site URL the
    /// homepage redirects to, does not allow fetching it. Nothing robots.txt
    /// disallows is ever requested.
    RobotsDisallowed,
    /// The homepage redirects to another registrable domain (`fb.com` ->
    /// `facebook.com`). `final_url` is where that redirect points (resolved,
    /// absolute); it is never requested, since that site's robots.txt has
    /// not been checked.
    OffsiteRedirect {
        final_url: String,
    },
    /// The final response was not a 2xx.
    HttpStatus {
        status: u16,
    },
    /// The final response was not HTML.
    NotHtml {
        content_type: String,
    },
    /// Not crawled because of an error. The message says what went wrong,
    /// and starts with `robots.txt` when that is where it went wrong.
    ///
    /// `network` is true when no usable answer came back at all: the host
    /// name does not resolve (or resolves only to non-public addresses, see
    /// [`CrawlConfig::allow_private_addresses`]), connecting or the TLS
    /// handshake failed, the request timed out, the connection broke off
    /// before the response was complete, or what came back was not HTTP.
    /// That says little about the site and is often temporary, so it is
    /// worth retrying sooner than other failures. Every fallback URL has
    /// been tried by then (see [`crawl_homepages`]).
    ///
    /// Otherwise `network` is false: robots.txt answered 5xx or 429 (RFC
    /// 9309 says not to crawl then), could not be parsed, or has rules that
    /// would take too much memory; there were more than `max_redirects`
    /// redirects; the target URL is not a usable http(s) URL; the body could
    /// not be decoded; or the HTTP client could not be built.
    Failed {
        error: String,
        #[serde(default)]
        network: bool,
    },
}

/// The outcome for one [`CrawlTarget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlResult {
    /// The target's registrable domain.
    pub domain: String,
    pub outcome: CrawlOutcome,
}
