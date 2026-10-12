//! Today's page sets read as items of the data format ([`plumb_format`]),
//! so a set can be published in it without being rebuilt, and items of
//! other sets made into pages, so the page index can search them
//! ([`item_page`]).
//!
//! Each page becomes one item ([`page_item`]) whose extra fields a page
//! set's manifest declares with [`page_fields`]. What a page is about
//! ([`Page::site`]) becomes an `about` edge to that site, and the item's
//! own `site` is the site its URL is on, as a node works it out. The item
//! gives the same page back ([`page_of_item`]), so a node can index a page
//! set it took as items as it indexes the set's articles file.
//!
//! Ids follow the format's rule of the source's own stable id: the
//! Wikidata item of a Wikidata or films page, `<registry>:<name>` of a
//! package, a paper's DOI. Every other page is named by its canonical URL,
//! Wikipedia articles included: the articles files do not carry the page
//! id the format names an article by, so until they do a renamed article
//! gets a new id.

use std::collections::BTreeMap;

use plumb_core::article::{SearchContent, SearchPassage, SearchSymbol, SEARCH_VERSION};
use plumb_core::facts::Fact;
use plumb_core::packages::PackageInfo;
use plumb_core::papers::{ArxivVerification, PaperCorrection, PaperCountKind, PaperMetadata};
use plumb_core::profiles::Profile;
use plumb_format::item::{MAX_ITEM_BYTES, MAX_TEXT_CHARS};
use plumb_format::refs::{is_domain, is_namespace};
use plumb_format::urls::canonical_url;
use plumb_format::value::{is_date, is_language, normalize_text};
use plumb_format::{Edge, FieldSpec, Item, Ref};
use serde_json::{json, Map, Value};

use crate::merge::Merged;
use crate::pages::{
    ItemSetPage, Page, BOOKS_SET, DOCS_SET, FILMS_SET, GITHUB_SET, MUSIC_SET, PACKAGES_SET,
    PAPERS_SET, PODCASTS_SET, STACKEXCHANGE_SET, STACKOVERFLOW_SET, WIKIDATA_SET, WIKTIONARY_SET,
};

/// Fields left out of an item, in this order, while it is over the size
/// cap: the ones a result can most do without.
const SHED: &[&str] = &["passages", "symbols", "sections", "facts", "profiles"];

/// The kind of record `page` is.
pub fn kind_of(page: &Page) -> &'static str {
    match page.set.as_str() {
        GITHUB_SET => "repo",
        STACKOVERFLOW_SET | STACKEXCHANGE_SET => "question",
        BOOKS_SET => "book",
        PODCASTS_SET => "podcast",
        PAPERS_SET => "paper",
        WIKIDATA_SET => "entity",
        WIKTIONARY_SET => "word",
        PACKAGES_SET => "package",
        MUSIC_SET if page.is_song() => "song",
        MUSIC_SET => "album",
        FILMS_SET if page.is_show() => "show",
        FILMS_SET => "film",
        DOCS_SET => "docs-page",
        set if set.starts_with("wikipedia-") => "article",
        _ => "page",
    }
}

/// The site a node gives `item`: the site of the host its URL is on, by
/// the site rules this node uses, whatever its publisher wrote. An item
/// with no URL keeps the site it names. `None` when that is not a domain,
/// as for an IP address.
pub fn node_site(item: &Item) -> Option<String> {
    match &item.url {
        Some(url) => site_of_url(url),
        None => item.site.clone().filter(|site| is_domain(site)),
    }
}

fn site_of_url(url: &str) -> Option<String> {
    plumb_core::registrable_domain(url).filter(|site| is_domain(site))
}

/// `page` as an item, or `None` when its URL has no canonical form or the
/// item fails the format's checks even without its optional fields (an id
/// longer than the format allows).
///
/// [`page_of_item`] gives the page back from it. Where the item's own
/// members say something differently from the page (a URL not in its
/// canonical form, a description with a space at its end), the `page`
/// field says it as the page has it, so the two are the same page member
/// for member unless the page's text is not text the format allows, or
/// the item is over the size cap.
pub fn page_item(page: &Page) -> Option<Item> {
    let mut item = page_stub(page)?;
    item.title = clean(&page.title);
    item.site = item.url.as_deref().and_then(site_of_url);
    item.about = page.description.as_deref().and_then(clean);
    item.text = page
        .lead
        .as_deref()
        .and_then(clean)
        .map(|lead| lead.chars().take(MAX_TEXT_CHARS).collect());
    item.lang = page
        .language()
        .filter(|lang| is_language(lang))
        .map(str::to_string);
    item.weight = Some(page.views as f64);
    item.names = names_of(&page.aliases, item.title.iter());
    if let Some(site) = page.site.as_deref() {
        if let Ok(to) = format!("site:{site}").parse() {
            item.edges.push(Edge {
                rel: "about".into(),
                to,
                text: None,
                w: None,
                other: BTreeMap::new(),
            });
        }
    }
    add_fields(page, &mut item);
    // Taking the URL as given can change the language a set implies, so
    // a second look can find more to keep.
    for _ in 0..3 {
        let Some(back) = page_of_item(&item, &page.set) else {
            break;
        };
        let mut given = match item.fields.get("page") {
            Some(Value::Object(given)) => given.clone(),
            _ => Map::new(),
        };
        if !note_given(&mut given, page, &back) {
            break;
        }
        item.fields.insert("page".into(), Value::Object(given));
    }
    while too_big(&item) {
        let Some(field) = SHED.iter().find(|f| item.fields.contains_key(**f)) else {
            break;
        };
        item.fields.remove(*field);
    }
    item.check().is_ok().then_some(item)
}

/// The page of the page set `set` that [`page_item`] made `item` from,
/// `None` for an item without a URL.
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
        item_set: None,
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

/// The members `back` has otherwise than `page`, by their names in a page's
/// JSON: what [`page_of_item`] did not give back of a page.
pub fn differing_members(page: &Page, back: &Page) -> Vec<String> {
    let (Ok(Value::Object(page)), Ok(Value::Object(back))) =
        (serde_json::to_value(page), serde_json::to_value(back))
    else {
        return vec!["page".into()];
    };
    let mut names: Vec<String> = page
        .iter()
        .filter(|(name, value)| back.get(*name) != Some(value))
        .map(|(name, _)| name.clone())
        .chain(
            back.keys()
                .filter(|name| !page.contains_key(*name))
                .cloned(),
        )
        .collect();
    names.sort();
    names
}

/// Notes in `given` each member of `page` that `back`, the page its item
/// gives, has otherwise, as `page` has it. False when there was nothing
/// new to note.
fn note_given(given: &mut Map<String, Value>, page: &Page, back: &Page) -> bool {
    let before = given.clone();
    let mut text = |name: &str, page: Option<&String>, back: Option<&String>| {
        if let Some(text) = page.map(|text| normalize_text(text)) {
            if back != Some(&text) {
                given.insert(name.into(), Value::from(text));
            }
        }
    };
    text("url", Some(&page.url), Some(&back.url));
    text("title", Some(&page.title), Some(&back.title));
    text(
        "description",
        page.description.as_ref(),
        back.description.as_ref(),
    );
    text("site", page.site.as_ref(), back.site.as_ref());
    text("item", page.item.as_ref(), back.item.as_ref());
    text("website", page.website.as_ref(), back.website.as_ref());
    text("lead", page.lead.as_ref(), back.lead.as_ref());
    text(
        "content_language",
        page.content_language.as_ref(),
        back.content_language.as_ref(),
    );
    for (name, page, back) in [
        ("aliases", &page.aliases, &back.aliases),
        ("names", &page.names, &back.names),
        ("sections", &page.sections, &back.sections),
    ] {
        let list: Vec<String> = page.iter().map(|text| normalize_text(text)).collect();
        if list != *back {
            given.insert(name.into(), json!(list));
        }
    }
    *given != before
}

/// Sets each member of `page` that `given` says as the page set has it.
fn take_given(page: &mut Page, given: &Map<String, Value>) {
    for (name, value) in given {
        let text = value.as_str().map(str::to_string);
        match name.as_str() {
            "url" => page.url = text.unwrap_or_else(|| page.url.clone()),
            "title" => page.title = text.unwrap_or_else(|| page.title.clone()),
            "description" => page.description = text,
            "site" => page.site = text,
            "item" => page.item = text,
            "website" => page.website = text,
            "lead" => page.lead = text,
            "content_language" => page.content_language = text,
            "aliases" => page.aliases = texts(Some(value)),
            "names" => page.names = texts(Some(value)),
            "sections" => page.sections = texts(Some(value)),
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

/// `names` cleaned, each once, leaving out those in `taken`.
fn names_of<'a>(names: &[String], taken: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut seen: Vec<String> = taken.cloned().collect();
    let mut kept = Vec::new();
    for name in names.iter().filter_map(|name| clean(name)) {
        if !seen.contains(&name) {
            seen.push(name.clone());
            kept.push(name);
        }
    }
    kept
}

/// What a layer finds `page` by ([`crate::merge::Layers::on`]): the id,
/// kind, URL and outside ids of [`page_item`], without the rest, which
/// takes far longer to make.
pub fn page_stub(page: &Page) -> Option<Item> {
    let url = canonical_url(&page.url)?;
    let wd = page
        .item
        .as_deref()
        .filter(|item| format!("wd:{item}").parse::<Ref>().is_ok());
    let mut item = Item {
        id: id_of(page, wd, &url),
        kind: kind_of(page).to_string(),
        url: Some(url),
        ..Item::default()
    };
    if let Some(wd) = wd {
        item.ids.insert("wd".into(), wd.to_string());
    }
    if let Some(package) = page
        .package
        .as_ref()
        .filter(|package| is_namespace(&package.registry))
    {
        item.ids
            .insert(package.registry.clone(), package.name.clone());
    }
    if let Some(paper) = &page.paper {
        for (namespace, id) in [
            ("doi", paper.doi.as_deref().map(str::to_lowercase)),
            ("arxiv", paper.arxiv_id.clone()),
            ("openalex", paper.openalex_id.clone()),
        ] {
            if let Some(id) = id.filter(|id| !id.is_empty()) {
                item.ids.insert(namespace.into(), id);
            }
        }
    }
    Some(item)
}

/// What the `fields` of a page set's items are, for its manifest. `page`
/// holds what [`page_item`] says of a page as the page set has it.
pub fn page_fields() -> BTreeMap<String, FieldSpec> {
    let object_list = json!({"type": "object", "list": true, "roles": ["show"]});
    serde_json::from_value(json!({
        "website": {"type": "url", "roles": ["show"]},
        "other_names": {"type": "text", "list": true, "roles": ["search:name"]},
        "profiles": object_list,
        "facts": object_list,
        "package": {"type": "object", "roles": ["show"]},
        "sections": {"type": "text", "list": true, "roles": ["search:about"]},
        "symbols": {"type": "object", "list": true, "roles": ["search:body"]},
        "passages": {"type": "object", "list": true, "roles": ["search:body"]},
        "authors": {"type": "text", "list": true, "same_as": "wd:P50", "roles": ["search:about", "show"]},
        "venue": {"type": "text", "same_as": "wd:P1433", "roles": ["show"]},
        "paper": {"type": "object", "roles": ["show"]},
        "corrections": {"type": "object", "list": true},
        "verified_arxiv": {"type": "object"},
        "page": {"type": "object"},
    }))
    .expect("the page fields are well formed")
}

fn id_of(page: &Page, wd: Option<&str>, url: &str) -> String {
    match page.set.as_str() {
        WIKIDATA_SET | FILMS_SET => wd.map(str::to_string),
        PACKAGES_SET => page
            .package
            .as_ref()
            .map(|package| format!("{}:{}", package.registry, package.name)),
        PAPERS_SET => page.paper.as_ref().and_then(|paper| {
            paper
                .doi
                .as_deref()
                .map(|doi| doi.trim_start_matches("https://doi.org/").to_lowercase())
                .or_else(|| paper.openalex_id.clone())
        }),
        _ => None,
    }
    .filter(|id| !id.is_empty())
    .unwrap_or_else(|| url.to_string())
}

fn add_fields(page: &Page, item: &mut Item) {
    let fields = &mut item.fields;
    if let Some(website) = page.website.as_deref().and_then(canonical_url) {
        if item.url.as_ref() != Some(&website) {
            fields.insert("website".into(), json!(website));
        }
    }
    let taken = item.title.iter().chain(&item.names);
    let other_names = names_of(&page.names, taken);
    put_list(
        fields,
        "other_names",
        other_names.into_iter().map(Value::from).collect(),
    );
    let profiles: Vec<Value> = page
        .profiles
        .iter()
        .map(|p| object(&[("service", Some(&p.service)), ("id", Some(&p.id))]))
        .collect();
    put_list(fields, "profiles", profiles);
    let facts: Vec<Value> = page
        .facts
        .iter()
        .filter_map(|fact| {
            let kind = serde_json::to_value(fact.kind).ok()?;
            Some(object(&[
                ("kind", kind.as_str().map(str::to_string).as_ref()),
                ("value", Some(&fact.value)),
            ]))
        })
        .collect();
    put_list(fields, "facts", facts);
    let sections: Vec<Value> = page
        .sections
        .iter()
        .filter_map(|s| clean(s))
        .map(Value::from)
        .collect();
    put_list(fields, "sections", sections);
    if let Some(package) = &page.package {
        fields.insert(
            "package".into(),
            object(&[
                ("registry", Some(&package.registry)),
                ("name", Some(&package.name)),
                ("version", package.version.as_ref()),
                ("released", package.released.as_ref()),
                ("license", package.license.as_ref()),
                ("docs", package.docs.as_ref()),
                ("repo", package.repo.as_ref()),
                ("homepage", package.homepage.as_ref()),
            ]),
        );
    }
    if let Some(search) = &page.search {
        let symbols: Vec<Value> = search
            .symbols
            .iter()
            .map(|s| {
                object(&[
                    ("identifier", Some(&s.identifier)),
                    ("anchor", s.anchor.as_ref()),
                ])
            })
            .collect();
        put_list(fields, "symbols", symbols);
        let passages: Vec<Value> = search
            .passages
            .iter()
            .map(|p| {
                object(&[
                    ("heading", Some(&p.heading).filter(|h| !h.is_empty())),
                    ("text", Some(&p.text)),
                    ("anchor", p.anchor.as_ref()),
                ])
            })
            .collect();
        put_list(fields, "passages", passages);
    }
    if let Some(paper) = &page.paper {
        add_paper(paper, fields);
        item.published = paper.publication_date.clone().filter(|date| is_date(date));
    }
}

/// A paper's authors and venue as the fields search and show them, and
/// the rest of what its page set says of it in `paper`, `corrections` and
/// `verified_arxiv`.
fn add_paper(paper: &PaperMetadata, fields: &mut BTreeMap<String, Value>) {
    let authors: Vec<String> = paper.authors.iter().filter_map(|a| clean(a)).collect();
    put_list(
        fields,
        "authors",
        authors.iter().map(|a| json!(a)).collect(),
    );
    let venue = paper.venue.as_deref().and_then(clean);
    if let Some(venue) = &venue {
        fields.insert("venue".into(), json!(venue));
    }
    let mut rest = Map::new();
    if authors != paper.authors {
        rest.insert("authors".into(), text_list(&paper.authors));
    }
    if venue != paper.venue {
        put_text(&mut rest, "venue", paper.venue.as_ref());
    }
    for (name, value) in [
        ("doi", &paper.doi),
        ("arxiv_id", &paper.arxiv_id),
        ("openalex_id", &paper.openalex_id),
        ("publication_date", &paper.publication_date),
        ("raw_publication_date", &paper.raw_publication_date),
        ("preprint_date", &paper.preprint_date),
        ("version_date", &paper.version_date),
        ("preprint_version_date", &paper.preprint_version_date),
    ] {
        put_text(&mut rest, name, value.as_ref());
    }
    if let Some(year) = paper.publication_year {
        rest.insert("publication_year".into(), json!(year));
    }
    put_text(
        &mut rest,
        "source",
        Some(&paper.source).filter(|s| !s.is_empty()),
    );
    if paper.count_kind != PaperCountKind::default() {
        if let Ok(kind) = serde_json::to_value(&paper.count_kind) {
            rest.insert("count_kind".into(), kind);
        }
    }
    if paper.count != 0 {
        rest.insert("count".into(), json!(paper.count));
    }
    if !paper.alternate_urls.is_empty() {
        rest.insert("alternate_urls".into(), text_list(&paper.alternate_urls));
    }
    fields.insert("paper".into(), Value::Object(rest));
    let corrections: Vec<Value> = paper
        .corrections
        .iter()
        .map(|c| {
            let mut object = Map::new();
            for (name, value) in [
                ("reason", Some(&c.reason).filter(|s| !s.is_empty())),
                ("source_url", Some(&c.source_url).filter(|s| !s.is_empty())),
                ("previous_item", c.previous_item.as_ref()),
                (
                    "previous_title",
                    Some(&c.previous_title).filter(|s| !s.is_empty()),
                ),
                ("previous_description", c.previous_description.as_ref()),
                (
                    "previous_publication_date",
                    c.previous_publication_date.as_ref(),
                ),
                (
                    "previous_raw_publication_date",
                    c.previous_raw_publication_date.as_ref(),
                ),
            ] {
                put_text(&mut object, name, value);
            }
            if !c.previous_authors.is_empty() {
                object.insert("previous_authors".into(), text_list(&c.previous_authors));
            }
            if let Some(year) = c.previous_publication_year {
                object.insert("previous_publication_year".into(), json!(year));
            }
            Value::Object(object)
        })
        .collect();
    if !corrections.is_empty() {
        fields.insert("corrections".into(), Value::Array(corrections));
    }
    if let Some(verified) = &paper.verified_arxiv {
        let mut object = Map::new();
        put_text(&mut object, "id", Some(&verified.id));
        put_text(&mut object, "title", Some(&verified.title));
        object.insert("authors".into(), text_list(&verified.authors));
        put_text(&mut object, "submitted", Some(&verified.submitted));
        put_text(&mut object, "updated", verified.updated.as_ref());
        fields.insert("verified_arxiv".into(), Value::Object(object));
    }
}

/// The paper [`add_paper`] wrote into `item`'s fields.
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

/// An object of the members that have a value, each as text the format
/// allows, but otherwise as it is: empty text stays.
fn object(members: &[(&str, Option<&String>)]) -> Value {
    let mut map = Map::new();
    for (name, value) in members {
        put_text(&mut map, name, *value);
    }
    Value::Object(map)
}

fn put_text(map: &mut Map<String, Value>, name: &str, value: Option<&String>) {
    if let Some(value) = value {
        map.insert(name.to_string(), Value::from(normalize_text(value)));
    }
}

fn text_list(texts: &[String]) -> Value {
    texts.iter().map(|text| normalize_text(text)).collect()
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

fn put_list(fields: &mut BTreeMap<String, Value>, name: &str, list: Vec<Value>) {
    let list: Vec<Value> = list
        .into_iter()
        .filter(|v| v.as_object().is_none_or(|map| !map.is_empty()))
        .collect();
    if !list.is_empty() {
        fields.insert(name.into(), Value::Array(list));
    }
}

/// `text` normalized for an item and trimmed, `None` when nothing is left.
fn clean(text: &str) -> Option<String> {
    let text = normalize_text(text);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn too_big(item: &Item) -> bool {
    serde_json::to_vec(item).map_or(true, |json| json.len() > MAX_ITEM_BYTES)
}

/// Where an item set's pages are listed among results: its manifest's
/// `place` (`manifest.md` in the design notes).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Place {
    /// As other pages are: when a query names one. `under-site` is listed
    /// this way too, for now.
    #[default]
    Own,
    /// Only for a query with one of these words, in [`ask_word`] form.
    Asked(Vec<String>),
    /// Never listed.
    AnswerOnly,
}

/// Most words a set may be asked by.
pub const MAX_ASK_WORDS: usize = 8;
/// Longest word, or words, a set may be asked by, in characters.
pub const MAX_ASK_CHARS: usize = 40;

impl Place {
    /// The placement `place` says, or why a node can't read it.
    pub fn read(place: Option<&Value>) -> Result<Place, String> {
        let Some(place) = place else {
            return Ok(Place::Own);
        };
        let how = place
            .get("how")
            .and_then(Value::as_str)
            .ok_or("its place says no how")?;
        match how {
            "own" | "under-site" => Ok(Place::Own),
            "answer-only" => Ok(Place::AnswerOnly),
            "asked" => {
                let words = place
                    .get("ask")
                    .and_then(Value::as_array)
                    .ok_or("an asked place needs its ask words")?;
                let mut ask: Vec<String> = Vec::new();
                for word in words {
                    let word = word.as_str().map(ask_word).ok_or("ask words are text")?;
                    if word.is_empty() || word.chars().count() > MAX_ASK_CHARS {
                        return Err(format!("{word:?} can't be an ask word"));
                    }
                    if !ask.contains(&word) {
                        ask.push(word);
                    }
                }
                if ask.is_empty() || ask.len() > MAX_ASK_WORDS {
                    return Err(format!("an asked place has 1 to {MAX_ASK_WORDS} ask words"));
                }
                Ok(Place::Asked(ask))
            }
            other => Err(format!(
                "place {other:?} is not own, under-site, asked or answer-only"
            )),
        }
    }
}

/// `text` as ask words are compared: lower case, with single spaces.
pub fn ask_word(text: &str) -> String {
    plumb_core::collapse_whitespace(text.trim()).to_lowercase()
}

/// What the page index takes from an item set's manifest for its pages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetView {
    /// `<scope>/<name>`, which becomes the pages' [`Page::set`].
    pub set: String,
    pub title: Option<String>,
    /// The line under a page's title, `show.about`, with `{field}` slots.
    pub about: Option<String>,
    pub place: Place,
}

impl SetView {
    /// The view of the set `manifest` describes. A place this version
    /// can't read counts as [`Place::Own`].
    pub fn of(manifest: &plumb_format::Manifest) -> SetView {
        SetView {
            set: manifest.set.clone(),
            title: manifest.title.clone(),
            about: manifest.show.get("about").cloned(),
            place: Place::read(manifest.place.as_ref()).unwrap_or_default(),
        }
    }
}

/// Longest line under the title of a page made from an item, in
/// characters.
pub const MAX_PAGE_ABOUT_CHARS: usize = 300;

/// The page `merged` becomes in the page index, as its set's `view` says.
/// `None` for an item without a title or a URL, which can't be listed.
///
/// Names from the owner and the project are its aliases; names from
/// outside publishers, which count for less (merge rule 7), are its other
/// names ([`Page::names`]), which a query matches less well.
pub fn item_page(merged: &Merged, view: &SetView) -> Option<Page> {
    let item = &merged.item;
    let title = item
        .title
        .as_deref()
        .and_then(clean)
        .filter(|title| title.chars().count() <= 1000)?;
    let url = item.url.clone()?;
    let description = view
        .about
        .as_deref()
        .and_then(|template| show(template, merged))
        .or_else(|| item.about.as_deref().and_then(clean))
        .or_else(|| item.text.as_deref().and_then(clean))
        .map(|line| plumb_core::truncate_chars(&line, MAX_PAGE_ABOUT_CHARS));
    let site = about_site(item);
    let (mut aliases, mut names) = (Vec::new(), Vec::new());
    for (n, name) in item.names.iter().enumerate() {
        if *name == title {
            continue;
        }
        if merged
            .name_weights
            .get(n)
            .is_none_or(|weight| *weight >= 1.0)
        {
            aliases.push(name.clone());
        } else {
            names.push(name.clone());
        }
    }
    let ask = match &view.place {
        Place::Asked(words) => words.clone(),
        Place::Own | Place::AnswerOnly => Vec::new(),
    };
    Some(Page {
        paper: None,
        set: view.set.clone(),
        url,
        title,
        description,
        site,
        views: views_of(item.weight),
        aliases,
        item: item.ids.get("wd").cloned(),
        profiles: Vec::new(),
        website: None,
        package: None,
        facts: Vec::new(),
        lead: None,
        names,
        sections: Vec::new(),
        content_language: item.lang.as_deref().and_then(plumb_core::language_code),
        search: None,
        item_set: Some(ItemSetPage {
            title: view.title.clone(),
            kind: item.kind.clone(),
            ask,
        }),
    })
}

/// `page`, of today's page sets, with what the layers merged onto it
/// ([`page_item`], then [`crate::merge::merge`]) add: the description,
/// views, Wikidata item or language of a layer that ranks above the page's
/// own set, and the names layers add, split as [`item_page`] splits them.
/// Everything else stays as the page has it, so a page no layer names
/// comes back the same.
pub fn layered_page(mut page: Page, merged: &Merged) -> Page {
    let item = &merged.item;
    let from_layer = |name: &str| {
        merged
            .sources
            .get(name)
            .is_some_and(|source| *source != merged.source)
    };
    if from_layer("about") {
        if let Some(about) = item.about.as_deref().and_then(clean) {
            page.description = Some(plumb_core::truncate_chars(&about, MAX_PAGE_ABOUT_CHARS));
        }
    }
    if from_layer("weight") {
        page.views = views_of(item.weight);
    }
    if from_layer("ids.wd") {
        page.item = item.ids.get("wd").cloned();
    }
    if from_layer("lang") {
        if let Some(lang) = item.lang.as_deref().and_then(plumb_core::language_code) {
            page.content_language = Some(lang);
        }
    }
    let mut known: std::collections::HashSet<String> = std::iter::once(&page.title)
        .chain(&page.aliases)
        .chain(&page.names)
        .map(|name| name.to_lowercase())
        .collect();
    for (name, weight) in item.names.iter().zip(&merged.name_weights) {
        if !known.insert(name.to_lowercase()) {
            continue;
        }
        if *weight >= 1.0 {
            page.aliases.push(name.clone());
        } else {
            page.names.push(name.clone());
        }
    }
    page
}

/// A page's views for an item's `weight`.
fn views_of(weight: Option<f64>) -> u64 {
    weight
        .filter(|weight| weight.is_finite() && *weight > 0.0)
        .map_or(0, |weight| weight.round().min(u64::MAX as f64) as u64)
}

/// `template` with each `{name}` slot filled from `merged`: its `title`,
/// `about`, `text`, `lang`, `site` or `published` (the day), or a field
/// of its set. `None` when a slot has no value, so that a line never
/// shows an empty one.
pub fn show(template: &str, merged: &Merged) -> Option<String> {
    let mut line = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        line.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find('}')?;
        line.push_str(&slot(&after[..end], merged)?);
        rest = &after[end + 1..];
    }
    line.push_str(rest);
    let line = plumb_core::collapse_whitespace(line.trim());
    (!line.is_empty()).then_some(line)
}

fn slot(name: &str, merged: &Merged) -> Option<String> {
    let item = &merged.item;
    let text = match name {
        "title" => item.title.clone(),
        "about" => item.about.clone(),
        "text" => item.text.clone(),
        "lang" => item.lang.clone(),
        "site" => item.site.clone(),
        "published" => item
            .published
            .as_deref()
            .map(|date| date.get(..10).unwrap_or(date).to_string()),
        _ => merged
            .fields
            .values()
            .filter(|field| field.name == name)
            .min_by_key(|field| field.source != merged.source)
            .and_then(|field| shown(&field.value)),
    };
    text.filter(|text| !text.trim().is_empty())
}

/// A field's value as a line shows it: lists by their first five values,
/// whole numbers without a point. `None` for objects.
fn shown(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(match number.as_f64() {
            Some(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{}", n as i64),
            _ => number.to_string(),
        }),
        Value::Bool(yes) => Some(if *yes { "yes" } else { "no" }.to_string()),
        Value::Array(list) => {
            let values: Vec<String> = list.iter().filter_map(shown).take(5).collect();
            (!values.is_empty()).then(|| values.join(", "))
        }
        Value::Null | Value::Object(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_format::Manifest;

    fn page(value: Value) -> Page {
        serde_json::from_value(value).unwrap()
    }

    fn manifest(set: &str) -> Manifest {
        let mut manifest: Manifest = serde_json::from_value(json!({
            "format": "plumb-set/1",
            "set": format!("plumb/{set}"),
            "publisher": "ed25519:11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo",
            "generation": "2026-10-12T00:00:00Z",
            "files": [{"path": "items-001.jsonl.zst", "count": 1, "bytes": 1,
                       "sha256": "9f2c3b0e7d6a5f4e3d2c1b0a9f8e7d6c5b4a39281706f5e4d3c2b1a098f7e6d5"}],
        }))
        .unwrap();
        manifest.fields = page_fields();
        manifest.check().unwrap();
        manifest
    }

    #[test]
    fn a_wikipedia_article() {
        let page = page(json!({
            "set": "wikipedia-en",
            "url": "https://en.wikipedia.org/wiki/U.S._Bancorp",
            "title": "U.S. Bancorp",
            "description": "American bank holding company",
            "site": "usbank.com",
            "views": 120000,
            "aliases": ["US Bank", "U.S. Bancorp"],
            "names": ["U.S. Bank", "US Bank"],
            "item": "Q739637",
            "profiles": [{"service": "x", "id": "usbank"}],
            "facts": [{"kind": "founded", "value": "1863"}],
            "lead": "U.S. Bancorp is an American\tbank holding company.",
        }));
        let item = page_item(&page).unwrap();
        manifest("wikipedia-en").check_item(&item).unwrap();
        assert_eq!(item.id, "https://en.wikipedia.org/wiki/U.S._Bancorp");
        assert_eq!(item.kind, "article");
        assert_eq!(item.site.as_deref(), Some("wikipedia.org"));
        assert_eq!(item.names, ["US Bank"]);
        assert_eq!(item.fields["other_names"], json!(["U.S. Bank"]));
        assert_eq!(item.ids["wd"], "Q739637");
        assert_eq!(item.lang.as_deref(), Some("en"));
        assert_eq!(item.edges[0].to.to_string(), "site:usbank.com");
        assert_eq!(
            item.text.as_deref(),
            Some("U.S. Bancorp is an American bank holding company.")
        );
        assert_eq!(
            item.fields["profiles"],
            json!([{"service": "x", "id": "usbank"}])
        );
        assert_eq!(
            item.fields["facts"],
            json!([{"kind": "founded", "value": "1863"}])
        );
    }

    /// A page of each page set, with everything its set can say of it.
    fn pages_of_every_set() -> Vec<Page> {
        let search = json!({"version": 1,
            "symbols": [{"identifier": "str.split", "anchor": "str.split"}, {"identifier": "len"}],
            "passages": [{"heading": "Methods", "text": "Return a list of the words.", "anchor": "m"},
                         {"text": "No heading here."}]});
        [
            json!({"set": "wikipedia-en", "url": "https://en.wikipedia.org/wiki/YouTube_Music",
                "title": "YouTube Music", "description": "Music streaming service",
                "site": "youtube.com", "views": 81234, "aliases": ["YT Music", "Youtube music"],
                "names": ["Google Play Music"], "item": "Q28404534",
                "profiles": [{"service": "x", "id": "youtubemusic"}],
                "website": "https://music.youtube.com/",
                "facts": [{"kind": "founded", "value": "2015-11-12"},
                          {"kind": "owner", "value": "Google"}],
                "lead": "YouTube Music is a music streaming service."}),
            json!({"set": "wikipedia-de", "url": "https://de.wikipedia.org/wiki/K%C3%B6ln",
                "title": "Köln", "views": 9, "content_language": "de"}),
            json!({"set": "github", "url": "https://github.com/rust-lang/rust",
                "title": "rust-lang/rust", "description": "Empowering everyone",
                "site": "rust-lang.org", "views": 100000, "aliases": ["rust"]}),
            json!({"set": "stackoverflow", "url": "https://stackoverflow.com/questions/11227809",
                "title": "Why is processing a sorted array faster?", "description": "java c++",
                "views": 27000, "aliases": ["Branch prediction"], "content_language": "en"}),
            json!({"set": "stackexchange", "url": "https://diy.stackexchange.com/questions/12345",
                "title": "How do I fix a leaky tap?", "views": 40}),
            json!({"set": "books", "url": "https://openlibrary.org/works/OL45804W",
                "title": "Fantastic Mr Fox", "description": "Roald Dahl", "views": 300,
                "content_language": "en"}),
            json!({"set": "podcasts", "url": "https://podcastindex.org/podcast/920666",
                "title": "Podcasting 2.0", "site": "podcasting2.org", "views": 5,
                "profiles": [{"service": "youtube-handle", "id": "podcasting20"}]}),
            json!({"set": "papers",
                "url": "https://doi.org/10.1002/(SICI)1097-4636(199706)35:4<473::AID-JBM7>3.0.CO;2-B",
                "title": "Calcium phosphate cements", "description": "Journal of Biomedical Materials Research",
                "views": 312, "aliases": ["CPC review"],
                "website": "https://arxiv.org/abs/2001.00001",
                "paper": {"doi": "10.1002/(SICI)1097-4636(199706)35:4<473::AID-JBM7>3.0.CO;2-B",
                    "arxiv_id": "2001.00001", "openalex_id": "W2001",
                    "authors": ["Ada Lovelace", " Charles Babbage "],
                    "publication_date": "1997-06-01", "raw_publication_date": "June 1997",
                    "publication_year": 1997, "preprint_date": "2020-01-01",
                    "version_date": "2020-02-01", "preprint_version_date": "2020-02-01",
                    "venue": "J. Biomed. Mater. Res.", "source": "openalex",
                    "count_kind": "citations", "count": 312,
                    "alternate_urls": ["https://example.org/cpc.pdf"],
                    "verified_arxiv": {"id": "2001.00001", "title": "Calcium phosphate cements",
                        "authors": ["Ada Lovelace"], "submitted": "2020-01-01", "updated": null},
                    "corrections": [{"reason": "title", "source_url": "https://doi.org/x",
                        "previous_title": "Calcium cements", "previous_authors": ["A. Lovelace"],
                        "previous_publication_year": 1996}]},
                "search": search.clone()}),
            json!({"set": "papers", "url": "https://openalex.org/W99", "title": "An untitled work",
                "views": 0, "paper": {}}),
            json!({"set": "wikidata", "url": "https://www.wikidata.org/wiki/Q42",
                "title": "Douglas Adams", "description": "English author", "site": "douglasadams.com",
                "views": 140, "aliases": ["Douglas Noël Adams"], "item": "Q42",
                "profiles": [{"service": "x", "id": "dna"}],
                "facts": [{"kind": "born", "value": "1952-03-11"}]}),
            json!({"set": "wiktionary", "url": "https://en.wiktionary.org/wiki/serendipity",
                "title": "serendipity", "description": "An unsought, fortunate discovery", "views": 77}),
            json!({"set": "packages", "url": "https://pypi.org/project/requests/",
                "title": "requests", "description": "HTTP for Humans", "views": 1000,
                "package": {"registry": "pypi", "name": "requests", "version": "2.32.3",
                    "released": "2024-05-29", "license": "Apache-2.0",
                    "docs": "https://requests.readthedocs.io", "repo": "https://github.com/psf/requests",
                    "homepage": "https://requests.readthedocs.io/en/latest/"}}),
            json!({"set": "music", "url": "https://musicbrainz.org/recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69",
                "title": "Bohemian Rhapsody", "description": "Queen", "views": 4000,
                "profiles": [{"service": "spotify-track", "id": "7tFiyTwD0nx5a1eklYtX2J"}]}),
            json!({"set": "films", "url": "https://en.wikipedia.org/wiki/Inception",
                "title": "Inception", "description": "2010 film by Christopher Nolan",
                "views": 90, "item": "Q25188", "aliases": ["Origen"]}),
            json!({"set": "films", "url": "https://www.wikidata.org/wiki/Q999999",
                "title": "Un film", "views": 3, "item": "Q999999", "content_language": "fr"}),
            json!({"set": "docs",
                "url": "https://docs.python.org/3/library/stdtypes.html#str.split",
                "title": "str.split", "description": "Built-in Types", "views": 12,
                "aliases": ["split"], "sections": ["Text Sequence Type", "String Methods"],
                "search": search.clone(), "content_language": "en"}),
            json!({"set": "reference", "url": "https://www.healthline.com/health/sleep",
                "title": "Sleep", "views": 2, "search": search.clone()}),
            json!({"set": "subpages", "url": "https://www.bbc.co.uk/sport/football",
                "title": "Football", "description": "BBC Sport", "views": 1}),
        ]
        .into_iter()
        .map(page)
        .collect()
    }

    #[test]
    fn every_page_set_page_comes_back_from_its_item() {
        let manifest = manifest("mixed");
        for page in pages_of_every_set() {
            let item = page_item(&page).unwrap();
            manifest.check_item(&item).unwrap();
            assert_eq!(
                page_of_item(&item, &page.set).as_ref(),
                Some(&page),
                "{}",
                page.url
            );
            // Through the file, as a node reads it.
            let line = serde_json::to_string(&item).unwrap();
            let read: Item = serde_json::from_str(&line).unwrap();
            assert_eq!(page_of_item(&read, &page.set), Some(page));
        }
    }

    #[test]
    fn what_an_items_members_say_otherwise_comes_back_as_the_page_had_it() {
        let page = page(json!({
            "set": "wikipedia-en", "url": "https://en.wikipedia.org/wiki/Paris",
            "title": " Paris", "description": "Capital of France ", "views": 9,
            "aliases": ["Paris", "City of Light", "City of Light", ""],
            "names": ["City of Light", "Lutetia"], "website": "https://en.wikipedia.org/wiki/Paris",
            "content_language": "en", "lead": "", "sections": [" Name "],
        }));
        let item = page_item(&page).unwrap();
        manifest("wikipedia-en").check_item(&item).unwrap();
        assert_eq!(item.title.as_deref(), Some("Paris"));
        assert_eq!(item.names, ["City of Light"]);
        assert_eq!(item.fields["other_names"], json!(["Lutetia"]));
        assert_eq!(
            item.fields["page"],
            json!({"title": " Paris", "description": "Capital of France ",
                   "aliases": ["Paris", "City of Light", "City of Light", ""],
                   "names": ["City of Light", "Lutetia"], "lead": "",
                   "website": "https://en.wikipedia.org/wiki/Paris",
                   "content_language": "en", "sections": [" Name "]})
        );
        assert_eq!(page_of_item(&item, "wikipedia-en"), Some(page));

        // A page with nothing to say otherwise has no `page` field.
        let plain = pages_of_every_set().remove(0);
        assert!(!page_item(&plain).unwrap().fields.contains_key("page"));

        // Text the format does not allow comes back as it allows it.
        let bell = Page {
            description: Some("Rings\u{7}".into()),
            ..plain.clone()
        };
        let back = page_of_item(&page_item(&bell).unwrap(), &bell.set).unwrap();
        assert_eq!(back.description.as_deref(), Some("Rings "));
        assert_eq!(
            Page {
                description: plain.description.clone(),
                ..back
            },
            plain
        );
    }

    #[test]
    fn stable_ids_where_the_source_has_them() {
        let film = page(json!({
            "set": "films", "url": "https://en.wikipedia.org/wiki/Inception",
            "title": "Inception", "views": 90, "item": "Q25188",
        }));
        let package = page(json!({
            "set": "packages", "url": "https://pypi.org/project/requests/",
            "title": "requests", "views": 5,
            "package": {"registry": "pypi", "name": "requests", "version": "2.32.3",
                        "repo": "https://github.com/psf/requests"},
        }));
        let paper = page(json!({
            "set": "papers", "url": "https://doi.org/10.1000/ABC",
            "title": "A paper", "views": 12,
            "paper": {"doi": "10.1000/ABC", "openalex_id": "W123", "authors": ["Ada Lovelace"],
                      "publication_date": "2020-05-01", "venue": "Nature", "source": "openalex",
                      "count": 12, "alternate_urls": [], "corrections": []},
        }));
        let manifest = manifest("mixed");
        let ids: Vec<String> = [film, package, paper]
            .iter()
            .map(|page| {
                let item = page_item(page).unwrap();
                manifest.check_item(&item).unwrap();
                // Layers find a page by its stub as by its item.
                let stub = page_stub(page).unwrap();
                assert_eq!(
                    (&stub.id, &stub.url, &stub.ids),
                    (&item.id, &item.url, &item.ids)
                );
                item.id
            })
            .collect();
        assert_eq!(ids, ["Q25188", "pypi:requests", "10.1000/abc"]);
        let item = page_item(&page(json!({
            "set": "papers", "url": "https://doi.org/10.1000/ABC", "title": "A paper",
            "views": 12, "paper": {"doi": "10.1000/ABC", "arxiv_id": "2001.00001",
            "authors": ["Ada Lovelace"], "publication_date": "2020-05-01",
            "source": "openalex", "count": 12, "alternate_urls": [], "corrections": []},
        })))
        .unwrap();
        assert_eq!(item.ids["doi"], "10.1000/abc");
        assert_eq!(item.ids["arxiv"], "2001.00001");
        assert_eq!(item.published.as_deref(), Some("2020-05-01"));
        assert_eq!(item.fields["authors"], json!(["Ada Lovelace"]));
    }

    #[test]
    fn oversized_pages_shed_their_passages() {
        let passages: Vec<Value> = (0..40)
            .map(|n| json!({"heading": format!("Part {n}"), "text": "word ".repeat(100)}))
            .collect();
        let page = page(json!({
            "set": "docs", "url": "https://docs.python.org/3/tutorial/",
            "title": "The Python Tutorial", "views": 3,
            "sections": ["Whetting Your Appetite"],
            "search": {"version": 1, "symbols": [], "passages": passages},
        }));
        let item = page_item(&page).unwrap();
        manifest("docs").check_item(&item).unwrap();
        assert_eq!(item.kind, "docs-page");
        assert!(!item.fields.contains_key("passages"));
        assert_eq!(item.fields["sections"], json!(["Whetting Your Appetite"]));
    }

    #[test]
    fn pages_without_a_web_address_or_with_too_long_an_id_are_skipped() {
        let long = format!("https://example.com/{}", "a".repeat(300));
        for url in ["ftp://example.com/x", long.as_str()] {
            let page = page(json!({"set": "docs", "url": url, "title": "x", "views": 0}));
            assert_eq!(page_item(&page), None, "{url}");
        }
    }

    #[test]
    fn the_node_works_out_an_items_site_from_its_url() {
        let cases = [
            (
                json!({"id": "a", "kind": "page", "url": "https://blog.example.org/x",
                       "site": "elsewhere.example"}),
                Some("example.org"),
            ),
            (
                json!({"id": "b", "kind": "page", "url": "https://news.ycombinator.com/item"}),
                Some("news.ycombinator.com"),
            ),
            (
                json!({"id": "c", "kind": "page", "url": "http://192.0.2.1/"}),
                None,
            ),
            (
                json!({"id": "d", "kind": "note", "site": "python.org"}),
                Some("python.org"),
            ),
            (json!({"id": "e", "kind": "note"}), None),
        ];
        for (value, site) in cases {
            let item = item(value);
            assert_eq!(node_site(&item).as_deref(), site, "{}", item.id);
        }
    }

    fn item(value: Value) -> Item {
        serde_json::from_value(value).unwrap()
    }

    fn sources() -> Vec<crate::merge::Source> {
        use crate::merge::{Source, Trust, Weight};
        let source = |publisher: &str, set: &str, trust| Source {
            publisher: publisher.into(),
            set: set.into(),
            generation: 1,
            trust,
            same_as: BTreeMap::new(),
            labels: Vec::new(),
        };
        vec![
            source("ed25519:me", "local/hn-stories", Trust::Owner),
            source(
                "ed25519:other",
                "other/nicknames",
                Trust::Outside {
                    weight: Weight::Low,
                    added: 0,
                },
            ),
        ]
    }

    #[test]
    fn a_layer_changes_only_what_it_gives_of_a_page_set_page() {
        use crate::merge::{merge, Source, Sourced, Trust};
        let mut sources = sources();
        sources.push(Source {
            publisher: "ed25519:project".into(),
            set: "plumb/wikipedia-en".into(),
            generation: 1,
            trust: Trust::Project,
            same_as: BTreeMap::new(),
            labels: Vec::new(),
        });
        let adams = page(json!({
            "set": "wikipedia-en", "url": "https://en.wikipedia.org/wiki/Douglas_Adams",
            "title": "Douglas Adams", "description": "English author", "views": 5000,
            "aliases": ["DNA"], "item": "Q42", "lead": "Douglas Noël Adams was an English author.",
        }));
        let thing = page_item(&adams).unwrap();
        let merged_with = |layers: &[(Item, usize)]| {
            let on: Vec<Sourced<'_>> = layers
                .iter()
                .map(|(layer, source)| Sourced {
                    item: layer,
                    source: *source,
                })
                .collect();
            let thing = Sourced {
                item: &thing,
                source: 2,
            };
            layered_page(adams.clone(), &merge(&sources, thing, &on).unwrap())
        };
        assert_eq!(merged_with(&[]), adams, "no layer, no change");

        // An outside publisher adds a name, which counts for less, and
        // can't change the project's description.
        let outside = item(json!({"id": "x", "extends": "wd:Q42",
                                  "names": ["Hitchhiker author", "dna"], "about": "Not this"}));
        let page = merged_with(&[(outside.clone(), 1)]);
        assert_eq!(page.names, ["Hitchhiker author"]);
        assert_eq!(page.description, adams.description);
        assert_eq!(
            Page {
                names: Vec::new(),
                ..page
            },
            adams
        );

        // The owner ranks above the project.
        let mine = item(
            json!({"id": "o", "extends": "url:https://en.wikipedia.org/wiki/Douglas_Adams",
                               "names": ["Doug"], "about": "My favourite author", "weight": 9000}),
        );
        let page = merged_with(&[(outside, 1), (mine, 0)]);
        assert_eq!(page.description.as_deref(), Some("My favourite author"));
        assert_eq!(page.views, 9000);
        assert_eq!(page.aliases, ["DNA", "Doug"]);
        assert_eq!(page.names, ["Hitchhiker author"]);
        assert_eq!(
            (page.lead, page.item, page.title),
            (adams.lead, adams.item, adams.title)
        );
    }

    #[test]
    fn an_items_page_takes_its_sets_title_line_and_place() {
        use crate::merge::{merge, Sourced};
        let story = item(json!({
            "id": "hn:41", "kind": "story", "title": "Plumb, a search engine you run",
            "url": "https://plumbsearch.org/", "names": ["Plumb"], "weight": 272.4,
            "lang": "en", "published": "2026-10-11T08:00:00Z", "ids": {"wd": "Q1"},
            "edges": [{"rel": "about", "to": "site:plumbsearch.org"}],
            "fields": {"points": 272, "comments": 31, "tags": ["show", "rust"]},
        }));
        let nickname = item(json!({"id": "n", "extends": "wd:Q1", "names": ["Plumbing"]}));
        let sources = sources();
        let merged = merge(
            &sources,
            Sourced {
                item: &story,
                source: 0,
            },
            &[Sourced {
                item: &nickname,
                source: 1,
            }],
        )
        .unwrap();
        let view = SetView {
            set: "local/hn-stories".into(),
            title: Some("Hacker News stories".into()),
            about: Some("{points} points · {comments} comments, {published}".into()),
            place: Place::read(Some(
                &json!({"how": "asked", "ask": ["HN", " Hacker  News"]}),
            ))
            .unwrap(),
        };
        let page = item_page(&merged, &view).unwrap();
        assert_eq!(page.set, "local/hn-stories");
        assert_eq!(page.title, "Plumb, a search engine you run");
        assert_eq!(page.url, "https://plumbsearch.org/");
        assert_eq!(
            page.description.as_deref(),
            Some("272 points · 31 comments, 2026-10-11")
        );
        assert_eq!(page.site.as_deref(), Some("plumbsearch.org"));
        assert_eq!(page.views, 272);
        assert_eq!(page.aliases, ["Plumb"], "the owner's name counts in full");
        assert_eq!(page.names, ["Plumbing"], "an outside publisher's for less");
        assert_eq!(page.item.as_deref(), Some("Q1"));
        assert_eq!(page.content_language.as_deref(), Some("en"));
        let set = page.item_set.as_ref().unwrap();
        assert_eq!(set.title.as_deref(), Some("Hacker News stories"));
        assert_eq!(set.kind, "story");
        assert_eq!(set.ask, ["hn", "hacker news"]);
        assert!(page.is_asked());
        assert_eq!(page.set_name(), "Hacker News stories");
        assert_eq!(page.set_domain(), "plumbsearch.org");

        // A slot with no value leaves the item's own line; lists show
        // their first values.
        let view = SetView {
            about: Some("{votes} votes".into()),
            title: None,
            ..view
        };
        let mut about = merged.clone();
        about.item.about = Some("A search engine".into());
        let page = item_page(&about, &view).unwrap();
        assert_eq!(page.description.as_deref(), Some("A search engine"));
        assert_eq!(page.set_name(), "plumbsearch.org");
        assert_eq!(show("{tags}", &merged).as_deref(), Some("show, rust"));
        assert_eq!(show("{points", &merged), None);

        // No title or no URL, no page.
        let mut untitled = merged.clone();
        untitled.item.title = None;
        assert!(item_page(&untitled, &view).is_none());
        let mut nowhere = merged;
        nowhere.item.url = None;
        assert!(item_page(&nowhere, &view).is_none());
    }

    #[test]
    fn a_sets_place_reads_as_a_node_lists_its_pages() {
        let read = |place: Value| Place::read(Some(&place));
        assert_eq!(Place::read(None), Ok(Place::Own));
        assert_eq!(read(json!({"how": "own", "cap": 1.0})), Ok(Place::Own));
        assert_eq!(read(json!({"how": "under-site"})), Ok(Place::Own));
        assert_eq!(read(json!({"how": "answer-only"})), Ok(Place::AnswerOnly));
        assert_eq!(
            read(json!({"how": "asked", "ask": ["hn", "HN"]})),
            Ok(Place::Asked(vec!["hn".into()]))
        );
        for wrong in [
            json!({"how": "asked"}),
            json!({"how": "asked", "ask": []}),
            json!({"how": "asked", "ask": [" "]}),
            json!({"how": "asked", "ask": [1]}),
            json!({"how": "asked", "ask": ["x".repeat(41)]}),
            json!({"how": "first"}),
            json!({"ask": ["hn"]}),
        ] {
            assert!(read(wrong.clone()).is_err(), "{wrong}");
        }
    }

    /// Every row of a real articles file reads as an item that passes the
    /// format's checks and gives the same page back. Set `PLUMB_SET_FILE`
    /// to an uncompressed articles file and `PLUMB_SET_NAME` to its set.
    #[test]
    #[ignore]
    fn a_real_set_file() {
        let path = std::env::var("PLUMB_SET_FILE").unwrap();
        let set = std::env::var("PLUMB_SET_NAME").unwrap();
        let reader = std::io::BufReader::new(std::fs::File::open(path).unwrap());
        let manifest = manifest(&set);
        let (mut pages, mut items, mut ids) = (0, 0, std::collections::HashSet::new());
        let (mut skipped, mut given) = (Vec::new(), 0);
        let mut differ: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for article in plumb_core::article::read_articles(reader, usize::MAX).unwrap() {
            let Some(page) = Page::from_set(&set, article) else {
                continue;
            };
            pages += 1;
            let Some(item) = page_item(&page) else {
                skipped.push(page.url);
                continue;
            };
            manifest.check_item(&item).unwrap();
            assert!(ids.insert(item.id.clone()), "{} twice", item.id);
            if items == 0 {
                println!("{}", serde_json::to_string(&item).unwrap());
            }
            items += 1;
            if let Some(Value::Object(fields)) = item.fields.get("page") {
                given += 1;
                if given <= 5 {
                    println!("given: {}", Value::Object(fields.clone()));
                }
            }
            let back = page_of_item(&item, &set).unwrap();
            for name in differing_members(&page, &back) {
                differ.entry(name).or_default().push(page.url.clone());
            }
        }
        for (name, urls) in &differ {
            println!(
                "{name} differs on {} pages: {:?}",
                urls.len(),
                &urls[..urls.len().min(5)]
            );
        }
        println!("{given} items say part of their page as given");
        println!("{pages} pages, {items} items, skipped {skipped:?}");
        assert!(items > 0);
    }
}
