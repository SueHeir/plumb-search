//! Today's page sets read as items of the data format ([`plumb_format`]),
//! so a set can be published in it without being rebuilt, and items of
//! other sets made into pages, so the page index can search them
//! ([`item_page`]).
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
    for name in page.aliases.iter().chain(&page.names) {
        if let Some(name) = clean(name) {
            if item.title.as_ref() != Some(&name) && !item.names.contains(&name) {
                item.names.push(name);
            }
        }
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
    let site = item.edges.iter().find_map(|edge| match &edge.to {
        Ref::Site(domain) if edge.rel == "about" => Some(domain.clone()),
        _ => None,
    });
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
