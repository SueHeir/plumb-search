//! A site as its item ([`crate::site_item`]), the way a node keeps it
//! ([`crate::stored`]), read and changed a part at a time: each part of a
//! site has a function that reads it and one that sets it, and merging,
//! cleaning, scoring and sending a site ([`to_item`]) work on its item.
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
//! the item, so an item holds only what says something. A part that is a
//! key of an object field (a rank, a crawl mark) changes only that key, so
//! keys this version does not know stay, as the format keeps them;
//! [`clean`] drops them. Reading never fails: a part that is not there, or
//! does not read, is nothing.
//!
//! A node's sites together, by domain, are a [`crate::sites::Sites`],
//! which merges and cleans them with these functions.

use anyhow::{ensure, Result};
use plumb_format::refs::is_domain;
use plumb_format::value::{is_language, unix_seconds};
use plumb_format::Ref;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::site_item::{
    canonical_owned, edge, number, once, sent_item, string, strings, tidy, tidy_all, tidy_keyword,
    tidy_keywords, timestamp, SITE,
};
use crate::{
    canonical_domain, collapse_whitespace, is_generic_kind, is_stand_in_homepage, kind_key,
    linker_bit, min_some, normalize_text, registrable_domain, site_language,
    sort_and_cap_link_texts, truncate_chars, AdultLevel, Headline, KeyPage, LinkText, Redirect,
    Signals, MAX_ALIASES, MAX_KEY_PAGES, MAX_KINDS, MAX_LINKS_TO, MAX_TEXT_CHARS,
};

/// The item a site is, as these functions read and change it: code that
/// works on sites through this module names it here.
pub use plumb_format::Item;

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

/// The homepage's visible `<h1>` and `<h2>` texts, in page order, at most
/// [`crate::MAX_HEADINGS`] and [`crate::MAX_HEADING_WORDS`] words in all.
pub fn headings(site: &Item) -> Vec<String> {
    strings(site.fields.get("headings"))
}

/// Sets [`headings`].
pub fn set_headings(site: &mut Item, headings: Vec<String>) {
    put(site, "headings", json!(headings));
}

/// Search terms picked from the whole homepage's text (YAKE), best first,
/// at most [`crate::MAX_TERMS`]; a word stands more than once to weigh
/// more. Searched like headings, below names.
pub fn terms(site: &Item) -> Vec<String> {
    strings(site.fields.get("terms"))
}

/// Sets [`terms`].
pub fn set_terms(site: &mut Item, terms: Vec<String>) {
    put(site, "terms", json!(terms));
}

/// What kind of thing the site's organization is, from Wikidata ("bank",
/// "airline"), at most [`MAX_KINDS`] ([`add_kind`]).
pub fn kinds(site: &Item) -> Vec<String> {
    strings(site.fields.get("kinds"))
}

/// Sets [`kinds`].
pub fn set_kinds(site: &mut Item, kinds: Vec<String>) {
    put(site, "kinds", json!(kinds));
}

/// The site's own search address, with `{searchTerms}` where the words go
/// ([`crate::search_link`]), read from a search form on its homepage.
pub fn search_url(site: &Item) -> Option<&str> {
    field_str(site, "search_url")
}

/// Sets [`search_url`].
pub fn set_search_url(site: &mut Item, search_url: Option<String>) {
    put(site, "search_url", json!(search_url));
}

/// The first sentences of the English Wikipedia article about the site's
/// organization, for well-known sites ("GitHub is a proprietary developer
/// platform that ...").
pub fn intro(site: &Item) -> Option<&str> {
    field_str(site, "intro")
}

/// Sets [`intro`].
pub fn set_intro(site: &mut Item, intro: Option<String>) {
    put(site, "intro", json!(intro));
}

/// One sentence saying what the site is, written by a language model for
/// a well-known site that has no words of its own here (no description,
/// homepage text, terms or Wikidata or Wikipedia text; see `plumb-tools
/// summaries`). Searched like the description, and only while the site
/// has none.
pub fn summary(site: &Item) -> Option<&str> {
    field_str(site, "summary")
}

/// Sets [`summary`].
pub fn set_summary(site: &mut Item, summary: Option<String>) {
    put(site, "summary", json!(summary));
}

/// What the site's organization is, in a few words, from Wikidata's
/// English description ("American bank holding company"), unlike the
/// item's `about`, which is the homepage's description.
pub fn wikidata_about(site: &Item) -> Option<&str> {
    member(site, "wikidata", "about").and_then(Value::as_str)
}

/// Sets [`wikidata_about`].
pub fn set_wikidata_about(site: &mut Item, about: Option<String>) {
    set_member(site, "wikidata", "about", about.map(Value::String));
}

/// The country the site belongs to, as an ISO 3166-1 alpha-2 code (`US`),
/// from Wikidata's country of the organization whose official website it
/// is. [`site_country`] falls back to the domain ending when it has none.
pub fn country(site: &Item) -> Option<&str> {
    member(site, "wikidata", "country").and_then(Value::as_str)
}

/// Sets [`country`].
pub fn set_country(site: &mut Item, country: Option<String>) {
    set_member(site, "wikidata", "country", country.map(Value::String));
}

/// The site's icon as a small PNG, base64, as a crawl another node shared
/// brings it into the inbox. A node keeps icons in a store of their own,
/// and [`merge`] leaves this item's as it is.
pub fn icon(site: &Item) -> Option<&str> {
    field_str(site, "icon")
}

/// Sets [`icon`].
pub fn set_icon(site: &mut Item, icon: Option<String>) {
    put(site, "icon", json!(icon));
}

/// Recent headlines from the site's feed ([`crate::news`]), as a feed
/// check another node shared brings them into the inbox. A node keeps
/// headlines in a store of their own, and [`merge`] leaves this item's as
/// they are.
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

/// When the homepage was last read, in unix seconds: the last successful
/// crawl.
pub fn crawled(site: &Item) -> Option<u64> {
    field_str(site, "crawled")
        .and_then(unix_seconds)
        .and_then(|unix| u64::try_from(unix).ok())
}

/// Sets [`crawled`]; a time past the year 9999 is none.
pub fn set_crawled(site: &mut Item, at: Option<u64>) {
    put(site, "crawled", json!(at.and_then(timestamp)));
}

/// What the last crawl read from the homepage, as the crawler's
/// `CRAWL_VERSION` then (`plumb_crawl::CRAWL_VERSION`). When crawlers learn
/// to read more, or better, from a homepage, the version goes up and nodes
/// crawl the sites read by an older one again, best-known first.
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
    /// The last fetch of the homepage, successful or not, so that sites
    /// that keep failing are not tried on every crawl.
    pub tried: Option<u64>,
    /// Tries in a row, up to the last, that could not reach the site at
    /// all (no connection or no answer); 0 once a try gets an answer.
    /// Crawlers try such sites again sooner than ones that answered,
    /// waiting longer after each failure.
    pub failures: u32,
    /// When this node judged the site dead (its homepage could not be
    /// reached for weeks; see `plumb run --drop-dead-sites`) and cut it
    /// down to its crawl marks and ranks ([`make_gone`]): a gone site is
    /// left out of the index, and a crawl that reaches it again brings it
    /// back. A node's own judgment, which [`merge`] never takes from
    /// another site.
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

/// The words other sites link to this one with, most used first, at most
/// [`crate::MAX_LINK_TEXTS`], each with the sites that use it.
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
/// (`pncbank.com` to `pnc.com`), as of the last crawl. Sent to the other's
/// front page, the site is the other one under another name
/// ([`Redirect::folds_into`]). A stored item's may name a site an item
/// sent to other nodes cannot, one with a `_`.
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

/// The site's key pages ("sitelinks": sign in, docs, pricing), from the
/// links its homepage makes to the site itself, at most [`MAX_KEY_PAGES`]
/// ([`crate::key_pages::pick_key_pages`]): its `key_page` edges with their
/// labels.
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

/// The other sites the homepage links to, as registrable domains in the
/// order of their first link on the page, at most [`MAX_LINKS_TO`]
/// ([`crate::valid_links_to`]): its `links_to` edges, the site's edges in
/// the web's link graph.
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

/// The language the site is in, as its homepage declares or as its title
/// and description are written ([`site_language`]). The index, a node's
/// headlines and private search all judge a site by it.
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

/// The country the site belongs to: Wikidata's ([`country`]) when it has
/// one, else its domain ending's ([`crate::tld_country`]). `None` for
/// global sites such as most `.com`s.
pub fn site_country(site: &Item) -> Option<String> {
    crate::country::country_of(country(site), &site.id)
}

/// How adult the site looks: [`AdultLevel::Explicit`] for an adult
/// Wikidata kind, else [`crate::adult_level`] of its title, description,
/// Wikidata's description and other names.
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

/// Adds a link to this site with `text` from the site `linking_domain` (its
/// registrable domain). Each linking site counts once per text, however
/// often it is added ([`add_link_text_linkers`]).
pub fn add_link_text(site: &mut Item, text: &str, linking_domain: &str) {
    add_link_text_linkers(site, text, linker_bit(linking_domain));
}

/// Adds link text used by a set of linking sites, their [`linker_bit`]s
/// OR-ed together. The text is normalized first; empty text or an empty
/// set is ignored. Keeps the list sorted and capped at
/// [`crate::MAX_LINK_TEXTS`].
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

/// Adds a name to the item's `names`, its text tidied as [`clean`] would,
/// unless one with the same normalized form is there already or the site
/// has [`MAX_ALIASES`].
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

/// Adds a kind ("bank"), its text tidied as [`clean`] would, unless it is
/// generic ([`is_generic_kind`]), the same kind is there already (plurals
/// and case aside), or the site has [`MAX_KINDS`] already.
pub fn add_kind(site: &mut Item, kind: &str) {
    let mut kinds = kinds(site);
    if push_kind(&mut kinds, kind) {
        set_kinds(site, kinds);
    }
}

/// Adds `kind` to `kinds` as [`add_kind`] does; whether it did.
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

/// Folds another item of the same site into this one.
///
/// The page (url, title, description, language, search address, headings,
/// text, terms, key pages and linked sites) comes from whichever was
/// crawled more recently; a fresher crawl that found no search form or no
/// headings means the site has none now, and an older crawl only fills
/// what this item lacks. The first item's country and Wikidata
/// description win, and kinds are unioned. Link texts take the union of
/// their linking sites (a site seen by both counts once), names are
/// unioned, ranks keep the best (lowest) value, `linking_domains` the
/// larger count (sources often overlap, so adding would count twice), and
/// `official_site` is OR-ed. The later crawl try is kept, with the
/// failures counted at it (the larger count when both tried at the same
/// time). A redirect stays only when no successful crawl came after it. A
/// gone site ([`CrawlMarks::gone`]) takes only ranks and crawl marks, until
/// a crawl made after it went reaches the site. The icon and headlines
/// are this item's, and so are keys of its fields this version does not
/// know.
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

/// [`merge`] for a crawl another node shared. Shared crawls carry only some
/// of what a crawl finds (no search box, and page text only from trusted
/// crawlers), so what the shared crawl leaves empty keeps what this item
/// has rather than being cleared.
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
/// `official_site` is OR-ed ([`merge`]).
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

/// Cuts the site down to a gone one's ([`CrawlMarks::gone`]): its domain,
/// ranks, last crawl and crawl marks, gone since `at`.
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
/// for the site ([`is_stand_in_homepage`]).
///
/// So is a homepage read on another site's host: prevention.nih.gov's
/// "NIH Office of Disease Prevention" kept for nih.gov, or
/// play.google.com's page for google.com, from crawls before those
/// subdomains were sites of their own ([`crate::subsites`]); crawlers now
/// stop at such a redirect. And so is a hosting company's page for a
/// suspended account (cPanel's `/cgi-sys/suspendedpage.cgi`, titled
/// "Contact Support").
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

/// Clears what a bot check put in the item ([`is_bot_check`]): url, title,
/// description, headings, page text, terms, search box, key pages, linked
/// sites and the crawl time, so it no longer counts as crawled. Merged into
/// another item, it then leaves that item's page alone. Returns whether it
/// did.
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

/// Puts the site in its item's form: text in Unicode NFC, control
/// characters made spaces, trimmed;
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

/// The site as an item to send ([`crate::site_item`]): of the site as
/// [`clean`] makes it, without the node's own fields (its crawl marks, icon and
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

/// The site as one line of JSON to send, the item [`to_item`] makes: how
/// crawl batches and bucket answers carry a site.
pub fn to_line(site: &Item) -> Result<String> {
    Ok(serde_json::to_string(&to_item(site)?)?)
}

/// The site a line [`to_line`] wrote holds, as the node takes it in: once
/// its item keeps the format's rules ([`Item::check`]) and is a whole
/// site, what [`from_sent`] keeps of it.
pub fn from_line(line: &str) -> Result<Item> {
    let item: Item = serde_json::from_str(line)?;
    item.check()?;
    ensure!(
        !item.is_extension() && !item.is_deleted(),
        "item {:?} is not a whole site",
        item.id
    );
    from_sent(&item)
}

/// What a node keeps of a `site` item from elsewhere, another node's or a
/// signed set's: the parts a site has, each as it reads, and nothing of
/// what the item holds besides (members and fields this version does not
/// know, keys of fields it does not know, edges but key pages and linked
/// sites, a redirect to a site no item can name, and the crawl marks, icon
/// and headlines a node keeps of its own). Nothing is cleaned.
pub fn from_sent(sent: &Item) -> Result<Item> {
    ensure!(sent.kind == SITE, "item {:?} is not a site", sent.id);
    let mut site = new(sent.id.clone());
    site.url = sent.url.clone();
    site.title = sent.title.clone();
    site.about = sent.about.clone();
    site.text = sent.text.clone();
    site.lang = sent.lang.clone();
    site.names = sent.names.clone();
    let owned = |text: Option<&str>| text.map(str::to_string);
    set_headings(&mut site, headings(sent));
    set_terms(&mut site, terms(sent));
    set_kinds(&mut site, kinds(sent));
    set_search_url(&mut site, owned(search_url(sent)));
    set_intro(&mut site, owned(intro(sent)));
    set_summary(&mut site, owned(summary(sent)));
    set_wikidata_about(&mut site, owned(wikidata_about(sent)));
    set_country(&mut site, owned(country(sent)));
    set_signals(&mut site, &signals(sent));
    set_crawled(&mut site, crawled(sent));
    set_crawl_version(&mut site, crawl_version(sent));
    set_link_texts(&mut site, &link_texts(sent));
    set_redirect(&mut site, redirect(sent).filter(|r| is_domain(&r.to)));
    set_key_pages(&mut site, key_pages(sent));
    set_links_to(&mut site, links_to(sent));
    Ok(site)
}

/// Adds a name to the item's `names` as [`add_alias`] does, but with its
/// text not tidied but for its whitespace, as a node takes in the names of
/// a crawl another node sent, which the format holds to Unicode NFC
/// already.
pub fn add_sent_alias(site: &mut Item, alias: &str) {
    let alias = truncate_chars(&collapse_whitespace(alias), MAX_TEXT_CHARS);
    let key = normalize_text(&alias);
    if key.is_empty() || site.names.len() >= MAX_ALIASES {
        return;
    }
    if site.names.iter().any(|name| normalize_text(name) == key) {
        return;
    }
    site.names.push(alias);
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
    use crate::stored::item_as_line;

    /// A site's stored line, as text.
    fn line(site: &Item) -> String {
        String::from_utf8(item_as_line(site).unwrap()).unwrap()
    }

    /// A site with every part a site's stored item holds.
    pub(crate) fn full_site() -> Item {
        let mut site = new("python.org");
        site.url = Some("https://www.python.org/".into());
        site.title = Some("Welcome to Python.org".into());
        site.about = Some("The official home of the Python Programming Language".into());
        site.text = Some("Python is a programming language that lets you work quickly.".into());
        site.lang = Some("en".into());
        site.names = vec!["Python".into(), "Python Software Foundation".into()];
        set_key_pages(
            &mut site,
            vec![
                KeyPage {
                    label: "Downloads".into(),
                    url: "https://www.python.org/downloads/".into(),
                },
                KeyPage {
                    label: "Docs".into(),
                    url: "https://docs.python.org/3/".into(),
                },
            ],
        );
        set_links_to(&mut site, vec!["psf.org".into(), "pypi.org".into()]);
        set_headings(&mut site, vec!["Get Started".into(), "Download".into()]);
        let terms = ["python", "download python", "python"];
        set_terms(&mut site, terms.map(String::from).to_vec());
        set_link_texts(
            &mut site,
            &[
                LinkText::from_linkers("python", 0x00a1_b2c3_d4e5_f607),
                LinkText::with_count("python.org", 3),
            ],
        );
        set_kinds(&mut site, vec!["programming language".into()]);
        set_search_url(
            &mut site,
            Some("https://www.python.org/search/?q={searchTerms}".into()),
        );
        set_intro(
            &mut site,
            Some("Python is a high-level, general-purpose programming language.".into()),
        );
        set_summary(
            &mut site,
            Some("The home of the Python programming language.".into()),
        );
        set_wikidata_about(
            &mut site,
            Some("general-purpose programming language".into()),
        );
        set_country(&mut site, Some("US".into()));
        set_signals(
            &mut site,
            &Signals {
                harmonic_rank: Some(300),
                pagerank_rank: Some(250),
                tranco_rank: Some(512),
                linking_domains: 412,
                official_site: true,
                sitelinks: 150,
                fact_checks: 12,
                fact_agrees: 11,
            },
        );
        set_crawled(&mut site, Some(1_791_605_520));
        set_crawl_version(&mut site, 7);
        set_crawl_marks(
            &mut site,
            CrawlMarks {
                tried: Some(1_791_605_600),
                failures: 2,
                gone: Some(1_791_000_000),
            },
        );
        set_redirect(
            &mut site,
            Some(Redirect {
                to: "python.com".into(),
                at: 1_791_605_520,
                inner: true,
            }),
        );
        set_icon(&mut site, Some("iVBORw0KGgo=".into()));
        set_news(
            &mut site,
            vec![Headline {
                title: "Python 3.15 is out".into(),
                url: "https://www.python.org/news/".into(),
                at: 1_791_605_520,
            }],
        );
        site
    }

    /// Clean sites of python.org that differ in each thing merging reads:
    /// which was crawled last, a crawl that found no search box or page
    /// text, a gone site, redirects before and after a crawl, a try at the
    /// same time, and names, kinds, link texts and ranks of their own.
    pub(crate) fn variants() -> Vec<Item> {
        let full = full_site();
        let mut never = new("python.org");
        never.names = vec!["CPython".into(), "python".into()];
        let never_kinds = ["software", "programming languages"];
        set_kinds(&mut never, never_kinds.map(String::from).to_vec());
        set_wikidata_about(&mut never, Some("a programming language".into()));
        let ranked = Signals {
            tranco_rank: Some(900),
            ..Signals::default()
        };
        set_signals(&mut never, &ranked);
        let mut older = full.clone();
        older.title = Some("Python".into());
        older.text = None;
        set_crawled(&mut older, Some(1_791_000_100));
        let tried = CrawlMarks {
            tried: Some(1_791_000_100),
            ..CrawlMarks::default()
        };
        set_crawl_marks(&mut older, tried);
        set_redirect(&mut older, None);
        set_search_url(&mut older, None);
        set_headings(&mut older, Vec::new());
        set_key_pages(&mut older, Vec::new());
        set_links_to(&mut older, Vec::new());
        set_country(&mut older, None);
        set_intro(&mut older, None);
        set_link_texts(
            &mut older,
            &[
                LinkText::from_linkers("python", 0x10),
                LinkText::with_count("py", 9),
            ],
        );
        let older_signals = Signals {
            tranco_rank: Some(400),
            linking_domains: 500,
            fact_checks: 20,
            fact_agrees: 3,
            ..Signals::default()
        };
        set_signals(&mut older, &older_signals);
        let mut gone = new("python.org");
        set_crawled(&mut gone, Some(1_791_605_520));
        let marks = CrawlMarks {
            tried: Some(1_791_700_000),
            failures: 5,
            gone: Some(1_791_700_000),
        };
        set_crawl_marks(&mut gone, marks);
        let harmonic = Signals {
            harmonic_rank: Some(100),
            ..Signals::default()
        };
        set_signals(&mut gone, &harmonic);
        let mut newer = new("python.org");
        newer.url = Some("https://python.org/".into());
        newer.title = Some("Python".into());
        newer.text = Some("Python 3.15".into());
        set_headings(&mut newer, vec!["News".into()]);
        set_crawled(&mut newer, Some(1_791_800_000));
        set_crawl_version(&mut newer, 8);
        let marks = CrawlMarks {
            tried: Some(1_791_900_000),
            failures: 1,
            gone: None,
        };
        set_crawl_marks(&mut newer, marks);
        let redirect = Redirect {
            to: "foo_bar.com".into(),
            at: 1_791_900_000,
            inner: false,
        };
        set_redirect(&mut newer, Some(redirect));
        set_kinds(&mut newer, vec!["website".into()]);
        let mut shared = newer.clone();
        shared.text = None;
        set_headings(&mut shared, Vec::new());
        set_redirect(&mut shared, None);
        set_links_to(&mut shared, vec!["example.com".into()]);
        set_icon(&mut shared, Some("R0lGODlh".into()));
        let headline = Headline {
            title: "Python 3.15".into(),
            url: "https://www.python.org/news/315".into(),
            at: 1_791_800_000,
        };
        set_news(&mut shared, vec![headline]);
        let mut same_try = older.clone();
        set_crawled(&mut same_try, None);
        let marks = CrawlMarks {
            tried: crawl_marks(&full).tried,
            failures: 4,
            gone: None,
        };
        set_crawl_marks(&mut same_try, marks);
        let mut variants = vec![full, never, older, gone, newer, shared, same_try];
        variants.iter_mut().for_each(clean);
        variants
    }

    /// The item the setters make of what the getters read off `site`.
    fn rebuilt(site: &Item) -> Item {
        let owned = |text: Option<&str>| text.map(str::to_string);
        let mut rebuilt = new(domain(site));
        rebuilt.url = site.url.clone();
        rebuilt.title = site.title.clone();
        rebuilt.about = site.about.clone();
        rebuilt.text = site.text.clone();
        rebuilt.lang = site.lang.clone();
        rebuilt.names = site.names.clone();
        // Edges in the other order than the item has them.
        set_links_to(&mut rebuilt, links_to(site));
        set_key_pages(&mut rebuilt, key_pages(site));
        set_headings(&mut rebuilt, headings(site));
        set_terms(&mut rebuilt, terms(site));
        set_kinds(&mut rebuilt, kinds(site));
        set_search_url(&mut rebuilt, owned(search_url(site)));
        set_intro(&mut rebuilt, owned(intro(site)));
        set_summary(&mut rebuilt, owned(summary(site)));
        set_country(&mut rebuilt, owned(country(site)));
        set_signals(&mut rebuilt, &signals(site));
        set_wikidata_about(&mut rebuilt, owned(wikidata_about(site)));
        set_icon(&mut rebuilt, owned(icon(site)));
        set_news(&mut rebuilt, news(site));
        set_crawled(&mut rebuilt, crawled(site));
        set_crawl_version(&mut rebuilt, crawl_version(site));
        set_crawl_marks(&mut rebuilt, crawl_marks(site));
        set_link_texts(&mut rebuilt, &link_texts(site));
        set_redirect(&mut rebuilt, redirect(site));
        rebuilt
    }

    #[test]
    fn a_site_s_parts_read_back_as_they_were_set() {
        for site in variants() {
            assert_eq!(rebuilt(&site), site);
        }
        assert_eq!(rebuilt(&new("example.com")), new("example.com"));
        assert_eq!(
            line(&new("example.com")),
            r#"{"id":"example.com","kind":"site"}"#
        );
        assert_eq!(
            line(&full_site()),
            r#"{"id":"python.org","kind":"site","title":"Welcome to Python.org","url":"https://www.python.org/","names":["Python","Python Software Foundation"],"about":"The official home of the Python Programming Language","text":"Python is a programming language that lets you work quickly.","lang":"en","weight":412.0,"edges":[{"rel":"key_page","to":"url:https://www.python.org/downloads/","text":"Downloads"},{"rel":"key_page","to":"url:https://docs.python.org/3/","text":"Docs"},{"rel":"links_to","to":"site:psf.org"},{"rel":"links_to","to":"site:pypi.org"}],"fields":{"crawl":{"failures":2,"gone":1791000000,"tried":1791605600},"crawl_version":7,"crawled":"2026-10-10T04:12:00Z","fact_checks":{"agreed":11,"checked":12},"headings":["Get Started","Download"],"icon":"iVBORw0KGgo=","intro":"Python is a high-level, general-purpose programming language.","kinds":["programming language"],"link_texts":[{"count":39,"linkers":"00a1b2c3d4e5f607","text":"python"},{"count":3,"text":"python.org"}],"news":[{"at":1791605520,"title":"Python 3.15 is out","url":"https://www.python.org/news/"}],"ranks":{"harmonic":300,"pagerank":250,"tranco":512},"redirect":{"at":"2026-10-10T04:12:00Z","inner":true,"to":"python.com"},"search_url":"https://www.python.org/search/?q={searchTerms}","summary":"The home of the Python programming language.","terms":["python","download python","python"],"wikidata":{"about":"general-purpose programming language","country":"US","official_site":true,"sitelinks":150}}}"#
        );
    }

    #[test]
    fn setting_a_part_to_nothing_takes_it_out() {
        let mut site = full_site();
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
        let mut site = full_site();
        set_signals(&mut site, &Signals::default());
        assert_eq!(country(&site), Some("US"));
        assert_eq!(
            wikidata_about(&site),
            Some("general-purpose programming language")
        );
    }

    #[test]
    fn a_fresher_crawl_brings_its_page_and_an_older_one_fills_gaps() {
        let mut a = new("x.com");
        a.title = Some("New".into());
        set_crawled(&mut a, Some(200));
        let mut b = new("x.com");
        b.title = Some("Old".into());
        b.about = Some("Desc".into());
        set_crawled(&mut b, Some(100));
        merge(&mut a, b);
        assert_eq!(a.title.as_deref(), Some("New"));
        assert_eq!(a.about.as_deref(), Some("Desc"));
        assert_eq!(crawled(&a), Some(200));

        let crawl = |at: u64, heads: &[&str]| {
            let mut site = new("a.com");
            set_crawled(&mut site, Some(at));
            set_headings(&mut site, heads.iter().map(|h| h.to_string()).collect());
            site
        };
        let mut site = crawl(1, &["Old"]);
        merge(&mut site, crawl(2, &[]));
        assert!(headings(&site).is_empty());
        let mut site = crawl(2, &["New"]);
        merge(&mut site, crawl(1, &["Old"]));
        assert_eq!(headings(&site), ["New"]);
        let mut seed = new("a.com");
        merge(&mut seed, crawl(1, &["Found"]));
        assert_eq!(headings(&seed), ["Found"]);
    }

    #[test]
    fn redirects_last_until_a_later_successful_crawl() {
        let redirected = |at: u64| {
            let mut site = new("pncbank.com");
            let to = Redirect {
                to: "pnc.com".into(),
                at,
                inner: false,
            };
            set_redirect(&mut site, Some(to));
            site
        };
        let crawl = |at: u64| {
            let mut site = new("pncbank.com");
            site.title = Some("PNC Bank".into());
            set_crawled(&mut site, Some(at));
            site
        };
        // In either order, the later event wins.
        for (first, second) in [(crawl(10), redirected(20)), (redirected(20), crawl(10))] {
            let mut site = first;
            merge(&mut site, second);
            assert_eq!(redirect(&site).map(|r| r.to).as_deref(), Some("pnc.com"));
        }
        for (first, second) in [(crawl(30), redirected(20)), (redirected(20), crawl(30))] {
            let mut site = first;
            merge(&mut site, second);
            assert_eq!(redirect(&site), None);
        }
        let mut site = redirected(20);
        merge(&mut site, redirected(10));
        assert_eq!(redirect(&site).unwrap().at, 20);
    }

    #[test]
    fn a_shared_crawl_updates_the_page_but_keeps_what_it_leaves_out() {
        let mut mine = new("a.com");
        mine.title = Some("Old".into());
        mine.text = Some("A shop for things".into());
        set_crawled(&mut mine, Some(1));
        set_search_url(&mut mine, Some("https://a.com/search?q={q}".into()));
        set_headings(&mut mine, vec!["Welcome".into()]);
        let sign_in = KeyPage {
            label: "Sign in".into(),
            url: "https://a.com/login".into(),
        };
        set_key_pages(&mut mine, vec![sign_in]);
        set_links_to(&mut mine, vec!["b.com".into()]);
        let mut shared = new("a.com");
        shared.title = Some("New".into());
        set_crawled(&mut shared, Some(2));
        let mut plain = mine.clone();
        merge(&mut plain, shared.clone());
        assert!(search_url(&plain).is_none() && plain.text.is_none());
        assert!(key_pages(&plain).is_empty());
        assert!(links_to(&plain).is_empty());

        merge_shared(&mut mine, shared);
        assert_eq!(mine.title.as_deref(), Some("New"));
        assert_eq!(crawled(&mine), Some(2));
        assert_eq!(search_url(&mine), Some("https://a.com/search?q={q}"));
        assert_eq!(headings(&mine), ["Welcome"]);
        assert_eq!(mine.text.as_deref(), Some("A shop for things"));
        assert_eq!(key_pages(&mine).len(), 1);
        assert_eq!(links_to(&mine), ["b.com"]);

        // Text a trusted crawler shared does replace it.
        let mut trusted = new("a.com");
        trusted.text = Some("Now a blog".into());
        set_crawled(&mut trusted, Some(3));
        set_headings(&mut trusted, vec!["Hello".into()]);
        merge_shared(&mut mine, trusted);
        assert_eq!(headings(&mine), ["Hello"]);
        assert_eq!(mine.text.as_deref(), Some("Now a blog"));
    }

    #[test]
    fn merge_handles_country_kinds_and_search_addresses() {
        let mut seed = new("chase.com");
        set_country(&mut seed, Some("US".into()));
        add_kind(&mut seed, "bank");
        add_kind(&mut seed, "Public company");
        assert_eq!(kinds(&seed), ["bank"], "generic kinds are dropped");

        let mut crawl = new("chase.com");
        set_crawled(&mut crawl, Some(10));
        let search = "https://www.chase.com/search?q={searchTerms}";
        set_search_url(&mut crawl, Some(search.into()));
        set_country(&mut crawl, Some("GB".into()));
        add_kind(&mut crawl, "Banks");
        add_kind(&mut crawl, "financial services");
        merge(&mut seed, crawl);
        assert_eq!(country(&seed), Some("US"));
        assert_eq!(kinds(&seed), ["bank", "financial services"]);
        assert_eq!(search_url(&seed), Some(search));

        // A later crawl without a search form clears it; an older one does not.
        let mut older = new("chase.com");
        set_crawled(&mut older, Some(5));
        merge(&mut seed, older);
        assert_eq!(search_url(&seed), Some(search));
        let mut newer = new("chase.com");
        set_crawled(&mut newer, Some(20));
        merge(&mut seed, newer);
        assert_eq!(search_url(&seed), None);
    }

    #[test]
    fn merge_keeps_best_signals_and_fresh_pages() {
        let mut a = new("usbank.com");
        a.title = Some("Old title".into());
        set_crawled(&mut a, Some(100));
        let ranked = Signals {
            tranco_rank: Some(900),
            ..Signals::default()
        };
        set_signals(&mut a, &ranked);
        add_link_text(&mut a, "U.S. Bank", "a.com");
        add_link_text(&mut a, "U.S. Bank", "b.com");

        let mut b = new("usbank.com");
        b.title = Some("U.S. Bank | Personal Banking".into());
        b.about = Some("Banking, credit cards, loans".into());
        set_crawled(&mut b, Some(200));
        let tried = CrawlMarks {
            tried: Some(250),
            ..CrawlMarks::default()
        };
        set_crawl_marks(&mut b, tried);
        let ranked = Signals {
            tranco_rank: Some(1200),
            harmonic_rank: Some(5000),
            official_site: true,
            ..Signals::default()
        };
        set_signals(&mut b, &ranked);
        // b.com is already counted, c.com is new.
        add_link_text(&mut b, "us bank", "b.com");
        add_link_text(&mut b, "us bank", "c.com");
        add_alias(&mut b, "U.S. Bancorp");

        merge(&mut a, b);
        assert_eq!(a.title.as_deref(), Some("U.S. Bank | Personal Banking"));
        assert_eq!(a.about.as_deref(), Some("Banking, credit cards, loans"));
        assert_eq!(crawled(&a), Some(200));
        assert_eq!(crawl_marks(&a).tried, Some(250));
        let scores = signals(&a);
        assert_eq!(scores.tranco_rank, Some(900));
        assert_eq!(scores.harmonic_rank, Some(5000));
        assert!(scores.official_site);
        let linkers = ["a.com", "b.com", "c.com"]
            .iter()
            .fold(0, |bits, d| bits | linker_bit(d));
        assert_eq!(
            link_texts(&a),
            vec![LinkText {
                text: "us bank".into(),
                count: 3,
                linkers,
            }]
        );
        assert_eq!(a.names, ["U.S. Bancorp"]);
    }

    #[test]
    fn checked_facts_merge_to_the_bigger_count_and_the_network_keeps_the_worse() {
        let mut a = new("a.com");
        let all_right = Signals {
            fact_checks: 10,
            fact_agrees: 10,
            ..Default::default()
        };
        set_signals(&mut a, &all_right);
        let mut b = new("a.com");
        let some_wrong = Signals {
            fact_checks: 20,
            fact_agrees: 12,
            ..Default::default()
        };
        set_signals(&mut b, &some_wrong);
        merge(&mut a, b);
        let merged = signals(&a);
        assert_eq!((merged.fact_checks, merged.fact_agrees), (20, 12));
        let good = Signals {
            fact_checks: 30,
            fact_agrees: 30,
            ..Default::default()
        };
        assert_eq!(good.worse_facts(&some_wrong), (20, 12));
        assert_eq!(some_wrong.worse_facts(&good), (20, 12));
        // One answer with no count leaves none.
        assert_eq!(good.worse_facts(&Signals::default()), (0, 0));
    }

    #[test]
    fn a_gone_site_stays_small_until_a_later_crawl_reaches_it() {
        let mut gone = new("dead.com");
        gone.title = Some("Dead".into());
        gone.text = Some("words".into());
        add_alias(&mut gone, "Dead Inc");
        let ranked = Signals {
            tranco_rank: Some(50),
            ..Signals::default()
        };
        set_signals(&mut gone, &ranked);
        set_crawled(&mut gone, Some(100));
        let marks = CrawlMarks {
            tried: Some(900),
            failures: 6,
            gone: None,
        };
        set_crawl_marks(&mut gone, marks);
        make_gone(&mut gone, 1_000);
        assert_eq!(gone.title, None);
        assert!(gone.names.is_empty() && gone.text.is_none());
        assert_eq!(signals(&gone).tranco_rank, Some(50));
        let marks = crawl_marks(&gone);
        assert_eq!(
            (crawled(&gone), marks.tried, marks.failures),
            (Some(100), Some(900), 6)
        );

        // A seed or an older crawl only brings ranks and marks.
        let mut old = new("dead.com");
        old.title = Some("Dead".into());
        let ranked = Signals {
            tranco_rank: Some(40),
            ..Signals::default()
        };
        set_signals(&mut old, &ranked);
        set_crawled(&mut old, Some(500));
        let mut tried = new("dead.com");
        let marks = CrawlMarks {
            tried: Some(1_100),
            failures: 7,
            gone: None,
        };
        set_crawl_marks(&mut tried, marks);
        let mut merged = gone.clone();
        merge(&mut merged, old);
        merge_shared(&mut merged, tried);
        let marks = crawl_marks(&merged);
        assert_eq!(marks.gone, Some(1_000));
        assert_eq!(merged.title, None);
        assert_eq!(signals(&merged).tranco_rank, Some(40));
        assert_eq!(
            (crawled(&merged), marks.tried, marks.failures),
            (Some(100), Some(1_100), 7)
        );

        // A crawl that reaches it after it went brings it back.
        let mut back = new("dead.com");
        back.title = Some("Alive".into());
        set_crawled(&mut back, Some(1_200));
        let marks = CrawlMarks {
            tried: Some(1_200),
            ..CrawlMarks::default()
        };
        set_crawl_marks(&mut back, marks);
        merge(&mut merged, back);
        assert_eq!(crawl_marks(&merged).gone, None);
        assert_eq!(merged.title.as_deref(), Some("Alive"));
        assert_eq!(crawl_marks(&merged).failures, 0);

        // Gone is this node's own judgment: another site's never spreads.
        let mut alive = new("dead.com");
        alive.title = Some("Here".into());
        merge(&mut alive, gone);
        assert_eq!(crawl_marks(&alive).gone, None);
        assert_eq!(alive.title.as_deref(), Some("Here"));
    }

    #[test]
    fn the_crawl_version_follows_the_page_text() {
        let crawl = |at: u64, version: u32, text: bool| {
            let mut site = new("a.com");
            site.text = text.then(|| "words".to_owned());
            set_crawled(&mut site, Some(at));
            set_crawl_version(&mut site, version);
            site
        };
        let mut a = crawl(10, 2, true);
        merge(&mut a, crawl(20, 1, true));
        assert_eq!(crawl_version(&a), 1, "a fresher crawl by an older crawler");
        let mut a = crawl(10, 2, true);
        merge(&mut a, crawl(5, 1, true));
        assert_eq!(crawl_version(&a), 2, "an older crawl changes nothing");
        // A shared crawl without the page's text read none of it.
        let mut a = crawl(10, 2, true);
        merge_shared(&mut a, crawl(20, 0, false));
        assert_eq!(crawl_version(&a), 2);
        let mut a = crawl(10, 1, true);
        merge_shared(&mut a, crawl(20, 2, true));
        assert_eq!(crawl_version(&a), 2);
    }

    #[test]
    fn merge_keeps_the_failure_count_of_the_later_attempt() {
        let tried = |tried: Option<u64>, failures: u32| {
            let mut site = new("flaky.com");
            let marks = CrawlMarks {
                tried,
                failures,
                gone: None,
            };
            set_crawl_marks(&mut site, marks);
            site
        };
        let merged = |mut a: Item, b: Item| {
            merge(&mut a, b);
            let marks = crawl_marks(&a);
            (marks.tried, marks.failures)
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
    fn link_texts_count_each_linking_site_once() {
        let mut a = new("usbank.com");
        for _ in 0..60 {
            // One site repeating a footer link on every page.
            add_link_text(&mut a, "US Bank", "spam.example");
        }
        assert_eq!(link_texts(&a)[0].count, 1);

        // A monthly re-crawl of the same five sites adds nothing.
        let linkers = ["a.com", "b.com", "c.com", "d.com", "e.com"];
        let crawl = |text: &str| {
            let mut site = new("usbank.com");
            for d in linkers {
                add_link_text(&mut site, text, d);
            }
            site
        };
        let mut b = crawl("us bank");
        for _ in 0..12 {
            merge(&mut b, crawl("us bank"));
        }
        assert_eq!(link_texts(&b)[0].count, 5);

        // Plain counts (no linker set) keep the larger count on merge.
        let mut c = new("x.com");
        set_link_texts(&mut c, &[LinkText::with_count("x", 7)]);
        let mut d = new("x.com");
        set_link_texts(&mut d, &[LinkText::with_count("x", 4)]);
        add_link_text(&mut d, "y", "a.com");
        merge(&mut c, d);
        let texts = link_texts(&c);
        assert_eq!(texts[0], LinkText::with_count("x", 7));
        assert_eq!(texts[1].count, 1);
    }

    /// Merging each of the sites merging is tested on into each other
    /// keeps a site of the domain, holding the best ranks of the two.
    #[test]
    fn variants_merge_into_each_other() {
        let variants = variants();
        for (i, mine) in variants.iter().enumerate() {
            for (j, theirs) in variants.iter().enumerate() {
                for shared in [false, true] {
                    let mut site = mine.clone();
                    if shared {
                        merge_shared(&mut site, theirs.clone());
                    } else {
                        merge(&mut site, theirs.clone());
                    }
                    assert_eq!(site.id, "python.org");
                    let (a, b, merged) = (signals(mine), signals(theirs), signals(&site));
                    let best = min_some(a.tranco_rank, b.tranco_rank);
                    assert_eq!(merged.tranco_rank, best, "{i} {j} {shared}");
                }
            }
        }
    }

    #[test]
    fn an_item_cleans_into_its_stored_form() {
        let mut site = full_site();
        site.url = Some("HTTPS://WWW.Python.org/index%2ehtml?".into());
        site.title = Some("  Welcome\u{7} to Python ".into());
        site.about = Some("Dư\u{323} ba\u{301}o".into());
        site.names = vec!["Python".into(), " Python".into(), String::new()];
        set_terms(&mut site, vec!["python\nlanguage".into()]);
        set_kinds(&mut site, vec!["language".into(), "language ".into()]);
        let link_texts = [
            LinkText::with_count("python ", 3),
            LinkText::from_linkers("python", 0x3),
            LinkText::with_count("\u{7}", 1),
        ];
        set_link_texts(&mut site, &link_texts);
        set_country(&mut site, Some("US\n".into()));
        let mut pages = key_pages(&site);
        pages.push(KeyPage {
            label: "Bad".into(),
            url: "not a url".into(),
        });
        set_key_pages(&mut site, pages);
        let mut links = links_to(&site);
        links.push("foo_bar.com".into());
        set_links_to(&mut site, links);
        let to = Redirect {
            to: "Python.COM".into(),
            at: 1_791_605_520,
            inner: false,
        };
        set_redirect(&mut site, Some(to));
        site.lang = Some("not a language tag".into());
        // What no site holds goes too.
        site.published = Some("2026-10-15".into());
        site.ids.insert("wd".into(), "Q28865".into());
        site.fields.insert("colour".into(), json!("blue"));
        clean(&mut site);
        assert_eq!(
            line(&site),
            r#"{"id":"python.org","kind":"site","title":"Welcome  to Python","url":"https://www.python.org/index.html","names":["Python"],"about":"Dự báo","text":"Python is a programming language that lets you work quickly.","weight":412.0,"edges":[{"rel":"key_page","to":"url:https://www.python.org/downloads/","text":"Downloads"},{"rel":"key_page","to":"url:https://docs.python.org/3/","text":"Docs"},{"rel":"links_to","to":"site:psf.org"},{"rel":"links_to","to":"site:pypi.org"}],"fields":{"crawl":{"failures":2,"gone":1791000000,"tried":1791605600},"crawl_version":7,"crawled":"2026-10-10T04:12:00Z","fact_checks":{"agreed":11,"checked":12},"headings":["Get Started","Download"],"icon":"iVBORw0KGgo=","intro":"Python is a high-level, general-purpose programming language.","kinds":["language"],"link_texts":[{"count":3,"linkers":"0000000000000003","text":"python"}],"news":[{"at":1791605520,"title":"Python 3.15 is out","url":"https://www.python.org/news/"}],"ranks":{"harmonic":300,"pagerank":250,"tranco":512},"redirect":{"at":"2026-10-10T04:12:00Z","to":"python.com"},"search_url":"https://www.python.org/search/?q={searchTerms}","summary":"The home of the Python programming language.","terms":["python language"],"wikidata":{"about":"general-purpose programming language","country":"US","official_site":true,"sitelinks":150}}}"#
        );
        assert_eq!(redirect(&site).unwrap().to, "python.com");
        let clean_once = site.clone();
        clean(&mut site);
        assert_eq!(site, clean_once);
    }

    #[test]
    fn a_gone_site_and_a_bot_check_are_cut_down() {
        let gone: Vec<String> = variants()
            .into_iter()
            .map(|mut site| {
                make_gone(&mut site, 1_792_000_000);
                line(&site)
            })
            .collect();
        assert_eq!(
            gone,
            [
                r#"{"id":"python.org","kind":"site","weight":412.0,"fields":{"crawl":{"failures":2,"gone":1792000000,"tried":1791605600},"crawled":"2026-10-10T04:12:00Z","fact_checks":{"agreed":11,"checked":12},"ranks":{"harmonic":300,"pagerank":250,"tranco":512},"wikidata":{"official_site":true,"sitelinks":150}}}"#,
                r#"{"id":"python.org","kind":"site","fields":{"crawl":{"gone":1792000000},"ranks":{"tranco":900}}}"#,
                r#"{"id":"python.org","kind":"site","weight":500.0,"fields":{"crawl":{"gone":1792000000,"tried":1791000100},"crawled":"2026-10-03T04:01:40Z","fact_checks":{"agreed":3,"checked":20},"ranks":{"tranco":400}}}"#,
                r#"{"id":"python.org","kind":"site","fields":{"crawl":{"failures":5,"gone":1792000000,"tried":1791700000},"crawled":"2026-10-10T04:12:00Z","ranks":{"harmonic":100}}}"#,
                r#"{"id":"python.org","kind":"site","fields":{"crawl":{"failures":1,"gone":1792000000,"tried":1791900000},"crawled":"2026-10-12T10:13:20Z"}}"#,
                r#"{"id":"python.org","kind":"site","fields":{"crawl":{"failures":1,"gone":1792000000,"tried":1791900000},"crawled":"2026-10-12T10:13:20Z"}}"#,
                r#"{"id":"python.org","kind":"site","weight":500.0,"fields":{"crawl":{"failures":4,"gone":1792000000,"tried":1791605600},"fact_checks":{"agreed":3,"checked":20},"ranks":{"tranco":400}}}"#,
            ]
        );

        let mut site = full_site();
        site.url = Some("https://www.google.com/".into());
        assert!(is_bot_check(&site));
        assert!(drop_bot_check(&mut site));
        assert_eq!(
            line(&site),
            r#"{"id":"python.org","kind":"site","names":["Python","Python Software Foundation"],"lang":"en","weight":412.0,"fields":{"crawl":{"failures":2,"gone":1791000000,"tried":1791605600},"crawl_version":7,"fact_checks":{"agreed":11,"checked":12},"icon":"iVBORw0KGgo=","intro":"Python is a high-level, general-purpose programming language.","kinds":["programming language"],"link_texts":[{"count":39,"linkers":"00a1b2c3d4e5f607","text":"python"},{"count":3,"text":"python.org"}],"news":[{"at":1791605520,"title":"Python 3.15 is out","url":"https://www.python.org/news/"}],"ranks":{"harmonic":300,"pagerank":250,"tranco":512},"redirect":{"at":"2026-10-10T04:12:00Z","inner":true,"to":"python.com"},"summary":"The home of the Python programming language.","wikidata":{"about":"general-purpose programming language","country":"US","official_site":true,"sitelinks":150}}}"#
        );
        assert!(!is_bot_check(&site));
        for mut site in variants() {
            let before = site.clone();
            assert!(!drop_bot_check(&mut site));
            assert_eq!(site, before);
        }
    }

    #[test]
    fn names_kinds_and_link_texts_are_added_tidied_and_once() {
        let added: Vec<_> = variants()
            .into_iter()
            .map(|mut site| {
                for alias in [
                    "Python",
                    "PYTHON",
                    "Python  Language",
                    "",
                    "Py\u{7}thon",
                    "Dư\u{323} ba\u{301}o",
                ] {
                    add_alias(&mut site, alias);
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
                }
                add_link_text(&mut site, "Python", "example.com");
                add_link_text_linkers(&mut site, "The Python", 0xff);
                add_link_text_linkers(&mut site, "nothing", 0);
                let texts: Vec<(String, u32, u64)> = link_texts(&site)
                    .into_iter()
                    .map(|lt| (lt.text, lt.count, lt.linkers))
                    .collect();
                (
                    site.names.clone(),
                    kinds(&site),
                    texts,
                    written_language(&site),
                )
            })
            .collect();
        let names = |names: &[&str]| -> Vec<String> {
            let tail = ["Python Language", "Py thon", "Dự báo"];
            names
                .iter()
                .chain(&tail)
                .map(|name| name.to_string())
                .collect()
        };
        let kinds =
            |kinds: &[&str]| -> Vec<String> { kinds.iter().map(|kind| kind.to_string()).collect() };
        let texts = |texts: &[(&str, u32, u64)]| -> Vec<(String, u32, u64)> {
            let texts = texts.iter();
            texts
                .map(|&(text, count, linkers)| (text.to_string(), count, linkers))
                .collect()
        };
        let the = ("the python", 9, 255);
        let full = names(&["Python", "Python Software Foundation"]);
        let english = Some("en".to_string());
        let own = ["programming language", "café", "ba nk", "film"];
        let older = texts(&[("py", 9, 0), the, ("python", 2, 524_304)]);
        let crawled = ["website", "café", "ba nk", "programming language", "film"];
        let gone = ["café", "ba nk", "programming language", "film"];
        let once = texts(&[the, ("python", 1, 524_288)]);
        assert_eq!(
            added,
            [
                (
                    full.clone(),
                    kinds(&own),
                    texts(&[
                        ("python", 40, 45_514_025_411_147_271),
                        the,
                        ("python.org", 3, 0)
                    ]),
                    english.clone()
                ),
                (
                    names(&["CPython", "python"]),
                    kinds(&["software", "programming languages", "café", "ba nk", "film"]),
                    once.clone(),
                    None
                ),
                (full.clone(), kinds(&own), older.clone(), english.clone()),
                (names(&["Python"]), kinds(&gone), once.clone(), None),
                (names(&["Python"]), kinds(&crawled), once.clone(), None),
                (names(&["Python"]), kinds(&crawled), once, None),
                (full, kinds(&own), older, english),
            ]
        );
    }

    #[test]
    fn a_site_s_country_and_adult_level_come_from_its_parts() {
        let mut sites = variants();
        let mut adult = new("example.fr");
        set_kinds(&mut adult, vec!["adult website".into()]);
        let mut suggestive = new("example.com");
        suggestive.title = Some("Sexy lingerie".into());
        set_country(&mut suggestive, Some("uk".into()));
        let mut named = new("example.de");
        named.names = vec!["XXX".into()];
        let mut described = new("example.org");
        described.about = Some("Erotic stories".into());
        set_wikidata_about(&mut described, Some("website".into()));
        let mut nestle = new("nestle.com");
        set_country(&mut nestle, Some("ch".into()));
        let mut airline = new("americanairlines.fr");
        set_country(&mut airline, Some("bogus".into()));
        sites.extend([adult, suggestive, named, described, nestle, airline]);
        sites.push(new("nestle.com"));
        let read: Vec<(String, Option<String>, AdultLevel)> = sites
            .into_iter()
            .map(|mut site| {
                clean(&mut site);
                (site.id.clone(), site_country(&site), adult_level(&site))
            })
            .collect();
        let python = |country: Option<&str>| {
            (
                "python.org".to_string(),
                country.map(str::to_string),
                AdultLevel::None,
            )
        };
        let of = |domain: &str, country: Option<&str>, level| {
            (domain.to_string(), country.map(str::to_string), level)
        };
        assert_eq!(
            read,
            [
                python(Some("US")),
                python(None),
                python(None),
                python(None),
                python(None),
                python(None),
                python(None),
                of("example.fr", Some("FR"), AdultLevel::Explicit),
                of("example.com", Some("GB"), AdultLevel::Suggestive),
                of("example.de", Some("DE"), AdultLevel::Explicit),
                of("example.org", None, AdultLevel::Suggestive),
                of("nestle.com", Some("CH"), AdultLevel::None),
                of("americanairlines.fr", Some("FR"), AdultLevel::None),
                of("nestle.com", None, AdultLevel::None),
            ]
        );
    }

    #[test]
    fn parts_keep_the_keys_this_version_does_not_know() {
        let mut site = full_site();
        site.fields.get_mut("ranks").unwrap()["majestic"] = json!(7);
        site.fields.get_mut("crawl").unwrap()["queued"] = json!(1);
        site.fields.get_mut("wikidata").unwrap()["since"] = json!("1991");
        set_signals(&mut site, &Signals::default());
        set_crawl_marks(&mut site, CrawlMarks::default());
        assert_eq!(site.fields["ranks"], json!({"majestic": 7}));
        assert_eq!(site.fields["crawl"], json!({"queued": 1}));
        assert_eq!(site.fields["wikidata"]["since"], json!("1991"));
        let mut theirs = full_site();
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
        let mut site = full_site();
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
        assert_eq!(site, full_site());
        assert_eq!(site.kind, SITE);
        assert_eq!(site.deleted, None);
        assert!(site.labels.is_empty());
        assert_eq!(site.weight, Some(412.0));
    }

    #[test]
    fn a_site_is_sent_as_an_item_that_keeps_the_format_s_rules() {
        assert_eq!(
            to_line(&full_site()).unwrap(),
            r#"{"id":"python.org","kind":"site","title":"Welcome to Python.org","url":"https://www.python.org/","names":["Python","Python Software Foundation"],"about":"The official home of the Python Programming Language","text":"Python is a programming language that lets you work quickly.","lang":"en","weight":412.0,"edges":[{"rel":"key_page","to":"url:https://www.python.org/downloads/","text":"Downloads"},{"rel":"key_page","to":"url:https://docs.python.org/3/","text":"Docs"},{"rel":"links_to","to":"site:psf.org"},{"rel":"links_to","to":"site:pypi.org"}],"fields":{"crawl_version":7,"crawled":"2026-10-10T04:12:00Z","fact_checks":{"agreed":11,"checked":12},"headings":["Get Started","Download"],"intro":"Python is a high-level, general-purpose programming language.","kinds":["programming language"],"link_texts":[{"count":39,"linkers":"00a1b2c3d4e5f607","text":"python"},{"count":3,"text":"python.org"}],"ranks":{"harmonic":300,"pagerank":250,"tranco":512},"redirect":{"at":"2026-10-10T04:12:00Z","inner":true,"to":"python.com"},"search_url":"https://www.python.org/search/?q={searchTerms}","summary":"The home of the Python programming language.","terms":["python","download python","python"],"wikidata":{"about":"general-purpose programming language","country":"US","official_site":true,"sitelinks":150}}}"#
        );
        // The site never crawled, the gone one, and the newer crawl and
        // what another node shared of it.
        let variants = variants();
        let sent = [1, 3, 4, 5].map(|i| to_line(&variants[i]).unwrap());
        assert_eq!(
            sent,
            [
                r#"{"id":"python.org","kind":"site","site":"python.org","names":["CPython","python"],"fields":{"kinds":["software","programming languages"],"ranks":{"tranco":900},"wikidata":{"about":"a programming language"}}}"#,
                r#"{"id":"python.org","kind":"site","site":"python.org","fields":{"crawled":"2026-10-10T04:12:00Z","ranks":{"harmonic":100}}}"#,
                r#"{"id":"python.org","kind":"site","title":"Python","url":"https://python.org/","text":"Python 3.15","fields":{"crawl_version":8,"crawled":"2026-10-12T10:13:20Z","headings":["News"],"kinds":["website"]}}"#,
                r#"{"id":"python.org","kind":"site","title":"Python","url":"https://python.org/","edges":[{"rel":"links_to","to":"site:example.com"}],"fields":{"crawl_version":8,"crawled":"2026-10-12T10:13:20Z","kinds":["website"]}}"#,
            ]
        );
        // Not clean yet, with more text than an item holds and a redirect
        // to a site no item can name: sent as its clean self is.
        let mut untidy = full_site();
        untidy.url = Some("HTTPS://WWW.Python.org/index%2ehtml?".into());
        untidy.title = Some("  Welcome\u{7} to Python ".into());
        untidy.text = Some("word ".repeat(400));
        let to = Redirect {
            to: "foo_bar.com".into(),
            at: 1_791_605_520,
            inner: false,
        };
        set_redirect(&mut untidy, Some(to));
        let item = to_item(&untidy).unwrap();
        let mut clean_untidy = untidy.clone();
        clean(&mut clean_untidy);
        assert_eq!(to_item(&clean_untidy).unwrap(), item);
        assert_eq!(
            item.url.as_deref(),
            Some("https://www.python.org/index.html")
        );
        assert_eq!(item.title.as_deref(), Some("Welcome  to Python"));
        assert_eq!(item.text.as_ref().unwrap().chars().count(), 999);
        assert!(!item.fields.contains_key("redirect"));
        // Too big until its terms are shed, and too big even so.
        let mut shed = new("example.com");
        let terms = (0..100)
            .map(|i| format!("{i} {}", "x".repeat(200)))
            .collect();
        set_terms(&mut shed, terms);
        assert_eq!(
            to_line(&shed).unwrap(),
            r#"{"id":"example.com","kind":"site","site":"example.com"}"#
        );
        let mut big = new("big.example");
        big.names = (0..16)
            .map(|i| format!("{i} {}", "x".repeat(1_100)))
            .collect();
        let err = to_item(&big).unwrap_err();
        assert_eq!(
            err.to_string(),
            "big.example is over 16384 bytes as an item"
        );
        assert_eq!(
            to_line(&new("example.org")).unwrap(),
            r#"{"id":"example.org","kind":"site","site":"example.org"}"#
        );
    }

    /// A site's item from elsewhere is taken in with the parts a site has,
    /// each as it reads: items as nodes send them and as they store them,
    /// and one holding all a site does not, or that does not read.
    #[test]
    fn a_site_from_elsewhere_is_taken_in_with_what_reads() {
        for site in variants() {
            let sent = to_item(&site).unwrap();
            let mut taken = from_sent(&sent).unwrap();
            assert_eq!(taken.site.take(), None);
            let mut expected = sent.clone();
            expected.site = None;
            assert_eq!(taken, expected);
            let stored = from_sent(&site).unwrap();
            for name in ["crawl", "icon", "news"] {
                assert!(!stored.fields.contains_key(name), "{name}");
            }
            let line = to_line(&site).unwrap();
            assert_eq!(from_line(&line).unwrap(), from_sent(&sent).unwrap());
            assert_eq!(to_line(&from_line(&line).unwrap()).unwrap(), line);
        }
        let mut odd = full_site();
        odd.site = Some("python.org".into());
        odd.weight = Some(4.7);
        odd.labels = vec!["label".into()];
        odd.ids.insert("wikidata".into(), "Q28865".into());
        odd.license = Some("CC0-1.0".into());
        odd.fields.insert("unknown".into(), json!({"a": 1}));
        let ranks = json!({"tranco": 5_000_000_000_u64, "harmonic": 3, "other": 1});
        odd.fields.insert("ranks".into(), ranks);
        odd.fields.insert(
            "wikidata".into(),
            json!({"about": 7, "country": "NL", "x": 1}),
        );
        let redirect = json!({"to": "foo_bar.com", "at": "2026-10-01T00:00:00Z"});
        odd.fields.insert("redirect".into(), redirect);
        odd.fields
            .insert("crawled".into(), json!("2026-10-01T00:00:00.000Z"));
        odd.fields.insert("headings".into(), json!(["News", 3]));
        odd.edges.reverse();
        odd.edges
            .push(edge("cites", Ref::Site("example.com".into()), None));
        odd.edges.push(edge(
            "key_page",
            Ref::Url("https://www.python.org/x".into()),
            None,
        ));
        let taken = from_sent(&odd).unwrap();
        assert_eq!(taken.weight, Some(4.0));
        assert_eq!(taken.site, None);
        assert!(taken.labels.is_empty() && taken.ids.is_empty() && taken.license.is_none());
        assert_eq!(taken.fields["ranks"], json!({"harmonic": 3}));
        assert_eq!(taken.fields["wikidata"], json!({"country": "NL"}));
        assert_eq!(headings(&taken), ["News"]);
        for name in ["unknown", "redirect", "crawled", "crawl", "icon", "news"] {
            assert!(!taken.fields.contains_key(name), "{name}");
        }
        let pages: Vec<String> = key_pages(&taken)
            .into_iter()
            .map(|page| page.label)
            .collect();
        assert_eq!(pages, ["Docs", "Downloads"]);
        assert_eq!(links_to(&taken), ["pypi.org", "psf.org"]);
        assert_eq!(taken.edges.len(), 4);
        let mut weightless = odd.clone();
        weightless.weight = Some(-3.0);
        weightless.fields.remove("ranks");
        let taken = from_sent(&weightless).unwrap();
        assert_eq!(taken.weight, None);
        assert!(!taken.fields.contains_key("ranks"));
        let mut page = odd;
        page.kind = "page".into();
        assert!(from_sent(&page).is_err());

        let mut deleted = to_item(&full_site()).unwrap();
        deleted.deleted = Some(plumb_format::Deleted {
            at: "2026-10-01T00:00:00Z".into(),
            reason: None,
            other: Default::default(),
        });
        let line = serde_json::to_string(&deleted).unwrap();
        assert!(from_line(&line).is_err());
        assert!(from_line("{").is_err());
    }

    /// A name from another node's crawl is added as it is, but for its
    /// whitespace.
    #[test]
    fn a_sent_alias_is_added_as_it_is() {
        let long = format!("{} long", "x".repeat(999));
        let aliases = [
            "A1 Bank",
            "  a1   bank ",
            "A One\n",
            "",
            " ",
            "Ca\u{301}fe",
            &long,
        ];
        let mut site = new("a1.example");
        for alias in aliases {
            add_sent_alias(&mut site, alias);
        }
        assert_eq!(site.names[..3], ["A1 Bank", "A One", "Ca\u{301}fe"]);
        let counts: Vec<usize> = site.names.iter().map(|name| name.chars().count()).collect();
        assert_eq!(counts, [7, 5, 5, 300]);
        for i in 0..MAX_ALIASES {
            add_sent_alias(&mut site, &format!("name {i}"));
        }
        assert_eq!(site.names.len(), MAX_ALIASES);
        assert_eq!(
            site.names[4..],
            (0..12).map(|i| format!("name {i}")).collect::<Vec<_>>()
        );
    }
}
