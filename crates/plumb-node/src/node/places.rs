//! Keeps the node's place index ([`crate::places`]) in step with its
//! places file and settings, next to the page index (the places file is
//! taken from trusted nodes with the other page sets, in [`super::pages`]).

use plumb_core::locks::RwLocked;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plumb_core::place::Place;
use plumb_index::places::{PlaceResults, PlaceSearcher};
use tracing::warn;

use super::Inner;
use crate::pages::thousands;
use crate::places::{
    open_or_build_with_budget, remove_indexes_except_with_budget, remove_other_indexes_with_budget,
    wanted,
};

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
        .read_locked()
        .as_ref()
        .map(|(key, _)| key.clone());
    retired.retain(|reader| reader.strong_count() > 0);
    if retired.is_empty() {
        // Keep the desired cache until it has been opened, including on
        // startup when there is no current reader. The current reader
        // must also stay usable while opening its replacement.
        let keep: Vec<_> = current
            .iter()
            .chain(key.iter())
            .map(String::as_str)
            .collect();
        remove_indexes_except_with_budget(&inner.paths.data, &keep, inner.storage.as_deref());
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
            let _big = inner.one_big_job();
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
        &mut *inner.places.write_locked(),
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
        .read_locked()
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
        .read_locked()
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
        .read_locked()
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use plumb_core::place::{write_place, PLACES_HEADER};
    use plumb_core::storage::{directory_bytes, StorageBudget};

    use super::super::{open_data_dir, NodeConfig, NodeSettings};
    use super::*;
    use crate::pages::PageSets;
    use crate::places::{index_dir, open_or_build, set_info, WantedPlaces};

    const CACHE_MARKER: &str = ".test-cache-reuse";
    const CACHE_PROOF: &[u8] = b"existing cache must survive opening";

    fn write_fixture(data: &Path) {
        let file = set_info().file(data);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut text = PLACES_HEADER.as_bytes().to_vec();
        for (name, kind, osm) in [
            ("Cacheton", "place=town", "n1"),
            ("Fixture cafe", "amenity=cafe", "n2"),
        ] {
            write_place(
                &mut text,
                &Place {
                    name: name.into(),
                    kind: kind.into(),
                    lat: 1.0,
                    lon: 2.0,
                    osm: osm.into(),
                    ..Place::default()
                },
            )
            .unwrap();
        }
        std::fs::write(file, text).unwrap();
    }

    fn cached(data: &Path, count: u64) -> (String, PathBuf, PlaceSearcher) {
        let wanted = WantedPlaces {
            file: set_info().file(data),
            count,
            near: Vec::new(),
        };
        let (key, searcher) = open_or_build(data, &wanted).unwrap();
        let dir = index_dir(data, &key);
        // A rebuild replaces this whole directory, so this proves reuse
        // even when rebuilding would produce identical search results.
        std::fs::write(dir.join(CACHE_MARKER), CACHE_PROOF).unwrap();
        (key, dir, searcher)
    }

    fn settings(sets: &str) -> NodeSettings {
        NodeSettings {
            page_sets: PageSets::parse(sets).unwrap(),
            ..NodeSettings::default()
        }
    }

    fn open_node(data: &Path, settings: NodeSettings) -> Inner {
        let mut config = NodeConfig::desktop(data.to_path_buf());
        config.settings = settings;
        let rank = crate::rank_config(config.alpha);
        // Use the real startup data-directory open and fresh Inner, then
        // drive its production refresh synchronously without workers.
        let opened = open_data_dir(&config, rank).unwrap();
        let budget = StorageBudget::open(data, u64::MAX).unwrap();
        let (_stop, stopped) = tokio::sync::watch::channel(false);
        Inner::new(config, rank, opened, stopped, Some(budget))
    }

    #[test]
    fn startup_reuses_desired_cache_and_cleans_obsolete_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        write_fixture(data);
        let (key, cache, _) = cached(data, 1);
        let (_, obsolete, _) = cached(data, 2);
        let settings = settings("places=1");
        let inner = open_node(data, settings.clone());
        assert!(inner.places.read().unwrap().is_none());
        let mut failed = None;
        let mut retired = Vec::new();

        refresh(&inner, &settings, &mut failed, &mut retired);

        assert_eq!(
            std::fs::read(cache.join(CACHE_MARKER)).unwrap(),
            CACHE_PROOF
        );
        let places = inner.places.read().unwrap();
        let (opened_key, searcher) = places.as_ref().unwrap();
        assert_eq!(opened_key, &key);
        assert_eq!(searcher.num_places(), 1);
        assert_eq!(
            searcher.locate("Cacheton", None).unwrap().unwrap().osm,
            "n1"
        );
        assert!(!obsolete.exists());
        assert!(failed.is_none());
        assert!(retired.is_empty());
        assert_eq!(inner.storage.as_ref().unwrap().status().reserved_bytes, 0);
    }

    #[test]
    fn cached_switch_and_places_off_retain_readers_until_release() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        write_fixture(data);
        let (old_key, old_cache, old_searcher) = cached(data, 1);
        let (key, cache, _) = cached(data, 2);
        let (_, obsolete, _) = cached(data, 3);
        let mut settings = settings("places=2");
        let inner = open_node(data, settings.clone());
        let held = Arc::new(old_searcher);
        *inner.places.write().unwrap() = Some((old_key, held.clone()));
        let budget = inner.storage.as_ref().unwrap();
        let mut failed = None;
        let mut retired = Vec::new();

        refresh(&inner, &settings, &mut failed, &mut retired);

        assert_eq!(
            std::fs::read(cache.join(CACHE_MARKER)).unwrap(),
            CACHE_PROOF
        );
        {
            let places = inner.places.read().unwrap();
            let (opened_key, searcher) = places.as_ref().unwrap();
            assert_eq!(opened_key, &key);
            assert_eq!(searcher.num_places(), 2);
        }
        assert!(!obsolete.exists());
        assert!(old_cache.exists());
        assert_eq!(retired.len(), 1);
        let used = budget.status().used_bytes;
        refresh(&inner, &settings, &mut failed, &mut retired);
        assert_eq!(budget.status().used_bytes, used);
        assert!(old_cache.exists());

        settings.page_sets = PageSets::parse("places=off").unwrap();
        refresh(&inner, &settings, &mut failed, &mut retired);
        assert!(inner.places.read().unwrap().is_none());
        assert!(old_cache.exists());
        assert!(cache.exists());
        assert_eq!(held.locate("Cacheton", None).unwrap().unwrap().osm, "n1");

        drop(held);
        let removed = directory_bytes(&old_cache).unwrap() + directory_bytes(&cache).unwrap();
        let used = budget.status().used_bytes;
        refresh(&inner, &settings, &mut failed, &mut retired);
        assert!(!old_cache.exists());
        assert!(!cache.exists());
        assert!(retired.is_empty());
        assert!(failed.is_none());
        assert_eq!(budget.status().used_bytes, used - removed);
        assert_eq!(budget.status().reserved_bytes, 0);
    }

    #[test]
    fn places_off_startup_removes_existing_caches_and_releases_budget() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        write_fixture(data);
        let (_, first, _) = cached(data, 1);
        let (_, second, _) = cached(data, 2);
        let settings = settings("places=off");
        let inner = open_node(data, settings.clone());
        let budget = inner.storage.as_ref().unwrap();
        let used = budget.status().used_bytes;
        let removed = directory_bytes(&first).unwrap() + directory_bytes(&second).unwrap();
        let mut failed = None;
        let mut retired = Vec::new();

        refresh(&inner, &settings, &mut failed, &mut retired);

        assert!(inner.places.read().unwrap().is_none());
        assert!(!first.exists());
        assert!(!second.exists());
        assert!(retired.is_empty());
        assert!(failed.is_none());
        assert_eq!(budget.status().used_bytes, used - removed);
        assert_eq!(budget.status().reserved_bytes, 0);
    }
}
