//! The home page, the setup page a starting node shows, error pages and
//! the OpenSearch description.

use std::collections::HashSet;
use std::fmt::Write as _;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use plumb_core::now_unix;
use url::Url;

use super::html::{
    app_bar, escape_html, group_thousands, html_response, page, page_with_head, results_form,
    security_headers, settings_form, time_ago, time_until, FOOTER,
};
use super::params::{error_settings, flag, SearchParams, Settings};
use super::{history, welcome, AppState, OPENSEARCH_TYPE};
use crate::node::{Phase, Status, Step};

/// Seconds between two reloads of the setup page, and the `Retry-After` of
/// a search asked for before the index is ready.
pub(crate) const SETUP_RELOAD_SECONDS: u32 = 5;

pub(crate) async fn not_found(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let settings = error_settings(&state, &SearchParams::default(), &headers);
    html_response(
        StatusCode::NOT_FOUND,
        message_page(
            "Page not found",
            "There is no page at this address. Search below or return home.",
            "",
            &settings,
        ),
    )
}

pub(crate) fn message_page(title: &str, message: &str, query: &str, settings: &Settings) -> String {
    page(
        title,
        &format!(
            "<div class=\"wrap wide\">{}<main><h1>{}</h1><p>{}</p></main></div>",
            results_form(query, settings),
            escape_html(title),
            escape_html(message)
        ),
    )
}

pub(crate) fn bad_search_query(state: &AppState, headers: &HeaderMap, uri: &Uri) -> Response {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    let mut seen = HashSet::new();
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()) {
        if key != "limit" && seen.insert(key.to_string()) {
            query.append_pair(&key, &value);
        }
    }
    let recovered: Uri = format!("/search?{}", query.finish())
        .parse()
        .expect("encoded URI");
    let params = Query::<SearchParams>::try_from_uri(&recovered)
        .map(|q| q.0)
        .unwrap_or_default();
    html_response(
        StatusCode::BAD_REQUEST,
        message_page(
            "Check your search",
            "Use a whole number for the result limit and try again.",
            &params.query(),
            &error_settings(state, &params, headers),
        ),
    )
}

pub(crate) async fn home(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    home_or_setup(&state, &params, &headers)
}

/// The home page, or the setup page while a node is still setting up. Its
/// settings gear shows what `params` and `headers` ask for.
pub(crate) fn home_or_setup(
    state: &AppState,
    params: &SearchParams,
    headers: &HeaderMap,
) -> Response {
    let status = state.node.as_ref().map(|node| node.status());
    let now = now_unix();
    match &status {
        Some(status) if status.phase != Phase::Ready => {
            setup_response(status, now, state.local_controls(headers))
        }
        _ => {
            let visitor = history::Visitor::of(state, headers, params.history_prefs());
            let browser_about = (visitor.is_none() && state.node.is_some())
                .then(|| history::browser_about(headers));
            // A new searcher is asked about themselves, until they answer
            // or say no thanks.
            let told = match (&visitor, &browser_about) {
                (Some(visitor), _) => !visitor.about.is_empty(),
                (None, Some(about)) => !about.is_empty(),
                (None, None) => false,
            };
            let settings = Settings {
                manage: state.local_controls(headers),
                options: params.page_options(&state.settings, headers),
                network: state.net_setting(params),
                scope: state.search_scope(),
                private: state.private_search(),
                country_mode: params.country_mode(),
                debug: flag(&params.debug),
                request_fields: params.request_fields(),
                history: visitor.as_ref().map(history::Visitor::view),
                browser_about,
                experiment: None,
                welcome: state.node.is_some() && !told && !history::welcomed(headers),
            };
            let response = html_response(
                StatusCode::OK,
                render_home(state.backend.num_docs(), status.as_ref(), now, &settings),
            );
            match &visitor {
                Some(visitor) => visitor.send_cookies(response),
                None => response,
            }
        }
    }
}

pub(crate) fn setup_response(status: &Status, now: u64, controls: bool) -> Response {
    (
        StatusCode::OK,
        security_headers(),
        [(header::CACHE_CONTROL, "no-store")],
        Html(render_setup_with(status, now, controls)),
    )
        .into_response()
}

/// `GET /opensearch.xml`: the OpenSearch description browsers add Plumb
/// from. Its search URL is on the host the request was sent to, so it is
/// right however the server is reached: `127.0.0.1:8080`, the desktop app's
/// port or a server's name on the LAN.
pub(crate) async fn opensearch(headers: HeaderMap, uri: Uri) -> Response {
    let Some(origin) = request_origin(&headers, &uri) else {
        return (
            StatusCode::BAD_REQUEST,
            security_headers(),
            "The Host header is missing or is not a host name and port.\n",
        )
            .into_response();
    };
    (
        StatusCode::OK,
        security_headers(),
        [(header::CONTENT_TYPE, OPENSEARCH_TYPE)],
        render_opensearch(&origin),
    )
        .into_response()
}

/// The origin a request was sent to, such as `http://127.0.0.1:7586`: the
/// `Host` header (or, without one, the request's authority, as in HTTP/2),
/// with `https` when a proxy in front says so in `X-Forwarded-Proto`. `None`
/// unless the host is a plain host name or address with an optional port.
pub(crate) fn request_origin(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    let host = match headers.get(header::HOST) {
        Some(host) => host.to_str().ok()?,
        None => uri.authority()?.as_str(),
    };
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|proto| proto.to_str().ok())
        .and_then(|proto| proto.split(',').next())
        .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"));
    let scheme = if https { "https" } else { "http" };
    let url = Url::parse(&format!("{scheme}://{host}/")).ok()?;
    let plain = url.username().is_empty()
        && url.password().is_none()
        && url.host().is_some()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none();
    plain.then(|| url.origin().ascii_serialization())
}

/// The OpenSearch 1.1 description of the search engine at `origin`.
pub(crate) fn render_opensearch(origin: &str) -> String {
    let icon = &*ICON_PNG_BASE64;
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <OpenSearchDescription xmlns=\"http://a9.com/-/spec/opensearch/1.1/\">\n\
         <ShortName>Plumb Search</ShortName>\n\
         <Description>Plumb Search</Description>\n\
         <InputEncoding>UTF-8</InputEncoding>\n\
         <Image width=\"32\" height=\"32\" type=\"image/png\">\
         data:image/png;base64,{icon}</Image>\n\
         <Url type=\"text/html\" method=\"get\" template=\"{}/search?q={{searchTerms}}\"/>\n\
         </OpenSearchDescription>\n",
        escape_html(origin)
    )
}

/// The desktop icon, embedded in OpenSearch and page chrome without a request.
pub(crate) static ICON_PNG_BASE64: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .encode(include_bytes!("../../../plumb-desktop/icons/32x32.png"))
});

/// The home page, with the count of items searched: the `docs` of a bare
/// index, or all a node holds by its `status` ([`Status::items`]), and what
/// the node is doing.
pub(crate) fn render_home(
    docs: u64,
    status: Option<&Status>,
    now: u64,
    settings: &Settings,
) -> String {
    let items = status.map_or(docs, Status::items);
    let note = status
        .and_then(|status| node_note(status, now))
        .map(|note| format!(" &middot; {}", escape_html(&note)))
        .unwrap_or_default();
    let wikidata = status
        .and_then(|status| wikidata_note(status, now))
        .map(|note| format!("\n<p class=\"s\">{}</p>", escape_html(&note)))
        .unwrap_or_default();
    let recent = settings
        .history
        .as_ref()
        .map(|history| history.recent_html(&settings.options))
        .unwrap_or_default();
    let welcome = if settings.welcome {
        welcome::invite_html()
    } else {
        ""
    };
    let body = format!(
        "<div class=\"wrap search-home\">{}<main class=\"home\">\n\
         <h1>Plumb Search</h1>\n\
         <p class=\"home-intro\">Find sites, articles, places and more.</p>\
         {}{welcome}{recent}\n<p class=\"index-status\"><span class=\"status-dot\" aria-hidden=\"true\"></span>{} items indexed{note}</p>{wikidata}\n\
         <details class=\"search-help\"><summary>Search tips</summary>\
         <p>Try a site name, a topic, a calculation, or a place. Use \
         <code>site:</code> to search one domain and quotes for exact words.</p>\
         <p>Add !g, !ddg or !b to search Google, DuckDuckGo or Bing.</p></details>\
         {FOOTER}\n</main></div>",
        app_bar("search", settings.manage),
        settings_form("", true, settings),
        group_thousands(items)
    );
    page("Plumb Search", &body)
}

/// What a ready node is up to, in a few words for the home page.
pub(crate) fn node_note(status: &Status, now: u64) -> Option<String> {
    let note = match (status.step, &status.progress) {
        (Step::Crawling, Some(p)) => format!(
            "crawling homepages, {} of {}",
            group_thousands(p.done),
            group_thousands(p.total)
        ),
        (Step::Indexing, _) => "rebuilding the index".to_string(),
        _ if status.last_error.is_some() => {
            "the last update failed and will be tried again (see /api/status)".to_string()
        }
        _ => format!("updated {}", time_ago(status.last_refresh?, now)),
    };
    Some(note)
}

/// Says that the index lacks Wikidata's official websites, while it does.
fn wikidata_note(status: &Status, now: u64) -> Option<String> {
    if !status.wikidata_missing {
        return None;
    }
    let Some(err) = &status.wikidata_error else {
        // Right after the quick first setup, Wikidata is next.
        if status.phase == Phase::SettingUp {
            return None;
        }
        return Some(
            "Plumb is still downloading Wikidata's list of official websites and more \
             rankings. Search works now, and results get better once those are in."
                .to_string(),
        );
    };
    let mut note = "Wikidata's list of official websites could not be downloaded yet, so the \
                    index does without it for now: official sites get no boost over look-alikes."
        .to_string();
    if let Some(retry_at) = err.retry_at {
        let _ = write!(note, " Plumb will try again {}.", time_until(retry_at, now));
    }
    Some(note)
}

/// The page shown while a node sets up: the step, its progress and the last
/// error. It reloads itself, since the page allows no script.
#[cfg(test)]
pub(crate) fn render_setup(status: &Status, now: u64) -> String {
    render_setup_with(status, now, false)
}

fn render_setup_with(status: &Status, now: u64, controls: bool) -> String {
    let bar = app_bar("search", controls);
    let mut body = format!(
        "<div class=\"wrap\">{bar}<main class=\"home setup\">\n<h1>Plumb Search</h1>\n\
         <p class=\"tag\">Setting up your search engine</p>\n",
    );
    let _ = writeln!(
        body,
        "<p class=\"step\">{}</p>",
        escape_html(&status.detail)
    );
    if let Some(progress) = &status.progress {
        let max = progress.total.max(progress.done).max(1);
        let _ = writeln!(
            body,
            "<progress aria-label=\"Setup progress\" value=\"{}\" max=\"{max}\"></progress>\n<p>{} of {} {}</p>",
            progress.done,
            group_thousands(progress.done),
            group_thousands(progress.total),
            escape_html(&progress.unit)
        );
    }
    if let Some(err) = &status.last_error {
        let _ = write!(
            body,
            "<div class=\"err\" role=\"alert\">\n<p><strong>Something went wrong</strong> \
             {}:</p>\n<p class=\"msg\">{}</p>\n",
            time_ago(err.at, now),
            escape_html(&err.message)
        );
        if let Some(retry_at) = err.retry_at {
            let _ = writeln!(
                body,
                "<p>Plumb will try again {}.</p>",
                time_until(retry_at, now)
            );
        }
        body.push_str("</div>\n");
    }
    if let Some(note) = wikidata_note(status, now) {
        let _ = writeln!(body, "<p>{}</p>", escape_html(&note));
        if let Some(err) = &status.wikidata_error {
            let _ = writeln!(body, "<p class=\"msg\">{}</p>", escape_html(&err.message));
        }
    }
    let _ = write!(
        body,
        "<p class=\"s\">On its first start, Plumb downloads a public list of popular websites \
         and builds a first search index from it, which takes a minute or two. It adds more \
         lists while you search. This page reloads every {SETUP_RELOAD_SECONDS} seconds.</p>\n</main></div>"
    );
    let head = format!("<meta http-equiv=\"refresh\" content=\"{SETUP_RELOAD_SECONDS}\">\n");
    page_with_head("Setting up - Plumb Search", &head, &body)
}
