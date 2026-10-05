//! A site's key pages ("sitelinks"): the few links on its homepage that
//! people most often want from it, such as signing in, docs or pricing.
//! Results pages list them under a site that was searched for by name, and
//! "paypal login" can go straight to PayPal's sign-in page.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{collapse_whitespace, normalize_text, registrable_domain};

/// Most key pages kept per site.
pub const MAX_KEY_PAGES: usize = 6;
/// Longest key page label kept, in characters.
pub const MAX_KEY_PAGE_LABEL_CHARS: usize = 30;
/// Most words in a key page label: longer link texts are sentences, not
/// the name of a page.
pub const MAX_KEY_PAGE_LABEL_WORDS: usize = 4;
/// Longest key page address kept, in bytes.
pub const MAX_KEY_PAGE_URL_BYTES: usize = 200;

/// One key page of a site: its link text on the homepage and where it goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPage {
    pub label: String,
    pub url: String,
}

impl KeyPage {
    /// Whether this is a fit key page for the site `domain`: a short label
    /// and an `http`/`https` address on the same site (subdomains count).
    pub fn is_valid_for(&self, domain: &str) -> bool {
        let label_ok = !self.label.trim().is_empty()
            && self.label.chars().count() <= MAX_KEY_PAGE_LABEL_CHARS
            && self.label.split_whitespace().count() <= MAX_KEY_PAGE_LABEL_WORDS;
        let lower = self.url.to_ascii_lowercase();
        let url_ok = self.url.len() <= MAX_KEY_PAGE_URL_BYTES
            && (lower.starts_with("https://") || lower.starts_with("http://"))
            && registrable_domain(&self.url).as_deref() == Some(domain);
        label_ok && url_ok
    }

    /// What the page is for, from its label.
    pub fn intent(&self) -> Option<PageIntent> {
        PageIntent::of_label(&self.label)
    }
}

/// Keeps the key pages fit for the site `domain` ([`KeyPage::is_valid_for`]),
/// each address once, at most [`MAX_KEY_PAGES`].
pub fn valid_key_pages(pages: Vec<KeyPage>, domain: &str) -> Vec<KeyPage> {
    let mut kept: Vec<KeyPage> = Vec::new();
    for page in pages {
        if kept.len() == MAX_KEY_PAGES {
            break;
        }
        if page.is_valid_for(domain) && !kept.iter().any(|k| k.url == page.url) {
            kept.push(page);
        }
    }
    kept
}

/// What someone wants from a site, as its links and searches say it. The
/// order is the order key pages are listed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PageIntent {
    Login,
    SignUp,
    Account,
    Docs,
    Pricing,
    Support,
    Download,
    Contact,
    Status,
    Store,
    Blog,
    Careers,
    About,
}

/// Each intent with the normalized phrases that name it.
const INTENT_PHRASES: &[(PageIntent, &[&str])] = &[
    (
        PageIntent::Login,
        &[
            "login",
            "log in",
            "logon",
            "log on",
            "signin",
            "sign in",
            "sign on",
            "member login",
        ],
    ),
    (
        PageIntent::SignUp,
        &[
            "sign up",
            "signup",
            "register",
            "create account",
            "create an account",
            "create new account",
            "join",
            "join now",
        ],
    ),
    (
        PageIntent::Account,
        &["account", "my account", "your account"],
    ),
    (
        PageIntent::Docs,
        &[
            "docs",
            "documentation",
            "developers",
            "developer",
            "developer docs",
            "api",
            "api docs",
            "api reference",
            "reference",
            "guides",
            "web docs",
        ],
    ),
    (
        PageIntent::Pricing,
        &[
            "pricing",
            "plans",
            "plans and pricing",
            "plans pricing",
            "prices",
        ],
    ),
    (
        PageIntent::Support,
        &[
            "help",
            "support",
            "help center",
            "help centre",
            "customer service",
            "customer support",
            "faq",
            "faqs",
        ],
    ),
    (
        PageIntent::Download,
        &["download", "downloads", "get the app", "install"],
    ),
    (PageIntent::Contact, &["contact", "contact us"]),
    (PageIntent::Status, &["status", "system status"]),
    (PageIntent::Store, &["store", "shop"]),
    (PageIntent::Blog, &["blog", "news", "newsroom"]),
    (PageIntent::Careers, &["careers", "jobs"]),
    (PageIntent::About, &["about", "about us"]),
];

impl PageIntent {
    /// The intent a link label names: one of its phrases, alone or going
    /// on with [`LABEL_GOES_ON`] ("Download for Mac", "Sign in to your
    /// account", but not "Account users").
    pub fn of_label(label: &str) -> Option<PageIntent> {
        let words = normalize_text(label);
        let words: Vec<&str> = words.split_whitespace().collect();
        if words.is_empty() || words.len() > MAX_KEY_PAGE_LABEL_WORDS + 2 {
            return None;
        }
        best_match(|phrase| {
            words.starts_with(phrase)
                && words
                    .get(phrase.len())
                    .is_none_or(|next| LABEL_GOES_ON.contains(next))
        })
    }

    /// Whether a host name or path holds one of the intent's phrases, as
    /// addresses spell them (`/login`, `/sign-in`, `docs.`, `/help-center`).
    fn in_address(self, part: &str) -> bool {
        let part = part.to_ascii_lowercase().replace(['-', '_'], "");
        INTENT_PHRASES
            .iter()
            .filter(|(intent, _)| *intent == self)
            .flat_map(|(_, phrases)| phrases.iter())
            .any(|phrase| part.contains(&phrase.replace(' ', "")))
    }

    /// The intent the end of a query names ("paypal login", "stripe api
    /// docs"), with how many of its words name it. The query must have
    /// other words before them.
    pub fn of_query_end(query: &str) -> Option<(PageIntent, usize)> {
        let words = normalize_text(query);
        let words: Vec<&str> = words.split_whitespace().collect();
        let ends = |phrase: &[&str]| phrase.len() < words.len() && words.ends_with(phrase);
        let intent = best_match(ends)?;
        let n = longest_phrase(intent, ends);
        Some((intent, n))
    }
}

/// Words after an intent's phrase that keep a label about that intent.
const LABEL_GOES_ON: &[&str] = &["to", "for", "now", "here", "free", "with", "and", "us"];

/// The intent whose longest phrase `matches`.
fn best_match(matches: impl Fn(&[&str]) -> bool) -> Option<PageIntent> {
    let mut best: Option<(PageIntent, usize)> = None;
    for (intent, phrases) in INTENT_PHRASES {
        for phrase in *phrases {
            let phrase: Vec<&str> = phrase.split(' ').collect();
            if matches(&phrase) && best.is_none_or(|(_, n)| phrase.len() > n) {
                best = Some((*intent, phrase.len()));
            }
        }
    }
    best.map(|(intent, _)| intent)
}

/// The most words of a phrase of `intent` that `matches`.
fn longest_phrase(intent: PageIntent, matches: impl Fn(&[&str]) -> bool) -> usize {
    INTENT_PHRASES
        .iter()
        .filter(|(i, _)| *i == intent)
        .flat_map(|(_, phrases)| phrases.iter())
        .map(|phrase| phrase.split(' ').collect::<Vec<_>>())
        .filter(|phrase| matches(phrase))
        .map(|phrase| phrase.len())
        .max()
        .unwrap_or(0)
}

/// A link from a homepage to its own site, as the crawler read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnLink {
    /// The link's text as shown, falling back to its `aria-label` or
    /// `title`.
    pub label: String,
    /// Absolute URL, without a fragment.
    pub url: String,
    /// Inside the page's `<nav>` or `<header>`: the site's main menu.
    pub in_nav: bool,
}

/// Menu entries that are not pages anyone searches for.
const NAV_NOISE: &[&str] = &[
    "home",
    "menu",
    "more",
    "search",
    "close",
    "open menu",
    "skip to content",
    "skip to main content",
    "main content",
    "next",
    "previous",
    "back",
    "english",
    "language",
    "all",
    "see all",
    "view all",
    "learn more",
    "read more",
    "logo",
    "cart",
    "bag",
];

/// The key pages of the homepage at `homepage` (its final URL), from the
/// links it makes to its own site, in page order: first one link for each
/// [`PageIntent`] the labels name, in that order, then the rest of the
/// site's main menu, up to [`MAX_KEY_PAGES`]. Links back to the homepage,
/// to other sites, and with long or empty labels are left out, except
/// that a homepage that is itself a sign-in form (`signs_in_here`, as
/// facebook.com is) gets a "Log in" key page to itself when it links to
/// no other sign-in page.
pub fn pick_key_pages(
    homepage: &str,
    domain: &str,
    links: &[OwnLink],
    signs_in_here: bool,
) -> Vec<KeyPage> {
    let host = |url: &Url| {
        let host = url.host_str().unwrap_or_default();
        host.strip_prefix("www.").unwrap_or(host).to_string()
    };
    let page = Url::parse(homepage).ok();
    let home = page.as_ref().map(host);
    // The homepage under another spelling: `www.` or not, `/en/`, `index.html`.
    let is_home = |url: &str| {
        let Ok(url) = Url::parse(url) else {
            return true;
        };
        let same_host = home.as_deref() == Some(host(&url).as_str());
        let same_path = page.as_ref().is_some_and(|page| page.path() == url.path());
        same_host && url.query().is_none() && (same_path || crate::is_homepage_path(url.path()))
    };
    let candidates: Vec<(KeyPage, Option<PageIntent>, bool)> = links
        .iter()
        .filter_map(|link| {
            // Arrows and bullets around the text ("Contact us ›") go.
            let label = collapse_whitespace(&link.label)
                .trim_matches(|c: char| !c.is_alphanumeric() && !"()?!".contains(c))
                .to_string();
            // "support" shows as "Support".
            let label = if label.chars().any(char::is_uppercase) {
                label
            } else {
                let mut chars = label.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().chain(chars).collect())
                    .unwrap_or_default()
            };
            let page = KeyPage {
                label,
                url: link.url.clone(),
            };
            (page.is_valid_for(domain) && !is_home(&page.url)).then(|| {
                let intent = page.intent();
                (page, intent, link.in_nav)
            })
        })
        .collect();
    let mut picked: Vec<KeyPage> = Vec::new();
    // For each intent, the link to the page most likely the site's own
    // for it: an address that says so (`/support`, not a menu toggle),
    // then the shortest (`/support`, not `/openstack/support`), then the
    // first on the page.
    let rank = |page: &KeyPage, intent: PageIntent| {
        let Ok(url) = Url::parse(&page.url) else {
            return (true, usize::MAX);
        };
        let depth = url.path().split('/').filter(|s| !s.is_empty()).count();
        let says =
            intent.in_address(url.host_str().unwrap_or_default()) || intent.in_address(url.path());
        (!says, depth)
    };
    let mut by_intent: Vec<(PageIntent, &KeyPage)> = Vec::new();
    for (page, intent, _) in &candidates {
        let Some(intent) = *intent else {
            continue;
        };
        match by_intent.iter_mut().find(|(i, _)| *i == intent) {
            Some(best) if rank(page, intent) < rank(best.1, intent) => best.1 = page,
            Some(_) => {}
            None => by_intent.push((intent, page)),
        }
    }
    let sign_in = KeyPage {
        label: "Log in".into(),
        url: homepage.to_string(),
    };
    let has_login = by_intent.iter().any(|(i, _)| *i == PageIntent::Login);
    if signs_in_here && !has_login && sign_in.is_valid_for(domain) {
        by_intent.push((PageIntent::Login, &sign_in));
    }
    by_intent.sort_by_key(|(intent, _)| *intent);
    let add = |page: &KeyPage, picked: &mut Vec<KeyPage>| {
        let label = normalize_text(&page.label);
        if picked.len() < MAX_KEY_PAGES
            && !picked
                .iter()
                .any(|p| p.url == page.url || normalize_text(&p.label) == label)
        {
            picked.push(page.clone());
        }
    };
    for (_, page) in by_intent {
        add(page, &mut picked);
    }
    for (page, intent, in_nav) in &candidates {
        let label = normalize_text(&page.label);
        let menu_entry = *in_nav
            && intent.is_none()
            && label.split_whitespace().count() <= 3
            && !NAV_NOISE.contains(&label.as_str());
        if menu_entry {
            add(page, &mut picked);
        }
    }
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(label: &str, url: &str, in_nav: bool) -> OwnLink {
        OwnLink {
            label: label.into(),
            url: url.into(),
            in_nav,
        }
    }

    fn labels(pages: &[KeyPage]) -> Vec<&str> {
        pages.iter().map(|p| p.label.as_str()).collect()
    }

    #[test]
    fn labels_name_intents() {
        for (label, intent) in [
            ("Log in", Some(PageIntent::Login)),
            ("SIGN IN", Some(PageIntent::Login)),
            ("Sign in to your account", Some(PageIntent::Login)),
            ("Sign up", Some(PageIntent::SignUp)),
            ("Docs", Some(PageIntent::Docs)),
            ("API reference", Some(PageIntent::Docs)),
            ("Plans & Pricing", Some(PageIntent::Pricing)),
            ("Help Center", Some(PageIntent::Support)),
            ("Download for Mac", Some(PageIntent::Download)),
            ("Contact us", Some(PageIntent::Contact)),
            ("Account users", None),
            ("Products", None),
            ("The best way to log in", None),
            ("", None),
        ] {
            assert_eq!(PageIntent::of_label(label), intent, "{label:?}");
        }
    }

    #[test]
    fn queries_end_in_intents() {
        assert_eq!(
            PageIntent::of_query_end("PayPal login"),
            Some((PageIntent::Login, 1))
        );
        assert_eq!(
            PageIntent::of_query_end("bank of america sign in"),
            Some((PageIntent::Login, 2))
        );
        assert_eq!(
            PageIntent::of_query_end("stripe api docs"),
            Some((PageIntent::Docs, 2))
        );
        assert_eq!(PageIntent::of_query_end("login"), None);
        assert_eq!(PageIntent::of_query_end("paypal"), None);
    }

    #[test]
    fn picks_one_page_per_intent_then_the_menu() {
        let links = [
            link("Home", "https://www.stripe.com/", true),
            link("Products", "https://stripe.com/products", true),
            link("Pricing", "https://stripe.com/pricing", true),
            link("Docs", "https://docs.stripe.com/", true),
            link("Sign in", "https://dashboard.stripe.com/login", true),
            link("Log in", "https://dashboard.stripe.com/login?2", false),
            link("Menu", "https://stripe.com/#menu", true),
            link("Customers", "https://stripe.com/customers", true),
            link(
                "Read the story of how we grew",
                "https://stripe.com/story",
                true,
            ),
            link("Support", "https://support.stripe.com/", false),
            link("Privacy", "https://stripe.com/privacy", false),
            link("Partner", "https://partner.example/", true),
            link("Careers", "https://stripe.com/jobs", false),
            link("Enterprise", "https://stripe.com/enterprise", true),
        ];
        let pages = pick_key_pages("https://stripe.com/", "stripe.com", &links, true);
        assert_eq!(
            labels(&pages),
            ["Sign in", "Docs", "Pricing", "Support", "Careers", "Products"]
        );
    }

    #[test]
    fn prefers_the_page_whose_address_names_the_intent() {
        let links = [
            link("Support", "https://ubuntu.com/navigation", true),
            link("Docs", "https://ubuntu.com/ceph/docs", true),
            link("Support", "https://ubuntu.com/openstack/support", false),
            link("Support", "https://ubuntu.com/support", false),
            link("Docs", "https://documentation.ubuntu.com/server/", false),
            link("Contact us ›", "https://ubuntu.com/contact-us", false),
        ];
        let pages = pick_key_pages("https://ubuntu.com/", "ubuntu.com", &links, false);
        let urls: Vec<&str> = pages.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://documentation.ubuntu.com/server/",
                "https://ubuntu.com/support",
                "https://ubuntu.com/contact-us",
            ]
        );
        assert_eq!(pages[2].label, "Contact us");
    }

    #[test]
    fn a_homepage_that_is_a_sign_in_form_gets_a_log_in_page() {
        let links = [
            link(
                "Forgotten password?",
                "https://www.facebook.com/recover/initiate/",
                false,
            ),
            link(
                "Create new account",
                "https://www.facebook.com/r.php",
                false,
            ),
            link(
                "Marketplace",
                "https://www.facebook.com/marketplace/",
                false,
            ),
            link("Help", "https://www.facebook.com/help/", false),
        ];
        let pages = pick_key_pages("https://www.facebook.com/", "facebook.com", &links, true);
        let pages: Vec<(&str, &str)> = pages
            .iter()
            .map(|p| (p.label.as_str(), p.url.as_str()))
            .collect();
        assert_eq!(
            pages,
            [
                ("Log in", "https://www.facebook.com/"),
                ("Create new account", "https://www.facebook.com/r.php"),
                ("Help", "https://www.facebook.com/help/"),
            ]
        );
    }

    #[test]
    fn leaves_out_the_homepage_and_bad_links() {
        let links = [
            link("Home", "https://example.com/", true),
            link("English", "https://example.com/en/", true),
            link("Mail", "mailto:hi@example.com", true),
            link("", "https://example.com/about", true),
            link("Evil", "javascript:alert(1)", true),
            link("Blog", "https://example.com/blog", false),
        ];
        let pages = pick_key_pages("https://example.com/", "example.com", &links, false);
        assert_eq!(labels(&pages), ["Blog"]);
    }

    #[test]
    fn validation_keeps_same_site_web_addresses() {
        let page = |label: &str, url: &str| KeyPage {
            label: label.into(),
            url: url.into(),
        };
        let kept = valid_key_pages(
            vec![
                page("Log in", "https://www.paypal.com/signin"),
                page("Log in again", "https://www.paypal.com/signin"),
                page("Phish", "https://paypal-login.us/"),
                page("Script", "javascript:alert(1)"),
                page(
                    "A label far too long to be the name of a page",
                    "https://paypal.com/x",
                ),
                page("Help", "http://help.paypal.com/"),
            ],
            "paypal.com",
        );
        assert_eq!(labels(&kept), ["Log in", "Help"]);
    }
}
