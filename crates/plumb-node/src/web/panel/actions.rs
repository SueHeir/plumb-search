//! The panel's forms: saving settings, features, plugins and remote control,
//! pausing, retrying and restarting, and backups and restores.

use std::net::SocketAddr;

use axum::extract::{FromRequest, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use plumb_core::now_unix;
use serde::Deserialize;
use tracing::warn;

use super::forms::{features_error, settings_error};
use super::guard::{forbidden, panel_error, refusal, PANEL_REFERRER_POLICY};
use super::view::LAYOUT_STYLE;
use super::{panel_page, PANEL_STYLE};
use crate::node::backup::{self, Backup};
use crate::node::control;
use crate::node::features::{AnswerLimit, FeatureSettings};
use crate::node::{CrawlHours, NodeSettings, Retry, Workload};
use crate::pages::{PageSetSize, SETS};
use crate::web::{escape_html, page_with_head, request_origin, AppState};

/// The settings form as posted. A checkbox that is not ticked is not sent.
#[derive(Debug, Default, Deserialize)]
pub(in crate::web) struct SettingsForm {
    #[serde(default)]
    pub(in crate::web) background_updates: Option<String>,
    /// Megabytes; empty for no limit.
    #[serde(default)]
    pub(in crate::web) download_limit_mb_per_day: String,
    /// Megabytes; empty for no limit.
    #[serde(default)]
    pub(in crate::web) storage_limit_mb: String,
    /// A [`Workload`] name; empty keeps the limits as typed.
    #[serde(default)]
    pub(in crate::web) workload: String,
    /// Ticked to crawl only between `crawl_from` and `crawl_to`.
    #[serde(default)]
    pub(in crate::web) crawl_hours: Option<String>,
    #[serde(default)]
    pub(in crate::web) crawl_from: String,
    #[serde(default)]
    pub(in crate::web) crawl_to: String,
    /// Ticked to fill free space from the network.
    #[serde(default)]
    pub(in crate::web) fill_from_network: Option<String>,
    /// Sent by forms that show the fill checkbox; without it the setting
    /// is kept as it is.
    #[serde(default)]
    pub(in crate::web) fill_shown: Option<String>,
    /// Focus topics, one per line or comma.
    #[serde(default)]
    pub(in crate::web) focus_topics: String,
    /// Sent by forms that show the focus topics; without it they are kept.
    #[serde(default)]
    pub(in crate::web) focus_shown: Option<String>,
    /// Sent by forms that show the page sets; without it they are kept.
    #[serde(default)]
    pub(in crate::web) page_sets_shown: Option<String>,
    /// How much the site grades count, in percent; without it the setting
    /// is kept.
    #[serde(default)]
    pub(in crate::web) site_grades_percent: Option<String>,
    /// `page_set.<id>`: a [`PageSetSize`] for each page set shown.
    #[serde(flatten)]
    pub(in crate::web) other: std::collections::HashMap<String, String>,
}

/// The retry form: `work`, `wikidata` or `meaning`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(in crate::web) struct RetryForm {
    what: String,
}

impl RetryForm {
    pub(in crate::web) fn what(&self) -> Option<Retry> {
        match self.what.as_str() {
            "work" => Some(Retry::Work),
            "wikidata" => Some(Retry::Wikidata),
            "meaning" => Some(Retry::Meaning),
            _ => None,
        }
    }
}

/// The form restoring a saved backup.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(in crate::web) struct RestoreForm {
    name: String,
    confirm: Option<String>,
}

/// The pause form: `hour`, `tomorrow` or `resume`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(in crate::web) struct PauseForm {
    until: String,
}

/// `settings` paused or resumed as the pause form asks, at `now`; `None`
/// when the form makes no sense.
pub(in crate::web) fn paused(
    settings: &NodeSettings,
    form: &PauseForm,
    now: u64,
) -> Option<NodeSettings> {
    let until = match form.until.as_str() {
        "hour" => Some(now + 3600),
        "tomorrow" => Some(next_morning(now)),
        "resume" => None,
        _ => return None,
    };
    Some(NodeSettings {
        paused_until: until,
        ..settings.clone()
    })
}

/// 06:00 tomorrow on the node's clock, roughly: the seconds left of today
/// and six hours.
fn next_morning(now: u64) -> u64 {
    let (hour, minute, second) = crate::node::schedule::local_time();
    let today = u64::from(hour) * 3600 + u64::from(minute) * 60 + u64::from(second);
    now + (24 * 3600 - today) + 6 * 3600
}

/// A limit typed into the form, in megabytes: empty or 0 for none.
pub(super) fn parse_limit(text: &str) -> Option<u64> {
    let text = text.trim().replace([',', '_', ' '], "");
    if text.is_empty() {
        return Some(0);
    }
    text.parse().ok()
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub(in crate::web) struct FeaturesForm {
    pub(in crate::web) section: String,
    pub(in crate::web) network: Option<String>,
    pub(in crate::web) search_by_meaning: Option<String>,
    pub(in crate::web) share_popularity: Option<String>,
    pub(in crate::web) search_history: Option<String>,
    /// Find nodes through the Plumb network's own bootstrap nodes.
    pub(in crate::web) plumb_bootstrap: Option<String>,
    /// Further bootstrap nodes, one per line.
    pub(in crate::web) bootstrap: String,
    /// Set by the form that shows the trust choices, so that a form without
    /// them leaves trust as it was.
    pub(in crate::web) trust_shown: Option<String>,
    pub(in crate::web) default_trust: Option<String>,
    pub(in crate::web) trusted: String,
    /// Which nodes network searches ask, from the network section.
    pub(in crate::web) search_from: Option<String>,
    /// Set by the form that shows the credit choices.
    pub(in crate::web) credits_shown: Option<String>,
    /// Free answers a day for other nodes; blank for no limit.
    pub(in crate::web) answer_per_day: String,
    pub(in crate::web) spend_credits: Option<String>,
}

pub(in crate::web) async fn save_features(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(Form(form)) = Form::<FeaturesForm>::from_request(request, &state).await else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "The feature settings could not be read.",
        );
    };
    let mut features = match node.saved_features() {
        Ok(features) => features,
        Err(err) => return panel_error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    let section = match apply_features_form(&form, &mut features, "/app") {
        Ok(section) => section,
        Err(response) => return response,
    };
    if let Err(err) = node.change_features(features) {
        return features_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &form,
            "/app",
            &format!("Could not save feature settings: {err}"),
        );
    }
    Redirect::to(&format!("/app?section={section}&saved=features")).into_response()
}

/// Puts the choices of the features form, from section "search" or
/// "network", into `features`, and returns the section; the error page when
/// they cannot work together.
// A response is big, but these run once per request.
#[allow(clippy::result_large_err)]
pub(in crate::web) fn apply_features_form(
    form: &FeaturesForm,
    features: &mut FeatureSettings,
    base: &str,
) -> Result<&'static str, Response> {
    let section = if form.section == "search" {
        "search"
    } else {
        "network"
    };
    if section == "search" {
        features.search_by_meaning = form.search_by_meaning.is_some();
        features.search_history = Some(form.search_history.is_some());
    } else {
        features.network = form.network.is_some();
        features.share_popularity = form.share_popularity.is_some();
        features.set_bootstrap(
            form.plumb_bootstrap.is_some(),
            form.bootstrap.split_whitespace(),
        );
        if form.trust_shown.is_some() {
            features.no_default_trust = form.default_trust.is_none();
            features.trusted = form.trusted.split_whitespace().map(str::to_owned).collect();
        }
        if let Some(scope) = &form.search_from {
            match scope.parse() {
                Ok(scope) => features.search_from = Some(scope),
                Err(err) => {
                    return Err(features_error(
                        StatusCode::BAD_REQUEST,
                        form,
                        base,
                        &err.to_string(),
                    ))
                }
            }
        }
        if form.credits_shown.is_some() {
            let typed: String = form
                .answer_per_day
                .chars()
                .filter(|c| !matches!(c, ',' | '_' | ' ' | '\u{a0}'))
                .collect();
            let per_day = if typed.is_empty() {
                None
            } else {
                match typed.parse::<u64>() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        return Err(features_error(
                            StatusCode::BAD_REQUEST,
                            form,
                            base,
                            "The daily limit on searches answered for other nodes must be a \
                             whole number, or blank for no limit. Nothing was changed.",
                        ))
                    }
                }
            };
            features.answer_limit = Some(AnswerLimit { per_day });
            features.spend_credits = Some(form.spend_credits.is_some());
        }
    }
    if let Err(err) = features.check() {
        return Err(features_error(
            StatusCode::BAD_REQUEST,
            form,
            base,
            &err.to_string(),
        ));
    }
    Ok(section)
}

pub(in crate::web) async fn save_settings(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(Form(form)) = Form::<SettingsForm>::from_request(request, &state).await else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "The settings form could not be read.",
        );
    };
    let current = node.settings().unwrap_or_default();
    let settings = match settings_from_form(&form, &current, "/app") {
        Ok(settings) => settings,
        Err(response) => return response,
    };
    if let Err(err) = node.change_settings(settings) {
        warn!("could not save the settings: {err:#}");
        return settings_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &form,
            "/app",
            &format!("Could not save the settings: {err:#}"),
        );
    }
    Redirect::to("/app?section=resources&saved=settings").into_response()
}

/// The settings the settings form asks for; the error page when its limits
/// are not numbers.
// A response is big, but these run once per request.
#[allow(clippy::result_large_err)]
pub(in crate::web) fn settings_from_form(
    form: &SettingsForm,
    current: &NodeSettings,
    base: &str,
) -> Result<NodeSettings, Response> {
    let (Some(download), Some(storage)) = (
        parse_limit(&form.download_limit_mb_per_day),
        parse_limit(&form.storage_limit_mb),
    ) else {
        return Err(settings_error(
            StatusCode::BAD_REQUEST,
            form,
            base,
            "Limits are whole numbers of megabytes, or empty for none. Nothing was changed.",
        ));
    };
    let hour = |text: &str| text.trim().parse::<u8>().ok().filter(|h| *h < 24);
    let crawl_hours = match &form.crawl_hours {
        None => None,
        Some(_) => match (hour(&form.crawl_from), hour(&form.crawl_to)) {
            (Some(from), Some(to)) => Some(CrawlHours { from, to }),
            _ => {
                return Err(settings_error(
                    StatusCode::BAD_REQUEST,
                    form,
                    base,
                    "Crawl hours are whole hours from 0 to 23. Nothing was changed.",
                ))
            }
        },
    };
    let site_grades_percent = match &form.site_grades_percent {
        None => current.site_grades_percent,
        Some(text) => match text.trim().parse::<u8>().ok().filter(|p| *p <= 100) {
            Some(percent) => percent,
            None => {
                return Err(settings_error(
                    StatusCode::BAD_REQUEST,
                    form,
                    base,
                    "Site grades count from 0 to 100 percent. Nothing was changed.",
                ))
            }
        },
    };
    let mut page_sets = current.page_sets.clone();
    if form.page_sets_shown.is_some() {
        for set in SETS {
            let Some(size) = form.other.get(&format!("page_set.{}", set.id)) else {
                continue;
            };
            match size.parse::<PageSetSize>() {
                Ok(size) => page_sets.set(set.id, size),
                Err(_) => {
                    return Err(settings_error(
                        StatusCode::BAD_REQUEST,
                        form,
                        base,
                        "A page set size is automatic, off, all or a number of pages. Nothing \
                         was changed.",
                    ))
                }
            }
        }
    }
    let mut settings = NodeSettings {
        background_updates: form.background_updates.is_some(),
        download_limit_mb_per_day: download,
        storage_limit_mb: storage,
        workload: Workload::Custom,
        crawl_hours,
        // A pause stays until it ends or is lifted.
        paused_until: current.paused_until,
        fill_from_network: if form.fill_shown.is_some() {
            form.fill_from_network.is_some()
        } else {
            current.fill_from_network
        },
        // Saving the resources is a choice of how much to keep too.
        setup_chosen: true,
        page_sets,
        focus_topics: if form.focus_shown.is_some() {
            crate::about::About::topics_from_text(&form.focus_topics)
        } else {
            current.focus_topics.clone()
        },
        site_grades_percent,
    };
    settings.set_workload(Workload::from_name(&form.workload).unwrap_or(Workload::Custom));
    Ok(settings)
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub(in crate::web) struct RemoteControlForm {
    /// `on` makes a new token; anything else turns remote control off.
    action: String,
    allow_public: Option<String>,
}

/// `POST /app/remote-control`: turns remote control on with a new token,
/// shown on the page this answers with and nowhere else, or off.
pub(in crate::web) async fn save_remote_control(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Some(dir) = node.data_dir() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let origin = request_origin(request.headers(), request.uri()).unwrap_or_default();
    let Ok(Form(form)) = Form::<RemoteControlForm>::from_request(request, &state).await else {
        return panel_error(StatusCode::BAD_REQUEST, "The form could not be read.");
    };
    if form.action != "on" {
        if let Err(err) = control::turn_off(&dir) {
            return panel_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Could not turn remote control off: {err:#}"),
            );
        }
        return Redirect::to("/app?section=remote").into_response();
    }
    let allow_public = form.allow_public.is_some();
    match control::turn_on(&dir, allow_public) {
        Ok(token) => {
            let https = node.https_bind().map(|https| {
                let fingerprint = crate::tls::load_or_create(&dir)
                    .map(|cert| cert.fingerprint())
                    .unwrap_or_else(|err| format!("unreadable: {err:#}"));
                (https, fingerprint)
            });
            panel_page(render_new_token(&token, &origin, node.bind(), https))
        }
        Err(err) => panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not turn remote control on: {err:#}"),
        ),
    }
}

/// The page that shows a new token, the one time it can be read.
fn render_new_token(
    token: &str,
    origin: &str,
    bind: Option<SocketAddr>,
    https: Option<(SocketAddr, String)>,
) -> String {
    let address = match (&https, bind) {
        (Some((https, _)), _) => format!(
            "https://{}:{}",
            if https.ip().is_unspecified() {
                "&lt;this computer's address&gt;".to_string()
            } else {
                escape_html(&https.ip().to_string())
            },
            https.port()
        ),
        (None, Some(addr)) if addr.ip().is_unspecified() => {
            format!("http://&lt;this computer's address&gt;:{}", addr.port())
        }
        _ => escape_html(origin),
    };
    let fingerprint = https
        .map(|(_, fingerprint)| {
            format!(
                "<dt>Certificate fingerprint</dt><dd><code>{}</code></dd>\n",
                escape_html(&fingerprint)
            )
        })
        .unwrap_or_default();
    let body = format!(
        "<main class=\"wrap node-panel\">\n<h1>Remote control is on</h1>\n\
         <p>On the other computer, open the Plumb Search app, choose \
         <strong>Connect to a node</strong>, and enter this node's address and token:</p>\n\
         <dl><dt>Address</dt><dd><code>{address}</code></dd>\n\
         <dt>Token</dt><dd><code>{}</code></dd>\n{fingerprint}</dl>\n\
         <p class=\"notice\">Copy the token now: this is the only time it is shown. \
         Anyone with it can change this node's settings, so keep it like a password. \
         Making a new token, or turning remote control off, stops the old one working.</p>\n\
         <p><a class=\"btn\" href=\"/app?section=remote\">Done</a></p>\n</main>",
        escape_html(token)
    );
    let head = format!(
        "<meta name=\"referrer\" content=\"{PANEL_REFERRER_POLICY}\">\n\
         <style>{PANEL_STYLE}</style><style>{LAYOUT_STYLE}</style>\n"
    );
    page_with_head("Remote control - Plumb Search", &head, &body)
}

pub(in crate::web) async fn refresh(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    node.refresh_now();
    Redirect::to("/app").into_response()
}

pub(in crate::web) async fn pause(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(Form(form)) = Form::<PauseForm>::from_request(request, &state).await else {
        return panel_error(StatusCode::BAD_REQUEST, "The pause form could not be read.");
    };
    let current = node.settings().unwrap_or_default();
    let Some(settings) = paused(&current, &form, now_unix()) else {
        return panel_error(StatusCode::BAD_REQUEST, "The pause form could not be read.");
    };
    if let Err(err) = node.change_settings(settings) {
        return panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not save the settings: {err:#}"),
        );
    }
    Redirect::to("/app").into_response()
}

pub(in crate::web) async fn retry(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(Form(form)) = Form::<RetryForm>::from_request(request, &state).await else {
        return panel_error(StatusCode::BAD_REQUEST, "The retry form could not be read.");
    };
    let Some(what) = form.what() else {
        return panel_error(StatusCode::BAD_REQUEST, "The retry form could not be read.");
    };
    if let Err(err) = node.retry(what) {
        return panel_error(StatusCode::CONFLICT, &err.to_string());
    }
    Redirect::to("/app").into_response()
}

pub(in crate::web) async fn backup(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    match node.make_backup() {
        Ok(_) => Redirect::to("/app?section=backup&saved=backup").into_response(),
        Err(err) => panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not make a backup: {err:#}"),
        ),
    }
}

pub(in crate::web) async fn download_backup(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    request: Request,
) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // It holds keys: only for this computer, like the settings.
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Some(path) = node.data_dir().and_then(|dir| backup::path_of(&dir, &name)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match std::fs::read(&path) {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json".to_owned()),
                (header::CACHE_CONTROL, "no-store".to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{name}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(in crate::web) async fn restore_saved(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(Form(form)) = Form::<RestoreForm>::from_request(request, &state).await else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "The restore form could not be read.",
        );
    };
    let Some(path) = node
        .data_dir()
        .and_then(|dir| backup::path_of(&dir, &form.name))
    else {
        return panel_error(StatusCode::NOT_FOUND, "There is no such backup.");
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return panel_error(StatusCode::NOT_FOUND, "There is no such backup.");
    };
    if form.confirm.as_deref() != Some("yes") {
        return panel_page(page_with_head("Confirm restore - Plumb Search", &format!("<meta name=\"referrer\" content=\"same-origin\"><style>{PANEL_STYLE}{LAYOUT_STYLE}</style>"), &format!("<main class=\"wrap node-panel\"><h1>Restore this backup?</h1><p>This replaces this node’s identity keys, trusted nodes and settings. The current keys are lost unless backed up. Plumb saves a before-restore backup first; download a copy from Backup if you need to keep it elsewhere.</p><form method=\"post\" action=\"/app/backups/restore\"><input type=\"hidden\" name=\"name\" value=\"{}\"><button name=\"confirm\" value=\"yes\">Replace keys and settings</button> <a href=\"/app?section=backup\">Cancel</a></form></main>", escape_html(&form.name))));
    }
    restore_bytes(node.as_ref(), &bytes)
}

pub(in crate::web) async fn restore_upload(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(mut form) = axum::extract::Multipart::from_request(request, &state).await else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "The backup file could not be read.",
        );
    };
    let mut confirmed = false;
    while let Ok(Some(field)) = form.next_field().await {
        if field.name() == Some("confirm") {
            confirmed = field.text().await.ok().as_deref() == Some("yes");
            continue;
        }
        if !confirmed {
            return panel_error(StatusCode::BAD_REQUEST, "Confirm that you want to replace the node’s keys and settings before uploading a backup. Nothing was changed.");
        }
        if field.name() == Some("backup") {
            return match field.bytes().await {
                Ok(bytes) if !bytes.is_empty() => restore_bytes(node.as_ref(), &bytes),
                _ => panel_error(StatusCode::BAD_REQUEST, "Choose a backup file first."),
            };
        }
    }
    panel_error(StatusCode::BAD_REQUEST, "Choose a backup file first.")
}

fn restore_bytes(node: &dyn crate::web::StatusSource, bytes: &[u8]) -> Response {
    let backup = match Backup::parse(bytes) {
        Ok(backup) => backup,
        Err(err) => {
            return panel_error(
                StatusCode::BAD_REQUEST,
                &format!("{err:#}. Nothing was changed."),
            )
        }
    };
    if let Err(err) = node.restore_backup(&backup) {
        return panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not restore the backup: {err:#}"),
        );
    }
    Redirect::to("/app?section=backup&saved=restored").into_response()
}

pub(in crate::web) async fn restart(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    if let Err(err) = node.restart() {
        return panel_error(StatusCode::CONFLICT, &err.to_string());
    }
    panel_page(page_with_head(
        "Restarting - Plumb Search",
        &format!(
            "<meta http-equiv=\"refresh\" content=\"5;url=/app\"><style>{PANEL_STYLE}</style>"
        ),
        "<main class=\"wrap node-panel\"><h1>Restarting Plumb Search</h1>\
         <p>The node is stopping and starting again with the saved settings. This page \
         comes back by itself.</p></main>",
    ))
}

pub(in crate::web) async fn retry_network(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    if let Err(err) = node.reconnect_network() {
        return panel_error(StatusCode::CONFLICT, &err.to_string());
    }
    Redirect::to("/app?section=network&saved=retry").into_response()
}

/// Keeps the owner's choices of when the node's plugins run, from the
/// form of [`render_plugin_choices`]: each plugin's folder name and its
/// choice.
pub(in crate::web) async fn save_plugins(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    if state.node.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let Ok(Form(choices)) = Form::<Vec<(String, String)>>::from_request(request, &state).await
    else {
        return panel_error(
            StatusCode::BAD_REQUEST,
            "The plugin choices could not be read.",
        );
    };
    for (plugin, choice) in choices {
        let Some(suggest) = crate::plugins::Suggest::parse(&choice) else {
            return panel_error(StatusCode::BAD_REQUEST, "That is not a plugin choice.");
        };
        if let Err(err) = state.settings.plugins.set_suggest(&plugin, suggest) {
            return panel_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Could not save plugin choices: {err:#}"),
            );
        }
    }
    Redirect::to("/app?section=search&saved=plugins").into_response()
}
