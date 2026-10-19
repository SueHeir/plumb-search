//! A crawled site as an item of the data format ([`plumb_format`]): kind
//! `site`, its id the registrable domain. [`crate::site::to_item`] makes
//! the one a site sends and [`crate::site::from_sent`] takes one in, so
//! sets of sites (`plumb/sites`), crawl batches and bucket answers carry
//! sites as items, the last two a line of JSON each
//! ([`crate::site::to_line`]).
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
//! [`crate::site::clean`] puts a site in this form, short of cutting its
//! text, and a set cleans each site it takes
//! ([`crate::sites::Sites::upsert`]). A node keeps a clean site as its
//! stored item ([`crate::stored`]), which holds what the item it sends
//! leaves out besides.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use plumb_format::item::{MAX_ITEM_BYTES, MAX_TEXT_CHARS};
use plumb_format::refs::is_domain;
use plumb_format::urls::{canonical_url, is_canonical};
use plumb_format::value::{normalize_text, unix_seconds};
use plumb_format::{Edge, FieldSpec, Item, Ref};
use serde_json::{json, Value};
use unicode_normalization::{is_nfc_quick, IsNormalized};

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

/// The site a line [`crate::site::to_line`] wrote names, and when it was
/// crawled, read without checking the rest: enough to note where a crawl
/// of a site is, for much less work than [`crate::site::from_line`].
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

/// The item a clean site sends of the item with all it holds and nothing
/// of the node's own ([`crate::site::to_item`]):
/// its text cut to the format's 1,000 characters, a redirect to a site no
/// item can name left out, and fields shed until it fits. Errors when it
/// would not be a valid item.
pub(crate) fn sent_item(mut item: Item) -> Result<Item> {
    item.text = item
        .text
        .map(|text| match text.char_indices().nth(MAX_TEXT_CHARS) {
            Some((cut, _)) => text[..cut].trim_end().to_string(),
            None => text,
        });
    let unnamed = |redirect: &Value| !redirect["to"].as_str().is_some_and(is_domain);
    if item.fields.get("redirect").is_some_and(unnamed) {
        item.fields.remove("redirect");
    }
    if item.url.is_none() && is_domain(&item.id) {
        item.site = Some(item.id.clone());
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

pub(crate) fn edge(rel: &str, to: Ref, text: Option<String>) -> Edge {
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
pub(crate) fn canonical_owned(url: String) -> Option<String> {
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
pub(crate) fn plainly_canonical(url: &str) -> bool {
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
pub(crate) fn tidy_keyword(text: String) -> Option<String> {
    if text.contains('\n') {
        tidy(text.replace('\n', " "))
    } else {
        tidy(text)
    }
}

pub(crate) fn tidy_all(list: Vec<String>) -> Vec<String> {
    list.into_iter().filter_map(tidy).collect()
}

pub(crate) fn tidy_keywords(list: Vec<String>) -> Vec<String> {
    list.into_iter().filter_map(tidy_keyword).collect()
}

/// `list` with each text kept the first time it comes.
pub(crate) fn once(list: Vec<String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::with_capacity(list.len());
    for text in list {
        if !kept.contains(&text) {
            kept.push(text);
        }
    }
    kept
}

pub(crate) fn string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

pub(crate) fn strings(value: Option<&Value>) -> Vec<String> {
    let list = value.and_then(Value::as_array).into_iter().flatten();
    list.filter_map(|value| string(Some(value))).collect()
}

/// A whole number that fits `T`, `None` for anything else.
pub(crate) fn number<T: TryFrom<u64>>(value: Option<&Value>) -> Option<T> {
    T::try_from(value?.as_u64()?).ok()
}

fn too_big(item: &Item) -> bool {
    serde_json::to_vec(item).map_or(true, |json| json.len() > MAX_ITEM_BYTES)
}

/// `unix` seconds as a UTC timestamp, `YYYY-MM-DDTHH:MM:SSZ`, the other
/// way from [`unix_seconds`]; `None` past the year 9999.
pub(crate) fn timestamp(unix: u64) -> Option<String> {
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
    use crate::site::tests::full_site;
    use crate::site::{self, from_line, from_sent, to_item, to_line, CrawlMarks};
    use crate::{KeyPage, LinkText, Redirect, Signals};

    /// `site` without the node's own parts, which an item sent elsewhere
    /// leaves out: its crawl marks, icon and headlines.
    fn as_carried(mut site: Item) -> Item {
        for name in ["crawl", "icon", "news"] {
            site.fields.remove(name);
        }
        site
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
        let item = to_item(&full_site()).unwrap();
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
            json!({"text": "python", "count": 39, "linkers": "00a1b2c3d4e5f607"})
        );
        assert_eq!(item.weight, Some(412.0));
        assert_eq!(
            item.fields["redirect"],
            json!({"to": "python.com", "at": "2026-10-10T04:12:00Z", "inner": true})
        );
        assert_eq!(item, as_carried(full_site()));
        assert_eq!(from_sent(&item).unwrap(), item);

        // Through JSON, as a set carries it.
        let json = serde_json::to_string(&item).unwrap();
        let read: Item = serde_json::from_str(&json).unwrap();
        assert_eq!(from_sent(&read).unwrap(), item);
        // As a line, as a batch or a bucket answer carries it.
        let line = to_line(&full_site()).unwrap();
        assert_eq!(line, json);
        assert_eq!(from_line(&line).unwrap(), item);
    }

    #[test]
    fn a_line_is_read_only_as_a_whole_site_that_keeps_the_format_s_rules() {
        let line = |value: Value| from_line(&value.to_string());
        assert!(line(json!({"id": "example.com", "kind": "site"})).is_ok());
        // A site's record JSON, as nodes before v0.4.0 sent them.
        assert!(from_line(r#"{"domain":"example.com","signals":{}}"#).is_err());
        assert!(line(json!({"id": "example.com", "kind": "page"})).is_err());
        assert!(line(json!({"id": "example.com", "extends": "site:example.com"})).is_err());
        let not_canonical =
            json!({"id": "example.com", "kind": "site", "url": "HTTPS://example.com"});
        assert!(line(not_canonical).is_err());
        assert!(from_line("not json").is_err());
    }

    #[test]
    fn a_line_says_its_site_and_crawl_time_without_being_read_whole() {
        let line = to_line(&full_site()).unwrap();
        let crawled = Some(1_791_605_520);
        assert_eq!(crawl_of_line(&line), Some(("python.org".into(), crawled)));
        let never = to_line(&site::new("example.com")).unwrap();
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
        let mut site = site::new("example.com");
        let ranked = Signals {
            tranco_rank: Some(90_000),
            ..Signals::default()
        };
        site::set_signals(&mut site, &ranked);
        site::add_alias(&mut site, "Example");
        let item = to_item(&site).unwrap();
        assert_eq!(item.url, None);
        assert_eq!(item.site.as_deref(), Some("example.com"));
        check_fields(&item);
        assert_eq!(
            serde_json::to_string(&item).unwrap(),
            r#"{"id":"example.com","kind":"site","site":"example.com","names":["Example"],"fields":{"ranks":{"tranco":90000}}}"#
        );
        let mut sent = from_sent(&item).unwrap();
        assert_eq!(sent.site.take(), None);
        assert_eq!(sent, site);
        let bare = to_item(&site::new("example.com")).unwrap();
        assert_eq!(from_sent(&bare).unwrap(), site::new("example.com"));
    }

    #[test]
    fn text_and_urls_are_made_fit_for_an_item() {
        let mut site = site::new("example.com");
        // Decomposed é, a tab, and a URL with a default port.
        site.title = Some("Cafe\u{301}\tMenu".into());
        site.url = Some("https://example.com:443/a/../b".into());
        site.text = Some("word ".repeat(400));
        site.lang = Some("not a language".into());
        site::set_terms(&mut site, vec!["two\nlines".into(), " ".into()]);
        let pages = vec![
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
        site::set_key_pages(&mut site, pages);
        let links = vec!["foo_bar.blogspot.com".into(), "example.org".into()];
        site::set_links_to(&mut site, links);
        let item = to_item(&site).unwrap();
        let back: Item = serde_json::from_str(&serde_json::to_string(&item).unwrap()).unwrap();
        assert_eq!(
            site::key_pages(&back),
            [KeyPage {
                label: "Menu".into(),
                url: "https://example.com/menu".into(),
            }]
        );
        assert_eq!(site::links_to(&back), ["example.org"]);
        assert_eq!(item.title.as_deref(), Some("Caf\u{e9} Menu"));
        assert_eq!(item.url.as_deref(), Some("https://example.com/b"));
        assert_eq!(item.text.as_ref().unwrap().chars().count(), 999);
        assert_eq!(item.lang, None);
        assert_eq!(item.fields["terms"], json!(["two lines"]));

        site.url = Some("https://example.com/%%41b".into());
        assert_eq!(to_item(&site).unwrap().url, None);
        // A host the format cannot name as a site is no item's site.
        let item = to_item(&site::new("foo_bar.blogspot.com")).unwrap();
        assert_eq!(item.site, None);
    }

    #[test]
    fn a_clean_site_is_in_its_item_s_form() {
        let mut site = full_site();
        site.url = Some("https://www.python.org/?".into());
        site.title = Some(" Welcome to\u{7}Python.org\n".into());
        // Decomposed: the same name and link text as the composed ones.
        site.names.push("Caf\u{e9}".into());
        site.names.push("Cafe\u{301}".into());
        let mut link_texts = site::link_texts(&site);
        link_texts.push(LinkText::from_linkers("Caf\u{e9}", 0b01));
        link_texts.push(LinkText::from_linkers("Cafe\u{301}", 0b10));
        site::set_link_texts(&mut site, &link_texts);
        let mut kinds = site::kinds(&site);
        kinds.push(" programming language\n".into());
        site::set_kinds(&mut site, kinds);
        site.text = Some(format!("{}\u{0}", "word ".repeat(400)));
        site.lang = Some("en_US".into());
        let mut pages = site::key_pages(&site);
        pages[0].url = "https://www.python.org/downloads/#top".into();
        pages.push(KeyPage {
            label: "Odd".into(),
            url: "https://www.python.org/%%41b".into(),
        });
        site::set_key_pages(&mut site, pages);
        let mut links = site::links_to(&site);
        links.extend(["foo_bar.blogspot.com".into(), "index_.shtml".into()]);
        site::set_links_to(&mut site, links);
        let mut redirect = site::redirect(&site).unwrap();
        redirect.to = "WWW.Python.COM".into();
        site::set_redirect(&mut site, Some(redirect));

        let mut clean = site.clone();
        site::clean(&mut clean);
        assert_eq!(clean.url.as_deref(), Some("https://www.python.org/"));
        assert_eq!(clean.title.as_deref(), Some("Welcome to Python.org"));
        assert_eq!(clean.names[2..], ["Caf\u{e9}"]);
        let link_texts = site::link_texts(&clean);
        let cafe = link_texts.iter().find(|lt| lt.text == "Caf\u{e9}");
        assert_eq!(cafe.unwrap().linkers, 0b11);
        assert_eq!(link_texts.len(), 3);
        assert_eq!(site::kinds(&clean), ["programming language"]);
        // The page text is not cut.
        assert_eq!(clean.text.as_ref().unwrap().len(), 1_999);
        assert_eq!(clean.lang, None);
        let full = full_site();
        assert_eq!(site::key_pages(&clean), site::key_pages(&full));
        assert_eq!(site::links_to(&clean), site::links_to(&full));
        assert_eq!(site::redirect(&clean), site::redirect(&full));
        // What the item leaves out stays as it was.
        assert_eq!(site::crawl_marks(&clean), site::crawl_marks(&site));
        assert_eq!(site::icon(&clean), site::icon(&site));
        assert_eq!(site::news(&clean), site::news(&site));

        let mut again = clean.clone();
        site::clean(&mut again);
        assert_eq!(again, clean);

        let item = to_item(&site).unwrap();
        assert_eq!(item, to_item(&clean).unwrap());
        let mut sent = as_carried(clean.clone());
        sent.text = item.text.clone();
        assert_eq!(from_sent(&item).unwrap(), sent);

        let mut set = crate::sites::Sites::new();
        set.upsert(site);
        assert_eq!(set.get("python.org"), Some(clean));

        let mut late = site::new("example.com");
        site::set_crawled(&mut late, Some(253_402_300_800));
        assert_eq!(late, site::new("example.com"));
        // A redirect stays, for folding, to a site no item can name too.
        let mut underscore = site::new("example.com");
        let redirect = Redirect {
            to: "foo_bar.com".into(),
            at: 1_791_605_520,
            inner: false,
        };
        site::set_redirect(&mut underscore, Some(redirect.clone()));
        site::clean(&mut underscore);
        assert_eq!(site::redirect(&underscore), Some(redirect));
        let sent = to_item(&underscore).unwrap();
        assert!(!sent.fields.contains_key("redirect"));
    }

    #[test]
    fn a_stored_item_holds_all_a_clean_site_does() {
        let mut site = full_site();
        site.text = Some("word ".repeat(400).trim_end().to_string());
        // Over 16 KiB, which an item sent elsewhere sheds.
        let terms = (0..100)
            .map(|i| format!("term {i} {}", "x".repeat(150)))
            .collect();
        site::set_terms(&mut site, terms);
        let mut redirect = site::redirect(&site).unwrap();
        redirect.to = "foo_bar.com".into();
        site::set_redirect(&mut site, Some(redirect));
        let facts = Signals {
            fact_checks: 0,
            fact_agrees: 2,
            ..site::signals(&site)
        };
        site::set_signals(&mut site, &facts);
        site::clean(&mut site);
        assert!(site.fields.contains_key("crawl"));
        assert_eq!(
            site.fields["fact_checks"],
            json!({"checked": 0, "agreed": 2})
        );
        let json = serde_json::to_string(&site).unwrap();
        let back: Item = serde_json::from_str(&json).unwrap();
        assert_eq!(back, site);
        // A site never crawled has no crawl marks to store.
        let mut bare = site::new("example.com");
        site::set_crawl_marks(&mut bare, CrawlMarks::default());
        assert!(bare.fields.is_empty());
        // Sent elsewhere, the same site is cut down to the format's rules.
        let sent = to_item(&site).unwrap();
        assert!(!sent.fields.contains_key("redirect"));
        assert!(!sent.fields.contains_key("crawl"));
        assert!(!sent.fields.contains_key("icon"));
        assert!(!sent.fields.contains_key("terms"));
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
        let mut site = full_site();
        let terms = (0..100)
            .map(|i| format!("term {i} {}", "x".repeat(150)))
            .collect();
        site::set_terms(&mut site, terms);
        let item = to_item(&site).unwrap();
        assert!(!item.fields.contains_key("terms"));
        assert!(item.fields.contains_key("headings"));

        let headings = (0..8)
            .map(|i| format!("{i} {}", "y".repeat(1_000)))
            .collect();
        site::set_headings(&mut site, headings);
        let link_texts: Vec<LinkText> = (0..32)
            .map(|i| LinkText::with_count(format!("{i} {}", "z".repeat(290)), 1))
            .collect();
        site::set_link_texts(&mut site, &link_texts);
        let item = to_item(&site).unwrap();
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
        assert!(from_sent(&item).is_err());
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
