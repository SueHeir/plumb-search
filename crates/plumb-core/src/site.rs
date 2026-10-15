//! A site as its item ([`crate::site_item`]), the way a node keeps it
//! ([`crate::stored`]), read and changed a part at a time. Code that works
//! on sites moves off [`SiteRecord`] onto items (step 4 of the data format
//! plan), and this is what it uses in its place: each part of a site has a
//! function that reads it and one that sets it, and merging, cleaning,
//! scoring and sending a site ([`to_item`]) work on its item as they do on
//! a record.
//!
//! The item's own members hold the homepage's `url`, `title`, description
//! (`about`), page text (`text`), language (`lang`) and the site's other
//! names (`names`), and code reads and sets those directly. The rest are
//! here: how many sites link to it (`weight`, in [`signals`]), its key
//! pages and the sites it links to (edges), the fields of
//! [`crate::site_item::site_fields`], and the node's own, which are never
//! sent: its crawl marks ([`CrawlMarks`]), the site's icon and its headlines.
//!
//! Setting a part to nothing (`None`, an empty list, zero) takes it out of
//! the item, so an item holds only what says something, as
//! [`SiteRecord::to_stored_item`] writes it. A part that is a key of an
//! object field (a rank, a crawl mark) changes only that key, so keys this
//! version does not know stay, as the format keeps them; [`clean`] drops
//! them. Reading never fails: a part that is not there, or does not read,
//! is nothing.
//!
//! A node's sites together, by domain, are a [`crate::sites::Sites`],
//! which does with these functions what [`crate::RecordSet`] does with
//! records.

use anyhow::Result;
use plumb_format::refs::is_domain;
use plumb_format::value::{is_language, unix_seconds};
use plumb_format::{Item, Ref};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::site_item::{
    canonical_owned, edge, number, once, sent_item, string, strings, tidy, tidy_all, tidy_keyword,
    tidy_keywords, timestamp, SITE,
};
#[cfg(doc)]
use crate::SiteRecord;
use crate::{
    canonical_domain, collapse_whitespace, is_generic_kind, is_stand_in_homepage, kind_key,
    linker_bit, min_some, normalize_text, registrable_domain, site_language,
    sort_and_cap_link_texts, truncate_chars, AdultLevel, Headline, KeyPage, LinkText, Redirect,
    Signals, MAX_ALIASES, MAX_KEY_PAGES, MAX_KINDS, MAX_LINKS_TO, MAX_TEXT_CHARS,
};

/// A new site's item, holding nothing but its domain, which must already
/// be canonical ([`canonical_domain`]).
pub fn new(domain: impl Into<String>) -> Item {
    Item {
        id: domain.into(),
        kind: SITE.into(),
        ..Item::default()
    }
}

/// The site's registrable domain: its item's id.
pub fn domain(site: &Item) -> &str {
    &site.id
}

/// The homepage's section headings ([`SiteRecord::headings`]).
pub fn headings(site: &Item) -> Vec<String> {
    strings(site.fields.get("headings"))
}

/// Sets [`headings`].
pub fn set_headings(site: &mut Item, headings: Vec<String>) {
    put(site, "headings", json!(headings));
}

/// Words the homepage is about ([`SiteRecord::terms`]).
pub fn terms(site: &Item) -> Vec<String> {
    strings(site.fields.get("terms"))
}

/// Sets [`terms`].
pub fn set_terms(site: &mut Item, terms: Vec<String>) {
    put(site, "terms", json!(terms));
}

/// What the site is ("bank"), from Wikidata ([`SiteRecord::kinds`]).
pub fn kinds(site: &Item) -> Vec<String> {
    strings(site.fields.get("kinds"))
}

/// Sets [`kinds`].
pub fn set_kinds(site: &mut Item, kinds: Vec<String>) {
    put(site, "kinds", json!(kinds));
}

/// The site's own search, as a URL template ([`SiteRecord::search_url`]).
pub fn search_url(site: &Item) -> Option<&str> {
    field_str(site, "search_url")
}

/// Sets [`search_url`].
pub fn set_search_url(site: &mut Item, search_url: Option<String>) {
    put(site, "search_url", json!(search_url));
}

/// [`SiteRecord::intro`].
pub fn intro(site: &Item) -> Option<&str> {
    field_str(site, "intro")
}

/// Sets [`intro`].
pub fn set_intro(site: &mut Item, intro: Option<String>) {
    put(site, "intro", json!(intro));
}

/// [`SiteRecord::summary`].
pub fn summary(site: &Item) -> Option<&str> {
    field_str(site, "summary")
}

/// Sets [`summary`].
pub fn set_summary(site: &mut Item, summary: Option<String>) {
    put(site, "summary", json!(summary));
}

/// What Wikidata says the site is about ([`SiteRecord::about`]), unlike the
/// item's `about`, which is the homepage's description.
pub fn wikidata_about(site: &Item) -> Option<&str> {
    member(site, "wikidata", "about").and_then(Value::as_str)
}

/// Sets [`wikidata_about`].
pub fn set_wikidata_about(site: &mut Item, about: Option<String>) {
    set_member(site, "wikidata", "about", about.map(Value::String));
}

/// The country the site is of ([`SiteRecord::country`]).
pub fn country(site: &Item) -> Option<&str> {
    member(site, "wikidata", "country").and_then(Value::as_str)
}

/// Sets [`country`].
pub fn set_country(site: &mut Item, country: Option<String>) {
    set_member(site, "wikidata", "country", country.map(Value::String));
}

/// The site's icon, as a crawl another node shared brings it
/// ([`SiteRecord::icon`]).
pub fn icon(site: &Item) -> Option<&str> {
    field_str(site, "icon")
}

/// Sets [`icon`].
pub fn set_icon(site: &mut Item, icon: Option<String>) {
    put(site, "icon", json!(icon));
}

/// The site's headlines, as a crawl another node shared brings them
/// ([`SiteRecord::news`]).
pub fn news(site: &Item) -> Vec<Headline> {
    let list = site.fields.get("news").and_then(Value::as_array);
    list.into_iter()
        .flatten()
        .filter_map(|headline| Headline::deserialize(headline).ok())
        .collect()
}

/// Sets [`news`].
pub fn set_news(site: &mut Item, news: Vec<Headline>) {
    put(site, "news", json!(news));
}

/// The site's ranks and the counts that score it ([`crate::link_score`]):
/// the item's `weight` (sites linking to it) and its `ranks`, `wikidata`
/// and `fact_checks` fields.
pub fn signals(site: &Item) -> Signals {
    let rank = |key: &str| member(site, "ranks", key);
    let wikidata = |key: &str| member(site, "wikidata", key);
    let facts = |key: &str| member(site, "fact_checks", key);
    Signals {
        linking_domains: site.weight.map_or(0, |weight| weight as u32),
        tranco_rank: number(rank("tranco")),
        harmonic_rank: number(rank("harmonic")),
        pagerank_rank: number(rank("pagerank")),
        official_site: wikidata("official_site") == Some(&Value::Bool(true)),
        sitelinks: number(wikidata("sitelinks")).unwrap_or_default(),
        fact_checks: number(facts("checked")).unwrap_or_default(),
        fact_agrees: number(facts("agreed")).unwrap_or_default(),
    }
}

/// Sets what [`signals`] reads, leaving the other keys of the fields it
/// shares (the `wikidata` field's about and country among them).
pub fn set_signals(site: &mut Item, signals: &Signals) {
    site.weight = (signals.linking_domains > 0).then(|| f64::from(signals.linking_domains));
    for (key, rank) in [
        ("tranco", signals.tranco_rank.map(u64::from)),
        ("harmonic", signals.harmonic_rank),
        ("pagerank", signals.pagerank_rank),
    ] {
        set_member(site, "ranks", key, rank.map(|rank| json!(rank)));
    }
    let official = signals.official_site.then_some(Value::Bool(true));
    set_member(site, "wikidata", "official_site", official);
    let sitelinks = (signals.sitelinks > 0).then(|| json!(signals.sitelinks));
    set_member(site, "wikidata", "sitelinks", sitelinks);
    // Both counts or neither, as a fact check says how many agreed of how
    // many were checked.
    let facts = signals.fact_checks > 0 || signals.fact_agrees > 0;
    let checked = facts.then(|| json!(signals.fact_checks));
    set_member(site, "fact_checks", "checked", checked);
    let agreed = facts.then(|| json!(signals.fact_agrees));
    set_member(site, "fact_checks", "agreed", agreed);
}

/// When the homepage was last read, in unix seconds
/// ([`SiteRecord::crawled_at`]).
pub fn crawled(site: &Item) -> Option<u64> {
    field_str(site, "crawled")
        .and_then(unix_seconds)
        .and_then(|unix| u64::try_from(unix).ok())
}

/// Sets [`crawled`]; a time past the year 9999 is none.
pub fn set_crawled(site: &mut Item, at: Option<u64>) {
    put(site, "crawled", json!(at.and_then(timestamp)));
}

/// [`SiteRecord::crawl_version`].
pub fn crawl_version(site: &Item) -> u32 {
    number(site.fields.get("crawl_version")).unwrap_or_default()
}

/// Sets [`crawl_version`].
pub fn set_crawl_version(site: &mut Item, version: u32) {
    put(
        site,
        "crawl_version",
        json!((version > 0).then_some(version)),
    );
}

/// A node's own marks of its crawls of a site, in the `crawl` field of the
/// item it stores, in unix seconds: `{"tried": 1791605600, "failures": 2,
/// "gone": 1791000000}`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrawlMarks {
    /// The last try ([`SiteRecord::crawl_attempted_at`]).
    pub tried: Option<u64>,
    /// Failures in a row up to it ([`SiteRecord::crawl_failures`]).
    pub failures: u32,
    /// When the site was judged gone ([`SiteRecord::gone_at`]).
    pub gone: Option<u64>,
}

/// The node's marks of its crawls of the site.
pub fn crawl_marks(site: &Item) -> CrawlMarks {
    let mark = |key: &str| member(site, "crawl", key);
    CrawlMarks {
        tried: mark("tried").and_then(Value::as_u64),
        failures: number(mark("failures")).unwrap_or_default(),
        gone: mark("gone").and_then(Value::as_u64),
    }
}

/// Sets [`crawl_marks`].
pub fn set_crawl_marks(site: &mut Item, marks: CrawlMarks) {
    set_member(
        site,
        "crawl",
        "tried",
        marks.tried.map(|tried| json!(tried)),
    );
    let failures = (marks.failures > 0).then(|| json!(marks.failures));
    set_member(site, "crawl", "failures", failures);
    set_member(site, "crawl", "gone", marks.gone.map(|gone| json!(gone)));
}

/// The words other sites link to this one with ([`SiteRecord::link_texts`]).
pub fn link_texts(site: &Item) -> Vec<LinkText> {
    let list = site.fields.get("link_texts").and_then(Value::as_array);
    list.into_iter()
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
        .collect()
}

/// Sets [`link_texts`]; `linkers` is written in hex, since a 64-bit number
/// does not survive every JSON reader.
pub fn set_link_texts(site: &mut Item, link_texts: &[LinkText]) {
    let list: Vec<Value> = link_texts
        .iter()
        .map(|link_text| {
            let mut object = json!({"text": link_text.text, "count": link_text.count});
            if link_text.linkers != 0 {
                object["linkers"] = json!(format!("{:016x}", link_text.linkers));
            }
            object
        })
        .collect();
    put(site, "link_texts", Value::Array(list));
}

/// Where the homepage sent the crawler, when it went to another site
/// ([`SiteRecord::redirect`]). A stored item's may name a site an item sent
/// to other nodes cannot, one with a `_`.
pub fn redirect(site: &Item) -> Option<Redirect> {
    let redirect = site.fields.get("redirect")?;
    Some(Redirect {
        to: string(redirect.get("to"))?,
        at: redirect
            .get("at")
            .and_then(Value::as_str)
            .and_then(unix_seconds)
            .and_then(|unix| u64::try_from(unix).ok())?,
        inner: redirect.get("inner") == Some(&Value::Bool(true)),
    })
}

/// Sets [`redirect`]; one past the year 9999 is none.
pub fn set_redirect(site: &mut Item, redirect: Option<Redirect>) {
    let value = redirect.and_then(|redirect| {
        let mut object = json!({"to": redirect.to, "at": timestamp(redirect.at)?});
        if redirect.inner {
            object["inner"] = json!(true);
        }
        Some(object)
    });
    put(site, "redirect", value.unwrap_or_default());
}

/// The homepage's key pages, its `key_page` edges with their labels
/// ([`SiteRecord::key_pages`]).
pub fn key_pages(site: &Item) -> Vec<KeyPage> {
    let pages = site.edges.iter().filter(|edge| edge.rel == "key_page");
    pages
        .filter_map(|edge| match (&edge.to, &edge.text) {
            (Ref::Url(url), Some(label)) => Some(KeyPage {
                label: label.clone(),
                url: url.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// Sets [`key_pages`], ahead of the item's other edges.
pub fn set_key_pages(site: &mut Item, pages: Vec<KeyPage>) {
    let others = std::mem::take(&mut site.edges)
        .into_iter()
        .filter(|edge| edge.rel != "key_page");
    site.edges = pages
        .into_iter()
        .map(|page| edge("key_page", Ref::Url(page.url), Some(page.label)))
        .chain(others)
        .collect();
}

/// The sites the homepage links to, its `links_to` edges
/// ([`SiteRecord::links_to`]).
pub fn links_to(site: &Item) -> Vec<String> {
    let links = site.edges.iter().filter(|edge| edge.rel == "links_to");
    links
        .filter_map(|edge| match &edge.to {
            Ref::Site(site) => Some(site.clone()),
            _ => None,
        })
        .collect()
}

/// Sets [`links_to`], after the item's other edges.
pub fn set_links_to(site: &mut Item, sites: Vec<String>) {
    site.edges.retain(|edge| edge.rel != "links_to");
    let links = sites
        .into_iter()
        .map(|to| edge("links_to", Ref::Site(to), None));
    site.edges.extend(links);
}

/// [`SiteRecord::written_language`].
pub fn written_language(site: &Item) -> Option<String> {
    let text = format!(
        "{} {}",
        site.title.as_deref().unwrap_or_default(),
        site.about.as_deref().unwrap_or_default()
    );
    site_language(site.lang.as_deref(), &text)
}

/// The site's popularity prior ([`crate::link_score`]).
pub fn link_score(site: &Item) -> f32 {
    crate::link_score(&signals(site))
}

/// The country the site belongs to ([`crate::site_country`]): Wikidata's
/// ([`country`]) when it has one, else its domain ending's.
pub fn site_country(site: &Item) -> Option<String> {
    crate::country::country_of(country(site), &site.id)
}

/// How adult the site looks ([`crate::record_adult_level`]): by its kinds,
/// and by its title, description, Wikidata's description and other names.
pub fn adult_level(site: &Item) -> AdultLevel {
    let texts = [
        site.title.as_deref(),
        site.about.as_deref(),
        wikidata_about(site),
    ]
    .into_iter()
    .flatten()
    .chain(site.names.iter().map(String::as_str));
    crate::safe::site_adult_level(&site.id, &kinds(site), texts)
}

/// [`SiteRecord::add_link_text`].
pub fn add_link_text(site: &mut Item, text: &str, linking_domain: &str) {
    add_link_text_linkers(site, text, linker_bit(linking_domain));
}

/// [`SiteRecord::add_link_text_linkers`].
pub fn add_link_text_linkers(site: &mut Item, text: &str, linkers: u64) {
    let text = truncate_chars(&normalize_text(text), MAX_TEXT_CHARS);
    if text.is_empty() || linkers == 0 {
        return;
    }
    let mut link_texts = link_texts(site);
    absorb(&mut link_texts, LinkText::from_linkers(text, linkers));
    sort_and_cap_link_texts(&mut link_texts);
    set_link_texts(site, &link_texts);
}

/// [`SiteRecord::add_alias`], to the item's `names`, with the text tidied
/// as [`clean`] would.
pub fn add_alias(site: &mut Item, alias: &str) {
    let Some(alias) = tidy(truncate_chars(&collapse_whitespace(alias), MAX_TEXT_CHARS)) else {
        return;
    };
    let key = normalize_text(&alias);
    if key.is_empty() || site.names.len() >= MAX_ALIASES {
        return;
    }
    if site.names.iter().any(|name| normalize_text(name) == key) {
        return;
    }
    site.names.push(alias);
}

/// [`SiteRecord::add_kind`], with the text tidied as [`clean`] would.
pub fn add_kind(site: &mut Item, kind: &str) {
    let mut kinds = kinds(site);
    if push_kind(&mut kinds, kind) {
        set_kinds(site, kinds);
    }
}

/// Adds `kind` to `kinds` as [`SiteRecord::add_kind`] does; whether it did.
fn push_kind(kinds: &mut Vec<String>, kind: &str) -> bool {
    let kind = truncate_chars(&collapse_whitespace(kind), MAX_TEXT_CHARS);
    let Some(kind) = tidy_keyword(kind) else {
        return false;
    };
    if kinds.len() >= MAX_KINDS || is_generic_kind(&kind) {
        return false;
    }
    let key = kind_key(&kind);
    if kinds.iter().any(|known| kind_key(known) == key) {
        return false;
    }
    kinds.push(kind);
    true
}

/// Adds `link_text` to `link_texts`, counted with the one of the same text.
fn absorb(link_texts: &mut Vec<LinkText>, link_text: LinkText) {
    match link_texts
        .iter_mut()
        .find(|kept| kept.text == link_text.text)
    {
        Some(kept) => kept.absorb(&link_text),
        None => link_texts.push(link_text),
    }
}

/// Folds another item of the same site into this one, by the rules of
/// [`SiteRecord::merge`]: the fresher crawl's page, a redirect only when no
/// successful crawl came after it, names, kinds and link texts together,
/// the best ranks, and a gone site changed only in its ranks and crawl
/// marks until a crawl after it went reaches it. The icon and headlines are
/// this item's, and so are keys of its fields this version does not know.
pub fn merge(site: &mut Item, mut other: Item) {
    debug_assert_eq!(site.id, other.id);
    let mut scores = signals(site);
    merge_signals(&mut scores, &signals(&other));
    let mut marks = crawl_marks(site);
    merge_attempts(&mut marks, crawl_marks(&other));
    let their_crawl = crawled(&other);
    if let Some(gone) = marks.gone {
        if their_crawl.is_none_or(|at| at <= gone) {
            // Nothing reached it since it went: only its crawl marks and
            // ranks change, so it stays small and stays out.
            set_signals(site, &scores);
            set_crawl_marks(site, marks);
            return;
        }
        // Reached again: it is back, and the fresh crawl fills it in.
        marks.gone = None;
    }
    let my_crawl = crawled(site);
    if their_crawl.is_some() && their_crawl >= my_crawl {
        site.url = other.url.take().or(site.url.take());
        site.title = other.title.take().or(site.title.take());
        site.about = other.about.take().or(site.about.take());
        site.lang = other.lang.take().or(site.lang.take());
        // A fresh crawl that found no search form, or no headings, means
        // the site has none now.
        site.text = other.text.take();
        for name in [
            "search_url",
            "headings",
            "terms",
            "crawl_version",
            "crawled",
        ] {
            match other.fields.remove(name) {
                Some(value) => site.fields.insert(name.into(), value),
                None => site.fields.remove(name),
            };
        }
        set_key_pages(site, key_pages(&other));
        set_links_to(site, links_to(&other));
    } else {
        site.url = site.url.take().or(other.url.take());
        site.title = site.title.take().or(other.title.take());
        site.about = site.about.take().or(other.about.take());
        site.lang = site.lang.take().or(other.lang.take());
        if my_crawl.is_none() {
            site.text = site.text.take().or(other.text.take());
            for name in ["search_url", "headings", "terms"] {
                fill(site, &mut other, name);
            }
            if key_pages(site).is_empty() {
                set_key_pages(site, key_pages(&other));
            }
            if links_to(site).is_empty() {
                set_links_to(site, links_to(&other));
            }
            set_crawl_version(site, crawl_version(&other));
        }
    }
    // The latest crawl decides: a redirect seen after the last successful
    // crawl stands, a successful crawl after it ends it.
    let redirect = match (redirect(site), redirect(&other)) {
        (Some(mine), Some(theirs)) => Some(if theirs.at >= mine.at { theirs } else { mine }),
        (mine, theirs) => mine.or(theirs),
    };
    let crawled_now = crawled(site);
    let redirect = redirect.filter(|redirect| crawled_now.is_none_or(|at| at <= redirect.at));
    set_redirect(site, redirect);
    if country(site).is_none() {
        set_country(site, country(&other).map(str::to_string));
    }
    if wikidata_about(site).is_none() {
        set_wikidata_about(site, wikidata_about(&other).map(str::to_string));
    }
    fill(site, &mut other, "intro");
    fill(site, &mut other, "summary");
    let their_kinds = kinds(&other);
    if !their_kinds.is_empty() {
        let mut kinds = kinds(site);
        let mut added = false;
        for kind in &their_kinds {
            added |= push_kind(&mut kinds, kind);
        }
        if added {
            set_kinds(site, kinds);
        }
    }
    let their_link_texts = link_texts(&other);
    if !their_link_texts.is_empty() {
        let mut link_texts = link_texts(site);
        for link_text in their_link_texts {
            absorb(&mut link_texts, link_text);
        }
        sort_and_cap_link_texts(&mut link_texts);
        set_link_texts(site, &link_texts);
    }
    for alias in &other.names {
        add_alias(site, alias);
    }
    set_signals(site, &scores);
    set_crawl_marks(site, marks);
}

/// [`merge`] for a crawl another node shared, by the rules of
/// [`SiteRecord::merge_shared`]: what the shared crawl leaves empty (it has
/// no search box, and page text only from trusted crawlers) keeps what
/// this item has rather than being cleared.
pub fn merge_shared(site: &mut Item, other: Item) {
    let mut kept = Map::new();
    for name in ["search_url", "headings", "terms"] {
        if !other.fields.contains_key(name) {
            if let Some(mine) = site.fields.remove(name) {
                kept.insert(name.into(), mine);
            }
        }
    }
    let text = other.text.is_none().then(|| site.text.take()).flatten();
    // Without the page's text, the shared crawl did not read what a newer
    // crawler reads either.
    let version = other.text.is_none().then(|| crawl_version(site));
    let pages = key_pages(&other).is_empty().then(|| key_pages(site));
    if pages.is_some() {
        set_key_pages(site, Vec::new());
    }
    let links = links_to(&other).is_empty().then(|| links_to(site));
    if links.is_some() {
        set_links_to(site, Vec::new());
    }
    merge(site, other);
    for (name, mine) in kept {
        site.fields.entry(name).or_insert(mine);
    }
    site.text = site.text.take().or(text);
    if let Some(mine) = version {
        let version = crawl_version(site).max(mine);
        set_crawl_version(site, version);
    }
    if let Some(mine) = pages {
        if key_pages(site).is_empty() {
            set_key_pages(site, mine);
        }
    }
    if let Some(mine) = links {
        if links_to(site).is_empty() {
            set_links_to(site, mine);
        }
    }
}

/// Ranks keep the best (lowest) value, counts the larger one, and
/// `official_site` is OR-ed ([`SiteRecord::merge`]).
fn merge_signals(signals: &mut Signals, other: &Signals) {
    signals.harmonic_rank = min_some(signals.harmonic_rank, other.harmonic_rank);
    signals.pagerank_rank = min_some(signals.pagerank_rank, other.pagerank_rank);
    signals.tranco_rank = min_some(signals.tranco_rank, other.tranco_rank);
    signals.linking_domains = signals.linking_domains.max(other.linking_domains);
    signals.official_site |= other.official_site;
    signals.sitelinks = signals.sitelinks.max(other.sitelinks);
    if other.fact_checks > signals.fact_checks {
        signals.fact_checks = other.fact_checks;
        signals.fact_agrees = other.fact_agrees.min(other.fact_checks);
    }
}

/// The later try is kept, with the failures counted at it (the larger
/// count when both tried at the same time).
fn merge_attempts(marks: &mut CrawlMarks, other: CrawlMarks) {
    match other.tried.cmp(&marks.tried) {
        std::cmp::Ordering::Greater => marks.failures = other.failures,
        std::cmp::Ordering::Equal => marks.failures = marks.failures.max(other.failures),
        std::cmp::Ordering::Less => {}
    }
    marks.tried = marks.tried.max(other.tried);
}

/// Cuts the site down to a gone one's ([`SiteRecord::make_gone`]): its
/// domain, ranks, last crawl and crawl marks, gone since `at`.
pub fn make_gone(site: &mut Item, at: u64) {
    let signals = signals(site);
    let crawled = crawled(site);
    let marks = CrawlMarks {
        gone: Some(at),
        ..crawl_marks(site)
    };
    *site = new(std::mem::take(&mut site.id));
    set_signals(site, &signals);
    set_crawled(site, crawled);
    set_crawl_marks(site, marks);
}

/// Whether the homepage read was a bot check or another page standing in
/// for the site ([`SiteRecord::is_bot_check`]).
pub fn is_bot_check(site: &Item) -> bool {
    if let Some(url) = site.url.as_deref() {
        let read_elsewhere = registrable_domain(url)
            .is_some_and(|host| canonical_domain(&site.id).is_some_and(|domain| host != domain));
        let suspended = url
            .to_ascii_lowercase()
            .contains("/cgi-sys/suspendedpage.cgi");
        if read_elsewhere || suspended {
            return true;
        }
    }
    is_stand_in_homepage(
        &site.id,
        site.title.as_deref(),
        site.about.as_deref(),
        &headings(site),
        site.text.as_deref(),
    )
}

/// Clears what a bot check put in the item, as
/// [`SiteRecord::drop_bot_check`] does; whether it did.
pub fn drop_bot_check(site: &mut Item) -> bool {
    if !is_bot_check(site) {
        return false;
    }
    site.url = None;
    site.title = None;
    site.about = None;
    site.text = None;
    for name in ["headings", "terms", "search_url", "crawled"] {
        site.fields.remove(name);
    }
    set_key_pages(site, Vec::new());
    set_links_to(site, Vec::new());
    true
}

/// Puts the site in its item's form, as [`SiteRecord::clean`] does a
/// record: text in Unicode NFC, control characters made spaces, trimmed;
/// URLs canonical; only the language tag, key pages, linked sites and
/// times an item can hold; names and kinds once each, and link texts that
/// come out the same counted together. The page text is not cut, and a
/// redirect may go to any site. Cleaning a clean site changes nothing.
///
/// It makes a whole site of the item: its kind is `site`, and what a site
/// does not hold goes, a tombstone's `deleted`, labels and other members,
/// edges but key pages and linked sites, fields and keys of fields this
/// version does not know, and parts that do not read.
pub fn clean(site: &mut Item) {
    let mut clean = new(std::mem::take(&mut site.id));
    clean.url = site.url.take().and_then(canonical_owned);
    clean.title = site.title.take().and_then(tidy);
    clean.about = site.about.take().and_then(tidy);
    clean.text = site.text.take().and_then(tidy);
    clean.lang = site.lang.take().filter(|lang| is_language(lang));
    clean.names = once(tidy_all(std::mem::take(&mut site.names)));
    set_headings(&mut clean, tidy_all(headings(site)));
    set_terms(&mut clean, tidy_keywords(terms(site)));
    set_kinds(&mut clean, once(tidy_keywords(kinds(site))));
    let owned = |text: Option<&str>| text.map(str::to_string);
    set_search_url(&mut clean, owned(search_url(site)).and_then(tidy_keyword));
    let mut link_texts: Vec<LinkText> = Vec::new();
    for mut link_text in self::link_texts(site) {
        if let Some(text) = tidy(link_text.text) {
            link_text.text = text;
            absorb(&mut link_texts, link_text);
        }
    }
    sort_and_cap_link_texts(&mut link_texts);
    set_link_texts(&mut clean, &link_texts);
    set_wikidata_about(&mut clean, owned(wikidata_about(site)).and_then(tidy));
    set_country(&mut clean, owned(country(site)).and_then(tidy_keyword));
    set_intro(&mut clean, owned(intro(site)).and_then(tidy));
    set_summary(&mut clean, owned(summary(site)).and_then(tidy));
    set_signals(&mut clean, &signals(site));
    set_crawled(&mut clean, crawled(site));
    set_crawl_version(&mut clean, crawl_version(site));
    let redirect = redirect(site).map(|mut redirect| {
        if let Some(to) = canonical_domain(&redirect.to) {
            redirect.to = to;
        }
        redirect
    });
    set_redirect(&mut clean, redirect);
    let pages = key_pages(site).into_iter().filter_map(|page| {
        Some(KeyPage {
            url: canonical_owned(page.url)?,
            label: tidy(page.label)?,
        })
    });
    set_key_pages(&mut clean, pages.take(MAX_KEY_PAGES).collect());
    let mut links = links_to(site);
    links.retain(|to| is_domain(to));
    links.truncate(MAX_LINKS_TO);
    set_links_to(&mut clean, links);
    set_crawl_marks(&mut clean, crawl_marks(site));
    set_icon(&mut clean, owned(icon(site)));
    set_news(&mut clean, news(site));
    *site = clean;
}

/// The site as an item to send ([`crate::site_item`]), as
/// [`SiteRecord::to_item`] makes one of a record: of the site as [`clean`]
/// makes it, without the node's own fields (its crawl marks, icon and
/// headlines), with its text cut to the format's 1,000 characters. Errors
/// when the item would not be a valid one, for a domain that is no id or a
/// site too big even after shedding.
pub fn to_item(site: &Item) -> Result<Item> {
    let mut site = site.clone();
    clean(&mut site);
    for name in ["crawl", "icon", "news"] {
        site.fields.remove(name);
    }
    sent_item(site)
}

fn field_str<'a>(site: &'a Item, name: &str) -> Option<&'a str> {
    site.fields.get(name).and_then(Value::as_str)
}

fn member<'a>(site: &'a Item, field: &str, key: &str) -> Option<&'a Value> {
    site.fields.get(field)?.get(key)
}

/// Sets the field `name` to `value`, or takes it out when `value` says
/// nothing (null, or an empty list or object).
fn put(site: &mut Item, name: &str, value: Value) {
    let empty = match &value {
        Value::Null => true,
        Value::Array(list) => list.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    };
    if empty {
        site.fields.remove(name);
    } else {
        site.fields.insert(name.into(), value);
    }
}

/// Sets `key` of the object field `field`, or takes it out with `None`.
fn set_member(site: &mut Item, field: &str, key: &str, value: Option<Value>) {
    let mut object = match site.fields.remove(field) {
        Some(Value::Object(object)) => object,
        _ => Map::new(),
    };
    match value {
        Some(value) => object.insert(key.into(), value),
        None => object.remove(key),
    };
    put(site, field, Value::Object(object));
}

/// Gives `site` the field `name` of `other` when it has none.
fn fill(site: &mut Item, other: &mut Item, name: &str) {
    if !site.fields.contains_key(name) {
        if let Some(value) = other.fields.remove(name) {
            site.fields.insert(name.into(), value);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::site_item::tests::full_record;
    use crate::SiteRecord;

    /// The item [`new`] and the setters make of a record's fields.
    fn from_fields(record: &SiteRecord) -> Item {
        let mut site = new(&record.domain);
        site.url = record.url.clone();
        site.title = record.title.clone();
        site.about = record.description.clone();
        site.text = record.body_text.clone();
        site.lang = record.language.clone();
        site.names = record.aliases.clone();
        // Edges in the other order than the item has them.
        set_links_to(&mut site, record.links_to.clone());
        set_key_pages(&mut site, record.key_pages.clone());
        set_headings(&mut site, record.headings.clone());
        set_terms(&mut site, record.terms.clone());
        set_kinds(&mut site, record.kinds.clone());
        set_search_url(&mut site, record.search_url.clone());
        set_intro(&mut site, record.intro.clone());
        set_summary(&mut site, record.summary.clone());
        set_country(&mut site, record.country.clone());
        set_signals(&mut site, &record.signals);
        set_wikidata_about(&mut site, record.about.clone());
        set_icon(&mut site, record.icon.clone());
        set_news(&mut site, record.news.clone());
        set_crawled(&mut site, record.crawled_at);
        set_crawl_version(&mut site, record.crawl_version);
        set_crawl_marks(
            &mut site,
            CrawlMarks {
                tried: record.crawl_attempted_at,
                failures: record.crawl_failures,
                gone: record.gone_at,
            },
        );
        set_link_texts(&mut site, &record.link_texts);
        set_redirect(&mut site, record.redirect.clone());
        site
    }

    /// The record the getters read off `site`.
    fn to_fields(site: &Item) -> SiteRecord {
        let owned = |text: Option<&str>| text.map(str::to_string);
        let marks = crawl_marks(site);
        SiteRecord {
            domain: domain(site).to_string(),
            url: site.url.clone(),
            title: site.title.clone(),
            description: site.about.clone(),
            body_text: site.text.clone(),
            language: site.lang.clone(),
            aliases: site.names.clone(),
            headings: headings(site),
            terms: terms(site),
            kinds: kinds(site),
            search_url: owned(search_url(site)),
            intro: owned(intro(site)),
            summary: owned(summary(site)),
            about: owned(wikidata_about(site)),
            country: owned(country(site)),
            icon: owned(icon(site)),
            news: news(site),
            signals: signals(site),
            crawled_at: crawled(site),
            crawl_version: crawl_version(site),
            crawl_attempted_at: marks.tried,
            crawl_failures: marks.failures,
            gone_at: marks.gone,
            link_texts: link_texts(site),
            redirect: redirect(site),
            key_pages: key_pages(site),
            links_to: links_to(site),
        }
    }

    /// Clean records of one site that differ in each thing merging reads:
    /// which was crawled last, a crawl that found no search box or page
    /// text, a gone site, redirects before and after a crawl, a try at the
    /// same time, and names, kinds, link texts and ranks of their own.
    pub(crate) fn variants() -> Vec<SiteRecord> {
        let full = full_record();
        let mut never = SiteRecord::new("python.org");
        never.signals.tranco_rank = Some(900);
        never.aliases = vec!["CPython".into(), "python".into()];
        never.kinds = vec!["software".into(), "programming languages".into()];
        never.about = Some("a programming language".into());
        let mut older = full.clone();
        older.crawled_at = Some(1_791_000_100);
        older.crawl_attempted_at = Some(1_791_000_100);
        older.crawl_failures = 0;
        older.gone_at = None;
        older.redirect = None;
        older.title = Some("Python".into());
        older.search_url = None;
        older.headings.clear();
        older.body_text = None;
        older.key_pages.clear();
        older.links_to.clear();
        older.country = None;
        older.intro = None;
        older.link_texts = vec![
            LinkText::from_linkers("python", 0x10),
            LinkText::with_count("py", 9),
        ];
        older.signals = Signals {
            tranco_rank: Some(400),
            linking_domains: 500,
            fact_checks: 20,
            fact_agrees: 3,
            ..Signals::default()
        };
        let mut gone = SiteRecord::new("python.org");
        gone.crawled_at = Some(1_791_605_520);
        gone.crawl_attempted_at = Some(1_791_700_000);
        gone.crawl_failures = 5;
        gone.gone_at = Some(1_791_700_000);
        gone.signals.harmonic_rank = Some(100);
        let mut newer = SiteRecord::new("python.org");
        newer.url = Some("https://python.org/".into());
        newer.title = Some("Python".into());
        newer.headings = vec!["News".into()];
        newer.body_text = Some("Python 3.15".into());
        newer.crawled_at = Some(1_791_800_000);
        newer.crawl_version = 8;
        newer.crawl_attempted_at = Some(1_791_900_000);
        newer.crawl_failures = 1;
        newer.redirect = Some(Redirect {
            to: "foo_bar.com".into(),
            at: 1_791_900_000,
            inner: false,
        });
        newer.kinds = vec!["website".into()];
        let mut shared = newer.clone();
        shared.body_text = None;
        shared.search_url = None;
        shared.headings.clear();
        shared.redirect = None;
        shared.links_to = vec!["example.com".into()];
        shared.icon = Some("R0lGODlh".into());
        shared.news = vec![Headline {
            title: "Python 3.15".into(),
            url: "https://www.python.org/news/315".into(),
            at: 1_791_800_000,
        }];
        let mut same_try = older.clone();
        same_try.crawl_attempted_at = full.crawl_attempted_at;
        same_try.crawl_failures = 4;
        same_try.crawled_at = None;
        let mut variants = vec![full, never, older, gone, newer, shared, same_try];
        for record in &mut variants {
            record.clean();
        }
        variants
    }

    #[test]
    fn a_site_s_parts_are_its_record_s_fields() {
        for record in variants() {
            let item = record.to_stored_item();
            assert_eq!(from_fields(&record), item, "{record:?}");
            assert_eq!(to_fields(&item), record);
        }
        let empty = SiteRecord::new("example.com");
        assert_eq!(from_fields(&empty), new("example.com"));
        assert_eq!(from_fields(&empty), empty.to_stored_item());
    }

    #[test]
    fn setting_a_part_to_nothing_takes_it_out() {
        let record = full_record();
        let mut site = record.to_stored_item();
        set_headings(&mut site, Vec::new());
        set_search_url(&mut site, None);
        set_country(&mut site, None);
        set_wikidata_about(&mut site, None);
        set_signals(&mut site, &Signals::default());
        set_crawl_marks(&mut site, CrawlMarks::default());
        set_redirect(&mut site, None);
        set_key_pages(&mut site, Vec::new());
        set_links_to(&mut site, Vec::new());
        set_crawled(&mut site, Some(u64::MAX));
        set_redirect(
            &mut site,
            Some(Redirect {
                to: "python.com".into(),
                at: u64::MAX,
                inner: false,
            }),
        );
        for name in [
            "headings",
            "search_url",
            "wikidata",
            "ranks",
            "fact_checks",
            "crawl",
            "redirect",
            "crawled",
        ] {
            assert!(!site.fields.contains_key(name), "{name}");
        }
        assert!(site.edges.is_empty());
        assert_eq!(site.weight, None);
        // What else a field holds stays.
        let mut site = record.to_stored_item();
        set_signals(&mut site, &Signals::default());
        assert_eq!(country(&site), Some("US"));
        assert_eq!(
            wikidata_about(&site),
            Some("general-purpose programming language")
        );
    }

    #[test]
    fn items_merge_as_their_records_do() {
        let variants = variants();
        for (i, mine) in variants.iter().enumerate() {
            for (j, theirs) in variants.iter().enumerate() {
                let mut record = mine.clone();
                record.merge(theirs.clone());
                let mut site = mine.to_stored_item();
                merge(&mut site, theirs.to_stored_item());
                assert_eq!(site, record.to_stored_item(), "merge {i} with {j}");

                let mut record = mine.clone();
                record.merge_shared(theirs.clone());
                let mut site = mine.to_stored_item();
                merge_shared(&mut site, theirs.to_stored_item());
                assert_eq!(site, record.to_stored_item(), "merge_shared {i} with {j}");
            }
        }
    }

    #[test]
    fn an_item_cleans_as_its_record_does() {
        let mut record = full_record();
        record.url = Some("HTTPS://WWW.Python.org/index%2ehtml?".into());
        record.title = Some("  Welcome\u{7} to Python ".into());
        record.description = Some("Dư\u{323} ba\u{301}o".into());
        record.aliases = vec!["Python".into(), " Python".into(), String::new()];
        record.terms = vec!["python\nlanguage".into()];
        record.kinds = vec!["language".into(), "language ".into()];
        record.link_texts = vec![
            LinkText::with_count("python ", 3),
            LinkText::from_linkers("python", 0x3),
            LinkText::with_count("\u{7}", 1),
        ];
        record.country = Some("US\n".into());
        record.key_pages.push(KeyPage {
            label: "Bad".into(),
            url: "not a url".into(),
        });
        record.links_to.push("foo_bar.com".into());
        record.redirect = Some(Redirect {
            to: "Python.COM".into(),
            at: 1_791_605_520,
            inner: false,
        });
        record.language = Some("not a language tag".into());
        let mut site = record.clone().stored_item_as_is();
        // What no site holds goes too.
        site.published = Some("2026-10-15".into());
        site.ids.insert("wd".into(), "Q28865".into());
        site.fields.insert("colour".into(), json!("blue"));
        clean(&mut site);
        assert_eq!(site, record.to_stored_item());
        assert_eq!(redirect(&site).unwrap().to, "python.com");
        let clean_once = site.clone();
        clean(&mut site);
        assert_eq!(site, clean_once);
    }

    #[test]
    fn a_gone_site_and_a_bot_check_are_cut_down_as_records_are() {
        for mut record in variants() {
            let mut site = record.to_stored_item();
            make_gone(&mut site, 1_792_000_000);
            record.make_gone(1_792_000_000);
            assert_eq!(site, record.to_stored_item());
        }

        let mut record = full_record();
        record.url = Some("https://www.google.com/".into());
        record.clean();
        let mut site = record.to_stored_item();
        assert!(is_bot_check(&site));
        assert!(drop_bot_check(&mut site));
        assert!(record.drop_bot_check());
        assert_eq!(site, record.to_stored_item());
        for mut record in variants() {
            let mut site = record.to_stored_item();
            assert_eq!(drop_bot_check(&mut site), record.drop_bot_check());
            assert_eq!(site, record.to_stored_item());
        }
    }

    #[test]
    fn names_kinds_and_link_texts_add_as_on_records() {
        for mut record in variants() {
            let mut site = record.to_stored_item();
            for alias in [
                "Python",
                "PYTHON",
                "Python  Language",
                "",
                "Py\u{7}thon",
                "Dư\u{323} ba\u{301}o",
            ] {
                add_alias(&mut site, alias);
                record.add_alias(alias);
            }
            for kind in [
                "cafe\u{301}",
                "ba\u{7}nk",
                "programming language",
                "website",
                "Programming Languages",
                "film",
            ] {
                add_kind(&mut site, kind);
                record.add_kind(kind);
            }
            add_link_text(&mut site, "Python", "example.com");
            record.add_link_text("Python", "example.com");
            add_link_text_linkers(&mut site, "The Python", 0xff);
            record.add_link_text_linkers("The Python", 0xff);
            add_link_text_linkers(&mut site, "nothing", 0);
            record.add_link_text_linkers("nothing", 0);
            assert_eq!(site, record.to_stored_item());
            assert_eq!(written_language(&site), record.written_language());
            assert_eq!(link_score(&site), record.link_score());
        }
    }

    #[test]
    fn a_site_s_country_and_adult_level_are_its_record_s() {
        let mut records = variants();
        let mut adult = SiteRecord::new("example.fr");
        adult.kinds = vec!["adult website".into()];
        let mut suggestive = SiteRecord::new("example.com");
        suggestive.title = Some("Sexy lingerie".into());
        suggestive.country = Some("uk".into());
        let mut named = SiteRecord::new("example.de");
        named.aliases = vec!["XXX".into()];
        let mut described = SiteRecord::new("example.org");
        described.description = Some("Erotic stories".into());
        described.about = Some("website".into());
        records.extend([adult, suggestive, named, described]);
        for mut record in records {
            record.clean();
            let site = record.to_stored_item();
            assert_eq!(site_country(&site), crate::site_country(&record));
            assert_eq!(adult_level(&site), crate::record_adult_level(&record));
        }
    }

    #[test]
    fn parts_keep_the_keys_this_version_does_not_know() {
        let mut site = full_record().to_stored_item();
        site.fields.get_mut("ranks").unwrap()["majestic"] = json!(7);
        site.fields.get_mut("crawl").unwrap()["queued"] = json!(1);
        site.fields.get_mut("wikidata").unwrap()["since"] = json!("1991");
        set_signals(&mut site, &Signals::default());
        set_crawl_marks(&mut site, CrawlMarks::default());
        assert_eq!(site.fields["ranks"], json!({"majestic": 7}));
        assert_eq!(site.fields["crawl"], json!({"queued": 1}));
        assert_eq!(site.fields["wikidata"]["since"], json!("1991"));
        let mut theirs = full_record().to_stored_item();
        theirs.fields.get_mut("ranks").unwrap()["majestic"] = json!(3);
        merge(&mut site, theirs);
        assert_eq!(site.fields["ranks"]["majestic"], json!(7));
        clean(&mut site);
        for (field, key) in [
            ("ranks", "majestic"),
            ("crawl", "queued"),
            ("wikidata", "since"),
        ] {
            assert!(site.fields[field].get(key).is_none(), "{field}.{key}");
        }
    }

    #[test]
    fn parts_that_do_not_read_are_nothing() {
        let mut site = new("example.com");
        site.fields = serde_json::from_value(json!({
            "headings": "Get Started",
            "ranks": [1, 2],
            "wikidata": "Q28865",
            "crawl": 5,
            "crawled": 1791605520,
            "crawl_version": -1,
            "redirect": {"to": "example.org"},
            "link_texts": [{"text": "good", "count": 2}, {"text": "no count"}, 7,
                           {"text": "bad linkers", "count": 1, "linkers": "xyz"}],
            "news": [{"title": "Kept", "url": "https://example.com/1", "at": 5}, {"title": 1}],
            "icon": 12,
        }))
        .unwrap();
        site.weight = Some(-5.0);
        assert!(headings(&site).is_empty());
        assert_eq!(signals(&site), Signals::default());
        assert_eq!(country(&site), None);
        assert_eq!(crawl_marks(&site), CrawlMarks::default());
        assert_eq!(crawled(&site), None);
        assert_eq!(crawl_version(&site), 0);
        assert_eq!(redirect(&site), None);
        assert_eq!(link_texts(&site), vec![LinkText::with_count("good", 2)]);
        assert_eq!(news(&site).len(), 1);
        assert_eq!(icon(&site), None);
        // Set over, a field that is not an object becomes one.
        set_country(&mut site, Some("US".into()));
        assert_eq!(site.fields["wikidata"], json!({"country": "US"}));
    }

    #[test]
    fn cleaning_makes_a_whole_site_of_any_item() {
        let mut site = full_record().to_stored_item();
        site.kind = "page".into();
        site.deleted = Some(plumb_format::item::Deleted {
            at: "2026-10-15".into(),
            ..Default::default()
        });
        site.labels = vec!["personal".into()];
        site.weight = Some(412.7);
        site.edges
            .push(edge("about", Ref::Site("psf.org".into()), None));
        site.edges
            .push(edge("links_to", Ref::Url("https://psf.org/".into()), None));
        site.edges.push(edge(
            "key_page",
            Ref::Url("https://www.python.org/about/".into()),
            None,
        ));
        clean(&mut site);
        let mut record = full_record();
        record.clean();
        assert_eq!(site, record.to_stored_item());
        assert_eq!(site.kind, SITE);
        assert_eq!(site.deleted, None);
        assert!(site.labels.is_empty());
        assert_eq!(site.weight, Some(412.0));
    }

    #[test]
    fn a_site_is_sent_as_its_record_is() {
        let mut records = variants();
        // Not clean yet, with more text than an item holds and a redirect
        // to a site no item can name.
        let mut untidy = full_record();
        untidy.url = Some("HTTPS://WWW.Python.org/index%2ehtml?".into());
        untidy.title = Some("  Welcome\u{7} to Python ".into());
        untidy.body_text = Some("word ".repeat(400));
        untidy.redirect = Some(Redirect {
            to: "foo_bar.com".into(),
            at: 1_791_605_520,
            inner: false,
        });
        records.push(untidy);
        // Too big until its terms are shed, and too big even so.
        let mut shed = SiteRecord::new("example.com");
        shed.terms = (0..100)
            .map(|i| format!("{i} {}", "x".repeat(200)))
            .collect();
        records.push(shed.clone());
        let mut big = SiteRecord::new("big.example");
        big.aliases = (0..16)
            .map(|i| format!("{i} {}", "x".repeat(1_100)))
            .collect();
        records.push(big);
        records.push(SiteRecord::new("example.org"));
        for record in records {
            let sent = record.to_item();
            for site in [record.to_stored_item(), record.clone().stored_item_as_is()] {
                match (&sent, to_item(&site)) {
                    (Ok(sent), Ok(item)) => assert_eq!(&item, sent, "{}", record.domain),
                    (Err(_), Err(_)) => {}
                    (sent, item) => panic!("{}: {sent:?}, {item:?}", record.domain),
                }
            }
        }
        let shed = to_item(&shed.to_stored_item()).unwrap();
        assert!(!shed.fields.contains_key("terms"));
        assert_eq!(shed.site.as_deref(), Some("example.com"));
    }
}
