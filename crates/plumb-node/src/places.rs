//! Places from OpenStreetMap, searched by where they are: "pizza in
//! denver", "coffee near me" (see [`plumb_core::place`] and
//! [`plumb_index::places`]).
//!
//! The places file is a page set's file (`DIR/pages/sets/places.tsv.gz`,
//! most notable first), so it is made, kept to a number, and taken from
//! trusted nodes like the other sets (see [`crate::pages`]). Its index is
//! its own, `DIR/pages/places-<key>/`, since places are searched by where
//! they are rather than by name.

use std::path::{Path, PathBuf};

use anyhow::Result;
use plumb_core::place::{parse_place, Place};
use plumb_index::places::{build_place_index, PlaceSearcher, PLACES_SET};
use tracing::{info, warn};

use crate::pages::{PageSets, SetInfo, PAGES_DIR};

/// The places set.
pub fn set_info() -> &'static SetInfo {
    SetInfo::find(PLACES_SET).expect("the places set is listed")
}

/// The places file to index and how many of its places, `None` when none
/// are kept or there is no file yet.
pub fn wanted(data_dir: &Path, sets: &PageSets, storage_limit_mb: u64) -> Option<(PathBuf, u64)> {
    let set = set_info();
    let count = set.kept(sets, storage_limit_mb);
    let file = set.file(data_dir);
    (count > 0 && file.is_file()).then_some((file, count))
}

/// Places kept on Automatic under a storage limit of `storage_limit_mb`
/// (0 for none). The file starts with every city and town and the places
/// with a Wikidata item (about a million, 140 MB with the index), then the
/// rest: under 1 GB none, since towns alone find nothing; under 8 GB that
/// first million, so museums, sights and stations are found anywhere; with
/// 8 GB or more, all of them (about 24 million, 3.5 GB), so every café and
/// shop is.
pub fn auto_places(storage_limit_mb: u64) -> u64 {
    match storage_limit_mb {
        0 => u64::MAX,
        mb if mb < 1_000 => 0,
        mb if mb < 8_000 => 1_000_000,
        _ => u64::MAX,
    }
}

/// Names the index of `wanted`: changes when the file (size or time) or
/// the count changes.
pub fn key(wanted: &(PathBuf, u64)) -> String {
    let (file, count) = wanted;
    let meta = std::fs::metadata(file).ok();
    let modified = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let len = meta.map_or(0, |m| m.len());
    let text = format!("v1|{len}:{modified}:{count}");
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// The directory of the place index named `key`.
pub fn index_dir(data_dir: &Path, key: &str) -> PathBuf {
    data_dir.join(PAGES_DIR).join(format!("places-{key}"))
}

/// Reads up to `limit` places of the places file `path`, most notable
/// first.
pub fn read_places(path: &Path, limit: u64) -> Result<impl Iterator<Item = Place>> {
    let reader = plumb_ingest::open_maybe_gz(path)?;
    let path = path.to_path_buf();
    let mut bad = 0u64;
    Ok(std::io::BufRead::lines(reader)
        .enumerate()
        .map_while(move |(n, line)| match line {
            Ok(line) => Some((n, line)),
            Err(err) => {
                warn!("reading {}: {err}", path.display());
                None
            }
        })
        .filter(|(n, line)| !(*n == 0 && line.starts_with("rank\t")) && !line.is_empty())
        .filter_map(move |(n, line)| match parse_place(&line) {
            Ok(place) => Some(place),
            Err(err) => {
                bad += 1;
                if bad <= 3 {
                    warn!("places line {}: {err:#}", n + 1);
                }
                None
            }
        })
        .take(usize::try_from(limit).unwrap_or(usize::MAX)))
}

/// Opens the place index for `wanted`, building it first when there is
/// none.
pub fn open_or_build(data_dir: &Path, wanted: &(PathBuf, u64)) -> Result<(String, PlaceSearcher)> {
    let key = key(wanted);
    let dir = index_dir(data_dir, &key);
    if let Ok(searcher) = PlaceSearcher::open(&dir) {
        return Ok((key, searcher));
    }
    let started = std::time::Instant::now();
    info!("indexing places from {}", wanted.0.display());
    let stats = build_place_index(&dir, read_places(&wanted.0, wanted.1)?)?;
    info!(
        "built the place index of {} places ({} towns) in {:.1}s",
        stats.places,
        stats.towns,
        started.elapsed().as_secs_f32()
    );
    Ok((key, PlaceSearcher::open(&dir)?))
}

/// Opens the index of the places file `file` (all of it), built next to
/// it on first use: for `plumb serve` and `plumb search`.
pub fn open_file(file: &Path) -> Result<PlaceSearcher> {
    let key = key(&(file.to_path_buf(), u64::MAX));
    let mut name = file.file_name().unwrap_or_default().to_owned();
    name.push(format!(".index-{key}"));
    let dir = file.with_file_name(name);
    if let Ok(searcher) = PlaceSearcher::open(&dir) {
        return Ok(searcher);
    }
    info!(
        "indexing places from {} into {}",
        file.display(),
        dir.display()
    );
    build_place_index(&dir, read_places(file, u64::MAX)?)?;
    PlaceSearcher::open(&dir)
}

/// Deletes place indexes other than `keep`.
pub fn remove_other_indexes(data_dir: &Path, keep: Option<&str>) {
    let Ok(entries) = std::fs::read_dir(data_dir.join(PAGES_DIR)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(key) = name.to_str().and_then(|n| n.strip_prefix("places-")) else {
            continue;
        };
        if Some(key) != keep {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_core::place::{place_rank, write_place, PLACES_HEADER};

    fn write_places(data_dir: &Path, places: &[Place]) {
        let file = set_info().file(data_dir);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut text = PLACES_HEADER.as_bytes().to_vec();
        for place in places {
            write_place(&mut text, place).unwrap();
        }
        std::fs::write(file, text).unwrap();
    }

    #[test]
    fn places_follow_their_own_sizes() {
        let sets = PageSets::default();
        let kept = |mb| set_info().kept(&sets, mb);
        assert_eq!(kept(500), 0);
        assert_eq!(kept(2_000), 1_000_000);
        assert_eq!(kept(8_000), u64::MAX);
        assert_eq!(kept(0), u64::MAX);
        let off = PageSets::parse("places=off").unwrap();
        assert_eq!(set_info().kept(&off, 0), 0);
        // Other sets are as they were.
        let wikipedia = SetInfo::find("wikipedia-en").unwrap();
        assert_eq!(wikipedia.kept(&sets, 500), 100_000);
    }

    #[test]
    fn places_are_indexed_apart_from_pages() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let sets = PageSets::default();
        assert!(wanted(data, &sets, 0).is_none());
        write_places(
            data,
            &[
                Place {
                    rank: place_rank("place=city", 715_000, false, false, 3),
                    name: "Denver".into(),
                    kind: "place=city".into(),
                    lat: 39.7392,
                    lon: -104.9903,
                    osm: "n1".into(),
                    ..Place::default()
                },
                Place {
                    rank: place_rank("amenity=cafe", 0, false, false, 3),
                    name: "Huckleberry".into(),
                    kind: "amenity=cafe".into(),
                    lat: 39.76,
                    lon: -104.99,
                    osm: "n2".into(),
                    ..Place::default()
                },
            ],
        );
        // The page index leaves the places file alone.
        assert!(crate::pages::Wanted::new(data, &sets, 0).key().is_none());
        let wanted = wanted(data, &sets, 0).unwrap();
        let (key, searcher) = open_or_build(data, &wanted).unwrap();
        assert_eq!(searcher.num_places(), 2);
        let found = searcher
            .search("coffee in denver", None, None, 5)
            .unwrap()
            .unwrap();
        assert_eq!(found.hits[0].place.name, "Huckleberry");
        // Opened again, not rebuilt; other keys are cleared away.
        assert_eq!(open_or_build(data, &wanted).unwrap().0, key);
        let one = (wanted.0.clone(), 1);
        let (other, searcher) = open_or_build(data, &one).unwrap();
        assert_eq!(searcher.num_places(), 1);
        remove_other_indexes(data, Some(&other));
        assert!(!index_dir(data, &key).exists());
        assert!(index_dir(data, &other).exists());
        // Page indexes are not place indexes.
        crate::pages::remove_other_indexes(data, None);
        assert!(index_dir(data, &other).exists());
    }
}
