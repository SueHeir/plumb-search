//! The index of places (see [`plumb_core::place`]), searched for queries
//! that say where: "pizza in denver", "coffee near me", "hotels near the
//! eiffel tower", "denver pizza".
//!
//! [`parse_place_query`] splits such a query into what is looked for and
//! where. Where is a town (or any named place) found by its name or another
//! name, optionally followed by its region or country ("portland maine",
//! "paris france"); among towns of the same name the bigger one wins, and
//! one in the searcher's country a little more. "Near me" is the town the
//! searcher gave Plumb, never worked out from their address.
//!
//! What is looked for must be in a place's name or among the words for its
//! kind (`amenity=cafe` is found by "café", "coffee" and "coffee shop").
//! Places within the town's size ([`town_size`]) of its centre are listed,
//! nearest first, those with a website, brand or Wikidata item a little
//! ahead.

use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::place::{
    distance_km, is_kind_word, normalize_region, parse_place, town_size, write_place, Place,
};
use plumb_core::{country_of_name, joined, normalize_text};
use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
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
}

/// Splits `query` into what is looked for and where, when it says where.
/// A query without "in", "near" or "near me" says where only when it is a
/// kind of place and a town: "denver pizza" ([`PlaceSearcher::search`]
/// checks the town).
pub fn parse_place_query(query: &str) -> Option<PlaceQuery> {
    let words: Vec<String> = normalize_text(query)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
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
            });
        }
    }
    if words.first().is_some_and(|w| w == "nearby") && words.len() > 1 {
        return Some(PlaceQuery {
            what: what_of(&words[1..])?,
            near: Near::Me,
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
                });
            }
        }
    }
    // "denver pizza", "pizza denver": a town on one side, kind words on the
    // other.
    if words.len() >= 2 {
        for town_words in (1..=3.min(words.len() - 1)).rev() {
            let (what, town) = words.split_at(words.len() - town_words);
            if what
                .iter()
                .all(|w| is_kind_word(w) || FILLER.contains(&w.as_str()))
            {
                if let Some(what) = what_of(what) {
                    return Some(PlaceQuery {
                        what,
                        near: Near::Named(town.join(" ")),
                    });
                }
            }
            let (town, what) = words.split_at(town_words);
            if what.iter().all(|w| is_kind_word(w)) {
                return Some(PlaceQuery {
                    what: what.join(" "),
                    near: Near::Named(town.join(" ")),
                });
            }
        }
    }
    None
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
    /// How far around the centre it looked, in km.
    pub radius_km: f64,
    pub hits: Vec<PlaceHit>,
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
    let staging = Staging::new(dir)?;
    let (schema, fields) = schema();
    let index = Index::create_in_dir(staging.path(), schema)
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
    drop(writer);
    std::fs::write(
        staging.path().join("places.json"),
        serde_json::to_vec(&stats)?,
    )?;
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
    /// with towns in `country` a little ahead of others of their name.
    /// `None` when the query does not ask for places, or names no place
    /// this index knows.
    pub fn search(
        &self,
        query: &str,
        home: Option<&str>,
        country: Option<&str>,
        limit: usize,
    ) -> Result<Option<PlaceResults>> {
        let Some(asked) = parse_place_query(query) else {
            return Ok(None);
        };
        let center = match &asked.near {
            Near::Me => match home {
                Some(home) => self.locate(home, country)?,
                None => None,
            },
            Near::Named(name) => match self.locate(name, country)? {
                Some(place) => Some(place),
                None => return Ok(None),
            },
        };
        let near_me = asked.near == Near::Me;
        let Some(center) = center else {
            return Ok(Some(PlaceResults {
                what: asked.what,
                center: None,
                near_me,
                radius_km: 0.0,
                hits: Vec::new(),
            }));
        };
        let radius = town_size(&center.kind).unwrap_or(LANDMARK_KM);
        let mut hits = self.around(&asked.what, &center, radius, limit)?;
        let mut radius_km = radius;
        if hits.len() < FEW.min(limit) {
            // A small town: look a little farther.
            radius_km = radius * 3.0;
            hits = self.around(&asked.what, &center, radius_km, limit)?;
        }
        if hits.is_empty() && !near_me {
            return Ok(None);
        }
        Ok(Some(PlaceResults {
            what: asked.what,
            center: Some(center),
            near_me,
            radius_km,
            hits,
        }))
    }

    /// The best `limit` places matching `what` within `km` of `center`.
    fn around(&self, what: &str, center: &Place, km: f64, limit: usize) -> Result<Vec<PlaceHit>> {
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
        let key = |hit: &PlaceHit| {
            let tier = f64::from(hit.place.rank / 1_000_000).clamp(1.0, 6.0);
            hit.km / (1.0 + 0.1 * (tier - 1.0))
        };
        hits.sort_by(|a, b| key(a).total_cmp(&key(b)));
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
        let words: Vec<&str> = text
            .split(|c: char| c.is_whitespace() || c == ',')
            .collect();
        let words: Vec<String> = normalize_text(&words.join(" "))
            .split(' ')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect();
        for k in (1..=words.len()).rev() {
            let name = words[..k].join(" ");
            let qualifier = words[k..].join(" ");
            let candidates = self.named(&name)?;
            let fits = |place: &Place| qualifier.is_empty() || place_is_in(place, &qualifier);
            let best = candidates
                .into_iter()
                .filter(|place| fits(place))
                .max_by(|a, b| town_score(a, country).total_cmp(&town_score(b, country)));
            if best.is_some() {
                return Ok(best);
            }
        }
        Ok(None)
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
    if country.is_some() && place.country.as_deref() == country {
        score += HOME_COUNTRY_TIERS;
    }
    score
}

/// Whether `qualifier` ("maine", "me", "france", "fr") is `place`'s
/// region or country.
fn place_is_in(place: &Place, qualifier: &str) -> bool {
    let region_code = normalize_region(qualifier);
    if let Some(region) = &place.region {
        if normalize_text(region) == qualifier
            || region.eq_ignore_ascii_case(&region_code)
            || joined(region) == joined(qualifier)
        {
            return true;
        }
    }
    let Some(country) = &place.country else {
        return false;
    };
    country.eq_ignore_ascii_case(qualifier) || country_of_name(qualifier) == Some(country.as_str())
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
        for plain in [
            "pizza",
            "apple music",
            "new york times",
            "in",
            "near me",
            "log in",
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
        // No town of that name, or nothing of that kind there.
        assert!(searcher
            .search("pizza in gotham", None, None, 5)
            .unwrap()
            .is_none());
        assert!(searcher.search("pizza", None, None, 5).unwrap().is_none());
        // Landmarks work too.
        let hotels = searcher
            .search("hotels near the tour eiffel", None, None, 5)
            .unwrap()
            .unwrap();
        assert_eq!(hotels.hits[0].place.name, "Hotel Eiffel");
    }

    #[test]
    fn towns_of_the_same_name_are_told_apart() {
        let (_dir, searcher) = index(places());
        let locate = |text: &str, country: Option<&str>| {
            let place = searcher.locate(text, country).unwrap().unwrap();
            (place.name, place.region.unwrap_or_default())
        };
        assert_eq!(locate("portland", None).1, "OR");
        assert_eq!(locate("portland maine", None).1, "ME");
        assert_eq!(locate("Portland, ME", None).1, "ME");
        assert_eq!(locate("paris", Some("US")).1, "Île-de-France");
        assert_eq!(locate("paris tx", None).1, "TX");
        assert_eq!(locate("paris france", Some("US")).1, "Île-de-France");
        assert_eq!(locate("nyc", None).0, "New York");
        assert!(searcher.locate("atlantis", None).unwrap().is_none());
    }

    #[test]
    fn cells_wrap_around_the_world() {
        let cells = cells_around(0.0, 179.99, 12.0);
        assert!(cells.iter().any(|c| c.ends_with(":-1800")));
        assert!(cells.contains(&cell(0.0, 179.99)));
    }
}
