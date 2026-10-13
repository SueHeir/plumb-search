//! The JSON API: searches, recent headlines and a node's status.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::{header, Extensions, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use plumb_core::SiteRecord;
use plumb_index::pages::{place_operator_pages, place_pages};
use plumb_index::{Hit, SearchResults};
use serde::Serialize;
use tracing::error;

use super::home::SETUP_RELOAD_SECONDS;
use super::html::security_headers;
use super::network::{network_search, TooManySearches};
use super::params::{flag, SearchParams};
use super::search::{
    extras, paper_response, place_sites, plugin_about, route_sources, run_places, run_search,
    shown_results,
};
use super::{answers, mcp, places, AppState};
use crate::news::Recent;
use crate::plugins::PluginResults;

pub(crate) async fn api_search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    if let Some(status) = state.setting_up() {
        let body = serde_json::json!({
            "error": "the search index is not ready yet",
            "phase": status.phase,
            "step": status.step,
        });
        let retry_after = SETUP_RELOAD_SECONDS.to_string();
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            security_headers(),
            [(header::RETRY_AFTER, retry_after)],
            Json(body),
        )
            .into_response();
    }
    match params.paper_search_query() {
        Ok(Some(query)) => return paper_response(&state, &query, &params, &headers, true).await,
        Ok(None) => {}
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                security_headers(),
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    }
    let query = params.query();
    let full = flag(&params.full);
    if query.is_empty() {
        return if full {
            (
                StatusCode::OK,
                security_headers(),
                Json(SearchResults::default()),
            )
                .into_response()
        } else {
            (StatusCode::OK, security_headers(), Json(Vec::<Hit>::new())).into_response()
        };
    }
    let options = params.options(&state.settings, &headers);
    let found = run_search(&state, &query, params.limit(), &options).await;
    match found {
        Ok(mut results) if full => {
            let places = crate::assembly::places(
                &query,
                run_places(&state, &query, None, options.country.as_deref()).await,
                &results.hits,
                &options,
                |domain| {
                    state
                        .node
                        .as_ref()
                        .is_some_and(|node| node.blocks_adult(domain))
                },
            );
            if let Some(found) = &places {
                let local = place_sites(&state, &found.found, &options).await;
                places::local_first(&found.found, &mut results.hits, local, params.limit());
            }
            let extras = extras(&state, &query, &options, None).await;
            route_sources(
                &state,
                &query,
                extras.answer.as_ref().map(|a| a.kind),
                options.country.as_deref(),
                &mut results,
                params.limit(),
            );
            let about = plugin_about(&state, &query, &results, &extras, &options).await;
            let shown_to_plugins = if state.settings.plugins.any_annotate() {
                shown_results(&results, params.limit())
            } else {
                Vec::new()
            };
            let (plugins, plugin_notes) = tokio::join!(
                state.plugin_results(&query, &options, about.as_ref(), params.run.as_deref()),
                state.settings.plugins.annotate(&query, &shown_to_plugins)
            );
            let plugin_offers =
                state
                    .settings
                    .plugins
                    .offers(&query, about.as_ref(), params.run.as_deref());
            let info = match &extras.profile {
                Some(profile) => answers::info_from_page(&profile.page, &results.hits),
                // As on the results page: operators change what the query
                // names, and a sum names nothing.
                None if plumb_core::Operators::parse(&query).any()
                    || answers::is_sum(extras.answer.as_ref()) =>
                {
                    None
                }
                None => {
                    let placed = place_pages(
                        &query,
                        &results.hits,
                        results.pages.iter().map(|p| p.hit.clone()).collect(),
                    );
                    answers::info_box(&results.hits, &placed)
                }
            };
            let recent = crate::assembly::recent(
                &query,
                state.recent(&query, &results, &options),
                &options,
                params.limit(),
                |domain| {
                    state
                        .node
                        .as_ref()
                        .is_some_and(|node| node.blocks_adult(domain))
                },
            );
            let placed = if plumb_core::Operators::parse(&query).any() {
                place_operator_pages(
                    &plumb_core::Operators::parse(&query),
                    &results.hits,
                    results.pages.iter().map(|p| p.hit.clone()).collect(),
                )
            } else {
                place_pages(
                    &query,
                    &results.hits,
                    results.pages.iter().map(|p| p.hit.clone()).collect(),
                )
            };
            let assembled = crate::assembly::Assembled {
                rows: crate::assembly::ordered_rows(&results.hits, &placed, params.limit()),
                places: places.clone(),
                recent: recent.clone(),
                answer: extras.answer.as_ref(),
                profile: extras.profile.as_ref(),
            };
            let mut limited = SearchResults {
                site_search: results.site_search.clone(),
                spelling: results.spelling.clone(),
                ..Default::default()
            };
            for row in &assembled.rows {
                match row {
                    crate::assembly::Row::Site { site, pages } => {
                        limited.hits.push((*site).clone());
                        limited.pages.extend(pages.iter().map(|p| (**p).clone()));
                    }
                    crate::assembly::Row::Page { page } => limited.pages.push((*page).clone()),
                }
            }
            let body = FullResults {
                assembled,
                recent,
                results: &limited,
                answer: extras.answer.clone(),
                profile: extras.profile.clone(),
                info,
                places,
                plugins,
                plugin_notes,
                plugin_offers,
            };
            (StatusCode::OK, security_headers(), Json(body)).into_response()
        }
        Ok(results) => (StatusCode::OK, security_headers(), Json(results.hits)).into_response(),
        Err(_) => {
            error!("search API local lookup failed");
            let body = serde_json::json!({ "error": "search failed" });
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                security_headers(),
                Json(body),
            )
                .into_response()
        }
    }
}

/// `GET /api/recent?q=...`: the "Recent" block the results page shows
/// for the query, as JSON; `{"headlines":[]}` when it shows none.
pub(crate) async fn api_recent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    let query = params.query();
    let recent = if query.is_empty() || state.setting_up().is_some() {
        None
    } else {
        let options = params.options(&state.settings, &headers);
        match run_search(&state, &query, params.limit(), &options).await {
            Ok(results) => crate::assembly::recent(
                &query,
                state.recent(&query, &results, &options),
                &options,
                params.limit(),
                |domain| {
                    state
                        .node
                        .as_ref()
                        .is_some_and(|node| node.blocks_adult(domain))
                },
            ),
            Err(_) => None,
        }
    };
    (
        StatusCode::OK,
        security_headers(),
        Json(recent.unwrap_or_default()),
    )
        .into_response()
}

/// `/api/search?full=1`: the results, and the instant answer and info box
/// the results page shows with them.
#[derive(Serialize)]
struct FullResults<'a> {
    assembled: crate::assembly::Assembled<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recent: Option<Recent>,
    #[serde(flatten)]
    results: &'a SearchResults,
    #[serde(skip_serializing_if = "Option::is_none")]
    answer: Option<plumb_answer::Answer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<answers::ProfileAnswer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    info: Option<answers::InfoBox>,
    #[serde(skip_serializing_if = "Option::is_none")]
    places: Option<crate::assembly::Places>,
    /// What the node's plugins found; never from other nodes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    plugins: Vec<PluginResults>,
    /// What the node's plugins say about its results, by their addresses.
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    plugin_notes: crate::plugins::ResultNotes,
    /// Plugins this search fits but did not run, with the search that
    /// runs each.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    plugin_offers: Vec<crate::plugins::Offer>,
}

/// `GET /api/network/search?q=&limit=`: [`NetworkResults`] as JSON.
pub(crate) async fn api_network_search(
    State(state): State<AppState>,
    extensions: Extensions,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    let client = mcp::client_of(&extensions, &headers);
    if state.network().is_none() {
        let body = serde_json::json!({ "error": "this node has not joined the Plumb network" });
        return (StatusCode::NOT_FOUND, security_headers(), Json(body)).into_response();
    }
    let query = params.query();
    if query.is_empty() {
        return (
            StatusCode::OK,
            security_headers(),
            Json(NetworkResults::default()),
        )
            .into_response();
    }
    let options = params.options(&state.settings, &headers);
    match network_search(&state, client, &query, params.limit(), &options).await {
        Ok(results) => (StatusCode::OK, security_headers(), Json(results)).into_response(),
        Err(err) if err.downcast_ref::<TooManySearches>().is_some() => {
            let wait = err.downcast_ref::<TooManySearches>().map_or(60, |t| t.0);
            let body = serde_json::json!({ "error": err.to_string() });
            (
                StatusCode::TOO_MANY_REQUESTS,
                security_headers(),
                [(header::RETRY_AFTER, wait.to_string())],
                Json(body),
            )
                .into_response()
        }
        Err(_) => {
            error!("network search API lookup failed");
            let body = serde_json::json!({ "error": "network search failed" });
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                security_headers(),
                Json(body),
            )
                .into_response()
        }
    }
}

/// What a network search found, ranked by this node.
#[derive(Debug, Default, Serialize)]
pub struct NetworkResults {
    /// Buckets asked for, padding included.
    pub buckets: usize,
    /// Requests sent, each to one node under its own throwaway identity.
    pub asked: usize,
    pub answered: usize,
    /// Answered sealed through another node, so the node answering never
    /// saw this node's IP address.
    pub relayed: usize,
    /// Sent straight to the node answering, because no other node could
    /// relay; that node saw this node's IP address.
    pub direct: usize,
    /// Answers dropped because a proof in them did not check out.
    pub rejected: usize,
    /// The query's buckets answered from this node's copies of ones it
    /// fetched lately, without asking the network again.
    pub cached: usize,
    /// Needed buckets waiting for a scheduled background download.
    pub pending: usize,
    /// Retained buckets used while their background refresh is due.
    pub stale: usize,
    pub hits: Vec<NetworkResult>,
}

/// One site from other nodes, ranked here.
#[derive(Debug, Serialize)]
pub struct NetworkResult {
    #[serde(flatten)]
    pub hit: Hit,
    /// The text comes from a signed crawl whose proof checked out.
    pub verified: bool,
    /// The node that signed that crawl.
    pub crawler: Option<String>,
    /// Signed crawls from two or more different nodes agree on the text.
    pub confirmed: bool,
    /// How many answers held the site.
    pub answers: usize,
    /// The signed crawl this node may keep, see
    /// [`FoundSite::shared`](plumb_net::FoundSite::shared).
    #[serde(skip)]
    pub shared: Option<SiteRecord>,
}

/// `GET /api/status`, routed for nodes only.
pub(crate) async fn api_status(State(state): State<AppState>) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        StatusCode::OK,
        security_headers(),
        [(header::CACHE_CONTROL, "no-store")],
        Json({
            let mut status = serde_json::to_value(node.status()).expect("serializable status");
            status["build"] =
                serde_json::to_value(crate::build_info::current()).expect("serializable build");
            status
        }),
    )
        .into_response()
}
