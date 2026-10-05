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
//! Many apps hand the model only each result's `content`, never loading
//! the page, so the instant answer also leads the first result's `content`.
//!
//! Taken from SearXNG's parameters: `q`, `pageno`, `safesearch` (0, 1 or
//! 2), `categories` (`news` alone lists only recent headlines) and
//! `time_range` (any value puts recent headlines first); Plumb's own
//! `country`, `safe` and `lang` work too. The others (`engines`,
//! `language`) are ignored. Headlines carry `publishedDate`.
//!
//! `POST /api/websearch` answers Open WebUI's "external" web search
//! engine: `{"query": "...", "count": 5}` in, `[{"link", "title",
//! "snippet"}]` out, the same results in the same order.

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use plumb_answer::Answer;
use plumb_core::SafeSearch;
use plumb_index::pages::{place_operator_pages, place_pages, PlacedPage};
use plumb_index::Hit;
use serde_json::{json, Value};
use tracing::error;
use url::Url;

use super::answers::InfoBox;
use super::{
    answers, extras, run_search, security_headers, AppState, SearchParams, MAX_LIMIT,
    MAX_QUERY_CHARS, SETUP_RELOAD_SECONDS,
};

/// Most pages of results asked for.
const MAX_PAGENO: usize = 10;

/// One result, as SearXNG lists it.
struct Entry {
    url: String,
    title: String,
    content: String,
    category: &'static str,
    /// When it was published (Unix seconds), for headlines.
    published: Option<u64>,
}

impl Entry {
    fn site(hit: &Hit) -> Self {
        Entry {
            url: hit.url.clone(),
            title: hit.title.clone().unwrap_or_else(|| hit.domain.clone()),
            content: hit.description.clone().unwrap_or_default(),
            category: "general",
            published: None,
        }
    }

    fn page(placed: &PlacedPage) -> Self {
        let page = &placed.hit.page;
        Entry {
            url: page.url.clone(),
            title: page.title.clone(),
            content: match (&page.description, &page.package) {
                (description, Some(package)) => {
                    let card = package.summary();
                    match description {
                        Some(d) if !d.is_empty() => format!("{d} {card}"),
                        _ => card,
                    }
                }
                (description, None) => description.clone().unwrap_or_default(),
            },
            category: "general",
            published: None,
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
            "publishedDate": self.published.and_then(published_date),
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

/// Unix seconds as SearXNG writes dates: `2026-10-05T08:00:00`.
fn published_date(at: u64) -> Option<String> {
    let at = chrono::DateTime::from_timestamp(i64::try_from(at).ok()?, 0)?;
    Some(at.format("%Y-%m-%dT%H:%M:%S").to_string())
}

/// What `categories` and `time_range` ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    Everything,
    /// Recent headlines first.
    RecentFirst,
    /// Recent headlines only.
    NewsOnly,
}

impl Wanted {
    fn of(params: &SearchParams) -> Self {
        let categories: Vec<&str> = params
            .categories
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        if !categories.is_empty() && categories.iter().all(|c| c.eq_ignore_ascii_case("news")) {
            Wanted::NewsOnly
        } else if params
            .time_range
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
        {
            Wanted::RecentFirst
        } else {
            Wanted::Everything
        }
    }
}

/// Everything a search shows, in order.
struct Collected {
    entries: Vec<Entry>,
    answer: Option<Answer>,
    info: Option<InfoBox>,
    spelling: Option<plumb_index::Spelling>,
}

async fn collect(
    state: &AppState,
    query: &str,
    limit: usize,
    options: &plumb_index::SearchOptions,
    wanted: Wanted,
) -> anyhow::Result<Collected> {
    let results = run_search(state, query, limit, options).await?;
    let extras = extras(state, query, &results, options).await;
    let found_pages = results.pages.iter().map(|p| p.hit.clone()).collect();
    let operators = plumb_core::Operators::parse(query);
    let placed = if operators.any() {
        place_operator_pages(&operators, &results.hits, found_pages)
    } else {
        place_pages(query, &results.hits, found_pages)
    };
    let info = match &extras.profile {
        Some(profile) => answers::info_from_page(&profile.page, &results.hits),
        None if operators.any() => None,
        None => answers::info_box(&results.hits, &placed),
    };
    let recent = state.recent(query, &results);

    let mut entries = Vec::new();
    if let Some(profile) = &extras.profile {
        entries.push(Entry {
            url: profile.url.clone(),
            title: format!("{} on {}", profile.of, profile.service),
            content: "Official profile, from Wikidata".to_string(),
            category: "general",
            published: None,
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
    let now = plumb_core::now_unix();
    let headlines: Vec<Entry> = recent
        .iter()
        .flat_map(|recent| &recent.headlines)
        .map(|headline| Entry {
            url: headline.url.clone(),
            title: headline.title.clone(),
            content: format!("{}, {}", headline.domain, super::time_ago(headline.at, now)),
            category: "news",
            published: Some(headline.at),
        })
        .collect();
    match wanted {
        Wanted::NewsOnly => entries = headlines,
        Wanted::RecentFirst => {
            entries.splice(0..0, headlines);
        }
        Wanted::Everything => {
            let at = entries.len().min(1);
            entries.splice(at..at, headlines);
        }
    }
    // Apps that only read snippets still get the answer.
    if let (Some(answer), Some(first)) = (&extras.answer, entries.first_mut()) {
        let line = crate::mcp::answer_line(&answer.question, &answer.answer);
        first.content = if first.content.is_empty() {
            format!("Answer: {line}.")
        } else {
            format!("Answer: {line}. {}", first.content)
        };
    }
    Ok(Collected {
        entries,
        answer: extras.answer,
        info,
        spelling: results.spelling,
    })
}

fn not_ready(state: &AppState) -> Option<Response> {
    let status = state.setting_up()?;
    let body = json!({
        "error": "the search index is not ready yet",
        "phase": status.phase,
        "step": status.step,
    });
    Some(
        (
            StatusCode::SERVICE_UNAVAILABLE,
            security_headers(),
            [(header::RETRY_AFTER, SETUP_RELOAD_SECONDS.to_string())],
            Json(body),
        )
            .into_response(),
    )
}

pub(super) async fn search(state: AppState, headers: HeaderMap, params: SearchParams) -> Response {
    let query = params.query();
    if let Some(response) = not_ready(&state) {
        return response;
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
    let collected = match collect(
        &state,
        &query,
        per_page * pageno,
        &options,
        Wanted::of(&params),
    )
    .await
    {
        Ok(collected) => collected,
        Err(_) => {
            error!("SearXNG-style search failed");
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "search failed" }),
            );
        }
    };
    let entries = &collected.entries;
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
    if let Some(answer) = &collected.answer {
        let mut text = crate::mcp::answer_line(&answer.question, &answer.answer);
        if let Some(note) = &answer.note {
            text.push_str(&format!(" ({note})"));
        }
        fields.insert(
            "answers".into(),
            json!([{ "answer": text, "engine": "plumb" }]),
        );
    }
    if let Some(spelling) = &collected.spelling {
        fields.insert("suggestions".into(), json!([spelling.query]));
    }
    if let Some(info) = collected.info {
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

/// What Open WebUI's external web search sends.
#[derive(Debug, serde::Deserialize)]
pub(super) struct ExternalSearch {
    #[serde(default)]
    query: String,
    count: Option<usize>,
}

/// `POST /api/websearch`: Open WebUI's "external" web search engine.
pub(super) async fn external(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(asked): Json<ExternalSearch>,
) -> Response {
    if let Some(response) = not_ready(&state) {
        return response;
    }
    let query = plumb_core::truncate_chars(
        &plumb_core::collapse_whitespace(&asked.query),
        MAX_QUERY_CHARS,
    );
    if query.is_empty() {
        return reply(StatusCode::OK, json!([]));
    }
    let count = asked
        .count
        .unwrap_or(super::DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let params = SearchParams::default();
    let options = params.options(&state.settings.home, &headers);
    match collect(&state, &query, count, &options, Wanted::Everything).await {
        Ok(collected) => {
            let results: Vec<Value> = collected
                .entries
                .iter()
                .take(count)
                .map(|entry| json!({ "link": entry.url, "title": entry.title, "snippet": entry.content }))
                .collect();
            reply(StatusCode::OK, json!(results))
        }
        Err(_) => {
            error!("external web search failed");
            reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": "search failed" }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_categories_read_as_searxng_writes_them() {
        assert_eq!(
            published_date(1_790_000_000).as_deref(),
            Some("2026-09-21T14:13:20")
        );
        let params = |categories: Option<&str>, time_range: Option<&str>| SearchParams {
            categories: categories.map(str::to_string),
            time_range: time_range.map(str::to_string),
            ..SearchParams::default()
        };
        assert_eq!(Wanted::of(&params(None, None)), Wanted::Everything);
        assert_eq!(Wanted::of(&params(Some("news"), None)), Wanted::NewsOnly);
        assert_eq!(
            Wanted::of(&params(Some("general,news"), None)),
            Wanted::Everything
        );
        assert_eq!(
            Wanted::of(&params(Some("general"), Some("day"))),
            Wanted::RecentFirst
        );
    }
}
