//! Items of the data format ([`plumb_format`]) read back as the pages of
//! today's page sets, so a node can index a page set it took as items as
//! it indexes the set's articles file.
//!
//! `plumb-tools` writes each page as one item (`plumb_tools::page_items`).
//! What a page is about ([`Page::site`]) is an `about` edge to that site,
//! and where the item's own members say something differently from the
//! page, its `page` field says it as the page has it, so [`page_of_item`]
//! gives the same page back.

use plumb_core::article::{SearchContent, SearchPassage, SearchSymbol, SEARCH_VERSION};
use plumb_core::facts::Fact;
use plumb_core::packages::PackageInfo;
use plumb_core::papers::{ArxivVerification, PaperCorrection, PaperMetadata};
use plumb_core::profiles::Profile;
use plumb_format::{Item, Ref};
use serde_json::{Map, Value};

use crate::pages::Page;

/// The page of the page set `set` that `item` was made from, `None` for an
/// item without a URL.
pub fn page_of_item(item: &Item, set: &str) -> Option<Page> {
    let field = |name: &str| item.fields.get(name);
    let mut page = Page {
        paper: paper_of(item),
        set: set.to_string(),
        url: item.url.clone()?,
        title: item.title.clone().unwrap_or_default(),
        description: item.about.clone(),
        site: about_site(item),
        views: views_of(item.weight),
        aliases: item.names.clone(),
        item: item.ids.get("wd").cloned(),
        profiles: objects(field("profiles"))
            .filter_map(|profile| {
                Some(Profile {
                    service: member(profile, "service")?,
                    id: member(profile, "id")?,
                })
            })
            .collect(),
        website: field("website").and_then(Value::as_str).map(str::to_string),
        package: field("package")
            .and_then(Value::as_object)
            .and_then(package_of),
        facts: objects(field("facts"))
            .filter_map(|fact| {
                Some(Fact {
                    kind: serde_json::from_value(fact.get("kind")?.clone()).ok()?,
                    value: member(fact, "value")?,
                })
            })
            .collect(),
        lead: item.text.clone(),
        names: texts(field("other_names")),
        sections: texts(field("sections")),
        content_language: None,
        search: search_of(item),
    };
    let given = field("page").and_then(Value::as_object);
    if let Some(given) = given {
        take_given(&mut page, given);
    }
    if given.is_none_or(|given| !given.contains_key("content_language")) {
        // The item's language, unless it is the one the set implies.
        if item.lang.as_deref() != page.language() {
            page.content_language = item.lang.clone();
        }
    }
    Some(page)
}

/// The member of an item's `page` field that lists the members written
/// escaped ([`unescape`]).
pub const ESCAPED: &str = "escaped";

/// The text that `text` says escaped, `None` when it is not one: `\\` is
/// `\`, and `\u{hex}` the character with that code. Text the format does
/// not allow (not in NFC, or with control characters but newlines) is
/// written so in an item's `page` field.
pub fn unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            'u' if chars.next()? == '{' => {
                let hex: String = chars.by_ref().take_while(|c| *c != '}').collect();
                if hex.is_empty() || hex.len() > 6 {
                    return None;
                }
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Sets each member of `page` that `given` says as the page set has it.
fn take_given(page: &mut Page, given: &Map<String, Value>) {
    let escaped = texts(given.get(ESCAPED));
    let text_of = |name: &str, text: &str| {
        if escaped.iter().any(|member| member == name) {
            unescape(text)
        } else {
            Some(text.to_string())
        }
    };
    for (name, value) in given {
        let text = value.as_str().and_then(|text| text_of(name, text));
        let list = || -> Vec<String> {
            texts(Some(value))
                .iter()
                .filter_map(|text| text_of(name, text))
                .collect()
        };
        match name.as_str() {
            "url" => page.url = text.unwrap_or_else(|| page.url.clone()),
            "title" => page.title = text.unwrap_or_else(|| page.title.clone()),
            "description" => page.description = text,
            "site" => page.site = text,
            "item" => page.item = text,
            "website" => page.website = text,
            "lead" => page.lead = text,
            "content_language" => page.content_language = text,
            "aliases" => page.aliases = list(),
            "names" => page.names = list(),
            "sections" => page.sections = list(),
            _ => {}
        }
    }
}

/// The site an item is about: its `about` edge to a `site:` ref.
fn about_site(item: &Item) -> Option<String> {
    item.edges.iter().find_map(|edge| match &edge.to {
        Ref::Site(domain) if edge.rel == "about" => Some(domain.clone()),
        _ => None,
    })
}

/// The paper an item's fields hold: `authors`, `venue`, `paper`,
/// `corrections` and `verified_arxiv`.
fn paper_of(item: &Item) -> Option<PaperMetadata> {
    let paper = item.fields.get("paper")?.as_object()?;
    let text = |name: &str| member(paper, name);
    let year = |object: &Map<String, Value>, name: &str| {
        object
            .get(name)
            .and_then(Value::as_i64)
            .and_then(|year| i32::try_from(year).ok())
    };
    Some(PaperMetadata {
        verified_arxiv: item
            .fields
            .get("verified_arxiv")
            .and_then(Value::as_object)
            .and_then(|verified| {
                Some(ArxivVerification {
                    id: member(verified, "id")?,
                    title: member(verified, "title")?,
                    authors: texts(verified.get("authors")),
                    submitted: member(verified, "submitted")?,
                    updated: member(verified, "updated"),
                })
            }),
        doi: text("doi"),
        arxiv_id: text("arxiv_id"),
        openalex_id: text("openalex_id"),
        authors: texts(paper.get("authors").or(item.fields.get("authors"))),
        publication_date: text("publication_date"),
        raw_publication_date: text("raw_publication_date"),
        publication_year: year(paper, "publication_year"),
        preprint_date: text("preprint_date"),
        version_date: text("version_date"),
        preprint_version_date: text("preprint_version_date"),
        venue: text("venue").or_else(|| {
            item.fields
                .get("venue")
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        source: text("source").unwrap_or_default(),
        count_kind: paper
            .get("count_kind")
            .and_then(|kind| serde_json::from_value(kind.clone()).ok())
            .unwrap_or_default(),
        count: paper.get("count").and_then(Value::as_u64).unwrap_or(0),
        alternate_urls: texts(paper.get("alternate_urls")),
        corrections: objects(item.fields.get("corrections"))
            .map(|c| PaperCorrection {
                reason: member(c, "reason").unwrap_or_default(),
                source_url: member(c, "source_url").unwrap_or_default(),
                previous_item: member(c, "previous_item"),
                previous_title: member(c, "previous_title").unwrap_or_default(),
                previous_description: member(c, "previous_description"),
                previous_authors: texts(c.get("previous_authors")),
                previous_publication_date: member(c, "previous_publication_date"),
                previous_raw_publication_date: member(c, "previous_raw_publication_date"),
                previous_publication_year: year(c, "previous_publication_year"),
            })
            .collect(),
    })
}

fn package_of(package: &Map<String, Value>) -> Option<PackageInfo> {
    Some(PackageInfo {
        registry: member(package, "registry")?,
        name: member(package, "name")?,
        version: member(package, "version"),
        released: member(package, "released"),
        license: member(package, "license"),
        docs: member(package, "docs"),
        repo: member(package, "repo"),
        homepage: member(package, "homepage"),
    })
}

fn search_of(item: &Item) -> Option<SearchContent> {
    let symbols: Vec<SearchSymbol> = objects(item.fields.get("symbols"))
        .filter_map(|symbol| {
            Some(SearchSymbol {
                identifier: member(symbol, "identifier")?,
                anchor: member(symbol, "anchor"),
            })
        })
        .collect();
    let passages: Vec<SearchPassage> = objects(item.fields.get("passages"))
        .filter_map(|passage| {
            Some(SearchPassage {
                heading: member(passage, "heading").unwrap_or_default(),
                text: member(passage, "text")?,
                anchor: member(passage, "anchor"),
            })
        })
        .collect();
    (!symbols.is_empty() || !passages.is_empty()).then_some(SearchContent {
        version: SEARCH_VERSION,
        symbols,
        passages,
    })
}

/// The objects of a list field.
fn objects(value: Option<&Value>) -> impl Iterator<Item = &Map<String, Value>> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
}

/// The texts of a list.
fn texts(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn member(object: &Map<String, Value>, name: &str) -> Option<String> {
    object.get(name).and_then(Value::as_str).map(str::to_string)
}

/// A page's views for an item's `weight`.
fn views_of(weight: Option<f64>) -> u64 {
    weight
        .filter(|weight| weight.is_finite() && *weight > 0.0)
        .map_or(0, |weight| weight.round().min(u64::MAX as f64) as u64)
}
