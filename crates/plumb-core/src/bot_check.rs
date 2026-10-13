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
//! for sale" or bare "Hello!".

use crate::{canonical_domain, normalize_text, registrable_domain, SiteRecord};

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
    "captcha check",
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
    "checking",
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
    "domain for sale",
    "domain parked",
    "down for maintenance",
    "internal server error",
    "page not found",
    "parked domain",
    "server busy",
    "service unavailable",
    "site maintenance",
    "site unavailable",
    "system down",
    "temporarily unavailable",
    "under maintenance",
    // Canva's answer to a browser it does not know: "Unsupported client –
    // Canva" on canva.com.
    "unsupported client",
    "verifica tu identidad",
    "verify your identity",
    "website suspended",
    "website unavailable",
];

/// Whole titles, after [`normalize_text`], of a placeholder where a
/// homepage should be: a web server's default page or a suspended hosting
/// account. A site not built yet is [`NOT_BUILT_ENDS`].
const PLACEHOLDER_TITLES: &[&str] = &[
    "caddy works",
    "welcome to nginx",
    "welcome to nginx on debian",
    "welcome to centos",
    "apache2 ubuntu default page it works",
    "apache2 debian default page it works",
    "test page for the apache http server",
    "iis windows server",
    "it works",
    "account suspended",
    "this account has been suspended",
    "suspended domain",
    "default web site page",
    "web server s default page",
];

/// Title endings, after [`normalize_text`], of a blog host's page for a
/// domain no one has a blog on yet: "anime-japan.net | 忍者ブログ - 忍者ツールズ".
const UNUSED_DOMAIN_TITLE_ENDS: &[&str] = &["忍者ブログ 忍者ツールズ"];

/// How a title part, after [`normalize_text`], of a site not built yet
/// ends, after nothing but [`NOT_BUILT_WORDS`]: "Coming Soon", "This Web
/// site coming soon", "Nobel — New website coming soon".
const NOT_BUILT_ENDS: &[&str] = &["coming soon", "under construction"];

/// Words that may come before a [`NOT_BUILT_ENDS`] in a title part.
const NOT_BUILT_WORDS: &[&str] = &[
    "new", "our", "page", "site", "the", "this", "web", "website",
];

/// Words, after [`normalize_text`], of a homepage that only greets: "Hello!",
/// "hello hello", ":: HELLO ::", "Hello world". Parked domains show such
/// pages by the thousand, and a query with "hello" in it found them by
/// their one word and by meaning.
const GREETING_WORDS: &[&str] = &["hello", "hi", "hey", "hallo", "hola", "welcome", "world"];

/// What a page title is split at into parts for [`STAND_IN_TITLE_PARTS`].
const TITLE_SEPARATORS: [char; 10] = ['|', '·', '•', ':', '–', '—', '»', '«', '/', '\\'];

/// Title beginnings, after [`normalize_text`], that only a bot check or a
/// page standing in for the homepage has ("Just a moment...", "Checking
/// your browser before accessing example.com", "Bot Verification | Example
/// Council", "Redirecting Request. Please wait.", a defacer's "Hacked By
/// …").
const CHECK_TITLE_STARTS: &[&str] = &[
    "attention required",
    "bot verification",
    "checking your browser",
    "hacked by",
    "human verification",
    "just a moment",
    "one moment please",
    "please wait",
    "redirecting request",
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
    // A server's directory listing: "Index of /".
    if title.is_some_and(|title| title.trim_start().to_lowercase().starts_with("index of /")) {
        return true;
    }
    if let Some(title) = title.map(normalize_text) {
        let starts = |start: &&str| {
            title
                .strip_prefix(start)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
        };
        let ends = |end: &&str| {
            title
                .strip_suffix(end)
                .is_some_and(|rest| rest.is_empty() || rest.ends_with(' '))
        };
        if CHECK_TITLES.contains(&title.as_str())
            || PLACEHOLDER_TITLES.contains(&title.as_str())
            || CHECK_TITLE_STARTS.iter().any(starts)
            || UNUSED_DOMAIN_TITLE_ENDS.iter().any(ends)
        {
            return true;
        }
    }
    let stand_in = |part: &str| {
        let part = normalize_text(part);
        STAND_IN_TITLE_PARTS.contains(&part.as_str()) || not_built_yet(&part)
    };
    if title.is_some_and(|title| {
        title
            .split(TITLE_SEPARATORS)
            .flat_map(|part| part.split(" - "))
            .any(stand_in)
    }) {
        return true;
    }
    all().any(|text| {
        let text = format!(" {} ", normalize_text(text));
        CHECK_PHRASES
            .iter()
            .any(|phrase| text.contains(&format!(" {phrase} ")))
    })
}

/// Whether a normalized title part says the site is not built yet
/// ([`NOT_BUILT_ENDS`]).
fn not_built_yet(part: &str) -> bool {
    NOT_BUILT_ENDS.iter().any(|end| {
        part.strip_suffix(end).is_some_and(|rest| {
            (rest.is_empty() || rest.ends_with(' '))
                && rest
                    .split_whitespace()
                    .all(|word| NOT_BUILT_WORDS.contains(&word))
        })
    })
}

/// Whether a crawled homepage stands in for the site: a bot check or
/// placeholder ([`is_bot_check_page`]), or a page that only greets
/// ([`only_greets`]). A page someone asks to read is what it is, so the
/// page reader asks [`is_bot_check_page`] alone.
pub fn is_stand_in_homepage(
    domain: &str,
    title: Option<&str>,
    description: Option<&str>,
    headings: &[String],
    body_text: Option<&str>,
) -> bool {
    is_bot_check_page(domain, title, description, headings, body_text)
        || only_greets(title, description, headings, body_text)
}

/// Whether a homepage's whole text greets and says nothing else
/// ([`GREETING_WORDS`]). Judged only with the page's text to hand: a crawl
/// passed on as facts has its title alone, and a band's site titled
/// "Hello" says more below it. "World" alone is not a greeting.
fn only_greets(
    title: Option<&str>,
    description: Option<&str>,
    headings: &[String],
    body_text: Option<&str>,
) -> bool {
    if headings.is_empty() && body_text.is_none_or(|text| text.trim().is_empty()) {
        return false;
    }
    let texts = title
        .into_iter()
        .chain(description)
        .chain(headings.iter().map(String::as_str))
        .chain(body_text);
    let mut greets = false;
    for text in texts {
        for word in normalize_text(text).split_whitespace() {
            if !GREETING_WORDS.contains(&word) {
                return false;
            }
            greets |= word != "world";
        }
    }
    greets
}

/// Whole titles, after [`normalize_text`], of sign-in pages that
/// companies run for themselves with someone else's product: Outlook on
/// the web, webmail, VPN and remote-desktop gateways. Such a homepage is
/// the company's door, not a page about the product.
const PORTAL_TITLES: &[&str] = &[
    "outlook",
    "outlook web app",
    "outlook web access",
    "outlook sign in",
    "sign in to outlook",
    "microsoft exchange",
    "exchange admin center",
    "owa",
    "roundcube webmail",
    "roundcube webmail login",
    "squirrelmail",
    "horde",
    "horde login",
    "zimbra web client sign in",
    "zimbra",
    "webmail",
    "webmail login",
    "citrix gateway",
    "netscaler gateway",
    "netscaler aaa",
    "citrix storefront",
    "pulse connect secure",
    "ivanti connect secure",
    "globalprotect portal",
    "sslvpn portal",
    "fortinet ssl vpn",
    "remote desktop web access",
    "rd web access",
    "vmware horizon",
    "cpanel login",
    "whm login",
    "plesk login",
    "sign in",
    "log in",
    "login",
    "user login",
    "member login",
];

/// Whether a homepage titled `title` is a sign-in page for a product the
/// site at `domain` only uses ([`PORTAL_TITLES`]): "Outlook Web App" on
/// bpl.net. The product's own site (outlook.com, citrix.com) is not.
pub fn is_sign_in_portal(domain: &str, title: &str) -> bool {
    let title = normalize_text(title);
    if !PORTAL_TITLES.contains(&title.as_str()) {
        return false;
    }
    let label = domain.split('.').next().unwrap_or("");
    let product = title.split(' ').next().unwrap_or("");
    let generic = ["sign", "log", "login", "user", "member", "webmail"];
    generic.contains(&product) || !label.contains(product)
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
    /// Whether the record's page fields came from a bot check or another
    /// page standing in for the site's homepage ([`is_stand_in_homepage`]).
    ///
    /// So is a homepage read on another site's host: prevention.nih.gov's
    /// "NIH Office of Disease Prevention" kept for nih.gov, or
    /// play.google.com's page for google.com, from crawls before those
    /// subdomains were sites of their own ([`crate::subsites`]); crawlers
    /// now stop at such a redirect. And so is a hosting company's page for
    /// a suspended account (cPanel's `/cgi-sys/suspendedpage.cgi`, titled
    /// "Contact Support").
    pub fn is_bot_check(&self) -> bool {
        if let Some(url) = self.url.as_deref() {
            let read_elsewhere = registrable_domain(url).is_some_and(|site| {
                canonical_domain(&self.domain).is_some_and(|domain| site != domain)
            });
            let suspended = url
                .to_ascii_lowercase()
                .contains("/cgi-sys/suspendedpage.cgi");
            if read_elsewhere || suspended {
                return true;
            }
        }
        is_stand_in_homepage(
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
        self.terms.clear();
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
            "Captcha Check",
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
            ("amazon.com.au", "Server Busy"),
            ("canva.com", "Unsupported client – Canva"),
            ("oilpatchsurplus.com", "Website Suspended"),
            ("grsaccess.com", "Redirecting Request. Please wait."),
            ("kireie.com", "Checking..."),
            (
                "the-brooklyn-star.com",
                "Hacked By Peyman Siyahi & M@raz Ali",
            ),
            ("labubu.st", "Domain Parked"),
            (
                "charlotterampling.net",
                "Domain for Sale – charlotterampling.net | Brandable & Ready to Use",
            ),
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
            "Checking Accounts | Example Bank",
            "Hacked: a podcast about hackers",
            "Website suspended for a week: what we learned",
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

    #[test]
    fn placeholders_are_not_homepages() {
        for title in [
            "Welcome to nginx!",
            "Coming Soon",
            "endgame.info — Coming Soon",
            "This Web site coming soon",
            "Nobel — New website coming soon",
            "Under Construction",
            "Account Suspended",
            "Index of /",
            "Apache2 Ubuntu Default Page: It works",
            "Caddy works!",
            "anime-japan.net | 忍者ブログ - 忍者ツールズ",
        ] {
            assert!(check("example.com", title), "{title}");
        }
        for title in [
            "Gladiator II | Coming Soon to Theaters",
            "Odeon | Films Now Showing and Coming Soon",
            "Caddy - The Ultimate Server with Automatic HTTPS",
            "忍者ツールズ｜ホームページ ブログ アクセス解析 カウンター メールフォーム",
            "アニメ日記 | 忍者ブログ",
            "Nginx: Advanced Load Balancer",
            "Index Ventures",
        ] {
            assert!(!check("example.com", title), "{title}");
        }
        // Parked pages that only greet, whatever field the greeting is in.
        let page = |title: Option<&str>, description: Option<&str>, body: Option<&str>| {
            is_stand_in_homepage("example.com", title, description, &[], body)
        };
        assert!(page(None, None, Some("hello")));
        assert!(page(None, None, Some("hello hello")));
        assert!(page(Some(":: HELLO ::"), None, Some(":: HELLO ::")));
        assert!(page(Some("Hello!"), Some("Hello!"), Some("Hello")));
        assert!(page(Some("Hello World"), None, Some("Hello, world!")));
        // A greeting with anything more is a homepage.
        assert!(!page(Some("Hello"), Some("English band"), Some("Hello")));
        assert!(!page(Some("Hello"), None, Some("Hello, we sell bikes")));
        assert!(!page(Some("Hello Kitty"), None, Some("Hello")));
        assert!(!page(None, None, Some("World")));
        // A title alone, as a crawl passed on as facts has it, is not enough.
        assert!(!page(Some("Hello!"), None, None));
        // Nor is it a bot check for a page someone asks to read.
        assert!(!is_bot_check_page(
            "example.com",
            Some("Hello World"),
            None,
            &[],
            Some("Hello, world!")
        ));
    }

    #[test]
    fn pages_of_another_site_or_a_suspended_account_are_dropped() {
        let record = |domain: &str, url: &str| {
            let mut record = SiteRecord::new(domain);
            record.url = Some(url.into());
            record.title = Some("Some Title".into());
            record.crawled_at = Some(100);
            record
        };
        let mut nih = record("nih.gov", "https://prevention.nih.gov/");
        assert!(nih.drop_bot_check());
        assert_eq!(nih.title, None);
        assert!(record("google.com", "https://play.google.com/store/games").is_bot_check());
        assert!(record(
            "example.com",
            "https://example.com/cgi-sys/suspendedpage.cgi"
        )
        .is_bot_check());
        for (domain, url) in [
            ("nih.gov", "https://www.nih.gov/"),
            ("usbank.com", "https://www.usbank.com/index.html"),
            ("prevention.nih.gov", "https://prevention.nih.gov/"),
            ("squareup.com", "https://squareup.com/us/en"),
        ] {
            assert!(!record(domain, url).is_bot_check(), "{url}");
        }
    }

    #[test]
    fn sign_in_portals_of_other_products_are_told_apart() {
        assert!(is_sign_in_portal("bpl.net", "Outlook Web App"));
        assert!(is_sign_in_portal("se-coop.com", "Outlook"));
        assert!(is_sign_in_portal("example.org", "Login"));
        assert!(!is_sign_in_portal("outlook.com", "Outlook"));
        assert!(!is_sign_in_portal("citrix.com", "Citrix Gateway"));
        assert!(!is_sign_in_portal("bpl.net", "Brooklyn Public Library"));
    }
}
