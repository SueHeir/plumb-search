//! The remote control API, which the desktop app on another computer uses
//! to show and change this node's settings (see [`crate::node::control`]):
//!
//! - `GET /api/control` returns a [`ControlView`]: the status, settings and
//!   features,
//! - `POST /api/control/settings` puts a JSON [`NodeSettings`] in force,
//! - `POST /api/control/features` saves a JSON [`FeatureSettings`] for the
//!   next start,
//! - `POST /api/control/refresh` starts a refresh now.
//!
//! Each request needs remote control turned on (otherwise 404, as if the API
//! were not there) and its token as `Authorization: Bearer <token>`. Unless
//! the owner allowed public addresses, requests must also come from this
//! computer or a local network, straight to the node: one with a
//! `Forwarded`, `X-Forwarded-For` or `X-Real-IP` header came through a
//! reverse proxy, likely from the internet, and is refused. Requests with an
//! `Origin` header come from a web page, and are refused too; the API sends
//! no CORS headers, so no page can read its answers either.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::AppState;
use crate::node::control;
use crate::node::features::FeatureSettings;
use crate::node::{NodeSettings, Status};

/// What `GET /api/control` returns: all the panel needs to show a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlView {
    pub status: Status,
    pub settings: NodeSettings,
    /// The features the node runs with.
    pub features: FeatureSettings,
    /// The features it will run with after a restart.
    pub saved_features: FeatureSettings,
    /// Whether the node offers private search now.
    #[serde(default)]
    pub private_search_ready: bool,
    #[serde(default)]
    pub data_dir: Option<String>,
}

/// An error answer: `{"error": "..."}`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ControlError {
    pub error: String,
}

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/control", get(view))
        .route("/api/control/settings", post(change_settings))
        .route("/api/control/features", post(change_features))
        .route("/api/control/refresh", post(refresh))
}

fn error(status: StatusCode, message: &str) -> Response {
    let mut response = (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(ControlError {
            error: message.to_string(),
        }),
    )
        .into_response();
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, "Bearer".parse().unwrap());
    }
    response
}

/// Headers that say a request came through a reverse proxy.
const FORWARDED_HEADERS: [&str; 3] = ["forwarded", "x-forwarded-for", "x-real-ip"];

/// Lets the request through, or says why not.
// A response is big, but these run once per request.
#[allow(clippy::result_large_err)]
fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Result<(), Response> {
    let off = || error(StatusCode::NOT_FOUND, "Remote control is off on this node.");
    let Some(dir) = state.node.as_ref().and_then(|node| node.data_dir()) else {
        return Err(off());
    };
    let control = match control::load(&dir) {
        Ok(Some(control)) => control,
        Ok(None) => return Err(off()),
        Err(err) => {
            warn!("cannot read the remote control settings: {err:#}");
            return Err(off());
        }
    };
    if headers.contains_key(header::ORIGIN) {
        return Err(error(
            StatusCode::FORBIDDEN,
            "Web pages cannot use remote control.",
        ));
    }
    if !control.allow_public {
        let proxied = FORWARDED_HEADERS
            .iter()
            .any(|name| headers.contains_key(*name));
        let nearby = peer.is_some_and(|peer| control::is_nearby(peer.ip()));
        if proxied || !nearby {
            return Err(error(
                StatusCode::FORBIDDEN,
                "This node takes remote control only from its own computer and local \
                 networks. Its owner can allow other addresses when turning remote control on.",
            ));
        }
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.trim().split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(token)
        });
    match token {
        Some(token) if control.accepts(token) => Ok(()),
        _ => {
            info!(
                "refused a remote control request with a wrong or missing token from {}",
                peer.map_or("an unknown address".to_string(), |peer| peer
                    .ip()
                    .to_string())
            );
            Err(error(
                StatusCode::UNAUTHORIZED,
                "The remote control token is wrong. Make a new one on this node.",
            ))
        }
    }
}

fn peer(request: &Request) -> Option<SocketAddr> {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer)
}

async fn view(State(state): State<AppState>, request: Request) -> Response {
    if let Err(response) = authorize(&state, request.headers(), peer(&request)) {
        return response;
    }
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let saved_features = match node.saved_features() {
        Ok(saved) => saved,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let view = ControlView {
        status: node.status(),
        settings: node.settings().unwrap_or_default(),
        features: node.features(),
        saved_features,
        private_search_ready: state.private_search(),
        data_dir: node.data_dir().map(|dir| dir.display().to_string()),
    };
    ([(header::CACHE_CONTROL, "no-store")], Json(view)).into_response()
}

/// The request's JSON body, after checking it may change anything.
// A response is big, but these run once per request.
#[allow(clippy::result_large_err)]
async fn authorized_body<T: serde::de::DeserializeOwned>(
    state: &AppState,
    request: Request,
) -> Result<T, Response> {
    authorize(state, request.headers(), peer(&request))?;
    let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|_| error(StatusCode::BAD_REQUEST, "The request is too big."))?;
    serde_json::from_slice(&bytes).map_err(|err| {
        error(
            StatusCode::BAD_REQUEST,
            &format!("Cannot read the request: {err}"),
        )
    })
}

async fn change_settings(State(state): State<AppState>, request: Request) -> Response {
    let settings: NodeSettings = match authorized_body(&state, request).await {
        Ok(settings) => settings,
        Err(response) => return response,
    };
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match node.change_settings(settings) {
        Ok(()) => {
            info!("settings changed by remote control");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not save the settings: {err:#}"),
        ),
    }
}

async fn change_features(State(state): State<AppState>, request: Request) -> Response {
    let features: FeatureSettings = match authorized_body(&state, request).await {
        Ok(features) => features,
        Err(response) => return response,
    };
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Err(err) = features.check() {
        return error(StatusCode::BAD_REQUEST, &err.to_string());
    }
    match node.change_features(features) {
        Ok(()) => {
            info!("features changed by remote control");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not save feature settings: {err:#}"),
        ),
    }
}

async fn refresh(State(state): State<AppState>, request: Request) -> Response {
    if let Err(response) = authorize(&state, request.headers(), peer(&request)) {
        return response;
    }
    let Some(node) = &state.node else {
        return StatusCode::NOT_FOUND.into_response();
    };
    node.refresh_now();
    StatusCode::NO_CONTENT.into_response()
}
