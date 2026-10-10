//! Keeps the node's place index ([`crate::places`]) in step with its
//! places file and settings, next to the page index (the places file is
//! taken from trusted nodes with the other page sets, in [`super::pages`]).

use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use plumb_core::place::Place;
use plumb_index::places::{PlaceResults, PlaceSearcher};
use tracing::warn;

use super::Inner;
use crate::pages::thousands;
use crate::places::{open_or_build_with_budget, remove_other_indexes_with_budget, wanted};

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
    retired: &mut Vec<std::sync::Weak<PlaceSearcher>>,
) {
    let near = homes(inner);
    let wanted = wanted(
        &inner.paths.data,
        &settings.page_sets,
        settings.storage_limit_mb,
        &near,
    );
    let key = wanted.as_ref().map(crate::places::key);
    let current = inner
        .places
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(key, _)| key.clone());
    retired.retain(|reader| reader.strong_count() > 0);
    if retired.is_empty() {
        remove_other_indexes_with_budget(
            &inner.paths.data,
            current.as_deref(),
            inner.storage.as_deref(),
        );
    }
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
        Some(wanted) => {
            match open_or_build_with_budget(&inner.paths.data, wanted, inner.storage.clone()) {
                Ok(found) => Some(found),
                Err(err) => {
                    warn!("places: {err:#}");
                    inner
                        .journal
                        .warning(format!("Could not build the place index: {err:#}"));
                    *failed = key.map(|k| (k, Instant::now()));
                    return;
                }
            }
        }
    };
    let kept = found.as_ref().map(|(k, _)| k.clone());
    let places = found.as_ref().map_or(0, |(_, s)| s.num_places());
    let old = std::mem::replace(
        &mut *inner.places.write().unwrap_or_else(PoisonError::into_inner),
        found.map(|(k, s)| (k, Arc::new(s))),
    );
    if let Some((_, reader)) = old {
        retired.push(Arc::downgrade(&reader));
    }
    retired.retain(|reader| reader.strong_count() > 0);
    if retired.is_empty() {
        remove_other_indexes_with_budget(
            &inner.paths.data,
            kept.as_deref(),
            inner.storage.as_deref(),
        );
    }
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

/// Where the towns on the node's About pages are, found in the place
/// index being served: a node with a storage limit keeps every place
/// near them (see [`crate::places::WantedPlaces`]). Empty until there is a
/// place index, or when no About page gives a town.
pub(super) fn homes(inner: &Inner) -> Vec<(f64, f64)> {
    known_homes(inner).unwrap_or_default()
}

/// [`homes`], but `None` while About pages give towns and there is no
/// place index yet to find them in: until then, which places are near
/// them is not known.
pub(super) fn known_homes(inner: &Inner) -> Option<Vec<(f64, f64)>> {
    let towns = crate::about::all_towns(&inner.paths.data.join("history"));
    if towns.is_empty() {
        return Some(Vec::new());
    }
    let searcher = inner
        .places
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(_, s)| s.clone())?;
    let mut homes: Vec<(f64, f64)> = towns
        .iter()
        .filter_map(|town| searcher.locate(town, None).ok().flatten())
        .map(|place| (place.lat, place.lon))
        .collect();
    homes.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    homes.dedup();
    Some(homes)
}

/// The town (or else other place) `text` names.
pub(super) fn locate(inner: &Inner, text: &str, country: Option<&str>) -> Option<Place> {
    let searcher: Arc<PlaceSearcher> = inner
        .places
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(_, s)| s.clone())?;
    searcher.locate(text, country).unwrap_or_else(|err| {
        warn!("locating {text:?}: {err:#}");
        None
    })
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
