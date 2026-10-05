//! Places from an OpenStreetMap extract (`.osm.pbf`, the whole planet or a
//! piece of it from Geofabrik): named shops, restaurants, parks, museums,
//! stations, towns and so on (see [`plumb_core::place`]).
//!
//! The file is read up to three times, each time on every core:
//!
//! 1. Every node, way and relation with a name and a tag that makes it a
//!    place. A node has its location; a way's location is the middle of
//!    its first and middle nodes, and a relation's that of its first outer
//!    way, both looked up below.
//! 2. Only when relations were kept: the nodes of their first ways.
//! 3. The locations of the nodes the ways need.
//!
//! Towns (cities, towns and villages) are taken from nodes only, which is
//! where OpenStreetMap puts a town's centre. Then each place gets the town
//! it is in (its address's, or the nearest town's) and each town the
//! country and region most of its places' addresses give.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use osmpbf::{Element, ElementReader, RelMemberType};
use plumb_core::normalize_country;
use plumb_core::place::{
    distance_km, normalize_region, place_rank, Place, MAX_PLACE_ALIASES, MAX_PLACE_NAME_CHARS,
    MAX_WEBSITE_CHARS,
};
use tracing::info;

/// The whole planet, about 90 GB, updated weekly.
pub const PLANET_URL: &str = "https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf";

/// Places farther than this from every town get no town.
const TOWN_REACH_KM: f64 = 25.0;
/// Size of the cells towns are looked up in, in degrees.
const CELL_DEGREES: f64 = 0.1;

/// Values of `amenity` that are not places people search for by name.
const SKIPPED_AMENITIES: &[&str] = &[
    "bench",
    "waste_basket",
    "waste_disposal",
    "recycling",
    "parking_space",
    "parking_entrance",
    "bicycle_parking",
    "motorcycle_parking",
    "vending_machine",
    "post_box",
    "telephone",
    "drinking_water",
    "shelter",
    "clock",
    "hunting_stand",
    "grit_bin",
    "fountain",
    "loading_dock",
    "water_point",
    "bbq",
    "lounger",
    "give_box",
    "letter_box",
    "table",
    "watering_place",
    "toilets",
    "bicycle_repair_station",
    "compressed_air",
    "photo_booth",
    "ticket_validator",
    "parcel_locker",
    "atm",
];
/// Values of `leisure` that are not places people search for by name.
const SKIPPED_LEISURE: &[&str] = &[
    "pitch",
    "track",
    "picnic_table",
    "slipway",
    "firepit",
    "schoolyard",
    "outdoor_seating",
    "bleachers",
    "common",
    "fishing",
    "bird_hide",
    "fitness_station",
    "sports_hall",
];
/// Values of `tourism` that are not places people search for by name.
const SKIPPED_TOURISM: &[&str] = &["information", "artwork", "picnic_site", "yes"];
/// Values of `historic` that are kept.
const KEPT_HISTORIC: &[&str] = &[
    "castle",
    "monument",
    "ruins",
    "fort",
    "palace",
    "battlefield",
    "ship",
    "manor",
    "city_gate",
];
/// Keys whose tag makes a named thing a place, most telling first.
const PLACE_KEYS: &[&str] = &[
    "amenity",
    "shop",
    "tourism",
    "leisure",
    "historic",
    "craft",
    "office",
    "healthcare",
    "aeroway",
    "railway",
];
/// Values of `place` that are towns or parts of them.
const TOWN_VALUES: &[&str] = &[
    "city",
    "town",
    "village",
    "borough",
    "suburb",
    "quarter",
    "neighbourhood",
];

/// What step 1 finds: a place, with its location or what leads to it.
enum Found {
    Located(Place),
    /// A way's place and its first and middle nodes.
    Way(Place, [i64; 2]),
    /// A relation's place and its first outer way.
    Relation(Place, i64),
}

/// The place `tags` make of something named, its location left at 0.
/// `node` is whether it is a node: towns are taken from nodes only.
fn place_of(tags: &[(&str, &str)], osm: String, node: bool) -> Option<Place> {
    let get = |key: &str| {
        tags.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    };
    let name = get("name")?;
    let kind = match get("place") {
        Some(value) if TOWN_VALUES.contains(&value) => {
            if !node {
                return None;
            }
            format!("place={value}")
        }
        _ => {
            let (key, value) = PLACE_KEYS
                .iter()
                .find_map(|key| get(key).map(|value| (*key, value)))?;
            let kept = match key {
                "amenity" => !SKIPPED_AMENITIES.contains(&value),
                "leisure" => !SKIPPED_LEISURE.contains(&value),
                "tourism" => !SKIPPED_TOURISM.contains(&value),
                "historic" => KEPT_HISTORIC.contains(&value),
                "aeroway" => value == "aerodrome",
                "railway" => value == "station",
                _ => value != "no" && value != "vacant",
            };
            if !kept || value.contains(';') {
                return None;
            }
            format!("{key}={value}")
        }
    };
    let mut place_tags = Vec::new();
    if let Some(cuisines) = get("cuisine") {
        for cuisine in cuisines.split(';').map(str::trim).filter(|c| !c.is_empty()) {
            place_tags.push(format!("cuisine={}", cuisine.to_lowercase()));
        }
    }
    for diet in ["vegan", "vegetarian"] {
        if matches!(get(&format!("diet:{diet}")), Some("yes" | "only")) {
            place_tags.push(format!("cuisine={diet}"));
        }
    }
    place_tags.truncate(4);
    let website = ["website", "contact:website", "url", "brand:website"]
        .iter()
        .find_map(|key| get(key).and_then(clean_website));
    let population = get("population")
        .map(|p| p.replace([',', '.', ' '], ""))
        .and_then(|p| p.parse::<u64>().ok())
        .unwrap_or(0);
    let mut aliases = Vec::new();
    let others: &[&str] = if kind.starts_with("place=") {
        &["name:en", "short_name", "alt_name", "official_name"]
    } else {
        &["name:en"]
    };
    for key in others {
        for alias in get(key).unwrap_or("").split(';').map(str::trim) {
            if !alias.is_empty()
                && alias != name
                && !aliases.iter().any(|a: &String| a == alias)
                && aliases.len() < MAX_PLACE_ALIASES
            {
                aliases.push(cut(alias, MAX_PLACE_NAME_CHARS));
            }
        }
    }
    let address = match (get("addr:housenumber"), get("addr:street")) {
        (Some(number), Some(street)) => Some(format!("{number} {street}")),
        (None, Some(street)) => Some(street.to_string()),
        _ => None,
    };
    let rank = place_rank(
        &kind,
        population,
        get("wikidata").is_some(),
        website.is_some() || get("brand").is_some(),
        tags.len(),
    );
    Some(Place {
        rank,
        name: cut(name, MAX_PLACE_NAME_CHARS),
        kind,
        tags: place_tags,
        lat: 0.0,
        lon: 0.0,
        town: get("addr:city").map(|c| cut(c, MAX_PLACE_NAME_CHARS)),
        region: get("addr:state")
            .or_else(|| get("is_in:state_code"))
            .map(normalize_region),
        country: get("addr:country")
            .or_else(|| get("is_in:country_code"))
            .and_then(normalize_country),
        address: address.map(|a| cut(&a, MAX_PLACE_NAME_CHARS)),
        website,
        osm,
        aliases,
    })
}

/// `text` cut to `chars` characters.
fn cut(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// A website as tagged, as a link: `www.example.com` ->
/// `https://www.example.com`. `None` when it is not a web address.
fn clean_website(raw: &str) -> Option<String> {
    let raw = raw.split(';').next()?.trim();
    let url = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else if raw.contains('.') && !raw.contains(' ') && !raw.contains(':') {
        format!("https://{raw}")
    } else {
        return None;
    };
    let parsed = url::Url::parse(&url).ok()?;
    if parsed.host_str().is_none_or(|h| !h.contains('.')) || url.len() > MAX_WEBSITE_CHARS {
        return None;
    }
    Some(url)
}

fn collect_tags<'a>(tags: impl Iterator<Item = (&'a str, &'a str)>) -> Vec<(&'a str, &'a str)> {
    tags.collect()
}

/// Step 1: every place in the file.
fn find_places(path: &Path) -> Result<Vec<Found>> {
    let reader =
        ElementReader::from_path(path).with_context(|| format!("opening {}", path.display()))?;
    let found = reader.par_map_reduce(
        |element| match element {
            Element::DenseNode(node) => {
                let mut tags = node.tags().peekable();
                if tags.peek().is_none() {
                    return Vec::new();
                }
                let tags = collect_tags(tags);
                place_of(&tags, format!("n{}", node.id()), true)
                    .map(|mut place| {
                        place.lat = node.lat();
                        place.lon = node.lon();
                        vec![Found::Located(place)]
                    })
                    .unwrap_or_default()
            }
            Element::Node(node) => {
                let tags = collect_tags(node.tags());
                place_of(&tags, format!("n{}", node.id()), true)
                    .map(|mut place| {
                        place.lat = node.lat();
                        place.lon = node.lon();
                        vec![Found::Located(place)]
                    })
                    .unwrap_or_default()
            }
            Element::Way(way) => {
                let tags = collect_tags(way.tags());
                let Some(place) = place_of(&tags, format!("w{}", way.id()), false) else {
                    return Vec::new();
                };
                refs_of(&way.refs().collect::<Vec<_>>())
                    .map(|refs| vec![Found::Way(place, refs)])
                    .unwrap_or_default()
            }
            Element::Relation(relation) => {
                let tags = collect_tags(relation.tags());
                let Some(place) = place_of(&tags, format!("r{}", relation.id()), false) else {
                    return Vec::new();
                };
                relation
                    .members()
                    .find(|m| {
                        m.member_type == RelMemberType::Way && matches!(m.role(), Ok("outer" | ""))
                    })
                    .map(|m| vec![Found::Relation(place, m.member_id)])
                    .unwrap_or_default()
            }
        },
        Vec::new,
        |mut a, mut b| {
            if a.len() < b.len() {
                std::mem::swap(&mut a, &mut b);
            }
            a.append(&mut b);
            a
        },
    )?;
    Ok(found)
}

/// The first and middle of a way's node ids.
fn refs_of(refs: &[i64]) -> Option<[i64; 2]> {
    Some([*refs.first()?, refs[refs.len() / 2]])
}

/// Step 2: the first and middle nodes of the ways `wanted` (sorted).
fn way_nodes(path: &Path, wanted: &[i64]) -> Result<HashMap<i64, [i64; 2]>> {
    let reader = ElementReader::from_path(path)?;
    let found = reader.par_map_reduce(
        |element| match element {
            Element::Way(way) if wanted.binary_search(&way.id()).is_ok() => {
                refs_of(&way.refs().collect::<Vec<_>>())
                    .map(|refs| vec![(way.id(), refs)])
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        },
        Vec::new,
        |mut a, mut b| {
            a.append(&mut b);
            a
        },
    )?;
    Ok(found.into_iter().collect())
}

/// Step 3: where the nodes `wanted` (sorted) are.
fn node_locations(path: &Path, wanted: &[i64]) -> Result<HashMap<i64, (f64, f64)>> {
    let reader = ElementReader::from_path(path)?;
    let found = reader.par_map_reduce(
        |element| match element {
            Element::DenseNode(node) if wanted.binary_search(&node.id()).is_ok() => {
                vec![(node.id(), (node.lat(), node.lon()))]
            }
            Element::Node(node) if wanted.binary_search(&node.id()).is_ok() => {
                vec![(node.id(), (node.lat(), node.lon()))]
            }
            _ => Vec::new(),
        },
        Vec::new,
        |mut a, mut b| {
            a.append(&mut b);
            a
        },
    )?;
    Ok(found.into_iter().collect())
}

/// Reads the places of the OpenStreetMap file `path`, most notable first.
pub fn read_places(path: &Path) -> Result<Vec<Place>> {
    info!("reading places from {}", path.display());
    let found = find_places(path)?;
    let mut located = Vec::new();
    let mut ways = Vec::new();
    let mut relations = Vec::new();
    for item in found {
        match item {
            Found::Located(place) => located.push(place),
            Found::Way(place, refs) => ways.push((place, refs)),
            Found::Relation(place, way) => relations.push((place, way)),
        }
    }
    info!(
        "found {} places on nodes, {} on ways, {} on relations",
        located.len(),
        ways.len(),
        relations.len()
    );
    if !relations.is_empty() {
        let mut wanted: Vec<i64> = relations.iter().map(|(_, way)| *way).collect();
        wanted.sort_unstable();
        wanted.dedup();
        let nodes = way_nodes(path, &wanted)?;
        for (place, way) in relations {
            if let Some(refs) = nodes.get(&way) {
                ways.push((place, *refs));
            }
        }
    }
    if !ways.is_empty() {
        let mut wanted: Vec<i64> = ways.iter().flat_map(|(_, refs)| *refs).collect();
        wanted.sort_unstable();
        wanted.dedup();
        info!("looking up {} nodes of ways", wanted.len());
        let locations = node_locations(path, &wanted)?;
        for (mut place, refs) in ways {
            let known: Vec<(f64, f64)> = refs
                .iter()
                .filter_map(|id| locations.get(id).copied())
                .collect();
            if known.is_empty() {
                continue;
            }
            place.lat = known.iter().map(|l| l.0).sum::<f64>() / known.len() as f64;
            place.lon = known.iter().map(|l| l.1).sum::<f64>() / known.len() as f64;
            located.push(place);
        }
    }
    fill_in_towns(&mut located);
    located.sort_by(|a, b| {
        b.rank
            .cmp(&a.rank)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.osm.cmp(&b.osm))
    });
    Ok(located)
}

/// Writes `places` (most notable first) to the gzipped places file `path`.
pub fn write_places_file(path: &Path, places: &[Place]) -> Result<()> {
    use std::io::Write;
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut out = flate2::write::GzEncoder::new(
        std::io::BufWriter::new(file),
        flate2::Compression::default(),
    );
    out.write_all(plumb_core::place::PLACES_HEADER.as_bytes())?;
    for place in places {
        plumb_core::place::write_place(&mut out, place)?;
    }
    out.finish()?.flush()?;
    Ok(())
}

/// Towns looked up by where they are.
struct TownGrid {
    cells: HashMap<(i32, i32), Vec<usize>>,
}

fn cell_of(lat: f64, lon: f64) -> (i32, i32) {
    (
        (lat / CELL_DEGREES).floor() as i32,
        (lon / CELL_DEGREES).floor() as i32,
    )
}

impl TownGrid {
    fn new(places: &[Place], towns: impl Iterator<Item = usize>) -> Self {
        let mut cells: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        for i in towns {
            cells
                .entry(cell_of(places[i].lat, places[i].lon))
                .or_default()
                .push(i);
        }
        TownGrid { cells }
    }

    /// The nearest town within `reach` km of `lat`, `lon` for which `ok`
    /// holds.
    fn nearest(
        &self,
        places: &[Place],
        lat: f64,
        lon: f64,
        reach: f64,
        ok: impl Fn(&Place) -> bool,
    ) -> Option<usize> {
        let (row, col) = cell_of(lat, lon);
        let km_per_cell = 111.2 * CELL_DEGREES;
        let rows = (reach / km_per_cell).ceil() as i32;
        let cols = ((reach / (km_per_cell * lat.to_radians().cos().max(0.05))).ceil() as i32)
            .min((360.0 / CELL_DEGREES) as i32);
        let mut best: Option<(f64, usize)> = None;
        for r in row - rows..=row + rows {
            for c in col - cols..=col + cols {
                let Some(towns) = self.cells.get(&(r, c)) else {
                    continue;
                };
                for &i in towns {
                    let town = &places[i];
                    if !ok(town) {
                        continue;
                    }
                    let d = distance_km(lat, lon, town.lat, town.lon);
                    if d <= reach && best.is_none_or(|(b, _)| d < b) {
                        best = Some((d, i));
                    }
                }
            }
        }
        best.map(|(_, i)| i)
    }
}

/// Whether `kind` is a city, town or village, which places belong to.
fn is_settlement(kind: &str) -> bool {
    matches!(kind, "place=city" | "place=town" | "place=village")
}

/// The value most votes went to.
fn winner(votes: &HashMap<String, u32>) -> Option<String> {
    votes
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(value, _)| value.clone())
}

/// Gives towns the country and region their places' addresses mostly
/// give, and places without a town, region or country in their address
/// those of the nearest town.
pub fn fill_in_towns(places: &mut [Place]) {
    let settlements: Vec<usize> = (0..places.len())
        .filter(|&i| is_settlement(&places[i].kind))
        .collect();
    let grid = TownGrid::new(places, settlements.iter().copied());
    let mut countries: HashMap<usize, HashMap<String, u32>> = HashMap::new();
    let mut regions: HashMap<usize, HashMap<String, u32>> = HashMap::new();
    let nearest: Vec<Option<usize>> = places
        .iter()
        .map(|place| {
            if is_settlement(&place.kind) {
                None
            } else {
                grid.nearest(places, place.lat, place.lon, TOWN_REACH_KM, |_| true)
            }
        })
        .collect();
    for (place, town) in places.iter().zip(&nearest) {
        let Some(town) = *town else { continue };
        if let Some(country) = &place.country {
            *countries
                .entry(town)
                .or_default()
                .entry(country.clone())
                .or_default() += 1;
        }
        if let Some(region) = &place.region {
            *regions
                .entry(town)
                .or_default()
                .entry(region.clone())
                .or_default() += 1;
        }
    }
    for &i in &settlements {
        if places[i].country.is_none() {
            places[i].country = countries.get(&i).and_then(winner);
        }
        if places[i].region.is_none() {
            places[i].region = regions.get(&i).and_then(winner);
        }
    }
    // Towns none of whose places had an address: their neighbours'.
    for &i in &settlements {
        if places[i].country.is_some() {
            continue;
        }
        let (lat, lon) = (places[i].lat, places[i].lon);
        if let Some(near) = grid.nearest(places, lat, lon, 4.0 * TOWN_REACH_KM, |t| {
            t.country.is_some()
        }) {
            places[i].country = places[near].country.clone();
            if places[i].region.is_none() {
                places[i].region = places[near].region.clone();
            }
        }
    }
    for (i, town) in nearest.into_iter().enumerate() {
        let Some(town) = town else { continue };
        let (name, region, country) = (
            places[town].name.clone(),
            places[town].region.clone(),
            places[town].country.clone(),
        );
        let place = &mut places[i];
        let same_country = place.country.is_none() || place.country == country;
        if place.town.is_none() && same_country {
            place.town = Some(name);
        }
        if place.region.is_none() && same_country {
            place.region = region;
        }
        if place.country.is_none() {
            place.country = country;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags<'a>(pairs: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        pairs.to_vec()
    }

    #[test]
    fn named_shops_and_towns_are_places() {
        let place = place_of(
            &tags(&[
                ("name", "Pizzeria Locale"),
                ("amenity", "restaurant"),
                ("cuisine", "pizza;italian"),
                ("addr:housenumber", "1730"),
                ("addr:street", "Pearl Street"),
                ("addr:city", "Boulder"),
                ("addr:state", "Colorado"),
                ("website", "www.pizzerialocale.com"),
            ]),
            "n1".into(),
            true,
        )
        .unwrap();
        assert_eq!(place.kind, "amenity=restaurant");
        assert_eq!(place.tags, ["cuisine=pizza", "cuisine=italian"]);
        assert_eq!(place.address.as_deref(), Some("1730 Pearl Street"));
        assert_eq!(place.region.as_deref(), Some("CO"));
        assert_eq!(
            place.website.as_deref(),
            Some("https://www.pizzerialocale.com")
        );
        assert_eq!(place.label(), "Pizza restaurant");
        // Benches and unnamed things are not.
        assert!(place_of(
            &tags(&[("name", "x"), ("amenity", "bench")]),
            "n2".into(),
            true
        )
        .is_none());
        assert!(place_of(&tags(&[("amenity", "cafe")]), "n3".into(), true).is_none());
        // Towns come from nodes only.
        let town = tags(&[
            ("name", "New York"),
            ("place", "city"),
            ("population", "8,804,190"),
            ("name:en", "New York"),
            ("alt_name", "NYC;New York City"),
        ]);
        let city = place_of(&town, "n4".into(), true).unwrap();
        assert_eq!(city.aliases, ["NYC", "New York City"]);
        assert!(city.rank > place.rank);
        assert!(place_of(&town, "r4".into(), false).is_none());
    }

    #[test]
    fn places_get_the_nearest_towns_country() {
        let at = |name: &str, kind: &str, lat: f64, lon: f64| Place {
            name: name.into(),
            kind: kind.into(),
            lat,
            lon,
            ..Place::default()
        };
        let mut places = vec![
            at("Denver", "place=city", 39.7392, -104.9903),
            at("Boulder", "place=city", 40.0150, -105.2705),
            Place {
                country: Some("US".into()),
                region: Some("CO".into()),
                ..at("Shop", "shop=bakery", 39.74, -104.99)
            },
            Place {
                country: Some("US".into()),
                region: Some("CO".into()),
                ..at("Shop 2", "shop=bakery", 39.75, -104.98)
            },
            at("Café", "amenity=cafe", 40.01, -105.27),
            at("Capitol Hill", "place=neighbourhood", 39.73, -104.98),
            at("Far away", "amenity=cafe", 0.0, 0.0),
        ];
        fill_in_towns(&mut places);
        assert_eq!(places[0].country.as_deref(), Some("US"));
        assert_eq!(places[0].region.as_deref(), Some("CO"));
        // Boulder had no addresses: it takes Denver's country.
        assert_eq!(places[1].country.as_deref(), Some("US"));
        assert_eq!(places[2].town.as_deref(), Some("Denver"));
        assert_eq!(places[4].town.as_deref(), Some("Boulder"));
        assert_eq!(places[4].country.as_deref(), Some("US"));
        assert_eq!(places[5].town.as_deref(), Some("Denver"));
        assert_eq!(places[6].town, None);
    }

    #[test]
    fn websites_must_be_web_addresses() {
        assert_eq!(
            clean_website("example.com").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            clean_website("http://a.b/c").as_deref(),
            Some("http://a.b/c")
        );
        assert!(clean_website("mailto:x@y.z").is_none());
        assert!(clean_website("not a site").is_none());
        assert!(clean_website("localhost").is_none());
    }
}
