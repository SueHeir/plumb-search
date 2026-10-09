//! The `films` page set: films and TV shows from Wikidata (CC0), the most
//! linked first.
//!
//! Wikipedia's articles already list the best-known films and shows by
//! their titles. What this set adds is what tells them apart and what
//! leads to them: each keeps its year (or the years a show ran), its
//! director or creator and its best-known cast, so "dune 2021", "dune
//! david lynch" and "breaking bad tv show" ask for one of them; where it is
//! listed and can be watched (IMDb, Letterboxd, Rotten Tomatoes, Netflix);
//! and the films and shows English Wikipedia has no article on, found by
//! their English or original titles. Nothing is copied from IMDb or TMDB:
//! only Wikidata's own identifiers for them, which make links.
//!
//! It is fetched in two steps from Wikidata's query service:
//!
//! 1. Each class of [`CLASSES`] (film, TV series, anime series...) is
//!    asked for whole, [`ITEMS_PAGE`] items at a time with their
//!    sitelinks, the number of Wikipedias and other wikis with a page on
//!    them, which stands for how well known they are. A query for one
//!    class and nothing else is a scan of one index. Each class's label is
//!    checked first, so a mistaken item number leaves that class out
//!    rather than filling the set with something else.
//! 2. The items kept (the most linked, at least `min_sitelinks`) are asked
//!    about [`DETAILS_BATCH`] at a time: their labels, English article,
//!    dates, director, creator, cast and listings.
//!
//! A batch the query service keeps failing to answer is left out with a
//! warning; more than [`MOST_FAILED`] of them fail the fetch.

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};
use plumb_core::article::{Article, MAX_ALIASES, MAX_ARTICLE_DESCRIPTION_CHARS};
use plumb_core::films::CAST_SEPARATOR;
use plumb_core::profiles::{Profile, Service, SERVICES};
use serde::Deserialize;
use tracing::{info, warn};

use crate::download::WikidataPacing;
use crate::facts::sparql_json;

/// Items of one class asked for in one query.
pub const ITEMS_PAGE: usize = 100_000;
/// Items asked about in one query for their details.
pub const DETAILS_BATCH: usize = 200;
/// Most films and shows kept by default, the most linked.
pub const DEFAULT_MAX_FILMS: usize = 150_000;
/// Fewest sitelinks of a film or show kept by default.
pub const DEFAULT_MIN_SITELINKS: u64 = 3;
/// Share of detail batches that may fail before the fetch does.
pub const MOST_FAILED: f64 = 0.05;
/// Most cast members named in a description, the best known.
pub const CAST_NAMED: usize = 3;
/// Most directors or creators named.
const MAKERS_NAMED: usize = 2;

/// The listings a film or show keeps, as [`SERVICES`] keys.
pub const FILM_SERVICES: &[&str] = &[
    "netflix",
    "imdb",
    "rotten-tomatoes",
    "metacritic",
    "letterboxd",
    "tmdb-movie",
    "tmdb-tv",
    "myanimelist",
];

/// A Wikidata class whose items are films or shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilmClass {
    /// Its item: `Q11424`.
    pub item: &'static str,
    /// What its items are called, as their descriptions start
    /// ([`plumb_core::films::FILM_KINDS`], [`plumb_core::films::SHOW_KINDS`]).
    pub kind: &'static str,
    /// A word its English label must have, checked before it is asked for.
    pub label_word: &'static str,
}

/// The classes asked for. An item of more than one is called after the
/// first it is of: an animated film is an "Animated film", not a "Film".
pub const CLASSES: &[FilmClass] = &[
    FilmClass {
        item: "Q63952888",
        kind: "Anime series",
        label_word: "anime",
    },
    FilmClass {
        item: "Q117467246",
        kind: "Animated series",
        label_word: "animated",
    },
    FilmClass {
        item: "Q581714",
        kind: "Animated series",
        label_word: "animated",
    },
    FilmClass {
        item: "Q1259759",
        kind: "Miniseries",
        label_word: "miniseries",
    },
    FilmClass {
        item: "Q526877",
        kind: "Web series",
        label_word: "web",
    },
    FilmClass {
        item: "Q5398426",
        kind: "TV series",
        label_word: "series",
    },
    FilmClass {
        item: "Q202866",
        kind: "Animated film",
        label_word: "animated",
    },
    FilmClass {
        item: "Q29168811",
        kind: "Animated film",
        label_word: "animated",
    },
    FilmClass {
        item: "Q93204",
        kind: "Documentary film",
        label_word: "documentary",
    },
    FilmClass {
        item: "Q506240",
        kind: "TV film",
        label_word: "television",
    },
    FilmClass {
        item: "Q24869",
        kind: "Film",
        label_word: "film",
    },
    FilmClass {
        item: "Q11424",
        kind: "Film",
        label_word: "film",
    },
];

/// What [`fetch_films`] keeps and reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilmOptions {
    pub max_films: usize,
    pub min_sitelinks: u64,
}

impl Default for FilmOptions {
    fn default() -> Self {
        FilmOptions {
            max_films: DEFAULT_MAX_FILMS,
            min_sitelinks: DEFAULT_MIN_SITELINKS,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Response {
    results: Results,
}

#[derive(Debug, Deserialize)]
struct Results {
    bindings: Vec<HashMap<String, Term>>,
}

#[derive(Debug, Deserialize)]
struct Term {
    value: String,
}

fn bindings(json: &[u8]) -> Result<Vec<HashMap<String, Term>>> {
    let response: Response = serde_json::from_slice(json).context("reading the Wikidata answer")?;
    Ok(response.results.bindings)
}

/// `Q42` of `http://www.wikidata.org/entity/Q42`.
fn entity_id(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or(uri)
}

fn is_item_id(id: &str) -> bool {
    id.len() > 1 && id.starts_with('Q') && id[1..].bytes().all(|b| b.is_ascii_digit())
}

/// Where English Wikipedia's articles are, as Wikidata's sitelinks say.
const ENGLISH_WIKIPEDIA: &str = "https://en.wikipedia.org/wiki/";

fn class_labels_query() -> String {
    let values: Vec<String> = CLASSES.iter().map(|c| format!("wd:{}", c.item)).collect();
    format!(
        "SELECT ?class ?label WHERE {{ VALUES ?class {{ {} }} ?class rdfs:label ?label . \
         FILTER(LANG(?label) = \"en\") }}",
        values.join(" ")
    )
}

/// The classes whose English label, in the answer to
/// [`class_labels_query`], has the word it should.
fn checked_classes(json: &[u8]) -> Result<Vec<&'static FilmClass>> {
    let mut labels: HashMap<String, String> = HashMap::new();
    for row in bindings(json)? {
        if let (Some(class), Some(label)) = (row.get("class"), row.get("label")) {
            labels.insert(
                entity_id(&class.value).to_string(),
                label.value.to_lowercase(),
            );
        }
    }
    let mut checked = Vec::new();
    for class in CLASSES {
        match labels.get(class.item) {
            Some(label) if label.contains(class.label_word) => {
                info!("{} ({}): {label}", class.kind, class.item);
                checked.push(class);
            }
            label => warn!(
                "left out {}: Wikidata calls {} {label:?}, without {:?}",
                class.kind, class.item, class.label_word
            ),
        }
    }
    Ok(checked)
}

fn items_page_query(class: &FilmClass, offset: usize) -> String {
    format!(
        "SELECT ?item ?links WHERE {{ ?item wdt:P31 wd:{} ; wikibase:sitelinks ?links . }} \
         LIMIT {ITEMS_PAGE} OFFSET {offset}",
        class.item
    )
}

/// The items found so far: their sitelinks and the first of [`CLASSES`]
/// (by its place there) each is of.
type Found = HashMap<String, (u64, usize)>;

/// Adds the rows of an answer to [`items_page_query`] for the class at
/// `class` in [`CLASSES`] to `found`; returns how many rows there were.
fn add_items_page(found: &mut Found, class: usize, json: &[u8]) -> Result<usize> {
    let rows = bindings(json)?;
    for row in &rows {
        let (Some(item), Some(links)) = (row.get("item"), row.get("links")) else {
            continue;
        };
        let item = entity_id(&item.value);
        let Ok(links) = links.value.parse::<u64>() else {
            continue;
        };
        if !is_item_id(item) {
            continue;
        }
        let kept = found.entry(item.to_string()).or_insert((links, class));
        kept.1 = kept.1.min(class);
    }
    Ok(rows.len())
}

/// The items of `found` to keep under `options`, the most linked first,
/// with their sitelinks and kind.
fn chosen(found: Found, options: &FilmOptions) -> Vec<(String, u64, &'static str)> {
    let mut items: Vec<(String, u64, &'static str)> = found
        .into_iter()
        .filter(|(_, (links, _))| *links >= options.min_sitelinks)
        .map(|(item, (links, class))| (item, links, CLASSES[class].kind))
        .collect();
    items.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| item_number(&a.0).cmp(&item_number(&b.0)))
    });
    items.truncate(options.max_films);
    items
}

fn item_number(item: &str) -> u64 {
    item[1..].parse().unwrap_or(u64::MAX)
}

fn details_query(items: &[&str], services: &[&Service]) -> String {
    let values: Vec<String> = items.iter().map(|item| format!("wd:{item}")).collect();
    let mut parts = vec![
        "{ ?item rdfs:label ?v . FILTER(LANG(?v) = \"en\") BIND(\"label\" AS ?k) }".to_string(),
        "{ ?item rdfs:label ?v . FILTER(LANG(?v) = \"mul\") BIND(\"mul\" AS ?k) }".to_string(),
        "{ ?item skos:altLabel ?v . FILTER(LANG(?v) = \"en\") BIND(\"alias\" AS ?k) }".to_string(),
        "{ ?item wdt:P1476 ?v . BIND(\"title\" AS ?k) }".to_string(),
        "{ ?v schema:about ?item ; schema:isPartOf <https://en.wikipedia.org/> . \
         BIND(\"article\" AS ?k) }"
            .to_string(),
        "{ ?item wdt:P577 ?v . BIND(\"date\" AS ?k) }".to_string(),
        "{ ?item wdt:P580 ?v . BIND(\"start\" AS ?k) }".to_string(),
        "{ ?item wdt:P582 ?v . BIND(\"end\" AS ?k) }".to_string(),
    ];
    for (property, key) in [("P57", "director"), ("P170", "creator"), ("P161", "cast")] {
        parts.push(format!(
            "{{ ?item wdt:{property} ?p . ?p rdfs:label ?v ; wikibase:sitelinks ?n . \
             FILTER(LANG(?v) = \"en\") BIND(\"{key}\" AS ?k) }}"
        ));
    }
    for service in services {
        parts.push(format!(
            "{{ ?item wdt:{} ?v . BIND(\"{}\" AS ?k) }}",
            service.property, service.key
        ));
    }
    format!(
        "SELECT ?item ?k ?v ?n WHERE {{ VALUES ?item {{ {} }} {} }}",
        values.join(" "),
        parts.join(" UNION ")
    )
}

/// What Wikidata says of one film or show, from answers to
/// [`details_query`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Details {
    label: Option<String>,
    /// Its label in no language in particular (`mul`).
    any_label: Option<String>,
    aliases: Vec<String>,
    /// Its titles in its own language (P1476).
    titles: Vec<String>,
    /// Its English article's address path: `Dune_(2021_film)`.
    article: Option<String>,
    /// The year it first came out (P577), or a show began (P580).
    released: Option<i32>,
    started: Option<i32>,
    ended: Option<i32>,
    /// People and their sitelinks.
    directors: Vec<(String, u64)>,
    creators: Vec<(String, u64)>,
    cast: Vec<(String, u64)>,
    profiles: Vec<Profile>,
}

/// The year of a Wikidata date: `2010` of `2010-07-08T00:00:00Z`. Only
/// years of film are taken.
fn year(date: &str) -> Option<i32> {
    let year: i32 = date.split('-').next()?.parse().ok()?;
    (1870..=2100).contains(&year).then_some(year)
}

fn earliest(kept: &mut Option<i32>, year: Option<i32>) {
    if let Some(year) = year {
        *kept = Some(kept.map_or(year, |kept| kept.min(year)));
    }
}

fn add_person(people: &mut Vec<(String, u64)>, name: String, links: u64) {
    match people.iter_mut().find(|(kept, _)| *kept == name) {
        Some(person) => person.1 = person.1.max(links),
        None => people.push((name, links)),
    }
}

fn add_details(details: &mut HashMap<String, Details>, json: &[u8]) -> Result<()> {
    for row in bindings(json)? {
        let (Some(item), Some(key), Some(value)) = (row.get("item"), row.get("k"), row.get("v"))
        else {
            continue;
        };
        let kept = details
            .entry(entity_id(&item.value).to_string())
            .or_default();
        let text = value.value.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            continue;
        }
        let links = row.get("n").and_then(|n| n.value.parse().ok()).unwrap_or(0);
        match key.value.as_str() {
            "label" => kept.label = Some(text),
            "mul" => kept.any_label = Some(text),
            "alias" if !kept.aliases.contains(&text) => kept.aliases.push(text),
            "title" if !kept.titles.contains(&text) => kept.titles.push(text),
            "article" => {
                // Written as Plumb writes articles' addresses, which
                // Wikidata's differ from ("%27" for "'").
                if let Some(path) = value.value.strip_prefix(ENGLISH_WIKIPEDIA) {
                    let url = plumb_core::article::article_url("en", &article_title(path));
                    if let Some(path) = url.strip_prefix(ENGLISH_WIKIPEDIA) {
                        kept.article = Some(path.to_string());
                    }
                }
            }
            "date" => earliest(&mut kept.released, year(&text)),
            "start" => earliest(&mut kept.started, year(&text)),
            "end" => {
                if let Some(end) = year(&text) {
                    kept.ended = Some(kept.ended.map_or(end, |kept| kept.max(end)));
                }
            }
            "director" => add_person(&mut kept.directors, text, links),
            "creator" => add_person(&mut kept.creators, text, links),
            "cast" => add_person(&mut kept.cast, text, links),
            service => {
                let Some(service) = SERVICES.iter().find(|s| s.key == service) else {
                    continue;
                };
                let id = text.trim_start_matches('@');
                if service.accepts(id) && !kept.profiles.iter().any(|p| p.service == service.key) {
                    kept.profiles.push(Profile {
                        service: service.key.to_string(),
                        id: id.to_string(),
                    });
                }
            }
        }
    }
    Ok(())
}

/// The best known of `people`, at most `most`: "Leonardo DiCaprio, Elliot
/// Page".
fn best_known(people: &[(String, u64)], most: usize) -> Vec<&str> {
    let mut people: Vec<&(String, u64)> = people.iter().collect();
    people.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    people
        .into_iter()
        .take(most)
        .map(|(name, _)| name.as_str())
        .collect()
}

/// "Film by Christopher Nolan, 2010 · with Leonardo DiCaprio, Elliot
/// Page", "TV series by Vince Gilligan, 2008–2013", "Miniseries, 2019–":
/// what it is, who made it, when, and its best-known cast, as many as fit
/// in [`MAX_ARTICLE_DESCRIPTION_CHARS`].
fn describe(kind: &str, details: &Details) -> String {
    let show = plumb_core::films::describes_a_show(kind);
    let makers = if show && !details.creators.is_empty() {
        &details.creators
    } else {
        &details.directors
    };
    let mut text = kind.to_string();
    let makers = best_known(makers, MAKERS_NAMED);
    if !makers.is_empty() {
        text.push_str(" by ");
        text.push_str(&makers.join(" and "));
    }
    let when = if show {
        match (details.started.or(details.released), details.ended) {
            (Some(start), Some(end)) if end > start => Some(format!("{start}–{end}")),
            (Some(start), _) => Some(format!("{start}–")),
            (None, _) => None,
        }
    } else {
        details.released.map(|year| year.to_string())
    };
    if let Some(when) = when {
        text.push_str(", ");
        text.push_str(&when);
    }
    let byline = plumb_core::truncate_chars(&text, MAX_ARTICLE_DESCRIPTION_CHARS);
    let mut cast = best_known(&details.cast, CAST_NAMED);
    while !cast.is_empty() {
        let with = format!("{byline}{CAST_SEPARATOR}{}", cast.join(", "));
        if with.chars().count() <= MAX_ARTICLE_DESCRIPTION_CHARS {
            return with;
        }
        cast.pop();
    }
    byline
}

/// The title of the English article at address path `path`:
/// `Dune (2021 film)` of `Dune_(2021_film)`.
fn article_title(path: &str) -> String {
    let decoded: String = url::form_urlencoded::parse(format!("t={path}").as_bytes())
        .next()
        .map(|(_, title)| title.into_owned())
        .unwrap_or_default();
    decoded.replace('_', " ")
}

/// The film or show `item`, of `kind` and with `sitelinks`, as an article
/// of the films set (see `plumb_index::pages::Page::from_film`): `None` with
/// no English name or title to find it by.
fn film_article(item: &str, kind: &str, sitelinks: u64, details: Details) -> Option<Article> {
    let article_title = details.article.as_deref().map(article_title);
    // The English label, or the article's title without its qualifier,
    // or its title in its own language.
    let title = details
        .label
        .clone()
        .or_else(|| {
            article_title
                .as_deref()
                .map(|title| match title.rfind(" (") {
                    Some(i) if title.ends_with(')') && i > 0 => title[..i].to_string(),
                    _ => title.to_string(),
                })
        })
        .or_else(|| details.titles.first().cloned())
        .or_else(|| details.any_label.clone())?;
    let mut aliases: Vec<String> = Vec::new();
    for alias in details.aliases.iter().chain(&details.titles) {
        if alias != &title && !aliases.contains(alias) {
            aliases.push(alias.clone());
        }
    }
    aliases.truncate(MAX_ALIASES);
    let mut profiles = details.profiles.clone();
    profiles.sort_by_key(|p| SERVICES.iter().position(|s| s.key == p.service));
    let item = match &details.article {
        Some(path) => format!("{item}/{path}"),
        None => item.to_string(),
    };
    Some(Article {
        description: Some(describe(kind, &details)),
        title,
        item: Some(item),
        site: None,
        views: sitelinks,
        aliases,
        profiles,
        website: None,
        package: None,
        facts: Vec::new(),
        lead: None,
        names: Vec::new(),
    })
}

/// Asks Wikidata's query service at `endpoint` for the films and shows to
/// keep under `options`, as the articles of the films set, the most linked
/// first.
pub async fn fetch_films(
    client: &reqwest::Client,
    endpoint: &str,
    pacing: WikidataPacing,
    options: &FilmOptions,
) -> Result<Vec<Article>> {
    let json = sparql_json(client, endpoint, &class_labels_query(), pacing)
        .await
        .context("asking Wikidata for the classes' labels")?;
    let classes = checked_classes(&json)?;
    if classes.is_empty() {
        bail!("no class of film or show checked out");
    }
    let json = sparql_json(
        client,
        endpoint,
        &crate::profiles::formatters_query(),
        pacing,
    )
    .await
    .context("asking Wikidata for the listings' formats")?;
    let services: Vec<&Service> = crate::profiles::checked_services(&json)?
        .into_iter()
        .filter(|s| FILM_SERVICES.contains(&s.key))
        .collect();
    let mut found = Found::new();
    for class in classes {
        let at = CLASSES.iter().position(|c| c == class).unwrap_or(0);
        let mut offset = 0;
        loop {
            tokio::time::sleep(pacing.pause).await;
            let json = sparql_json(client, endpoint, &items_page_query(class, offset), pacing)
                .await
                .with_context(|| format!("asking Wikidata for {} ({})", class.kind, class.item))?;
            let rows = add_items_page(&mut found, at, &json)?;
            info!(
                "{} ({}): {rows} items from {offset}",
                class.kind, class.item
            );
            if rows < ITEMS_PAGE {
                break;
            }
            offset += ITEMS_PAGE;
        }
    }
    info!("{} films and shows in all", found.len());
    let items = chosen(found, options);
    info!(
        "{} with at least {} sitelinks kept",
        items.len(),
        options.min_sitelinks
    );
    let ids: Vec<&str> = items.iter().map(|(item, _, _)| item.as_str()).collect();
    let mut details = HashMap::new();
    let batches = ids.len().div_ceil(DETAILS_BATCH);
    let mut failed = 0;
    for (n, batch) in ids.chunks(DETAILS_BATCH).enumerate() {
        tokio::time::sleep(pacing.pause).await;
        match sparql_json(client, endpoint, &details_query(batch, &services), pacing).await {
            Ok(json) => add_details(&mut details, &json)?,
            Err(err) => {
                failed += 1;
                warn!("left out {} films: {err:#}", batch.len());
                if failed as f64 > MOST_FAILED * batches as f64 && failed > 2 {
                    bail!("Wikidata failed {failed} of the first {} batches", n + 1);
                }
            }
        }
        if n % 50 == 0 {
            info!(
                "details: {} of {} films",
                (n + 1) * DETAILS_BATCH,
                ids.len()
            );
        }
    }
    let articles = films_of(items, details);
    let with_article = articles
        .iter()
        .filter(|a| a.item.as_deref().is_some_and(|i| i.contains('/')))
        .count();
    info!(
        "{} films and shows, {with_article} with an English article; {failed} batches failed",
        articles.len()
    );
    Ok(articles)
}

/// The articles of `items` (most linked first) from their `details`, in
/// that order, each title and year once.
fn films_of(
    items: Vec<(String, u64, &'static str)>,
    mut details: HashMap<String, Details>,
) -> Vec<Article> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter_map(|(item, links, kind)| {
            let details = details.remove(&item)?;
            film_article(&item, kind, links, details)
        })
        .filter(|article| seen.insert((article.title.to_lowercase(), article.description.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_the_classes_labels() {
        let json = r#"{"results": {"bindings": [
            {"class": {"value": "http://www.wikidata.org/entity/Q11424"}, "label": {"value": "film"}},
            {"class": {"value": "http://www.wikidata.org/entity/Q5398426"}, "label": {"value": "television series"}},
            {"class": {"value": "http://www.wikidata.org/entity/Q63952888"}, "label": {"value": "a moth"}}
        ]}}"#;
        let checked = checked_classes(json.as_bytes()).unwrap();
        let items: Vec<&str> = checked.iter().map(|c| c.item).collect();
        assert_eq!(items, ["Q5398426", "Q11424"]);
    }

    #[test]
    fn keeps_the_most_linked_items_of_their_first_class() {
        let mut found = Found::new();
        let film = CLASSES.iter().position(|c| c.item == "Q11424").unwrap();
        let animated = CLASSES.iter().position(|c| c.item == "Q202866").unwrap();
        let json = r#"{"results": {"bindings": [
            {"item": {"value": "http://www.wikidata.org/entity/Q1"}, "links": {"value": "50"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q2"}, "links": {"value": "2"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q3"}, "links": {"value": "80"}},
            {"item": {"value": "http://www.wikidata.org/entity/L5"}, "links": {"value": "80"}}
        ]}}"#;
        assert_eq!(
            add_items_page(&mut found, film, json.as_bytes()).unwrap(),
            4
        );
        let json = r#"{"results": {"bindings": [
            {"item": {"value": "http://www.wikidata.org/entity/Q1"}, "links": {"value": "50"}}
        ]}}"#;
        add_items_page(&mut found, animated, json.as_bytes()).unwrap();
        let options = FilmOptions {
            max_films: 10,
            min_sitelinks: 3,
        };
        assert_eq!(
            chosen(found, &options),
            [
                ("Q3".to_string(), 80, "Film"),
                ("Q1".to_string(), 50, "Animated film")
            ]
        );
    }

    fn details_json() -> &'static str {
        r#"{"results": {"bindings": [
            {"item": {"value": "http://www.wikidata.org/entity/Q25188"}, "k": {"value": "label"}, "v": {"value": "Inception"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q25188"}, "k": {"value": "article"}, "v": {"value": "https://en.wikipedia.org/wiki/Inception"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q25188"}, "k": {"value": "date"}, "v": {"value": "2010-07-16T00:00:00Z"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q25188"}, "k": {"value": "date"}, "v": {"value": "2010-07-08T00:00:00Z"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q25188"}, "k": {"value": "director"}, "v": {"value": "Christopher Nolan"}, "n": {"value": "90"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q25188"}, "k": {"value": "cast"}, "v": {"value": "Leonardo DiCaprio"}, "n": {"value": "150"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q25188"}, "k": {"value": "cast"}, "v": {"value": "Elliot Page"}, "n": {"value": "80"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q25188"}, "k": {"value": "cast"}, "v": {"value": "Dileep Rao"}, "n": {"value": "10"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q25188"}, "k": {"value": "cast"}, "v": {"value": "Tom Hardy"}, "n": {"value": "100"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q25188"}, "k": {"value": "imdb"}, "v": {"value": "tt1375666"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q25188"}, "k": {"value": "netflix"}, "v": {"value": "70131314"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q25188"}, "k": {"value": "imdb"}, "v": {"value": "not an id"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q1079"}, "k": {"value": "label"}, "v": {"value": "Breaking Bad"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q1079"}, "k": {"value": "start"}, "v": {"value": "2008-01-20T00:00:00Z"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q1079"}, "k": {"value": "end"}, "v": {"value": "2013-09-29T00:00:00Z"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q1079"}, "k": {"value": "creator"}, "v": {"value": "Vince Gilligan"}, "n": {"value": "30"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q1079"}, "k": {"value": "director"}, "v": {"value": "Someone Else"}, "n": {"value": "3"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q9"}, "k": {"value": "title"}, "v": {"value": "Les Dents de la nuit"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q9"}, "k": {"value": "date"}, "v": {"value": "2008-01-01T00:00:00Z"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q10"}, "k": {"value": "date"}, "v": {"value": "2008-01-01T00:00:00Z"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q11"}, "k": {"value": "label"}, "v": {"value": "Dune"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q11"}, "k": {"value": "alias"}, "v": {"value": "Dune: Part One"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q11"}, "k": {"value": "article"}, "v": {"value": "https://en.wikipedia.org/wiki/Dune_(2021_film)"}},
            {"item": {"value": "http://www.wikidata.org/entity/Q12"}, "k": {"value": "article"}, "v": {"value": "https://en.wikipedia.org/wiki/Schindler%27s_List"}}
        ]}}"#
    }

    #[test]
    fn writes_films_and_shows_from_their_details() {
        let mut details = HashMap::new();
        add_details(&mut details, details_json().as_bytes()).unwrap();
        let items = vec![
            ("Q1079".to_string(), 90, "TV series"),
            ("Q25188".to_string(), 80, "Film"),
            ("Q11".to_string(), 70, "Film"),
            ("Q9".to_string(), 4, "Film"),
            ("Q10".to_string(), 3, "Film"),
        ];
        let films = films_of(items, details);
        let lines: Vec<(&str, &str, &str)> = films
            .iter()
            .map(|a| {
                (
                    a.title.as_str(),
                    a.description.as_deref().unwrap(),
                    a.item.as_deref().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            lines,
            [
                ("Breaking Bad", "TV series by Vince Gilligan, 2008–2013", "Q1079"),
                (
                    "Inception",
                    "Film by Christopher Nolan, 2010 · with Leonardo DiCaprio, Tom Hardy, Elliot Page",
                    "Q25188/Inception"
                ),
                ("Dune", "Film", "Q11/Dune_(2021_film)"),
                // No English name: its own title.
                ("Les Dents de la nuit", "Film, 2008", "Q9"),
            ]
        );
        assert_eq!(films[1].views, 80);
        let profiles: Vec<&str> = films[1]
            .profiles
            .iter()
            .map(|p| p.service.as_str())
            .collect();
        assert_eq!(profiles, ["netflix", "imdb"]);
        assert_eq!(films[2].aliases, ["Dune: Part One"]);
        let mut details = HashMap::new();
        add_details(&mut details, details_json().as_bytes()).unwrap();
        assert_eq!(details["Q12"].article.as_deref(), Some("Schindler's_List"));
        assert!(plumb_core::films::describes_a_show(
            films[0].description.as_deref().unwrap()
        ));
    }

    #[test]
    fn names_as_much_cast_as_fits() {
        let details = Details {
            released: Some(1999),
            directors: vec![("D".repeat(100), 5)],
            cast: vec![("A".repeat(30), 3), ("B".repeat(30), 2)],
            ..Details::default()
        };
        let text = describe("Film", &details);
        assert!(
            text.chars().count() <= MAX_ARTICLE_DESCRIPTION_CHARS,
            "{text}"
        );
        assert!(
            text.ends_with(&format!("{CAST_SEPARATOR}{}", "A".repeat(30))),
            "{text}"
        );
        assert_eq!(article_title("Am%C3%A9lie_(film)"), "Amélie (film)");
        assert_eq!(year("-0044-03-15T00:00:00Z"), None);
        assert_eq!(year("1977-05-25T00:00:00Z"), Some(1977));
    }
}
