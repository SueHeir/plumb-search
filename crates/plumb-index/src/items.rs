//! Today's page sets read as items of the data format ([`plumb_format`]),
//! so a set can be published in it without being rebuilt. Nothing in
//! search reads these items yet.
//!
//! Each page becomes one item ([`page_item`]) whose extra fields a page
//! set's manifest declares with [`page_fields`]. What a page is about
//! ([`Page::site`]) becomes an `about` edge to that site, and the item's
//! own `site` is the site its URL is on, as a node works it out.
//!
//! Ids follow the format's rule of the source's own stable id: the
//! Wikidata item of a Wikidata or films page, `<registry>:<name>` of a
//! package, a paper's DOI. Every other page is named by its canonical URL,
//! Wikipedia articles included: the articles files do not carry the page
//! id the format names an article by, so until they do a renamed article
//! gets a new id.

use std::collections::BTreeMap;

use plumb_format::item::{MAX_ITEM_BYTES, MAX_TEXT_CHARS};
use plumb_format::refs::is_namespace;
use plumb_format::urls::canonical_url;
use plumb_format::value::{is_date, is_language, normalize_text};
use plumb_format::{Edge, FieldSpec, Item, Ref};
use serde_json::{json, Map, Value};

use crate::pages::{
    Page, BOOKS_SET, DOCS_SET, FILMS_SET, GITHUB_SET, MUSIC_SET, PACKAGES_SET, PAPERS_SET,
    PODCASTS_SET, STACKEXCHANGE_SET, STACKOVERFLOW_SET, WIKIDATA_SET, WIKTIONARY_SET,
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

/// `page` as an item, or `None` when its URL has no canonical form or the
/// item fails the format's checks even without its optional fields (an id
/// longer than the format allows).
pub fn page_item(page: &Page) -> Option<Item> {
    let url = canonical_url(&page.url)?;
    let wd = page
        .item
        .as_deref()
        .filter(|item| format!("wd:{item}").parse::<Ref>().is_ok());
    let mut item = Item {
        id: id_of(page, wd, &url),
        kind: kind_of(page).to_string(),
        title: clean(&page.title),
        site: plumb_core::registrable_domain(&url)
            .filter(|site| plumb_format::refs::is_domain(site)),
        about: page.description.as_deref().and_then(clean),
        text: page
            .lead
            .as_deref()
            .and_then(clean)
            .map(|lead| lead.chars().take(MAX_TEXT_CHARS).collect()),
        lang: page
            .language()
            .filter(|lang| is_language(lang))
            .map(str::to_string),
        weight: Some(page.views as f64),
        url: Some(url),
        ..Item::default()
    };
    for name in page.aliases.iter().chain(&page.names) {
        if let Some(name) = clean(name) {
            if item.title.as_ref() != Some(&name) && !item.names.contains(&name) {
                item.names.push(name);
            }
        }
    }
    if let Some(wd) = wd {
        item.ids.insert("wd".into(), wd.to_string());
    }
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
    while too_big(&item) {
        let Some(field) = SHED.iter().find(|f| item.fields.contains_key(**f)) else {
            break;
        };
        item.fields.remove(*field);
    }
    item.check().is_ok().then_some(item)
}

/// What the `fields` of a page set's items are, for its manifest.
pub fn page_fields() -> BTreeMap<String, FieldSpec> {
    let object_list = json!({"type": "object", "list": true, "roles": ["show"]});
    serde_json::from_value(json!({
        "website": {"type": "url", "roles": ["show"]},
        "profiles": object_list,
        "facts": object_list,
        "package": {"type": "object", "roles": ["show"]},
        "sections": {"type": "text", "list": true, "roles": ["search:about"]},
        "symbols": {"type": "object", "list": true, "roles": ["search:body"]},
        "passages": {"type": "object", "list": true, "roles": ["search:body"]},
        "authors": {"type": "text", "list": true, "same_as": "wd:P50", "roles": ["search:about", "show"]},
        "venue": {"type": "text", "same_as": "wd:P1433", "roles": ["show"]},
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
        if is_namespace(&package.registry) {
            item.ids
                .insert(package.registry.clone(), package.name.clone());
        }
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
                    ("heading", Some(&p.heading)),
                    ("text", Some(&p.text)),
                    ("anchor", p.anchor.as_ref()),
                ])
            })
            .collect();
        put_list(fields, "passages", passages);
    }
    if let Some(paper) = &page.paper {
        let authors: Vec<Value> = paper
            .authors
            .iter()
            .filter_map(|a| clean(a))
            .map(Value::from)
            .collect();
        put_list(fields, "authors", authors);
        if let Some(venue) = paper.venue.as_deref().and_then(clean) {
            fields.insert("venue".into(), json!(venue));
        }
        for (namespace, id) in [
            ("doi", paper.doi.as_deref().map(str::to_lowercase)),
            ("arxiv", paper.arxiv_id.clone()),
            ("openalex", paper.openalex_id.clone()),
        ] {
            if let Some(id) = id.filter(|id| !id.is_empty()) {
                item.ids.insert(namespace.into(), id);
            }
        }
        item.published = paper.publication_date.clone().filter(|date| is_date(date));
    }
}

/// An object of the members that have a value, each as text.
fn object(members: &[(&str, Option<&String>)]) -> Value {
    let mut map = Map::new();
    for (name, value) in members {
        if let Some(value) = value.and_then(|v| clean(v)) {
            map.insert(name.to_string(), Value::from(value));
        }
    }
    Value::Object(map)
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
        assert_eq!(item.names, ["US Bank", "U.S. Bank"]);
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

    /// Every row of a real articles file reads as an item that passes the
    /// format's checks. Set `PLUMB_SET_FILE` to an uncompressed articles
    /// file and `PLUMB_SET_NAME` to its set.
    #[test]
    #[ignore]
    fn a_real_set_file() {
        let path = std::env::var("PLUMB_SET_FILE").unwrap();
        let set = std::env::var("PLUMB_SET_NAME").unwrap();
        let reader = std::io::BufReader::new(std::fs::File::open(path).unwrap());
        let manifest = manifest(&set);
        let (mut pages, mut items, mut ids) = (0, 0, std::collections::HashSet::new());
        let mut skipped = Vec::new();
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
        }
        println!("{pages} pages, {items} items, skipped {skipped:?}");
        assert!(items > 0);
    }
}
