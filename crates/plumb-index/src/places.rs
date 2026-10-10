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

use anyhow::{bail, Context, Result};
use plumb_core::place::{
    distance_km, is_kind_word, normalize_region, parse_place, town_size, write_place, Place,
};
use plumb_core::{country_of_name, joined, normalize_text};
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
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
/// Required intrinsic rank separation to choose an unqualified namesake.
/// Locale preference can order alternatives but cannot create this gap.
const DISTINCT_LOCATION_TIERS: f64 = 0.15;
/// Bounded location alternatives returned when the name is ambiguous.
const LOCATION_ALTERNATIVES: usize = 5;

/// Words a query may say "near me" with.
const NEAR_ME: &[&[&str]] = &[
    &["near", "me"],
    &["nearby"],
    &["near", "by"],
    &["around", "me"],
    &["close", "to", "me"],
    &["near", "my", "location"],
    &["in", "my", "area"],
];
/// Words that say where, in "pizza in denver".
const WHERE_WORDS: &[&[&str]] = &[&["in"], &["near"], &["around"], &["close", "to"], &["at"]];
/// Kinds of town a query may name without "in" or "near": "denver
/// pizza", "barber brooklyn".
const GUESSED_TOWNS: &[&str] = &["place=city", "place=borough"];
/// Words ending a query that say when, not where: "open now".
const WHEN: &[&[&str]] = &[
    &["open", "now"],
    &["open", "late"],
    &["open", "today"],
    &["open", "24", "hours"],
    &["24", "hours"],
    &["24", "7"],
    &["now"],
    &["today"],
    &["tonight"],
];
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
/// checks the town).
pub fn parse_place_query(query: &str) -> Option<PlaceQuery> {
    let mut words: Vec<String> = normalize_text(query)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    // "plumber boston open now": when is not where.
    let mut said_when = false;
    while let Some(when) = WHEN
        .iter()
        .find(|when| words.len() > when.len() && words[words.len() - when.len()..] == ***when)
    {
        words.truncate(words.len() - when.len());
        said_when = true;
    }
    if words
        .iter()
        .map(String::as_str)
        .find(|w| !FILLER.contains(w))
        .is_some_and(|w| ABOUT_TOWN.contains(&w))
    {
        return None;
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
    let ends_with = |suffix: &[&str]| {
        words.len() > suffix.len() && words[words.len() - suffix.len()..] == *suffix
    };
    for suffix in NEAR_ME {
        if ends_with(suffix) {
            return Some(PlaceQuery {
                what: what_of(&words[..words.len() - suffix.len()])?,
                near: Near::Me,
                said_where: true,
            });
        }
    }
    if words.first().is_some_and(|w| w == "nearby") && words.len() > 1 {
        return Some(PlaceQuery {
            what: what_of(&words[1..])?,
            near: Near::Me,
            said_where: true,
        });
    }
    // The last "in" or "near" with words on both sides.
    for at in (1..words.len().saturating_sub(1)).rev() {
        for marker in WHERE_WORDS {
            let end = at + marker.len();
            if end < words.len() && words[at..end] == **marker {
                let what = what_of(&words[..at])?;
                let mut place = &words[end..];
                if place.first().is_some_and(|w| w == "the") && place.len() > 1 {
                    place = &place[1..];
                }
                return Some(PlaceQuery {
                    what,
                    near: Near::Named(place.join(" ")),
                    said_where: true,
                });
            }
        }
    }
    // "denver pizza", "pizza denver": a town on one side, kind words on the
    // other.
    // A country after the kind still qualifies the city: "Rome museums
    // Italy", rather than treating "Italy" as a town and "Rome" as part
    // of the business name. Only country names, not ambiguous state codes.
    for count in (1..=3.min(words.len().saturating_sub(2))).rev() {
        let at = words.len() - count;
        let country = words[at..].join(" ");
        if country_of_name(&country).is_some() {
            if let Some(mut asked) = parse_place_query(&words[..at].join(" ")) {
                if let Near::Named(name) = &mut asked.near {
                    name.push(' ');
                    name.push_str(&country);
                    return Some(asked);
                }
            }
        }
    }
    if words.len() >= 2 {
        for town_words in (1..=3.min(words.len() - 1)).rev() {
            let (what, town) = words.split_at(words.len() - town_words);
            // What is looked for ends in a kind of place: "pizza", "best
            // climbing gym", "cheap bookstore".
            if what.last().is_some_and(|w| is_kind_word(w)) {
                if let Some(what) = what_of(what) {
                    return Some(PlaceQuery {
                        what,
                        near: Near::Named(town.join(" ")),
                        said_where: false,
                    });
                }
            }
            let (town, what) = words.split_at(town_words);
            if what.iter().all(|w| is_kind_word(w)) {
                return Some(PlaceQuery {
                    what: what.join(" "),
                    near: Near::Named(town.join(" ")),
                    said_where: false,
                });
            }
        }
    }
    // "coffee open now", "pharmacy open late": a kind of place asked for
    // at a time, with no town, is near the searcher.
    if said_when && words.last().is_some_and(|w| is_kind_word(w)) {
        return Some(PlaceQuery {
            what: what_of(&words)?,
            near: Near::Me,
            said_where: true,
        });
    }
    None
}

/// `query` without the "near me" that ends it ("safeway near me" ->
/// "safeway"), for searching sites: "me" names no site (domain.me,
/// maine.gov). `None` when it has none.
pub fn without_near_me(query: &str) -> Option<String> {
    let words: Vec<String> = normalize_text(query)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    NEAR_ME.iter().find_map(|suffix| {
        (words.len() > suffix.len() && words[words.len() - suffix.len()..] == **suffix)
            .then(|| words[..words.len() - suffix.len()].join(" "))
    })
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
        })
    }

    pub fn num_places(&self) -> u64 {
        self.stats.places
    }

    /// The places `query` asks for, when it asks for places somewhere:
    /// around `home`, the searcher's town as they typed it, for "near me";
    /// with `country` ordering alternatives for an ambiguous name.
    /// `None` when the query does not ask for places. Explicit unknown
    /// locations return a coverage status instead of a guessed town.
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
        let asked = match parse_place_query(query) {
            Some(asked) => Some(asked),
            None => self.named_business_query(query, preferred_country, required_country)?,
        };
        let Some(asked) = asked else {
            return Ok(None);
        };
        let location = match &asked.near {
            Near::Me => match home {
                Some(home) => self.resolve(home, preferred_country, required_country)?,
                None => LocationResolution {
                    status: LocationStatus::MissingLocation,
                    requested: String::new(),
                    candidates: Vec::new(),
                },
            },
            Near::Named(name) => self.resolve(name, preferred_country, required_country)?,
        };
        let center = location.selected().cloned();
        if let Some(place) = &center {
            // A town guessed from words ("toy story", "hotel
            // california", "crypto exchange") is only taken for a big
            // one: Story in France and Crypto in Poland are villages
            // whose names are words.
            if !asked.said_where && !GUESSED_TOWNS.contains(&place.kind.as_str()) {
                return Ok(None);
            }
        } else if !asked.said_where && location.status == LocationStatus::UnknownLocation {
            return Ok(None);
        }
        let near_me = asked.near == Near::Me;
        let guessed = !asked.said_where;
        let Some(center) = center else {
            return Ok(Some(PlaceResults {
                what: asked.what,
                center: None,
                near_me,
                guessed,
                radius_km: 0.0,
                hits: Vec::new(),
                location: Some(location),
            }));
        };
        let radius = town_size(&center.kind).unwrap_or(LANDMARK_KM);
        let mut hits = self.around(&asked.what, &center, radius, required_country, limit)?;
        let mut radius_km = radius;
        if hits.len() < FEW.min(limit) {
            // A small town: look a little farther.
            radius_km = radius * 3.0;
            hits = self.around(&asked.what, &center, radius_km, required_country, limit)?;
        }
        // A resolved location with no matching businesses is an indexed
        // coverage result too, including explicit landmark searches.
        Ok(Some(PlaceResults {
            what: asked.what,
            center: Some(center),
            near_me,
            guessed,
            radius_km,
            hits,
            location: Some(location),
        }))
    }

    /// A named business followed by an actually indexed city: "Poilâne
    /// Paris". No geographic interpretation is made for an unknown tail.
    fn named_business_query(
        &self,
        query: &str,
        preferred_country: Option<&str>,
        required_country: Option<&str>,
    ) -> Result<Option<PlaceQuery>> {
        let normalized = normalize_text(query);
        let words: Vec<&str> = normalized.split_whitespace().collect();
        if !(2..=6).contains(&words.len()) || ABOUT_TOWN.contains(&words[0]) {
            return Ok(None);
        }
        for count in (1..=3.min(words.len() - 1)).rev() {
            let (what, near) = words.split_at(words.len() - count);
            let name = near.join(" ");
            let location = self.resolve(&name, preferred_country, required_country)?;
            if !location.candidates.is_empty()
                && location
                    .candidates
                    .iter()
                    .all(|place| GUESSED_TOWNS.contains(&place.kind.as_str()))
            {
                return Ok(Some(PlaceQuery {
                    what: what.join(" "),
                    near: Near::Named(name),
                    said_where: false,
                }));
            }
        }
        Ok(None)
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
        let cells: Vec<(Occur, Box<dyn Query>)> = cells_around(center.lat, center.lon, km)
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
            .collect();
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(cells))));
        let searcher = self.reader.searcher();
        let found = searcher.search(
            &BooleanQuery::new(clauses),
            &TopDocs::with_limit(CANDIDATES)
                .order_by_fast_field::<u64>("rank", tantivy::Order::Desc),
        )?;
        // "park" is also the stem of "parking".
        let parking = ["parking", "garage", "car park"]
            .iter()
            .any(|word| what.contains(word));
        let mut hits = Vec::new();
        for (_, address) in found {
            let document: TantivyDocument = searcher.doc(address)?;
            let place = self.stored(&document)?;
            if required_country.is_some_and(|country| !country_matches(&place, country)) {
                continue;
            }
            if place.is_town() || (place.kind == "amenity=parking" && !parking) {
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
        let is_kind = |place: &Place| {
            let words: HashSet<String> = analysis::tokens(&self.stemmed, &place.kind_words())
                .into_iter()
                .collect();
            stems.iter().all(|stem| words.contains(stem))
        };
        let key = |hit: &PlaceHit| {
            let tier = f64::from(hit.place.rank / 1_000_000).clamp(1.0, 6.0);
            let named_only = if is_kind(&hit.place) { 0.0 } else { 1e6 };
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
        Ok(self.resolve(text, country, None)?.selected().cloned())
    }

    /// Resolves the name without allowing a default-country boost to
    /// override explicit qualifiers or decide a close namesake tie.
    pub fn resolve(
        &self,
        text: &str,
        preferred_country: Option<&str>,
        required_country: Option<&str>,
    ) -> Result<LocationResolution> {
        // Country-only intent cannot select a foreign town of that name:
        // Italy, Texas or Us, France. A city-state can still resolve to
        // an actual town within that country (Singapore).
        let named_country = country_of_name(text);
        let words: Vec<&str> = text
            .split(|c: char| c.is_whitespace() || c == ',')
            .collect();
        let words: Vec<String> = normalize_text(&words.join(" "))
            .split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect();
        // A town first, however the words split ("boulder co" is Boulder in
        // Colorado before it is a gym called Boulder & Co.), then any place.
        let mut other = None;
        let mut conflict = false;
        for k in (1..=words.len()).rev() {
            let name = words[..k].join(" ");
            let qualifier = words[k..].join(" ");
            let matching: Vec<Place> = self
                .named(&name)?
                .into_iter()
                .filter(|place| {
                    (qualifier.is_empty() || place_is_in(place, &qualifier))
                        && named_country.is_none_or(|country| country_matches(place, country))
                })
                .collect();
            let mut candidates: Vec<Place> = matching
                .iter()
                .filter(|place| {
                    required_country.is_none_or(|country| country_matches(place, country))
                })
                .cloned()
                .collect();
            conflict |= !qualifier.is_empty() && !matching.is_empty() && candidates.is_empty();
            if candidates.iter().any(Place::is_town) {
                candidates.retain(Place::is_town);
                return Ok(resolve_candidates(text, candidates, preferred_country));
            }
            if !candidates.is_empty() && other.is_none() {
                other = Some(resolve_candidates(text, candidates, preferred_country));
            }
        }
        Ok(other.unwrap_or_else(|| LocationResolution {
            requested: text.to_string(),
            status: if conflict {
                LocationStatus::ConflictingConstraints
            } else if named_country.is_some() {
                LocationStatus::MissingLocation
            } else {
                LocationStatus::UnknownLocation
            },
            candidates: Vec::new(),
        }))
    }

    /// Places whose name or other name is `name`.
    fn named(&self, name: &str) -> Result<Vec<Place>> {
        let Some(key) = analysis::tokens(&self.joined, name).pop() else {
            return Ok(Vec::new());
        };
        let searcher = self.reader.searcher();
        let found = searcher.search(
            &TermQuery::new(
                Term::from_field_text(self.fields.keys, &key),
                IndexRecordOption::Basic,
            ),
            &TopDocs::with_limit(TOWN_CANDIDATES)
                .order_by_fast_field::<u64>("rank", tantivy::Order::Desc),
        )?;
        found
            .into_iter()
            .map(|(_, address)| self.stored(&searcher.doc(address)?))
            .collect()
    }

    fn stored(&self, document: &TantivyDocument) -> Result<Place> {
        let stored = document
            .get_first(self.fields.place)
            .and_then(|v| v.as_str())
            .context("a place without its record")?;
        parse_place(stored)
    }
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
/// name: towns before other places, bigger towns first, towns in the
/// searcher's country a little ahead.
fn town_score(place: &Place, country: Option<&str>) -> f64 {
    let mut score = f64::from(place.rank) / 1_000_000.0;
    if !place.is_town() {
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
    let ambiguous = distinct.get(1).is_some_and(|next| {
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
    let normalized =
        plumb_core::normalize_country(name).or_else(|| country_of_name(name).map(str::to_string));
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

fn region_matches(place: &Place, qualifier: &str) -> bool {
    let region_code = normalize_region(qualifier);
    if let Some(region) = &place.region {
        if normalize_text(region) == qualifier
            || region.eq_ignore_ascii_case(&region_code)
            || joined(region) == joined(qualifier)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_core::place::place_rank;

    fn query(text: &str) -> Option<(String, Near)> {
        parse_place_query(text).map(|q| (q.what, q.near))
    }

    #[test]
    fn queries_say_what_and_where() {
        assert_eq!(
            query("pizza in denver"),
            Some(("pizza".into(), Near::Named("denver".into())))
        );
        assert_eq!(
            query("best coffee near me"),
            Some(("coffee".into(), Near::Me))
        );
        assert_eq!(query("Coffee nearby"), Some(("coffee".into(), Near::Me)));
        assert_eq!(
            query("hotels near the eiffel tower"),
            Some(("hotels".into(), Near::Named("eiffel tower".into())))
        );
        assert_eq!(
            query("places to eat in boulder co"),
            Some(("eat".into(), Near::Named("boulder co".into())))
        );
        assert_eq!(
            query("denver pizza"),
            Some(("pizza".into(), Near::Named("denver".into())))
        );
        assert_eq!(
            query("sushi san francisco"),
            Some(("sushi".into(), Near::Named("san francisco".into())))
        );
        assert_eq!(
            query("best climbing gym seattle"),
            Some(("climbing gym".into(), Near::Named("seattle".into())))
        );
        assert_eq!(
            query("plumber in boston open now"),
            Some(("plumber".into(), Near::Named("boston".into())))
        );
        assert_eq!(
            query("pizza denver open now"),
            Some(("pizza".into(), Near::Named("denver".into())))
        );
        assert_eq!(
            query("plumber boston open now"),
            Some(("plumber".into(), Near::Named("boston".into())))
        );
        // A kind of place at a time, with no town, is near the searcher.
        assert_eq!(query("coffee open now"), Some(("coffee".into(), Near::Me)));
        assert_eq!(
            query("pharmacy open late"),
            Some(("pharmacy".into(), Near::Me))
        );
        assert_eq!(query("pizza tonight"), Some(("pizza".into(), Near::Me)));
        // Not for words that are no kind of place.
        assert_eq!(query("chrome open now"), None);
        assert_eq!(query("news today"), None);
        // Only "in", "near" and "near me" say where for sure.
        assert!(parse_place_query("pizza in denver").unwrap().said_where);
        assert!(parse_place_query("coffee near me").unwrap().said_where);
        assert!(!parse_place_query("denver pizza").unwrap().said_where);
        assert!(!parse_place_query("us bank").unwrap().said_where);
        for plain in [
            "pizza",
            "apple music",
            "new york times",
            "in",
            "near me",
            "log in",
            "open now",
            "leonardo dicaprio",
            "tim cook",
            // About the town, not places in it.
            "time in tokyo",
            "weather in denver",
            "best time to visit seattle",
            "capital of washington",
            "capital of singapore",
        ] {
            assert_eq!(query(plain), None, "{plain}");
        }
    }

    fn at(name: &str, kind: &str, lat: f64, lon: f64) -> Place {
        Place {
            rank: place_rank(kind, 0, false, false, 3),
            name: name.into(),
            kind: kind.into(),
            lat,
            lon,
            osm: "n1".into(),
            ..Place::default()
        }
    }

    fn city(name: &str, population: u64, region: &str, country: &str, lat: f64, lon: f64) -> Place {
        Place {
            rank: place_rank("place=city", population, false, false, 3),
            region: Some(region.into()),
            country: Some(country.into()),
            ..at(name, "place=city", lat, lon)
        }
    }

    fn index(places: Vec<Place>) -> (tempfile::TempDir, PlaceSearcher) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("places");
        build_place_index(&path, places).unwrap();
        let searcher = PlaceSearcher::open(&path).unwrap();
        (dir, searcher)
    }

    fn places() -> Vec<Place> {
        vec![
            city("Denver", 715_000, "CO", "US", 39.7392, -104.9903),
            city("Portland", 650_000, "OR", "US", 45.5152, -122.6784),
            city("Portland", 68_000, "ME", "US", 43.6591, -70.2568),
            city("Paris", 2_100_000, "Île-de-France", "FR", 48.8566, 2.3522),
            city("Paris", 25_000, "TX", "US", 33.6609, -95.5555),
            Place {
                rank: place_rank("place=village", 300, false, false, 3),
                country: Some("FR".into()),
                ..at("Story", "place=village", 46.0, 3.0)
            },
            at("King Jouet", "shop=toys", 46.001, 3.001),
            Place {
                tags: vec!["cuisine=pizza".into()],
                address: Some("1 Main St".into()),
                ..at("Blue Pan", "amenity=restaurant", 39.75, -104.98)
            },
            Place {
                rank: place_rank("amenity=fast_food", 0, false, true, 9),
                ..at("Pizza Hut", "amenity=fast_food", 39.70, -104.95)
            },
            at("Huckleberry and Co", "amenity=cafe", 39.76, -104.99),
            // The same café mapped twice, and a car park.
            Place {
                name: "Huckleberry & Co".into(),
                ..at("Huckleberry and Co", "amenity=cafe", 39.7601, -104.9901)
            },
            at(
                "Union Station Park-n-Ride",
                "amenity=parking",
                39.75,
                -104.99,
            ),
            at("Commons Park", "leisure=park", 39.755, -105.0),
            // Too far from Denver.
            at("Boulder Pizza", "amenity=restaurant", 40.0150, -105.2705),
            at("Pizza Port", "amenity=restaurant", 45.52, -122.67),
            at("Tour Eiffel", "tourism=attraction", 48.8584, 2.2945),
            Place {
                aliases: vec!["NYC".into()],
                ..city("New York", 8_800_000, "NY", "US", 40.7128, -74.0060)
            },
            at("Hotel Eiffel", "tourism=hotel", 48.857, 2.296),
            // Named like a hotel, nearer, but a manor.
            at("Hôtel de Béhague", "historic=manor", 48.8584, 2.2946),
            // A gym whose name spells a town and its state.
            Place {
                country: Some("IT".into()),
                ..at("Boulder & Co.", "leisure=sports_centre", 45.5, 9.3)
            },
            city("Boulder", 108_000, "CO", "US", 40.0150, -105.2705),
        ]
    }

    #[test]
    fn places_are_found_around_a_town() {
        let (_dir, searcher) = index(places());
        let found = searcher
            .search("pizza in denver", None, None, 5)
            .unwrap()
            .unwrap();
        assert_eq!(found.center.as_ref().unwrap().name, "Denver");
        let names: Vec<&str> = found.hits.iter().map(|h| h.place.name.as_str()).collect();
        assert_eq!(names, ["Blue Pan", "Pizza Hut"]);
        assert!(found.hits[0].km < 2.0);
        let coffee = searcher
            .search("coffee shop in denver", None, None, 5)
            .unwrap()
            .unwrap();
        assert_eq!(coffee.hits[0].place.name, "Huckleberry and Co");
        assert_eq!(coffee.hits.len(), 1);
        let parks = searcher
            .search("park in denver", None, None, 5)
            .unwrap()
            .unwrap();
        let names: Vec<&str> = parks.hits.iter().map(|h| h.place.name.as_str()).collect();
        assert_eq!(names, ["Commons Park"]);
        let parking = searcher
            .search("parking in denver", None, None, 5)
            .unwrap()
            .unwrap();
        assert_eq!(parking.hits[0].place.name, "Union Station Park-n-Ride");
        // Nothing of the kind around a town: still a search there, with no
        // places to list.
        let gyms = searcher
            .search("climbing gym in denver", None, None, 5)
            .unwrap()
            .unwrap();
        assert!(gyms.hits.is_empty());
        assert_eq!(gyms.center.unwrap().name, "Denver");
        // The searcher's own town, for "near me".
        let near = searcher
            .search("coffee near me", Some("Denver, CO"), None, 5)
            .unwrap()
            .unwrap();
        assert!(near.near_me);
        assert_eq!(near.hits.len(), 1);
        let nowhere = searcher
            .search("coffee near me", None, None, 5)
            .unwrap()
            .unwrap();
        assert!(nowhere.center.is_none() && nowhere.hits.is_empty());
        // A guessed town must be a city: "pizza paris" is Paris, France,
        // but a village called Story is not "toy story".
        assert!(searcher
            .search("denver pizza", None, None, 5)
            .unwrap()
            .is_some());
        assert!(searcher
            .search("toy story", None, None, 5)
            .unwrap()
            .is_none());
        let toys = searcher
            .search("toys in story", None, None, 5)
            .unwrap()
            .unwrap();
        assert_eq!(toys.hits[0].place.name, "King Jouet");
        assert_eq!(
            without_near_me("Safeway near me").as_deref(),
            Some("safeway")
        );
        assert_eq!(without_near_me("safeway"), None);
        // No town of that name, or nothing of that kind there.
        let missing = searcher
            .search("pizza in gotham", None, None, 5)
            .unwrap()
            .unwrap();
        assert_eq!(
            missing.location.unwrap().status,
            LocationStatus::UnknownLocation
        );
        assert!(searcher.search("pizza", None, None, 5).unwrap().is_none());
        // Landmarks work too.
        let hotels = searcher
            .search("hotels near the tour eiffel", None, None, 5)
            .unwrap()
            .unwrap();
        assert_eq!(hotels.hits[0].place.name, "Hotel Eiffel");
        assert_eq!(hotels.hits[1].place.name, "Hôtel de Béhague");
    }

    #[test]
    fn towns_of_the_same_name_are_told_apart() {
        let (_dir, searcher) = index(places());
        let locate = |text: &str, country: Option<&str>| {
            let place = searcher.locate(text, country).unwrap().unwrap();
            (place.name, place.region.unwrap_or_default())
        };
        assert!(searcher.locate("portland", None).unwrap().is_none());
        assert_eq!(locate("portland maine", None).1, "ME");
        assert_eq!(locate("Portland, ME", None).1, "ME");
        assert_eq!(locate("paris", Some("US")).1, "Île-de-France");
        assert_eq!(locate("paris tx", None).1, "TX");
        assert_eq!(locate("paris france", Some("US")).1, "Île-de-France");
        assert_eq!(locate("nyc", None).0, "New York");
        assert_eq!(locate("Boulder, CO", None), ("Boulder".into(), "CO".into()));
        assert!(searcher.locate("atlantis", None).unwrap().is_none());
    }

    #[test]
    fn locale_preference_cannot_change_rome_to_a_us_namesake() {
        let mut fixtures = places();
        fixtures.extend([
            city("Rome", 1_000_000, "Lazio", "IT", 41.90, 12.50),
            city("Rome", 37_000, "GA", "US", 34.26, -85.16),
            at("Capitoline Museum", "tourism=museum", 41.901, 12.501),
            at("Poilâne", "shop=bakery", 48.857, 2.352),
            city("Italy", 2_000, "TX", "US", 32.18, -96.88),
        ]);
        let (_dir, searcher) = index(fixtures);
        let museums = searcher
            .search("Rome museums", None, Some("US"), 5)
            .unwrap()
            .unwrap();
        assert_eq!(museums.center.unwrap().country.as_deref(), Some("IT"));
        assert_eq!(museums.hits[0].place.name, "Capitoline Museum");
        let qualified = searcher
            .search("Rome museums Italy", None, Some("US"), 5)
            .unwrap()
            .unwrap();
        assert_eq!(qualified.center.unwrap().country.as_deref(), Some("IT"));
        assert_eq!(qualified.hits[0].place.name, "Capitoline Museum");
        let country_only = searcher
            .search("museums in Italy", None, Some("US"), 5)
            .unwrap()
            .unwrap();
        assert!(country_only.center.is_none());
        assert_eq!(
            country_only.location.unwrap().status,
            LocationStatus::MissingLocation
        );
        let bakery = searcher
            .search("Poilâne Paris", None, Some("US"), 5)
            .unwrap()
            .unwrap();
        assert_eq!(bakery.center.unwrap().country.as_deref(), Some("FR"));
        assert_eq!(bakery.hits[0].place.name, "Poilâne");
    }

    #[test]
    fn ambiguous_towns_and_explicit_constraints_remain_visible() {
        let (_dir, searcher) = index(places());
        let ambiguous = searcher.resolve("Portland", Some("US"), None).unwrap();
        assert_eq!(ambiguous.status, LocationStatus::AmbiguousLocation);
        assert!(ambiguous.selected().is_none());
        assert_eq!(ambiguous.candidates.len(), 2);
        let found = searcher
            .search("pizza in Portland", None, Some("US"), 5)
            .unwrap()
            .unwrap();
        assert!(found.center.is_none() && found.hits.is_empty());
        assert_eq!(
            found.location.unwrap().status,
            LocationStatus::AmbiguousLocation
        );
        for query in ["Portland Maine US", "Portland Maine United States"] {
            let explicit = searcher.resolve(query, Some("FR"), None).unwrap();
            assert_eq!(explicit.selected().unwrap().region.as_deref(), Some("ME"));
        }
        let texas = searcher
            .resolve("Paris Texas US", Some("FR"), None)
            .unwrap();
        assert_eq!(texas.selected().unwrap().region.as_deref(), Some("TX"));
        let country = searcher.resolve("Paris", Some("FR"), Some("US")).unwrap();
        assert_eq!(country.selected().unwrap().region.as_deref(), Some("TX"));
        let conflict = searcher
            .search_in_country("pizza in Paris France", None, Some("FR"), Some("US"), 5)
            .unwrap()
            .unwrap();
        assert!(conflict.center.is_none() && conflict.hits.is_empty());
        assert_eq!(
            conflict.location.unwrap().status,
            LocationStatus::ConflictingConstraints
        );
        let missing = searcher
            .search("pizza near me", None, Some("US"), 5)
            .unwrap()
            .unwrap();
        assert_eq!(
            missing.location.unwrap().status,
            LocationStatus::MissingLocation
        );
        assert!(searcher
            .search("Poilâne Atlantis", None, Some("US"), 5)
            .unwrap()
            .is_none());
    }

    #[test]
    fn cells_wrap_around_the_world() {
        let cells = cells_around(0.0, 179.99, 12.0);
        assert!(cells.iter().any(|c| c.ends_with(":-1800")));
        assert!(cells.contains(&cell(0.0, 179.99)));
    }
}
