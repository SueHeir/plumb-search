//! Telling bot checks apart from homepages.
//!
//! Many sites sit behind a service that answers crawlers with a check
//! instead of the site: Cloudflare's "Just a moment...", DDoS-Guard,
//! Sucuri, Imperva's "Pardon Our Interruption", PerimeterX's "Access to
//! this page has been denied", and KillBot-style "verification" pages
//! (KillBot is also what phishing kits use to hide from scanners). Such a
//! page says nothing about the site, so its title, description and links
//! must not become the site's facts or travel to other nodes.
//!
//! The same goes for other pages that stand in for a homepage: an error or
//! maintenance page ("FedEx | System Down"), an identity check ("Verifica
//! tu identidad", "Client Challenge") or a parked domain's "this domain is
//! for sale".

use crate::{normalize_text, SiteRecord};

/// Whole titles, after [`normalize_text`], that only a bot check has.
const CHECK_TITLES: &[&str] = &[
    "403 forbidden",
    "access denied",
    "are you a robot",
    "are you human",
    "attention required",
    "bot check",
    "bot verification",
    "client challenge",
    "error",
    "browser check",
    "captcha",
    "captcha verification",
    "checking your browser",
    "ddos protection",
    "forbidden",
    "human check",
    "human verification",
    "just a moment",
    "one moment please",
    "one more step",
    "please verify you are a human",
    "please wait",
    "request blocked",
    "request rejected",
    "robot check",
    "robot or human",
    "security check",
    "security checkpoint",
    "security verification",
    "verification",
    "verification required",
    "verify you are human",
    "verifying",
    "verifying you are human",
    "you have been blocked",
    "not found",
    "page not found",
];

/// Title parts, after [`normalize_text`], of a page standing in for the
/// homepage, wherever they are in a title split at its separators: "FedEx |
/// System Down", "Walmart: Verifica tu identidad".
const STAND_IN_TITLE_PARTS: &[&str] = &[
    "404 not found",
    "500 internal server error",
    "502 bad gateway",
    "503 service unavailable",
    "access denied",
    "bad gateway",
    "client challenge",
    "down for maintenance",
    "internal server error",
    "page not found",
    "service unavailable",
    "site maintenance",
    "site unavailable",
    "system down",
    "temporarily unavailable",
    "under maintenance",
    "verifica tu identidad",
    "verify your identity",
    "website unavailable",
];

/// What a page title is split at into parts for [`STAND_IN_TITLE_PARTS`].
const TITLE_SEPARATORS: [char; 10] = ['|', '·', '•', ':', '–', '—', '»', '«', '/', '\\'];

/// Title beginnings, after [`normalize_text`], that only a bot check has
/// ("Just a moment...", "Checking your browser before accessing example.com",
/// "Bot Verification | Example Council").
const CHECK_TITLE_STARTS: &[&str] = &[
    "attention required",
    "bot verification",
    "checking your browser",
    "human verification",
    "just a moment",
    "one moment please",
    "please wait",
    "security check",
];

/// Phrases, after [`normalize_text`], that mark a bot check wherever they
/// appear in a title, description, heading or the page text.
const CHECK_PHRASES: &[&str] = &[
    "killbot",
    "killbot user verification",
    "ddos guard",
    "link11 captcha",
    "checking your browser",
    "attention required cloudflare",
    "just a moment cloudflare",
    "checking if the site connection is secure",
    "verifying you are human",
    "verify you are human by completing",
    "needs to review the security of your connection",
    "enable javascript and cookies to continue",
    "performance security by cloudflare",
    "ddos protection by cloudflare",
    "sucuri website firewall",
    "pardon our interruption",
    "access to this page has been denied",
    "vercel security checkpoint",
    "please complete the security check",
    "press hold to confirm you are a human",
    "this website is using a security service to protect itself",
    // Parked domains.
    "domain is for sale",
    "this domain may be for sale",
    "buy this domain",
];

/// Vendors whose own homepages talk about their checks.
const CHECK_VENDORS: &[&str] = &[
    "captcha",
    "cloudflare",
    "ddos-guard",
    "imperva",
    "killbot",
    "perimeterx",
    "plumbsearch",
    "sucuri",
    "vercel",
    // Domain marketplaces, whose homepages sell domains.
    "afternic",
    "dan.com",
    "godaddy",
    "hugedomains",
    "namecheap",
    "sedo",
];

/// Whether a page with this title, description, headings and text is a
/// bot check rather than the site at `domain`. A page that echoes the
/// request back ([`echoes_the_request`]) in its title or description, or
/// shows this crawler's User-Agent anywhere, always is; otherwise a
/// vendor's own site (a domain that names the vendor) never is. A
/// bracketed address further down is not enough: "Download version
/// [4.8.0.1]" reads as one.
pub fn is_bot_check_page(
    domain: &str,
    title: Option<&str>,
    description: Option<&str>,
    headings: &[String],
    body_text: Option<&str>,
) -> bool {
    let all = || {
        title
            .into_iter()
            .chain(description)
            .chain(headings.iter().map(String::as_str))
            .chain(body_text)
    };
    if domain != crate::HOME_SITE
        && (title.into_iter().chain(description).any(echoes_the_request)
            || all().any(shows_the_user_agent))
    {
        return true;
    }
    if CHECK_VENDORS.iter().any(|vendor| domain.contains(vendor)) {
        return false;
    }
    if let Some(title) = title.map(normalize_text) {
        let starts = |start: &&str| {
            title
                .strip_prefix(start)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
        };
        if CHECK_TITLES.contains(&title.as_str()) || CHECK_TITLE_STARTS.iter().any(starts) {
            return true;
        }
    }
    let stand_in = |part: &str| STAND_IN_TITLE_PARTS.contains(&normalize_text(part).as_str());
    if title.is_some_and(|title| title
            .split(TITLE_SEPARATORS)
            .flat_map(|part| part.split(" - "))
            .any(stand_in)) {
        return true;
    }
    all().any(|text| {
        let text = format!(" {} ", normalize_text(text));
        CHECK_PHRASES
            .iter()
            .any(|phrase| text.contains(&format!(" {phrase} ")))
    })
}

/// Whether `text` shows what the crawler sent rather than what the site
/// says: this crawler's User-Agent (`PlumbSearch/0.1.0 (+https://...)`) or
/// an IPv4 address in brackets, the visitor's address as KillBot's check
/// puts it ("KillBot user verification [203.0.113.7] [PlumbSearch/...]").
/// Such text would also publish the crawler's address.
pub fn echoes_the_request(text: &str) -> bool {
    if shows_the_user_agent(text) {
        return true;
    }
    text.split('[')
        .skip(1)
        .filter_map(|after| after.split_once(']'))
        .any(|(inside, _)| inside.trim().parse::<std::net::Ipv4Addr>().is_ok())
}

/// Whether `text` shows this crawler's User-Agent.
fn shows_the_user_agent(text: &str) -> bool {
    text.to_ascii_lowercase().contains("plumbsearch/")
}

impl SiteRecord {
    /// Whether the record's page fields came from a bot check
    /// ([`is_bot_check_page`]) rather than the site's homepage.
    pub fn is_bot_check(&self) -> bool {
        is_bot_check_page(
            &self.domain,
            self.title.as_deref(),
            self.description.as_deref(),
            &self.headings,
            self.body_text.as_deref(),
        )
    }

    /// Clears what a bot check put in the record: url, title, description,
    /// headings, page text, search box, key pages and `crawled_at`, so the
    /// record no longer counts as crawled. Merged into another record, it
    /// then leaves that record's page fields alone. Returns whether it did.
    pub fn drop_bot_check(&mut self) -> bool {
        if !self.is_bot_check() {
            return false;
        }
        self.url = None;
        self.title = None;
        self.description = None;
        self.headings.clear();
        self.body_text = None;
        self.search_url = None;
        self.key_pages.clear();
        self.links_to.clear();
        self.crawled_at = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(domain: &str, title: &str) -> bool {
        is_bot_check_page(domain, Some(title), None, &[], None)
    }

    #[test]
    fn known_checks_are_caught() {
        for title in [
            "Just a moment...",
            "Attention Required! | Cloudflare",
            "DDoS-Guard",
            "KillBot Verification",
            "Verifying you are human",
            "Robot or human?",
            "Pardon Our Interruption",
            "Access to this page has been denied",
            "Sucuri WebSite Firewall - Access Denied",
            "Vercel Security Checkpoint",
            "Human Verification",
            "Security Check",
            "KillBot user verification [172.98.218.140] [PlumbSearch/0.1.0 (+https://github.com/SueHeir/plumb-search)]",
            "Checking your browser before accessing anandtech.com",
            "Bot Verification | North Lincolnshire Council",
            "One moment, please...",
            "Captcha",
            "Link11 - CAPTCHA",
            "Access Denied",
            "Verification",
        ] {
            assert!(check("example.com", title), "{title}");
        }
    }

    #[test]
    fn error_identity_and_parked_pages_are_not_homepages() {
        for (domain, title) in [
            ("fedex.com", "FedEx | System Down"),
            ("walmart.com.mx", "Walmart - Verifica tu identidad"),
            ("statesman.com", "Client Challenge"),
            ("example.com", "Example: Under Maintenance"),
            ("example.com", "503 Service Unavailable"),
            ("example.com", "Error"),
        ] {
            assert!(check(domain, title), "{title}");
        }
        assert!(is_bot_check_page(
            "icloud.sm",
            Some("icloud.sm"),
            Some("This domain is for sale!"),
            &[],
            None,
        ));
        // A marketplace's own homepage, and ordinary titles, are not.
        assert!(!is_bot_check_page(
            "sedo.com",
            Some("Sedo: Buy and sell domains"),
            Some("Buy this domain or sell yours"),
            &[],
            None,
        ));
        for title in [
            "FedEx | Shipping, Tracking & Delivery",
            "Downdetector",
            "Errors in Medicine Journal",
            "Not Found Records",
        ] {
            assert!(!check("example.com", title), "{title}");
        }
    }

    #[test]
    fn echoed_requests_are_checks_even_on_vendor_sites() {
        for domain in ["killbot.ru", "kill-bot.net", "example.ru"] {
            assert!(check(
                domain,
                "KillBot user verification [23.92.78.91] [PlumbSearch/0.1.0 (+https://github.com/SueHeir/plumb-search)]"
            ));
        }
        assert!(is_bot_check_page(
            "example.com",
            Some("Welcome"),
            Some("Your address is [198.51.100.4]"),
            &[],
            None,
        ));
        assert!(!echoes_the_request("Array [1.5] and [x]"));
        assert!(!check(crate::HOME_SITE, "Plumb Search"));
    }

    #[test]
    fn page_text_catches_checks_with_plain_titles() {
        assert!(is_bot_check_page(
            "example.com",
            Some("example.com"),
            None,
            &[],
            Some("example.com needs to review the security of your connection before proceeding. Ray ID: 8c1f"),
        ));
        assert!(is_bot_check_page(
            "example.com",
            Some("Loading"),
            Some("Protected by KillBot"),
            &[],
            None,
        ));
        assert!(is_bot_check_page(
            "example.ru",
            Some("Your browser: PlumbSearch/0.1.0 (+https://github.com/SueHeir/plumb-search)"),
            None,
            &[],
            None,
        ));
    }

    #[test]
    fn version_numbers_in_page_text_are_not_echoed_addresses() {
        let headings = ["Download version [4.8.0.1]".to_string()];
        assert!(!is_bot_check_page(
            "example.org",
            Some("Example Tool"),
            Some("A tool for examples."),
            &headings,
            Some("Download version [4.8.0.1] now. Release notes [4.8.0.0]."),
        ));
        assert!(is_bot_check_page(
            "example.org",
            Some("Example Tool"),
            None,
            &[],
            Some("You are PlumbSearch/0.1.0 (+https://github.com/SueHeir/plumb-search)"),
        ));
    }

    #[test]
    fn real_homepages_are_kept() {
        for (domain, title) in [
            ("usbank.com", "Personal Banking | U.S. Bank"),
            (
                "cloudflare.com",
                "Connect, Protect, and Build Everywhere | Cloudflare",
            ),
            ("hcaptcha.com", "hCaptcha - captcha"),
            ("captcha.com", "Captcha - BotDetect CAPTCHA Generator"),
            ("ddos-guard.net", "DDoS-Guard: DDoS protection and CDN"),
            ("killbot.ru", "KillBot - anti-bot protection"),
            ("192-168-1-1-ip.co", "192.168.1.1 Admin Login"),
            ("github.com", "GitHub - SueHeir/plumb-search: Find websites"),
            ("accessdenied.org", "Access Denied Productions"),
            ("pleasewaitwhileweload.com", "Pleased to meet you"),
            (
                "security.org",
                "Security.org: Home Security, Safety and Privacy",
            ),
            ("verificationacademy.com", "Verification Academy"),
            ("waitbutwhy.com", "Wait But Why"),
        ] {
            assert!(!check(domain, title), "{title}");
        }
    }

    #[test]
    fn dropping_a_check_keeps_the_last_good_crawl() {
        let mut good = SiteRecord::new("example.com");
        good.title = Some("Example Shop".into());
        good.crawled_at = Some(100);
        let mut check = SiteRecord::new("example.com");
        check.url = Some("https://example.com/verify".into());
        check.title = Some("KillBot Verification".into());
        check.crawled_at = Some(200);
        assert!(check.drop_bot_check());
        good.merge(check);
        assert_eq!(good.title.as_deref(), Some("Example Shop"));
        assert_eq!(good.crawled_at, Some(100));

        let mut kept = good.clone();
        assert!(!kept.drop_bot_check());
        assert_eq!(kept, good);
    }
}
