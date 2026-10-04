//! Keeps the node's page index ([`crate::pages`]) in step with its page
//! set files and settings, and adds pages to search results.

use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use plumb_index::pages::place_pages;
use plumb_index::SearchResults;
use tracing::warn;

use super::Inner;
use crate::pages::{open_or_build, remove_other_indexes, thousands, Wanted};

/// How often the job looks for changed settings or set files.
#[cfg(not(test))]
const LOOK_EVERY: Duration = Duration::from_secs(10);
#[cfg(test)]
const LOOK_EVERY: Duration = Duration::from_millis(200);
/// Wait after a failed build before trying the same pages again.
const RETRY_WAIT: Duration = Duration::from_secs(30 * 60);
/// How often a wait looks for shutdown.
const TICK: Duration = Duration::from_millis(250);
/// Pages looked at per search, before [`place_pages`] picks.
const PAGES_PER_SEARCH: usize = 10;

/// Runs until the node stops, on a blocking thread.
pub(super) fn run(inner: Arc<Inner>) {
    let mut failed: Option<(String, Instant)> = None;
    while !inner.stopping() {
        let settings = inner.settings();
        let wanted = Wanted::new(
            &inner.paths.data,
            &settings.page_sets,
            settings.storage_limit_mb,
        );
        let key = wanted.key();
        let current = inner.page_key();
        let retry_later = failed
            .as_ref()
            .is_some_and(|(k, at)| Some(k) == key.as_ref() && at.elapsed() < RETRY_WAIT);
        if key != current && !retry_later {
            match open_or_build(&inner.paths.data, &wanted) {
                Ok(found) => {
                    let pages = found.as_ref().map_or(0, |(_, s)| s.num_pages());
                    let kept = found.as_ref().map(|(k, _)| k.clone());
                    *inner.pages.write().unwrap_or_else(PoisonError::into_inner) =
                        found.map(|(k, s)| (k, Arc::new(s)));
                    remove_other_indexes(&inner.paths.data, kept.as_deref());
                    if kept.is_some() {
                        inner.journal.info(format!(
                            "Page sets ready: {} pages searched next to the sites",
                            thousands(pages)
                        ));
                    } else if current.is_some() {
                        inner.journal.info("Page sets turned off");
                    }
                    failed = None;
                }
                Err(err) => {
                    warn!("page sets: {err:#}");
                    inner
                        .journal
                        .warning(format!("Could not build the page index: {err:#}"));
                    failed = key.map(|k| (k, Instant::now()));
                }
            }
        }
        let until = Instant::now() + LOOK_EVERY;
        while !inner.stopping() && Instant::now() < until {
            std::thread::sleep(TICK);
        }
    }
}

impl Inner {
    fn page_key(&self) -> Option<String> {
        self.pages
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|(key, _)| key.clone())
    }
}

/// Adds the pages found for `query` (as corrected, when the results are
/// for a corrected spelling) to `results`.
pub(super) fn add_pages(inner: &Inner, query: &str, results: &mut SearchResults) {
    let Some(searcher) = inner
        .pages
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(_, s)| s.clone())
    else {
        return;
    };
    let query = match &results.spelling {
        Some(spelling) if spelling.applied => spelling.query.as_str(),
        _ => query,
    };
    match searcher.search(query, PAGES_PER_SEARCH) {
        Ok(found) => results.pages = place_pages(&results.hits, found),
        Err(err) => warn!("searching pages: {err:#}"),
    }
}
