//! "Set up my node" (Liz, 2026-10-04: "the desktop app should have a
//! 'setup my node' playbook which lets you select how much data to download
//! from crawlers").
//!
//! A new desktop node sets up from a trusted node in the network with its
//! best 50,000 sites, then waits for the person to choose how much more of
//! the network's crawls to keep before filling its free space (see
//! `node/fill.rs` and [`NodeSettings::setup_chosen`]). Until they choose,
//! the panel's overview opens with this step; `POST /app/setup` saves the
//! choice as the storage limit. Servers and Docker containers never ask:
//! they keep the best million sites, or what memory allows.

use axum::extract::{FromRequest, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;
use tracing::warn;

use super::panel::{forbidden, panel_error, refusal};
use super::AppState;
use crate::node::NodeSettings;

/// The choices offered: a name, the storage limit in MB (0 for none), and
/// what it holds, in words. A site takes about 5 KB of disk with its index
/// entry and vector, and filling stops at 90% of the limit.
pub(super) const SIZES: [(&str, u64, &str); 4] = [
    (
        "small",
        500,
        "500 MB: about 100,000 of the best-known sites and 100,000 Wikipedia articles",
    ),
    (
        "medium",
        2_000,
        "2 GB: about 400,000 sites and a million Wikipedia articles (recommended)",
    ),
    (
        "large",
        8_000,
        "8 GB: about 1.5 million sites and every Wikipedia article",
    ),
    (
        "everything",
        0,
        "Everything the network has, as much as this computer's memory allows",
    ),
];

/// The size picked when none is.
const RECOMMENDED: &str = "medium";

#[derive(Debug, Deserialize)]
pub(super) struct SetupForm {
    size: String,
}

/// The setup step, for the top of the panel's overview.
pub(super) fn render_setup(body: &mut String, settings: &NodeSettings) {
    body.push_str(
        "<form method=\"post\" action=\"/app/setup\" class=\"notice setup\">\
         <h3>Set up my node</h3>\
         <p>Your node searches the 50,000 best-known sites already. How much more \
         of the network's crawls should it keep on this computer? More sites find \
         more, and take more disk. You can change this later under Resources.</p>",
    );
    let current = SIZES
        .iter()
        .find(|(_, mb, _)| *mb == settings.storage_limit_mb)
        .map_or(RECOMMENDED, |(name, _, _)| name);
    for (name, _, words) in SIZES {
        body.push_str(&format!(
            "<label><input type=\"radio\" name=\"size\" value=\"{name}\"{}> {words}</label>",
            if name == current { " checked" } else { "" }
        ));
    }
    body.push_str("<p class=\"btns\"><button type=\"submit\">Set up my node</button></p></form>");
}

/// `settings` with the size chosen; `None` for a size not offered.
pub(super) fn chosen(settings: &NodeSettings, size: &str) -> Option<NodeSettings> {
    let (_, mb, _) = SIZES.iter().find(|(name, _, _)| *name == size)?;
    Some(NodeSettings {
        storage_limit_mb: *mb,
        setup_chosen: true,
        ..settings.clone()
    })
}

/// `POST /app/setup`: saves the size chosen.
pub(super) async fn save_setup(State(state): State<AppState>, request: Request) -> Response {
    let Some(node) = state.node.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(why) = refusal(&request) {
        return forbidden(why);
    }
    let form = Form::<SetupForm>::from_request(request, &state).await;
    let current = node.settings().unwrap_or_default();
    let Some(settings) = form
        .ok()
        .and_then(|Form(form)| chosen(&current, &form.size))
    else {
        return panel_error(StatusCode::BAD_REQUEST, "Pick how much to keep.");
    };
    if let Err(err) = node.change_settings(settings) {
        warn!("could not save the settings: {err:#}");
        return panel_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Could not save the settings: {err:#}"),
        );
    }
    Redirect::to("/app?saved=setup").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_sets_the_storage_limit_and_ends_the_step() {
        let settings = NodeSettings::desktop();
        assert!(!settings.setup_chosen);
        let small = chosen(&settings, "small").unwrap();
        assert_eq!((small.storage_limit_mb, small.setup_chosen), (500, true));
        assert_eq!(small.download_limit_mb_per_day, 500, "the rest stays");
        assert_eq!(chosen(&settings, "everything").unwrap().storage_limit_mb, 0);
        assert!(chosen(&settings, "huge").is_none());
    }

    #[test]
    fn the_step_offers_every_size_with_the_current_one_picked() {
        let mut body = String::new();
        render_setup(&mut body, &NodeSettings::desktop());
        assert!(body.contains("action=\"/app/setup\""), "{body}");
        assert!(body.contains("value=\"medium\" checked"), "{body}");
        for (name, _, _) in SIZES {
            assert!(body.contains(&format!("value=\"{name}\"")), "{body}");
        }
    }
}
