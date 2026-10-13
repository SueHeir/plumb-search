//! A search from the results page: this node's results, the network's,
//! pages, places, answers and plugins, put together.

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{Extensions, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use plumb_core::{now_unix, RecentNews};
use plumb_index::pages::{place_pages, PageHit};
use plumb_index::{Hit, RankConfig, SearchOptions, SearchResults};
use tracing::{debug, error};

use super::api::NetworkResults;
use super::hit::{block_go_link, render_page};
use super::home::{bad_search_query, home_or_setup, request_origin, setup_response};
use super::html::{
    escape_html, html_response, http_url, page, results_form, safe_href, search_link,
    security_headers,
};
use super::network::network_search;
use super::params::{error_settings, flag, NetSetting, SearchParams, Settings};
use super::results::{render_error, render_results_with};
use super::{
    answers, control, history, mcp, panel, places, plugins, searxng, tune, weather, AppState,
};
use crate::about::About;
use crate::learn::{self, Block, Choice};
use crate::websearch::bang_url;

/// The places `query` asks for, searched on a blocking thread.
pub(crate) async fn run_places(
    state: &AppState,
    query: &str,
    home: Option<&str>,
    country: Option<&str>,
) -> Option<plumb_index::places::PlaceResults> {
    if plumb_core::Operators::parse(query).any() {
        return None;
    }
    plumb_index::places::parse_place_query(query)?;
    let backend = Arc::clone(&state.backend);
    let (query, home, country) = (
        query.to_string(),
        home.map(str::to_string),
        country.map(str::to_string),
    );
    tokio::task::spawn_blocking(move || backend.places(&query, home.as_deref(), country.as_deref()))
        .await
        .ok()
        .flatten()
}

/// The sites of the places in `found`, in the order the places are
/// listed: as the index has them, or else as the place says (its name and
/// kind), since small local sites are often not in the index. Looked up on
/// a blocking thread.
///
/// Places that give no website are looked up by name: the index often has
/// a small shop's site even when OpenStreetMap does not
/// ([`site_named_for`](crate::assembly::site_named_for)). They come after
/// the places that give one.
pub(crate) async fn place_sites(
    state: &AppState,
    found: &plumb_index::places::PlaceResults,
    options: &SearchOptions,
) -> Vec<Hit> {
    let backend = Arc::clone(&state.backend);
    let found = found.clone();
    let sites = tokio::task::spawn_blocking(move || {
        crate::assembly::linked_sites(
            &found,
            |domain| backend.site(domain),
            |name| backend.search(name, 5).unwrap_or_default(),
        )
    })
    .await
    .unwrap_or_default();
    // Safe search holds for local sites as for any result.
    let adult = |domain: &str| {
        state
            .node
            .as_ref()
            .is_some_and(|node| node.blocks_adult(domain))
    };
    sites
        .into_iter()
        .filter(|hit| options.safe == plumb_core::SafeSearch::Off || !adult(&hit.domain))
        .collect()
}

/// Where the places go on a results page that does not start them at
/// the top.
pub(crate) const PLACES_MARK: &str = "<!--places-->\n";

impl AppState {
    pub(crate) fn local_controls(&self, headers: &HeaderMap) -> bool {
        self.node.is_some()
            && request_origin(headers, &Uri::from_static("/"))
                .as_deref()
                .is_some_and(panel::local_origin)
            && !control::FORWARDED_HEADERS
                .iter()
                .any(|name| headers.contains_key(*name))
    }

    /// Which nodes this node's network searches ask, set by its owner.
    pub(crate) fn search_scope(&self) -> plumb_net::SearchScope {
        self.network()
            .map(|net| net.status().search_scope)
            .unwrap_or_default()
    }

    pub(crate) fn net_setting(&self, params: &SearchParams) -> NetSetting {
        match (self.network().is_some(), flag(&params.net)) {
            (false, _) => NetSetting::Unavailable,
            (true, false) => NetSetting::Off,
            (true, true) => NetSetting::On,
        }
    }
}

/// How the network part of a search went, for the line that says where
/// the results came from.
pub(crate) enum NetOutcome {
    /// The network was not asked.
    NotAsked,
    Answered(NetworkResults),
    Failed,
}

pub(crate) async fn search_page(
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
    let client = mcp::client_of(&extensions, &headers);
    if params
        .format
        .as_deref()
        .is_some_and(|format| format.eq_ignore_ascii_case("json"))
    {
        return searxng::search(state, headers, params).await;
    }
    // A bang leaves Plumb, even while it sets up.
    if let Some(url) = bang_url(&params.q) {
        return (security_headers(), Redirect::to(&url)).into_response();
    }
    if let Some(status) = state.setting_up() {
        // Reloading keeps the query, so the results show up once the index is ready.
        return setup_response(&status, now_unix(), state.local_controls(&headers));
    }
    match params.paper_search_query() {
        Ok(Some(query)) => return paper_response(&state, &query, &params, &headers, false).await,
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
    if query.is_empty() {
        return home_or_setup(&state, &params, &headers);
    }
    let mut visitor = history::Visitor::of(&state, &headers, params.history_prefs());
    let tuning = visitor
        .as_ref()
        .and(params.tune.as_deref())
        .and_then(tune::Tuning::parse);
    let editing = visitor.is_some() && (flag(&params.edit) || tuning.is_some());
    if let Some(visitor) = &mut visitor {
        visitor.editing = editing;
    }
    let mut settings = Settings {
        manage: state.local_controls(&headers),
        options: params.page_options(&state.settings, &headers),
        network: state.net_setting(&params),
        scope: state.search_scope(),
        private: state.private_search(),
        country_mode: params.country_mode(),
        debug: flag(&params.debug),
        request_fields: params.request_fields(),
        history: None,
        browser_about: (visitor.is_none() && state.node.is_some())
            .then(|| history::browser_about(&headers)),
        welcome: false,
        experiment: None,
    };
    let limit = params.limit();
    // The ranking experiments this search is in, if any; tuning and edit
    // mode are not searches as made.
    let lab = state.experiments().filter(|_| tuning.is_none() && !editing);
    let arms = lab
        .as_ref()
        .map(|lab| lab.assign(&query, visitor.as_ref().and_then(|v| v.profile())))
        .unwrap_or_default();
    let rank = match (&lab, &state.node) {
        (Some(lab), Some(node)) if !arms.is_empty() => Some(lab.rank(node.rank(), &arms)),
        _ => None,
    };
    let local = run_search_ranked(&state, &query, limit, &settings.options, rank).await;
    // "Near me", "weather" and "what time is it" go by the town the
    // searcher gave.
    let town = match &visitor {
        Some(visitor) => visitor.about.town(),
        None => settings.browser_about.as_ref().and_then(About::town),
    };
    let mut extras = match &local {
        Ok(_) => extras(&state, &query, &settings.options, town).await,
        Err(_) => answers::Extras::default(),
    };
    // Plugins run once the node knows what the search is about, so that
    // they can look it up by its identifiers.
    let about = match &local {
        Ok(results) => plugin_about(&state, &query, results, &extras, &settings.options).await,
        Err(_) => None,
    };
    let shown_to_plugins = match &local {
        Ok(results) if state.settings.plugins.any_annotate() => shown_results(results, limit),
        _ => Vec::new(),
    };
    (extras.plugins, extras.plugin_notes) = tokio::join!(
        state.plugin_results(
            &query,
            &settings.options,
            about.as_ref(),
            params.run.as_deref()
        ),
        state.settings.plugins.annotate(&query, &shown_to_plugins)
    );
    extras.plugin_token = plugins::button_token(&state, &extensions, &headers, &uri);
    extras.plugin_offers =
        state
            .settings
            .plugins
            .offers(&query, about.as_ref(), params.run.as_deref());
    let (local, network) = if settings.network == NetSetting::On {
        let network = network_search(&state, client, &query, limit, &settings.options).await;
        let network = match network {
            Ok(results) => NetOutcome::Answered(results),
            Err(_) => {
                // The page still has this node's own results.
                error!("search page network lookup failed");
                NetOutcome::Failed
            }
        };
        // Sites the searcher never wants to see stay out, wherever they
        // came from.
        let network = match (network, &visitor, &settings.browser_about) {
            (NetOutcome::Answered(mut found), Some(visitor), _) => {
                found.hits.retain(|result| {
                    !visitor.about.hides(&result.hit.domain)
                        && (editing || !visitor.hid(&query, &result.hit.domain))
                });
                NetOutcome::Answered(found)
            }
            (NetOutcome::Answered(mut found), None, Some(about)) => {
                found.hits.retain(|result| !about.hides(&result.hit.domain));
                NetOutcome::Answered(found)
            }
            (network, _, _) => network,
        };
        (local, network)
    } else {
        (local, NetOutcome::NotAsked)
    };
    let found_places = run_places(&state, &query, town, settings.options.country.as_deref()).await;
    let response = match local {
        Ok(mut results) => {
            route_sources(
                &state,
                &query,
                extras.answer.as_ref().map(|a| a.kind),
                settings.options.country.as_deref(),
                &mut results,
                limit,
            );
            let found_places = crate::assembly::places(
                &query,
                found_places,
                &results.hits,
                &settings.options,
                |domain| {
                    state
                        .node
                        .as_ref()
                        .is_some_and(|node| node.blocks_adult(domain))
                },
            )
            .map(|places| places.found);
            if let Some(found) = &found_places {
                let local = place_sites(&state, found, &settings.options).await;
                places::local_first(found, &mut results.hits, local, limit);
            }
            // Places shown: none found around a town is no list.
            let found_places = found_places.filter(|found| found.near_me || !found.hits.is_empty());
            if let Some(visitor) = &mut visitor {
                visitor.rank(
                    &query,
                    &mut results.hits,
                    settings.options.country.as_deref(),
                );
                // Tuning's searches are not the searcher's own.
                if tuning.is_none() {
                    visitor.note_search(&query);
                } else {
                    visitor.ensure_profile();
                }
                if editing {
                    visitor.note_judged(&query, &results.hits, settings.options.country.as_deref());
                }
                let mut view = visitor.view();
                view.tune_bar = tuning.map(|t| t.bar(&settings.options));
                view.home = settings.options.country.clone();
                view.verdicts = visitor
                    .history
                    .learned
                    .verdicts_for(&query)
                    .into_iter()
                    .collect();
                if editing {
                    let tune = tuning
                        .map(|t| format!("&tune={}", t.value()))
                        .unwrap_or_default();
                    view.edit = Some(history::Editing {
                        query: query.clone(),
                        back: format!(
                            "{}&edit=1{tune}",
                            search_link(
                                "/search",
                                &query,
                                &settings.options,
                                settings.network == NetSetting::On
                            )
                        ),
                    });
                }
                settings.history = Some(view);
            } else if let Some(about) = &settings.browser_about {
                about.apply(&mut results.hits);
            }
            let mut domains: Vec<String> =
                results.hits.iter().map(|hit| hit.domain.clone()).collect();
            if let NetOutcome::Answered(found) = &network {
                domains.extend(found.hits.iter().map(|result| result.hit.domain.clone()));
            }
            for placed in &results.pages {
                let domain = placed.hit.page.set_domain();
                if !domains.iter().any(|d| d == domain) {
                    domains.push(domain.to_string());
                }
            }
            if let Some(profile) = &extras.profile {
                domains.extend(plumb_core::registrable_domain(&profile.url));
            }
            if let Some(found) = &found_places {
                domains.extend(places::website_domains(found));
            }
            let icons = state.icons(domains).await;
            let recent = crate::assembly::recent(
                &query,
                state.recent(&query, &results, &settings.options),
                &settings.options,
                limit,
                |domain| {
                    state
                        .node
                        .as_ref()
                        .is_some_and(|node| node.blocks_adult(domain))
                },
            );
            // What the searcher's clicks taught this node: places they
            // seldom open for searches like this are folded, headlines they
            // read come unfolded.
            let places_context = found_places
                .as_ref()
                .filter(|found| found.center.is_some() && !found.hits.is_empty())
                .map(|found| learn::places_context(found.guessed));
            let news_context = recent
                .as_ref()
                .filter(|_| settings.options.recent != RecentNews::Off)
                .map(|recent| learn::news_context(recent.site.is_some()));
            let fold_places = match (&visitor, places_context) {
                (Some(visitor), Some(context)) => {
                    visitor.choice(Block::Places, &query, context) == Choice::Fold
                }
                _ => false,
            };
            if let (Some(visitor), Some(context), Some(view)) =
                (&visitor, news_context, settings.history.as_mut())
            {
                view.open_news = visitor.choice(Block::News, &query, context) == Choice::Open;
                view.news_verdict =
                    visitor
                        .history
                        .learned
                        .box_verdict(Block::News, &query, context);
            }
            settings.experiment = lab.as_ref().and_then(|lab| lab.note_search(&arms));
            let mut page = render_results_with(
                &query,
                &results,
                Some(&extras),
                &network,
                &settings,
                state.settings.web_search,
                limit,
                state.shares_popularity(),
                &icons,
                recent.as_ref(),
            );
            if let Some(found) = &found_places {
                // Above the sites.
                let learns = visitor.as_ref().is_some_and(history::Visitor::learns);
                let link = |href: &str| {
                    if learns {
                        block_go_link(&query, &settings.options, Block::Places, href)
                    } else {
                        href.to_owned()
                    }
                };
                let base_map = state.backend.base_map();
                let mut html = places::render_places(
                    found,
                    base_map.as_deref(),
                    // A node has the About page, kept by the browser if not
                    // with history.
                    state.node.is_some(),
                    settings.options.country.as_deref(),
                    &icons,
                    &link,
                );
                if let (Some(edit), Some(context), Some(visitor)) = (
                    settings.history.as_ref().and_then(|h| h.edit.as_ref()),
                    places_context,
                    &visitor,
                ) {
                    let now = visitor
                        .history
                        .learned
                        .box_verdict(Block::Places, &query, context);
                    let buttons = history::box_buttons(edit, Block::Places, context, now);
                    match html.rfind("</section>") {
                        Some(end) => html.insert_str(end, &buttons),
                        None => html.push_str(&buttons),
                    }
                }
                if fold_places {
                    let chosen = places_context.zip(visitor.as_ref()).is_some_and(|(c, v)| {
                        v.history
                            .learned
                            .box_verdict(Block::Places, &query, c)
                            .is_some()
                    });
                    html = places::fold_places(found, &html, chosen);
                }
                let at = match page.find(PLACES_MARK) {
                    Some(at) => Some(at + PLACES_MARK.len()),
                    None => page.find("<main>\n").map(|at| at + "<main>\n".len()),
                };
                if let Some(at) = at {
                    page.insert_str(at, &format!("<style>{}</style>\n{html}", places::STYLE));
                }
            }
            if let Some(visitor) = visitor.as_mut().filter(|_| tuning.is_none()) {
                let mut shown = Vec::new();
                let mut folded = Vec::new();
                if let Some(context) = places_context {
                    if fold_places {
                        folded.push((Block::Places, context));
                    } else {
                        shown.push((Block::Places, context));
                    }
                }
                if let Some(context) = news_context {
                    shown.push((Block::News, context));
                }
                let sites: Vec<String> = results.hits.iter().map(|h| h.domain.clone()).collect();
                visitor.note_page(&query, &shown, &folded, &sites);
            }
            html_response(StatusCode::OK, page)
        }
        Err(_) => {
            error!("search page local lookup failed");
            html_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                render_error(&query, &settings),
            )
        }
    };
    match &visitor {
        Some(visitor) => visitor.send_cookies(response),
        None => response,
    }
}

/// Paper lists preserve their relevance/date order through rendering and
/// report indexed coverage rather than asserting that research is absent.
pub(crate) async fn paper_response(
    state: &AppState,
    query: &str,
    params: &SearchParams,
    headers: &HeaderMap,
    json: bool,
) -> Response {
    let options = params.options(&state.settings, headers);
    let paper = match plumb_core::paper_query::PaperQuery::parse(query) {
        Ok(paper) => paper,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    };
    let backend = Arc::clone(&state.backend);
    let limit = params.limit();
    let found = tokio::task::spawn_blocking(move || backend.papers(&paper, limit, &options)).await;
    let hits = match found {
        Ok(Ok(hits)) => hits,
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "paper search failed" })),
            )
                .into_response()
        }
    };
    let coverage = state.backend.paper_coverage();
    let message = if hits.is_empty() {
        Some("No matching papers in the indexed corpus with the requested publication precision. This does not establish that no such research exists.")
    } else {
        None
    };
    if json {
        return (StatusCode::OK, security_headers(), Json(serde_json::json!({ "query": query, "kind": "paper", "results": [], "pages": hits, "coverage": coverage, "message": message }))).into_response();
    }
    let mut body = format!(
        "<div class=\"wrap wide\">{}<main><h1 class=\"visually-hidden\">Paper results for {}</h1>",
        results_form(&params.query(), &error_settings(state, params, headers)),
        escape_html(&params.query())
    );
    if let Some(message) = message {
        let _ = write!(body, "<p class=\"s\">{message}</p>");
    }
    if let Some(coverage) = coverage {
        let _ = write!(body, "<p class=\"s\">Indexed papers: {}; known publication year: {}; known publication day: {}.</p>", coverage.total, coverage.publication_year, coverage.publication_day);
    }
    body.push_str("<ol class=\"results\">");
    for hit in hits {
        render_page(&mut body, &hit, None);
    }
    body.push_str("</ol></main></div>");
    html_response(StatusCode::OK, page(query, &body))
}

/// The instant answer and the official profile asked for, for `query`
/// by resolving the subject's stable entity before inspecting its facts
/// or profiles. Displayed results cannot select a different namesake.
pub(crate) async fn extras(
    state: &AppState,
    query: &str,
    options: &SearchOptions,
    town: Option<&str>,
) -> answers::Extras {
    let country = options.country.as_deref();
    let answer = match instant_answer(state, query, town).await {
        Some(answer) => Some(answer),
        None => match answers::town_time(state, query, country).await {
            Some(answer) => Some(answer),
            None => weather::answer(state, query, town, country).await,
        },
    };
    let mut profile = None;
    for name in answers::profile_lookups(query) {
        let Ok(entities) = run_entities(state, &name, options).await else {
            continue;
        };
        let entities = answers::profile_candidates(query, &name, entities);
        let resolution = answers::resolve_entity(&name, &entities, None);
        if let answers::EntityResolution::Resolved(page) = &resolution {
            profile = answers::profile_answer_from_page(query, page);
            if profile.is_some() {
                break;
            }
        }
        if !answers::searches_songs(query, &resolution) {
            continue;
        }
        // MusicBrainz songs have no Wikidata ID.
        profile = run_search(state, &name, PROFILE_SEARCH_LIMIT, options)
            .await
            .ok()
            .and_then(|found| answers::songs_profile(query, &resolution, found.pages));
        if profile.is_some() {
            break;
        }
    }
    // A fact the query asks about something ("capital of australia"),
    // looked up by searching for that something.
    let answer = match (answer, plumb_core::facts::fact_asked(query)) {
        (None, Some(asked)) => run_entities(state, &asked.subject, options)
            .await
            .ok()
            .and_then(|found| {
                match answers::resolve_entity(&asked.subject, &found, Some(&asked.kinds)) {
                    answers::EntityResolution::Resolved(page) => {
                        answers::fact_answer_from_page(&asked, page, now_unix())
                    }
                    _ => None,
                }
            }),
        (answer, _) => answer,
    };
    // What something is ("what is a manatee"): the first sentence of the
    // article it names, or what the word means ("define anadromous").
    let answer = match (answer, answers::definition_asked(query)) {
        (None, Some(name)) => {
            let backend = Arc::clone(&state.backend);
            let word = {
                let name = name.clone();
                tokio::task::spawn_blocking(move || backend.definition(&name))
                    .await
                    .ok()
                    .flatten()
                    .and_then(|page| answers::word_answer(&page))
            };
            if word.is_some() && answers::asks_word(query) {
                word
            } else {
                run_search(state, &name, PROFILE_SEARCH_LIMIT, options)
                    .await
                    .ok()
                    .and_then(|found| answers::definition_answer(&found.pages))
                    .or(word)
            }
        }
        (answer, _) => answer,
    };
    answers::Extras {
        answer,
        profile,
        ..answers::Extras::default()
    }
}

/// Lists first the sites that serve `query` best, when it asks for a tool
/// or a quick fact ([`crate::sources`]); `answer` is the kind of the
/// instant answer shown for it. At most `limit` sites, or as many as there
/// were.
pub(crate) fn route_sources(
    state: &AppState,
    query: &str,
    answer: Option<plumb_answer::Kind>,
    country: Option<&str>,
    results: &mut SearchResults,
    limit: usize,
) {
    crate::assembly::route_sources(query, answer, country, results, limit, |domain| {
        state.backend.site(domain)
    });
}

/// Results asked for when looking up whose profile a query asks for.
pub(crate) const PROFILE_SEARCH_LIMIT: usize = 5;

/// Direct entity candidates, before displayed-result limits and placement.
const ENTITY_SEARCH_LIMIT: usize = 20;

async fn run_entities(
    state: &AppState,
    query: &str,
    options: &SearchOptions,
) -> Result<Vec<PageHit>> {
    let backend = Arc::clone(&state.backend);
    let query = query.to_string();
    let options = options.clone();
    tokio::task::spawn_blocking(move || backend.entities(&query, ENTITY_SEARCH_LIMIT, &options))
        .await
        .context("the entity lookup task failed")?
}

/// The instant answer to `query`, with currency rates when it needs them.
/// "What time is it" is the time in `town`, the searcher's own, when they
/// gave one.
async fn instant_answer(
    state: &AppState,
    query: &str,
    town: Option<&str>,
) -> Option<plumb_answer::Answer> {
    let rates = state.rates.for_query(query).await;
    let now = i64::try_from(now_unix()).unwrap_or(i64::MAX);
    plumb_answer::answer(query, now, rates.as_ref()).or_else(|| {
        let town =
            town.filter(|_| crate::sources::asks_the_time(&plumb_core::normalize_text(query)))?;
        plumb_answer::answer(&format!("time in {town}"), now, None)
    })
}

/// Runs a search on the blocking thread pool, since searching is CPU and
/// disk work. A panicking backend becomes an error, not a dropped connection.
/// Callers log only the failed operation: backend error messages can include
/// search terms, so neither those messages nor queries belong in diagnostics.
pub(crate) async fn run_search(
    state: &AppState,
    query: &str,
    limit: usize,
    options: &SearchOptions,
) -> Result<SearchResults> {
    run_search_ranked(state, query, limit, options, None).await
}

/// [`run_search`] with `rank` instead of the backend's own knobs, when
/// given.
async fn run_search_ranked(
    state: &AppState,
    query: &str,
    limit: usize,
    options: &SearchOptions,
    rank: Option<RankConfig>,
) -> Result<SearchResults> {
    if limit == 0 {
        return Ok(SearchResults::default());
    }
    let paper = plumb_core::paper_query::PaperQuery::parse(query)?;
    if paper.constrained {
        let backend = Arc::clone(&state.backend);
        let options = options.clone();
        let found = tokio::task::spawn_blocking(move || backend.papers(&paper, limit, &options))
            .await
            .context("the paper search task failed")??;
        return Ok(SearchResults {
            pages: found
                .into_iter()
                .map(|hit| plumb_index::pages::PlacedPage {
                    hit,
                    under: None,
                    at: 0,
                })
                .collect(),
            ..Default::default()
        });
    }
    let backend = Arc::clone(&state.backend);
    // "safeway near me": the places list is for "near me"; the sites are
    // for the rest ("me" names no site).
    let near_me = plumb_index::places::without_near_me(query);
    let local = near_me.is_some();
    let owned_query = near_me.unwrap_or_else(|| query.to_string());
    let owned_options = options.clone();
    let mut results = tokio::task::spawn_blocking(move || match rank {
        Some(rank) => backend.search_ranked(&owned_query, limit, &owned_options, &rank),
        None => backend.search_full(&owned_query, limit, &owned_options),
    })
    .await
    .context("the search task failed")??;
    // "handyman near me" asks for someone to call: a bare handyman.nl,
    // named by the word but saying nothing, goes after sites that say
    // what they are.
    if local {
        let (said, bare): (Vec<Hit>, Vec<Hit>) = std::mem::take(&mut results.hits)
            .into_iter()
            .partition(|hit| hit.title.is_some() || hit.official);
        results.hits = said.into_iter().chain(bare).collect();
    }
    debug!("local search completed: {} hits", results.hits.len());
    Ok(results)
}

/// The node's own results for its plugins to mark up: its sites, then
/// its articles, each with what it is about when the node knows.
pub(crate) fn shown_results(
    results: &SearchResults,
    limit: usize,
) -> Vec<plumb_plugin::ShownResult> {
    let about_of = |page: &plumb_index::pages::Page| {
        (page.item.is_some() || page.set.starts_with("wikipedia-"))
            .then(|| crate::plugins::about_page(page))
    };
    let mut shown = Vec::new();
    for hit in results.hits.iter().take(limit) {
        let Some(url) = safe_href(hit) else {
            continue;
        };
        // The article a site's result carries is about what the site is.
        let about = results
            .pages
            .iter()
            .map(|placed| &placed.hit.page)
            .filter(|page| page.site.as_deref() == Some(hit.domain.as_str()))
            .find_map(about_of);
        shown.push(plumb_plugin::ShownResult {
            id: shown.len() as u32,
            url,
            title: hit.title.clone().unwrap_or_else(|| hit.domain.clone()),
            site: hit.domain.clone(),
            about,
        });
    }
    for placed in &results.pages {
        let page = &placed.hit.page;
        let Some(url) = http_url(&page.url) else {
            continue;
        };
        shown.push(plumb_plugin::ShownResult {
            id: shown.len() as u32,
            url,
            title: page.title.clone(),
            site: page.set_domain().to_string(),
            about: about_of(page),
        });
    }
    shown
}

/// What the node takes `query`, whose results are `results`, to be
/// about, for its plugins: the article an info box would show, or else a
/// song or album among the top results that the query names.
pub(crate) fn search_about(
    query: &str,
    results: &SearchResults,
    extras: &answers::Extras,
) -> Option<plumb_plugin::About> {
    if let Some(profile) = &extras.profile {
        return Some(crate::plugins::about_page(&profile.page));
    }
    if plumb_core::Operators::parse(query).any() {
        return None;
    }
    let placed = place_pages(
        query,
        &results.hits,
        results.pages.iter().map(|p| p.hit.clone()).collect(),
    );
    answers::page_about(&results.hits, &placed)
        .or_else(|| music_about(&placed))
        .map(crate::plugins::about_page)
}

/// What plugins are told `query`, whose results are `results`, is about:
/// [`search_about`], unless that is only a namesake of the query (its
/// title says more, as "Creep (2014 film)" does) and the query is a
/// [`known_song`]'s title.
pub(crate) async fn plugin_about(
    state: &AppState,
    query: &str,
    results: &SearchResults,
    extras: &answers::Extras,
    options: &SearchOptions,
) -> Option<plumb_plugin::About> {
    let about = search_about(query, results, extras);
    if about
        .as_ref()
        .is_some_and(|about| same_title(&about.title, query))
    {
        return about;
    }
    known_song(state, query, results, options).await.or(about)
}

/// Whether `title` is `query`, but for case and spacing.
fn same_title(title: &str, query: &str) -> bool {
    let words =
        |text: &str| -> Vec<String> { text.split_whitespace().map(str::to_lowercase).collect() };
    words(title) == words(query)
}

/// The song or album a search of `query` alone, whose results are
/// `results`, is surely for, as plugins are told: "creep" is Radiohead's,
/// though no result lists it, and "Creep (2014 film)" is only a namesake.
/// Not when the query is the very name of a site or an article, which it
/// is then rather for: "yellow" is the colour, and "maps" a site. A site
/// found for a near spelling (creed.com for "creep") does not count.
pub(crate) async fn known_song(
    state: &AppState,
    query: &str,
    results: &SearchResults,
    options: &SearchOptions,
) -> Option<plumb_plugin::About> {
    if state.settings.plugins.is_empty() || plumb_core::Operators::parse(query).any() {
        return None;
    }
    let joined: String = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .concat();
    let names_site = |hit: &Hit| {
        hit.named
            && (hit.domain.split('.').next() == Some(joined.as_str())
                || hit
                    .title
                    .as_deref()
                    .is_some_and(|title| same_title(title, query)))
    };
    let named_else = results.hits.iter().any(names_site)
        || results.pages.iter().any(|placed| {
            let page = &placed.hit.page;
            placed.hit.named
                && page.set != plumb_index::pages::MUSIC_SET
                && same_title(&page.title, query)
                && !answers::is_disambiguation(&page.title, page.description.as_deref())
        });
    if named_else {
        return None;
    }
    let backend = Arc::clone(&state.backend);
    let (query, options) = (query.to_string(), options.clone());
    tokio::task::spawn_blocking(move || backend.known_song(&query, &options))
        .await
        .ok()
        .flatten()
        .map(|page| crate::plugins::about_page(&page))
}

/// How near the top a song or album must be listed for a search to be
/// taken to be for it: among the first three results.
pub(crate) const MUSIC_ABOUT_AT: usize = 2;

/// The song or album of the music set that the query names, by its title
/// ("creep") or with its artist ("radiohead creep"), listed among the top
/// results on its own: what a search for a song is about.
pub(crate) fn music_about(
    placed: &[plumb_index::pages::PlacedPage],
) -> Option<&plumb_index::pages::Page> {
    placed
        .iter()
        .filter(|placed| {
            placed.hit.page.set == plumb_index::pages::MUSIC_SET
                && (placed.hit.named || placed.hit.whole)
                && placed.under.is_none()
                && placed.at <= MUSIC_ABOUT_AT
        })
        .min_by_key(|placed| placed.at)
        .map(|placed| &placed.hit.page)
}
