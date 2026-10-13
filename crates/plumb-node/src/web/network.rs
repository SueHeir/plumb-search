//! Searching the network from the web pages, with a limit on how often.

use std::net::IpAddr;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{Extensions, HeaderMap, StatusCode, Uri};
use axum::response::Response;
use plumb_core::{SafeSearch, SiteRecord};
use plumb_index::{build_index, RankConfig, SearchOptions, Searcher};
use plumb_net::{FoundSite, NetSearch};
use tracing::error;

use super::api::{NetworkResult, NetworkResults};
use super::home::{bad_search_query, home_or_setup, message_page};
use super::html::html_response;
use super::params::{error_settings, SearchParams};
use super::results::{render_error, render_network};
use super::{mcp, AppState, NETWORK_SEARCH_WAIT};

/// `GET /network?q=`: only what other nodes answer, for a node in the network.
pub(crate) async fn network_page(
    State(state): State<AppState>,
    extensions: Extensions,
    headers: HeaderMap,
    uri: Uri,
    params: Result<Query<SearchParams>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let params = match params {
        Ok(Query(params)) => params,
        Err(_) => return bad_search_query(&state, &headers, &uri),
    };
    let settings = error_settings(&state, &params, &headers);
    let client = mcp::client_of(&extensions, &headers);
    if state.network().is_none() {
        return html_response(
            StatusCode::NOT_FOUND,
            message_page(
                "Network search unavailable",
                "This node does not search the network. You can still search its own index.",
                &params.query(),
                &settings,
            ),
        );
    }
    let query = params.query();
    if query.is_empty() {
        return home_or_setup(&state, &params, &headers);
    }
    let options = params.options(&state.settings, &headers);
    match network_search(&state, client, &query, params.limit(), &options).await {
        Ok(results) => {
            let domains = results.hits.iter().map(|r| r.hit.domain.clone()).collect();
            let icons = state.icons(domains).await;
            html_response(
                StatusCode::OK,
                render_network(&query, &results, &icons, &settings),
            )
        }
        Err(_) => {
            error!("network search page lookup failed");
            html_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                render_error(&query, &settings),
            )
        }
    }
}

/// Fetches the query's buckets from other nodes and ranks the sites that
/// match with this node's own ranking and the searcher's choices, in a
/// small index built for the purpose and deleted after.
/// A client searched the network too often; it may again in this many
/// seconds.
#[derive(Debug)]
pub(crate) struct TooManySearches(pub(crate) u64);

impl std::fmt::Display for TooManySearches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "too many network searches; try again in {} seconds",
            self.0
        )
    }
}

impl std::error::Error for TooManySearches {}

pub(crate) async fn network_search(
    state: &AppState,
    client: Option<IpAddr>,
    query: &str,
    limit: usize,
    options: &SearchOptions,
) -> Result<NetworkResults> {
    let net = state.network().context("not in the network")?;
    if let Err(wait) = state.net_limiter.take(client, Instant::now()) {
        return Err(TooManySearches(wait).into());
    }
    let rank = state
        .node
        .as_ref()
        .map_or_else(RankConfig::default, |node| node.rank());
    // Search operators narrow the ranking below; only words pick buckets.
    let lookup = plumb_core::Operators::parse(query).lookup_text();
    let found = net.search(&lookup, NETWORK_SEARCH_WAIT).await?;
    let query = query.to_string();
    let options = options.clone();
    let node = state.node.clone();
    tokio::task::spawn_blocking(move || {
        // Signed crawls of sites this node holds fill in what its own
        // records lack, for its next index and search by meaning.
        if let Some(node) = &node {
            let shared: Vec<SiteRecord> = found
                .found
                .iter()
                .filter_map(|site| site.keeps().cloned())
                .collect();
            if !shared.is_empty() {
                node.keep_from_network(shared);
            }
        }
        let mut results = rank_found(found, &query, limit, &rank, &options)?;
        // Adult sites stay out of network results too.
        if let (Some(node), true) = (&node, options.safe != SafeSearch::Off) {
            results
                .hits
                .retain(|result| !node.blocks_adult(&result.hit.domain));
        }
        Ok(results)
    })
    .await
    .context("the ranking task failed")?
}

fn rank_found(
    found: NetSearch,
    query: &str,
    limit: usize,
    rank: &RankConfig,
    options: &SearchOptions,
) -> Result<NetworkResults> {
    let mut results = NetworkResults {
        buckets: found.buckets,
        asked: found.asked,
        answered: found.answered,
        relayed: found.relayed,
        direct: found.direct,
        rejected: found.rejected,
        cached: found.cached,
        pending: found.pending,
        stale: found.stale,
        hits: Vec::new(),
    };
    if found.found.is_empty() || limit == 0 {
        return Ok(results);
    }
    let dir = tempfile::tempdir().context("making a folder for ranking")?;
    let index = dir.path().join("index");
    let records: Vec<SiteRecord> = found.found.iter().map(|f| f.record.clone()).collect();
    build_index(&index, &records)?;
    // The few sites found are no dictionary to correct typos against: the
    // query was corrected, if at all, by this node's own index.
    let options = SearchOptions {
        exact: true,
        ..options.clone()
    };
    let hits = Searcher::open(&index)?
        .search_full(query, limit, rank, &options)?
        .hits;
    let by_domain: std::collections::HashMap<&str, &FoundSite> = found
        .found
        .iter()
        .map(|f| (f.record.domain.as_str(), f))
        .collect();
    for hit in hits {
        let Some(site) = by_domain.get(hit.domain.as_str()) else {
            continue;
        };
        results.hits.push(NetworkResult {
            verified: site.verified,
            crawler: site.crawler.clone(),
            confirmed: site.confirmed,
            answers: site.answers,
            shared: site.shared.clone(),
            hit,
        });
    }
    Ok(results)
}
