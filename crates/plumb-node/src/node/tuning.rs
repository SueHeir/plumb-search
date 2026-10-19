//! What a node's owner and their AI apps asked of it, as the MCP tool
//! `node_tuning` shows it, so an app that improves search for them can
//! see what earlier runs did and pick up from there.

use plumb_core::locks::RwLocked;
use serde::Serialize;

use super::Inner;
use crate::wanted_pages::WantedSite;

/// Most sites [`Tuning`] lists.
const MAX_SITES_LISTED: usize = 100;

/// What [`Inner::tuning`] gives.
#[derive(Debug, Clone, Serialize)]
pub struct Tuning {
    /// The focus topics of the node's settings, which `focus_topics`
    /// changes.
    pub focus_topics: Vec<String>,
    /// More focus topics, given with `--focus` when the node started.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fixed_focus_topics: Vec<String>,
    /// Why crawls wait now, when they do; `fetch_pages` waits only for
    /// the download and storage limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crawls_paused: Option<String>,
    /// Whether the node holds page sets, next to which it shows pages
    /// found: `fetch_pages` needs them.
    pub shows_pages: bool,
    /// The sites AI apps asked for pages of, the most recent first.
    pub asked_sites: Vec<WantedSite>,
    /// Pages kept for them in all.
    pub asked_pages: usize,
}

impl Inner {
    pub(super) fn tuning(&self) -> Tuning {
        let (mut asked_sites, asked_pages) = self.asked_pages.summary();
        asked_sites.retain(|site| !self.denies(&format!("https://{}/", site.site)));
        asked_sites.truncate(MAX_SITES_LISTED);
        Tuning {
            focus_topics: self.settings().focus_topics,
            fixed_focus_topics: self.config.focus_topics.clone(),
            crawls_paused: self.pause_shown().map(|pause| pause.reason),
            shows_pages: self.pages.read_locked().is_some(),
            asked_sites,
            asked_pages,
        }
    }
}
