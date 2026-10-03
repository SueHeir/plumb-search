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

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Most inbound link texts kept per site (the most frequent ones win).
pub const MAX_LINK_TEXTS: usize = 32;
/// Most aliases kept per site.
pub const MAX_ALIASES: usize = 16;
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
}

/// Inbound link text and how many links used it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkText {
    pub text: String,
    pub count: u32,
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
}

fn is_zero(n: &u32) -> bool {
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

    /// Adds `count` uses of a link text. The text is normalized first; empty
    /// text is ignored. Keeps the list sorted and capped at [`MAX_LINK_TEXTS`].
    pub fn add_link_text(&mut self, text: &str, count: u32) {
        let text = truncate_chars(&normalize_text(text), MAX_TEXT_CHARS);
        if text.is_empty() || count == 0 {
            return;
        }
        match self.link_texts.iter_mut().find(|lt| lt.text == text) {
            Some(lt) => lt.count = lt.count.saturating_add(count),
            None => self.link_texts.push(LinkText { text, count }),
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

    /// Folds another record for the same domain into this one.
    ///
    /// Page fields (url, title, description) come from whichever record was
    /// crawled more recently, and missing ones are filled from the other.
    /// Link text counts add up, aliases are unioned, ranks keep the best
    /// (lowest) value, `linking_domains` keeps the larger count (sources often
    /// overlap, so adding would double count), `official_site` is OR-ed, and
    /// `crawl_attempted_at` keeps the later time.
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
            self.crawled_at = other.crawled_at;
        } else {
            self.url = self.url.take().or(other.url);
            self.title = self.title.take().or(other.title);
            self.description = self.description.take().or(other.description);
        }
        for lt in other.link_texts {
            match self.link_texts.iter_mut().find(|mine| mine.text == lt.text) {
                Some(mine) => mine.count = mine.count.saturating_add(lt.count),
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
#[derive(Debug, Clone, Default)]
pub struct RecordSet {
    map: HashMap<String, SiteRecord>,
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
        self.map.get(domain)
    }

    /// The record for `domain`, created empty if it is not there yet.
    pub fn entry(&mut self, domain: &str) -> &mut SiteRecord {
        self.map
            .entry(domain.to_string())
            .or_insert_with(|| SiteRecord::new(domain))
    }

    /// Inserts a record, merging it into an existing one for the same domain.
    pub fn upsert(&mut self, record: SiteRecord) {
        match self.map.get_mut(&record.domain) {
            Some(existing) => existing.merge(record),
            None => {
                self.map.insert(record.domain.clone(), record);
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &SiteRecord> {
        self.map.values()
    }

    /// All records, best [`link_score`] first, ties broken by domain.
    pub fn into_sorted_vec(self) -> Vec<SiteRecord> {
        let mut records: Vec<SiteRecord> = self.map.into_values().collect();
        sort_by_link_score(&mut records);
        records
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

/// A popularity prior in `0.0..=1.0` built from the site's signals.
///
/// Ranks map onto a log scale shared by every ranking (rank 1 is 1.0, rank
/// 1,000 about 0.63, rank 1,000,000 about 0.25, rank 100,000,000 is 0), and the best
/// of them counts for 75%. The number of linking domains, also on a log
/// scale, counts for the other 25%. An official website listed in Wikidata
/// gets a 0.15 bonus. The result is capped at 1.0.
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
        score += 0.15;
    }
    score.min(1.0) as f32
}

/// The lowercase host of a URL or bare hostname, without port or trailing dot.
/// IDNs come back as punycode. Returns `None` for IP addresses and junk.
pub fn host_of(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let parsed = if input.contains("://") {
        url::Url::parse(input).ok()?
    } else {
        url::Url::parse(&format!("http://{input}")).ok()?
    };
    match parsed.host()? {
        url::Host::Domain(host) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            (!host.is_empty()).then_some(host)
        }
        url::Host::Ipv4(_) | url::Host::Ipv6(_) => None,
    }
}

/// The registrable domain ("eTLD+1") of a URL or hostname, using the Public
/// Suffix List: `https://www.usbank.com/x` -> `usbank.com`,
/// `news.bbc.co.uk` -> `bbc.co.uk`. Returns `None` for IP addresses, bare
/// public suffixes like `co.uk`, and single-label hosts like `localhost`.
pub fn registrable_domain(input: &str) -> Option<String> {
    let host = host_of(input)?;
    psl::domain_str(&host).map(str::to_string)
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
            current.extend(ch.to_lowercase());
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

/// Collapses every whitespace run to one space and trims the ends.
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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
        assert_eq!(joined("U.S. Bank"), "usbank");
        assert_eq!(joined("Bank of America"), "bankofamerica");
    }

    #[test]
    fn merge_keeps_best_signals_and_fresh_pages() {
        let mut a = SiteRecord::new("usbank.com");
        a.title = Some("Old title".into());
        a.crawled_at = Some(100);
        a.signals.tranco_rank = Some(900);
        a.add_link_text("U.S. Bank", 3);

        let mut b = SiteRecord::new("usbank.com");
        b.title = Some("U.S. Bank | Personal Banking".into());
        b.description = Some("Banking, credit cards, loans".into());
        b.crawled_at = Some(200);
        b.crawl_attempted_at = Some(250);
        b.signals.tranco_rank = Some(1200);
        b.signals.harmonic_rank = Some(5000);
        b.signals.official_site = true;
        b.add_link_text("us bank", 2);
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
        assert_eq!(
            a.link_texts,
            vec![LinkText {
                text: "us bank".into(),
                count: 5
            }]
        );
        assert_eq!(a.aliases, vec!["U.S. Bancorp".to_string()]);
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
