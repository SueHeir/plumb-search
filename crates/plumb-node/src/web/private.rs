//! Private search, for a node that serves it
//! ([`crate::node::NodeConfig::private_search`]):
//!
//! - `GET /private` is a search page whose searches never reach the node.
//!   Its script, `crates/plumb-private` compiled to WebAssembly, works out
//!   the query's buckets in the browser, fetches them, and ranks the sites
//!   itself. The query stays in the page's fragment (`/private#q=...`),
//!   which browsers do not send. The search box has no `name`, so without
//!   the script a search sends nothing either: the page then says private
//!   search needs it.
//! - `GET /private/{version}/{file}` serves that script, under the hash of
//!   the build, so it can be cached for good.
//! - `GET /api/buckets` names the bucket table being served, and
//!   `GET /api/buckets/{table}/{bucket}` returns one bucket's sites as a
//!   JSON list of trimmed records ([`plumb_core::keys::slim_record`]).
//!   A table's buckets never change, so they are cached for good too; once
//!   a newer index replaces it, its address answers 404 and the page asks
//!   for the new table.
//!
//! Each search fetches [`plumb_core::keys::BUCKETS_PER_SEARCH`] buckets, its
//! own padded with random ones, so the node learns a few bucket numbers,
//! each shared by a few hundred keys, and never the words searched for.

use std::sync::LazyLock;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use plumb_core::keys::{slim_record, BUCKETS};
use plumb_core::SiteRecord;
use tracing::warn;

use super::{escape_html, page_with_head, AppState, SearchParams};

/// The WebAssembly module and its JavaScript loader, from `wasm-bindgen`.
/// Empty when the node was built without them (see `build.rs`).
const MODULE_JS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plumb_private.js"));
const MODULE_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plumb_private_bg.wasm"));
/// Starts the module; a file, because the pages allow no inline script.
const BOOT_JS: &str = "import init from \"./plumb_private.js\";\ninit();\n";

/// The private page may run this site's scripts and WebAssembly and fetch
/// from this site, nothing else.
const PRIVATE_CSP: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; \
     connect-src 'self'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; \
     frame-ancestors 'none'";

/// Caching for files that never change at their address.
const FOREVER: &str = "public, max-age=31536000, immutable";

/// Names this build's script files in their addresses.
static VERSION: LazyLock<String> = LazyLock::new(|| {
    let hash = plumb_net::hash::Hash::of(&[MODULE_JS, MODULE_WASM, BOOT_JS.as_bytes()]);
    hash.to_hex()[..16].to_string()
});

/// Whether this build holds the private search script.
pub(crate) fn in_build() -> bool {
    !MODULE_JS.is_empty() && !MODULE_WASM.is_empty()
}

impl AppState {
    /// Whether private search can be offered: the node serves it, its
    /// index has buckets, and this build holds the script.
    pub(super) fn private_search(&self) -> bool {
        in_build()
            && self
                .node
                .as_ref()
                .is_some_and(|node| node.bucket_table().is_some())
    }
}

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/private", get(private_page))
        .route("/private/{version}/{file}", get(script))
        .route("/api/buckets", get(table_info))
        .route("/api/buckets/{table}/{bucket}", get(bucket))
}

async fn private_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<SearchParams>,
) -> Response {
    // A query in the address (`/private?q=`) is not read: private searches
    // come from the fragment.
    let auto_country = SearchParams::default()
        .options(&state.settings.home, &headers)
        .country;
    let country_mode = params.country.clone().unwrap_or_else(|| "auto".into());
    let options = SearchParams {
        q: String::new(),
        ..params
    }
    .options(&state.settings.home, &headers);
    let available = state.private_search();
    let status = if available {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        [
            (header::CONTENT_SECURITY_POLICY, PRIVATE_CSP),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        axum::response::Html(render_private_options(
            available,
            auto_country.as_deref(),
            options.only_country,
            &country_mode,
        )),
    )
        .into_response()
}

/// The private search page. `available` says whether its script can run
/// here; `country` is the home country the ranking favors.
#[cfg(test)]
fn render_private(available: bool, country: Option<&str>) -> String {
    render_private_options(available, country, false, country.unwrap_or("auto"))
}

fn render_private_options(
    available: bool,
    country: Option<&str>,
    only: bool,
    country_mode: &str,
) -> String {
    let head = if available {
        format!(
            "<script type=\"module\" src=\"/private/{}/boot.js\"></script>\n",
            *VERSION
        )
    } else {
        String::new()
    };
    let note = if available {
        "<noscript><p class=\"err\">Private search runs in your browser, so it needs \
         JavaScript and WebAssembly. <a href=\"/\">Search normally</a> instead.</p></noscript>"
    } else {
        "<p class=\"err\">This site does not offer private search right now. \
         <a href=\"/\">Search normally</a> instead.</p>"
    };
    let mut choices = format!(
        "<option value=\"auto\"{}>Automatic</option><option value=\"any\"{}>Any country</option>",
        if country_mode == "auto" {
            " selected"
        } else {
            ""
        },
        if country_mode == "any" {
            " selected"
        } else {
            ""
        }
    );
    for (code, name) in crate::country::COUNTRY_CHOICES {
        choices.push_str(&format!(
            "<option value=\"{code}\"{}>{name}</option>",
            if country_mode.eq_ignore_ascii_case(code) {
                " selected"
            } else {
                ""
            }
        ));
    }
    let controls = format!("<details class=\"gear\"><summary aria-label=\"Settings\">&#9881;</summary><div class=\"panel\"><label>Country <select id=\"pq-country\">{choices}</select></label><label><input id=\"pq-only\" type=\"checkbox\"{}> Only this country</label><button type=\"submit\">Apply</button></div></details>", if only { " checked" } else { "" });
    let disabled = if available { "" } else { " disabled" };
    let body = format!(
        "<main class=\"wrap\" id=\"pq\" data-country=\"{}\">\n\
         <header><a id=\"pq-home\" class=\"logo\" href=\"/\">Plumb Search</a>\
         <form id=\"pq-form\" action=\"/private\" method=\"get\" role=\"search\">\
         <fieldset style=\"display:contents;border:0;padding:0;margin:0\"{disabled}><input type=\"search\" id=\"pq-q\" placeholder=\"Site name\" \
         aria-label=\"Search privately\" autocomplete=\"off\">{controls}\
         <button type=\"submit\" id=\"pq-go\" disabled>Search</button></fieldset></form></header>\n\
         <h1 class=\"visually-hidden\">Private search</h1><p class=\"src\"><strong>Private search.</strong> Your browser looks up the results \
         itself: it fetches a few groups of sites from this server, padded with random ones, \
         and picks the matches. This server never sees what you search for. \
         <a id=\"pq-normal\" href=\"/\">Normal search</a></p>\n{note}\n\
         <p class=\"s\" id=\"pq-status\" role=\"status\"></p>\n<ol id=\"pq-results\"></ol>\n\
         </main>",
        escape_html(country.unwrap_or(""))
    );
    let mut params = url::form_urlencoded::Serializer::new(String::new());
    params.append_pair("country", country_mode);
    if only {
        params.append_pair("only", "1");
    }
    let normal = super::escape_html(&format!("/?{}", params.finish()));
    let body = body.replace("href=\"/\"", &format!("href=\"{normal}\""));
    page_with_head("Private search - Plumb Search", &head, &body)
}

/// `GET /private/{version}/{file}`: the script, for this build's version only.
async fn script(Path((version, file)): Path<(String, String)>) -> Response {
    if !in_build() || version != *VERSION {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (body, kind): (&'static [u8], &str) = match file.as_str() {
        "boot.js" => (BOOT_JS.as_bytes(), "text/javascript; charset=utf-8"),
        "plumb_private.js" => (MODULE_JS, "text/javascript; charset=utf-8"),
        "plumb_private_bg.wasm" => (MODULE_WASM, "application/wasm"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, FOREVER),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

/// `GET /api/buckets`: the table to fetch buckets from.
async fn table_info(State(state): State<AppState>) -> Response {
    let table = state.node.as_ref().and_then(|node| node.bucket_table());
    let Some(table) = table else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CACHE_CONTROL, "no-store")],
            "Private search is not available here.\n",
        )
            .into_response();
    };
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(serde_json::json!({ "table": table, "buckets": BUCKETS })),
    )
        .into_response()
}

/// `GET /api/buckets/{table}/{bucket}`: one bucket's sites.
async fn bucket(
    State(state): State<AppState>,
    Path((table, bucket)): Path<(String, u32)>,
) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if bucket >= BUCKETS {
        return StatusCode::NOT_FOUND.into_response();
    }
    let read = tokio::task::spawn_blocking(move || {
        node.bucket(&table, bucket)
            .map(|records| records.map(|records| slim_bucket(&records)))
    })
    .await;
    match read {
        Ok(Some(Ok(json))) => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static(FOREVER)),
            ],
            json,
        )
            .into_response(),
        // Not the table served (any more): the page asks for the new one.
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Ok(Some(Err(err))) => {
            warn!("cannot read bucket {bucket}: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(err) => {
            warn!("reading bucket {bucket} failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// A bucket's records, trimmed to what a browser needs, as one JSON list.
/// Records that do not read are left out.
fn slim_bucket(records: &[String]) -> String {
    let slim: Vec<SiteRecord> = records
        .iter()
        .filter_map(|json| serde_json::from_str::<SiteRecord>(json).ok())
        .map(slim_record)
        .collect();
    serde_json::to_string(&slim).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_never_sends_the_query() {
        let page = render_private(true, Some("DE"));
        // The search box has no name, so the form sends no query.
        assert!(page.contains("id=\"pq-q\""));
        assert!(!page.contains("name=\"q\""), "{page}");
        assert!(page.contains("data-country=\"DE\""));
        assert!(page.contains("<button type=\"submit\" id=\"pq-go\" disabled>"));
        assert!(page.contains(&format!("/private/{}/boot.js", *VERSION)));
        let off = render_private(false, None);
        assert!(!off.contains("<script"), "{off}");
        assert!(off.contains("does not offer private search"));
    }

    #[test]
    fn buckets_are_trimmed() {
        let mut record = SiteRecord::new("usbank.com");
        record.crawl_failures = 2;
        let json = slim_bucket(&[
            serde_json::to_string(&record).unwrap(),
            "not json".to_string(),
        ]);
        let back: Vec<SiteRecord> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].crawl_failures, 0);
    }
}
