//! The index of places (see [`plumb_core::place`]), searched for queries
//! that say where: "pizza in denver", "coffee near me", "hotels near the
//! eiffel tower", "denver pizza".
//!
//! [`parse_place_query`] splits such a query into what is looked for and
//! where. Where is a town (or any named place) found by its name or another
//! name, optionally followed by its region or country ("portland maine",
//! "paris france"); a clearly larger town can resolve the name, while
//! close namesakes need a region or country. Default-country preference
//! orders alternatives without deciding an ambiguous name. "Near me" is the town the
//! searcher gave Plumb, never worked out from their address.
//!
//! What is looked for must be in a place's name or among the words for its
//! kind (`amenity=cafe` is found by "café", "coffee" and "coffee shop").
//! Places within the town's size ([`town_size`]) of its centre are listed,
//! nearest first, those with a website, brand or Wikidata item a little
//! ahead.

use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use plumb_core::place::{
    area_size, distance_km, is_kind_word, normalize_region, parse_place, town_size,
    us_state_abbreviated, write_place, Place, CANADIAN_PROVINCES, US_STATES,
};
use plumb_core::{country_of_any_name, joined, normalize_text};
use serde::{Deserialize, Serialize};
use tantivy::collector::{Count, TopDocs};
use tantivy::directory::Directory;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, STORED, STRING,
};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, Term};

use crate::analysis::{self, JOINED_ANALYZER, STEMMED_ANALYZER};
use crate::replace::Staging;

/// The id of the places set.
pub const PLACES_SET: &str = "places";
/// Size of the cells places are looked up in, in degrees.
const CELL_DEGREES: f64 = 0.1;
/// Places of the right kind near the town looked at, most notable first.
const CANDIDATES: usize = 1_000;
/// Towns of the same name looked at.
const TOWN_CANDIDATES: usize = 50;
/// People in a borough or suburb that make it as much a town as a city of
/// that size: Brooklyn, Manhattan, Setagaya.
const CITY_SIZED: u64 = 200_000;
/// People in a city that is still found by its name typed with a slip.
const MISTYPED_CITY_PEOPLE: u64 = 100_000;
/// More cities than have [`MISTYPED_CITY_PEOPLE`] (about 4,500 in the world).
const BIG_CITIES_READ: usize = 20_000;
/// How far around a named place that is not a town places are looked for.
const LANDMARK_KM: f64 = 3.0;
/// Fewer places than this within a town's size: look farther.
const FEW: usize = 3;
/// Places this close (km) with the same name, address or website are one
/// place mapped twice: a shop and the pharmacy in it, a building and a
/// point inside it.
const SAME_PLACE_KM: f64 = 0.3;
/// How much a town in the searcher's country counts over a bigger one
/// elsewhere, in [`plumb_core::place::place_rank`] tiers.
const HOME_COUNTRY_TIERS: f64 = 0.2;
/// Namesakes this close (km) are one area to search around, not a
/// choice to ask about.
const SAME_AREA_KM: f64 = 30.0;
/// Required intrinsic rank separation to choose an unqualified namesake.
/// Locale preference can order alternatives but cannot create this gap.
const DISTINCT_LOCATION_TIERS: f64 = 0.15;
/// Bounded location alternatives returned when the name is ambiguous.
const LOCATION_ALTERNATIVES: usize = 5;

/// The most words a town named after a place takes ("union station
/// salt lake city").
const TOWN_WORDS: usize = 4;
/// Where in a town that is its centre: "parking in downtown denver".
const DOWNTOWN: &[&str] = &[
    "downtown",
    "city center",
    "city centre",
    "town center",
    "town centre",
];
/// Words a query may say "near me" with.
const NEAR_ME: &[&[&str]] = &[
    &["near", "me"],
    &["nearby"],
    &["near", "by"],
    &["around", "me"],
    &["close", "to", "me"],
    &["near", "my", "location"],
    &["in", "my", "area"],
    &["in", "the", "area"],
    &["near", "here"],
    &["around", "here"],
];
/// Words that say where, in "pizza in denver".
const WHERE_WORDS: &[&[&str]] = &[&["in"], &["near"], &["around"], &["close", "to"], &["at"]];
/// What may follow "in" or "at" without naming a place: "tallest building
/// in the world", "coffee in the morning", "pizza at home".
const NOWHERE: &[&str] = &[
    "world",
    "whole world",
    "universe",
    "morning",
    "afternoon",
    "evening",
    "night",
    "weekend",
    "summer",
    "winter",
    "home",
];
/// Units a measure is asked in, which name no place either: "tom cruise
/// height in cm", "area of texas in square miles", "price of gold in
/// euros". Not "stone" or "acre", which are towns too.
const UNITS: &[&str] = &[
    "cm",
    "m",
    "km",
    "meters",
    "metres",
    "kilometers",
    "inches",
    "feet",
    "ft",
    "miles",
    "square feet",
    "square meters",
    "square miles",
    "square kilometers",
    "km2",
    "acres",
    "hectares",
    "kg",
    "pounds",
    "lbs",
    "grams",
    "ounces",
    "liters",
    "gallons",
    "celsius",
    "fahrenheit",
    "mph",
    "dollars",
    "euros",
];
/// Kinds of town a query may name without "in" or "near": "denver
/// pizza", "barber brooklyn".
const GUESSED_TOWNS: &[&str] = &["place=city", "place=borough"];
/// Words a name is written with in full or short, and a search may say
/// either way: "st paul" is Saint Paul, "ft lauderdale" Fort Lauderdale.
const SHORT_WORDS: &[(&str, &str)] = &[
    ("st", "saint"),
    ("ste", "sainte"),
    ("ft", "fort"),
    ("mt", "mount"),
];
/// Words ending a query that say when it is open, not where: "open now".
const HOURS: &[&[&str]] = &[
    &["open", "now"],
    &["open", "late"],
    &["open", "today"],
    &["open", "24", "hours"],
    &["open", "at", "night"],
    &["24", "hours"],
    &["24", "7"],
];
/// Words ending a query that say when, as of places as of much else:
/// "pizza tonight", "stock market today".
const WHEN: &[&[&str]] = &[&["now"], &["today"], &["tonight"]];
/// First words of a query about a town itself, not places in it: "time in
/// tokyo", "weather in denver", "capital of washington".
const ABOUT_TOWN: &[&str] = &[
    "time",
    "timezone",
    "weather",
    "temperature",
    "forecast",
    "climate",
    "population",
    "capital",
    "history",
    "news",
    "mayor",
    "elevation",
    "sunrise",
    "sunset",
    "currency",
    "cost",
    "crime",
    "jobs",
];
/// First words of a question of how or why, which asks for no places
/// whatever words follow: "how to merge cells in excel" wants no phone
/// stores in Excel, Alabama.
const ASKS_HOW: &[&str] = &["how", "why"];
/// Words left out of what is looked for: "best pizza", "places to eat".
const FILLER: &[&str] = &[
    "best", "good", "great", "cheap", "top", "nice", "nearest", "closest", "open", "the", "a",
    "an", "some", "any", "find", "where", "is", "are", "places", "place", "spots", "spot", "to",
    "local",
];

/// Where a place query looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Near {
    /// The searcher's own town.
    Me,
    /// A town or other named place, as typed.
    Named(String),
}

/// A query that looks for places somewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceQuery {
    /// What is looked for, without filler words: "pizza".
    pub what: String,
    pub near: Near,
    /// The query said where with "in", "near" or "near me"; `false` when
    /// a town was guessed from its words ("denver pizza", "us bank").
    pub said_where: bool,
}

/// Splits `query` into what is looked for and where, when it says where.
/// A query without "in", "near" or "near me" says where only when it is a
/// kind of place and a town: "denver pizza" ([`PlaceSearcher::search`]
/// checks the town). This is the first of [`place_query_readings`].
pub fn parse_place_query(query: &str) -> Option<PlaceQuery> {
    place_query_readings(query).into_iter().next()
}

/// Every way `query` can be read as asking for places somewhere, the most
/// likely first. Only a query that names its town without "in" or "near"
/// has more than one: "coffee shop boulder" is coffee in "shop boulder" or
/// a coffee shop in Boulder, and only the index knows which town there is.
pub fn place_query_readings(query: &str) -> Vec<PlaceQuery> {
    let mut words: Vec<String> = normalize_text(query)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let (said_when, said_hours) = take_times(&mut words);
    if words
        .iter()
        .map(String::as_str)
        .find(|w| !FILLER.contains(w))
        .is_some_and(|w| ABOUT_TOWN.contains(&w) || ASKS_HOW.contains(&w))
    {
        return Vec::new();
    }
    let what_of = |words: &[String]| -> Option<String> {
        let what: Vec<&str> = words
            .iter()
            .map(String::as_str)
            .skip_while(|w| FILLER.contains(w))
            .filter(|w| !FILLER.contains(w) || *w == "to")
            .collect();
        let what = what.join(" ");
        (!what.is_empty() && what != "to").then_some(what)
    };
    let near_me = |what: Option<String>| -> Vec<PlaceQuery> {
        what.map(|what| PlaceQuery {
            what,
            near: Near::Me,
            said_where: true,
        })
        .into_iter()
        .collect()
    };
    if let Some((at, _)) = near_me_at(&words) {
        return near_me(what_of(&words[..at]));
    }
    if words.first().is_some_and(|w| w == "nearby") && words.len() > 1 {
        return near_me(what_of(&words[1..]));
    }
    // The last "in" or "near" with words on both sides.
    for at in (1..words.len().saturating_sub(1)).rev() {
        for marker in WHERE_WORDS {
            let end = at + marker.len();
            if end < words.len() && words[at..end] == **marker {
                let Some(what) = what_of(&words[..at]) else {
                    return Vec::new();
                };
                let mut place = &words[end..];
                if place.first().is_some_and(|w| w == "the") && place.len() > 1 {
                    place = &place[1..];
                }
                let place = place.join(" ");
                if NOWHERE.contains(&place.as_str()) || UNITS.contains(&place.as_str()) {
                    return Vec::new();
                }
                return vec![PlaceQuery {
                    what,
                    near: Near::Named(place),
                    said_where: true,
                }];
            }
        }
    }
    // "denver pizza", "pizza denver": a town on one side, kind words on the
    // other.
    // A country after the kind still qualifies the city: "Rome museums
    // Italy", rather than treating "Italy" as a town and "Rome" as part
    // of the business name. So does a state's or province's name,
    // "springfield city hall illinois", which alone names no town. Not
    // their codes, which are words too ("in", "or").
    for count in (1..=3.min(words.len().saturating_sub(2))).rev() {
        let at = words.len() - count;
        let country = words[at..].join(" ");
        let region = US_STATES
            .iter()
            .chain(CANADIAN_PROVINCES)
            .any(|(_, name)| normalize_text(name) == country);
        if region || country_of_any_name(&country).is_some() {
            let qualified: Vec<PlaceQuery> = place_query_readings(&words[..at].join(" "))
                .into_iter()
                .filter_map(|mut asked| match &mut asked.near {
                    Near::Named(name) => {
                        name.push(' ');
                        name.push_str(&country);
                        Some(asked)
                    }
                    Near::Me => None,
                })
                .collect();
            if !qualified.is_empty() {
                return qualified;
            }
        }
    }
    let mut readings = Vec::new();
    let guessed = |what: String, town: &[String]| PlaceQuery {
        what,
        near: Near::Named(town.join(" ")),
        said_where: false,
    };
    if words.len() >= 2 {
        for town_words in (1..=3.min(words.len() - 1)).rev() {
            let (what, town) = words.split_at(words.len() - town_words);
            // What is looked for ends in a kind of place: "pizza", "best
            // climbing gym", "cheap bookstore".
            if what.last().is_some_and(|w| is_kind_word(w)) {
                readings.extend(what_of(what).map(|what| guessed(what, town)));
            }
            let (town, what) = words.split_at(town_words);
            if what.iter().all(|w| is_kind_word(w)) {
                readings.extend(what_of(what).map(|what| guessed(what, town)));
            }
        }
    }
    // "coffee open now", "pharmacy open late": a kind of place asked for
    // at a time, with no town, is near the searcher, and so is the
    // "nearest pharmacy". A time alone says that only of words that are
    // all kinds of place: "pizza tonight", not "stock market today".
    let kinds = |words: &[String]| {
        words
            .iter()
            .all(|w| FILLER.contains(&w.as_str()) || is_kind_word(w))
    };
    let said_nearest = words.iter().any(|w| w == "nearest" || w == "closest");
    if words.last().is_some_and(|w| is_kind_word(w))
        && (said_nearest || (said_when && (said_hours || kinds(&words))))
    {
        readings.extend(near_me(what_of(&words)));
    }
    readings
}

/// Takes the times out of `words`: "plumber boston open now", when is not
/// where. Whether `words` said a time, and whether it was opening hours.
fn take_times(words: &mut Vec<String>) -> (bool, bool) {
    let (mut said_when, mut said_hours) = (false, false);
    while let Some(when) = HOURS
        .iter()
        .chain(WHEN)
        .find(|when| words.len() > when.len() && words[words.len() - when.len()..] == ***when)
    {
        words.truncate(words.len() - when.len());
        said_when = true;
        said_hours |= HOURS.contains(when);
    }
    // Nor in the middle, "pharmacy open now denver", for the hours: a
    // word alone may be part of a name ("now tv").
    while let Some((at, length)) = HOURS.iter().find_map(|hours| {
        (1..words.len().saturating_sub(hours.len()))
            .find(|&at| words[at..at + hours.len()] == **hours)
            .map(|at| (at, hours.len()))
    }) {
        words.drain(at..at + length);
        said_when = true;
        said_hours = true;
    }
    (said_when, said_hours)
}

/// Where `words` say "near me" or the like, and in how many words: at
/// their end ("safeway near me"), or after the first word with more after
/// it ("dentist near me that takes medicaid", "car wash near me open
/// sunday"), which say no more of where. "nearby" only at the end: "hotels
/// nearby disney world" is near Disney World.
fn near_me_at(words: &[String]) -> Option<(usize, usize)> {
    let said = |at: usize, near: &[&str]| {
        words
            .get(at..at + near.len())
            .is_some_and(|words| words == near)
    };
    NEAR_ME
        .iter()
        .find(|near| words.len() > near.len() && said(words.len() - near.len(), near))
        .map(|near| (words.len() - near.len(), near.len()))
        .or_else(|| {
            (1..words.len()).find_map(|at| {
                NEAR_ME
                    .iter()
                    .find(|near| near.iter().any(|w| *w == "me" || *w == "my") && said(at, near))
                    .map(|near| (at, near.len()))
            })
        })
}

/// `query` without the "near me" in it ("safeway near me" -> "safeway",
/// "dentist near me that takes medicaid" -> "dentist that takes
/// medicaid"), for searching sites: "me" names no site (domain.me,
/// maine.gov). `None` when it has none.
pub fn without_near_me(query: &str) -> Option<String> {
    let mut words: Vec<String> = normalize_text(query)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let (at, length) = near_me_at(&words)?;
    words.drain(at..at + length);
    Some(words.join(" "))
}

/// Puts the sites that say what they are (a title) or are official first,
/// for a search "near me": "handyman near me" asks for someone to call, and
/// a bare handyman.nl, named by the word but saying nothing, goes after
/// them.
pub fn said_first(hits: &mut Vec<crate::Hit>) {
    let (said, bare): (Vec<crate::Hit>, Vec<crate::Hit>) = std::mem::take(hits)
        .into_iter()
        .partition(|hit| hit.title.is_some() || hit.official);
    *hits = said.into_iter().chain(bare).collect();
}

/// A place found, and how far it is from where the search looked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaceHit {
    pub place: Place,
    pub km: f64,
}

/// What a place search found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaceResults {
    /// What was looked for: "pizza".
    pub what: String,
    /// Where it looked: the town or place searched around. `None` when the
    /// query said "near me" and the searcher has given no town Plumb knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center: Option<Place>,
    /// The query said "near me".
    #[serde(default)]
    pub near_me: bool,
    /// The query did not say where; the town was guessed from its words
    /// ("denver pizza"), so it may be a name instead ("us bank").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub guessed: bool,
    /// How far around the centre it looked, in km.
    pub radius_km: f64,
    pub hits: Vec<PlaceHit>,
    /// Resolution evidence, absent in responses made by older nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationResolution>,
}

/// Why a location could or could not be selected from the indexed places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationStatus {
    Resolved,
    MissingLocation,
    UnknownLocation,
    AmbiguousLocation,
    ConflictingConstraints,
}

/// Bounded, factual location evidence, without a probability or a claim
/// that an unindexed place does not exist. The first candidate is selected
/// only for `resolved`; otherwise candidates are alternatives to clarify.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocationResolution {
    pub status: LocationStatus,
    pub requested: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<Place>,
}

impl LocationResolution {
    pub fn selected(&self) -> Option<&Place> {
        (self.status == LocationStatus::Resolved)
            .then(|| self.candidates.first())
            .flatten()
    }
}

#[derive(Clone, Copy)]
struct Fields {
    words: Field,
    keys: Field,
    kind: Field,
    cell: Field,
    rank: Field,
    place: Field,
}

fn schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let words = builder.add_text_field(
        "words",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(STEMMED_ANALYZER)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let keys = builder.add_text_field(
        "keys",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(JOINED_ANALYZER)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let kind = builder.add_text_field("kind", STRING);
    let cell = builder.add_text_field("cell", STRING);
    let rank = builder.add_u64_field("rank", FAST | STORED);
    let place = builder.add_text_field("place", STORED);
    (
        builder.build(),
        Fields {
            words,
            keys,
            kind,
            cell,
            rank,
            place,
        },
    )
}

/// The cell `lat`, `lon` is in.
fn cell(lat: f64, lon: f64) -> String {
    let row = (lat / CELL_DEGREES).floor() as i32;
    let col = (lon / CELL_DEGREES).floor() as i32;
    format!("{row}:{col}")
}

/// The cells within `km` of `lat`, `lon`.
fn cells_around(lat: f64, lon: f64, km: f64) -> Vec<String> {
    let km_per_cell = 111.2 * CELL_DEGREES;
    let rows = (km / km_per_cell).ceil() as i32;
    let cols = ((km / (km_per_cell * lat.to_radians().cos().max(0.05))).ceil() as i32).min(1800);
    let row = (lat / CELL_DEGREES).floor() as i32;
    let col = (lon / CELL_DEGREES).floor() as i32;
    let wrap = (360.0 / CELL_DEGREES) as i32;
    let mut cells = Vec::new();
    for r in row - rows..=row + rows {
        for c in col - cols..=col + cols {
            // Across the 180th meridian.
            let c = (c + wrap / 2).rem_euclid(wrap) - wrap / 2;
            cells.push(format!("{r}:{c}"));
        }
    }
    cells.sort();
    cells.dedup();
    cells
}

/// What [`build_place_index`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceIndexStats {
    pub places: u64,
    pub towns: u64,
}

/// Builds the place index of `places` in `dir`, replacing any there.
pub fn build_place_index(
    dir: &Path,
    places: impl IntoIterator<Item = Place>,
) -> Result<PlaceIndexStats> {
    build_place_index_with_budget(dir, places, None)
}

pub fn build_place_index_with_budget(
    dir: &Path,
    places: impl IntoIterator<Item = Place>,
    budget: Option<std::sync::Arc<plumb_core::storage::StorageBudget>>,
) -> Result<PlaceIndexStats> {
    let staging = Staging::new_with_budget(dir, budget.clone())?;
    let (schema, fields) = schema();
    let index = crate::storage::create_index(staging.path(), schema, budget, staging.lifecycle())
        .with_context(|| format!("creating the place index in {}", dir.display()))?;
    analysis::register(index.tokenizers());
    let mut writer = index
        .writer_with_num_threads(1, 128 << 20)
        .context("opening the place index for writing")?;
    let mut stats = PlaceIndexStats::default();
    for place in places {
        let mut document = TantivyDocument::default();
        document.add_text(fields.words, &place.name);
        for alias in &place.aliases {
            document.add_text(fields.words, alias);
        }
        document.add_text(fields.words, place.kind_words());
        document.add_text(fields.keys, &place.name);
        for alias in &place.aliases {
            document.add_text(fields.keys, alias);
        }
        document.add_text(fields.kind, &place.kind);
        document.add_text(fields.cell, cell(place.lat, place.lon));
        document.add_u64(fields.rank, u64::from(place.rank));
        // Stored as its places file line, half the size of JSON.
        let mut line = Vec::new();
        write_place(&mut line, &place)?;
        document.add_text(fields.place, String::from_utf8(line)?);
        writer.add_document(document)?;
        stats.places += 1;
        if place.is_town() {
            stats.towns += 1;
        }
    }
    writer.commit().context("writing the place index")?;
    // A merge the commit started must end before the index is put in
    // place: one cut short leaves its segment files behind for good.
    writer
        .wait_merging_threads()
        .context("finishing the place index merges")?;
    let stats_bytes = serde_json::to_vec(&stats)?;
    if staging.budget().is_some() {
        index
            .directory()
            .atomic_write(Path::new("places.json"), &stats_bytes)?;
    } else {
        std::fs::write(staging.path().join("places.json"), stats_bytes)?;
    }
    staging.install()?;
    Ok(stats)
}

/// Searches a place index.
pub struct PlaceSearcher {
    reader: IndexReader,
    fields: Fields,
    joined: TextAnalyzer,
    stemmed: TextAnalyzer,
    stats: PlaceIndexStats,
    /// Cities of [`MISTYPED_CITY_PEOPLE`] or more, the biggest first, with
    /// the keys of their names: a few thousand, read when first needed.
    big_cities: OnceLock<Vec<(Vec<String>, Place)>>,
}

impl PlaceSearcher {
    pub fn open(dir: &Path) -> Result<Self> {
        let index = Index::open_in_dir(dir)
            .with_context(|| format!("opening the place index in {}", dir.display()))?;
        analysis::register(index.tokenizers());
        let (schema, fields) = schema();
        if index.schema() != schema {
            bail!(
                "the place index in {} was built by another version; rebuild it",
                dir.display()
            );
        }
        let stats: PlaceIndexStats = serde_json::from_slice(
            &std::fs::read(dir.join("places.json"))
                .with_context(|| format!("reading {}/places.json", dir.display()))?,
        )?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(PlaceSearcher {
            reader,
            fields,
            joined: analysis::joined_analyzer(),
            stemmed: analysis::stemmed_analyzer(),
            stats,
            big_cities: OnceLock::new(),
        })
    }

    pub fn num_places(&self) -> u64 {
        self.stats.places
    }

    /// The places `query` asks for, when it asks for places somewhere:
    /// around `home`, the searcher's town as they typed it, for "near me";
    /// with `country` ordering alternatives for an ambiguous name.
    /// `None` when the query does not ask for places. A town it doesn't
    /// know gives a coverage status, not a guessed town, when what it
    /// asks for is a kind of place.
    pub fn search(
        &self,
        query: &str,
        home: Option<&str>,
        country: Option<&str>,
        limit: usize,
    ) -> Result<Option<PlaceResults>> {
        self.search_in_country(query, home, country, None, limit)
    }

    /// Place search with a hard country constraint separate from the
    /// searcher's default-country preference. Query qualifiers and this
    /// constraint are both retained; conflicts never broaden the search.
    pub fn search_in_country(
        &self,
        query: &str,
        home: Option<&str>,
        preferred_country: Option<&str>,
        required_country: Option<&str>,
        limit: usize,
    ) -> Result<Option<PlaceResults>> {
        // A business named with its town comes last, when no other
        // reading finds a town: "pizza hut denver" is not pizza near a
        // place called Hut Denver.
        let mut readings = place_query_readings(query);
        readings.extend(self.named_business_query(query, preferred_country, required_country)?);
        // A guessed town of several namesakes is only an answer when no
        // other reading finds one town: "denver pizza places" is not about
        // the villages called Places.
        let mut choice = None;
        for asked in readings {
            let location = match &asked.near {
                Near::Me => match home {
                    Some(home) => self.resolve(home, preferred_country, required_country)?,
                    None => LocationResolution {
                        status: LocationStatus::MissingLocation,
                        requested: String::new(),
                        candidates: Vec::new(),
                    },
                },
                Near::Named(name) if asked.said_where => {
                    self.resolve(name, preferred_country, required_country)?
                }
                Near::Named(name) => {
                    self.resolve_as_typed(name, preferred_country, required_country)?
                }
            };
            // A town guessed from words ("toy story", "hotel california",
            // "crypto exchange") is only taken for a big one: Story in
            // France and Crypto in Poland are villages whose names are
            // words. A choice of towns is offered only when the likeliest
            // is big: "stock market" offers no choice of suburbs called
            // Stock. Otherwise the query may still split another way.
            // A guess that names no town at all ("court view alaska") is
            // no reading either.
            let guessed_wrong = location
                .candidates
                .first()
                .is_none_or(|place| !GUESSED_TOWNS.contains(&place.kind.as_str()));
            if asked.said_where || (!guessed_wrong && location.selected().is_some()) {
                let found = self.found(asked, location, required_country, limit)?;
                return self.asks_for_places(found);
            }
            if !guessed_wrong && choice.is_none() {
                choice = Some((asked, location));
            }
        }
        match choice {
            Some((asked, location)) => {
                let found = self.found(asked, location, required_country, limit)?;
                self.asks_for_places(found)
            }
            None => Ok(None),
        }
    }

    /// `found`, unless it found nothing and what it looked for is no kind
    /// of place: "sort a list in python", "keel in english" and "calories
    /// in a shot" ask for no place, while "pizza in gotham" does. A place's
    /// name still asks when the town is one of several, so the choice is
    /// offered ("starbucks in portland"), or when only a state or country
    /// was named, so a town is asked for ("starbucks in texas"). Near me
    /// always does, so one that finds nothing still says so.
    fn asks_for_places(&self, found: PlaceResults) -> Result<Option<PlaceResults>> {
        let kind = found.what.rsplit(' ').next().is_some_and(is_kind_word);
        let wants_town = found.location.as_ref().is_some_and(|location| {
            matches!(
                location.status,
                LocationStatus::AmbiguousLocation | LocationStatus::MissingLocation
            )
        });
        if found.hits.is_empty()
            && !found.near_me
            && !kind
            && !(wants_town
                && self
                    .named(&found.what)?
                    .iter()
                    .any(|place| !place.is_town()))
        {
            return Ok(None);
        }
        Ok(Some(found))
    }

    /// The places `asked` for around `location`.
    fn found(
        &self,
        asked: PlaceQuery,
        location: LocationResolution,
        required_country: Option<&str>,
        limit: usize,
    ) -> Result<PlaceResults> {
        let center = location.selected().cloned();
        let near_me = asked.near == Near::Me;
        let guessed = !asked.said_where;
        let Some(center) = center else {
            return Ok(PlaceResults {
                what: asked.what,
                center: None,
                near_me,
                guessed,
                radius_km: 0.0,
                hits: Vec::new(),
                location: Some(location),
            });
        };
        let radius = town_size(&center.kind)
            .or_else(|| area_size(&center.kind))
            .unwrap_or(LANDMARK_KM);
        let mut what = asked.what;
        let (mut hits, mut radius_km) =
            self.around_town(&what, &center, radius, required_country, limit)?;
        // "luxury hotels", "24 hour pharmacy", "pet friendly hotels": a
        // word no place is found by leaves few or none, so the kinds alone
        // are looked for, then the last of them, after the places found
        // as asked. The heading then says what was looked for.
        for fewer in fewer_words(&what) {
            // A few places named as asked of another kind are what was
            // meant: "olive garden" is no kind of garden.
            let stems = analysis::tokens(&self.stemmed, &fewer);
            if hits.len() >= FEW.min(limit)
                || hits.iter().any(|hit| !self.is_kind(&hit.place, &stems))
            {
                break;
            }
            let (more, km) = self.around_town(&fewer, &center, radius, required_country, limit)?;
            let before = hits.len();
            for hit in more {
                if hits.len() < limit && !hits.iter().any(|h| same_place(&h.place, &hit.place)) {
                    hits.push(hit);
                }
            }
            if hits.len() > before {
                what = fewer;
                radius_km = radius_km.max(km);
            }
        }
        // A resolved location with no matching businesses is an indexed
        // coverage result too, including explicit landmark searches.
        Ok(PlaceResults {
            what,
            center: Some(center),
            near_me,
            guessed,
            radius_km,
            hits,
            location: Some(location),
        })
    }

    /// A named business followed by a city its name resolves to: "Poilâne
    /// Paris", "starbucks denver" (Colorado's, though villages elsewhere
    /// are called Denver too). No geographic interpretation is made for an
    /// unknown tail or a choice of towns.
    fn named_business_query(
        &self,
        query: &str,
        preferred_country: Option<&str>,
        required_country: Option<&str>,
    ) -> Result<Option<PlaceQuery>> {
        let normalized = normalize_text(query);
        let words: Vec<&str> = normalized.split_whitespace().collect();
        if !(2..=6).contains(&words.len())
            || ABOUT_TOWN.contains(&words[0])
            || ASKS_HOW.contains(&words[0])
        {
            return Ok(None);
        }
        for count in (1..=3.min(words.len() - 1)).rev() {
            let (what, near) = words.split_at(words.len() - count);
            // Most searches end in no city's name: found without reading
            // a place, since every search on a node comes here.
            let mut city = false;
            for k in 1..=near.len() {
                city = city || self.names_a_city(&near[..k].join(" "))?;
            }
            if !city {
                continue;
            }
            let name = near.join(" ");
            let location = self.resolve_as_typed(&name, preferred_country, required_country)?;
            let Some(town) = location
                .selected()
                .filter(|place| GUESSED_TOWNS.contains(&place.kind.as_str()))
            else {
                continue;
            };
            // Only a place called that, as far out as places are listed,
            // is a business named: "bible reading" asks for no Bible
            // church in Reading, Pennsylvania, and "boulder denver" for no
            // town near Denver. So is a branch called that and the town:
            // "sheraton charlotte" is the Sheraton Charlotte Hotel, though
            // nothing is called just Sheraton. "university of denver" is
            // one place's name, not a University of in Denver.
            let joined = what
                .last()
                .is_some_and(|word| ["of", "de", "di", "du"].contains(word));
            let what = what.join(" ");
            let km = 3.0 * town_size(&town.kind).unwrap_or(LANDMARK_KM);
            let named = self.named_within(&what, Some((town, km)))?;
            if named.iter().any(|place| !place.is_town())
                || (!joined && self.branch_within(&normalized, town, km, required_country)?)
            {
                return Ok(Some(PlaceQuery {
                    what,
                    near: Near::Named(name),
                    said_where: false,
                }));
            }
        }
        Ok(None)
    }

    /// Whether a place within `km` of `town`, other than a town, has every
    /// word of `query` in its name: "Sheraton Charlotte Hotel" for
    /// "sheraton charlotte".
    fn branch_within(
        &self,
        query: &str,
        town: &Place,
        km: f64,
        required_country: Option<&str>,
    ) -> Result<bool> {
        let stems = analysis::tokens(&self.stemmed, query);
        let hits = self.around(query, town, km, required_country, CANDIDATES)?;
        Ok(hits.iter().any(|hit| {
            std::iter::once(&hit.place.name)
                .chain(&hit.place.aliases)
                .any(|name| {
                    let named = analysis::tokens(&self.stemmed, name);
                    stems.iter().all(|stem| named.contains(stem))
                })
        }))
    }

    /// The places matching `what` within `radius` km of `center`, or three
    /// times as far when there are few (a small town), and how far it
    /// looked.
    fn around_town(
        &self,
        what: &str,
        center: &Place,
        radius: f64,
        required_country: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<PlaceHit>, f64)> {
        let hits = self.around(what, center, radius, required_country, limit)?;
        if hits.len() >= FEW.min(limit) {
            return Ok((hits, radius));
        }
        let farther = radius * 3.0;
        Ok((
            self.around(what, center, farther, required_country, limit)?,
            farther,
        ))
    }

    /// Whether `place` is of the kind all of `stems` say, not only named
    /// with them.
    fn is_kind(&self, place: &Place, stems: &[String]) -> bool {
        let words: HashSet<String> = analysis::tokens(&self.stemmed, &place.kind_words())
            .into_iter()
            .collect();
        stems.iter().all(|stem| words.contains(stem))
    }

    /// Whether a city or borough (a town a query may name without "in")
    /// is called `name`.
    fn names_a_city(&self, name: &str) -> Result<bool> {
        let Some(key) = self.key_query(name) else {
            return Ok(false);
        };
        let kinds = GUESSED_TOWNS
            .iter()
            .map(|kind| {
                let term = Term::from_field_text(self.fields.kind, kind);
                let query = TermQuery::new(term, IndexRecordOption::Basic);
                (Occur::Should, Box::new(query) as Box<dyn Query>)
            })
            .collect();
        let query = BooleanQuery::new(vec![
            (Occur::Must, Box::new(key)),
            (Occur::Must, Box::new(BooleanQuery::new(kinds))),
        ]);
        Ok(self.reader.searcher().search(&query, &Count)? > 0)
    }

    /// The best `limit` places matching `what` within `km` of `center`.
    fn around(
        &self,
        what: &str,
        center: &Place,
        km: f64,
        required_country: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PlaceHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let stems = analysis::tokens(&self.stemmed, what);
        if stems.is_empty() {
            return Ok(Vec::new());
        }
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = stems
            .iter()
            .map(|stem| {
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.words, stem),
                        IndexRecordOption::Basic,
                    )) as Box<dyn Query>,
                )
            })
            .collect();
        clauses.push((Occur::Must, Box::new(self.cells_query(center, km))));
        let searcher = self.reader.searcher();
        let found = searcher.search(
            &BooleanQuery::new(clauses),
            &TopDocs::with_limit(CANDIDATES)
                .order_by_fast_field::<u64>("rank", tantivy::Order::Desc),
        )?;
        // "park" is also the stem of "parking": a car park is no park,
        // and a park (or the Park Hotel) no parking.
        const PARKING: [&str; 3] = ["parking", "garage", "car park"];
        let says_parking = |text: &str| PARKING.iter().any(|word| text.contains(word));
        let parking = says_parking(what);
        let mut hits = Vec::new();
        for (_, address) in found {
            let document: TantivyDocument = searcher.doc(address)?;
            let place = self.stored(&document)?;
            if required_country.is_some_and(|country| !country_matches(&place, country)) {
                continue;
            }
            let car_park = place.kind == "amenity=parking";
            let no_parking = || {
                let words = normalize_text(&format!("{} {}", place.name, place.kind_words()));
                !car_park && !says_parking(&words)
            };
            if place.is_town() || (car_park && !parking) || (parking && no_parking()) {
                continue;
            }
            let d = distance_km(center.lat, center.lon, place.lat, place.lon);
            if d > km {
                continue;
            }
            hits.push(PlaceHit { place, km: d });
        }
        // Nearest first; a place that says more about itself (a website, a
        // brand, a Wikidata item) counts as up to half again as near.
        // Places of the kind asked for come before those that only have the
        // words in their name: sushi bars before an office called Sushi
        // Tech, for "sushi".
        let key = |hit: &PlaceHit| {
            let tier = f64::from(hit.place.rank / 1_000_000).clamp(1.0, 6.0);
            let named_only = if self.is_kind(&hit.place, &stems) {
                0.0
            } else {
                1e6
            };
            named_only + hit.km / (1.0 + 0.1 * (tier - 1.0))
        };
        let mut hits: Vec<(f64, PlaceHit)> = hits.into_iter().map(|h| (key(&h), h)).collect();
        hits.sort_by(|a, b| a.0.total_cmp(&b.0));
        let hits = hits.into_iter().map(|(_, h)| h);
        let mut kept: Vec<PlaceHit> = Vec::new();
        for hit in hits {
            let twice = kept
                .iter()
                .any(|other| same_place(&hit.place, &other.place));
            if !twice {
                kept.push(hit);
            }
            if kept.len() == limit {
                break;
            }
        }
        Ok(kept)
    }

    /// The town (or else other place) `text` names: "denver",
    /// "portland maine", "paris, france", "the eiffel tower".
    pub fn locate(&self, text: &str, country: Option<&str>) -> Result<Option<Place>> {
        Ok(self
            .resolve_as_typed(text, country, None)?
            .selected()
            .cloned())
    }

    /// Resolves the name without allowing a default-country boost to
    /// override explicit qualifiers or decide a close namesake tie. Where
    /// nothing is called that, the big cities whose names it mistypes
    /// ([`Self::mistyped_cities`]) are its candidates, to offer rather
    /// than search: "nature" is a slip from Natore, Bangladesh too.
    pub fn resolve(
        &self,
        text: &str,
        preferred_country: Option<&str>,
        required_country: Option<&str>,
    ) -> Result<LocationResolution> {
        let mut found = self.resolve_as_typed(text, preferred_country, required_country)?;
        if found.status == LocationStatus::UnknownLocation {
            let words: Vec<String> = normalize_text(text)
                .split(' ')
                .filter(|word| !word.is_empty())
                .map(str::to_string)
                .collect();
            found.candidates = self.mistyped_cities(&words, required_country)?;
            found.candidates.truncate(LOCATION_ALTERNATIVES);
        }
        Ok(found)
    }

    /// [`Self::resolve`] by the name as typed.
    fn resolve_as_typed(
        &self,
        text: &str,
        preferred_country: Option<&str>,
        required_country: Option<&str>,
    ) -> Result<LocationResolution> {
        // Country-only intent cannot select a foreign town of that name:
        // Italy, Texas or Us, France. A city-state can still resolve to
        // an actual town within that country (Singapore).
        let named_country = country_of_any_name(text);
        let words: Vec<&str> = text
            .split(|c: char| c.is_whitespace() || c == ',')
            .collect();
        let words: Vec<String> = normalize_text(&words.join(" "))
            .split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect();
        // A US state or Canadian province named alone is no town either:
        // "hotels in florida" is not Florida in Uruguay, nor "reserves in
        // ontario" Ontario, California. A town of its name in the state
        // still is ("new york", "quebec"), and "washington" is the capital
        // too.
        let whole = words.join(" ");
        let region = normalize_region(&whole);
        let named_state = US_STATES
            .iter()
            .map(|state| (state, "US"))
            .chain(CANADIAN_PROVINCES.iter().map(|province| (province, "CA")))
            .find(|((code, _), _)| *code == region)
            .map(|((code, _), country)| (*code, country));
        // A town first, however the words split ("boulder co" is Boulder in
        // Colorado before it is a gym called Boulder & Co.), then any place.
        // A landmark counts as a town: "yellowstone" is the park before a
        // village in Alberta. Where the words split more than one way into
        // towns, the biggest wins, so "san antonio texas" is the city in
        // Texas, not a village in Oaxaca called San Antonio Texas; a city
        // found is not looked past.
        let mut towns: Option<Vec<Place>> = None;
        let biggest = |places: &[Place]| places.iter().map(|place| place.rank).max();
        let mut other = None;
        let mut conflict = false;
        let notable = plumb_core::place::place_rank("", 0, true, false, 0);
        let town_like = |place: &Place| place.is_town() || place.is_landmark();
        for k in (1..=words.len()).rev() {
            let name = words[..k].join(" ");
            let qualifier = words[k..].join(" ");
            let fits = |place: &Place| {
                (qualifier.is_empty() || place_is_in(place, &qualifier))
                    // Nor an island the country is on: "hotels in jamaica"
                    // asks for a town.
                    && named_country.is_none_or(|country| {
                        place.is_town() && country_matches(place, country)
                    })
                    && named_state.is_none_or(|(state, country)| {
                        // Uruguay's Florida is in its department FL.
                        let (region, of) = (place.region.as_deref(), place.country.as_deref());
                        let in_state = of == Some(country)
                            && region.is_some_and(|region| {
                                region == state || (state == "WA" && region == "DC")
                            });
                        // Older places files know no Canadian town's
                        // province.
                        in_state
                            || (country == "CA" && region.is_none() && of.is_none_or(|of| of == "CA"))
                    })
            };
            let named = self.named(&name)?;
            let mut matching: Vec<Place> =
                named.iter().filter(|place| fits(place)).cloned().collect();
            // German without its accents ([`Self::named_within`]) also when
            // what the name as typed finds is no town that fits: "muenster
            // germany" is not Muenster, Texas.
            let plain = without_umlauts(&name);
            if plain != name && !matching.iter().any(town_like) {
                let found: Vec<Place> = self.named(&plain)?.into_iter().filter(fits).collect();
                if found.iter().any(town_like) {
                    matching = found;
                }
            }
            // A town the index knows no country of (Australia's, whose
            // addresses seldom give it) may be in the country named when
            // no town of the name is known to be: "hotels in perth
            // australia".
            if matching.is_empty() && country_of_any_name(&qualifier).is_some() {
                matching = named
                    .into_iter()
                    .filter(|place| place.is_town() && place.country.is_none())
                    .collect();
            }
            // "union station denver", "soho london": a place by the town
            // the qualifier names, not in a region of that name. A town is
            // a few words, and each try resolves them again.
            if matching.is_empty() && (1..=TOWN_WORDS).contains(&(words.len() - k)) {
                matching =
                    self.named_by_town(&name, &qualifier, preferred_country, required_country)?;
            }
            let mut candidates: Vec<Place> = matching
                .iter()
                .filter(|place| {
                    required_country.is_none_or(|country| country_matches(place, country))
                })
                .cloned()
                .collect();
            conflict |= !qualifier.is_empty() && !matching.is_empty() && candidates.is_empty();
            self.prefer_as_typed(&name, &mut candidates);
            // A place that is no town, named alone, is somewhere to search
            // around only when Wikidata knows it: "planets in the solar
            // system" is not about an office called Solar System, nor
            // "calories in a shot" about a restaurant called Ashot.
            if qualifier.is_empty() {
                candidates.retain(|place| place.is_town() || place.rank >= notable);
            }
            if candidates.iter().any(town_like) {
                candidates.retain(town_like);
                if towns
                    .as_deref()
                    .is_none_or(|towns| biggest(&candidates) > biggest(towns))
                {
                    towns = Some(candidates);
                }
                if towns
                    .iter()
                    .flatten()
                    .any(|place| place.kind == "place=city")
                {
                    break;
                }
                continue;
            }
            if !candidates.is_empty() && other.is_none() {
                other = Some(resolve_candidates(text, candidates, preferred_country));
            }
        }
        if let Some(towns) = towns {
            return Ok(resolve_candidates(text, towns, preferred_country));
        }
        Ok(other.unwrap_or_else(|| LocationResolution {
            requested: text.to_string(),
            status: if conflict {
                LocationStatus::ConflictingConstraints
            } else if named_country.is_some() || named_state.is_some() {
                LocationStatus::MissingLocation
            } else {
                LocationStatus::UnknownLocation
            },
            candidates: Vec::new(),
        }))
    }

    /// Cities of [`MISTYPED_CITY_PEOPLE`] or more whose name is a slip
    /// from how `words` begin, the rest saying where it is, for a WHERE
    /// nothing is called: one slip in a name of six to eight letters, two
    /// in a longer one ("hotels in cincinatti"). Shorter names are left
    /// alone: "pizza in gotham" is no Gotha.
    fn mistyped_cities(
        &self,
        words: &[String],
        required_country: Option<&str>,
    ) -> Result<Vec<Place>> {
        let cities = self.big_cities()?;
        for k in (1..=words.len()).rev() {
            let qualifier = words[k..].join(" ");
            let Some(key) = analysis::tokens(&self.joined, &words[..k].join(" ")).pop() else {
                continue;
            };
            let slips = match key.chars().count() {
                0..=5 => continue,
                6..=8 => 1,
                _ => 2,
            };
            let dfa = crate::spell::builder(slips).build_dfa(&key);
            let found: Vec<(u8, &Place)> = cities
                .iter()
                .filter(|(_, place)| {
                    (qualifier.is_empty() || place_is_in(place, &qualifier))
                        && required_country.is_none_or(|country| country_matches(place, country))
                })
                .filter_map(|(keys, place)| {
                    let slips = keys
                        .iter()
                        .filter_map(|key| match dfa.eval(key) {
                            levenshtein_automata::Distance::Exact(slips) => Some(slips),
                            levenshtein_automata::Distance::AtLeast(_) => None,
                        })
                        .min()?;
                    Some((slips, place))
                })
                .collect();
            // The fewest slips first, then the biggest city.
            if let Some(fewest) = found.iter().map(|(slips, _)| *slips).min() {
                return Ok(found
                    .into_iter()
                    .filter(|(slips, _)| *slips == fewest)
                    .map(|(_, place)| place.clone())
                    .collect());
            }
        }
        Ok(Vec::new())
    }

    /// [`Self::big_cities`], read the first time.
    fn big_cities(&self) -> Result<&[(Vec<String>, Place)]> {
        if let Some(cities) = self.big_cities.get() {
            return Ok(cities);
        }
        let big =
            plumb_core::place::place_rank("place=city", MISTYPED_CITY_PEOPLE, false, false, 0);
        let searcher = self.reader.searcher();
        let city = Term::from_field_text(self.fields.kind, "place=city");
        let found = searcher.search(
            &TermQuery::new(city, IndexRecordOption::Basic),
            &TopDocs::with_limit(BIG_CITIES_READ)
                .order_by_fast_field::<u64>("rank", tantivy::Order::Desc),
        )?;
        let mut cities = Vec::new();
        for (rank, address) in found {
            if rank.is_none_or(|rank| rank < u64::from(big)) {
                break;
            }
            let place = self.stored(&searcher.doc(address)?)?;
            let keys = std::iter::once(&place.name)
                .chain(&place.aliases)
                .filter_map(|name| analysis::tokens(&self.joined, name).pop())
                .collect();
            cities.push((keys, place));
        }
        Ok(self.big_cities.get_or_init(|| cities))
    }

    /// Places whose name or other name is `name`.
    fn named(&self, name: &str) -> Result<Vec<Place>> {
        self.named_within(name, None)
    }

    /// Places called `name` within reach of the town `town` names
    /// ("union station" by Denver), or for "downtown" the one in that
    /// town, else the town itself.
    fn named_by_town(
        &self,
        name: &str,
        town: &str,
        preferred_country: Option<&str>,
        required_country: Option<&str>,
    ) -> Result<Vec<Place>> {
        let location = self.resolve_as_typed(town, preferred_country, required_country)?;
        let Some(town) = location.selected().filter(|town| town.is_town()) else {
            return Ok(Vec::new());
        };
        let km = town_size(&town.kind).unwrap_or(LANDMARK_KM) * 3.0;
        let mut found = self.named_within(name, Some((town, km)))?;
        // A town's downtown is in it, not the Downtown of a suburb in
        // reach: "hotels in downtown chicago" is not Oak Park's.
        if DOWNTOWN.contains(&name) {
            let names: Vec<String> = std::iter::once(&town.name)
                .chain(&town.aliases)
                .map(|name| joined(name))
                .collect();
            found.retain(|place| {
                place
                    .town
                    .as_deref()
                    .is_some_and(|of| names.contains(&joined(of)))
            });
            if found.is_empty() {
                found.push(town.clone());
            }
        }
        Ok(found)
    }

    /// Places whose name or other name is `name`, the most notable first,
    /// only those within `km` of a place when `near` gives one. Names are
    /// kept without their accents, and German written without them puts
    /// an "e" after the vowel: "muenchen" is München. That reading is
    /// tried only when the name as typed names nothing, as Boerne is no
    /// Borne.
    fn named_within(&self, name: &str, near: Option<(&Place, f64)>) -> Result<Vec<Place>> {
        let typed = normalize_text(name);
        let plain = without_umlauts(&typed);
        let searcher = self.reader.searcher();
        for name in [&typed, &plain] {
            let Some(key) = self.key_query(name) else {
                continue;
            };
            let key: Box<dyn Query> = Box::new(key);
            let query = match near {
                None => key,
                Some((center, km)) => Box::new(BooleanQuery::new(vec![
                    (Occur::Must, key),
                    (Occur::Must, Box::new(self.cells_query(center, km))),
                ])),
            };
            let found = searcher.search(
                &query,
                &TopDocs::with_limit(TOWN_CANDIDATES)
                    .order_by_fast_field::<u64>("rank", tantivy::Order::Desc),
            )?;
            let mut places = Vec::new();
            for (_, address) in found {
                let place = self.stored(&searcher.doc(address)?)?;
                if near.is_none_or(|(c, km)| distance_km(c.lat, c.lon, place.lat, place.lon) <= km)
                {
                    places.push(place);
                }
            }
            if !places.is_empty() || plain == typed {
                return Ok(places);
            }
        }
        Ok(Vec::new())
    }

    /// Keeps of `places` found by `name` those named it as typed, and the
    /// rest only when clearly bigger: "st louis" is St. Louis, Missouri,
    /// not Saint-Louis, Senegal, but "st paul" is Saint Paul, Minnesota,
    /// not St. Paul, Alberta, and "banff" is the town, but "acadia" the
    /// national park, not a suburb of Calgary.
    fn prefer_as_typed(&self, name: &str, places: &mut Vec<Place>) {
        let typed = self.typed_keys(name);
        let as_typed = |place: &Place| {
            std::iter::once(&place.name)
                .chain(&place.aliases)
                .filter_map(|name| analysis::tokens(&self.joined, name).pop())
                .any(|key| typed.contains(&key))
        };
        let best = places
            .iter()
            .filter(|place| as_typed(place))
            .map(|place| town_score(place, None))
            .fold(f64::NEG_INFINITY, f64::max);
        places.retain(|place| {
            as_typed(place) || town_score(place, None) >= best + DISTINCT_LOCATION_TIERS
        });
    }

    /// Places whose name or other name is `name` ([`Self::typed_keys`]),
    /// `name` with its [`SHORT_WORDS`] written the other way ("st paul" is
    /// Saint Paul, and "fort worth" is Ft. Worth), or the national park of
    /// that name.
    fn key_query(&self, name: &str) -> Option<BooleanQuery> {
        let normalized = normalize_text(name);
        let swapped: Vec<&str> = normalized
            .split(' ')
            .map(|word| {
                SHORT_WORDS
                    .iter()
                    .find_map(|&(short, full)| {
                        (word == short)
                            .then_some(full)
                            .or((word == full).then_some(short))
                    })
                    .unwrap_or(word)
            })
            .collect();
        let swapped = swapped.join(" ");
        // A national park is often named without its "National Park":
        // "hotels near acadia", "mt rainier".
        let parks = [&normalized, &swapped]
            .into_iter()
            .filter(|name| !name.ends_with("national park"))
            .map(|name| format!("{name} national park"));
        let mut keys = self.typed_keys(name);
        for other in std::iter::once(swapped.clone()).chain(parks) {
            for key in self.typed_keys(&other) {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        let terms: Vec<(Occur, Box<dyn Query>)> = keys
            .iter()
            .map(|key| {
                let term = Term::from_field_text(self.fields.keys, key);
                let query = TermQuery::new(term, IndexRecordOption::Basic);
                (Occur::Should, Box::new(query) as Box<dyn Query>)
            })
            .collect();
        (!terms.is_empty()).then(|| BooleanQuery::new(terms))
    }

    /// The keys of `name` with and without a "the" before it: a search
    /// leaves out the "the" of "hotels in the hague", and "bronx ny" is The
    /// Bronx.
    fn typed_keys(&self, name: &str) -> Vec<String> {
        let name = normalize_text(name);
        if name.is_empty() {
            return Vec::new();
        }
        let other = match name.strip_prefix("the ") {
            Some(rest) => rest.to_string(),
            None => format!("the {name}"),
        };
        let mut keys: Vec<String> = Vec::new();
        for text in [name, other] {
            let key = analysis::tokens(&self.joined, &text).pop();
            if let Some(key) = key.filter(|key| !keys.contains(key)) {
                keys.push(key);
            }
        }
        keys
    }

    /// Places in the cells within `km` of `center`.
    fn cells_query(&self, center: &Place, km: f64) -> BooleanQuery {
        BooleanQuery::new(
            cells_around(center.lat, center.lon, km)
                .into_iter()
                .map(|c| {
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.cell, &c),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>,
                    )
                })
                .collect(),
        )
    }

    fn stored(&self, document: &TantivyDocument) -> Result<Place> {
        let stored = document
            .get_first(self.fields.place)
            .and_then(|v| v.as_str())
            .context("a place without its record")?;
        parse_place(stored)
    }

    /// The places within `km` of `lat`, `lon` that give a website, nearest
    /// first, at most `limit`: a node's local sites. Taken from the most
    /// notable `2 * limit` places there, which a website puts ahead of
    /// most.
    pub fn websites_near(
        &self,
        lat: f64,
        lon: f64,
        km: f64,
        limit: usize,
    ) -> Result<Vec<PlaceHit>> {
        let cells: Vec<(Occur, Box<dyn Query>)> = cells_around(lat, lon, km)
            .into_iter()
            .map(|c| {
                let term = Term::from_field_text(self.fields.cell, &c);
                let query = TermQuery::new(term, IndexRecordOption::Basic);
                (Occur::Should, Box::new(query) as Box<dyn Query>)
            })
            .collect();
        let searcher = self.reader.searcher();
        let found = searcher.search(
            &BooleanQuery::new(cells),
            &TopDocs::with_limit(limit.saturating_mul(2).max(1))
                .order_by_fast_field::<u64>("rank", tantivy::Order::Desc),
        )?;
        let mut hits = Vec::new();
        for (_, address) in found {
            let place = self.stored(&searcher.doc(address)?)?;
            let d = distance_km(lat, lon, place.lat, place.lon);
            if place.website.is_some() && d <= km {
                hits.push(PlaceHit { place, km: d });
            }
        }
        hits.sort_by(|a, b| a.km.total_cmp(&b.km));
        hits.truncate(limit);
        Ok(hits)
    }
}

/// What to look for when `what` finds nothing because of words that name
/// no kind of place: its kind words ("luxury hotels" -> "hotels"), then
/// the last of them, the kind the others describe ("pet friendly hotels"
/// -> "pet hotels" -> "hotels"). Nothing when every word is a kind's.
fn fewer_words(what: &str) -> Vec<String> {
    let words: Vec<&str> = what.split(' ').collect();
    let kinds: Vec<&str> = words.iter().copied().filter(|w| is_kind_word(w)).collect();
    let Some(last) = kinds.last() else {
        return Vec::new();
    };
    if kinds.len() == words.len() {
        return Vec::new();
    }
    let mut fewer = vec![kinds.join(" ")];
    if kinds.len() > 1 {
        fewer.push(last.to_string());
    }
    fewer
}

/// Whether `a` and `b` are one place mapped twice: close together, and
/// with the same name ("&" and "and" alike), address or website.
fn same_place(a: &Place, b: &Place) -> bool {
    if distance_km(a.lat, a.lon, b.lat, b.lon) > SAME_PLACE_KM {
        return false;
    }
    let name = |p: &Place| {
        normalize_text(&p.name)
            .split(' ')
            .filter(|w| *w != "and")
            .collect::<String>()
    };
    let site = |p: &Place| {
        p.website
            .as_deref()
            .and_then(|w| w.split("://").nth(1))
            .and_then(|rest| rest.split('/').next())
            .map(|host| host.trim_start_matches("www.").to_ascii_lowercase())
    };
    name(a) == name(b)
        || (a.address.is_some()
            && joined(a.address.as_deref().unwrap_or_default())
                == joined(b.address.as_deref().unwrap_or_default()))
        || (site(a).is_some() && site(a) == site(b))
}

/// How likely `place` is the one a searcher in `country` means by its
/// name: towns and landmarks before other places, bigger towns first,
/// towns in the searcher's country a little ahead.
fn town_score(place: &Place, country: Option<&str>) -> f64 {
    let mut rank = place.rank;
    // Brooklyn and Manhattan are boroughs of millions, not smaller than
    // Brooklyn, Ohio or Manhattan, Kansas: a borough or suburb of a city's
    // size ranks as a city of that size.
    let tier = |kind: &str, people| plumb_core::place::place_rank(kind, people, false, false, 0);
    if matches!(place.kind.as_str(), "place=borough" | "place=suburb")
        && rank >= tier(&place.kind, CITY_SIZED)
    {
        rank = rank - tier(&place.kind, 0) + tier("place=city", 0);
    }
    let mut score = f64::from(rank) / 1_000_000.0;
    if !place.is_town() && !place.is_landmark() {
        score -= 10.0;
    }
    if country.is_some_and(|country| country_matches(place, country)) {
        score += HOME_COUNTRY_TIERS;
    }
    score
}

fn resolve_candidates(
    text: &str,
    mut candidates: Vec<Place>,
    country: Option<&str>,
) -> LocationResolution {
    // Choose on intrinsic evidence first. A default preference can make
    // the nearby namesake appear first among alternatives, not resolved.
    candidates.sort_by(|a, b| town_score(b, None).total_cmp(&town_score(a, None)));
    let mut distinct = Vec::new();
    for place in candidates {
        if !distinct.iter().any(|other| same_place(&place, other)) {
            distinct.push(place);
        }
    }
    // Namesakes in one area are one place to search around: the Grand
    // Canyon and its station, Kansas City in Missouri and in Kansas.
    let ambiguous = distinct
        .iter()
        .skip(1)
        .find(|next| {
            distance_km(distinct[0].lat, distinct[0].lon, next.lat, next.lon) > SAME_AREA_KM
        })
        .is_some_and(|next| {
            town_score(&distinct[0], None) - town_score(next, None) < DISTINCT_LOCATION_TIERS
        });
    if ambiguous {
        distinct.sort_by(|a, b| town_score(b, country).total_cmp(&town_score(a, country)));
    }
    distinct.truncate(LOCATION_ALTERNATIVES);
    LocationResolution {
        requested: text.to_string(),
        status: if ambiguous {
            LocationStatus::AmbiguousLocation
        } else {
            LocationStatus::Resolved
        },
        candidates: distinct,
    }
}

fn country_matches(place: &Place, name: &str) -> bool {
    let normalized = plumb_core::normalize_country(name)
        .or_else(|| country_of_any_name(name).map(str::to_string));
    normalized.as_deref().is_some_and(|country| {
        place
            .country
            .as_deref()
            .is_some_and(|code| code.eq_ignore_ascii_case(country))
    })
}

/// Whether `qualifier` ("maine", "me", "france", "fr") is `place`'s
/// region or country.
fn place_is_in(place: &Place, qualifier: &str) -> bool {
    if region_matches(place, qualifier) || country_matches(place, qualifier) {
        return true;
    }
    // Region and country are independent constraints: "Portland Maine
    // US", "Paris Texas United States". Both must fit the same place.
    let words: Vec<&str> = qualifier.split_whitespace().collect();
    (1..words.len()).any(|at| {
        region_matches(place, &words[..at].join(" "))
            && country_matches(place, &words[at..].join(" "))
    })
}

/// `name` as German written without its accents would have it: "muenchen"
/// is "munchen", as München is kept.
fn without_umlauts(name: &str) -> String {
    name.replace("ae", "a")
        .replace("oe", "o")
        .replace("ue", "u")
}

fn region_matches(place: &Place, qualifier: &str) -> bool {
    let region_code = normalize_region(qualifier);
    // "paris, tex.": a state as the style books abbreviate it is a US one.
    let abbreviated = us_state_abbreviated(qualifier).filter(|_| {
        place
            .country
            .as_deref()
            .is_none_or(|country| country == "US")
    });
    if let Some(region) = &place.region {
        if normalize_text(region) == qualifier
            || region.eq_ignore_ascii_case(&region_code)
            || abbreviated.is_some_and(|code| region == code)
            || joined(region) == joined(qualifier)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests;
