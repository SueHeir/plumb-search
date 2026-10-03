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

use std::time::Duration;

use serde::{Deserialize, Serialize};

mod crawl;
mod dns;
mod extract;
mod records;

pub use crawl::crawl_homepages;
pub use extract::{extract_page_meta, MAX_OUT_LINKS};
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
    /// Pause between two requests to the same host (robots.txt, then the page).
    pub per_host_delay: Duration,
    /// Timeout for each request, including reading the body.
    pub timeout: Duration,
    /// Bodies are cut off after this many bytes.
    pub max_bytes: usize,
    /// Redirects followed per request.
    pub max_redirects: usize,
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
}

impl Default for CrawlConfig {
    fn default() -> Self {
        CrawlConfig {
            user_agent: USER_AGENT.to_string(),
            concurrency: 16,
            per_host_delay: Duration::from_secs(1),
            timeout: Duration::from_secs(15),
            max_bytes: 512 * 1024,
            max_redirects: 5,
            allow_private_addresses: false,
            use_system_proxy: false,
        }
    }
}

/// A homepage to fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlTarget {
    /// Registrable domain the results are filed under.
    pub domain: String,
    /// Starting URL; robots.txt is fetched from the same origin.
    pub url: String,
}

impl CrawlTarget {
    /// `https://<domain>/`.
    pub fn homepage(domain: &str) -> Self {
        CrawlTarget {
            domain: domain.to_string(),
            url: format!("https://{domain}/"),
        }
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
}

/// What happened to one target. See [`crawl_homepages`] for when each
/// outcome is produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CrawlOutcome {
    Fetched(CrawledPage),
    /// robots.txt does not allow fetching the homepage (or the same-site
    /// page it redirects to).
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
    /// DNS, connection, TLS, timeout or similar. Also used when robots.txt
    /// answers 5xx or 429, cannot be fetched, or cannot be parsed: RFC 9309
    /// says not to crawl then, and the error message starts with
    /// `robots.txt`. Worth retrying later.
    Failed {
        error: String,
    },
}

/// The outcome for one [`CrawlTarget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlResult {
    /// The target's registrable domain.
    pub domain: String,
    pub outcome: CrawlOutcome,
}
