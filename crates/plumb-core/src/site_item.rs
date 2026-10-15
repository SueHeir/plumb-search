//! A crawled site as an item of the data format ([`plumb_format`]): kind
//! `site`, its id the registrable domain. [`SiteRecord::to_item`] makes one
//! and [`SiteRecord::from_item`] reads it back, so sets of sites
//! (`plumb/sites`), crawl batches and bucket answers carry sites as items,
//! the last two a line of JSON each ([`SiteRecord::to_line`]).
//!
//! The item's own members hold what any reader of items understands: the
//! homepage's URL, title and description (`about`), the start of its text
//! (`text`, cut to the format's 1,000 characters), its language, the
//! site's other names, and how many other sites link to it (`weight`).
//! Its edges are the sites it links to (`links_to`, to `site:` refs) and
//! its key pages (`key_page`, to `url:` refs, the label as the link's
//! text). The rest are fields, declared by [`site_fields`]:
//!
//! ```json
//! {"headings": ["Get Started"], "terms": ["python", "download"],
//!  "search_url": "https://www.python.org/search/?q={searchTerms}",
//!  "link_texts": [{"text": "python", "count": 412, "linkers": "00a1b2c3d4e5f607"}],
//!  "ranks": {"tranco": 512, "harmonic": 300, "pagerank": 250},
//!  "wikidata": {"about": "general-purpose programming language", "country": "US",
//!               "official_site": true, "sitelinks": 150},
//!  "fact_checks": {"checked": 12, "agreed": 11},
//!  "kinds": ["programming language"], "intro": "...", "summary": "...",
//!  "crawled": "2026-10-10T04:12:00Z", "crawl_version": 7}
//! ```
//!
//! `linkers` is hex because a 64-bit number does not survive every JSON
//! reader. A homepage that sent the crawler to another site says where
//! (`"redirect": {"to": "pnc.com", "at": "2026-10-10T04:12:00Z"}`, with
//! `"inner": true` when it went to a page other than that site's front
//! page), so a search can fold the site into that one; filling leaves it
//! out. A node's own bookkeeping (crawl tries and failures, when it judged
//! the site gone) stays out, and so do icons and headlines, which travel
//! on their own. So do links to hosts the format cannot name as a site
//! (with a `_`), and URLs that do not come out canonical.
//!
//! [`SiteRecord::clean`] puts a record itself in its item's form, short of
//! cutting its text, so that a clean record comes back from its item as it
//! was, but for a redirect to a site no item can name. Records are cleaned
//! where they enter a set ([`crate::RecordSet::upsert`]).

use std::collections::BTreeMap;

use anyhow::{bail, ensure, Result};
use plumb_format::item::{MAX_ITEM_BYTES, MAX_TEXT_CHARS};
use plumb_format::refs::is_domain;
use plumb_format::urls::{canonical_url, is_canonical};
use plumb_format::value::{is_language, normalize_text, unix_seconds};
use plumb_format::{Edge, FieldSpec, Item, Ref};
use serde_json::{json, Map, Value};
use unicode_normalization::{is_nfc_quick, IsNormalized};

use crate::key_pages::{KeyPage, MAX_KEY_PAGES};
use crate::{
    canonical_domain, sort_and_cap_link_texts, LinkText, Redirect, SiteRecord, MAX_LINKS_TO,
};

/// The kind of a site's item.
pub const SITE: &str = "site";

/// The fields an item over the format's size sheds, in this order: the
/// ones searched least, as a page sheds its passages.
const SHED: [&str; 3] = ["terms", "headings", "link_texts"];

/// The fields of a set of sites, for its manifest.
pub fn site_fields() -> BTreeMap<String, FieldSpec> {
    serde_json::from_value(json!({
        "headings": {"type": "text", "list": true, "roles": ["search:body"]},
        "terms": {"type": "keyword", "list": true, "roles": ["search:body"]},
        "search_url": {"type": "keyword"},
        "link_texts": {"type": "object", "list": true, "roles": ["search:name"]},
        "ranks": {"type": "object"},
        "wikidata": {"type": "object", "roles": ["show"]},
        "fact_checks": {"type": "object"},
        "kinds": {"type": "keyword", "list": true, "roles": ["filter", "show"]},
        "intro": {"type": "text", "roles": ["search:about", "show"]},
        "summary": {"type": "text", "roles": ["search:about"]},
        "crawled": {"type": "date"},
        "crawl_version": {"type": "number"},
        "redirect": {"type": "object"},
    }))
    .expect("the site fields are well formed")
}

/// The site a line [`SiteRecord::to_line`] wrote names, and when it was
/// crawled, read without checking the rest: enough to note where a crawl
/// of a site is, for much less work than [`SiteRecord::from_line`].
pub fn crawl_of_line(line: &str) -> Option<(String, Option<u64>)> {
    #[derive(serde::Deserialize)]
    struct Line {
        id: String,
        kind: String,
        #[serde(default)]
        fields: Fields,
    }
    #[derive(serde::Deserialize, Default)]
    struct Fields {
        crawled: Option<String>,
    }
    let line: Line = serde_json::from_str(line).ok()?;
    let crawled = line.fields.crawled.as_deref().and_then(unix_seconds);
    (line.kind == SITE).then(|| (line.id, crawled.and_then(|at| u64::try_from(at).ok())))
}

impl SiteRecord {
    /// Puts the site in the form its item has ([`SiteRecord::to_item`]):
    /// text in Unicode NFC, control characters made spaces, trimmed; URLs
    /// in canonical form; and only the language tag, key pages, linked
    /// sites and crawl time an item can hold. What comes out empty or has
    /// no such form is dropped, names and kinds are kept once each, and
    /// link texts that come out the same are counted together. The page
    /// text is not cut, unlike the item's, and a node's own bookkeeping,
    /// the icon and headlines are left as they are. So is a redirect,
    /// which folding reads even to a site no item can name (with a `_`),
    /// but for its site's canonical spelling. Cleaning a clean record
    /// changes nothing.
    pub fn clean(&mut self) {
        self.url = self.url.take().and_then(canonical_owned);
        self.title = self.title.take().and_then(tidy);
        self.description = self.description.take().and_then(tidy);
        self.body_text = self.body_text.take().and_then(tidy);
        self.language = self.language.take().filter(|lang| is_language(lang));
        self.aliases = once(tidy_all(std::mem::take(&mut self.aliases)));
        self.headings = tidy_all(std::mem::take(&mut self.headings));
        self.terms = tidy_keywords(std::mem::take(&mut self.terms));
        self.kinds = once(tidy_keywords(std::mem::take(&mut self.kinds)));
        self.search_url = self.search_url.take().and_then(tidy_keyword);
        let mut link_texts: Vec<LinkText> = Vec::with_capacity(self.link_texts.len());
        for mut link_text in std::mem::take(&mut self.link_texts) {
            let Some(text) = tidy(link_text.text) else {
                continue;
            };
            match link_texts.iter_mut().find(|kept| kept.text == text) {
                Some(kept) => {
                    link_text.text = text;
                    kept.absorb(&link_text);
                }
                None => {
                    link_text.text = text;
                    link_texts.push(link_text);
                }
            }
        }
        sort_and_cap_link_texts(&mut link_texts);
        self.link_texts = link_texts;
        self.about = self.about.take().and_then(tidy);
        self.country = self.country.take().and_then(tidy_keyword);
        self.intro = self.intro.take().and_then(tidy);
        self.summary = self.summary.take().and_then(tidy);
        self.crawled_at = self.crawled_at.filter(|&at| timestamp(at).is_some());
        if let Some(redirect) = &mut self.redirect {
            if let Some(to) = canonical_domain(&redirect.to) {
                redirect.to = to;
            }
        }
        self.key_pages = std::mem::take(&mut self.key_pages)
            .into_iter()
            .filter_map(|page| {
                Some(KeyPage {
                    url: canonical_owned(page.url)?,
                    label: tidy(page.label)?,
                })
            })
            .take(MAX_KEY_PAGES)
            .collect();
        self.links_to.retain(|site| is_domain(site));
        self.links_to.truncate(MAX_LINKS_TO);
    }

    /// This site as an item ([`crate::site_item`]), of the record as
    /// [`SiteRecord::clean`] makes it, with its text cut to the format's
    /// 1,000 characters. Errors when the item would not be a valid one, for
    /// a domain that is no id or a record too big even after shedding.
    pub fn to_item(&self) -> Result<Item> {
        let mut site = self.clone();
        site.clean();
        site.clean_to_item()
    }

    /// [`SiteRecord::to_item`] of a clean record.
    fn clean_to_item(self) -> Result<Item> {
        let fields = self.fields();
        let mut item = Item {
            kind: SITE.into(),
            url: self.url,
            title: self.title,
            about: self.description,
            text: self
                .body_text
                .map(|text| match text.char_indices().nth(MAX_TEXT_CHARS) {
                    Some((cut, _)) => text[..cut].trim_end().to_string(),
                    None => text,
                }),
            lang: self.language,
            names: self.aliases,
            weight: (self.signals.linking_domains > 0)
                .then(|| f64::from(self.signals.linking_domains)),
            fields,
            id: self.domain,
            ..Item::default()
        };
        if item.url.is_none() && is_domain(&item.id) {
            item.site = Some(item.id.clone());
        }
        for page in self.key_pages {
            item.edges
                .push(edge("key_page", Ref::Url(page.url), Some(page.label)));
        }
        for site in self.links_to {
            item.edges.push(edge("links_to", Ref::Site(site), None));
        }
        while too_big(&item) {
            if let Some(field) = SHED.iter().find(|f| item.fields.contains_key(**f)) {
                item.fields.remove(*field);
            } else if item.edges.iter().any(|edge| edge.rel == "links_to") {
                item.edges.retain(|edge| edge.rel != "links_to");
            } else {
                bail!("{} is over {MAX_ITEM_BYTES} bytes as an item", item.id);
            }
        }
        item.check()?;
        Ok(item)
    }

    /// The site a `site` item holds ([`crate::site_item`]). Members and
    /// fields it does not know, or that do not read, are left out.
    pub fn from_item(item: &Item) -> Result<SiteRecord> {
        ensure!(item.kind == SITE, "item {:?} is not a site", item.id);
        let field = |name: &str| item.fields.get(name);
        let member = |name: &str, key: &str| field(name).and_then(|value| value.get(key));
        let mut record = SiteRecord {
            domain: item.id.clone(),
            url: item.url.clone(),
            title: item.title.clone(),
            description: item.about.clone(),
            body_text: item.text.clone(),
            aliases: item.names.clone(),
            language: item.lang.clone(),
            headings: strings(field("headings")),
            terms: strings(field("terms")),
            search_url: string(field("search_url")),
            kinds: strings(field("kinds")),
            intro: string(field("intro")),
            summary: string(field("summary")),
            about: string(member("wikidata", "about")),
            country: string(member("wikidata", "country")),
            crawled_at: field("crawled")
                .and_then(Value::as_str)
                .and_then(unix_seconds)
                .and_then(|unix| u64::try_from(unix).ok()),
            crawl_version: number(field("crawl_version")).unwrap_or_default(),
            ..SiteRecord::default()
        };
        let signals = &mut record.signals;
        signals.linking_domains = item.weight.map_or(0, |weight| weight as u32);
        signals.tranco_rank = number(member("ranks", "tranco"));
        signals.harmonic_rank = number(member("ranks", "harmonic"));
        signals.pagerank_rank = number(member("ranks", "pagerank"));
        signals.official_site = member("wikidata", "official_site") == Some(&Value::Bool(true));
        signals.sitelinks = number(member("wikidata", "sitelinks")).unwrap_or_default();
        signals.fact_checks = number(member("fact_checks", "checked")).unwrap_or_default();
        signals.fact_agrees = number(member("fact_checks", "agreed")).unwrap_or_default();
        record.link_texts = field("link_texts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|link_text| {
                let linkers = match link_text.get("linkers") {
                    Some(hex) => u64::from_str_radix(hex.as_str()?, 16).ok()?,
                    None => 0,
                };
                Some(LinkText {
                    text: string(link_text.get("text"))?,
                    count: number(link_text.get("count"))?,
                    linkers,
                })
            })
            .collect();
        record.redirect = field("redirect").and_then(|redirect| {
            Some(Redirect {
                to: string(redirect.get("to")).filter(|to| is_domain(to))?,
                at: redirect
                    .get("at")
                    .and_then(Value::as_str)
                    .and_then(unix_seconds)
                    .and_then(|unix| u64::try_from(unix).ok())?,
                inner: redirect.get("inner") == Some(&Value::Bool(true)),
            })
        });
        for edge in &item.edges {
            match (edge.rel.as_str(), &edge.to) {
                ("links_to", Ref::Site(site)) => record.links_to.push(site.clone()),
                ("key_page", Ref::Url(url)) => {
                    if let Some(label) = &edge.text {
                        record.key_pages.push(KeyPage {
                            label: label.clone(),
                            url: url.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(record)
    }

    /// This site as one line of JSON, the item [`SiteRecord::to_item`]
    /// makes: how crawl batches and bucket answers carry a site.
    pub fn to_line(&self) -> Result<String> {
        Ok(serde_json::to_string(&self.to_item()?)?)
    }

    /// The site of a line [`SiteRecord::to_line`] wrote, once its item
    /// keeps the format's rules ([`Item::check`]).
    pub fn from_line(line: &str) -> Result<SiteRecord> {
        let item: Item = serde_json::from_str(line)?;
        item.check()?;
        ensure!(
            !item.is_extension() && !item.is_deleted(),
            "item {:?} is not a whole site",
            item.id
        );
        SiteRecord::from_item(&item)
    }

    /// The item's fields ([`site_fields`]) of a clean record, each only
    /// when it says something.
    fn fields(&self) -> BTreeMap<String, Value> {
        let mut fields = BTreeMap::new();
        let mut put = |name: &str, value: Value| {
            let empty = match &value {
                Value::Null => true,
                Value::Array(list) => list.is_empty(),
                Value::Object(map) => map.is_empty(),
                _ => false,
            };
            if !empty {
                fields.insert(name.to_string(), value);
            }
        };
        put("headings", json!(self.headings));
        put("terms", json!(self.terms));
        put("kinds", json!(self.kinds));
        put("search_url", json!(self.search_url));
        let link_texts: Vec<Value> = self
            .link_texts
            .iter()
            .map(|link_text| {
                let mut object = json!({"text": link_text.text, "count": link_text.count});
                if link_text.linkers != 0 {
                    object["linkers"] = json!(format!("{:016x}", link_text.linkers));
                }
                object
            })
            .collect();
        put("link_texts", Value::Array(link_texts));
        let signals = &self.signals;
        let mut ranks = Map::new();
        let mut rank = |key: &str, rank: Option<u64>| {
            if let Some(rank) = rank {
                ranks.insert(key.into(), json!(rank));
            }
        };
        rank("tranco", signals.tranco_rank.map(u64::from));
        rank("harmonic", signals.harmonic_rank);
        rank("pagerank", signals.pagerank_rank);
        put("ranks", Value::Object(ranks));
        let mut wikidata = Map::new();
        if let Some(about) = &self.about {
            wikidata.insert("about".into(), json!(about));
        }
        if let Some(country) = &self.country {
            wikidata.insert("country".into(), json!(country));
        }
        if signals.official_site {
            wikidata.insert("official_site".into(), json!(true));
        }
        if signals.sitelinks > 0 {
            wikidata.insert("sitelinks".into(), json!(signals.sitelinks));
        }
        put("wikidata", Value::Object(wikidata));
        if signals.fact_checks > 0 {
            put(
                "fact_checks",
                json!({"checked": signals.fact_checks, "agreed": signals.fact_agrees}),
            );
        }
        put("intro", json!(self.intro));
        put("summary", json!(self.summary));
        put("crawled", json!(self.crawled_at.and_then(timestamp)));
        if self.crawl_version > 0 {
            put("crawl_version", json!(self.crawl_version));
        }
        if let Some(redirect) = self.redirect.as_ref().filter(|r| is_domain(&r.to)) {
            if let Some(at) = timestamp(redirect.at) {
                let mut object = json!({"to": redirect.to, "at": at});
                if redirect.inner {
                    object["inner"] = json!(true);
                }
                put("redirect", object);
            }
        }
        fields
    }
}

fn edge(rel: &str, to: Ref, text: Option<String>) -> Edge {
    Edge {
        rel: rel.into(),
        to,
        text,
        w: None,
        other: BTreeMap::new(),
    }
}

/// `url` in canonical form, `None` when it has none. A URL whose canonical
/// form canonicalizes differently again (`%%41`) is left out too, since
/// the format would refuse it.
fn canonical_owned(url: String) -> Option<String> {
    if plainly_canonical(&url) {
        return Some(url);
    }
    let canonical = canonical_url(&url)?;
    // Canonical already, as most are: no need to read it twice.
    (canonical == url || is_canonical(&canonical)).then_some(canonical)
}

/// Whether `url` is a canonical URL ([`canonical_url`]) of the plain shape
/// most are, `https://www.example.com/path?query`, told without reading it
/// as a URL: a lowercase host of plain labels, the last starting with a
/// letter, no port, and a path and query of characters a URL keeps as
/// they are, with no `%`, no dot segments and no fragment. `false` says
/// nothing: the URL may be canonical all the same.
fn plainly_canonical(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let Some(slash) = rest.find('/') else {
        return false;
    };
    let (host, path) = rest.split_at(slash);
    let plain_label = |label: &str| {
        !label.is_empty()
            && !label.starts_with("xn--")
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    let last_starts_with_a_letter = host
        .rsplit('.')
        .next()
        .is_some_and(|last| last.starts_with(|c: char| c.is_ascii_lowercase()));
    let plain = |b: u8| b.is_ascii_alphanumeric() || b"-._~!$&()*+,;=:@/".contains(&b);
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    host.contains('.')
        && host.split('.').all(plain_label)
        && last_starts_with_a_letter
        && !path.contains("/.")
        && path.bytes().all(plain)
        && query.bytes().all(|b| plain(b) || b == b'?')
        && !url.ends_with('?')
}

/// `text` normalized for an item and trimmed, `None` when nothing is left.
/// Text that is so already, as most is, comes back as it is.
pub(crate) fn tidy(text: String) -> Option<String> {
    let clean_already = !text.is_empty()
        && text.trim().len() == text.len()
        && if text.is_ascii() {
            !text.bytes().any(|b| b.is_ascii_control() && b != b'\n')
        } else {
            !text.chars().any(|c| c.is_control() && c != '\n')
                && is_nfc_quick(text.chars()) == IsNormalized::Yes
        };
    if clean_already {
        return Some(text);
    }
    let text = normalize_text(&text);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// `text` as a keyword: [`tidy`], on one line.
fn tidy_keyword(text: String) -> Option<String> {
    if text.contains('\n') {
        tidy(text.replace('\n', " "))
    } else {
        tidy(text)
    }
}

fn tidy_all(list: Vec<String>) -> Vec<String> {
    list.into_iter().filter_map(tidy).collect()
}

fn tidy_keywords(list: Vec<String>) -> Vec<String> {
    list.into_iter().filter_map(tidy_keyword).collect()
}

/// `list` with each text kept the first time it comes.
fn once(list: Vec<String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::with_capacity(list.len());
    for text in list {
        if !kept.contains(&text) {
            kept.push(text);
        }
    }
    kept
}

fn string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn strings(value: Option<&Value>) -> Vec<String> {
    let list = value.and_then(Value::as_array).into_iter().flatten();
    list.filter_map(|value| string(Some(value))).collect()
}

/// A whole number that fits `T`, `None` for anything else.
fn number<T: TryFrom<u64>>(value: Option<&Value>) -> Option<T> {
    T::try_from(value?.as_u64()?).ok()
}

fn too_big(item: &Item) -> bool {
    serde_json::to_vec(item).map_or(true, |json| json.len() > MAX_ITEM_BYTES)
}

/// `unix` seconds as a UTC timestamp, `YYYY-MM-DDTHH:MM:SSZ`, the other
/// way from [`unix_seconds`]; `None` past the year 9999.
fn timestamp(unix: u64) -> Option<String> {
    let (days, seconds) = (i64::try_from(unix / 86_400).ok()?, unix % 86_400);
    // The date `days` after 1970-01-01, in the proleptic Gregorian calendar.
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let of_era = days - era * 146_097;
    let year_of_era = (of_era - of_era / 1_460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * of_year + 2) / 153;
    let day = of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = era * 400 + year_of_era + i64::from(month <= 2);
    let (hour, minute, second) = (seconds / 3_600, seconds / 60 % 60, seconds % 60);
    (year <= 9_999)
        .then(|| format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Headline, Redirect, Signals};

    /// A site with every member a site item carries, and the ones it
    /// leaves out.
    fn full_record() -> SiteRecord {
        SiteRecord {
            domain: "python.org".into(),
            url: Some("https://www.python.org/".into()),
            title: Some("Welcome to Python.org".into()),
            description: Some("The official home of the Python Programming Language".into()),
            headings: vec!["Get Started".into(), "Download".into()],
            body_text: Some("Python is a programming language that lets you work quickly.".into()),
            terms: vec!["python".into(), "download python".into(), "python".into()],
            link_texts: vec![
                LinkText::from_linkers("python", 0x00a1_b2c3_d4e5_f607),
                LinkText::with_count("python.org", 3),
            ],
            aliases: vec!["Python".into(), "Python Software Foundation".into()],
            signals: Signals {
                harmonic_rank: Some(300),
                pagerank_rank: Some(250),
                tranco_rank: Some(512),
                linking_domains: 412,
                official_site: true,
                sitelinks: 150,
                fact_checks: 12,
                fact_agrees: 11,
            },
            crawled_at: Some(1_791_605_520),
            crawl_attempted_at: Some(1_791_605_600),
            country: Some("US".into()),
            kinds: vec!["programming language".into()],
            about: Some("general-purpose programming language".into()),
            intro: Some("Python is a high-level, general-purpose programming language.".into()),
            summary: Some("The home of the Python programming language.".into()),
            search_url: Some("https://www.python.org/search/?q={searchTerms}".into()),
            language: Some("en".into()),
            key_pages: vec![
                KeyPage {
                    label: "Downloads".into(),
                    url: "https://www.python.org/downloads/".into(),
                },
                KeyPage {
                    label: "Docs".into(),
                    url: "https://docs.python.org/3/".into(),
                },
            ],
            links_to: vec!["psf.org".into(), "pypi.org".into()],
            crawl_failures: 2,
            crawl_version: 7,
            gone_at: Some(1_791_000_000),
            redirect: Some(Redirect {
                to: "python.com".into(),
                at: 1_791_605_520,
                inner: true,
            }),
            icon: Some("iVBORw0KGgo=".into()),
            news: vec![Headline {
                title: "Python 3.15 is out".into(),
                url: "https://www.python.org/news/".into(),
                at: 1_791_605_520,
            }],
        }
    }

    /// `record` without what a site item leaves out.
    fn as_carried(mut record: SiteRecord) -> SiteRecord {
        record.crawl_attempted_at = None;
        record.crawl_failures = 0;
        record.gone_at = None;
        record.icon = None;
        record.news = Vec::new();
        record
    }

    fn check_fields(item: &Item) {
        let specs = site_fields();
        for (name, value) in &item.fields {
            let spec = &specs[name];
            spec.field_type
                .check_field(spec.list, value)
                .unwrap_or_else(|err| panic!("{name}: {err:#}"));
        }
    }

    #[test]
    fn a_site_comes_back_from_its_item() {
        let record = full_record();
        let item = record.to_item().unwrap();
        check_fields(&item);
        // Every field the item can hold is used.
        let unused: Vec<_> = site_fields()
            .into_keys()
            .filter(|name| !item.fields.contains_key(name))
            .collect();
        assert!(unused.is_empty(), "{unused:?}");
        assert_eq!(item.fields["crawled"], "2026-10-10T04:12:00Z");
        assert_eq!(
            item.fields["link_texts"][0],
            json!({"text": "python", "count": record.link_texts[0].count, "linkers": "00a1b2c3d4e5f607"})
        );
        assert_eq!(item.weight, Some(412.0));
        assert_eq!(
            item.fields["redirect"],
            json!({"to": "python.com", "at": "2026-10-10T04:12:00Z", "inner": true})
        );
        assert_eq!(SiteRecord::from_item(&item).unwrap(), as_carried(record));

        // Through JSON, as a set carries it.
        let json = serde_json::to_string(&item).unwrap();
        let read: Item = serde_json::from_str(&json).unwrap();
        assert_eq!(
            SiteRecord::from_item(&read).unwrap(),
            as_carried(full_record())
        );
        // As a line, as a batch or a bucket answer carries it.
        let line = full_record().to_line().unwrap();
        assert_eq!(line, json);
        assert_eq!(
            SiteRecord::from_line(&line).unwrap(),
            as_carried(full_record())
        );
    }

    #[test]
    fn a_line_is_read_only_as_a_whole_site_that_keeps_the_format_s_rules() {
        let line = |value: Value| SiteRecord::from_line(&value.to_string());
        assert!(line(json!({"id": "example.com", "kind": "site"})).is_ok());
        // A site record of its own, as nodes before v0.4.0 sent them.
        let old = serde_json::to_string(&SiteRecord::new("example.com")).unwrap();
        assert!(SiteRecord::from_line(&old).is_err());
        assert!(line(json!({"id": "example.com", "kind": "page"})).is_err());
        assert!(line(json!({"id": "example.com", "extends": "site:example.com"})).is_err());
        let not_canonical =
            json!({"id": "example.com", "kind": "site", "url": "HTTPS://example.com"});
        assert!(line(not_canonical).is_err());
        assert!(SiteRecord::from_line("not json").is_err());
    }

    #[test]
    fn a_line_says_its_site_and_crawl_time_without_being_read_whole() {
        let line = full_record().to_line().unwrap();
        let crawled = full_record().crawled_at;
        assert_eq!(crawl_of_line(&line), Some(("python.org".into(), crawled)));
        let never = SiteRecord::new("example.com").to_line().unwrap();
        assert_eq!(crawl_of_line(&never), Some(("example.com".into(), None)));
        assert_eq!(
            crawl_of_line(r#"{"icon_of": "example.com", "png": ""}"#),
            None
        );
        assert_eq!(
            crawl_of_line(r#"{"id": "example.com", "kind": "page"}"#),
            None
        );
    }

    #[test]
    fn a_site_never_crawled_comes_back_with_its_ranks_and_names() {
        let mut record = SiteRecord::new("example.com");
        record.signals.tranco_rank = Some(90_000);
        record.add_alias("Example");
        let item = record.to_item().unwrap();
        assert_eq!(item.url, None);
        assert_eq!(item.site.as_deref(), Some("example.com"));
        check_fields(&item);
        assert_eq!(SiteRecord::from_item(&item).unwrap(), record);
        assert_eq!(
            SiteRecord::from_item(&SiteRecord::new("example.com").to_item().unwrap()).unwrap(),
            SiteRecord::new("example.com")
        );
    }

    #[test]
    fn text_and_urls_are_made_fit_for_an_item() {
        let mut record = SiteRecord::new("example.com");
        // Decomposed é, a tab, and a URL with a default port.
        record.title = Some("Cafe\u{301}\tMenu".into());
        record.url = Some("https://example.com:443/a/../b".into());
        record.body_text = Some("word ".repeat(400));
        record.language = Some("not a language".into());
        record.terms = vec!["two\nlines".into(), " ".into()];
        record.key_pages = vec![
            KeyPage {
                label: "Menu".into(),
                url: "https://example.com/menu#top".into(),
            },
            // Canonical form comes out `%Ab`, which is not canonical.
            KeyPage {
                label: "Odd".into(),
                url: "https://example.com/%%41b".into(),
            },
            KeyPage {
                label: " ".into(),
                url: "https://example.com/blank".into(),
            },
        ];
        record.links_to = vec!["foo_bar.blogspot.com".into(), "example.org".into()];
        let item = record.to_item().unwrap();
        let back: Item = serde_json::from_str(&serde_json::to_string(&item).unwrap()).unwrap();
        let back = SiteRecord::from_item(&back).unwrap();
        assert_eq!(
            back.key_pages,
            [KeyPage {
                label: "Menu".into(),
                url: "https://example.com/menu".into(),
            }]
        );
        assert_eq!(back.links_to, ["example.org"]);
        assert_eq!(item.title.as_deref(), Some("Caf\u{e9} Menu"));
        assert_eq!(item.url.as_deref(), Some("https://example.com/b"));
        assert_eq!(item.text.as_ref().unwrap().chars().count(), 999);
        assert_eq!(item.lang, None);
        assert_eq!(item.fields["terms"], json!(["two lines"]));

        record.url = Some("https://example.com/%%41b".into());
        assert_eq!(record.to_item().unwrap().url, None);
        // A host the format cannot name as a site is no item's site.
        let item = SiteRecord::new("foo_bar.blogspot.com").to_item().unwrap();
        assert_eq!(item.site, None);
    }

    #[test]
    fn a_clean_record_is_in_its_item_s_form_and_comes_back_from_it() {
        let mut record = full_record();
        record.url = Some("https://www.python.org/?".into());
        record.title = Some(" Welcome to\u{7}Python.org\n".into());
        // Decomposed: the same name and link text as the composed ones.
        record.aliases.push("Caf\u{e9}".into());
        record.aliases.push("Cafe\u{301}".into());
        record
            .link_texts
            .push(LinkText::from_linkers("Caf\u{e9}", 0b01));
        record
            .link_texts
            .push(LinkText::from_linkers("Cafe\u{301}", 0b10));
        record.kinds.push(" programming language\n".into());
        record.body_text = Some(format!("{}\u{0}", "word ".repeat(400)));
        record.language = Some("en_US".into());
        record.key_pages[0].url = "https://www.python.org/downloads/#top".into();
        record.key_pages.push(KeyPage {
            label: "Odd".into(),
            url: "https://www.python.org/%%41b".into(),
        });
        record
            .links_to
            .extend(["foo_bar.blogspot.com".into(), "index_.shtml".into()]);
        record.redirect.as_mut().unwrap().to = "WWW.Python.COM".into();

        let mut clean = record.clone();
        clean.clean();
        assert_eq!(clean.url.as_deref(), Some("https://www.python.org/"));
        assert_eq!(clean.title.as_deref(), Some("Welcome to Python.org"));
        assert_eq!(clean.aliases[2..], ["Caf\u{e9}"]);
        let cafe = clean.link_texts.iter().find(|lt| lt.text == "Caf\u{e9}");
        assert_eq!(cafe.unwrap().linkers, 0b11);
        assert_eq!(clean.link_texts.len(), 3);
        assert_eq!(clean.kinds, ["programming language"]);
        // The page text is not cut.
        assert_eq!(clean.body_text.as_ref().unwrap().len(), 1_999);
        assert_eq!(clean.language, None);
        assert_eq!(clean.key_pages, full_record().key_pages);
        assert_eq!(clean.links_to, full_record().links_to);
        assert_eq!(clean.redirect, full_record().redirect);
        // What the item leaves out stays as it was.
        let marks = |r: &SiteRecord| (r.crawl_attempted_at, r.crawl_failures, r.gone_at);
        assert_eq!(marks(&clean), marks(&record));
        assert_eq!((&clean.icon, &clean.news), (&record.icon, &record.news));

        let mut again = clean.clone();
        again.clean();
        assert_eq!(again, clean);

        let item = record.to_item().unwrap();
        assert_eq!(item, clean.to_item().unwrap());
        let mut back = as_carried(clean.clone());
        back.body_text = item.text.clone();
        assert_eq!(SiteRecord::from_item(&item).unwrap(), back);

        let mut set = crate::RecordSet::new();
        set.upsert(record);
        assert_eq!(set.get("python.org"), Some(&clean));

        let mut late = SiteRecord::new("example.com");
        late.crawled_at = Some(253_402_300_800);
        late.clean();
        assert_eq!(late, SiteRecord::new("example.com"));
        // A redirect stays, for folding, to a site no item can name too.
        let mut underscore = SiteRecord::new("example.com");
        let redirect = Redirect {
            to: "foo_bar.com".into(),
            at: 1_791_605_520,
            inner: false,
        };
        underscore.redirect = Some(redirect.clone());
        underscore.clean();
        assert_eq!(underscore.redirect, Some(redirect));
        assert!(!underscore
            .to_item()
            .unwrap()
            .fields
            .contains_key("redirect"));
    }

    #[test]
    fn a_url_told_canonical_without_reading_it_is_canonical() {
        let plain = [
            "https://www.python.org/",
            "http://example.com/a/b.html",
            "https://example.com/search?q=a+b&lang=en",
            "https://example.com/a?b?c",
            "https://a-b.example.co.uk/~me/(1)/x",
            "https://example.com/@user:1;x=y,z*!$",
            "https://example.com//two",
            "https://1example.com/",
        ];
        let not_told = [
            "https://www.python.org/?",
            "https://www.python.org",
            "HTTPS://www.python.org/",
            "https://WWW.python.org/",
            "https://www.python.org:443/",
            "https://www.python.org:8080/",
            "https://www.python.org/a/../b",
            "https://www.python.org/./b",
            "https://www.python.org/.well-known/x",
            "https://www.python.org/a#top",
            "https://www.python.org/a b",
            "https://www.python.org/%7euser",
            "https://www.python.org/a?it's",
            "https://www.python.org/a\\b",
            "https://www.python.org/a`b{c}",
            "https://xn--mnchen-3ya.de/",
            "https://foo_bar.example.com/",
            "https://example..com/",
            "https://example.com./",
            "https://0x7f.1/",
            "https://127.0.0.1/",
            "https://example.123/",
            "https://user@example.com/",
            "ftp://example.com/",
            "https://example/",
            " https://example.com/",
        ];
        for url in plain {
            assert!(plainly_canonical(url), "{url}");
            assert_eq!(canonical_url(url).as_deref(), Some(url));
        }
        for url in not_told {
            assert!(!plainly_canonical(url), "{url}");
        }
        // What the long way makes of those is what cleaning keeps.
        for url in not_told {
            let canonical = canonical_url(url).filter(|url| is_canonical(url));
            assert_eq!(canonical_owned(url.to_string()), canonical, "{url}");
        }
    }

    #[test]
    fn an_item_too_big_sheds_terms_then_headings_then_link_texts() {
        let mut record = full_record();
        record.terms = (0..100)
            .map(|i| format!("term {i} {}", "x".repeat(150)))
            .collect();
        let item = record.to_item().unwrap();
        assert!(!item.fields.contains_key("terms"));
        assert!(item.fields.contains_key("headings"));

        record.headings = (0..8)
            .map(|i| format!("{i} {}", "y".repeat(1_000)))
            .collect();
        record.link_texts = (0..32)
            .map(|i| LinkText::with_count(format!("{i} {}", "z".repeat(290)), 1))
            .collect();
        let item = record.to_item().unwrap();
        assert!(!item.fields.contains_key("headings"));
        assert!(item.fields.contains_key("link_texts"));
        assert!(item.fields.contains_key("intro"));
    }

    #[test]
    fn items_of_other_kinds_are_not_sites() {
        let item = Item {
            id: "python.org".into(),
            kind: "page".into(),
            ..Item::default()
        };
        assert!(SiteRecord::from_item(&item).is_err());
    }

    #[test]
    fn timestamps_read_back_as_the_same_seconds() {
        assert_eq!(timestamp(0).as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(
            timestamp(951_782_400).as_deref(),
            Some("2000-02-29T00:00:00Z")
        );
        assert_eq!(timestamp(253_402_300_800), None);
        for unix in [
            0,
            59,
            86_399,
            951_782_400,
            1_791_605_520,
            4_102_444_799,
            253_402_300_799,
        ] {
            let text = timestamp(unix).unwrap();
            assert_eq!(unix_seconds(&text), Some(unix as i64), "{text}");
        }
    }
}
