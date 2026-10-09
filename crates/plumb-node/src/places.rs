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

/// The places a node indexes: the first `count` of `file`, plus the ones
/// within [`NEAR_KM`] of `near` (the towns on its About pages, as
/// latitude and longitude) past them.
#[derive(Debug, Clone, PartialEq)]
pub struct WantedPlaces {
    pub file: PathBuf,
    pub count: u64,
    pub near: Vec<(f64, f64)>,
}

/// The places to index under `sets` and a storage limit of
/// `storage_limit_mb`, around `near`; `None` when none are kept or there
/// is no file yet.
pub fn wanted(
    data_dir: &Path,
    sets: &PageSets,
    storage_limit_mb: u64,
    near: &[(f64, f64)],
) -> Option<WantedPlaces> {
    let set = set_info();
    let count = set.kept(sets, storage_limit_mb);
    let file = set.file(data_dir);
    (count > 0 && file.is_file()).then(|| WantedPlaces {
        file,
        count,
        near: if count == u64::MAX {
            Vec::new()
        } else {
            near.to_vec()
        },
    })
}

/// The towns whose places a node with `count` places kept everywhere
/// also keeps past them: none when it keeps them all.
pub fn file_near(count: u64, near: &[(f64, f64)]) -> &[(f64, f64)] {
    if count == 0 || count == u64::MAX {
        &[]
    } else {
        near
    }
}

/// Names the towns `near` for a places file's notes
/// ([`crate::pages::SetFileNotes::near`]): 0 for none.
pub fn near_key(near: &[(f64, f64)]) -> u64 {
    if near.is_empty() {
        return 0;
    }
    let text: String = near
        .iter()
        .map(|(lat, lon)| format!("{lat:.2},{lon:.2}|"))
        .collect();
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    u64::from_le_bytes(digest[..8].try_into().expect("8 bytes")).max(1)
}

/// Keeps the lines of a places file that are places within [`NEAR_KM`]
/// of a point of `near`, or specialty places ([`Place::is_specialty`]), for
/// a file cut past its first places.
pub fn near_lines(near: Vec<(f64, f64)>) -> crate::pages::LineFilter {
    Box::new(move |line: &[u8]| {
        if near.is_empty() && !may_be_specialty(line) {
            return false;
        }
        std::str::from_utf8(line)
            .ok()
            .and_then(|line| parse_place(line.trim_end_matches(['\n', '\r'])).ok())
            .is_some_and(|place| is_near(&place, &near) || place.is_specialty())
    })
}

/// Places on Automatic are kept everywhere up to this many: every city
/// and town and the places with a Wikidata item (140 MB with the index).
pub const EVERYWHERE: u64 = 1_000_000;

/// Past the first [`EVERYWHERE`], a node with a storage limit keeps the
/// places this close to a town on one of its About pages.
pub const NEAR_KM: f64 = 100.0;

/// Places kept everywhere on Automatic under a storage limit of
/// `storage_limit_mb` (0 for none). The file starts with every city and
/// town and the places with a Wikidata item (about a million, 140 MB with
/// the index), then the rest: under 1 GB none, since towns alone find
/// nothing; with a limit, that first million, so museums, sights and
/// stations are found anywhere, and every café and shop only near the
/// towns on the node's About pages ([`WantedPlaces::near`]); with no limit,
/// all of them (about 24 million, 4 GB with the file).
pub fn auto_places(storage_limit_mb: u64) -> u64 {
    match storage_limit_mb {
        0 => u64::MAX,
        mb if mb < 1_000 => 0,
        _ => EVERYWHERE,
    }
}

/// Names the index of `wanted`: changes when the file (size or time), the
/// count or the towns changes, and when what a place is found by
/// ([`plumb_core::place::Place::kind_words`]) changes (the `v`).
pub fn key(wanted: &WantedPlaces) -> String {
    let meta = std::fs::metadata(&wanted.file).ok();
    let modified = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let len = meta.map_or(0, |m| m.len());
    let mut text = format!("v3|{len}:{modified}:{}", wanted.count);
    for (lat, lon) in &wanted.near {
        text.push_str(&format!("|{lat:.2},{lon:.2}"));
    }
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
    read_places_near(path, limit, Vec::new())
}

/// Reads the first `limit` places of the places file `path`, most notable
/// first, then the ones within [`NEAR_KM`] of a point of `near` and the
/// specialty places ([`Place::is_specialty`]: brewpubs, climbing gyms)
/// anywhere.
pub fn read_places_near(
    path: &Path,
    limit: u64,
    near: Vec<(f64, f64)>,
) -> Result<impl Iterator<Item = Place>> {
    let reader = plumb_ingest::open_maybe_gz(path)?;
    let path = path.to_path_buf();
    let mut bad = 0u64;
    let near_empty = near.is_empty();
    Ok(std::io::BufRead::lines(reader)
        .enumerate()
        .map_while(move |(n, line)| match line {
            Ok(line) => Some((n, line)),
            Err(err) => {
                warn!("reading {}: {err}", path.display());
                None
            }
        })
        .filter(|(n, line)| !(line.is_empty() || *n == 0 && line.starts_with("rank\t")))
        .enumerate()
        // Past `limit`, only lines that may be near or a specialty are
        // parsed: most of the file is skipped by its text.
        .filter(move |(kept, (_, line))| {
            (*kept as u64) < limit || !near_empty || may_be_specialty(line.as_bytes())
        })
        .map(|(_, line)| line)
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
        .enumerate()
        .filter(move |(n, place)| {
            (*n as u64) < limit || is_near(place, &near) || place.is_specialty()
        })
        .map(|(_, place)| place))
}

/// Whether a places file line may be a specialty place, by its text alone.
fn may_be_specialty(line: &[u8]) -> bool {
    let has = |needle: &[u8]| line.windows(needle.len()).any(|w| w == needle);
    has(b"craft=brewery") || has(b"sport=")
}

/// Whether `place` is within [`NEAR_KM`] of a point of `near`.
fn is_near(place: &Place, near: &[(f64, f64)]) -> bool {
    near.iter().any(|&(lat, lon)| {
        plumb_core::place::distance_km(place.lat, place.lon, lat, lon) <= NEAR_KM
    })
}

/// Opens the place index for `wanted`, building it first when there is
/// none.
pub fn open_or_build(data_dir: &Path, wanted: &WantedPlaces) -> Result<(String, PlaceSearcher)> {
    let key = key(wanted);
    let dir = index_dir(data_dir, &key);
    if let Ok(searcher) = PlaceSearcher::open(&dir) {
        return Ok((key, searcher));
    }
    let started = std::time::Instant::now();
    info!("indexing places from {}", wanted.file.display());
    let places = read_places_near(&wanted.file, wanted.count, wanted.near.clone())?;
    let stats = build_place_index(&dir, places)?;
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
    let key = key(&WantedPlaces {
        file: file.to_path_buf(),
        count: u64::MAX,
        near: Vec::new(),
    });
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
        assert_eq!(kept(8_000), 1_000_000);
        assert_eq!(kept(100_000), 1_000_000);
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
        assert!(wanted(data, &sets, 0, &[]).is_none());
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
        let wanted = wanted(data, &sets, 0, &[]).unwrap();
        let (key, searcher) = open_or_build(data, &wanted).unwrap();
        assert_eq!(searcher.num_places(), 2);
        let found = searcher
            .search("coffee in denver", None, None, 5)
            .unwrap()
            .unwrap();
        assert_eq!(found.hits[0].place.name, "Huckleberry");
        // Opened again, not rebuilt; other keys are cleared away.
        assert_eq!(open_or_build(data, &wanted).unwrap().0, key);
        let one = WantedPlaces {
            count: 1,
            ..wanted.clone()
        };
        let (other, searcher) = open_or_build(data, &one).unwrap();
        assert_eq!(searcher.num_places(), 1);
        remove_other_indexes(data, Some(&other));
        assert!(!index_dir(data, &key).exists());
        assert!(index_dir(data, &other).exists());
        // Page indexes are not place indexes.
        crate::pages::remove_other_indexes(data, None);
        assert!(index_dir(data, &other).exists());
    }

    #[test]
    fn past_the_first_places_only_those_near_your_towns_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let place = |name: &str, kind: &str, lat: f64, lon: f64, osm: &str| Place {
            rank: place_rank(kind, 0, false, false, 3),
            name: name.into(),
            kind: kind.into(),
            lat,
            lon,
            osm: osm.into(),
            ..Place::default()
        };
        write_places(
            data,
            &[
                place("Denver", "place=city", 39.7392, -104.9903, "n1"),
                place("Paris cafe", "amenity=cafe", 48.85, 2.35, "n2"),
                place("Denver cafe", "amenity=cafe", 39.76, -104.99, "n3"),
                Place {
                    tags: vec!["sport=climbing".into()],
                    ..place("Paris climbing", "leisure=sports_centre", 48.86, 2.35, "n4")
                },
            ],
        );
        let sets = PageSets::parse("places=1").unwrap();
        let denver = [(39.74, -104.99)];
        // With a town, the file keeps the places near it past the first.
        assert_eq!(file_near(1, &denver), &denver[..]);
        assert!(file_near(u64::MAX, &denver).is_empty());
        assert_ne!(near_key(&denver), 0);
        assert_eq!(near_key(&[]), 0);
        let keep = near_lines(denver.to_vec());
        let file = set_info().file(data);
        let lines: Vec<String> =
            std::io::BufRead::lines(plumb_ingest::open_maybe_gz(&file).unwrap())
                .map(Result::unwrap)
                .skip(1)
                .collect();
        let kept: Vec<bool> = lines.iter().map(|l| keep(l.as_bytes())).collect();
        assert_eq!(kept, [true, false, true, true]);
        let near = wanted(data, &sets, 2_000, &denver).unwrap();
        let names: Vec<String> = read_places_near(&near.file, near.count, near.near.clone())
            .unwrap()
            .map(|p| p.name)
            .collect();
        // Climbing gyms are kept anywhere.
        assert_eq!(names, ["Denver", "Denver cafe", "Paris climbing"]);
        let far = wanted(data, &sets, 2_000, &[]).unwrap();
        assert_ne!(key(&near), key(&far));
        // Keeping everything needs no towns.
        assert!(wanted(data, &PageSets::default(), 0, &denver)
            .unwrap()
            .near
            .is_empty());
    }
}
