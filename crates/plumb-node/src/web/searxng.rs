//! `/search?format=json`: results in SearXNG's JSON format, so the AI apps
//! that take a SearXNG address for web search (Open WebUI, Perplexica,
//! LibreChat and others) can use a Plumb node instead, with no change on
//! their side: give them `http://127.0.0.1:7586/search?q=<query>`.
//!
//! The results are those of the results page, in its order: an official
//! profile asked for, then the sites with Wikipedia articles, Stack
//! Overflow questions and other pages where the page puts them, and recent
//! headlines after the best result. The instant answer goes in `answers`
//! and the info box in `infoboxes`, as SearXNG puts its own.
//!
//! Taken from SearXNG's parameters: `q`, `pageno` and `safesearch` (0, 1
//! or 2); Plumb's own `country`, `safe` and `lang` work too. The others
//! (`categories`, `engines`, `time_range`, `language`) are ignored.

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use plumb_core::SafeSearch;
use plumb_index::pages::{place_operator_pages, place_pages, PlacedPage};
use plumb_index::Hit;
use serde_json::{json, Value};
use tracing::error;
use url::Url;

use super::{
    answers, extras, run_search, searched_for, security_headers, AppState, SearchParams,
    SETUP_RELOAD_SECONDS,
};

/// Most pages of results asked for.
const MAX_PAGENO: usize = 10;

/// One result, as SearXNG lists it.
struct Entry {
    url: String,
    title: String,
    content: String,
    category: &'static str,
}

impl Entry {
    fn site(hit: &Hit) -> Self {
        Entry {
            url: hit.url.clone(),
            title: hit.title.clone().unwrap_or_else(|| hit.domain.clone()),
            content: hit.description.clone().unwrap_or_default(),
            category: "general",
        }
    }

    fn page(placed: &PlacedPage) -> Self {
        let page = &placed.hit.page;
        Entry {
            url: page.url.clone(),
            title: page.title.clone(),
            content: page.description.clone().unwrap_or_default(),
            category: "general",
        }
    }

    fn to_json(&self, position: usize) -> Value {
        let parsed = Url::parse(&self.url).ok();
        let parsed_url = parsed.as_ref().map(|url| {
            let netloc = match url.port() {
                Some(port) => format!("{}:{port}", url.host_str().unwrap_or("")),
                None => url.host_str().unwrap_or("").to_string(),
            };
            json!([
                url.scheme(),
                netloc,
                url.path(),
                "",
                url.query().unwrap_or(""),
                url.fragment().unwrap_or(""),
            ])
        });
        json!({
            "url": self.url,
            "title": self.title,
            "content": self.content,
            "engine": "plumb",
            "engines": ["plumb"],
            "positions": [position],
            "score": 1.0 / position as f64,
            "category": self.category,
            "parsed_url": parsed_url,
        })
    }
}

/// SearXNG's `safesearch`: 0 off, 1 moderate, 2 strict.
fn safesearch(level: &str) -> Option<SafeSearch> {
    match level.trim() {
        "0" => Some(SafeSearch::Off),
        "1" => Some(SafeSearch::Moderate),
        "2" => Some(SafeSearch::Strict),
        _ => None,
    }
}

fn reply(status: StatusCode, body: Value) -> Response {
    (status, security_headers(), Json(body)).into_response()
}

pub(super) async fn search(state: AppState, headers: HeaderMap, params: SearchParams) -> Response {
    let query = params.query();
    if let Some(status) = state.setting_up() {
        let body = json!({
            "error": "the search index is not ready yet",
            "phase": status.phase,
            "step": status.step,
        });
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            security_headers(),
            [(header::RETRY_AFTER, SETUP_RELOAD_SECONDS.to_string())],
            Json(body),
        )
            .into_response();
    }
    let mut body = json!({
        "query": query,
        "number_of_results": 0,
        "results": [],
        "answers": [],
        "corrections": [],
        "infoboxes": [],
        "suggestions": [],
        "unresponsive_engines": [],
    });
    if query.is_empty() {
        return reply(StatusCode::OK, body);
    }
    let per_page = params.limit();
    let pageno = params.pageno.unwrap_or(1).clamp(1, MAX_PAGENO);
    let mut options = params.options(&state.settings.home, &headers);
    if params.safe.is_none() {
        if let Some(level) = params.safesearch.as_deref().and_then(safesearch) {
            options.safe = level;
        }
    }
    let results = match run_search(&state, &query, per_page * pageno, &options).await {
        Ok(results) => results,
        Err(_) => {
            error!("SearXNG-style search failed");
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "search failed" }),
            );
        }
    };
    let extras = extras(&state, &query, &results, &options).await;
    let found_pages = results.pages.iter().map(|p| p.hit.clone()).collect();
    let operators = plumb_core::Operators::parse(&query);
    let placed = if operators.any() {
        place_operator_pages(&operators, &results.hits, found_pages)
    } else {
        place_pages(searched_for(&query, &results), &results.hits, found_pages)
    };
    let info = match &extras.profile {
        Some(profile) => answers::info_from_page(&profile.page, &results.hits),
        None if operators.any() => None,
        None => answers::info_box(&results.hits, &placed),
    };
    let recent = state.recent(&query, &results);

    let mut entries = Vec::new();
    if let Some(profile) = &extras.profile {
        entries.push(Entry {
            url: profile.url.clone(),
            title: format!("{} on {}", profile.of, profile.service),
            content: "Official profile, from Wikidata".to_string(),
            category: "general",
        });
    }
    let hits = &results.hits;
    for (i, hit) in hits.iter().enumerate() {
        for page in placed.iter().filter(|p| p.under.is_none() && p.at == i) {
            entries.push(Entry::page(page));
        }
        entries.push(Entry::site(hit));
        for page in placed
            .iter()
            .filter(|p| p.under.as_deref() == Some(hit.domain.as_str()))
        {
            entries.push(Entry::page(page));
        }
    }
    for page in placed
        .iter()
        .filter(|p| p.under.is_none() && p.at >= hits.len())
    {
        entries.push(Entry::page(page));
    }
    if let Some(recent) = &recent {
        let now = plumb_core::now_unix();
        let at = entries.len().min(1);
        let headlines = recent.headlines.iter().map(|headline| Entry {
            url: headline.url.clone(),
            title: headline.title.clone(),
            content: format!("{}, {}", headline.domain, super::time_ago(headline.at, now)),
            category: "news",
        });
        entries.splice(at..at, headlines);
    }

    let total = entries.len();
    let shown: Vec<Value> = entries
        .iter()
        .enumerate()
        .skip((pageno - 1) * per_page)
        .take(per_page)
        .map(|(i, entry)| entry.to_json(i + 1))
        .collect();
    let fields = body.as_object_mut().expect("an object");
    fields.insert("number_of_results".into(), json!(total));
    fields.insert("results".into(), json!(shown));
    if let Some(answer) = &extras.answer {
        let mut text = crate::mcp::answer_line(&answer.question, &answer.answer);
        if let Some(note) = &answer.note {
            text.push_str(&format!(" ({note})"));
        }
        fields.insert(
            "answers".into(),
            json!([{ "answer": text, "engine": "plumb" }]),
        );
    }
    if let Some(spelling) = &results.spelling {
        let key = if spelling.applied {
            "corrections"
        } else {
            "suggestions"
        };
        fields.insert(key.into(), json!([spelling.query]));
    }
    if let Some(info) = info {
        let mut urls = Vec::new();
        if let Some(site) = info.site.as_deref().and_then(super::homepage_url) {
            urls.push(json!({ "title": "Official site", "url": site }));
        }
        for profile in &info.profiles {
            urls.push(json!({ "title": profile.service, "url": profile.url }));
        }
        if let Some(article) = &info.article {
            urls.push(json!({ "title": "Wikipedia", "url": article }));
        }
        if let Some(item) = &info.wikidata {
            urls.push(json!({ "title": "Wikidata", "url": item }));
        }
        let attributes: Vec<Value> = info
            .country
            .iter()
            .map(|country| json!({ "label": "Country", "value": country }))
            .collect();
        fields.insert(
            "infoboxes".into(),
            json!([{
                "infobox": info.title,
                "id": info.article.clone().or(info.wikidata.clone()),
                "content": info.description,
                "urls": urls,
                "attributes": attributes,
                "engine": "plumb",
                "engines": ["plumb"],
            }]),
        );
    }
    reply(StatusCode::OK, body)
}
