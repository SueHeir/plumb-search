//! Today's page sets written as items of the data format
//! ([`plumb_format`]), so a set can be published in it without being
//! rebuilt (`plumb-tools set from-pages`).
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

use plumb_core::papers::{PaperCountKind, PaperMetadata};
use plumb_format::item::{MAX_ID_BYTES, MAX_ITEM_BYTES, MAX_TEXT_CHARS};
use plumb_format::refs::{is_domain, is_namespace};
use plumb_format::urls::{canonical_url, url_sha256};
use plumb_format::value::{check_text, is_date, is_language, normalize_text, stays_in_nfc};
use plumb_format::{Edge, FieldSpec, Item, Ref};
use serde_json::{json, Map, Value};

use crate::items::{page_of_item, ESCAPED};
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

/// The site of the host `url` is on, by the site rules this node uses;
/// `None` when that is not a domain, as for an IP address.
fn site_of_url(url: &str) -> Option<String> {
    plumb_core::registrable_domain(url).filter(|site| is_domain(site))
}

/// `page` as an item, or `None` when its URL has no canonical form or the
/// item fails the format's checks even without its optional fields (over
/// the size cap); [`page_item_or_why`] says which.
///
/// [`page_of_item`] gives the page back from it. Where the item's own
/// members say something differently from the page (a URL not in its
/// canonical form, a description with a space at its end, text not in
/// NFC), the `page` field says it as the page has it, so the two are the
/// same page member for member unless the item is over the size cap.
pub fn page_item(page: &Page) -> Option<Item> {
    page_item_or_why(page).ok()
}

/// [`page_item`], or why `page` can't be an item.
pub fn page_item_or_why(page: &Page) -> Result<Item, String> {
    let mut item = page_stub(page).ok_or("its URL has no canonical form")?;
    fill(page, &mut item);
    while too_big(&item) {
        let Some(field) = SHED.iter().find(|f| item.fields.contains_key(**f)) else {
            return Err(format!("it is over {MAX_ITEM_BYTES} bytes as an item"));
        };
        item.fields.remove(*field);
    }
    item.check().map_err(|err| format!("{err:#}"))?;
    Ok(item)
}

/// [`page_item`] as the page index keeps it ([`crate::pages::SetItem`]):
/// with every field, however big, a URL that has no canonical form as it
/// is, and not checked against the format, so that it gives back as much
/// of the page as an item can.
pub(crate) fn index_item(page: &Page) -> Item {
    let url = canonical_url(&page.url).unwrap_or_else(|| page.url.clone());
    let mut item = stub(page, url);
    fill(page, &mut item);
    item
}

/// Fills in `item`, the stub of `page`, with all [`page_item`] says of it.
fn fill(page: &Page, item: &mut Item) {
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
    add_fields(page, item);
    // Taking the URL as given can change the language a set implies, so
    // a second look can find more to keep.
    for _ in 0..3 {
        let Some(back) = page_of_item(item, &page.set) else {
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
    let mut escaped = texts(given.get(ESCAPED));
    // Text the format does not allow is written escaped, and its member
    // listed in `escaped`.
    let mut put = |name: &str, values: &[&String], list: bool| {
        let plain = values.iter().all(|text| check_text(text).is_ok());
        let mut values = values.iter().map(|text| {
            Value::from(if plain {
                text.to_string()
            } else {
                escape(text)
            })
        });
        let value = if list {
            Value::Array(values.collect())
        } else {
            values.next().unwrap_or_default()
        };
        given.insert(name.into(), value);
        escaped.retain(|member| member != name);
        if !plain {
            escaped.push(name.to_string());
        }
    };
    let mut text = |name: &str, page: Option<&String>, back: Option<&String>| {
        if let Some(text) = page {
            if back != Some(text) {
                put(name, &[text], false);
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
        if page != back {
            put(name, &page.iter().collect::<Vec<_>>(), true);
        }
    }
    escaped.sort();
    if escaped.is_empty() {
        given.remove(ESCAPED);
    } else {
        given.insert(ESCAPED.into(), json!(escaped));
    }
    *given != before
}

/// Text the format does not allow (not in NFC, or with control characters
/// but newlines) written so that it does: `\\` for `\`, and `\u{hex}` for
/// a control character or one NFC could change or join to the character
/// before it. [`crate::items::unescape`] gives the text back.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\\' {
            out.push_str("\\\\");
        } else if (c.is_control() && c != '\n') || !stays_in_nfc(c) {
            out.push_str(&format!("\\u{{{:x}}}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

/// `text` with each control character, and each character NFC could
/// change or join to the one before it, written as `\u{hex}`, to show
/// how two texts that look alike differ.
pub fn visible(text: &str) -> String {
    text.chars()
        .map(|c| {
            if (c.is_control() && c != '\n') || !stays_in_nfc(c) {
                format!("\\u{{{:x}}}", u32::from(c))
            } else {
                c.to_string()
            }
        })
        .collect()
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

/// The id, kind, URL and outside ids of [`page_item`].
fn page_stub(page: &Page) -> Option<Item> {
    Some(stub(page, canonical_url(&page.url)?))
}

/// [`page_stub`] with the URL `url`.
fn stub(page: &Page, url: String) -> Item {
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
    item
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
    .filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
    .unwrap_or_else(|| match url_sha256(url) {
        // An id may be no longer than the format allows.
        Some(hashed) if url.len() > MAX_ID_BYTES => hashed,
        _ => url.to_string(),
    })
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
    use crate::items::unescape;
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

        // Text the format does not allow comes back too, escaped in `page`.
        let odd = Page {
            title: "Cafe\u{301} \\ Bar".into(),
            description: Some("Rings\u{7}".into()),
            aliases: vec!["\u{18}".into(), "Cafe\u{301}".into()],
            ..plain.clone()
        };
        let item = page_item(&odd).unwrap();
        manifest(&odd.set).check_item(&item).unwrap();
        assert_eq!(item.title.as_deref(), Some("Caf\u{e9} \\ Bar"));
        assert_eq!(item.about.as_deref(), Some("Rings"));
        assert_eq!(item.names, ["Caf\u{e9}"]);
        assert_eq!(
            item.fields["page"]["escaped"],
            json!(["aliases", "description", "title"])
        );
        assert_eq!(item.fields["page"]["title"], json!("Cafe\\u{301} \\\\ Bar"));
        assert_eq!(page_of_item(&item, &odd.set), Some(odd));
    }

    #[test]
    fn escaped_text_is_text_the_format_allows_and_comes_back() {
        for text in [
            "plain",
            "Cafe\u{301}",
            "e\u{323}\u{301}",
            "\u{1100}\u{1161}",
            "\u{212b}ngstr\u{f6}m",
            "a\\u{41} \\",
            "\u{7}\tline\n",
        ] {
            let escaped = escape(text);
            plumb_format::value::check_text(&escaped).unwrap();
            assert_eq!(unescape(&escaped).as_deref(), Some(text), "{escaped}");
        }
        assert_eq!(escape("Caf\u{e9}"), "Caf\u{e9}", "NFC text stays as it is");
        assert_eq!(visible("Cafe\u{301}"), "Cafe\\u{301}");
        for bad in ["\\x", "\\u{}", "\\u{110000}", "\\"] {
            assert_eq!(unescape(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_page_with_a_url_too_long_for_an_id_is_named_by_its_hash() {
        let long = page(json!({
            "set": "docs", "url": format!("https://docs.example.org/{}", "a".repeat(300)),
            "title": "A long page", "views": 1,
        }));
        let item = page_item(&long).unwrap();
        assert!(item.id.starts_with("url-sha256:"), "{}", item.id);
        assert_eq!(page_stub(&long).unwrap().id, item.id);
        assert_eq!(page_of_item(&item, "docs"), Some(long));
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
    fn pages_without_a_web_address_are_skipped() {
        let page = page(
            json!({"set": "docs", "url": "ftp://example.com/x", "title": "x",
                               "views": 0}),
        );
        assert_eq!(page_item(&page), None);
        assert_eq!(
            page_item_or_why(&page),
            Err("its URL has no canonical form".to_string())
        );
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
