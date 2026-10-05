//! Shared types and helpers for Plumb Search.
//!
//! Plumb Search is a navigational ("names-only") search engine: for every
//! registrable domain it keeps the homepage title, the meta description,
//! the text other sites use when they link to it, a few aliases, and some
//! popularity signals. Every other crate passes [`SiteRecord`]s around.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub mod article;
mod bot_check;
mod country;
pub mod key_pages;
pub mod keys;
mod kinds;
#[cfg(feature = "oblivious")]
pub mod oblivious;
mod operators;
pub mod safe;
mod site_search;

pub use article::{article_url, Article};
pub use bot_check::{echoes_the_request, is_bot_check_page};
pub use country::{normalize_country, site_country, tld_country};
pub use key_pages::{KeyPage, PageIntent, MAX_KEY_PAGES};
pub use kinds::{is_generic_kind, kind_key, other_number, MAX_KINDS};
pub use operators::Operators;
pub use safe::{adult_level, record_adult_level, AdultLevel, SafeSearch};
pub use site_search::{search_link, search_template_for, SEARCH_TERMS};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The network's own public site. Few sites link to it yet and no ranking
/// list has it, so every node keeps a record of it ([`home_site_record`])
/// and crawls it, rather than waiting for a link to bring it in.
pub const HOME_SITE: &str = "plumbsearch.org";

/// The record every node starts [`HOME_SITE`] with, until a crawl of the
/// site replaces its url, title and description.
pub fn home_site_record() -> SiteRecord {
    let mut record = SiteRecord::new(HOME_SITE);
    record.url = Some(format!("https://{HOME_SITE}/"));
    record.title = Some("Plumb Search".into());
    record.description = Some(
        "Free, open-source search engine run by a peer-to-peer network of nodes anyone can host."
            .into(),
    );
    record.add_alias("Plumb Search");
    record.signals.official_site = true;
    record
}

/// Most inbound link texts kept per site (the most frequent ones win).
pub const MAX_LINK_TEXTS: usize = 32;
/// Most aliases kept per site.
pub const MAX_ALIASES: usize = 16;
/// Most homepage headings kept per site.
pub const MAX_HEADINGS: usize = 8;
/// Most words kept from a homepage's headings, all together.
pub const MAX_HEADING_WORDS: usize = 60;
/// Longest title, description, alias or link text kept, in characters.
pub const MAX_TEXT_CHARS: usize = 300;

/// One site in the index, keyed by its registrable domain.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SiteRecord {
    /// Registrable domain, lowercase ASCII (punycode for IDNs), e.g. `usbank.com`.
    pub domain: String,
    /// Homepage URL after redirects, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Homepage `<title>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Homepage meta description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The homepage's visible `<h1>` and `<h2>` texts, in page order, at
    /// most [`MAX_HEADINGS`] and [`MAX_HEADING_WORDS`] words in all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headings: Vec<String>,
    /// The start of the homepage's visible text, leaving out menus,
    /// headers, footers and headings, at most 100 words. Not searched by
    /// its words; it goes into the site's embedding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_text: Option<String>,
    /// Normalized text of links from other sites, most frequent first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub link_texts: Vec<LinkText>,
    /// Other names for the site, e.g. a Wikidata label or `og:site_name`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub signals: Signals,
    /// Unix seconds of the last successful homepage crawl.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crawled_at: Option<u64>,
    /// Unix seconds of the last homepage fetch attempt, successful or not, so
    /// sites that keep failing are not retried on every crawl.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crawl_attempted_at: Option<u64>,
    /// Country the site belongs to, as an ISO 3166-1 alpha-2 code (`US`),
    /// from Wikidata's country of the organization whose official website
    /// this is. When missing, [`site_country`] falls back to the domain
    /// ending (`.fr` -> `FR`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// What kind of thing the site's organization is, from Wikidata
    /// ("bank", "airline"), at most [`MAX_KINDS`]. See [`SiteRecord::add_kind`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    /// What the site's organization is, in a few words, from Wikidata's
    /// English description ("American bank holding company").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// The first sentences of the English Wikipedia article about the
    /// site's organization, for well-known sites ("GitHub is a proprietary
    /// developer platform that ...").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intro: Option<String>,
    /// The site's own search address, with `{searchTerms}` where the words
    /// go (see [`search_link`]), read from a search form on its homepage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_url: Option<String>,
    /// The language the homepage says it is in (`<html lang>`), as a
    /// lowercase primary language code ([`language_code`]): `en`, `de`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The site's key pages ("sitelinks": sign in, docs, pricing), from
    /// the links its homepage makes to the site itself, at most
    /// [`MAX_KEY_PAGES`]. See [`key_pages::pick_key_pages`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_pages: Vec<KeyPage>,
    /// Homepage fetch attempts in a row, up to the one at
    /// `crawl_attempted_at`, that could not reach the site at all (no
    /// connection or no answer); 0 once an attempt gets an answer. Crawlers
    /// retry such sites sooner than ones that answered, waiting longer after
    /// each failure.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub crawl_failures: u32,
    /// Where the homepage sent the crawler instead, when it redirects to
    /// another site (`pncbank.com` -> `pnc.com`), as of the last crawl. Such
    /// a site is the other one under another name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<Redirect>,
    /// The site's icon as a small PNG, base64, only while a crawl travels
    /// between nodes (a node's own crawl results to the network, a trusted
    /// crawler's to this node). Never kept in a records file: a node keeps
    /// icons in a store of their own, and [`SiteRecord::merge`] ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// A homepage's redirect to another registrable domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redirect {
    /// The registrable domain redirected to.
    pub to: String,
    /// Unix seconds of the crawl that saw it.
    pub at: u64,
}

/// Inbound link text and the sites that link with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkText {
    pub text: String,
    /// How many distinct sites link with this text, estimated from
    /// `linkers` ([`linker_count`]); a plain count when `linkers` is 0.
    pub count: u32,
    /// The linking sites as a set of 64 bits, one bit per site
    /// ([`linker_bit`]). Merging takes the union, so a site seen again, on
    /// another of its pages or in a later crawl, never counts twice.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
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

/// Popularity and trust signals. Ranks are 1-based (1 = best).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Signals {
    /// Position in Common Crawl's domain-level harmonic centrality ranking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harmonic_rank: Option<u64>,
    /// Position in Common Crawl's domain-level PageRank ranking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pagerank_rank: Option<u64>,
    /// Position in the Tranco list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tranco_rank: Option<u32>,
    /// Distinct other domains seen linking here (WAT files or our own crawls).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub linking_domains: u32,
    /// Listed as an official website in Wikidata.
    #[serde(default, skip_serializing_if = "is_false")]
    pub official_site: bool,
    /// For an official website, the most Wikipedia language editions (and
    /// other Wikimedia sites) with an article on an organization claiming
    /// it: how widely known the organization is.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub sitelinks: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl SiteRecord {
    pub fn new(domain: impl Into<String>) -> Self {
        SiteRecord {
            domain: domain.into(),
            ..Default::default()
        }
    }

    /// Adds a link to this site with `text` from the site `linking_domain`
    /// (its registrable domain). Each linking site counts once per text,
    /// however often it is added. See [`SiteRecord::add_link_text_linkers`].
    pub fn add_link_text(&mut self, text: &str, linking_domain: &str) {
        self.add_link_text_linkers(text, linker_bit(linking_domain));
    }

    /// Adds link text used by a set of linking sites, their [`linker_bit`]s
    /// OR-ed together. The text is normalized first; empty text or an empty
    /// set is ignored. Keeps the list sorted and capped at [`MAX_LINK_TEXTS`].
    pub fn add_link_text_linkers(&mut self, text: &str, linkers: u64) {
        let text = truncate_chars(&normalize_text(text), MAX_TEXT_CHARS);
        if text.is_empty() || linkers == 0 {
            return;
        }
        let added = LinkText::from_linkers(text, linkers);
        match self.link_texts.iter_mut().find(|lt| lt.text == added.text) {
            Some(lt) => lt.absorb(&added),
            None => self.link_texts.push(added),
        }
        sort_and_cap_link_texts(&mut self.link_texts);
    }

    /// Adds an alias unless one with the same normalized form is already there.
    pub fn add_alias(&mut self, alias: &str) {
        let alias = truncate_chars(&collapse_whitespace(alias), MAX_TEXT_CHARS);
        let key = normalize_text(&alias);
        if key.is_empty() || self.aliases.len() >= MAX_ALIASES {
            return;
        }
        if self.aliases.iter().any(|a| normalize_text(a) == key) {
            return;
        }
        self.aliases.push(alias);
    }

    /// Adds a kind ("bank") unless it is generic ([`is_generic_kind`]), the
    /// same kind is already there (plurals and case aside), or the site has
    /// [`MAX_KINDS`] already.
    pub fn add_kind(&mut self, kind: &str) {
        let kind = truncate_chars(&collapse_whitespace(kind), MAX_TEXT_CHARS);
        if self.kinds.len() >= MAX_KINDS || is_generic_kind(&kind) {
            return;
        }
        let key = kind_key(&kind);
        if self.kinds.iter().any(|k| kind_key(k) == key) {
            return;
        }
        self.kinds.push(kind);
    }

    /// Folds another record for the same domain into this one.
    ///
    /// Page fields (url, title, description, search_url) come from whichever
    /// record was crawled more recently, and missing ones are filled from the
    /// other (except `search_url`: a newer crawl without a search form
    /// clears it). The first record's `country` wins, and kinds are unioned.
    /// Link texts take the union of their linking sites (a site seen by both
    /// counts once), aliases are unioned, ranks keep the best
    /// (lowest) value, `linking_domains` keeps the larger count (sources often
    /// overlap, so adding would double count), `official_site` is OR-ed, and
    /// `crawl_attempted_at` keeps the later time, with the `crawl_failures`
    /// counted at that attempt (the larger count when both tried at the same
    /// time). A redirect stays only when no successful crawl came after it.
    /// [`SiteRecord::merge`] for a crawl another node shared. Shared crawls
    /// carry only some of what a crawl finds (no search box, and page text
    /// only from trusted crawlers), so a field the shared crawl leaves empty
    /// keeps what this node has rather than being cleared.
    pub fn merge_shared(&mut self, other: SiteRecord) {
        let search_url = other.search_url.is_none().then(|| self.search_url.take());
        let headings = other
            .headings
            .is_empty()
            .then(|| std::mem::take(&mut self.headings));
        let body_text = other.body_text.is_none().then(|| self.body_text.take());
        let key_pages = other
            .key_pages
            .is_empty()
            .then(|| std::mem::take(&mut self.key_pages));
        self.merge(other);
        if let Some(mine) = search_url {
            self.search_url = self.search_url.take().or(mine);
        }
        if let Some(mine) = headings {
            if self.headings.is_empty() {
                self.headings = mine;
            }
        }
        if let Some(mine) = body_text {
            self.body_text = self.body_text.take().or(mine);
        }
        if let Some(mine) = key_pages {
            if self.key_pages.is_empty() {
                self.key_pages = mine;
            }
        }
    }

    pub fn merge(&mut self, other: SiteRecord) {
        debug_assert_eq!(self.domain, other.domain);
        let other_is_fresher = other.crawled_at.is_some() && other.crawled_at >= self.crawled_at;
        if other_is_fresher {
            if other.url.is_some() {
                self.url = other.url;
            }
            if other.title.is_some() {
                self.title = other.title;
            }
            if other.description.is_some() {
                self.description = other.description;
            }
            if other.language.is_some() {
                self.language = other.language;
            }
            // A fresh crawl that found no search form, or no headings,
            // means the site has none now.
            self.search_url = other.search_url;
            self.headings = other.headings;
            self.body_text = other.body_text;
            self.key_pages = other.key_pages;
            self.crawled_at = other.crawled_at;
        } else {
            self.url = self.url.take().or(other.url);
            self.title = self.title.take().or(other.title);
            self.description = self.description.take().or(other.description);
            self.language = self.language.take().or(other.language);
            if self.crawled_at.is_none() {
                self.search_url = self.search_url.take().or(other.search_url);
                if self.headings.is_empty() {
                    self.headings = other.headings;
                }
                self.body_text = self.body_text.take().or(other.body_text);
                if self.key_pages.is_empty() {
                    self.key_pages = other.key_pages;
                }
            }
        }
        // The latest crawl decides: a redirect seen after the last
        // successful crawl stands, a successful crawl after it ends it.
        self.redirect = match (self.redirect.take(), other.redirect) {
            (Some(mine), Some(theirs)) => Some(if theirs.at >= mine.at { theirs } else { mine }),
            (mine, theirs) => mine.or(theirs),
        };
        if let (Some(redirect), Some(crawled_at)) = (&self.redirect, self.crawled_at) {
            if crawled_at > redirect.at {
                self.redirect = None;
            }
        }
        self.country = self.country.take().or(other.country);
        self.about = self.about.take().or(other.about);
        self.intro = self.intro.take().or(other.intro);
        for kind in &other.kinds {
            self.add_kind(kind);
        }
        for lt in other.link_texts {
            match self.link_texts.iter_mut().find(|mine| mine.text == lt.text) {
                Some(mine) => mine.absorb(&lt),
                None => self.link_texts.push(lt),
            }
        }
        sort_and_cap_link_texts(&mut self.link_texts);
        for alias in &other.aliases {
            self.add_alias(alias);
        }
        let s = &mut self.signals;
        let o = other.signals;
        s.harmonic_rank = min_some(s.harmonic_rank, o.harmonic_rank);
        s.pagerank_rank = min_some(s.pagerank_rank, o.pagerank_rank);
        s.tranco_rank = min_some(s.tranco_rank, o.tranco_rank);
        s.linking_domains = s.linking_domains.max(o.linking_domains);
        s.official_site |= o.official_site;
        s.sitelinks = s.sitelinks.max(o.sitelinks);
        match other.crawl_attempted_at.cmp(&self.crawl_attempted_at) {
            std::cmp::Ordering::Greater => self.crawl_failures = other.crawl_failures,
            std::cmp::Ordering::Equal => {
                self.crawl_failures = self.crawl_failures.max(other.crawl_failures);
            }
            std::cmp::Ordering::Less => {}
        }
        self.crawl_attempted_at = self.crawl_attempted_at.max(other.crawl_attempted_at);
    }

    /// Popularity prior for this site, see [`link_score`].
    pub fn link_score(&self) -> f32 {
        link_score(&self.signals)
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

/// Records keyed by domain; adding a record for a domain already present merges them.
///
/// Each record is boxed: a [`SiteRecord`] is several hundred bytes even
/// when nearly empty, and a hash table keeps room for about twice its
/// entries, so a million sites held inline would take a gigabyte of table
/// alone, and twice that while it grows.
#[derive(Debug, Clone, Default)]
pub struct RecordSet {
    map: HashMap<String, Box<SiteRecord>>,
}

impl RecordSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, domain: &str) -> Option<&SiteRecord> {
        self.map.get(domain).map(|record| &**record)
    }

    /// The record for `domain`, created empty if it is not there yet.
    /// `domain` must already be canonical, as [`registrable_domain`] returns it.
    pub fn entry(&mut self, domain: &str) -> &mut SiteRecord {
        self.map
            .entry(domain.to_string())
            .or_insert_with(|| Box::new(SiteRecord::new(domain)))
    }

    /// Inserts a record, merging it into an existing one for the same domain.
    /// The domain is made canonical first ([`canonical_domain`]), so
    /// `Example.COM` and `münchen.de` land on `example.com` and
    /// `xn--mnchen-3ya.de`, and page fields read off a bot check are dropped
    /// ([`SiteRecord::drop_bot_check`]). Returns false, dropping the record, when the
    /// domain is not a valid registrable domain.
    pub fn upsert(&mut self, record: SiteRecord) -> bool {
        self.upsert_with(record, SiteRecord::merge)
    }

    /// [`RecordSet::upsert`] for a crawl another node shared, merged with
    /// [`SiteRecord::merge_shared`].
    pub fn upsert_shared(&mut self, record: SiteRecord) -> bool {
        self.upsert_with(record, SiteRecord::merge_shared)
    }

    fn upsert_with(
        &mut self,
        mut record: SiteRecord,
        merge: impl FnOnce(&mut SiteRecord, SiteRecord),
    ) -> bool {
        match canonical_domain(&record.domain) {
            Some(domain) => record.domain = domain,
            None => return false,
        }
        // A bot check crawled in place of the homepage, by this node or by
        // one whose crawl it took, before crawlers knew to skip them.
        record.drop_bot_check();
        match self.map.get_mut(&record.domain) {
            Some(existing) => merge(existing, record),
            None => {
                self.map.insert(record.domain.clone(), Box::new(record));
            }
        }
        true
    }

    pub fn iter(&self) -> impl Iterator<Item = &SiteRecord> {
        self.map.values().map(|record| &**record)
    }

    /// Keeps only the records for which `keep` is true.
    pub fn retain(&mut self, mut keep: impl FnMut(&SiteRecord) -> bool) {
        self.map.retain(|_, record| keep(record));
    }

    /// All records, best [`link_score`] first, ties broken by domain.
    pub fn into_sorted_vec(self) -> Vec<SiteRecord> {
        let mut records: Vec<SiteRecord> = self.into_iter().collect();
        sort_by_link_score(&mut records);
        records
    }
}

/// All records, in no particular order. Each record is freed as soon as it
/// is taken, so a caller that keeps only a little of each never holds two
/// copies of the set.
impl IntoIterator for RecordSet {
    type Item = SiteRecord;
    type IntoIter = std::iter::Map<
        std::collections::hash_map::IntoValues<String, Box<SiteRecord>>,
        fn(Box<SiteRecord>) -> SiteRecord,
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.map.into_values().map(|record| *record)
    }
}

impl Extend<SiteRecord> for RecordSet {
    fn extend<I: IntoIterator<Item = SiteRecord>>(&mut self, iter: I) {
        for record in iter {
            self.upsert(record);
        }
    }
}

impl FromIterator<SiteRecord> for RecordSet {
    fn from_iter<I: IntoIterator<Item = SiteRecord>>(iter: I) -> Self {
        let mut set = RecordSet::new();
        set.extend(iter);
        set
    }
}

/// Sorts best [`link_score`] first, ties broken by domain so output is stable.
pub fn sort_by_link_score(records: &mut [SiteRecord]) {
    records.sort_by(|a, b| {
        b.link_score()
            .total_cmp(&a.link_score())
            .then_with(|| a.domain.cmp(&b.domain))
    });
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
/// organization is ([`Signals::sitelinks`]). The result is capped at 1.0.
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
    score.min(1.0) as f32
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

/// The registrable domain ("eTLD+1") of a URL or hostname, using the Public
/// Suffix List: `https://www.usbank.com/x` -> `usbank.com`,
/// `news.bbc.co.uk` -> `bbc.co.uk`. Returns `None` for IP addresses, bare
/// public suffixes like `co.uk`, single-label hosts like `localhost`, and
/// everything [`host_of`] rejects.
pub fn registrable_domain(input: &str) -> Option<String> {
    let host = host_of(input)?;
    psl::domain_str(&host).map(str::to_string)
}

/// The canonical spelling of a site record's domain: the domain itself
/// when it already is a lowercase registrable domain (the common case,
/// checked without parsing), otherwise [`registrable_domain`] of it. So
/// `Example.com.` -> `example.com`, `münchen.de` -> `xn--mnchen-3ya.de`,
/// `www.example.com` -> `example.com`, and junk -> `None`.
pub fn canonical_domain(domain: &str) -> Option<String> {
    if is_valid_host(domain) && psl::domain_str(domain) == Some(domain) {
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
/// `_` and two to four more.
fn is_locale_segment(segment: &str) -> bool {
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
    match psl::suffix_str(&domain) {
        Some(suffix) if domain.len() > suffix.len() + 1 && domain.ends_with(suffix) => {
            domain[..domain.len() - suffix.len() - 1].to_string()
        }
        _ => domain,
    }
}

/// Normalizes text for matching: lowercase, letters and digits only, every
/// other run of characters becomes one space. Apostrophes are dropped
/// (`McDonald's` -> `mcdonalds`) and runs of single-character tokens are
/// joined, so `U.S. Bank` -> `us bank` and `A.T.M.` -> `atm`.
pub fn normalize_text(text: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            // Only letters and digits of the lowercase form, so a second
            // pass changes nothing: `İ` lowercases to `i` plus a combining dot.
            current.extend(ch.to_lowercase().filter(|c| c.is_alphanumeric()));
        } else if matches!(ch, '\'' | '\u{2019}' | '\u{02BC}') {
            // Apostrophes join the two halves of a word.
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let mut j = i;
        while j < tokens.len() && tokens[j].chars().count() == 1 {
            j += 1;
        }
        if j - i >= 2 {
            out.push(tokens[i..j].concat());
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

/// The primary language of a language tag, lowercase: `en-US` -> `en`,
/// `zh-Hant-TW` -> `zh`, `DE` -> `de`. `None` for tags that name no
/// language (`x-default`, `und`, `zxx`, `mul`) or are not tags at all.
pub fn language_code(tag: &str) -> Option<String> {
    let primary = tag.trim().split(['-', '_']).next()?.to_ascii_lowercase();
    let letters = primary.bytes().all(|b| b.is_ascii_lowercase());
    let named = !matches!(primary.as_str(), "und" | "zxx" | "mul" | "mis");
    (letters && (2..=3).contains(&primary.len()) && named).then_some(primary)
}

/// Collapses every whitespace run to one space and trims the ends.
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
    fn language_codes_from_tags() {
        assert_eq!(language_code("en-US").as_deref(), Some("en"));
        assert_eq!(language_code("zh_Hant_TW").as_deref(), Some("zh"));
        assert_eq!(language_code(" DE ").as_deref(), Some("de"));
        for tag in ["", "x-default", "und", "zxx", "e", "english", "12"] {
            assert_eq!(language_code(tag), None, "{tag}");
        }
        let mut mine = SiteRecord::new("a.com");
        mine.language = Some("en".into());
        mine.crawled_at = Some(1);
        let mut newer = SiteRecord::new("a.com");
        newer.crawled_at = Some(2);
        mine.merge(newer.clone());
        assert_eq!(mine.language.as_deref(), Some("en"));
        newer.language = Some("de".into());
        newer.crawled_at = Some(3);
        mine.merge(newer);
        assert_eq!(mine.language.as_deref(), Some("de"));
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
        for text in ["İstanbul", "Ǆemal", "ﬁnance", "Straße", "ΣΊΣΥΦΟΣ", "İİ.İ"] {
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
    fn redirects_last_until_a_later_successful_crawl() {
        let redirect = |at: u64| SiteRecord {
            redirect: Some(Redirect {
                to: "pnc.com".into(),
                at,
            }),
            ..SiteRecord::new("pncbank.com")
        };
        let crawled = |at: u64| SiteRecord {
            crawled_at: Some(at),
            title: Some("PNC Bank".into()),
            ..SiteRecord::new("pncbank.com")
        };
        // In either order, the later event wins.
        for (first, second) in [(crawled(10), redirect(20)), (redirect(20), crawled(10))] {
            let mut record = first;
            record.merge(second);
            assert_eq!(
                record.redirect.as_ref().map(|r| r.to.as_str()),
                Some("pnc.com")
            );
        }
        for (first, second) in [(crawled(30), redirect(20)), (redirect(20), crawled(30))] {
            let mut record = first;
            record.merge(second);
            assert_eq!(record.redirect, None);
        }
        let mut record = redirect(20);
        record.merge(redirect(10));
        assert_eq!(record.redirect.unwrap().at, 20);
    }

    #[test]
    fn headings_follow_the_fresher_crawl() {
        let crawled = |at: u64, headings: &[&str]| SiteRecord {
            domain: "a.com".into(),
            crawled_at: Some(at),
            headings: headings.iter().map(|h| h.to_string()).collect(),
            ..SiteRecord::default()
        };
        let mut record = crawled(1, &["Old"]);
        record.merge(crawled(2, &[]));
        assert!(record.headings.is_empty());
        let mut record = crawled(2, &["New"]);
        record.merge(crawled(1, &["Old"]));
        assert_eq!(record.headings, ["New"]);
        let mut seed = SiteRecord {
            domain: "a.com".into(),
            ..SiteRecord::default()
        };
        seed.merge(crawled(1, &["Found"]));
        assert_eq!(seed.headings, ["Found"]);
    }

    #[test]
    fn a_shared_crawl_updates_the_page_but_keeps_what_it_leaves_out() {
        let mut mine = SiteRecord {
            domain: "a.com".into(),
            title: Some("Old".into()),
            crawled_at: Some(1),
            search_url: Some("https://a.com/search?q={q}".into()),
            headings: vec!["Welcome".into()],
            body_text: Some("A shop for things".into()),
            key_pages: vec![KeyPage {
                label: "Sign in".into(),
                url: "https://a.com/login".into(),
            }],
            ..SiteRecord::default()
        };
        let shared = SiteRecord {
            domain: "a.com".into(),
            title: Some("New".into()),
            crawled_at: Some(2),
            ..SiteRecord::default()
        };
        let mut plain = mine.clone();
        plain.merge(shared.clone());
        assert!(plain.search_url.is_none() && plain.body_text.is_none());
        assert!(plain.key_pages.is_empty());

        mine.merge_shared(shared);
        assert_eq!(mine.title.as_deref(), Some("New"));
        assert_eq!(mine.crawled_at, Some(2));
        assert_eq!(
            mine.search_url.as_deref(),
            Some("https://a.com/search?q={q}")
        );
        assert_eq!(mine.headings, ["Welcome"]);
        assert_eq!(mine.body_text.as_deref(), Some("A shop for things"));
        assert_eq!(mine.key_pages.len(), 1);

        // Text a trusted crawler shared does replace it.
        mine.merge_shared(SiteRecord {
            domain: "a.com".into(),
            crawled_at: Some(3),
            headings: vec!["Hello".into()],
            body_text: Some("Now a blog".into()),
            ..SiteRecord::default()
        });
        assert_eq!(mine.headings, ["Hello"]);
        assert_eq!(mine.body_text.as_deref(), Some("Now a blog"));
    }

    #[test]
    fn merge_handles_country_kinds_and_search_addresses() {
        let mut seed = SiteRecord::new("chase.com");
        seed.country = Some("US".into());
        seed.add_kind("bank");
        seed.add_kind("Public company");
        assert_eq!(seed.kinds, ["bank"], "generic kinds are dropped");

        let mut crawl = SiteRecord::new("chase.com");
        crawl.crawled_at = Some(10);
        crawl.search_url = Some("https://www.chase.com/search?q={searchTerms}".into());
        crawl.country = Some("GB".into());
        crawl.add_kind("Banks");
        crawl.add_kind("financial services");
        seed.merge(crawl);
        assert_eq!(seed.country.as_deref(), Some("US"));
        assert_eq!(seed.kinds, ["bank", "financial services"]);
        assert!(seed.search_url.is_some());

        // A later crawl without a search form clears it; an older one does not.
        let mut older = SiteRecord::new("chase.com");
        older.crawled_at = Some(5);
        seed.merge(older);
        assert!(seed.search_url.is_some());
        let mut newer = SiteRecord::new("chase.com");
        newer.crawled_at = Some(20);
        seed.merge(newer);
        assert_eq!(seed.search_url, None);
    }

    #[test]
    fn merge_keeps_best_signals_and_fresh_pages() {
        let mut a = SiteRecord::new("usbank.com");
        a.title = Some("Old title".into());
        a.crawled_at = Some(100);
        a.signals.tranco_rank = Some(900);
        a.add_link_text("U.S. Bank", "a.com");
        a.add_link_text("U.S. Bank", "b.com");

        let mut b = SiteRecord::new("usbank.com");
        b.title = Some("U.S. Bank | Personal Banking".into());
        b.description = Some("Banking, credit cards, loans".into());
        b.crawled_at = Some(200);
        b.crawl_attempted_at = Some(250);
        b.signals.tranco_rank = Some(1200);
        b.signals.harmonic_rank = Some(5000);
        b.signals.official_site = true;
        // b.com is already counted, c.com is new.
        b.add_link_text("us bank", "b.com");
        b.add_link_text("us bank", "c.com");
        b.add_alias("U.S. Bancorp");

        a.merge(b);
        assert_eq!(a.title.as_deref(), Some("U.S. Bank | Personal Banking"));
        assert_eq!(
            a.description.as_deref(),
            Some("Banking, credit cards, loans")
        );
        assert_eq!(a.crawled_at, Some(200));
        assert_eq!(a.crawl_attempted_at, Some(250));
        assert_eq!(a.signals.tranco_rank, Some(900));
        assert_eq!(a.signals.harmonic_rank, Some(5000));
        assert!(a.signals.official_site);
        let linkers = ["a.com", "b.com", "c.com"]
            .iter()
            .fold(0, |bits, d| bits | linker_bit(d));
        assert_eq!(
            a.link_texts,
            vec![LinkText {
                text: "us bank".into(),
                count: 3,
                linkers,
            }]
        );
        assert_eq!(a.aliases, vec!["U.S. Bancorp".to_string()]);
    }

    #[test]
    fn merge_keeps_the_failure_count_of_the_later_attempt() {
        let tried = |attempted_at: Option<u64>, failures: u32| {
            let mut r = SiteRecord::new("flaky.com");
            r.crawl_attempted_at = attempted_at;
            r.crawl_failures = failures;
            r
        };
        let merged = |a: SiteRecord, b: SiteRecord| {
            let mut a = a;
            a.merge(b);
            (a.crawl_attempted_at, a.crawl_failures)
        };
        // A later answer clears earlier failures, and a later failure counts.
        assert_eq!(
            merged(tried(Some(10), 3), tried(Some(20), 0)),
            (Some(20), 0)
        );
        assert_eq!(
            merged(tried(Some(20), 0), tried(Some(10), 3)),
            (Some(20), 0)
        );
        assert_eq!(
            merged(tried(Some(10), 0), tried(Some(20), 2)),
            (Some(20), 2)
        );
        assert_eq!(merged(tried(None, 0), tried(Some(20), 2)), (Some(20), 2));
        assert_eq!(merged(tried(Some(20), 2), tried(None, 0)), (Some(20), 2));
        // The same attempt seen twice keeps the larger count.
        assert_eq!(
            merged(tried(Some(20), 1), tried(Some(20), 2)),
            (Some(20), 2)
        );
        assert_eq!(
            merged(tried(Some(20), 2), tried(Some(20), 1)),
            (Some(20), 2)
        );
    }

    #[test]
    fn crawl_failures_are_left_out_of_json_until_there_are_some() {
        let mut r = SiteRecord::new("example.com");
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"domain":"example.com","signals":{}}"#
        );
        r.crawl_failures = 2;
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""crawl_failures":2"#), "{json}");
        assert_eq!(serde_json::from_str::<SiteRecord>(&json).unwrap(), r);
        let old: SiteRecord =
            serde_json::from_str(r#"{"domain":"example.com","crawl_attempted_at":5}"#).unwrap();
        assert_eq!((old.crawl_attempted_at, old.crawl_failures), (Some(5), 0));
    }

    #[test]
    fn link_texts_count_each_linking_site_once() {
        let mut a = SiteRecord::new("usbank.com");
        for _ in 0..60 {
            // One site repeating a footer link on every page.
            a.add_link_text("US Bank", "spam.example");
        }
        assert_eq!(a.link_texts[0].count, 1);

        // A monthly re-crawl of the same five sites adds nothing.
        let linkers = ["a.com", "b.com", "c.com", "d.com", "e.com"];
        let crawl = |text: &str| {
            let mut r = SiteRecord::new("usbank.com");
            for d in linkers {
                r.add_link_text(text, d);
            }
            r
        };
        let mut b = crawl("us bank");
        for _ in 0..12 {
            b.merge(crawl("us bank"));
        }
        assert_eq!(b.link_texts[0].count, 5);

        // Plain counts (no linker set) keep the larger count on merge.
        let mut c = SiteRecord::new("x.com");
        c.link_texts.push(LinkText::with_count("x", 7));
        let mut d = SiteRecord::new("x.com");
        d.link_texts.push(LinkText::with_count("x", 4));
        d.add_link_text("y", "a.com");
        c.merge(d);
        assert_eq!(c.link_texts[0], LinkText::with_count("x", 7));
        assert_eq!(c.link_texts[1].count, 1);
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
    fn link_text_json_round_trips() {
        let lt = LinkText::from_linkers("us bank", linker_bit("a.com") | linker_bit("b.com"));
        let json = serde_json::to_string(&lt).unwrap();
        assert_eq!(serde_json::from_str::<LinkText>(&json).unwrap(), lt);
        // Older files have a count only.
        let old: LinkText = serde_json::from_str(r#"{"text":"x","count":3}"#).unwrap();
        assert_eq!(old, LinkText::with_count("x", 3));
    }

    #[test]
    fn hosts_are_validated() {
        for bad in [
            "mailto:a@b.com",
            "a@b.com",
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
    fn record_domains_are_canonical() {
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

        let mut set = RecordSet::new();
        let mut a = SiteRecord::new("Example.com");
        a.signals.tranco_rank = Some(10);
        assert!(set.upsert(a));
        assert!(set.upsert(SiteRecord::new("example.com")));
        assert!(set.upsert(SiteRecord::new("münchen.de")));
        assert!(set.upsert(SiteRecord::new("xn--mnchen-3ya.de")));
        assert!(!set.upsert(SiteRecord::new("not a domain")));
        assert_eq!(set.len(), 2);
        assert_eq!(
            set.get("example.com").unwrap().signals.tranco_rank,
            Some(10)
        );
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
    fn older_records_only_fill_gaps() {
        let mut a = SiteRecord::new("x.com");
        a.title = Some("New".into());
        a.crawled_at = Some(200);
        let mut b = SiteRecord::new("x.com");
        b.title = Some("Old".into());
        b.description = Some("Desc".into());
        b.crawled_at = Some(100);
        a.merge(b);
        assert_eq!(a.title.as_deref(), Some("New"));
        assert_eq!(a.description.as_deref(), Some("Desc"));
        assert_eq!(a.crawled_at, Some(200));
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
    fn record_set_upserts_and_sorts() {
        let mut set = RecordSet::new();
        let mut a = SiteRecord::new("a.com");
        a.signals.tranco_rank = Some(50);
        let mut b = SiteRecord::new("b.com");
        b.signals.tranco_rank = Some(5);
        set.upsert(a);
        set.upsert(b);
        set.entry("a.com").add_alias("Alpha");
        set.upsert(SiteRecord::new("c.com"));
        assert_eq!(set.len(), 3);
        let sorted = set.into_sorted_vec();
        let domains: Vec<&str> = sorted.iter().map(|r| r.domain.as_str()).collect();
        assert_eq!(domains, ["b.com", "a.com", "c.com"]);
        assert_eq!(sorted[1].aliases, vec!["Alpha".to_string()]);
    }

    #[test]
    fn jsonl_round_trip() {
        let dir = std::env::temp_dir().join(format!("plumb-core-test-{}", std::process::id()));
        let path = dir.join("records.jsonl");
        let mut rec = SiteRecord::new("usbank.com");
        rec.title = Some("U.S. Bank".into());
        rec.signals.tranco_rank = Some(900);
        let written = write_jsonl(&path, &[rec.clone(), SiteRecord::new("example.com")]).unwrap();
        assert_eq!(written, 2);
        let back: Vec<SiteRecord> = read_jsonl(&path).unwrap();
        assert_eq!(back, vec![rec, SiteRecord::new("example.com")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncation_is_char_safe() {
        assert_eq!(truncate_chars("héllo wörld", 4), "héll");
        assert_eq!(truncate_chars("short", 10), "short");
    }
}
