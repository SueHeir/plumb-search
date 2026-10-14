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
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use osmpbf::{Element, ElementReader, RelMemberType};
use plumb_core::article::articles_of;
use plumb_core::place::{
    brews_where_served, distance_km, landmark_rank, normalize_region, place_rank, Place,
    BREWERY_TAG, CANADIAN_PROVINCES, MAX_PLACE_ALIASES, MAX_PLACE_NAME_CHARS, MAX_WEBSITE_CHARS,
    US_STATES,
};
use plumb_core::{joined, normalize_country};
use plumb_ingest::open_maybe_gz;
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
const SKIPPED_TOURISM: &[&str] = &["information", "artwork", "picnic_site", "camp_pitch", "yes"];
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
    "boundary",
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

/// Most other names taken from a place's own tags; the rest of
/// [`MAX_PLACE_ALIASES`] is left for its Wikipedia article's.
const TAGGED_ALIASES: usize = 4;
/// Most outer ways of a relation whose ends say where it is, spread over
/// all of them.
const OUTER_WAYS: usize = 32;
/// A Wikidata item tagged on more places than this is a brand or a
/// mistake, not any one of them, and lends them no names.
const PLACES_PER_ITEM: u32 = 3;
/// Every US state and Canadian province lies west of this longitude.
const AMERICAS_EAST_LON: f64 = -50.0;
/// The kind of a state or province read for its ISO 3166-2 code.
const STATE_KIND: &str = "boundary=administrative";
/// Farther than the middle of any state is from its edge (Alaska's is
/// about 2,000 km from Attu).
const STATE_REACH_KM: f64 = 2_500.0;

/// What step 1 finds: a place, its Wikidata item, and where it is or what
/// leads there.
struct Found {
    place: Place,
    item: Option<u64>,
    at: At,
}

enum At {
    /// A node, whose location the place has.
    Node,
    /// A way's first and middle nodes.
    Way([i64; 2]),
    /// A relation's outer ways.
    Relation(Vec<i64>),
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
    // Closed for good, but still mapped.
    let lower = name.to_lowercase();
    if lower.starts_with("(closed")
        || lower.starts_with("closed ")
        || lower.ends_with("(closed)")
        || get("opening_hours") == Some("closed")
        || get("end_date").is_some()
    {
        return None;
    }
    // A state or province, which says what country its code is in: WA is
    // Washington's and Western Australia's. It is not kept as a place.
    let code = get("ISO3166-2").filter(|_| !node && get("boundary") == Some("administrative"));
    if let Some((country, _)) = code.and_then(|code| code.split_once('-')) {
        // A territory with a country code of its own is in it: US-PR is
        // Puerto Rico, PR.
        let own = get("ISO3166-1:alpha2").or_else(|| get("ISO3166-1"));
        return Some(Place {
            name: cut(name, MAX_PLACE_NAME_CHARS),
            kind: STATE_KIND.to_string(),
            region: code.map(str::to_string),
            country: Some(
                own.and_then(normalize_country)
                    .or_else(|| normalize_country(country))?,
            ),
            osm,
            ..Place::default()
        });
    }
    let kind = match get("place") {
        Some(value) if TOWN_VALUES.contains(&value) => {
            if !node {
                return None;
            }
            format!("place={value}")
        }
        // An island is searched around ("hotels in maui") when Wikidata
        // knows it; there are tens of thousands of nameless rocks.
        Some(value @ ("island" | "archipelago")) if get("wikidata").is_some() => {
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
                "boundary" => value == "national_park",
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
    // A pub that brews its own ("brewery in denver"), and what a sports
    // centre is for ("climbing gym").
    if brews_where_served(&kind)
        && (get("craft") == Some("brewery") || get("microbrewery") == Some("yes"))
    {
        place_tags.push(BREWERY_TAG.to_string());
    }
    if let Some(sports) = get("sport") {
        for sport in sports
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .take(2)
        {
            place_tags.push(format!("sport={}", sport.to_lowercase()));
        }
    }
    let country = get("addr:country")
        .or_else(|| get("is_in:country_code"))
        .and_then(normalize_country);
    // Canada's addresses give the province as addr:province, and so do
    // Italy's (MI, PE), whose would be taken for states and provinces.
    let province = get("addr:province")
        .map(normalize_region)
        .filter(|region| CANADIAN_PROVINCES.iter().any(|(code, _)| code == region))
        .filter(|_| country.as_deref().is_none_or(|country| country == "CA"));
    let website = ["website", "contact:website", "url", "brand:website"]
        .iter()
        .find_map(|key| get(key).and_then(clean_website));
    let population = get("population")
        .map(|p| p.replace([',', '.', ' '], ""))
        .and_then(|p| p.parse::<u64>().ok())
        .unwrap_or(0);
    let mut aliases = Vec::new();
    // What people call it there ("Philly"), and an airport's code ("JFK").
    let others: &[&str] = match kind.as_str() {
        k if k.starts_with("place=") => &[
            "name:en",
            "short_name",
            "loc_name",
            "alt_name",
            "official_name",
        ],
        "aeroway=aerodrome" => &["name:en", "short_name", "loc_name", "iata"],
        _ => &["name:en", "short_name", "loc_name"],
    };
    for key in others {
        for alias in get(key).unwrap_or("").split(';') {
            push_alias(name, &mut aliases, alias, TAGGED_ALIASES);
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
            .map(normalize_region)
            .or(province),
        country,
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

/// Adds `alias` to the other names of the place called `name`, unless it
/// is that name or one of them written another way ("LasVegas"), names
/// nothing in letters (a redirect from "10.57232"), or `most` are kept.
fn push_alias(name: &str, aliases: &mut Vec<String>, alias: &str, most: usize) {
    let key = joined(alias);
    if key.contains(char::is_alphabetic)
        && aliases.len() < most.min(MAX_PLACE_ALIASES)
        && key != joined(name)
        && !aliases.iter().any(|a| joined(a) == key)
    {
        aliases.push(cut(alias.trim(), MAX_PLACE_NAME_CHARS));
    }
}

/// The number of the Wikidata item `tags` name: 41225 for `Q41225`. None
/// when it is the item of the place's brand, which names no one shop.
fn item_of(tags: &[(&str, &str)]) -> Option<u64> {
    let tag = |key: &str| tags.iter().find(|(k, _)| *k == key).map(|(_, v)| v.trim());
    let item = tag("wikidata")?;
    if tag("brand:wikidata") == Some(item) {
        return None;
    }
    item.strip_prefix('Q')?.parse().ok()
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
    let mut url = url;
    let mut parsed = url::Url::parse(&url).ok()?;
    // A link copied from Google's results: the address it leads to.
    if parsed
        .host_str()
        .is_some_and(|h| h.starts_with("google.") || h.contains(".google."))
    {
        let target = parsed
            .query_pairs()
            .find(|(k, _)| k == "url" || k == "q")
            .map(|(_, v)| v.into_owned())?;
        parsed = url::Url::parse(&target).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str()?.contains("google.") {
            return None;
        }
        url = parsed.to_string();
    }
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
    // Where a place is is looked for only once its tags make it one.
    let found_at = |tags: &[(&str, &str)], osm: String, node: bool, at: &dyn Fn() -> Option<At>| {
        let place = place_of(tags, osm, node)?;
        Some(Found {
            place,
            item: item_of(tags),
            at: at()?,
        })
    };
    let found = reader.par_map_reduce(
        |element| match element {
            Element::DenseNode(node) => {
                let mut tags = node.tags().peekable();
                if tags.peek().is_none() {
                    return Vec::new();
                }
                let tags = collect_tags(tags);
                found_at(&tags, format!("n{}", node.id()), true, &|| Some(At::Node))
                    .map(|mut found| {
                        found.place.lat = node.lat();
                        found.place.lon = node.lon();
                        vec![found]
                    })
                    .unwrap_or_default()
            }
            Element::Node(node) => {
                let tags = collect_tags(node.tags());
                found_at(&tags, format!("n{}", node.id()), true, &|| Some(At::Node))
                    .map(|mut found| {
                        found.place.lat = node.lat();
                        found.place.lon = node.lon();
                        vec![found]
                    })
                    .unwrap_or_default()
            }
            Element::Way(way) => {
                let tags = collect_tags(way.tags());
                let refs = || refs_of(&way.refs().collect::<Vec<_>>()).map(At::Way);
                found_at(&tags, format!("w{}", way.id()), false, &refs)
                    .into_iter()
                    .collect()
            }
            Element::Relation(relation) => {
                let tags = collect_tags(relation.tags());
                // A park's boundary is many ways: their ends all round it
                // say where its middle is, where one way's would not.
                let outer = || {
                    let outer: Vec<i64> = relation
                        .members()
                        .filter(|m| {
                            m.member_type == RelMemberType::Way
                                && matches!(m.role(), Ok("outer" | ""))
                        })
                        .map(|m| m.member_id)
                        .collect();
                    let step = outer.len().div_ceil(OUTER_WAYS).max(1);
                    (!outer.is_empty())
                        .then(|| At::Relation(outer.into_iter().step_by(step).collect()))
                };
                found_at(&tags, format!("r{}", relation.id()), false, &outer)
                    .into_iter()
                    .collect()
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

/// Reads the places of the OpenStreetMap files `paths` (extracts may
/// overlap), named also as the English Wikipedia articles in the articles
/// file `articles` name their Wikidata items, most notable first.
pub fn read_places(paths: &[PathBuf], articles: Option<&Path>) -> Result<Vec<Place>> {
    // A missing articles file fails the build before the extracts are read.
    if let Some(articles) = articles {
        open_maybe_gz(articles)?;
    }
    let mut found = Vec::new();
    for path in paths {
        found.extend(locate_places(path)?);
    }
    if paths.len() > 1 {
        // A place in two extracts once.
        found.sort_by(|a, b| a.0.osm.cmp(&b.0.osm));
        found.dedup_by(|a, b| a.0.osm == b.0.osm);
    }
    let (states, mut found): (Vec<_>, Vec<_>) = found
        .into_iter()
        .partition(|(place, _)| place.kind == STATE_KIND);
    if let Some(articles) = articles {
        add_article_names(&mut found, articles)?;
    }
    let mut located: Vec<Place> = found.into_iter().map(|(place, _)| place).collect();
    let states: Vec<Place> = states.into_iter().map(|(state, _)| state).collect();
    fill_in_towns(&mut located, &states);
    located.sort_by(|a, b| {
        b.rank
            .cmp(&a.rank)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.osm.cmp(&b.osm))
    });
    Ok(located)
}

/// The places of the OpenStreetMap file `path`, each with its Wikidata
/// item, where they are.
fn locate_places(path: &Path) -> Result<Vec<(Place, Option<u64>)>> {
    info!("reading places from {}", path.display());
    let found = find_places(path)?;
    let mut located = Vec::new();
    let mut ways = Vec::new();
    let mut relations = Vec::new();
    for Found { place, item, at } in found {
        match at {
            At::Node => located.push((place, item)),
            At::Way(refs) => ways.push((place, item, refs)),
            At::Relation(outer) => relations.push((place, item, outer)),
        }
    }
    info!(
        "found {} places on nodes, {} on ways, {} on relations",
        located.len(),
        ways.len(),
        relations.len()
    );
    if !relations.is_empty() {
        let mut wanted: Vec<i64> = relations.iter().flat_map(|r| r.2.iter().copied()).collect();
        wanted.sort_unstable();
        wanted.dedup();
        let nodes = way_nodes(path, &wanted)?;
        // Each relation's outer ways, from here on the ends of those ways.
        for (_, _, outer) in &mut relations {
            *outer = outer
                .iter()
                .filter_map(|way| nodes.get(way))
                .flatten()
                .copied()
                .collect();
        }
    }
    let mut wanted: Vec<i64> = ways
        .iter()
        .flat_map(|w| w.2)
        .chain(relations.iter().flat_map(|r| r.2.iter().copied()))
        .collect();
    if wanted.is_empty() {
        return Ok(located);
    }
    wanted.sort_unstable();
    wanted.dedup();
    info!("looking up {} nodes of ways", wanted.len());
    let locations = node_locations(path, &wanted)?;
    let mut place_at = |mut place: Place, item, refs: &[i64]| {
        let known: Vec<(f64, f64)> = refs
            .iter()
            .filter_map(|id| locations.get(id).copied())
            .collect();
        if let Some((lat, lon)) = middle(&known) {
            place.lat = lat;
            place.lon = lon;
            located.push((place, item));
        }
    };
    for (place, item, refs) in ways {
        place_at(place, item, &refs);
    }
    for (place, item, ends) in relations {
        place_at(place, item, &ends);
    }
    Ok(located)
}

/// The middle of the box around `points`, or of the box across the 180th
/// meridian when that one is narrower (the Aleutians).
fn middle(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let bounds = |shift: f64| {
        points
            .iter()
            .map(|&(lat, lon)| (lat, if lon < 0.0 { lon + shift } else { lon }))
            .fold(None, |bounds: Option<[f64; 4]>, (lat, lon)| {
                let [south, west, north, east] = bounds.unwrap_or([lat, lon, lat, lon]);
                Some([south.min(lat), west.min(lon), north.max(lat), east.max(lon)])
            })
    };
    let mut found = bounds(0.0)?;
    if found[3] - found[1] > 180.0 {
        found = bounds(360.0)?;
    }
    let [south, west, north, east] = found;
    let lon = (west + east) / 2.0;
    Some((
        (south + north) / 2.0,
        if lon > 180.0 { lon - 360.0 } else { lon },
    ))
}

/// The places of one Wikidata item: how many, whether one is a town, and
/// the best known of those that are not, the one that can be its landmark.
#[derive(Default)]
struct ItemPlaces {
    places: u32,
    town: bool,
    landmark: Option<usize>,
}

/// The names and reads of an English Wikipedia article.
struct ArticleNames {
    title: String,
    views: u64,
    aliases: Vec<String>,
}

/// Whether `name` is plain: "Big Ben", not "Louvre (museum)" or "Paris,
/// Texas", which name it only with a qualifier.
fn is_plain(name: &str) -> bool {
    !name.contains(['(', ','])
}

/// The state or province, and its country, that the title of a North
/// American town's article names: ("ON", "CA") for "Niagara Falls,
/// Ontario". The town at `lon` must be in the Americas: "Gori, Georgia" is
/// the Georgian town.
fn titled_region(title: &str, lon: f64) -> Option<(String, &'static str)> {
    let (_, qualifier) = title.rsplit_once(", ")?;
    let region = normalize_region(qualifier);
    let is = |table: &[(&str, &str)]| table.iter().any(|(code, _)| *code == region);
    let country = if is(US_STATES) {
        "US"
    } else if is(CANADIAN_PROVINCES) {
        "CA"
    } else {
        return None;
    };
    (lon < AMERICAS_EAST_LON).then_some((region, country))
}

/// Gives each place the plain title and other names (redirects) of the
/// English Wikipedia article on its Wikidata item, from the articles file
/// `path`: "Big Ben" for the Elizabeth Tower, "Philly" for Philadelphia.
/// The best known place of an item that is no town, and that the title
/// names plainly, is a landmark ([`landmark_rank`]). A place with its
/// town's item (a town hall) takes nothing from it.
fn add_article_names(found: &mut [(Place, Option<u64>)], path: &Path) -> Result<()> {
    let mut places_of: HashMap<u64, ItemPlaces> = HashMap::new();
    for (i, (place, item)) in found.iter().enumerate() {
        let Some(item) = item else { continue };
        let of = places_of.entry(*item).or_default();
        of.places += 1;
        of.town |= place.is_town();
        if !place.is_town()
            && of
                .landmark
                .is_none_or(|best| found[best].0.rank < place.rank)
        {
            of.landmark = Some(i);
        }
    }
    places_of.retain(|_, of| of.places <= PLACES_PER_ITEM);
    info!("reading the articles on {} items", places_of.len());
    let mut failed = None;
    let lines = open_maybe_gz(path)?.lines().map_while(|line| match line {
        Ok(line) => Some(line),
        Err(err) => {
            failed = Some(err);
            None
        }
    });
    let mut articles = HashMap::new();
    for (_, article) in articles_of(lines) {
        // A line the file's writer got wrong names nothing.
        let Ok(article) = article else { continue };
        let Some(item) = article
            .item
            .as_deref()
            .and_then(|item| item.strip_prefix('Q')?.parse::<u64>().ok())
            .filter(|item| places_of.contains_key(item))
        else {
            continue;
        };
        articles.insert(
            item,
            ArticleNames {
                title: article.title,
                views: article.views,
                aliases: article.aliases,
            },
        );
    }
    if let Some(err) = failed {
        return Err(anyhow::Error::new(err).context(format!("reading {}", path.display())));
    }
    let mut landmarks = 0;
    for (i, (place, item)) in found.iter_mut().enumerate() {
        let Some((article, of)) =
            item.and_then(|item| Some((articles.get(&item)?, &places_of[&item])))
        else {
            continue;
        };
        if of.town && !place.is_town() {
            continue;
        }
        // A town's article says which side of a border it is on, where
        // its places' addresses may not (both Niagara Falls).
        if is_settlement(&place.kind) && place.region.is_none() {
            if let Some((region, country)) = titled_region(&article.title, place.lon) {
                place.region = Some(region);
                place.country.get_or_insert_with(|| country.to_string());
            }
        }
        let names = std::iter::once(&article.title).chain(&article.aliases);
        for name in names.filter(|name| is_plain(name)) {
            push_alias(&place.name, &mut place.aliases, name, MAX_PLACE_ALIASES);
        }
        if of.landmark == Some(i) && is_plain(&article.title) {
            place.rank = landmark_rank(article.views);
            landmarks += 1;
        }
    }
    info!(
        "{} places have an article, {landmarks} of them landmarks",
        found
            .iter()
            .filter(|(_, item)| item.is_some_and(|item| articles.contains_key(&item)))
            .count()
    );
    Ok(())
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

/// Gives towns that know their region but no country the country of the
/// nearest of `states` with that code, and takes from towns that know
/// their country a region no state of it has: Rainy River, Ontario, voted
/// Minnesota's by shops across the river, has no region rather than MN.
fn countries_from_states(places: &mut [Place], settlements: &[usize], states: &[Place]) {
    // A state's region is its whole code; it is looked up by the part
    // after the country's: "WA" for AU-WA and US-WA.
    fn split(state: &Place) -> Option<(&str, &str)> {
        state.region.as_deref()?.split_once('-')
    }
    let mut coded: HashMap<String, Vec<&Place>> = HashMap::new();
    for state in states {
        if let Some((_, code)) = split(state) {
            coded.entry(code.to_uppercase()).or_default().push(state);
        }
    }
    for &i in settlements {
        let town = &places[i];
        let Some(states) = town
            .region
            .as_ref()
            .and_then(|r| coded.get(&r.to_uppercase()))
        else {
            continue;
        };
        match &town.country {
            None => {
                places[i].country = states
                    .iter()
                    .map(|state| (distance_km(town.lat, town.lon, state.lat, state.lon), state))
                    .filter(|(km, _)| *km < STATE_REACH_KM)
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .and_then(|(_, state)| state.country.clone());
            }
            // A territory's state is its own country's and its parent's:
            // US-PR is Puerto Rico's and the US's.
            Some(country) => {
                let ours = |state: &&Place| {
                    state.country.as_ref() == Some(country)
                        || split(state).is_some_and(|(parent, _)| parent == country)
                };
                if !states.iter().any(ours) {
                    places[i].region = None;
                }
            }
        }
    }
}

/// Gives towns that still have no country their neighbours', in their
/// region when one there has it. They are filled in rounds, each from
/// towns that knew theirs before it, towns that know their region first,
/// so a town takes the side of the nearest town that knows before a guess
/// can reach it from across a border; where none knows, the guess still
/// goes on from town to town (northern Ontario).
fn lend_countries(places: &mut [Place], grid: &TownGrid, settlements: &[usize]) {
    let reach = 4.0 * TOWN_REACH_KM;
    let mut unknown: Vec<usize> = settlements
        .iter()
        .copied()
        .filter(|&i| places[i].country.is_none())
        .collect();
    loop {
        let before = unknown.len();
        for with_region in [true, false] {
            let lent: Vec<(usize, usize)> = unknown
                .iter()
                .filter(|&&i| places[i].region.is_some() == with_region)
                .filter_map(|&i| {
                    let (lat, lon, region) = (places[i].lat, places[i].lon, &places[i].region);
                    let near =
                        |ok: &dyn Fn(&Place) -> bool| grid.nearest(places, lat, lon, reach, ok);
                    near(&|t| {
                        t.country.is_some()
                            && t.region.is_some()
                            && (region.is_none() || t.region == *region)
                    })
                    .or_else(|| near(&|t| t.country.is_some()))
                    .map(|near| (i, near))
                })
                .collect();
            for (i, near) in lent {
                places[i].country = places[near].country.clone();
                if places[i].region.is_none() {
                    places[i].region = places[near].region.clone();
                }
            }
        }
        unknown.retain(|&i| places[i].country.is_none());
        if unknown.len() == before {
            break;
        }
    }
}

/// Gives towns the region their places' addresses mostly give, and the
/// country those in that region mostly give, and places without a town,
/// region or country in their address those of the nearest town that
/// fits them. Across a border, the places on the other side say another
/// region: both Niagara Falls are nearest to shops of both. A town whose
/// places give no country takes that of the nearest of `states` with its
/// region's code, since Australia's addresses give none.
pub fn fill_in_towns(places: &mut [Place], states: &[Place]) {
    let settlements: Vec<usize> = (0..places.len())
        .filter(|&i| is_settlement(&places[i].kind))
        .collect();
    let grid = TownGrid::new(places, settlements.iter().copied());
    type Said = (Option<String>, Option<String>);
    let mut votes: HashMap<usize, HashMap<Said, u32>> = HashMap::new();
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
        if place.country.is_some() || place.region.is_some() {
            *votes
                .entry(town)
                .or_default()
                .entry((place.country.clone(), place.region.clone()))
                .or_default() += 1;
        }
    }
    for &i in &settlements {
        let Some(said) = votes.get(&i) else { continue };
        if places[i].region.is_none() {
            let mut regions = HashMap::new();
            for ((_, region), n) in said {
                if let Some(region) = region {
                    *regions.entry(region.clone()).or_default() += n;
                }
            }
            places[i].region = winner(&regions);
        }
        if places[i].country.is_none() {
            let region = places[i].region.clone();
            let mut countries = HashMap::new();
            for ((country, place_region), n) in said {
                if let Some(country) = country {
                    if place_region.is_none() || *place_region == region {
                        *countries.entry(country.clone()).or_default() += n;
                    }
                }
            }
            places[i].country = winner(&countries);
        }
    }
    // Towns none of whose places gave a country: a neighbour's that knows
    // its own, and where none does, the country of the nearest state with
    // the town's region's code, which near a border can be the other
    // side's (Rainy River, Ontario is nearest to shops in Minnesota). The
    // towns those fill lend in turn.
    lend_countries(places, &grid, &settlements);
    countries_from_states(places, &settlements, states);
    lend_countries(places, &grid, &settlements);
    // The country most towns of each region are in.
    let mut countries: HashMap<&str, HashMap<String, u32>> = HashMap::new();
    for &i in &settlements {
        if let (Some(region), Some(country)) = (&places[i].region, &places[i].country) {
            *countries
                .entry(region)
                .or_default()
                .entry(country.clone())
                .or_default() += 1;
        }
    }
    let region_country: HashMap<String, String> = countries
        .iter()
        .filter_map(|(region, votes)| Some((region.to_string(), winner(votes)?)))
        .collect();
    for (i, town) in nearest.into_iter().enumerate() {
        let Some(mut town) = town else { continue };
        let (lat, lon) = (places[i].lat, places[i].lon);
        let (country, region) = (places[i].country.clone(), places[i].region.clone());
        let fits = |t: &Place| {
            (country.is_none() || t.country == country) && (region.is_none() || t.region == region)
        };
        // A place across a border from its nearest town, by its country
        // or else its region's: the nearest town on its own side.
        let side = country
            .clone()
            .or_else(|| region_country.get(region.as_deref()?).cloned());
        if !fits(&places[town]) && side.is_some() && side != places[town].country {
            let ours = |t: &Place| t.country == side && (region.is_none() || t.region == region);
            town = grid
                .nearest(places, lat, lon, TOWN_REACH_KM, ours)
                .unwrap_or(town);
        }
        let (name, town_region, town_country) = (
            places[town].name.clone(),
            places[town].region.clone(),
            places[town].country.clone(),
        );
        let place = &mut places[i];
        let same_country = place.country.is_none() || place.country == town_country;
        if place.town.is_none() && same_country {
            place.town = Some(name);
        }
        if place.region.is_none() && same_country {
            place.region = town_region;
        }
        if place.country.is_none() {
            place.country = town_country;
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
        assert!(place_of(
            &tags(&[("name", "(closed) Lowdown Brewery"), ("craft", "brewery")]),
            "n5".into(),
            true
        )
        .is_none());
        // A pub that brews, and a climbing gym, are found by those words.
        let pub_ = place_of(
            &tags(&[
                ("name", "Wynkoop"),
                ("amenity", "pub"),
                ("microbrewery", "yes"),
            ]),
            "n6".into(),
            true,
        )
        .unwrap();
        assert!(pub_.tags.contains(&"craft=brewery".to_string()));
        assert!(pub_.kind_words().to_lowercase().contains("brewery"));
        let gym = place_of(
            &tags(&[
                ("name", "Vertical World"),
                ("leisure", "sports_centre"),
                ("sport", "climbing;fitness"),
            ]),
            "n7".into(),
            true,
        )
        .unwrap();
        assert!(gym.kind_words().contains("climbing"));
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
        fill_in_towns(&mut places, &[]);
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
    fn places_keep_what_people_call_them() {
        let airport = place_of(
            &tags(&[
                ("name", "John F. Kennedy International Airport"),
                ("aeroway", "aerodrome"),
                ("iata", "JFK"),
                ("loc_name", "JFK"),
                ("wikidata", "Q8685"),
            ]),
            "w1".into(),
            false,
        )
        .unwrap();
        assert_eq!(airport.aliases, ["JFK"]);
        assert_eq!(item_of(&tags(&[("wikidata", "Q8685")])), Some(8685));
        // A shop tagged with its brand's item is not the brand.
        let brand = [("wikidata", "Q37158"), ("brand:wikidata", "Q37158")];
        assert_eq!(item_of(&tags(&brand)), None);
        let city = place_of(
            &tags(&[
                ("name", "Philadelphia"),
                ("place", "city"),
                ("loc_name", "Philly"),
            ]),
            "n1".into(),
            true,
        )
        .unwrap();
        assert_eq!(city.aliases, ["Philly"]);
        // Canada's addresses say the province.
        let shop = place_of(
            &tags(&[
                ("name", "Tim Hortons"),
                ("amenity", "cafe"),
                ("addr:province", "Ontario"),
            ]),
            "n2".into(),
            true,
        )
        .unwrap();
        assert_eq!(shop.region.as_deref(), Some("ON"));
        // Italy's provinces are no states.
        let bar = place_of(
            &tags(&[
                ("name", "Bar Roma"),
                ("amenity", "bar"),
                ("addr:province", "MI"),
            ]),
            "n3".into(),
            true,
        )
        .unwrap();
        assert_eq!(bar.region, None);
        // A national park mapped only as its boundary is a park; other
        // boundaries are no places.
        let park = &[
            ("name", "Yosemite National Park"),
            ("boundary", "national_park"),
        ];
        let park = place_of(&tags(park), "r1".into(), false).unwrap();
        assert_eq!(park.kind, "boundary=national_park");
        let county = &[("name", "Mariposa County"), ("boundary", "administrative")];
        assert!(place_of(&tags(county), "r2".into(), false).is_none());
        // So is an island Wikidata knows, however it is mapped.
        let maui = &[
            ("name", "Maui"),
            ("place", "island"),
            ("wikidata", "Q183222"),
        ];
        assert_eq!(
            place_of(&tags(maui), "r3".into(), false).unwrap().kind,
            "place=island"
        );
        let rock = &[("name", "Seal Rock"), ("place", "island")];
        assert!(place_of(&tags(rock), "w4".into(), false).is_none());
    }

    #[test]
    fn places_take_names_from_their_articles() {
        let at = |name: &str, kind: &str, osm: &str| Place {
            rank: place_rank(kind, 0, true, false, 5),
            name: name.into(),
            kind: kind.into(),
            osm: osm.into(),
            ..Place::default()
        };
        let mut found = vec![
            (
                at("Elizabeth Tower", "tourism=attraction", "w1"),
                Some(41225),
            ),
            (at("Big Ben", "amenity=bar", "n2"), None),
            (
                Place {
                    rank: place_rank("place=city", 1_600_000, true, false, 5),
                    ..at("Philadelphia", "place=city", "n3")
                },
                Some(1345),
            ),
            (at("Café Louvre", "amenity=cafe", "n4"), Some(7)),
            // A brand's item on its shops names none of them.
            (at("Starbucks", "amenity=cafe", "n5"), Some(37158)),
            (at("Starbucks", "amenity=cafe", "n6"), Some(37158)),
            (at("Starbucks", "amenity=cafe", "n7"), Some(37158)),
            (at("Starbucks", "amenity=cafe", "n8"), Some(37158)),
            // The tower mapped twice, and a town hall with its town's item.
            (
                at("Elizabeth Tower", "tourism=attraction", "n9"),
                Some(41225),
            ),
            (at("City Hall", "amenity=townhall", "n10"), Some(1345)),
            // Towns on two sides of a border, and a namesake overseas.
            (
                Place {
                    lon: -79.08,
                    ..at("Niagara Falls", "place=city", "n11")
                },
                Some(274120),
            ),
            (
                Place {
                    lon: 44.11,
                    ..at("Gori", "place=town", "n12")
                },
                Some(1003),
            ),
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wikipedia-en.tsv");
        std::fs::write(
            &path,
            format!(
                "{}{}{}{}{}{}{}{}",
                plumb_core::article::ARTICLES_HEADER,
                "a line with no columns\n",
                "90000\tBig Ben\tclock tower\tQ41225\t\tElizabeth Tower|BigBen|10.57232|Clock Tower (Westminster)|Big Ben, London\n",
                "80000\tPhiladelphia\tcity\tQ1345\t\tPhilly|Philadelphia, Pennsylvania|City of Brotherly Love\n",
                "20\tCafé Louvre (Prague)\tcafé\tQ7\t\tCafe Louvre\n",
                "70000\tStarbucks\tcoffeehouse chain\tQ37158\t\t\n",
                "2293\tNiagara Falls, Ontario\tcity\tQ274120\t\tNiagara Falls, ON|Crowland Township\n",
                "900\tGori, Georgia\ttown\tQ1003\t\t\n",
            ),
        )
        .unwrap();
        add_article_names(&mut found, &path).unwrap();
        // Its name again, a name written another way, a redirect with no
        // letters and qualified titles add nothing.
        let tower = &found[0].0;
        assert_eq!(tower.aliases, ["Big Ben"]);
        assert!(tower.is_landmark());
        assert!(tower.rank > found[1].0.rank);
        let city = &found[2].0;
        assert_eq!(city.aliases, ["Philly", "City of Brotherly Love"]);
        assert!(city.is_town() && !city.is_landmark());
        // A title with a qualifier names nothing plainly: no landmark.
        let cafe = &found[3].0;
        assert_eq!(cafe.aliases, ["Cafe Louvre"]);
        assert!(!cafe.is_landmark());
        assert!(found[4].0.aliases.is_empty() && !found[4].0.is_landmark());
        // One landmark a name, and the town's names stay the town's.
        assert_eq!(found[8].0.aliases, ["Big Ben"]);
        assert!(!found[8].0.is_landmark());
        assert!(found[9].0.aliases.is_empty() && !found[9].0.is_landmark());
        // A North American town's title says its province.
        let falls = &found[10].0;
        assert_eq!(falls.region.as_deref(), Some("ON"));
        assert_eq!(falls.country.as_deref(), Some("CA"));
        assert_eq!(falls.aliases, ["Crowland Township"]);
        assert_eq!(found[11].0.region, None);
    }

    #[test]
    fn a_relation_is_where_the_middle_of_its_ways_is() {
        let (lat, lon) = middle(&[(37.5, -119.9), (38.2, -119.6), (37.9, -119.2)]).unwrap();
        assert!((lat - 37.85).abs() < 1e-9 && (lon + 119.55).abs() < 1e-9);
        assert!(middle(&[]).is_none());
        // An island across the 180th meridian is in the Pacific, not at 0.
        let (_, lon) = middle(&[(51.5, 179.0), (52.0, -177.0)]).unwrap();
        assert!((lon + 179.0).abs() < 1e-9);
    }

    #[test]
    fn border_towns_keep_their_own_side() {
        let at = |name: &str, kind: &str, lat: f64, lon: f64| Place {
            name: name.into(),
            kind: kind.into(),
            lat,
            lon,
            ..Place::default()
        };
        let shop = |lat: f64, lon: f64, country: Option<&str>, region: &str| Place {
            country: country.map(Into::into),
            region: Some(region.into()),
            ..at("Shop", "shop=gift", lat, lon)
        };
        // Niagara Falls, New York and Ontario, 2.5 km apart across the
        // river, and Buffalo, whose shops say they are in the US.
        let mut places = vec![
            at("Niagara Falls", "place=city", 43.0844, -79.0615),
            at("Niagara Falls", "place=city", 43.1066, -79.0639),
            at("Buffalo", "place=city", 42.8864, -78.8784),
            shop(42.89, -78.88, Some("US"), "NY"),
        ];
        // Each side's shops give the province or state; Canada's some
        // the country too, and a few are nearer the other side's centre.
        for i in 0..6 {
            let d = f64::from(i) * 0.001;
            places.push(shop(43.083 + d, -79.058, None, "NY"));
            places.push(shop(43.108 + d, -79.07, Some("CA"), "ON"));
        }
        places.push(shop(43.088, -79.075, Some("CA"), "ON"));
        places.push(shop(43.089, -79.074, Some("CA"), "ON"));
        places.push(shop(43.100, -79.058, None, "NY"));
        fill_in_towns(&mut places, &[]);
        let said = |i: usize| (places[i].region.clone(), places[i].country.clone());
        assert_eq!(said(0), (Some("NY".into()), Some("US".into())));
        assert_eq!(said(1), (Some("ON".into()), Some("CA".into())));
        // A New York shop nearer Ontario's centre is still in the US.
        let last = places.len() - 1;
        assert_eq!(places[last].country.as_deref(), Some("US"));
        assert_eq!(places[last].town.as_deref(), Some("Niagara Falls"));
    }

    #[test]
    fn a_town_takes_its_side_from_a_neighbour_that_knows_it() {
        let town =
            |name: &str, lat: f64, lon: f64, region: Option<&str>, country: Option<&str>| Place {
                name: name.into(),
                kind: "place=town".into(),
                lat,
                lon,
                region: region.map(Into::into),
                country: country.map(Into::into),
                ..Place::default()
            };
        // A village none of whose places say where it is, nearer a New
        // York town across the river than to Alexandria, but nearer still
        // to Cornwall, which knows only its province.
        let mut places = vec![
            town("Glen Walter", 45.02, -74.67, None, None),
            town("Massena", 45.00, -74.64, Some("NY"), Some("US")),
            town("Cornwall", 45.02, -74.69, Some("ON"), None),
            town("Alexandria", 45.31, -74.64, Some("ON"), Some("CA")),
            // And a remote town that only a town between learns it from.
            town("Nordegg", 52.47, -116.08, Some("AB"), None),
            town("Rocky Mountain House", 52.37, -114.92, Some("AB"), None),
            town("Red Deer", 52.27, -113.81, Some("AB"), Some("CA")),
        ];
        fill_in_towns(&mut places, &[]);
        let said = |i: usize| (places[i].region.as_deref(), places[i].country.as_deref());
        assert_eq!(said(0), (Some("ON"), Some("CA")));
        assert_eq!(said(2), (Some("ON"), Some("CA")));
        assert_eq!(said(4), (Some("AB"), Some("CA")));
    }

    #[test]
    fn a_town_takes_its_country_from_its_states_code() {
        let state = |name: &str, code: &str, own: &str, lat: f64, lon: f64| {
            let tags = [
                ("name", name),
                ("boundary", "administrative"),
                ("ISO3166-2", code),
                ("ISO3166-1:alpha2", own),
            ];
            Place {
                lat,
                lon,
                ..place_of(&tags, "r1".into(), false).unwrap()
            }
        };
        let states = [
            state("Western Australia", "AU-WA", "", -24.4, 121.0),
            state("Washington", "US-WA", "", 47.3, -120.8),
            state("Minnesota", "US-MN", "", 46.3, -94.3),
            state("Puerto Rico", "US-PR", "PR", 18.2, -66.5),
        ];
        assert_eq!(states[0].kind, STATE_KIND);
        // A boundary without a code is no place, and a node is no state.
        let unnamed = [("name", "Shire"), ("boundary", "administrative")];
        assert!(place_of(&unnamed, "r2".into(), false).is_none());
        let node = [
            ("name", "Western Australia"),
            ("place", "state"),
            ("boundary", "administrative"),
            ("ISO3166-2", "AU-WA"),
        ];
        assert!(place_of(&node, "n1".into(), true).is_none());
        let at = |name: &str, kind: &str, lat: f64, lon: f64| Place {
            name: name.into(),
            kind: kind.into(),
            region: kind.starts_with("shop").then(|| "WA".into()),
            lat,
            lon,
            ..Place::default()
        };
        let shop = |region: &str, lat: f64, lon: f64| Place {
            region: Some(region.into()),
            ..at("Shop", "shop=gift", lat, lon)
        };
        // Perth's shops give their state and, as all over Australia, no
        // country, and so do Spokane's; a village by Perth gives nothing.
        // Rainy River, Ontario is nearest to shops in Minnesota, but a
        // neighbour knows it is in Canada. Puerto Rico is a country too.
        let mut places = vec![
            at("Perth", "place=city", -31.95, 115.86),
            at("Boffins", "shop=books", -31.951, 115.857),
            at("Spokane", "place=city", 47.66, -117.43),
            at("Auntie's", "shop=books", 47.658, -117.425),
            at("Kalamunda", "place=town", -31.97, 116.06),
            at("Rainy River", "place=town", 48.72, -94.57),
            shop("MN", 48.721, -94.571),
            Place {
                region: Some("ON".into()),
                country: Some("CA".into()),
                ..at("Emo", "place=town", 48.63, -93.83)
            },
            at("Hatillo", "place=town", 18.49, -66.83),
            shop("PR", 18.491, -66.831),
        ];
        fill_in_towns(&mut places, &states);
        let said = |i: usize| (places[i].region.as_deref(), places[i].country.as_deref());
        assert_eq!(said(0), (Some("WA"), Some("AU")));
        assert_eq!(said(1), (Some("WA"), Some("AU")));
        assert_eq!(said(2), (Some("WA"), Some("US")));
        assert_eq!(said(4), (Some("WA"), Some("AU")));
        // Canada has no MN, so Rainy River keeps no region.
        assert_eq!(said(5), (None, Some("CA")));
        assert_eq!(said(8), (Some("PR"), Some("PR")));
        // A town whose addresses put Puerto Rico in the US keeps its state.
        let mut places = vec![
            at("Arecibo", "place=town", 18.47, -66.72),
            Place {
                country: Some("US".into()),
                ..shop("PR", 18.471, -66.721)
            },
        ];
        fill_in_towns(&mut places, &states);
        assert_eq!(places[0].region.as_deref(), Some("PR"));
        assert_eq!(places[0].country.as_deref(), Some("US"));
        // Without the states, no country is made up.
        let mut places = vec![
            at("Perth", "place=city", -31.95, 115.86),
            at("Boffins", "shop=books", -31.951, 115.857),
        ];
        fill_in_towns(&mut places, &[]);
        assert_eq!(places[0].country, None);
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
        assert_eq!(
            clean_website(
                "https://www.google.com/url?sa=t&source=web&url=https://locations.tacobell.com/co/denver/1.html"
            )
            .as_deref(),
            Some("https://locations.tacobell.com/co/denver/1.html")
        );
        assert!(clean_website("https://www.google.com/maps/place/x").is_none());
    }
}
