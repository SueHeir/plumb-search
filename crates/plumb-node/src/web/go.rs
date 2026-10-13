//! Opening a result (`/go`): noting the site opened, and blocking a site
//! from a results page.

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use plumb_index::{Hit, SearchOptions};
use serde::Deserialize;
use tracing::error;

use super::html::{http_url, safe_href, security_headers};
use super::params::SearchParams;
use super::search::{place_sites, run_places, run_search};
use super::{history, places, AppState, MAX_LIMIT};
use crate::about::About;
use crate::assembly::not_a_name;
use crate::learn::Block;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct GoParams {
    #[serde(default)]
    pub(crate) q: String,
    /// The domain picked.
    #[serde(default)]
    pub(crate) d: String,
    /// A box of the page (`places`, `news`) whose link `u` was opened.
    pub(crate) b: Option<String>,
    /// The page's token, for a search in ranking experiments, and the
    /// place of the result on it.
    pub(crate) x: Option<String>,
    pub(crate) p: Option<String>,
    pub(crate) u: Option<String>,
    pub(crate) country: Option<String>,
    pub(crate) only: Option<String>,
    pub(crate) exact: Option<String>,
    pub(crate) safe: Option<String>,
    pub(crate) lang: Option<String>,
    pub(crate) news: Option<String>,
}

/// `GET /go?q=&d=`: notes that `d` was picked for the query, when the node
/// shares popularity, and redirects to it. Only a site the same search
/// returns is redirected to; anything else goes back to the results.
pub(crate) async fn go(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<GoParams>,
) -> Response {
    let search = SearchParams {
        q: params.q,
        kind: None,
        after: None,
        before: None,
        order: None,
        run: None,
        limit: Some(MAX_LIMIT),
        country: params.country,
        only: params.only,
        full: None,
        net: None,
        exact: params.exact,
        debug: None,
        hist: None,
        hs: None,
        hr: None,
        hl: None,
        ht: None,
        edit: None,
        tune: None,
        safe: params.safe,
        lang: params.lang,
        news: params.news,
        format: None,
        pageno: None,
        safesearch: None,
        categories: None,
        time_range: None,
    };
    let query = search.query();
    let back = {
        let encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
        format!("/search?q={encoded}")
    };
    if let Some(block) = params.b.as_deref().and_then(Block::parse) {
        let href = params.u.unwrap_or_default();
        return go_block(&state, &headers, &search, &query, block, &href, &back).await;
    }
    let found = if query.is_empty() || state.setting_up().is_some() {
        None
    } else {
        let options = search.options(&state.settings, &headers);
        match run_search(&state, &query, MAX_LIMIT, &options).await {
            Ok(results) => match results.hits.iter().find(|hit| hit.domain == params.d) {
                Some(hit) => Some(hit.clone()),
                // Or one of the local sites the page puts first.
                None => {
                    place_site(&state, &headers, &query, &options, &results.hits, &params.d).await
                }
            },
            Err(_) => {
                error!("result redirect search failed");
                None
            }
        }
    };
    let Some((hit, href)) = found.and_then(|hit| safe_href(&hit).map(|href| (hit, href))) else {
        return redirect(&back);
    };
    if let Some(mut visitor) = history::Visitor::of(&state, &headers, None) {
        visitor.note_opened(&query, &hit.domain);
    }
    let token = params.x.as_deref().and_then(|x| x.parse::<u64>().ok());
    let place = params.p.as_deref().and_then(|p| p.parse::<usize>().ok());
    if let (Some(lab), Some(token), Some(place)) = (state.experiments(), token, place) {
        lab.note_click(token, place);
    }
    if state.shares_popularity() {
        if let Some(node) = state.node.clone() {
            let _ =
                tokio::task::spawn_blocking(move || node.record_pick(&query, &hit.domain)).await;
        }
    }
    redirect(&href)
}

/// The local site `domain` of the places the results page shows for
/// `query` above `hits`, if it is one.
async fn place_site(
    state: &AppState,
    headers: &HeaderMap,
    query: &str,
    options: &SearchOptions,
    hits: &[Hit],
    domain: &str,
) -> Option<Hit> {
    let about = match history::Visitor::of(state, headers, None) {
        Some(visitor) => visitor.about,
        None => history::browser_about(headers),
    };
    let town = about.town();
    let found = not_a_name(
        run_places(state, query, town, options.country.as_deref()).await,
        hits,
    )?;
    place_sites(state, &found)
        .await
        .into_iter()
        .find(|hit| hit.domain == domain)
}

/// `/go?q=&b=&u=`: notes that a link of `block` was opened for the query,
/// and goes on to `href` when the same search shows it in that box;
/// anything else goes back to the results.
async fn go_block(
    state: &AppState,
    headers: &HeaderMap,
    search: &SearchParams,
    query: &str,
    block: Block,
    href: &str,
    back: &str,
) -> Response {
    if query.is_empty() || href.is_empty() || state.setting_up().is_some() {
        return redirect(back);
    }
    let mut visitor = history::Visitor::of(state, headers, None);
    let options = search.options(&state.settings, headers);
    let links: Vec<String> = match block {
        Block::Places => {
            let browser_about = visitor.is_none().then(|| history::browser_about(headers));
            let town = match &visitor {
                Some(visitor) => visitor.about.town(),
                None => browser_about.as_ref().and_then(About::town),
            };
            run_places(state, query, town, options.country.as_deref())
                .await
                .map(|found| found.hits.iter().flat_map(places::place_links).collect())
                .unwrap_or_default()
        }
        Block::News => match run_search(state, query, MAX_LIMIT, &options).await {
            Ok(results) => state
                .recent(query, &results)
                .map(|recent| {
                    recent
                        .headlines
                        .iter()
                        .filter_map(|h| http_url(&h.url))
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => {
                error!("box link redirect search failed");
                Vec::new()
            }
        },
    };
    if !links.iter().any(|link| link == href) {
        return redirect(back);
    }
    if let Some(visitor) = &mut visitor {
        visitor.note_used(query, block);
    }
    redirect(href)
}

pub(crate) fn redirect(location: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        security_headers(),
        [
            (header::LOCATION, location.to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
    )
        .into_response()
}
