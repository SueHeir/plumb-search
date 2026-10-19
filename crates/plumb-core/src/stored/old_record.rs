//! A site's record, as versions up to 0.4 kept a site, frozen.
//!
//! Before a node kept each site as its stored item ([`super`]), it kept a
//! record of it, and a line of its records file, its journal or its inbox
//! was the record's JSON: `{"domain": "python.org", "url": ..., "signals":
//! {...}, ...}`. Only that shape is left here, and the two mappings between
//! it and the site's stored item: [`into_item`] and [`of_item`]. Nothing
//! else is done with a record; what is done with a site is done with its
//! item ([`crate::site`]).
//!
//! The shape is frozen, byte for byte, since files hold it and nodes of 0.4
//! and earlier read it. It is read and written only where a site may still
//! be in it:
//!
//! - an old line of a node's files, which reading takes in either shape
//!   ([`super::read_item`], [`super::head`], [`super::item`]);
//! - `/plumb/fill/1`, which answers with each site's record JSON
//!   ([`super::record_json`]) and whose answers are read as such
//!   ([`super::read_record_json`]);
//! - a bucket answer that a browser kept from a node of 0.4 or earlier
//!   (`plumb-private`);
//! - a piece of a lean PIR table, which holds its sites as records' JSON,
//!   the shape the tables were first built and measured in (`plumb-tools`).
//!
//! It goes when the last of those does: the readers of old lines a release
//! after the first that writes every site as its item (step 4b of the data
//! format plan), the fill wire and old buckets once no node of 0.4 or
//! earlier is left, and lean pieces once their tables are built of items.

use std::collections::BTreeMap;

use anyhow::{ensure, Result};
use plumb_format::value::unix_seconds;
use plumb_format::{Item, Ref};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::site_item::{edge, number, string, strings, timestamp, SITE};
use crate::{Headline, KeyPage};

/// A site as its record's JSON holds it. Each field is the part of the
/// site's item named beside it ([`crate::site`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SiteRecord {
    /// The item's `id`.
    pub domain: String,
    /// `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `about`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// [`crate::site::headings`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headings: Vec<String>,
    /// `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_text: Option<String>,
    /// [`crate::site::terms`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terms: Vec<String>,
    /// [`crate::site::link_texts`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub link_texts: Vec<LinkText>,
    /// `names`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// [`crate::site::signals`].
    #[serde(default)]
    pub signals: Signals,
    /// [`crate::site::crawled`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crawled_at: Option<u64>,
    /// [`crate::site::CrawlMarks::tried`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crawl_attempted_at: Option<u64>,
    /// [`crate::site::country`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// [`crate::site::kinds`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    /// [`crate::site::wikidata_about`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// [`crate::site::intro`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intro: Option<String>,
    /// [`crate::site::summary`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// [`crate::site::search_url`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_url: Option<String>,
    /// `lang`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// [`crate::site::key_pages`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_pages: Vec<KeyPage>,
    /// [`crate::site::links_to`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links_to: Vec<String>,
    /// [`crate::site::CrawlMarks::failures`].
    #[serde(default, skip_serializing_if = "is_zero")]
    pub crawl_failures: u32,
    /// [`crate::site::crawl_version`].
    #[serde(default, skip_serializing_if = "is_zero")]
    pub crawl_version: u32,
    /// [`crate::site::CrawlMarks::gone`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gone_at: Option<u64>,
    /// [`crate::site::redirect`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<Redirect>,
    /// [`crate::site::icon`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// [`crate::site::news`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub news: Vec<Headline>,
}

/// [`crate::Redirect`] as a record's JSON holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redirect {
    pub to: String,
    pub at: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub inner: bool,
}

/// [`crate::LinkText`] as a record's JSON holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkText {
    pub text: String,
    pub count: u32,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub linkers: u64,
}

/// [`crate::Signals`] as a record's JSON holds them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Signals {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harmonic_rank: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pagerank_rank: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tranco_rank: Option<u32>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub linking_domains: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub official_site: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub sitelinks: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fact_checks: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fact_agrees: u32,
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

/// The site's stored item of `record`, as it is, not cleaned, as a node
/// keeps a site it reads in this shape: its item with all the record holds
/// (the whole page text, every field however big, a redirect to a site no
/// item can name), and fields of its own for the crawl marks
/// ([`crate::site::CrawlMarks`]), the icon and the headlines. What has no
/// such form is left out: a crawl or a redirect past the year 9999.
pub fn into_item(mut record: SiteRecord) -> Item {
    let mut crawl = Map::new();
    if let Some(tried) = record.crawl_attempted_at {
        crawl.insert("tried".into(), json!(tried));
    }
    if record.crawl_failures > 0 {
        crawl.insert("failures".into(), json!(record.crawl_failures));
    }
    if let Some(gone) = record.gone_at {
        crawl.insert("gone".into(), json!(gone));
    }
    let (icon, news) = (record.icon.take(), std::mem::take(&mut record.news));
    let mut item = whole_item(record);
    if !crawl.is_empty() {
        item.fields.insert("crawl".into(), Value::Object(crawl));
    }
    if let Some(icon) = icon {
        item.fields.insert("icon".into(), json!(icon));
    }
    if !news.is_empty() {
        item.fields.insert("news".into(), json!(news));
    }
    item
}

/// The record of a site's stored item, as it is: what [`into_item`] makes
/// an item of. Members and fields of the item a record has no place for,
/// or that do not read, are left out. Errors for an item that is not a
/// site's, or whose headlines do not read.
pub fn of_item(site: &Item) -> Result<SiteRecord> {
    ensure!(site.kind == SITE, "item {:?} is not a site", site.id);
    let field = |name: &str| site.fields.get(name);
    let member = |name: &str, key: &str| field(name).and_then(|value| value.get(key));
    let crawl = |key: &str| member("crawl", key);
    let mut record = SiteRecord {
        domain: site.id.clone(),
        url: site.url.clone(),
        title: site.title.clone(),
        description: site.about.clone(),
        body_text: site.text.clone(),
        aliases: site.names.clone(),
        language: site.lang.clone(),
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
        crawl_attempted_at: crawl("tried").and_then(Value::as_u64),
        crawl_failures: number(crawl("failures")).unwrap_or_default(),
        gone_at: crawl("gone").and_then(Value::as_u64),
        icon: string(field("icon")),
        ..SiteRecord::default()
    };
    if let Some(news) = field("news") {
        record.news = serde_json::from_value(news.clone())?;
    }
    let signals = &mut record.signals;
    signals.linking_domains = site.weight.map_or(0, |weight| weight as u32);
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
            to: string(redirect.get("to"))?,
            at: redirect
                .get("at")
                .and_then(Value::as_str)
                .and_then(unix_seconds)
                .and_then(|unix| u64::try_from(unix).ok())?,
            inner: redirect.get("inner") == Some(&Value::Bool(true)),
        })
    });
    for edge in &site.edges {
        match (edge.rel.as_str(), &edge.to) {
            ("links_to", Ref::Site(to)) => record.links_to.push(to.clone()),
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

/// What [`super::head`] reads of a line of a record's JSON.
pub(super) fn head(line: &[u8]) -> Result<super::Head> {
    #[derive(Deserialize)]
    struct Head {
        domain: String,
        #[serde(default)]
        signals: Signals,
        #[serde(default)]
        redirect: Option<Redirect>,
        #[serde(default)]
        gone_at: Option<u64>,
    }
    let head: Head = serde_json::from_slice(line)?;
    let Signals {
        harmonic_rank,
        pagerank_rank,
        tranco_rank,
        linking_domains,
        official_site,
        sitelinks,
        fact_checks,
        fact_agrees,
    } = head.signals;
    Ok(super::Head {
        domain: head.domain,
        signals: crate::Signals {
            harmonic_rank,
            pagerank_rank,
            tranco_rank,
            linking_domains,
            official_site,
            sitelinks,
            fact_checks,
            fact_agrees,
        },
        redirect: head
            .redirect
            .map(|Redirect { to, at, inner }| crate::Redirect { to, at, inner }),
        gone_at: head.gone_at,
    })
}

/// The item of a record with all it holds but the node's own: its whole
/// page text, every field, and a redirect to any site.
fn whole_item(record: SiteRecord) -> Item {
    let fields = fields(&record);
    let mut item = Item {
        kind: SITE.into(),
        url: record.url,
        title: record.title,
        about: record.description,
        text: record.body_text,
        lang: record.language,
        names: record.aliases,
        weight: (record.signals.linking_domains > 0)
            .then(|| f64::from(record.signals.linking_domains)),
        fields,
        id: record.domain,
        ..Item::default()
    };
    for page in record.key_pages {
        item.edges
            .push(edge("key_page", Ref::Url(page.url), Some(page.label)));
    }
    for site in record.links_to {
        item.edges.push(edge("links_to", Ref::Site(site), None));
    }
    item
}

/// The item's fields ([`crate::site_item::site_fields`]) of `record`, each
/// only when it says something.
fn fields(record: &SiteRecord) -> BTreeMap<String, Value> {
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
    put("headings", json!(record.headings));
    put("terms", json!(record.terms));
    put("kinds", json!(record.kinds));
    put("search_url", json!(record.search_url));
    let link_texts: Vec<Value> = record
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
    let signals = &record.signals;
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
    if let Some(about) = &record.about {
        wikidata.insert("about".into(), json!(about));
    }
    if let Some(country) = &record.country {
        wikidata.insert("country".into(), json!(country));
    }
    if signals.official_site {
        wikidata.insert("official_site".into(), json!(true));
    }
    if signals.sitelinks > 0 {
        wikidata.insert("sitelinks".into(), json!(signals.sitelinks));
    }
    put("wikidata", Value::Object(wikidata));
    if signals.fact_checks > 0 || signals.fact_agrees > 0 {
        put(
            "fact_checks",
            json!({"checked": signals.fact_checks, "agreed": signals.fact_agrees}),
        );
    }
    put("intro", json!(record.intro));
    put("summary", json!(record.summary));
    put("crawled", json!(record.crawled_at.and_then(timestamp)));
    if record.crawl_version > 0 {
        put("crawl_version", json!(record.crawl_version));
    }
    if let Some(redirect) = &record.redirect {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site::tests::full_site;

    /// The record of [`full_site`], with every field a record holds.
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
                LinkText {
                    text: "python".into(),
                    count: 39,
                    linkers: 0x00a1_b2c3_d4e5_f607,
                },
                LinkText {
                    text: "python.org".into(),
                    count: 3,
                    linkers: 0,
                },
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

    /// The JSON is the one versions up to 0.4 wrote, and `/plumb/fill/1`
    /// still sends, byte for byte.
    #[test]
    fn a_record_s_json_is_as_it_was() {
        assert_eq!(
            serde_json::to_string(&full_record()).unwrap(),
            r#"{"domain":"python.org","url":"https://www.python.org/","title":"Welcome to Python.org","description":"The official home of the Python Programming Language","headings":["Get Started","Download"],"body_text":"Python is a programming language that lets you work quickly.","terms":["python","download python","python"],"link_texts":[{"text":"python","count":39,"linkers":45514025410622983},{"text":"python.org","count":3}],"aliases":["Python","Python Software Foundation"],"signals":{"harmonic_rank":300,"pagerank_rank":250,"tranco_rank":512,"linking_domains":412,"official_site":true,"sitelinks":150,"fact_checks":12,"fact_agrees":11},"crawled_at":1791605520,"crawl_attempted_at":1791605600,"country":"US","kinds":["programming language"],"about":"general-purpose programming language","intro":"Python is a high-level, general-purpose programming language.","summary":"The home of the Python programming language.","search_url":"https://www.python.org/search/?q={searchTerms}","language":"en","key_pages":[{"label":"Downloads","url":"https://www.python.org/downloads/"},{"label":"Docs","url":"https://docs.python.org/3/"}],"links_to":["psf.org","pypi.org"],"crawl_failures":2,"crawl_version":7,"gone_at":1791000000,"redirect":{"to":"python.com","at":1791605520,"inner":true},"icon":"iVBORw0KGgo=","news":[{"title":"Python 3.15 is out","url":"https://www.python.org/news/","at":1791605520}]}"#
        );
        let bare = SiteRecord {
            domain: "example.com".into(),
            ..SiteRecord::default()
        };
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"domain":"example.com","signals":{}}"#
        );
        let failed = SiteRecord {
            crawl_failures: 2,
            ..bare.clone()
        };
        let json = serde_json::to_string(&failed).unwrap();
        assert_eq!(
            json,
            r#"{"domain":"example.com","signals":{},"crawl_failures":2}"#
        );
        assert_eq!(serde_json::from_str::<SiteRecord>(&json).unwrap(), failed);
        // Older lines leave out what they did not know.
        let old: SiteRecord =
            serde_json::from_str(r#"{"domain":"example.com","crawl_attempted_at":5}"#).unwrap();
        assert_eq!((old.crawl_attempted_at, old.crawl_failures), (Some(5), 0));
        let text: LinkText = serde_json::from_str(r#"{"text":"x","count":3}"#).unwrap();
        assert_eq!(text.linkers, 0);
        let json = serde_json::to_string(&full_record()).unwrap();
        assert_eq!(
            serde_json::from_str::<SiteRecord>(&json).unwrap(),
            full_record()
        );
    }

    #[test]
    fn a_record_is_its_site_s_stored_item() {
        assert_eq!(into_item(full_record()), full_site());
        assert_eq!(of_item(&full_site()).unwrap(), full_record());
        let bare = SiteRecord {
            domain: "example.com".into(),
            ..SiteRecord::default()
        };
        assert_eq!(into_item(bare.clone()), crate::site::new("example.com"));
        assert_eq!(of_item(&crate::site::new("example.com")).unwrap(), bare);

        // As it is, not cleaned, but for what no item holds.
        let mut untidy = full_record();
        untidy.title = Some(" Welcome\u{7} to Python ".into());
        untidy.links_to.push("foo_bar.blogspot.com".into());
        untidy.redirect.as_mut().unwrap().to = "foo_bar.com".into();
        let item = into_item(untidy.clone());
        assert_eq!(item.title, untidy.title);
        assert_eq!(crate::site::links_to(&item).len(), 3);
        assert_eq!(crate::site::redirect(&item).unwrap().to, "foo_bar.com");
        assert_eq!(of_item(&item).unwrap(), untidy);
        let mut late = full_record();
        late.crawled_at = Some(300_000_000_000);
        late.redirect.as_mut().unwrap().at = 300_000_000_000;
        let item = into_item(late);
        assert_eq!(crate::site::crawled(&item), None);
        assert_eq!(crate::site::redirect(&item), None);

        let page = Item {
            id: "python.org".into(),
            kind: "page".into(),
            ..Item::default()
        };
        assert!(of_item(&page).is_err());
    }
}
