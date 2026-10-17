//! Shared types and helpers for Plumb Search.
//!
//! Plumb Search is a navigational ("names-only") search engine: for every
//! registrable domain it keeps the homepage title, the meta description,
//! the text other sites use when they link to it, a few aliases, and some
//! popularity signals. A site is kept as an item of Plumb's data format
//! ([`site`]), and a node's sites together are [`sites::Sites`].

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod article;
mod bot_check;
mod country;
pub mod docs;
pub mod facts;
pub mod films;
pub mod key_pages;
pub mod keys;
mod kinds;
mod language_words;
pub mod locks;
pub mod news;
#[cfg(feature = "oblivious")]
pub mod oblivious;
mod operators;
pub mod packages;
pub mod paper_query;
pub mod papers;
pub mod place;
pub mod profiles;
pub mod safe;
mod sign_in;
pub mod simhash;
pub mod site;
pub mod site_item;
mod site_search;
pub mod sites;
pub mod stack_exchange;
pub mod storage;
pub mod stored;
pub mod subsites;

pub use article::{article_url, Article};
pub use bot_check::{
    echoes_the_request, is_bot_check_page, is_sign_in_portal, is_stand_in_homepage,
};
pub use country::{
    country_of_any_name, country_of_name, normalize_country, tld_country, COUNTRY_CHOICES,
};
pub use key_pages::{KeyPage, PageIntent, MAX_KEY_PAGES};
pub use kinds::{is_generic_kind, kind_key, other_number, singular, MAX_KINDS};
pub use news::{Headline, RecentNews};
pub use operators::Operators;
pub use safe::{adult_level, AdultLevel, SafeSearch};
pub use sign_in::{asks_to_sign_in, is_sign_in_page, names_label, spells_label};
pub use site_search::{search_link, search_template_for, SEARCH_TERMS};
pub use subsites::{
    built_in_rules, parent_domain, subdomain_sites, use_site_rules, SITES_VERSION,
    SUBDOMAIN_SITE_NAMES,
};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// The network's own public site. Few sites link to it yet and no ranking
/// list has it, so every node keeps it ([`home_site`]) and crawls it,
/// rather than waiting for a link to bring it in.
pub const HOME_SITE: &str = "plumbsearch.org";

/// What [`HOME_SITE`] is called, and says of itself, until a crawl of it.
const HOME_SITE_NAME: &str = "Plumb Search";
const HOME_SITE_ABOUT: &str =
    "Free, open-source search engine run by a peer-to-peer network of nodes anyone can host.";

/// The site every node starts [`HOME_SITE`] with, as its stored item
/// ([`site`]), until a crawl of the site replaces its url, title and
/// description.
pub fn home_site() -> plumb_format::Item {
    let mut home = site::new(HOME_SITE);
    home.url = Some(format!("https://{HOME_SITE}/"));
    home.title = Some(HOME_SITE_NAME.into());
    home.about = Some(HOME_SITE_ABOUT.into());
    site::add_alias(&mut home, HOME_SITE_NAME);
    let mut signals = site::signals(&home);
    signals.official_site = true;
    site::set_signals(&mut home, &signals);
    home
}

/// Most inbound link texts kept per site (the most frequent ones win).
pub const MAX_LINK_TEXTS: usize = 32;
/// Most other sites kept per site ([`site::links_to`]).
pub const MAX_LINKS_TO: usize = 50;
/// Most aliases kept per site.
pub const MAX_ALIASES: usize = 16;
/// Most homepage headings kept per site.
pub const MAX_HEADINGS: usize = 8;
/// Most entries in a site's [`site::terms`].
pub const MAX_TERMS: usize = 100;
/// Most words kept from a homepage's headings, all together.
pub const MAX_HEADING_WORDS: usize = 60;
/// Longest title, description, alias or link text kept, in characters.
pub const MAX_TEXT_CHARS: usize = 300;

/// A homepage's redirect to another registrable domain ([`site::redirect`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// The registrable domain redirected to.
    pub to: String,
    /// Unix seconds of the crawl that saw it.
    pub at: u64,
    /// It went to a page of `to` other than its front page: a sign-in
    /// (`docs.google.com` to Google's), a bot check, or a page about the
    /// site (`destinythegame.com` to Bungie's page on Destiny). Older
    /// crawlers never set it, so their redirects fold as before.
    pub inner: bool,
}

/// Inbound link text and the sites that link with it ([`site::link_texts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkText {
    pub text: String,
    /// How many distinct sites link with this text, estimated from
    /// `linkers` ([`linker_count`]); a plain count when `linkers` is 0.
    pub count: u32,
    /// The linking sites as a set of 64 bits, one bit per site
    /// ([`linker_bit`]). Merging takes the union, so a site seen again, on
    /// another of its pages or in a later crawl, never counts twice.
    pub linkers: u64,
}

impl LinkText {
    /// Text used by the sites in `linkers` (see [`linker_bit`]).
    pub fn from_linkers(text: impl Into<String>, linkers: u64) -> Self {
        LinkText {
            text: text.into(),
            count: linker_count(linkers),
            linkers,
        }
    }

    /// Text with a plain count and no set of linking sites, for tests and
    /// hand-made data. Merging two of these keeps the larger count.
    pub fn with_count(text: impl Into<String>, count: u32) -> Self {
        LinkText {
            text: text.into(),
            count,
            linkers: 0,
        }
    }

    /// Adds the linking sites of `other`, which has the same text.
    fn absorb(&mut self, other: &LinkText) {
        let plain = |lt: &LinkText| if lt.linkers == 0 { lt.count } else { 0 };
        let plain = plain(self).max(plain(other));
        self.linkers |= other.linkers;
        self.count = linker_count(self.linkers).max(plain);
    }
}

/// Estimate [`linker_count`] gives once all 64 bits are set.
pub const MAX_LINKER_ESTIMATE: u32 = 300;

/// The bit that stands for the site `linking_domain` in [`LinkText::linkers`]:
/// one of 64, picked by a hash that is the same on every machine and in
/// every version.
pub fn linker_bit(linking_domain: &str) -> u64 {
    // FNV-1a, then the splitmix64 finalizer so the top bits are well mixed.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in linking_domain.as_bytes() {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    1 << (h >> 58)
}

/// How many distinct sites a [`LinkText::linkers`] set stands for, by linear
/// counting: exact for one site and nearly so for a handful (two sites share
/// a bit 1 time in 64), within about 10% up to 100, and
/// [`MAX_LINKER_ESTIMATE`] once every bit is set.
pub fn linker_count(linkers: u64) -> u32 {
    let set = linkers.count_ones();
    if set == 64 {
        return MAX_LINKER_ESTIMATE;
    }
    let bits = 64.0_f64;
    (bits * (bits / (bits - f64::from(set))).ln()).round() as u32
}

/// Popularity and trust signals ([`site::signals`]). Ranks are 1-based
/// (1 = best).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Signals {
    /// Position in Common Crawl's domain-level harmonic centrality ranking.
    pub harmonic_rank: Option<u64>,
    /// Position in Common Crawl's domain-level PageRank ranking.
    pub pagerank_rank: Option<u64>,
    /// Position in the Tranco list.
    pub tranco_rank: Option<u32>,
    /// Distinct other domains seen linking here (WAT files or our own crawls).
    pub linking_domains: u32,
    /// Listed as an official website in Wikidata.
    pub official_site: bool,
    /// For an official website, the most Wikipedia language editions (and
    /// other Wikimedia sites) with an article on an organization claiming
    /// it: how widely known the organization is.
    pub sitelinks: u32,
    /// Facts the site's pages state about Wikipedia's articles that were
    /// checked against Wikidata (`plumb-tools fact-trust`), each counted once.
    pub fact_checks: u32,
    /// How many of those were Wikidata's. See [`fact_accuracy`].
    pub fact_agrees: u32,
}

impl Signals {
    /// Of two counts of a site's checked facts, the one with less
    /// evidence for its accuracy: so one node alone cannot make a site
    /// look right.
    pub fn worse_facts(&self, other: &Signals) -> (u32, u32) {
        let mine = (self.fact_checks, self.fact_agrees);
        let theirs = (other.fact_checks, other.fact_agrees);
        match (fact_accuracy(self), fact_accuracy(other)) {
            (Some(a), Some(b)) => {
                if a <= b {
                    mine
                } else {
                    theirs
                }
            }
            _ => (0, 0),
        }
    }
}

fn sort_and_cap_link_texts(link_texts: &mut Vec<LinkText>) {
    link_texts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.text.cmp(&b.text)));
    link_texts.truncate(MAX_LINK_TEXTS);
}

fn min_some<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// How widely known an organization with `sitelinks` Wikipedia articles is,
/// from 0 to 1 on a log scale: 1 article 0.13, 25 articles 0.61, 200 or
/// more 1.
fn known_share(sitelinks: u32) -> f64 {
    const FULLY_KNOWN: f64 = 200.0;
    ((1.0 + f64::from(sitelinks)).ln() / (1.0 + FULLY_KNOWN).ln()).min(1.0)
}

/// A popularity prior in `0.0..=1.0` built from the site's signals.
///
/// Ranks map onto a log scale shared by every ranking (rank 1 is 1.0, rank
/// 1,000 about 0.63, rank 1,000,000 about 0.25, rank 100,000,000 is 0), and the best
/// of them counts for 75%. The number of linking domains, also on a log
/// scale, counts for the other 25%. An official website listed in Wikidata
/// gets a 0.15 bonus, plus up to 0.05 more the more widely known its
/// organization is ([`Signals::sitelinks`]). A site whose stated facts were
/// checked gains up to [`FACT_WEIGHT`] for getting them right, or loses as
/// much for getting them wrong ([`fact_trust`]). The result is in `0..=1`.
pub fn link_score(signals: &Signals) -> f32 {
    const RANK_SCALE: f64 = 1e8;
    const LINKS_SCALE: f64 = 1e5;
    fn from_rank(rank: u64) -> f64 {
        if rank == 0 {
            return 0.0;
        }
        (1.0 - (rank as f64).ln() / RANK_SCALE.ln()).clamp(0.0, 1.0)
    }
    let best_rank = [
        signals.tranco_rank.map(u64::from),
        signals.harmonic_rank,
        signals.pagerank_rank,
    ]
    .into_iter()
    .flatten()
    .map(from_rank)
    .fold(0.0, f64::max);
    let links = ((1.0 + signals.linking_domains as f64).ln() / (1.0 + LINKS_SCALE).ln()).min(1.0);
    let mut score = 0.75 * best_rank + 0.25 * links;
    if signals.official_site {
        score += 0.15 + 0.05 * known_share(signals.sitelinks);
    }
    score += FACT_WEIGHT * fact_trust(signals);
    score.clamp(0.0, 1.0) as f32
}

/// Fewest checked facts for a site's accuracy to count (the paper's 5).
pub const MIN_FACT_CHECKS: u32 = 5;

/// Share of checked facts a site states right before its own count is
/// known: about what sites get on average (97 to 99% in Common Crawl's
/// pages, where many of the rest are dates Wikidata and others dispute).
pub const FACT_PRIOR: f64 = 0.97;

/// An accuracy this low counts as fully wrong for [`fact_trust`]: half
/// the facts checked.
const FACT_WRONG: f64 = 0.5;

/// How many checks the prior weighs, so a few facts move a site little.
pub const FACT_PRIOR_CHECKS: f64 = 10.0;

/// Most a site's accuracy adds to (or takes off) its [`link_score`].
pub const FACT_WEIGHT: f64 = 0.1;

/// The share of a site's checked facts that it states right, pulled
/// toward [`FACT_PRIOR`] by [`FACT_PRIOR_CHECKS`] (Knowledge-Based Trust);
/// `None` with fewer than [`MIN_FACT_CHECKS`].
pub fn fact_accuracy(signals: &Signals) -> Option<f64> {
    if signals.fact_checks < MIN_FACT_CHECKS {
        return None;
    }
    let checks = f64::from(signals.fact_checks);
    let agrees = f64::from(signals.fact_agrees.min(signals.fact_checks));
    Some((agrees + FACT_PRIOR * FACT_PRIOR_CHECKS) / (checks + FACT_PRIOR_CHECKS))
}

/// How far a site's [`fact_accuracy`] is from the prior, from -1 (half
/// its facts wrong or worse, with many of them) to 1 (every fact right,
/// with many of them); 0 when too few were checked.
pub fn fact_trust(signals: &Signals) -> f64 {
    match fact_accuracy(signals) {
        None => 0.0,
        Some(accuracy) if accuracy >= FACT_PRIOR => (accuracy - FACT_PRIOR) / (1.0 - FACT_PRIOR),
        Some(accuracy) => ((accuracy - FACT_PRIOR) / (FACT_PRIOR - FACT_WRONG)).max(-1.0),
    }
}

/// The lowercase host of an http(s) URL or a bare hostname (optionally with
/// a port or path), without port or trailing dot. IDNs come back as
/// punycode. Returns `None` for other schemes (`mailto:`, `ftp://`), user
/// info without a scheme (`a@b.com`), IP addresses, and names that are not
/// valid DNS host names ([`is_valid_host`]).
pub fn host_of(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let parsed = if input.contains("://") {
        let parsed = url::Url::parse(input).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return None;
        }
        // User info names no site: `https://mailto:someone@gmail.com` is
        // an email address put where a website goes, not gmail.com's.
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return None;
        }
        parsed
    } else {
        // A bare host: anything before the first '/' other than an optional
        // `:port` would be a scheme (`mailto:`) or user info (`user@`).
        let authority = input.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.contains('@') {
            return None;
        }
        if let Some((_, port)) = authority.split_once(':') {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
        }
        url::Url::parse(&format!("http://{input}")).ok()?
    };
    match parsed.host()? {
        url::Host::Domain(host) => {
            let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
            is_valid_host(&host).then_some(host)
        }
        url::Host::Ipv4(_) | url::Host::Ipv6(_) => None,
    }
}

/// Whether `host` (lowercase ASCII, no trailing dot) is a usable DNS host
/// name: at most 253 bytes, at least two labels, each 1 to 63 bytes of
/// `a-z`, `0-9`, `-` or `_`, not starting or ending with `-`, and a
/// top-level label that is not all digits.
pub fn is_valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    let mut labels = 0;
    let mut last = "";
    for label in host.split('.') {
        let ok = (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return false;
        }
        labels += 1;
        last = label;
    }
    labels >= 2 && !last.bytes().all(|b| b.is_ascii_digit())
}

/// The site of a URL or hostname: its registrable domain ("eTLD+1"), using
/// the Public Suffix List, `https://www.usbank.com/x` -> `usbank.com`,
/// `news.bbc.co.uk` -> `bbc.co.uk`; or, for the few subdomains that are
/// sites of their own ([`subsites`]), that subdomain:
/// `https://news.ycombinator.com/item` -> `news.ycombinator.com`. Returns
/// `None` for IP addresses, bare public suffixes like `co.uk`, single-label
/// hosts like `localhost`, and everything [`host_of`] rejects. A node
/// may use other rules ([`use_site_rules`]).
pub fn registrable_domain(input: &str) -> Option<String> {
    subsites::rules().site_of(&host_of(input)?)
}

/// The site of a URL or hostname by the built-in rules, as
/// [`registrable_domain`] gives it while a node uses no other rules
/// ([`use_site_rules`]). For telling which sites other rules move.
pub fn built_in_site(input: &str) -> Option<String> {
    subsites::built_in().site_of(&host_of(input)?)
}

/// `links` made fit for [`site::links_to`] of `domain`: each made a
/// canonical domain, without `domain` itself, junk or repeats, in their
/// order, at most [`MAX_LINKS_TO`]. For links read off a homepage and for
/// links another node sends.
pub fn valid_links_to(links: Vec<String>, domain: &str) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for link in links {
        if kept.len() >= MAX_LINKS_TO {
            break;
        }
        let Some(link) = canonical_domain(&link) else {
            continue;
        };
        if link != domain && !kept.contains(&link) {
            kept.push(link);
        }
    }
    kept
}

/// The canonical spelling of a site's domain: the domain itself
/// when it already is a lowercase registrable domain (the common case,
/// checked without parsing), otherwise [`registrable_domain`] of it. So
/// `Example.com.` -> `example.com`, `münchen.de` -> `xn--mnchen-3ya.de`,
/// `www.example.com` -> `example.com`, and junk -> `None`.
pub fn canonical_domain(domain: &str) -> Option<String> {
    if is_valid_host(domain) && subsites::domain_of(domain) == Some(domain) {
        return Some(domain.to_string());
    }
    registrable_domain(domain)
}

/// True when a URL path points at a site's front page: empty, `/`,
/// `/index.<ext>` or `/default.<ext>`, optionally under one or two locale
/// segments such as `/en/`, `/us/en/` or `/pt-br/index.html`. Case and a
/// trailing slash don't matter. Query strings are the caller's call: pass
/// `Url::path()`, which excludes them.
///
/// Link text pointing at a front page names the site; link text pointing
/// deeper names the page (an article headline, a product), so Plumb only
/// collects the former.
pub fn is_homepage_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if path.ends_with('/') || segments.is_empty() {
        // A trailing slash means the last segment is a directory, not a file.
    } else if let Some(last) = segments.last() {
        let is_index_file = ["index.", "default."].iter().any(|prefix| {
            last.strip_prefix(prefix).is_some_and(|ext| {
                !ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
            })
        });
        if is_index_file {
            segments.pop();
        }
    }
    segments.len() <= 2 && segments.iter().all(|s| is_locale_segment(s))
}

/// `en`, `us`, `en-us`, `pt_br`, `zh-hans`: two letters, optionally a `-` or
/// `_` and two to four more. Lowercase only.
pub fn is_locale_segment(segment: &str) -> bool {
    let (lang, region) = match segment.split_once(['-', '_']) {
        Some((lang, region)) => (lang, Some(region)),
        None => (segment, None),
    };
    let letters = |s: &str| s.bytes().all(|b| b.is_ascii_lowercase());
    lang.len() == 2
        && letters(lang)
        && region.is_none_or(|r| (2..=4).contains(&r.len()) && letters(r))
}

/// Longest link text worth keeping, in characters after normalization.
pub const MAX_ANCHOR_CHARS: usize = 100;

/// Normalized link texts that say nothing about the site they point to.
pub const GENERIC_ANCHORS: &[&str] = &[
    "about",
    "about us",
    "back",
    "back to top",
    "click",
    "click here",
    "click to visit",
    "com",
    "contact",
    "contact us",
    "continue",
    "continue reading",
    "details",
    "download",
    "external link",
    "find out more",
    "full story",
    "go",
    "go to site",
    "go to website",
    "here",
    "home",
    "home page",
    "homepage",
    "http",
    "https",
    "info",
    "learn more",
    "link",
    "links",
    "main page",
    "more",
    "more info",
    "more information",
    "next",
    "official site",
    "official web site",
    "official website",
    "open",
    "previous",
    "read",
    "read more",
    "see more",
    "site",
    "source",
    "this",
    "this link",
    "top",
    "url",
    "view",
    "view more",
    "view site",
    "view website",
    "visit",
    "visit our website",
    "visit site",
    "visit the website",
    "visit website",
    "web",
    "web site",
    "website",
    "www",
];

/// Whether a link's normalized `text` says something about the site it
/// points to: not empty, at most [`MAX_ANCHOR_CHARS`], not one of the
/// [`GENERIC_ANCHORS`] ("click here", "official website"), and not just the
/// link's `href` or `target` URL spelled out.
pub fn is_useful_anchor(text: &str, href: &str, target: &url::Url) -> bool {
    !text.is_empty()
        && text.chars().count() <= MAX_ANCHOR_CHARS
        && !GENERIC_ANCHORS.contains(&text)
        && text != normalize_text(href)
        && text != normalize_text(target.as_str())
}

/// Turns Common Crawl's reversed host notation around: `com.example.www` -> `www.example.com`.
pub fn reverse_host(reversed: &str) -> String {
    let mut labels: Vec<&str> = reversed.trim().split('.').collect();
    labels.reverse();
    labels.join(".")
}

/// The part of a registrable domain before its public suffix:
/// `usbank.com` -> `usbank`, `bbc.co.uk` -> `bbc`.
pub fn domain_label(domain: &str) -> String {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    match subsites::suffix_of(&domain) {
        suffix if domain.len() > suffix.len() + 1 && domain.ends_with(suffix) => {
            domain[..domain.len() - suffix.len() - 1].to_string()
        }
        _ => domain,
    }
}

/// Normalizes text for matching: lowercase, letters and digits only, every
/// other run of characters becomes one space. Apostrophes and combining
/// marks are dropped (`McDonald's` -> `mcdonalds`, Thai `ข่าว` -> `ขาว`)
/// and runs of single-character tokens are joined, so `U.S. Bank` ->
/// `us bank` and `A.T.M.` -> `atm`. Text is read in Unicode NFC, as clean
/// sites hold it ([`site::clean`]), so a letter matches whether it
/// was typed whole or as a letter and a mark: `cafe\u{301}` is `café`.
pub fn normalize_text(text: &str) -> String {
    use unicode_categories::UnicodeCategories;
    use unicode_normalization::{is_nfc_quick, IsNormalized, UnicodeNormalization};
    let nfc: String;
    let text = if is_nfc_quick(text.chars()) == IsNormalized::Yes {
        text
    } else {
        nfc = text.nfc().collect();
        &nfc
    };
    // Dropping a mark or an apostrophe, lowercasing, or joining tokens can
    // leave letters that compose: each token is put in NFC again, so a
    // second pass changes nothing.
    let composed = |token: String| {
        if is_nfc_quick(token.chars()) == IsNormalized::Yes {
            token
        } else {
            token.nfc().collect()
        }
    };
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            // Only letters and digits of the lowercase form, so a second
            // pass changes nothing: `İ` lowercases to `i` plus a combining dot.
            current.extend(ch.to_lowercase().filter(|c| c.is_alphanumeric()));
        } else if matches!(ch, '\'' | '\u{2019}' | '\u{02BC}') || ch.is_mark() {
            // Apostrophes join the two halves of a word, and a mark is part
            // of its letter: a Thai tone mark or the virama of Hindi's `न्द`
            // is inside a word, not between two.
        } else if !current.is_empty() {
            tokens.push(composed(std::mem::take(&mut current)));
        }
    }
    if !current.is_empty() {
        tokens.push(composed(current));
    }

    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let mut j = i;
        while j < tokens.len() && tokens[j].chars().count() == 1 {
            j += 1;
        }
        if j - i >= 2 {
            out.push(composed(tokens[i..j].concat()));
            i = j;
        } else {
            out.push(std::mem::take(&mut tokens[i]));
            i += 1;
        }
    }
    out.join(" ")
}

/// [`normalize_text`] without the spaces: `U.S. Bank` -> `usbank`. Lets a
/// query like "us bank" match the domain label `usbank` and the other way round.
pub fn joined(text: &str) -> String {
    normalize_text(text).replace(' ', "")
}

/// Small words that join the words of a longer query ("pizza in denver",
/// "bank of america") and name nothing there. Normalized, as
/// [`normalize_text`] gives them.
pub const FUNCTION_WORDS: &[&str] = &[
    "a", "an", "and", "at", "by", "for", "from", "in", "into", "near", "of", "on", "or", "the",
    "to", "with",
];

/// Whether `word`, normalized, is one of [`FUNCTION_WORDS`].
pub fn is_function_word(word: &str) -> bool {
    FUNCTION_WORDS.contains(&word)
}

/// The primary language of a language tag, lowercase: `en-US` -> `en`,
/// `zh-Hant-TW` -> `zh`, `DE` -> `de`. `None` for tags that name no
/// language (`x-default`, `und`, `zxx`, `mul`) or are not tags at all.
pub fn language_code(tag: &str) -> Option<String> {
    let tag = tag.trim();
    if tag.len() > 63
        || tag.split(['-', '_']).any(|part| {
            part.is_empty() || part.len() > 8 || !part.bytes().all(|b| b.is_ascii_alphanumeric())
        })
    {
        return None;
    }
    let primary = tag.split(['-', '_']).next()?.to_ascii_lowercase();
    let letters = primary.bytes().all(|b| b.is_ascii_lowercase());
    let named = !matches!(primary.as_str(), "und" | "zxx" | "mul" | "mis");
    (letters && (2..=3).contains(&primary.len()) && named).then_some(primary)
}

/// Minimum alphabetic characters before using a script as fallback evidence.
const MIN_SCRIPT_LETTERS: usize = 6;

/// Scripts covered by this conservative filter, including secondary scripts
/// from Unicode CLDR languageData. This is not a language detector or a
/// complete language/script registry. Unlisted languages remain uncertain.
fn language_scripts(language: &str) -> Option<&'static [&'static str]> {
    Some(match language {
        "ru" | "uk" | "bg" | "mk" | "be" | "ky" | "tg" | "tt" | "ba" => &["Cyrl"],
        "sr" => &["Cyrl", "Latn"],
        "az" | "uz" | "ku" => &["Latn", "Cyrl", "Arab"],
        "kk" => &["Cyrl", "Arab"],
        "mn" => &["Cyrl", "Mong", "Phag"],
        "el" => &["Grek"],
        "ar" | "fa" | "ur" | "ps" | "ug" => &["Arab"],
        "sd" => &["Arab", "Deva", "Khoj", "Sind"],
        "ha" | "ms" => &["Latn", "Arab"],
        "he" | "yi" => &["Hebr"],
        "hi" | "mr" | "ne" | "sa" => &["Deva"],
        "bn" | "as" => &["Beng"],
        "ta" => &["Taml"],
        "te" => &["Telu"],
        "kn" => &["Knda"],
        "ml" => &["Mlym"],
        "gu" => &["Gujr"],
        "pa" => &["Guru", "Arab"],
        "si" => &["Sinh"],
        "th" => &["Thai"],
        "lo" => &["Laoo"],
        "km" => &["Khmr"],
        "my" => &["Mymr"],
        "ka" => &["Geor"],
        "hy" => &["Armn"],
        "am" | "ti" => &["Ethi"],
        "ko" => &["Hang", "Hani"],
        "ja" => &["Jpan", "Hani"],
        "zh" => &["Hani"],
        "en" | "de" | "fr" | "es" | "it" | "pt" | "nl" | "da" | "sv" | "no" | "nb" | "nn"
        | "fi" | "is" | "pl" | "cs" | "sk" | "sl" | "hr" | "ro" | "hu" | "tr" | "vi" | "id"
        | "sw" | "af" | "eu" | "ca" | "cy" | "ga" | "et" | "lv" | "lt" => &["Latn"],
        _ => return None,
    })
}

/// The approximate writing system of an alphabetic character. Block
/// ranges are deliberately limited; unsupported letters count toward
/// uncertainty, and digits and punctuation never count as letters.
fn char_script(c: char) -> Option<(&'static str, usize)> {
    use unicode_categories::UnicodeCategories;
    if !c.is_letter() {
        return None;
    }
    let script = match c as u32 {
        0x0041..=0x005A | 0x0061..=0x007A | 0x00C0..=0x024F | 0x1E00..=0x1EFF => "Latn",
        0x0370..=0x03FF | 0x1F00..=0x1FFF => "Grek",
        0x0400..=0x052F => "Cyrl",
        0x0530..=0x058F => "Armn",
        0x0590..=0x05FF => "Hebr",
        0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF => {
            "Arab"
        }
        0x0900..=0x097F => "Deva",
        0x0980..=0x09FF => "Beng",
        0x0A00..=0x0A7F => "Guru",
        0x0A80..=0x0AFF => "Gujr",
        0x0B80..=0x0BFF => "Taml",
        0x0C00..=0x0C7F => "Telu",
        0x0C80..=0x0CFF => "Knda",
        0x0D00..=0x0D7F => "Mlym",
        0x0D80..=0x0DFF => "Sinh",
        0x0E00..=0x0E7F => "Thai",
        0x0E80..=0x0EFF => "Laoo",
        0x1000..=0x109F => "Mymr",
        0x10A0..=0x10FF => "Geor",
        0x1200..=0x139F => "Ethi",
        0x1780..=0x17FF => "Khmr",
        0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => return Some(("Kana", 1)),
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF => return Some(("Hani", 1)),
        0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7AF => return Some(("Hang", 1)),
        _ if c.is_alphabetic() => "other",
        _ => return None,
    };
    Some((script, 1))
}

/// The writing system of at least two thirds of `text`'s letters, if not the
/// Latin alphabet: `Cyrl` for "Купить предметы | Магазин Rust", `Hani`
/// for Chinese, `Jpan` for Japanese (kanji with kana). `None` when most
/// letters are Latin, no one system has that share, or there are too
/// few. Lets a site that does not say its language still be told apart
/// from English by the script its title and description are in.
pub fn text_script(text: &str) -> Option<&'static str> {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    let mut total = 0;
    for (script, weight) in text.chars().filter_map(char_script) {
        total += weight;
        match counts.iter_mut().find(|(s, _)| *s == script) {
            Some((_, count)) => *count += weight,
            None => counts.push((script, weight)),
        }
    }
    let count = |script: &str| counts.iter().find(|(s, _)| *s == script).map_or(0, |c| c.1);
    // Kanji with any kana is Japanese; Chinese has no kana.
    let (kana, han) = (count("Kana"), count("Hani"));
    let (script, letters) = if kana > 0 {
        ("Jpan", kana + han)
    } else {
        counts
            .iter()
            .copied()
            .filter(|(s, _)| *s != "other")
            .max_by_key(|&(_, n)| n)?
    };
    (script != "Latn" && letters >= MIN_SCRIPT_LETTERS && letters * 3 >= total * 2)
        .then_some(script)
}

/// Whether a language filter is compatible with a declared language or
/// fallback script. A script can be shared by many languages; matching it
/// only keeps the candidate, it does not establish its language. Unlisted
/// languages and scripts fail open rather than excluding uncertain text.
pub fn language_fits(wanted: &str, site: &str) -> bool {
    if let Some(guessed) = site.strip_prefix(GUESSED) {
        return wanted == guessed || !language_words::tells(wanted);
    }
    if site.starts_with(|c: char| c.is_ascii_uppercase()) {
        if !matches!(
            site,
            "Cyrl"
                | "Grek"
                | "Arab"
                | "Hebr"
                | "Deva"
                | "Beng"
                | "Taml"
                | "Telu"
                | "Knda"
                | "Mlym"
                | "Gujr"
                | "Guru"
                | "Sinh"
                | "Thai"
                | "Laoo"
                | "Khmr"
                | "Mymr"
                | "Geor"
                | "Armn"
                | "Ethi"
                | "Hang"
                | "Jpan"
                | "Hani"
        ) {
            return true;
        }
        language_scripts(wanted).is_none_or(|scripts| scripts.contains(&site))
    } else {
        wanted == site
    }
}

/// Whether `query` has letters in a script `language` is not written in:
/// "天気予報", "погода москва" or "iphone 価格" with English chosen. Such
/// a search is for sites in that script, which a filter for `language`
/// would leave out. Latin letters never count, since names and codes are
/// typed in them whatever the language.
pub fn written_outside(language: &str, query: &str) -> bool {
    query.chars().filter_map(char_script).any(|(script, _)| {
        let script = if script == "Kana" { "Jpan" } else { script };
        !matches!(script, "Latn" | "other") && !language_fits(language, script)
    })
}

/// Marks a language [`site_language`] tells from a site's common words
/// rather than its declaration ("~de"). [`language_fits`] takes it as that
/// language, for the languages the words can tell; the shares of declared
/// languages a search's words are judged by leave it out, since their
/// limits were set on declarations alone.
pub const GUESSED: char = '~';

/// Whether at least half of `text`'s letters, and [`MIN_SCRIPT_LETTERS`]
/// or more, are in scripts `query` has no letter of: a title in Chinese or
/// Thai for a search typed in Latin letters, mixed with Latin words or not
/// ("ANIMG - 二次エロ画像"). A reader of the query is unlikely to read it.
/// Kana and kanji count as one, since Japanese mixes them.
pub fn written_apart(text: &str, query: &str) -> bool {
    let scripts = |text: &str| {
        text.chars()
            .filter_map(char_script)
            .map(|(script, _)| if script == "Kana" { "Hani" } else { script })
            .collect::<Vec<_>>()
    };
    let typed = scripts(query);
    let (mut apart, mut letters) = (0, 0);
    for script in scripts(text) {
        letters += 1;
        apart += usize::from(!typed.contains(&script));
    }
    letters >= MIN_SCRIPT_LETTERS && apart * 2 >= letters
}

/// Keep valid declared language metadata authoritative. Only missing or
/// invalid declarations fall back to dominant title/description script,
/// and then, for Latin letters, to the language their common words say
/// clearly ([`language_words::language_by_words`]), marked [`GUESSED`]:
/// seven sites in eight declare none, and a French or German one was
/// otherwise taken for English. Short excerpts and multilingual names do
/// not reliably contradict an explicit declaration; this intentionally
/// leaves mistagged pages alone.
pub fn site_language(declared: Option<&str>, text: &str) -> Option<String> {
    declared
        .and_then(language_code)
        .or_else(|| text_script(text).map(str::to_string))
        .or_else(|| language_words::language_by_words(text).map(|code| format!("{GUESSED}{code}")))
}

/// Collapses every whitespace run to one space and trims the ends.
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What windows-1252 has at bytes 0x80 to 0x9F, where Latin-1 has control
/// characters. The five it leaves undefined keep Latin-1's.
const WINDOWS_1252_HIGH: &str = "€\u{81}‚ƒ„…†‡ˆ‰Š‹Œ\u{8d}Ž\u{8f}\u{90}‘’“”•–—˜™š›œ\u{9d}žŸ";

/// `text` as the UTF-8 it was before something read it as windows-1252 or
/// Latin-1, as text copied from a page decoded in the wrong charset comes:
/// "MÃ¼nchen" is "München", "donâ€™t" is "don’t". Text with a character
/// those charsets lack, or whose bytes are not UTF-8, was not garbled so
/// and comes back as it is ("café", "東京").
pub fn unmangled(text: &str) -> Cow<'_, str> {
    let mut text = Cow::Borrowed(text);
    // Text garbled twice ("ÃƒÂ¼") is read back twice; each pass shortens it.
    while !text.is_ascii() {
        let bytes: Option<Vec<u8>> = text
            .chars()
            .map(|c| {
                u8::try_from(c).ok().or_else(|| {
                    let at = WINDOWS_1252_HIGH.chars().position(|high| high == c)?;
                    u8::try_from(0x80 + at).ok()
                })
            })
            .collect();
        match bytes.map(String::from_utf8) {
            Some(Ok(read)) => text = Cow::Owned(read),
            _ => break,
        }
    }
    text
}

/// An address as results pages show it: without `http://` or `https://`,
/// without the slash of a bare homepage, and cut to 100 characters.
pub fn display_url(href: &str) -> String {
    let shown = href
        .strip_prefix("https://")
        .or_else(|| href.strip_prefix("http://"))
        .unwrap_or(href);
    let shown = match shown.split_once('/') {
        Some((host, "")) => host,
        _ => shown,
    };
    truncate_chars(shown, 100)
}

/// How many colors [`site_initial`] picks from.
pub const SITE_INITIAL_COLORS: u8 = 8;

/// What a results page shows for a site that has no icon: the first
/// letter or digit of its host name after any `www.`, in capitals (`?` when
/// there is none), and a color number below [`SITE_INITIAL_COLORS`] that
/// stays the same for the site everywhere: on a node's pages and in the
/// browser's private search alike.
pub fn site_initial(domain: &str) -> (char, u8) {
    let host = domain.strip_prefix("www.").unwrap_or(domain);
    let letter = host
        .chars()
        .find(|c| c.is_alphanumeric())
        .and_then(|c| c.to_uppercase().next())
        .unwrap_or('?');
    // FNV-1a, so the color never changes between builds or machines.
    let mut hash: u32 = 0x811c_9dc5;
    for byte in domain.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    (letter, (hash % u32::from(SITE_INITIAL_COLORS)) as u8)
}

/// Cuts `text` to at most `max` characters (not bytes).
pub fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((idx, _)) => text[..idx].trim_end().to_string(),
        None => text.to_string(),
    }
}

/// Seconds since the Unix epoch.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reads one JSON value per line; blank lines are skipped.
pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut items = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let item = serde_json::from_str(line)
            .with_context(|| format!("{}:{}: invalid JSON line", path.display(), i + 1))?;
        items.push(item);
    }
    Ok(items)
}

/// Writes one JSON value per line, creating parent directories. Returns the count written.
pub fn write_jsonl<'a, T, I>(path: &Path, items: I) -> Result<usize>
where
    T: Serialize + 'a,
    I: IntoIterator<Item = &'a T>,
{
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    let mut count = 0;
    for item in items {
        serde_json::to_writer(&mut writer, item)?;
        writer.write_all(b"\n")?;
        count += 1;
    }
    writer
        .flush()
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_home_site_is_kept_until_a_crawl_of_it() {
        let line = stored::item_as_line(&home_site()).unwrap();
        assert_eq!(
            String::from_utf8(line).unwrap(),
            r#"{"id":"plumbsearch.org","kind":"site","title":"Plumb Search","url":"https://plumbsearch.org/","names":["Plumb Search"],"about":"Free, open-source search engine run by a peer-to-peer network of nodes anyone can host.","fields":{"wikidata":{"official_site":true}}}"#
        );
        let mut home = home_site();
        site::clean(&mut home);
        assert_eq!(home, home_site());
    }

    #[test]
    fn garbled_text_reads_as_the_utf8_it_was() {
        for (garbled, text) in [
            ("MÃ¼nchen", "München"),
            ("usembassy mÃ©xico", "usembassy méxico"),
            ("crÃ¨me brÃ»lÃ©e recipe", "crème brûlée recipe"),
            ("donâ€™t panic", "don’t panic"),
            // Read as Latin-1, 0x81 stays a control character.
            ("Ð¼Ð¾Ñ\u{81}ÐºÐ²Ð°", "москва"),
            ("ÃƒÂ¼ber", "über"),
        ] {
            assert_eq!(unmangled(garbled), text);
        }
        for text in ["café", "München", "東京", "Ã la carte", "plain", ""] {
            assert!(matches!(unmangled(text), Cow::Borrowed(same) if same == text));
        }
    }

    #[test]
    fn titles_tell_their_writing_system() {
        let script = |text: &str| text_script(text);
        // From the sites "cubecl rust" turned up with English chosen.
        assert_eq!(
            script("Burp Rust — донат и привилегии для Rust | Официальный магазин"),
            Some("Cyrl")
        );
        assert_eq!(script("Купить предметы | Магазин Rust"), Some("Cyrl"));
        assert_eq!(script("个人技术知识分享"), Some("Hani"));
        assert_eq!(script("東京の天気 - ウェザーニュース"), Some("Jpan"));
        assert_eq!(script("네이버 새로운 뉴스"), Some("Hang"));
        assert_eq!(script("الجزيرة نت - آخر الأخبار"), Some("Arab"));
        // Latin text, whatever its language, says nothing; nor do a few
        // words of another script in it, or too few letters.
        assert_eq!(script("Rust Programming Language"), None);
        assert_eq!(script("Nachrichten aus Österreich"), None);
        assert_eq!(script("Learn Greek | Ελληνικά lessons for beginners"), None);
        assert_eq!(
            script("GitHub 中文社区 - the Chinese GitHub community"),
            None
        );
        assert_eq!(script("Мир"), None);
        assert_eq!(script(""), None);

        assert!(language_fits("ru", "Cyrl"));
        assert!(language_fits("uk", "Cyrl"));
        assert!(!language_fits("en", "Cyrl"));
        assert!(language_fits("zh", "Hani"));
        assert!(language_fits("ja", "Hani"));
        assert!(language_fits("ja", "Jpan"));
        assert!(!language_fits("zh", "Jpan"));
        assert!(language_fits("de", "de"));
        assert!(!language_fits("en", "de"));

        let russian = "Купить предметы | Магазин Rust";
        assert_eq!(site_language(None, russian).as_deref(), Some("Cyrl"));
        // An excerpt cannot reliably contradict declared metadata.
        assert_eq!(site_language(Some("en"), russian).as_deref(), Some("en"));
        assert_eq!(site_language(Some("ru-RU"), russian).as_deref(), Some("ru"));
        assert_eq!(
            site_language(Some("ja"), "个人技术知识分享").as_deref(),
            Some("ja")
        );
        assert_eq!(
            site_language(Some("de"), "Bank Deutschland").as_deref(),
            Some("de")
        );
        assert_eq!(site_language(None, "Bank Deutschland"), None);
        // Undeclared Latin text falls back to what its common words say.
        let german = "Willkommen bei uns: wir bieten Reisen und Urlaub für alle";
        assert_eq!(site_language(None, german).as_deref(), Some("~de"));
        assert_eq!(site_language(Some("en"), german).as_deref(), Some("en"));
        assert!(language_fits("de", "~de"));
        assert!(!language_fits("en", "~de"));
        // A Danish site may be taken for German: unlisted languages keep it.
        assert!(language_fits("da", "~de"));
    }

    #[test]
    fn script_fallback_is_conservative_for_multilingual_text() {
        for text in [
            "研究所",        // Three Han characters are not six letters.
            "١٢٣٤٥٦٧٨٩٠،؛؟", // Arabic numbers and punctuation are not letters.
            "ًًًًِِِِ",              // Combining vowel marks alone are not letters.
            "ाााााााा",
            "東京 中文社区 English language research community",
            "Learn Ελληνικά with our English language lessons",
            "Олена Коваль — English language research and publications",
            "! ? 12345",
        ] {
            assert_eq!(text_script(text), None, "{text}");
        }
        for (language, text) in [
            ("az", "Азәрбајҹан елми арашдырмалар"),
            ("uz", "Ўзбекистон илмий тадқиқотлар"),
            ("kk", "قازاقستان غىلىمى زەرتتەۋ"),
            ("pa", "پنجابی علمی تحقیق"),
            ("ko", "韓國學術研究資料"),
            ("ja", "日本學術研究資料"),
        ] {
            let script = text_script(text).unwrap();
            assert!(language_fits(language, script), "{language} {script}");
            assert_eq!(
                site_language(Some(language), text).as_deref(),
                Some(language)
            );
        }
        assert!(language_fits("bo", "Hani"), "unlisted language fails open");
        assert!(
            language_fits("en", "Zzzz"),
            "unrecognized script fails open"
        );
        assert_eq!(
            site_language(Some("en-US"), "中國科學研究資料庫").as_deref(),
            Some("en")
        );
        assert_eq!(
            site_language(Some("invalid"), "中國科學研究資料庫").as_deref(),
            Some("Hani")
        );
        assert!(!language_fits("en", "Hani"));
        assert!(!language_fits("en", "ru"));
    }

    #[test]
    fn queries_in_another_script_than_the_language() {
        for query in [
            "天気予報",
            "東京タワー",
            "iphone 価格",
            "погода москва",
            "네이버",
            "الجزيرة",
            "मौसम",
        ] {
            assert!(written_outside("en", query), "{query}");
        }
        for (language, query) in [
            ("en", "weather tokyo"),
            ("en", "Nestlé café"),
            ("de", "münchen wetter"),
            ("ru", "погода москва"),
            ("ru", "weather"),
            ("ja", "天気予報"),
            ("ja", "東京タワー"),
            ("ko", "네이버 날씨"),
            ("ar", "الجزيرة"),
            ("bo", "བོད་"),
            ("en", "12345 !?"),
        ] {
            assert!(!written_outside(language, query), "{language} {query}");
        }
        // Kana is Japanese, not Chinese; Han alone fits either.
        assert!(written_outside("zh", "東京タワー"));
        assert!(!written_outside("zh", "天气预报"));
    }

    #[test]
    fn text_in_scripts_the_query_has_none_of() {
        for (text, query) in [
            ("和记娱乐 和记娱乐官网", "catalan numbers"),
            ("ANIMG - 二次エロ画像", "oeis a051023"),
            ("Vast MM | เครื่องจักร/ เครื่องมือช่าง", "rydberg constant"),
            ("Информационный портал района", "zara"),
        ] {
            assert!(written_apart(text, query), "{text}");
        }
        for (text, query) in [
            ("Tokyo Hotel 東京", "tokyo hotel"),
            ("Café de Flore", "cafe"),
            ("和记", "catalan numbers"),
            ("東京新聞のニュースサイト", "東京 ニュース"),
            ("東京拉麺店の公式サイト", "ラーメン"),
            ("", "news"),
        ] {
            assert!(!written_apart(text, query), "{text}");
        }
    }

    #[test]
    fn language_codes_from_tags() {
        assert_eq!(language_code("en-US").as_deref(), Some("en"));
        assert_eq!(language_code("zh_Hant_TW").as_deref(), Some("zh"));
        assert_eq!(language_code(" DE ").as_deref(), Some("de"));
        for tag in ["", "x-default", "und", "zxx", "e", "english", "12"] {
            assert_eq!(language_code(tag), None, "{tag}");
        }
        let mut mine = site::new("a.com");
        mine.lang = Some("en".into());
        site::set_crawled(&mut mine, Some(1));
        let mut newer = site::new("a.com");
        site::set_crawled(&mut newer, Some(2));
        site::merge(&mut mine, newer.clone());
        assert_eq!(mine.lang.as_deref(), Some("en"));
        newer.lang = Some("de".into());
        site::set_crawled(&mut newer, Some(3));
        site::merge(&mut mine, newer);
        assert_eq!(mine.lang.as_deref(), Some("de"));
    }

    #[test]
    fn addresses_are_shown_without_the_scheme() {
        assert_eq!(display_url("https://www.usbank.com/"), "www.usbank.com");
        assert_eq!(display_url("http://a.com/b/"), "a.com/b/");
        assert_eq!(display_url("https://a.com/?q=1"), "a.com/?q=1");
    }

    #[test]
    fn site_initials() {
        assert_eq!(site_initial("www.usbank.com").0, 'U');
        assert_eq!(site_initial("123movies.to").0, '1');
        assert_eq!(site_initial("éte.fr").0, 'É');
        assert_eq!(site_initial("").0, '?');
        assert_eq!(site_initial("example.com"), site_initial("example.com"));
        let colors: std::collections::HashSet<u8> = ["a.com", "b.com", "c.com", "d.com", "e.com"]
            .iter()
            .map(|d| site_initial(d).1)
            .collect();
        assert!(colors.len() > 1);
        assert!(colors.iter().all(|c| *c < SITE_INITIAL_COLORS));
    }

    #[test]
    fn registrable_domains() {
        assert_eq!(
            registrable_domain("https://www.usbank.com/home").as_deref(),
            Some("usbank.com")
        );
        assert_eq!(
            registrable_domain("news.bbc.co.uk").as_deref(),
            Some("bbc.co.uk")
        );
        assert_eq!(
            registrable_domain("WWW.Example.COM.").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            registrable_domain("http://example.com:8080/").as_deref(),
            Some("example.com")
        );
        assert_eq!(registrable_domain("co.uk"), None);
        assert_eq!(registrable_domain("localhost"), None);
        assert_eq!(registrable_domain("http://127.0.0.1/"), None);
        assert_eq!(registrable_domain(""), None);
        assert_eq!(
            registrable_domain("https://bücher.de/").as_deref(),
            Some("xn--bcher-kva.de")
        );
    }

    #[test]
    fn homepage_paths() {
        for path in [
            "",
            "/",
            "/index.html",
            "/INDEX.PHP",
            "/default.aspx",
            "/en",
            "/en/",
            "/us/en/",
            "/en-us/index.html",
            "/pt_BR/",
            "/zh-hans/",
        ] {
            assert!(is_homepage_path(path), "{path:?} should be a homepage");
        }
        for path in [
            "/about",
            "/index.",
            "/index.html/x",
            "/2026/10/03/story.html",
            "/en/about/",
            "/a/b/c/",
            "/english/",
            "/e1/",
            "/credit-cards",
        ] {
            assert!(!is_homepage_path(path), "{path:?} should not be a homepage");
        }
    }

    #[test]
    fn labels_and_reversed_hosts() {
        assert_eq!(domain_label("usbank.com"), "usbank");
        assert_eq!(domain_label("bbc.co.uk"), "bbc");
        assert_eq!(reverse_host("com.example.www"), "www.example.com");
        assert_eq!(reverse_host("com.usbank"), "usbank.com");
    }

    #[test]
    fn text_normalization() {
        assert_eq!(
            normalize_text("U.S. Bank | Personal Banking"),
            "us bank personal banking"
        );
        assert_eq!(normalize_text("McDonald's"), "mcdonalds");
        assert_eq!(normalize_text("  AT&T  Wireless "), "at t wireless");
        assert_eq!(normalize_text("Café Zürich"), "café zürich");
        assert_eq!(normalize_text("---"), "");
        // `İ` lowercases to `i` plus a combining dot; the dot is dropped so
        // normalizing twice gives the same text.
        assert_eq!(normalize_text("İstanbul Havalimanı"), "istanbul havalimanı");
        // Marks that are not letters stay inside their word: a Thai tone
        // mark, Hindi's virama.
        assert_eq!(normalize_text("รวมข่าว บันเทิง"), "รวมขาว บันเทิง");
        assert_eq!(normalize_text("हिन्दी समाचार"), "हिनदी समाचार");
        // A letter typed as a letter and a mark is the same letter typed
        // whole; Hindi's `ज़` is `ज` and a nukta either way.
        assert_eq!(normalize_text("cafe\u{301}s"), "cafés");
        assert_eq!(normalize_text("Du\u{31b}\u{323} ba\u{301}o"), "dự báo");
        assert_eq!(normalize_text("\u{95b}"), normalize_text("\u{91c}\u{93c}"));
        // Letters that compose only once a mark or an apostrophe between
        // them is dropped, or two tokens are joined.
        let composing = [
            "\u{386}\u{345}",
            "\u{3b1}'\u{345}",
            "\u{1100}'\u{1161}",
            "a \u{1100}'\u{1161} b",
            "\u{1100} \u{1161}",
        ];
        for text in ["İstanbul", "Ǆemal", "ﬁnance", "Straße", "ΣΊΣΥΦΟΣ", "İİ.İ"]
            .into_iter()
            .chain(composing)
        {
            let once = normalize_text(text);
            assert_eq!(normalize_text(&once), once, "{text:?}");
        }
        assert_eq!(joined("U.S. Bank"), "usbank");
        assert_eq!(joined("Bank of America"), "bankofamerica");
    }

    #[test]
    fn widely_known_official_sites_score_a_little_higher() {
        let official = |sitelinks| Signals {
            tranco_rank: Some(50_000),
            official_site: true,
            sitelinks,
            ..Signals::default()
        };
        let plain = link_score(&Signals {
            tranco_rank: Some(50_000),
            ..Signals::default()
        });
        let (unknown, few, many, most) = (
            link_score(&official(0)),
            link_score(&official(3)),
            link_score(&official(150)),
            link_score(&official(10_000)),
        );
        assert!((unknown - plain - 0.15).abs() < 1e-6);
        assert!(unknown < few && few < many && many < most);
        assert!((most - plain - 0.20).abs() < 1e-6);
    }

    #[test]
    fn links_to_are_canonical_distinct_and_capped() {
        let links = vec![
            "WWW.Example.com".into(),
            "a.com".into(),
            "example.com".into(),
            "localhost".into(),
            "news.bbc.co.uk".into(),
        ];
        assert_eq!(valid_links_to(links, "a.com"), ["example.com", "bbc.co.uk"]);
        let many: Vec<String> = (0..80).map(|i| format!("site{i}.com")).collect();
        let kept = valid_links_to(many, "a.com");
        assert_eq!(kept.len(), MAX_LINKS_TO);
        assert_eq!(kept[0], "site0.com");
    }

    #[test]
    fn linker_counts_are_close() {
        assert_eq!(linker_count(0), 0);
        assert_eq!(linker_count(1 << 7), 1);
        assert_eq!(linker_count(u64::MAX), MAX_LINKER_ESTIMATE);
        // The bit for a domain never changes between versions or machines.
        assert_eq!(linker_bit("example.com"), 1 << 19);
        assert_eq!(linker_bit("usbank.com"), 1 << 11);
        for n in [1_u32, 2, 5, 10, 30, 60, 100] {
            let bits = (0..n).fold(0, |bits, i| bits | linker_bit(&format!("site{i}.com")));
            let estimate = f64::from(linker_count(bits));
            let error = (estimate - f64::from(n)).abs() / f64::from(n);
            assert!(error <= 0.25, "{n} sites estimated as {estimate}");
        }
    }

    #[test]
    fn hosts_are_validated() {
        for bad in [
            "mailto:a@b.com",
            "a@b.com",
            "https://mailto:someone@gmail.com",
            "https://user:pass@example.com/",
            "ftp://example.com/",
            "javascript:alert(1)",
            "a..b.com",
            "x..com",
            ".com",
            "-bad.com",
            "bad-.com",
            "example.123",
            "example.com:http",
        ] {
            assert_eq!(host_of(bad), None, "{bad:?} should be rejected");
        }
        let long_label = format!("{}.com", "a".repeat(64));
        assert_eq!(host_of(&long_label), None);
        let long_name = format!("{}com", "abcdefghi.".repeat(26));
        assert!(long_name.len() > 253);
        assert_eq!(host_of(&long_name), None);
        assert_eq!(host_of(&"a".repeat(70_000)), None);

        assert_eq!(
            host_of("example.com:8080/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_of("HTTPS://Sub.Example.com./").as_deref(),
            Some("sub.example.com")
        );
        assert_eq!(
            host_of("my_host.example.com").as_deref(),
            Some("my_host.example.com")
        );
    }

    #[test]
    fn site_domains_are_canonical() {
        assert_eq!(
            canonical_domain("example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            canonical_domain("Example.COM.").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            canonical_domain("www.example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            canonical_domain("münchen.de").as_deref(),
            Some("xn--mnchen-3ya.de")
        );
        assert_eq!(canonical_domain("co.uk"), None);
        assert_eq!(canonical_domain("com..x"), None);

        let mut set = sites::Sites::new();
        let mut a = site::new("Example.com");
        let ranked = Signals {
            tranco_rank: Some(10),
            ..Signals::default()
        };
        site::set_signals(&mut a, &ranked);
        assert!(set.upsert(a));
        assert!(set.upsert(site::new("example.com")));
        assert!(set.upsert(site::new("münchen.de")));
        assert!(set.upsert(site::new("xn--mnchen-3ya.de")));
        assert!(!set.upsert(site::new("not a domain")));
        assert_eq!(set.len(), 2);
        let example = set.get("example.com").unwrap();
        assert_eq!(site::signals(&example).tranco_rank, Some(10));
        assert!(set.get("xn--mnchen-3ya.de").is_some());
    }

    #[test]
    fn useful_anchors() {
        let target = url::Url::parse("https://www.usbank.com/").unwrap();
        assert!(is_useful_anchor(
            "us bank",
            "https://www.usbank.com/",
            &target
        ));
        assert!(!is_useful_anchor("", "/", &target));
        assert!(!is_useful_anchor("click here", "/", &target));
        assert!(!is_useful_anchor("official website", "/", &target));
        assert!(!is_useful_anchor(
            "https www usbank com",
            "https://www.usbank.com/",
            &target
        ));
        assert!(!is_useful_anchor(&"word ".repeat(30), "/", &target));
        for text in GENERIC_ANCHORS {
            assert_eq!(&normalize_text(text), text);
        }
        let unique: std::collections::HashSet<&&str> = GENERIC_ANCHORS.iter().collect();
        assert_eq!(unique.len(), GENERIC_ANCHORS.len());
    }

    #[test]
    fn link_score_orders_sites() {
        let top = Signals {
            tranco_rank: Some(1),
            ..Default::default()
        };
        let mid = Signals {
            harmonic_rank: Some(1_000),
            ..Default::default()
        };
        let tail = Signals {
            harmonic_rank: Some(10_000_000),
            ..Default::default()
        };
        let none = Signals::default();
        assert!(link_score(&top) > link_score(&mid));
        assert!(link_score(&mid) > link_score(&tail));
        assert!(link_score(&tail) > link_score(&none));
        assert_eq!(link_score(&none), 0.0);
        let official = Signals {
            official_site: true,
            tranco_rank: Some(1),
            linking_domains: 1_000_000,
            ..Default::default()
        };
        assert_eq!(link_score(&official), 1.0);
    }

    #[test]
    fn facts_stated_right_raise_a_site_and_wrong_ones_lower_it() {
        let site = |checks, agrees| Signals {
            harmonic_rank: Some(100_000),
            fact_checks: checks,
            fact_agrees: agrees,
            ..Default::default()
        };
        let plain = link_score(&site(0, 0));
        // Too few to count.
        assert_eq!(link_score(&site(4, 0)), plain);
        assert_eq!(fact_accuracy(&site(4, 4)), None);
        let right = link_score(&site(50, 50));
        let some = link_score(&site(5, 5));
        let wrong = link_score(&site(40, 10));
        assert!(right > some && some > plain && plain > wrong);
        assert!(right - plain <= FACT_WEIGHT as f32 + 1e-6);
        assert!(plain - wrong <= FACT_WEIGHT as f32 + 1e-6);
        // A fact or two wrong among many costs little.
        assert!(plain - link_score(&site(100, 98)) < 0.01);
        assert!((-1.0..=1.0).contains(&fact_trust(&site(1_000_000, 0))));
        assert!((fact_trust(&site(1_000_000, 1_000_000)) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn jsonl_round_trip() {
        let dir = std::env::temp_dir().join(format!("plumb-core-test-{}", std::process::id()));
        let path = dir.join("records.jsonl");
        let mut bank = site::new("usbank.com");
        bank.title = Some("U.S. Bank".into());
        let sites = [bank, site::new("example.com")];
        let written = write_jsonl(&path, &sites).unwrap();
        assert_eq!(written, 2);
        let back: Vec<plumb_format::Item> = read_jsonl(&path).unwrap();
        assert_eq!(back, sites);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncation_is_char_safe() {
        assert_eq!(truncate_chars("héllo wörld", 4), "héll");
        assert_eq!(truncate_chars("short", 10), "short");
    }
}
