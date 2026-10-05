//! Keeps the node's place index ([`crate::places`]) in step with its
//! places file and settings, next to the page index (the places file is
//! taken from trusted nodes with the other page sets, in [`super::pages`]).

use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use plumb_index::places::{PlaceResults, PlaceSearcher};
use tracing::warn;

use super::Inner;
use crate::pages::thousands;
use crate::places::{open_or_build, remove_other_indexes, wanted};

/// Wait after a failed build before trying the same places again.
const RETRY_WAIT: Duration = Duration::from_secs(30 * 60);
/// Places listed for a search.
pub(super) const PLACES_PER_SEARCH: usize = 8;

/// The place index wanted under `settings`, opened or built when it
/// changed. `failed` remembers a failed build, so it is not tried again
/// at once.
pub(super) fn refresh(
    inner: &Inner,
    settings: &super::NodeSettings,
    failed: &mut Option<(String, Instant)>,
) {
    let wanted = wanted(
        &inner.paths.data,
        &settings.page_sets,
        settings.storage_limit_mb,
    );
    let key = wanted.as_ref().map(crate::places::key);
    let current = inner
        .places
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(key, _)| key.clone());
    if key == current {
        return;
    }
    let retry_later = failed
        .as_ref()
        .is_some_and(|(k, at)| Some(k) == key.as_ref() && at.elapsed() < RETRY_WAIT);
    if retry_later {
        return;
    }
    let found = match &wanted {
        None => None,
        Some(wanted) => match open_or_build(&inner.paths.data, wanted) {
            Ok(found) => Some(found),
            Err(err) => {
                warn!("places: {err:#}");
                inner
                    .journal
                    .warning(format!("Could not build the place index: {err:#}"));
                *failed = key.map(|k| (k, Instant::now()));
                return;
            }
        },
    };
    let kept = found.as_ref().map(|(k, _)| k.clone());
    let places = found.as_ref().map_or(0, |(_, s)| s.num_places());
    *inner.places.write().unwrap_or_else(PoisonError::into_inner) =
        found.map(|(k, s)| (k, Arc::new(s)));
    remove_other_indexes(&inner.paths.data, kept.as_deref());
    if kept.is_some() {
        inner.journal.info(format!(
            "Places ready: {} places from OpenStreetMap",
            thousands(places)
        ));
    } else if current.is_some() {
        inner.journal.info("Places turned off");
    }
    *failed = None;
}

/// The places `query` asks for, around `home` for "near me".
pub(super) fn search(
    inner: &Inner,
    query: &str,
    home: Option<&str>,
    country: Option<&str>,
) -> Option<PlaceResults> {
    let searcher: Arc<PlaceSearcher> = inner
        .places
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(_, s)| s.clone())?;
    match searcher.search(query, home, country, PLACES_PER_SEARCH) {
        Ok(found) => found,
        Err(err) => {
            warn!("searching places: {err:#}");
            None
        }
    }
}
