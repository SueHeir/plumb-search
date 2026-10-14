//! The place index's tests: reading place queries, and finding the
//! places and sites they ask for.

use super::*;
use plumb_core::place::place_rank;

fn query(text: &str) -> Option<(String, Near)> {
    parse_place_query(text).map(|q| (q.what, q.near))
}

#[test]
fn namesakes_in_one_area_are_no_choice() {
    let landmark = |kind: &str, tags: usize, lat: f64, lon: f64| Place {
        rank: place_rank(kind, 0, true, false, tags),
        region: Some("AZ".into()),
        country: Some("US".into()),
        ..at("Grand Canyon", kind, lat, lon)
    };
    let mut fixtures = places();
    fixtures.extend([
        landmark("tourism=attraction", 11, 36.098, -112.096),
        landmark("railway=station", 5, 36.057, -112.137),
        city("Kansas City", 510_000, "MO", "US", 39.10, -94.58),
        city("Kansas City", 155_000, "KS", "US", 39.11, -94.63),
    ]);
    let (_dir, searcher) = index(fixtures);
    let canyon = searcher.resolve("grand canyon", None, None).unwrap();
    assert_eq!(canyon.status, LocationStatus::Resolved);
    assert_eq!(canyon.selected().unwrap().kind, "tourism=attraction");
    let kansas_city = searcher.resolve("kansas city", None, None).unwrap();
    assert_eq!(
        kansas_city.selected().unwrap().region.as_deref(),
        Some("MO")
    );
    // Namesakes far apart are still a choice.
    let portland = searcher.resolve("portland", None, None).unwrap();
    assert_eq!(portland.status, LocationStatus::AmbiguousLocation);
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
    // Words after "near me" say no more of where.
    for (asked, what) in [
        ("dentist near me that takes medicaid", "dentist"),
        ("restaurants near me with outdoor seating", "restaurants"),
        ("urgent care near me open saturday", "urgent care"),
        ("car wash in my area open sunday", "car wash"),
    ] {
        assert_eq!(query(asked), Some((what.into(), Near::Me)), "{asked}");
    }
    assert!(!matches!(
        query("hotels nearby disney world"),
        Some((_, Near::Me))
    ));
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
    // Or before the town.
    assert_eq!(
        query("pharmacy open now denver"),
        Some(("pharmacy".into(), Near::Named("denver".into())))
    );
    assert_eq!(
        query("gym open 24 hours in boston"),
        Some(("gym".into(), Near::Named("boston".into())))
    );
    // A kind of place at a time, with no town, is near the searcher.
    assert_eq!(query("coffee open now"), Some(("coffee".into(), Near::Me)));
    assert_eq!(
        query("pharmacy open late"),
        Some(("pharmacy".into(), Near::Me))
    );
    assert_eq!(query("pizza tonight"), Some(("pizza".into(), Near::Me)));
    // After any town the words may name ("gas" is none).
    for (nearest, what) in [
        ("nearest gas station", "gas station"),
        ("where is the closest hospital", "hospital"),
    ] {
        let near_me = PlaceQuery {
            what: what.into(),
            near: Near::Me,
            said_where: true,
        };
        assert_eq!(place_query_readings(nearest).last(), Some(&near_me));
    }
    // A town still says where.
    assert_eq!(
        query("nearest pharmacy in denver"),
        Some(("pharmacy".into(), Near::Named("denver".into())))
    );
    assert_eq!(
        query("restaurants open at night"),
        Some(("restaurants".into(), Near::Me))
    );
    for here in [
        "coffee near here",
        "coffee around here",
        "coffee in the area",
    ] {
        assert_eq!(query(here), Some(("coffee".into(), Near::Me)), "{here}");
    }
    // Not for words that are no kind of place.
    assert_eq!(query("chrome open now"), None);
    assert_eq!(query("nearest neighbor search"), None);
    assert_eq!(query("closest pair of points"), None);
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
        // Nowhere, after "in" or "at".
        "tallest building in the world",
        "best pizza in the world",
        "largest library in the whole world",
        "coffee in the morning",
        "museums in winter",
        "pizza at home",
        // Units.
        "tom cruise height in cm",
        "lebron james height in feet",
        "how tall is tom cruise in feet",
        "mount everest height in feet",
        "area of texas in square miles",
        "size of russia in km2",
        "eiffel tower height in meters",
    ] {
        assert_eq!(query(plain), None, "{plain}");
    }
}

#[test]
fn a_business_named_with_a_city_is_looked_for_there() {
    let mut fixtures = places();
    fixtures.extend([
        Place {
            rank: place_rank("place=village", 3_800, false, false, 3),
            region: Some("PA".into()),
            country: Some("US".into()),
            ..at("Denver", "place=village", 40.23, -76.14)
        },
        at("Starbucks", "amenity=cafe", 39.74, -104.99),
        at("Starbucks", "amenity=cafe", 43.66, -70.26),
        city("Reading", 95_000, "PA", "US", 40.34, -75.93),
        city("Aurora", 390_000, "CO", "US", 39.73, -104.83),
        at("IKEA", "shop=furniture", 39.74, -104.45),
        at(
            "Bible Baptist Church",
            "amenity=place_of_worship",
            40.35,
            -75.92,
        ),
        city("Charlotte", 875_000, "NC", "US", 35.227, -80.843),
        at(
            "Sheraton Charlotte Airport Hotel",
            "tourism=hotel",
            35.21,
            -80.94,
        ),
        at("Sheraton Charlotte Hotel", "tourism=hotel", 35.223, -80.846),
        at(
            "University of Denver",
            "amenity=university",
            39.676,
            -104.962,
        ),
    ]);
    let (_dir, searcher) = index(fixtures);
    // Denver, Colorado, though a village is called Denver too.
    let found = searcher
        .search("starbucks denver", None, Some("US"), 5)
        .unwrap()
        .unwrap();
    assert_eq!(found.center.unwrap().region.as_deref(), Some("CO"));
    assert_eq!(found.hits[0].place.name, "Starbucks");
    // Which Portland is not guessed.
    let portland = searcher.search("starbucks portland", None, Some("US"), 5);
    assert!(portland.unwrap().is_none());
    // Nothing in Reading is called Bible.
    let bible = searcher.search("bible reading", None, Some("US"), 5);
    assert!(bible.unwrap().is_none());
    // Nothing is called just Sheraton, but a branch is called that and
    // the town.
    let branch = searcher
        .search("sheraton charlotte", None, Some("US"), 5)
        .unwrap()
        .unwrap();
    assert_eq!(branch.what, "sheraton");
    let names: Vec<&str> = branch.hits.iter().map(|h| h.place.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Sheraton Charlotte Hotel",
            "Sheraton Charlotte Airport Hotel"
        ]
    );
    let university = searcher.search("university of denver", None, Some("US"), 5);
    assert!(university.unwrap().is_none());
    // A town by Denver is no business, and an IKEA 46 km out is
    // farther than places are listed.
    for query in ["aurora denver", "ikea denver"] {
        let found = searcher.search(query, None, Some("US"), 5).unwrap();
        assert!(found.is_none(), "{query}: {found:?}");
    }
    // Not pizza near a place called Hut Denver.
    let hut = searcher
        .search("pizza hut denver", None, Some("US"), 5)
        .unwrap()
        .unwrap();
    assert_eq!(hut.center.unwrap().name, "Denver");
    assert_eq!(hut.hits[0].place.name, "Pizza Hut");
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
        // Wikidata knows it, so it is somewhere to search around.
        Place {
            rank: place_rank("tourism=attraction", 0, true, false, 3),
            ..at("Tour Eiffel", "tourism=attraction", 48.8584, 2.2945)
        },
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
fn a_borough_of_millions_outranks_a_small_city() {
    let borough = |name: &str, kind: &str, people: u64, lat: f64, lon: f64| Place {
        rank: place_rank(kind, people, true, false, 3),
        region: Some("NY".into()),
        country: Some("US".into()),
        ..at(name, kind, lat, lon)
    };
    let mut fixtures = places();
    fixtures.extend([
        city("Manhattan", 55_000, "KS", "US", 39.18, -96.57),
        borough("Manhattan", "place=borough", 1_600_000, 40.78, -73.97),
        city("Brooklyn", 11_000, "OH", "US", 41.44, -81.74),
        borough("Brooklyn", "place=suburb", 2_600_000, 40.65, -73.95),
        // A suburb of a few thousand stays behind the city.
        city("Richmond", 230_000, "VA", "US", 37.54, -77.44),
        borough("Richmond", "place=suburb", 20_000, 40.58, -74.15),
    ]);
    let (_dir, searcher) = index(fixtures);
    let at = |name: &str| {
        let found = searcher.resolve(name, Some("US"), None).unwrap();
        found.selected().map(|place| place.region.clone().unwrap())
    };
    assert_eq!(at("manhattan").as_deref(), Some("NY"));
    assert_eq!(at("brooklyn").as_deref(), Some("NY"));
    assert_eq!(at("richmond").as_deref(), Some("VA"));
    assert_eq!(at("manhattan kansas").as_deref(), Some("KS"));
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
    let names: Vec<&str> = parking.hits.iter().map(|h| h.place.name.as_str()).collect();
    assert_eq!(names, ["Union Station Park-n-Ride"]);
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
    assert_eq!(
        without_near_me("dentist near me that takes medicaid").as_deref(),
        Some("dentist that takes medicaid")
    );
    assert_eq!(without_near_me("hotels nearby disney world"), None);
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
fn a_search_about_no_place_lists_none() {
    let mut fixtures = places();
    fixtures.extend([
        // An office, a village, fuel stations and suburbs whose names
        // are words, and a water park.
        Place {
            rank: place_rank("office=company", 0, false, true, 3),
            ..at("Solar System", "office=company", -21.2, -50.4)
        },
        at(
            "Hot Planet Thermas Park",
            "leisure=water_park",
            -21.22,
            -50.42,
        ),
        Place {
            rank: place_rank("place=village", 2_000, false, false, 3),
            country: Some("NL".into()),
            ..at("Python", "place=village", 52.0, 5.0)
        },
        at("APP", "amenity=fuel", 48.0, 2.0),
        at("APP", "amenity=fuel", 40.0, -3.0),
        // Two suburbs of one name, which no one guesses is meant.
        Place {
            rank: place_rank("place=suburb", 0, false, false, 3),
            ..at("Stock", "place=suburb", 51.6, 0.4)
        },
        Place {
            rank: place_rank("place=suburb", 0, false, false, 3),
            ..at("Stock", "place=suburb", 50.1, 8.6)
        },
        Place {
            rank: place_rank("place=town", 9_000, false, false, 3),
            region: Some("CT".into()),
            ..at("Portland", "place=town", 41.57, -72.64)
        },
        // A village called English, and a pub called Keel far from it.
        Place {
            rank: place_rank("place=village", 600, false, false, 3),
            region: Some("IN".into()),
            ..at("English", "place=village", 38.33, -86.46)
        },
        at("Keel", "amenity=pub", 53.97, -10.08),
        // Villages called Excel and Chrome.
        Place {
            rank: place_rank("place=village", 500, false, false, 3),
            region: Some("AL".into()),
            ..at("Excel", "place=village", 31.43, -87.34)
        },
        Place {
            rank: place_rank("place=hamlet", 0, false, false, 3),
            region: Some("NJ".into()),
            ..at("Chrome", "place=hamlet", 40.57, -74.22)
        },
    ]);
    let (_dir, searcher) = index(fixtures);
    for query in [
        "planets in the solar system",
        "sort a list in python",
        "workout app",
        "stock market",
        "keel in english",
        "blue pan in python",
        "blue pan in gotham",
        // "cells" is a word for phone stores, but a question of how
        // asks for none.
        "how to merge cells in excel",
        "how to clear cache in chrome",
    ] {
        let found = searcher.search(query, None, None, 5).unwrap();
        assert!(found.is_none(), "{query}: {found:?}");
    }
    // A kind of place is still looked for, and so is a place's name in
    // one of several towns or in a state with no town.
    for query in [
        "pizza in python",
        "pizza in gotham",
        "blue pan in portland",
        "blue pan in texas",
    ] {
        let found = searcher.search(query, None, None, 5).unwrap();
        assert!(found.is_some(), "{query}");
    }
    // Two Portland cities are still a choice, though a town shares
    // their name, and near me says it found nothing.
    let portland = searcher.search("portland pizza", None, None, 5);
    assert_eq!(
        portland.unwrap().unwrap().location.unwrap().status,
        LocationStatus::AmbiguousLocation
    );
    let near = searcher.search("starbuks near me", Some("Denver"), None, 5);
    assert!(near.unwrap().unwrap().hits.is_empty());
}

#[test]
fn a_guess_that_names_no_town_asks_for_none() {
    let (_dir, searcher) = index(places());
    // A state is no town, and a time alone says near me only of kinds
    // of place: neither asks the searcher for a town.
    for query in ["court view montana", "stock market today"] {
        let found = searcher.search(query, None, None, 5).unwrap();
        assert!(found.is_none(), "{query}: {found:?}");
    }
    let tonight = searcher.search("pizza tonight", None, None, 5).unwrap();
    assert!(tonight.unwrap().near_me);
    let open = searcher.search("market open now", None, None, 5).unwrap();
    assert!(open.unwrap().near_me);
}

#[test]
fn a_town_named_without_in_is_found_however_the_words_split() {
    let mut fixtures = places();
    fixtures.extend([
        Place {
            tags: vec![plumb_core::place::BREWERY_TAG.into()],
            ..at("Wynkoop Brewing", "amenity=pub", 39.7534, -105.0001)
        },
        at("Little Man", "amenity=ice_cream", 39.7593, -105.0111),
    ]);
    // Villages called Places, none bigger than the others.
    for (lat, lon) in [(49.1, 0.5), (45.0, 4.0), (8.5, -12.0)] {
        fixtures.push(Place {
            rank: place_rank("place=village", 400, false, false, 3),
            ..at("Places", "place=village", lat, lon)
        });
    }
    let (_dir, searcher) = index(fixtures);
    let first = |query: &str, home: Option<&str>| {
        let found = searcher.search(query, home, None, 5).unwrap();
        let found = found.unwrap_or_else(|| panic!("{query}: no places"));
        assert_eq!(found.center.unwrap().name, "Denver", "{query}");
        found.hits[0].place.name.clone()
    };
    // Two-word kinds, before or after the town: "shop denver" and
    // "denver coffee" are no towns.
    assert_eq!(first("coffee shop denver", None), "Huckleberry and Co");
    assert_eq!(first("denver coffee shop", None), "Huckleberry and Co");
    assert_eq!(first("ice cream denver", None), "Little Man");
    // Plurals in -ies, and "places" left out of what is looked for.
    assert_eq!(first("denver breweries", None), "Wynkoop Brewing");
    assert_eq!(first("denver pizza places", None), "Blue Pan");
    // With no other reading, the choice of towns is still offered.
    let portland = searcher.search("portland pizza", None, None, 5).unwrap();
    let location = portland.unwrap().location.unwrap();
    assert_eq!(location.status, LocationStatus::AmbiguousLocation);
    // A two-word kind at a time is near the searcher.
    assert_eq!(
        first("coffee shop open now", Some("Denver")),
        "Huckleberry and Co"
    );
    let readings: Vec<(String, Near)> = place_query_readings("coffee shop boulder")
        .into_iter()
        .map(|q| (q.what, q.near))
        .collect();
    assert_eq!(
        readings[0],
        ("coffee".into(), Near::Named("shop boulder".into()))
    );
    assert!(readings.contains(&("coffee shop".into(), Near::Named("boulder".into()))));
    // Still nothing for names that split into no town.
    for name in ["toy story", "pizza hut", "bank of america"] {
        assert!(
            searcher.search(name, None, None, 5).unwrap().is_none(),
            "{name}"
        );
    }
}

#[test]
fn words_no_place_is_found_by_leave_the_kind() {
    let mut fixtures = places();
    fixtures.extend([
        at("Olive Garden", "amenity=restaurant", 39.74, -104.99),
        at("Denver Botanic Gardens", "leisure=garden", 39.732, -104.96),
    ]);
    let (_dir, searcher) = index(fixtures);
    let found = |query: &str| {
        let found = searcher.search(query, None, None, 5).unwrap().unwrap();
        let names: Vec<String> = found.hits.iter().map(|h| h.place.name.clone()).collect();
        (found.what, names)
    };
    assert_eq!(
        found("pizza delivery in denver"),
        ("pizza".into(), vec!["Blue Pan".into(), "Pizza Hut".into()])
    );
    assert_eq!(found("luxury hotels in paris").0, "hotels");
    assert_eq!(found("luxury hotels in paris").1[0], "Hotel Eiffel");
    // A place named as asked of another kind is what was meant.
    assert_eq!(
        found("olive garden in denver"),
        ("olive garden".into(), vec!["Olive Garden".into()])
    );
    // Places found as asked come first.
    assert_eq!(
        found("blue pizza in denver"),
        ("pizza".into(), vec!["Blue Pan".into(), "Pizza Hut".into()])
    );
    assert_eq!(found("pet friendly hotels in paris").0, "hotels");
    assert_eq!(found("coffee shops open now in denver").0, "coffee shops");
    // What is found as asked keeps every word, and kinds that are not
    // there are not swapped for others.
    assert_eq!(found("pizza in denver").0, "pizza");
    assert_eq!(
        found("chinese food in denver"),
        ("chinese food".into(), vec![])
    );
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
    assert_eq!(locate("paris, tex.", None).1, "TX");
    assert_eq!(locate("paris france", Some("US")).1, "Île-de-France");
    assert_eq!(locate("nyc", None).0, "New York");
    assert_eq!(locate("Boulder, CO", None), ("Boulder".into(), "CO".into()));
    assert!(searcher.locate("atlantis", None).unwrap().is_none());
}

#[test]
fn a_place_is_found_by_the_town_named_after_it() {
    let mut fixtures = places();
    fixtures.extend([
        at("Union Station", "railway=station", 39.7527, -105.0003),
        Place {
            region: Some("OR".into()),
            ..at("Union Station", "railway=station", 45.5290, -122.6766)
        },
        Place {
            town: Some("Aurora".into()),
            ..at("Downtown", "place=neighbourhood", 39.729, -104.83)
        },
        Place {
            town: Some("Paris".into()),
            ..at("City Centre", "place=neighbourhood", 48.857, 2.35)
        },
    ]);
    let (_dir, searcher) = index(fixtures);
    let center = |query: &str| {
        let found = searcher.search(query, None, None, 5).unwrap().unwrap();
        let center = found.center.unwrap_or_else(|| panic!("{query}: nowhere"));
        (center.name, center.lat, found.hits[0].place.name.clone())
    };
    assert_eq!(
        center("coffee near union station denver"),
        ("Union Station".into(), 39.7527, "Huckleberry and Co".into())
    );
    assert_eq!(
        center("hotels near the tour eiffel paris").2,
        "Hotel Eiffel"
    );
    // Downtown is the town's centre when no place in it is called
    // that, even with a suburb's Downtown in reach.
    assert_eq!(
        center("parking in downtown denver"),
        ("Denver".into(), 39.7392, "Union Station Park-n-Ride".into())
    );
    assert_eq!(center("hotels in city centre paris").0, "City Centre");
    // A region still qualifies as before, and a town that is nowhere
    // finds nothing.
    assert_eq!(center("pizza near union station or").2, "Pizza Port");
    let nowhere = searcher
        .search("pizza in downtown gotham", None, None, 5)
        .unwrap()
        .unwrap();
    assert_eq!(
        nowhere.location.unwrap().status,
        LocationStatus::UnknownLocation
    );
    // A long "where" is still read in a few lookups, not one for
    // every way its words split.
    let long = format!("pizza in {}", ["union station"; 30].join(" "));
    assert!(searcher.search(&long, None, None, 5).unwrap().is_some());
}

#[test]
fn landmarks_are_meant_before_villages_and_bars() {
    let park = Place {
        rank: plumb_core::place::landmark_rank(70_000),
        aliases: vec!["Yellowstone".into()],
        ..at(
            "Yellowstone National Park",
            "leisure=nature_reserve",
            44.6,
            -110.5,
        )
    };
    let tower = Place {
        rank: plumb_core::place::landmark_rank(90_000),
        aliases: vec!["Big Ben".into()],
        ..at("Elizabeth Tower", "tourism=attraction", 51.5007, -0.1246)
    };
    let (_dir, searcher) = index(vec![
        park,
        tower,
        Place {
            region: Some("AB".into()),
            ..at("Yellowstone", "place=village", 53.73, -114.38)
        },
        Place {
            rank: place_rank("amenity=bar", 0, false, true, 9),
            ..at("Big Ben", "amenity=bar", 48.17, 16.37)
        },
        at("Lake Hotel", "tourism=hotel", 44.55, -110.40),
        at("Hotel Westminster", "tourism=hotel", 51.499, -0.127),
        // An island, a village of its name, and an island country.
        Place {
            rank: plumb_core::place::landmark_rank(60_000),
            ..at("Maui", "place=island", 20.8, -156.33)
        },
        Place {
            country: Some("PH".into()),
            ..at("Maui", "place=village", 13.9, 124.15)
        },
        at("Wailea Beach Resort", "tourism=hotel", 20.68, -156.44),
        Place {
            rank: plumb_core::place::landmark_rank(80_000),
            country: Some("JM".into()),
            ..at("Jamaica", "place=island", 18.15, -77.3)
        },
        at("Jamaica Inn", "tourism=hotel", 18.16, -77.31),
        // Parks named without their "National Park".
        Place {
            rank: plumb_core::place::landmark_rank(50_000),
            ..at(
                "Acadia National Park",
                "boundary=national_park",
                44.35,
                -68.21,
            )
        },
        Place {
            region: Some("AB".into()),
            ..at("Acadia", "place=suburb", 50.97, -114.05)
        },
        at("Bar Harbor Inn", "tourism=hotel", 44.39, -68.20),
        Place {
            rank: plumb_core::place::landmark_rank(100_000),
            ..at(
                "Banff National Park",
                "boundary=national_park",
                51.5,
                -116.0,
            )
        },
        Place {
            rank: place_rank("place=town", 8_000, true, false, 3),
            region: Some("AB".into()),
            ..at("Banff", "place=town", 51.18, -115.57)
        },
        Place {
            rank: plumb_core::place::landmark_rank(80_000),
            ..at(
                "Mount Rainier National Park",
                "boundary=national_park",
                46.85,
                -121.75,
            )
        },
    ]);
    // The park is meant, and searched wide: its hotel is 9 km off.
    let park = searcher
        .search("hotels near yellowstone", None, None, 5)
        .unwrap()
        .unwrap();
    assert_eq!(park.center.unwrap().name, "Yellowstone National Park");
    assert_eq!(park.hits[0].place.name, "Lake Hotel");
    let tower = searcher
        .search("hotels near big ben", None, None, 5)
        .unwrap()
        .unwrap();
    assert_eq!(tower.center.unwrap().name, "Elizabeth Tower");
    assert_eq!(tower.hits[0].place.name, "Hotel Westminster");
    // The village is still found by its province.
    let village = searcher.locate("yellowstone ab", None).unwrap().unwrap();
    assert_eq!(village.kind, "place=village");
    // An island is searched around as widely as a park.
    let maui = searcher
        .search("hotels in maui", None, None, 5)
        .unwrap()
        .unwrap();
    assert_eq!(maui.center.unwrap().kind, "place=island");
    assert_eq!(maui.hits[0].place.name, "Wailea Beach Resort");
    let jamaica = searcher.resolve("jamaica", None, None).unwrap();
    assert_eq!(jamaica.status, LocationStatus::MissingLocation);
    // A national park by its name alone, unless a town has it.
    let acadia = searcher
        .search("hotels near acadia", None, None, 5)
        .unwrap()
        .unwrap();
    assert_eq!(acadia.center.unwrap().name, "Acadia National Park");
    assert_eq!(acadia.hits[0].place.name, "Bar Harbor Inn");
    let banff = searcher.locate("banff", None).unwrap().unwrap();
    assert_eq!(banff.kind, "place=town");
    let rainier = searcher.locate("mt rainier", None).unwrap().unwrap();
    assert_eq!(rainier.name, "Mount Rainier National Park");
}

#[test]
fn a_town_is_found_by_its_german_spelling_without_accents() {
    let mut fixtures = places();
    fixtures.extend([
        city("München", 1_500_000, "BY", "DE", 48.14, 11.58),
        Place {
            kind: "place=town".into(),
            rank: place_rank("place=town", 18_000, false, false, 3),
            ..city("Boerne", 18_000, "TX", "US", 29.79, -98.73)
        },
        city("Borne", 400_000, "OV", "NL", 52.30, 6.75),
        city("Münster", 315_000, "NW", "DE", 51.96, 7.63),
        Place {
            kind: "place=village".into(),
            rank: place_rank("place=village", 1_500, false, false, 3),
            ..city("Muenster", 1_500, "TX", "US", 33.65, -97.38)
        },
        at("Muenchen", "amenity=restaurant", 39.10, -84.51),
    ]);
    let (_dir, searcher) = index(fixtures);
    let found = |text: &str| searcher.locate(text, None).unwrap().map(|place| place.name);
    // Not the restaurant, which is no town.
    assert_eq!(found("muenchen").as_deref(), Some("München"));
    // Only where nothing that fits is called the name as typed.
    assert_eq!(found("boerne").as_deref(), Some("Boerne"));
    assert_eq!(found("muenster").as_deref(), Some("Muenster"));
    assert_eq!(found("muenster germany").as_deref(), Some("Münster"));
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
fn any_country_qualifies_a_town() {
    let mut fixtures = places();
    fixtures.extend([
        city(
            "Reykjavík",
            140_000,
            "Höfuðborgarsvæðið",
            "IS",
            64.146,
            -21.94,
        ),
        at("Hótel Borg", "tourism=hotel", 64.147, -21.939),
    ]);
    let (_dir, searcher) = index(fixtures);
    for query in ["hotels in reykjavik iceland", "reykjavik hotels iceland"] {
        let found = searcher.search(query, None, None, 5).unwrap().unwrap();
        assert_eq!(found.hits[0].place.name, "Hótel Borg", "{query}");
    }
    let country = searcher.resolve("iceland", None, None).unwrap();
    assert_eq!(country.status, LocationStatus::MissingLocation);
}

#[test]
fn a_state_at_the_end_qualifies_a_town() {
    let mut fixtures = places();
    fixtures.extend([
        city("Springfield", 170_000, "MO", "US", 37.21, -93.29),
        city("Springfield", 155_000, "MA", "US", 42.10, -72.59),
        city("Springfield", 114_000, "IL", "US", 39.80, -89.65),
        at("Springfield City Hall", "amenity=townhall", 42.101, -72.591),
        at("Springfield City Hall", "amenity=townhall", 39.801, -89.651),
        city("Columbus", 900_000, "OH", "US", 39.96, -83.00),
        city("Columbus", 200_000, "GA", "US", 32.46, -84.99),
        at(
            "Columbus Police Department",
            "amenity=police",
            32.461,
            -84.991,
        ),
        at(
            "Columbus Police Department",
            "amenity=police",
            39.961,
            -83.001,
        ),
    ]);
    let (_dir, searcher) = index(fixtures);
    // Not a town in Illinois, which the state alone would ask for.
    let hall = searcher
        .search("springfield city hall illinois", None, Some("US"), 5)
        .unwrap()
        .unwrap();
    assert_eq!(hall.center.unwrap().region.as_deref(), Some("IL"));
    assert_eq!(hall.hits[0].place.lat, 39.801);
    let police = searcher
        .search("columbus police department ohio", None, Some("US"), 5)
        .unwrap()
        .unwrap();
    assert_eq!(police.center.unwrap().region.as_deref(), Some("OH"));
    assert_eq!(police.hits[0].place.lat, 39.961);
    let pizza = searcher
        .search("denver pizza colorado", None, None, 5)
        .unwrap()
        .unwrap();
    assert_eq!(pizza.hits[0].place.name, "Blue Pan");
    // A state alone still names no town.
    let state = searcher
        .search("city hall illinois", None, Some("US"), 5)
        .unwrap();
    assert!(state.is_none_or(|found| found.center.is_none()));
}

#[test]
fn a_town_of_no_known_country_is_found_by_its_country() {
    let mut fixtures = places();
    // As the index has them: Perth with no country, as Australia's
    // addresses seldom give one.
    fixtures.extend([
        Place {
            country: None,
            ..city("Perth", 2_000_000, "WA", "", -31.95, 115.86)
        },
        at("Hotel Perth", "tourism=hotel", -31.951, 115.861),
        city("Perth", 47_000, "Scotland", "GB", 56.40, -3.43),
        at("Perth Hotel", "tourism=hotel", 56.401, -3.431),
    ]);
    let (_dir, searcher) = index(fixtures);
    let center = |query: &str| {
        let found = searcher.search(query, None, None, 5).unwrap().unwrap();
        let center = found.center.unwrap();
        (center.region, center.country)
    };
    let australia = center("hotels in perth australia");
    assert_eq!(australia, (Some("WA".into()), None));
    // A town known to be in the country named comes first.
    assert_eq!(center("hotels in perth scotland").1.as_deref(), Some("GB"));
    assert_eq!(center("hotels in perth uk").1.as_deref(), Some("GB"));
    assert_eq!(center("hotels in perth"), australia);
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
fn a_us_state_named_alone_is_no_town_elsewhere() {
    let mut fixtures = places();
    fixtures.extend([
        city("Florida", 58_000, "Valle del Cauca", "CO", 3.32, -76.23),
        city("Florida", 33_000, "FL", "UY", -34.10, -56.21),
        city("Montana", 40_000, "Montana", "BG", 43.41, 23.22),
        city("Washington", 690_000, "DC", "US", 38.90, -77.04),
        at("Hotel Florida", "tourism=hotel", 3.321, -76.231),
        city("Ontario", 175_000, "CA", "US", 34.06, -117.65),
        city("Newfoundland", 1_000, "NJ", "US", 41.04, -74.43),
        city("St. John's", 110_000, "NL", "CA", 47.56, -52.71),
        city("Ala", 9_000, "TN", "IT", 45.76, 11.0),
        // As a places file that read no provinces has it.
        Place {
            region: None,
            country: None,
            ..city("Québec", 550_000, "QC", "CA", 46.81, -71.21)
        },
    ]);
    let (_dir, searcher) = index(fixtures);
    for state in ["florida", "Montana", "ontario", "newfoundland"] {
        let found = searcher.resolve(state, Some("US"), None).unwrap();
        assert_eq!(found.status, LocationStatus::MissingLocation, "{state}");
        assert!(found.candidates.is_empty(), "{state}");
    }
    let hotels = searcher
        .search("hotels in florida", None, Some("US"), 5)
        .unwrap()
        .unwrap();
    assert!(hotels.center.is_none() && hotels.hits.is_empty());
    // The town is still found with its country, and towns named like
    // their state, or "washington", still are.
    let colombia = searcher.locate("florida colombia", None).unwrap().unwrap();
    assert_eq!(colombia.country.as_deref(), Some("CO"));
    assert_eq!(
        searcher.locate("new york", None).unwrap().unwrap().name,
        "New York"
    );
    let capital = searcher.locate("washington", None).unwrap().unwrap();
    assert_eq!(capital.region.as_deref(), Some("DC"));
    assert_eq!(
        searcher.locate("quebec", None).unwrap().unwrap().name,
        "Québec"
    );
    let california = searcher.locate("ontario ca", None).unwrap().unwrap();
    assert_eq!(california.region.as_deref(), Some("CA"));
    // A state's abbreviation alone is no state: Ala is in Italy.
    let ala = searcher.locate("ala", None).unwrap().unwrap();
    assert_eq!(ala.country.as_deref(), Some("IT"));
    // A province is often called by the first part of its name.
    let found = searcher.locate("st johns newfoundland", None).unwrap();
    assert_eq!(found.unwrap().name, "St. John's");
}

#[test]
fn a_town_is_found_by_its_short_words_either_way() {
    let town = |name: &str, population: u64, region: &str, country: &str, lat, lon| Place {
        kind: "place=town".into(),
        rank: place_rank("place=town", population, false, false, 3),
        ..city(name, population, region, country, lat, lon)
    };
    let mut fixtures = places();
    fixtures.extend([
        city("Saint Paul", 311_000, "MN", "US", 44.95, -93.09),
        town("St. Paul", 5_800, "AB", "CA", 53.99, -111.29),
        city("Fort Lauderdale", 182_000, "FL", "US", 26.12, -80.14),
        town("Mount Shasta", 3_200, "CA", "US", 41.31, -122.31),
        city("Saint Augustine", 15_000, "FL", "US", 29.89, -81.31),
        at("St. Augustine", "place=village", 0.29, 32.59),
        town("Sault Ste. Marie", 72_000, "ON", "CA", 46.52, -84.35),
        Place {
            aliases: vec!["St. Louis".into()],
            ..city("Saint Louis", 300_000, "MO", "US", 38.63, -90.20)
        },
        city("Saint-Louis", 260_000, "SL", "SN", 16.03, -16.49),
    ]);
    let (_dir, searcher) = index(fixtures);
    let found = |text: &str| {
        let place = searcher.locate(text, Some("US")).unwrap();
        let place = place.unwrap_or_else(|| panic!("{text}: nowhere"));
        (place.name, place.region.unwrap_or_default())
    };
    let said = |name: &str, region: &str| (name.to_string(), region.to_string());
    assert_eq!(found("st paul"), said("Saint Paul", "MN"));
    assert_eq!(found("st. paul ab"), said("St. Paul", "AB"));
    assert_eq!(found("saint paul alberta"), said("St. Paul", "AB"));
    assert_eq!(found("ft lauderdale"), said("Fort Lauderdale", "FL"));
    assert_eq!(found("mt shasta"), said("Mount Shasta", "CA"));
    assert_eq!(found("st augustine"), said("Saint Augustine", "FL"));
    assert_eq!(found("sault sainte marie"), said("Sault Ste. Marie", "ON"));
    // As typed first: Senegal's Saint-Louis is no bigger.
    assert_eq!(found("st louis"), said("Saint Louis", "MO"));
}

#[test]
fn a_town_whose_name_begins_with_the_is_found_without_it() {
    let mut fixtures = places();
    fixtures.extend([
        Place {
            aliases: vec!["The Hague".into()],
            ..city("Den Haag", 550_000, "ZH", "NL", 52.08, 4.31)
        },
        Place {
            kind: "place=town".into(),
            rank: place_rank("place=town", 1_000, false, false, 3),
            ..city("Hague", 1_000, "SK", "CA", 52.51, -106.42)
        },
        Place {
            kind: "place=borough".into(),
            ..city("The Bronx", 1_400_000, "NY", "US", 40.84, -73.87)
        },
        Place {
            town: Some("Varaždin".into()),
            ..at("Bronx", "place=quarter", 46.30, 16.34)
        },
        city("The Woodlands", 114_000, "TX", "US", 30.17, -95.50),
        at("Bronx Café", "amenity=cafe", 40.841, -73.871),
    ]);
    let (_dir, searcher) = index(fixtures);
    let centre = |query: &str| {
        let found = searcher
            .search(query, None, Some("US"), 5)
            .unwrap()
            .unwrap();
        found.center.map(|place| place.name).unwrap_or_default()
    };
    assert_eq!(centre("hotels in the hague"), "Den Haag");
    assert_eq!(centre("coffee in the bronx"), "The Bronx");
    assert_eq!(centre("cafe in bronx ny"), "The Bronx");
    assert_eq!(centre("restaurants in the woodlands tx"), "The Woodlands");
    let hague = searcher
        .locate("hague saskatchewan", None)
        .unwrap()
        .unwrap();
    assert_eq!(hague.name, "Hague");
}

#[test]
fn the_biggest_town_the_words_name_wins_however_they_split() {
    let mut fixtures = places();
    fixtures.extend([
        Place {
            region: Some("OAX".into()),
            country: Some("MX".into()),
            ..at("San Antonio Texas", "place=village", 18.17, -96.13)
        },
        city("San Antonio", 1_400_000, "TX", "US", 29.42, -98.49),
    ]);
    let (_dir, searcher) = index(fixtures);
    let found = searcher.locate("san antonio texas", None).unwrap().unwrap();
    assert_eq!(
        (found.name.as_str(), found.region.as_deref()),
        ("San Antonio", Some("TX"))
    );
    // With its own country, the village is still found.
    let found = searcher
        .locate("san antonio texas mexico", None)
        .unwrap()
        .unwrap();
    assert_eq!(found.name, "San Antonio Texas");
}

#[test]
fn a_big_city_typed_with_a_slip_is_offered() {
    let mut fixtures = places();
    fixtures.extend([
        city("Cincinnati", 309_000, "OH", "US", 39.10, -84.51),
        city("Manchester", 115_000, "NH", "US", 42.99, -71.46),
        city("Mancheston", 2_000_000, "TX", "US", 30.0, -95.0),
        city("Tucson", 540_000, "AZ", "US", 32.22, -110.97),
        city("Gorham", 17_000, "ME", "US", 43.68, -70.44),
        Place {
            kind: "place=town".into(),
            rank: place_rank("place=town", 46_000, false, false, 3),
            ..city("Gotha", 46_000, "TH", "DE", 50.95, 10.70)
        },
        at("Hotel Cincinnatian", "tourism=hotel", 39.101, -84.511),
    ]);
    let (_dir, searcher) = index(fixtures);
    // Offered, not searched.
    let offered = |query: &str| {
        let found = searcher
            .search(query, None, Some("US"), 5)
            .unwrap()
            .unwrap();
        assert!(found.center.is_none() && found.hits.is_empty(), "{query}");
        let location = found.location.unwrap();
        assert_eq!(location.status, LocationStatus::UnknownLocation);
        location
            .candidates
            .iter()
            .map(|place| place.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(offered("hotels in cincinatti"), ["Cincinnati"]);
    assert_eq!(offered("hotels in cincinatti ohio"), ["Cincinnati"]);
    assert_eq!(offered("hotels near tuscon az"), ["Tucson"]);
    // The fewest slips first, however big the city two slips off.
    assert_eq!(offered("pizza in manchestar"), ["Manchester"]);
    // Nor a small city, a town or a short name, nor in a guess.
    for town in ["gorhams", "gothas", "gotham", "cincinatti tx"] {
        assert!(offered(&format!("pizza in {town}")).is_empty(), "{town}");
    }
    assert_eq!(
        searcher.search("cincinatti hotels", None, None, 5).unwrap(),
        None
    );
}

#[test]
fn cells_wrap_around_the_world() {
    let cells = cells_around(0.0, 179.99, 12.0);
    assert!(cells.iter().any(|c| c.ends_with(":-1800")));
    assert!(cells.contains(&cell(0.0, 179.99)));
}

#[test]
fn websites_near_lists_the_sites_of_nearby_places_nearest_first() {
    let with_site = |name: &str, lat: f64, lon: f64, site: &str| Place {
        website: Some(site.into()),
        ..at(name, "amenity=cafe", lat, lon)
    };
    let (_dir, searcher) = index(vec![
        with_site("Far Cafe", 39.99, -105.0, "https://far.example/"),
        with_site("Near Cafe", 39.75, -105.0, "https://near.example/menu"),
        with_site("Next Cafe", 39.80, -105.0, "https://next.example/"),
        with_site("Boulder Cafe", 40.40, -105.0, "https://boulder.example/"),
        at("Plain Cafe", "amenity=cafe", 39.74, -105.0),
    ]);
    let found = searcher.websites_near(39.74, -105.0, 30.0, 10).unwrap();
    let names: Vec<&str> = found.iter().map(|hit| hit.place.name.as_str()).collect();
    assert_eq!(names, ["Near Cafe", "Next Cafe", "Far Cafe"]);
    let nearest = searcher.websites_near(39.74, -105.0, 30.0, 1).unwrap();
    assert_eq!(nearest.len(), 1);
}
