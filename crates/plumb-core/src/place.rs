//! Places: named shops, restaurants, parks, towns and the like from
//! OpenStreetMap, so "pizza in denver" or "coffee near me" lists places
//! next to the sites.
//!
//! A place is kept small, about 100 bytes: its name, what kind of place it
//! is (an OpenStreetMap tag such as `amenity=cafe`), a few more kinds
//! (`cuisine=pizza`), where it is, its street address, town, region and
//! country, its website and its OpenStreetMap id. Towns also keep their
//! other names, so "nyc" finds New York.
//!
//! Places travel as a tab-separated file, most notable first, so the top
//! `N` are its first `N` lines (see [`place_rank`]):
//!
//! ```text
//! rank  name  kind  tags  lat  lon  town  region  country  address  website  osm  aliases
//! ```
//!
//! Tags are separated by `;` and aliases by `|`.
//!
//! The data is © OpenStreetMap contributors, under the Open Database
//! License (ODbL): whatever shows places says so and links to
//! openstreetmap.org/copyright.

use std::io::Write;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The places file's first line.
pub const PLACES_HEADER: &str =
    "rank\tname\tkind\ttags\tlat\tlon\ttown\tregion\tcountry\taddress\twebsite\tosm\taliases\n";

/// Longest name kept, in characters.
pub const MAX_PLACE_NAME_CHARS: usize = 100;
/// Longest website kept, in characters.
pub const MAX_WEBSITE_CHARS: usize = 200;
/// Most other names kept for a town.
pub const MAX_PLACE_ALIASES: usize = 4;

/// Where OpenStreetMap's license and attribution are.
pub const OSM_COPYRIGHT_URL: &str = "https://www.openstreetmap.org/copyright";

/// One place.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Place {
    /// How notable it is ([`place_rank`]); the file is sorted by it.
    pub rank: u32,
    pub name: String,
    /// Its main OpenStreetMap tag, `key=value`: `amenity=cafe`.
    pub kind: String,
    /// More tags that say what it is: `cuisine=pizza`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub lat: f64,
    pub lon: f64,
    /// The town it is in (its address's, or the nearest one's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub town: Option<String>,
    /// State or region as addresses there write it: `CO`, `Bayern`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// ISO 3166-1 alpha-2 code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// House number and street: `1600 Pennsylvania Avenue NW`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    /// `n123` (a node), `w123` (a way) or `r123` (a relation).
    pub osm: String,
    /// Other names: `NYC` for New York.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

impl Place {
    /// Whether it is a town, village or part of one, which searches can be
    /// "in" or "near".
    pub fn is_town(&self) -> bool {
        town_size(&self.kind).is_some()
    }

    /// What people see: "Pizza restaurant", "Café", "Bicycle shop".
    pub fn label(&self) -> String {
        let cuisine = self
            .tags
            .iter()
            .find_map(|t| t.strip_prefix("cuisine="))
            .and_then(cuisine_label);
        match (cuisine, self.kind.as_str()) {
            (Some(cuisine), "amenity=restaurant") => format!("{cuisine} restaurant"),
            (Some(cuisine), "amenity=fast_food") => format!("{cuisine} (fast food)"),
            _ => kind_label(&self.kind),
        }
    }

    /// The words it is found by besides its name: its kind's and tags'
    /// labels and the other words people use for them.
    pub fn kind_words(&self) -> String {
        let mut words = vec![kind_label(&self.kind)];
        words.extend(kind_info(&self.kind).map(|k| k.2.to_string()));
        for tag in &self.tags {
            words.extend(kind_info(tag).map(|k| format!("{} {}", k.1, k.2)));
            if let Some(cuisine) = tag.strip_prefix("cuisine=") {
                words.push(cuisine.replace('_', " "));
            }
            if let Some(sport) = tag.strip_prefix("sport=") {
                words.push(sport.replace('_', " "));
            }
        }
        words.join(" ")
    }

    /// The place on openstreetmap.org.
    pub fn osm_url(&self) -> String {
        let (kind, id) = self.osm.split_at(self.osm.len().min(1));
        let kind = match kind {
            "w" => "way",
            "r" => "relation",
            _ => "node",
        };
        format!("https://www.openstreetmap.org/{kind}/{id}")
    }
}

/// How big a town of `kind` is: the distance in km within which places
/// count as "in" it. `None` for what is not a town.
pub fn town_size(kind: &str) -> Option<f64> {
    Some(match kind {
        "place=city" => 12.0,
        "place=town" => 6.0,
        "place=village" => 3.0,
        "place=borough" | "place=suburb" => 3.0,
        "place=quarter" | "place=neighbourhood" => 1.5,
        _ => return None,
    })
}

/// Tiers of [`place_rank`], a million apart.
const TIER: u32 = 1_000_000;

/// How notable a place is, for the order of the file: cities, then towns,
/// then places with a Wikidata item, suburbs, places with a website or a
/// brand, villages, neighbourhoods, and then everything else; within a
/// tier, bigger towns and better described places first.
pub fn place_rank(
    kind: &str,
    population: u64,
    wikidata: bool,
    website_or_brand: bool,
    tags: usize,
) -> u32 {
    // Within a tier, by the logarithm of the population: a city of 2
    // million is 0.83 of a tier up, one of 25,000 0.58.
    let people = ((population as f64).ln_1p() / 40_000_000f64.ln_1p() * f64::from(TIER - 1))
        .min(f64::from(TIER - 1)) as u32;
    let described = (tags as u32).saturating_mul(10_000).min(TIER - 1);
    match kind {
        "place=city" => 9 * TIER + people,
        "place=town" => 8 * TIER + people,
        "place=borough" | "place=suburb" => 5 * TIER + people,
        "place=village" => 3 * TIER + people,
        "place=quarter" | "place=neighbourhood" => 2 * TIER + people,
        _ if wikidata => 6 * TIER + described,
        _ if website_or_brand => 4 * TIER + described,
        _ => TIER + described,
    }
}

/// `text` with tabs, line breaks and the separators made spaces.
fn field(text: &str) -> String {
    crate::collapse_whitespace(&text.replace(['\t', '\n', '\r', '|', ';'], " "))
}

/// Writes `place` as one line of a places file.
pub fn write_place(out: &mut impl Write, place: &Place) -> std::io::Result<()> {
    let tags: Vec<String> = place.tags.iter().map(|t| field(t)).collect();
    let aliases: Vec<String> = place.aliases.iter().map(|a| field(a)).collect();
    let opt = |o: &Option<String>| field(o.as_deref().unwrap_or(""));
    writeln!(
        out,
        "{}\t{}\t{}\t{}\t{:.5}\t{:.5}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        place.rank,
        field(&place.name),
        field(&place.kind),
        tags.join(";"),
        place.lat,
        place.lon,
        opt(&place.town),
        opt(&place.region),
        opt(&place.country),
        opt(&place.address),
        // A website may hold `;` or `|` in its query.
        place
            .website
            .as_deref()
            .unwrap_or("")
            .replace(['\t', '\n', '\r', ' '], ""),
        field(&place.osm),
        aliases.join("|"),
    )
}

/// Parses one line of a places file (not the header).
pub fn parse_place(line: &str) -> Result<Place> {
    let line = line.trim_end_matches(['\n', '\r']);
    let cols: Vec<&str> = line.split('\t').collect();
    if cols.len() != 13 {
        bail!("expected 13 tab-separated fields, found {}", cols.len());
    }
    let rank = cols[0]
        .parse()
        .with_context(|| format!("bad rank {:?}", cols[0]))?;
    let name = cols[1].trim();
    if name.is_empty() {
        bail!("no name");
    }
    let lat: f64 = cols[4].parse().context("bad latitude")?;
    let lon: f64 = cols[5].parse().context("bad longitude")?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        bail!("no such place on Earth: {lat}, {lon}");
    }
    let some = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    let list = |s: &str, sep: char| {
        s.split(sep)
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect()
    };
    Ok(Place {
        rank,
        name: name.to_string(),
        kind: cols[2].trim().to_string(),
        tags: list(cols[3], ';'),
        lat,
        lon,
        town: some(cols[6]),
        region: some(cols[7]),
        country: some(cols[8]),
        address: some(cols[9]),
        website: some(cols[10]),
        osm: cols[11].trim().to_string(),
        aliases: list(cols[12], '|'),
    })
}

/// United States' states, as addresses there write them, and their names,
/// so "denver colorado" and "denver co" find the same Denver.
pub const US_STATES: &[(&str, &str)] = &[
    ("AL", "Alabama"),
    ("AK", "Alaska"),
    ("AZ", "Arizona"),
    ("AR", "Arkansas"),
    ("CA", "California"),
    ("CO", "Colorado"),
    ("CT", "Connecticut"),
    ("DE", "Delaware"),
    ("DC", "District of Columbia"),
    ("FL", "Florida"),
    ("GA", "Georgia"),
    ("HI", "Hawaii"),
    ("ID", "Idaho"),
    ("IL", "Illinois"),
    ("IN", "Indiana"),
    ("IA", "Iowa"),
    ("KS", "Kansas"),
    ("KY", "Kentucky"),
    ("LA", "Louisiana"),
    ("ME", "Maine"),
    ("MD", "Maryland"),
    ("MA", "Massachusetts"),
    ("MI", "Michigan"),
    ("MN", "Minnesota"),
    ("MS", "Mississippi"),
    ("MO", "Missouri"),
    ("MT", "Montana"),
    ("NE", "Nebraska"),
    ("NV", "Nevada"),
    ("NH", "New Hampshire"),
    ("NJ", "New Jersey"),
    ("NM", "New Mexico"),
    ("NY", "New York"),
    ("NC", "North Carolina"),
    ("ND", "North Dakota"),
    ("OH", "Ohio"),
    ("OK", "Oklahoma"),
    ("OR", "Oregon"),
    ("PA", "Pennsylvania"),
    ("RI", "Rhode Island"),
    ("SC", "South Carolina"),
    ("SD", "South Dakota"),
    ("TN", "Tennessee"),
    ("TX", "Texas"),
    ("UT", "Utah"),
    ("VT", "Vermont"),
    ("VA", "Virginia"),
    ("WA", "Washington"),
    ("WV", "West Virginia"),
    ("WI", "Wisconsin"),
    ("WY", "Wyoming"),
    ("PR", "Puerto Rico"),
];

/// A region as addresses write it, with a United States state's name made
/// its code: "Colorado" -> "CO".
pub fn normalize_region(region: &str) -> String {
    let region = crate::collapse_whitespace(region);
    US_STATES
        .iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(&region))
        .map_or(region, |(code, _)| (*code).to_string())
}

/// Great-circle distance in km.
pub fn distance_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6371.0 * a.sqrt().asin()
}

/// What a kind is called and the other words people search it by.
type KindInfo = (&'static str, &'static str, &'static str);

/// Kinds of places with names people know, and the other words they are
/// searched by. Kinds not listed are named after their value: `shop=kite`
/// is a "Kite shop".
const KINDS: &[KindInfo] = &[
    ("amenity=cafe", "Café", "cafe coffee shop espresso"),
    ("amenity=restaurant", "Restaurant", "food eat dinner lunch"),
    ("amenity=fast_food", "Fast food", "food restaurant takeout"),
    ("amenity=bar", "Bar", "drinks cocktails"),
    ("amenity=pub", "Pub", "bar drinks beer"),
    ("amenity=biergarten", "Beer garden", "bar beer"),
    ("amenity=ice_cream", "Ice cream", "gelato dessert"),
    ("amenity=food_court", "Food court", "food"),
    ("amenity=nightclub", "Nightclub", "club dancing"),
    ("amenity=fuel", "Gas station", "petrol fuel gas"),
    (
        "amenity=charging_station",
        "Charging station",
        "ev electric car charger",
    ),
    ("amenity=car_wash", "Car wash", ""),
    ("amenity=car_rental", "Car rental", "rent car hire"),
    ("amenity=bank", "Bank", ""),
    ("amenity=atm", "ATM", "cash machine"),
    ("amenity=bureau_de_change", "Currency exchange", "money"),
    ("amenity=pharmacy", "Pharmacy", "drugstore chemist"),
    ("amenity=hospital", "Hospital", "emergency er"),
    ("amenity=clinic", "Clinic", "doctor medical urgent care"),
    ("amenity=doctors", "Doctor", "doctors physician medical"),
    ("amenity=dentist", "Dentist", "dental"),
    ("amenity=veterinary", "Veterinarian", "vet animal"),
    ("amenity=school", "School", ""),
    ("amenity=kindergarten", "Kindergarten", "preschool daycare"),
    ("amenity=childcare", "Childcare", "daycare"),
    ("amenity=college", "College", ""),
    ("amenity=university", "University", "college campus"),
    ("amenity=library", "Library", "books"),
    ("amenity=post_office", "Post office", "mail postal"),
    ("amenity=police", "Police", "station"),
    ("amenity=fire_station", "Fire station", ""),
    ("amenity=townhall", "Town hall", "city hall"),
    ("amenity=courthouse", "Courthouse", "court"),
    ("amenity=place_of_worship", "Place of worship", "church"),
    ("amenity=theatre", "Theater", "theatre plays"),
    ("amenity=cinema", "Cinema", "movie theater movies"),
    ("amenity=arts_centre", "Arts center", "art centre"),
    ("amenity=community_centre", "Community center", "centre"),
    ("amenity=marketplace", "Market", "farmers market"),
    ("amenity=parking", "Parking", "car park garage"),
    ("amenity=bus_station", "Bus station", "bus"),
    ("amenity=ferry_terminal", "Ferry terminal", "ferry"),
    ("amenity=social_facility", "Social services", ""),
    ("amenity=coworking_space", "Coworking space", "office"),
    (
        "shop=supermarket",
        "Supermarket",
        "grocery groceries store food",
    ),
    ("shop=convenience", "Convenience store", "corner shop"),
    ("shop=bakery", "Bakery", "bread pastry"),
    ("shop=butcher", "Butcher", "meat"),
    (
        "shop=greengrocer",
        "Greengrocer",
        "produce vegetables fruit",
    ),
    ("shop=deli", "Deli", "delicatessen"),
    ("shop=coffee", "Coffee shop", "coffee beans roaster"),
    ("shop=alcohol", "Liquor store", "alcohol wine beer"),
    ("shop=wine", "Wine shop", "wine"),
    ("shop=clothes", "Clothing store", "clothes fashion shop"),
    ("shop=shoes", "Shoe store", "shoes"),
    ("shop=books", "Bookstore", "books bookshop"),
    ("shop=hardware", "Hardware store", "tools"),
    (
        "shop=doityourself",
        "Home improvement store",
        "hardware diy",
    ),
    ("shop=electronics", "Electronics store", "electronics"),
    ("shop=mobile_phone", "Phone store", "mobile cell phone"),
    ("shop=computer", "Computer store", "computers"),
    ("shop=furniture", "Furniture store", "furniture"),
    ("shop=department_store", "Department store", "store"),
    ("shop=mall", "Shopping mall", "mall shopping center"),
    (
        "shop=hairdresser",
        "Hair salon",
        "hairdresser haircut barber",
    ),
    ("shop=barber", "Barber", "haircut"),
    ("shop=beauty", "Beauty salon", "nails spa"),
    ("shop=car", "Car dealer", "dealership cars"),
    ("shop=car_repair", "Car repair", "mechanic auto garage"),
    ("shop=bicycle", "Bike shop", "bicycle bike"),
    ("shop=florist", "Florist", "flowers"),
    ("shop=gift", "Gift shop", "gifts"),
    ("shop=jewelry", "Jewelry store", "jewellery"),
    ("shop=optician", "Optician", "glasses eyewear"),
    ("shop=pet", "Pet store", "pets pet shop"),
    ("shop=sports", "Sporting goods", "sports"),
    ("shop=toys", "Toy store", "toys"),
    ("shop=laundry", "Laundromat", "laundry"),
    ("shop=dry_cleaning", "Dry cleaner", "cleaners"),
    ("shop=chemist", "Drugstore", "pharmacy"),
    ("shop=cannabis", "Cannabis store", "dispensary"),
    ("tourism=hotel", "Hotel", "lodging stay"),
    ("tourism=motel", "Motel", "hotel lodging"),
    ("tourism=hostel", "Hostel", "lodging"),
    (
        "tourism=guest_house",
        "Guest house",
        "bed breakfast lodging",
    ),
    ("tourism=apartment", "Holiday apartment", "lodging"),
    ("tourism=camp_site", "Campground", "camping campsite"),
    ("tourism=caravan_site", "RV park", "caravan camping"),
    ("tourism=museum", "Museum", ""),
    ("tourism=gallery", "Art gallery", "art"),
    ("tourism=attraction", "Attraction", "sights things to do"),
    ("tourism=viewpoint", "Viewpoint", "view"),
    ("tourism=zoo", "Zoo", "animals"),
    ("tourism=aquarium", "Aquarium", ""),
    ("tourism=theme_park", "Theme park", "amusement park rides"),
    ("leisure=park", "Park", ""),
    ("leisure=garden", "Garden", "park"),
    ("leisure=nature_reserve", "Nature reserve", "park"),
    ("leisure=playground", "Playground", "kids park"),
    ("leisure=dog_park", "Dog park", "dogs"),
    ("leisure=fitness_centre", "Gym", "fitness gym workout"),
    (
        "leisure=sports_centre",
        "Sports center",
        "gym sports centre",
    ),
    ("leisure=stadium", "Stadium", "arena"),
    ("leisure=golf_course", "Golf course", "golf"),
    ("leisure=swimming_pool", "Swimming pool", "pool swim"),
    ("leisure=water_park", "Water park", "pool"),
    ("leisure=ice_rink", "Ice rink", "skating"),
    ("leisure=bowling_alley", "Bowling alley", "bowling"),
    ("leisure=marina", "Marina", "boats"),
    ("leisure=beach_resort", "Beach resort", "beach"),
    ("historic=castle", "Castle", ""),
    ("historic=monument", "Monument", ""),
    ("historic=ruins", "Ruins", ""),
    ("historic=fort", "Fort", ""),
    ("historic=palace", "Palace", ""),
    ("aeroway=aerodrome", "Airport", "airfield"),
    ("railway=station", "Train station", "railway station train"),
    ("craft=brewery", "Brewery", "beer"),
    ("craft=winery", "Winery", "wine"),
    ("craft=distillery", "Distillery", "spirits"),
    ("craft=plumber", "Plumber", "plumbing"),
    ("craft=electrician", "Electrician", "electrical"),
    ("craft=carpenter", "Carpenter", "handyman woodworking"),
    ("craft=handyman", "Handyman", ""),
    ("craft=roofer", "Roofer", "roofing"),
    ("craft=painter", "Painter", "painting"),
    ("craft=hvac", "HVAC", "heating furnace"),
    ("craft=locksmith", "Locksmith", ""),
    ("shop=locksmith", "Locksmith", ""),
    ("craft=gardener", "Landscaper", "landscaping gardener lawn"),
    ("craft=tailor", "Tailor", "alterations"),
    ("craft=shoemaker", "Shoe repair", "cobbler"),
    ("office=government", "Government office", ""),
    ("office=company", "Company office", ""),
    ("healthcare=hospital", "Hospital", ""),
    ("place=city", "City", "town"),
    ("place=town", "Town", ""),
    ("place=village", "Village", "town"),
    ("place=borough", "Borough", ""),
    ("place=suburb", "Suburb", "neighborhood"),
    ("place=quarter", "Quarter", "neighborhood"),
    ("place=neighbourhood", "Neighborhood", "neighbourhood"),
    ("cuisine=pizza", "Pizza", "pizzeria"),
    ("cuisine=burger", "Burger", "burgers hamburger"),
    ("cuisine=sushi", "Sushi", "japanese"),
    ("cuisine=coffee_shop", "Coffee", "cafe coffee shop"),
    ("cuisine=mexican", "Mexican", "tacos burritos"),
    ("cuisine=chinese", "Chinese", ""),
    ("cuisine=italian", "Italian", "pasta"),
    ("cuisine=indian", "Indian", "curry"),
    ("cuisine=thai", "Thai", ""),
    ("cuisine=japanese", "Japanese", ""),
    ("cuisine=vietnamese", "Vietnamese", "pho"),
    ("cuisine=korean", "Korean", ""),
    ("cuisine=ramen", "Ramen", "noodles"),
    ("cuisine=noodle", "Noodles", "noodle"),
    ("cuisine=barbecue", "Barbecue", "bbq"),
    ("cuisine=steak_house", "Steakhouse", "steak"),
    ("cuisine=seafood", "Seafood", "fish"),
    ("cuisine=chicken", "Chicken", "fried chicken wings"),
    ("cuisine=sandwich", "Sandwiches", "sandwich subs"),
    ("cuisine=donut", "Donuts", "doughnuts donut"),
    ("cuisine=bagel", "Bagels", "bagel"),
    ("cuisine=breakfast", "Breakfast", "brunch"),
    ("cuisine=vegan", "Vegan", "vegetarian"),
    ("cuisine=vegetarian", "Vegetarian", ""),
    ("cuisine=french", "French", ""),
    ("cuisine=greek", "Greek", "gyros"),
    ("cuisine=mediterranean", "Mediterranean", ""),
    ("cuisine=middle_eastern", "Middle Eastern", "falafel"),
    ("cuisine=kebab", "Kebab", "doner"),
    ("cuisine=american", "American", ""),
    ("cuisine=tex-mex", "Tex-Mex", "mexican"),
];

/// What `kind` is called and the other words for it, when it is listed.
fn kind_info(kind: &str) -> Option<&'static KindInfo> {
    KINDS.iter().find(|k| k.0 == kind)
}

/// What a kind is called: "Café" for `amenity=cafe`, "Kite shop" for
/// `shop=kite`.
pub fn kind_label(kind: &str) -> String {
    if let Some(info) = kind_info(kind) {
        return info.1.to_string();
    }
    let (key, value) = kind.split_once('=').unwrap_or(("", kind));
    let value = value.replace(['_', ';'], " ");
    let mut label = match key {
        "shop" => format!("{value} shop"),
        "office" => format!("{value} office"),
        "craft" => format!("{value} workshop"),
        _ => value,
    };
    if let Some(first) = label.get(..1) {
        label = first.to_uppercase() + &label[1..];
    }
    label
}

/// Whether `word` (normalized, lowercase) names a kind of place or is one
/// of the words people search kinds by: "pizza", "coffee", "hotels".
pub fn is_kind_word(word: &str) -> bool {
    // Little words of labels ("Place of worship"): "capital of washington"
    // asks for no place.
    if matches!(
        word,
        "of" | "to" | "and" | "the" | "for" | "a" | "an" | "in" | "on" | "at"
    ) {
        return false;
    }
    let one = word
        .strip_suffix("es")
        .filter(|w| w.ends_with(['s', 'x', 'h']))
        .or_else(|| word.strip_suffix('s'))
        .unwrap_or(word);
    KINDS.iter().any(|(_, label, words)| {
        crate::normalize_text(label)
            .split(' ')
            .chain(words.split(' '))
            .any(|w| w == word || w == one)
    })
}

/// What a cuisine is called, for "Pizza restaurant".
fn cuisine_label(cuisine: &str) -> Option<String> {
    let first = cuisine.split(';').next()?.trim();
    if first.is_empty() {
        return None;
    }
    Some(match kind_info(&format!("cuisine={first}")) {
        Some(info) => info.1.to_string(),
        None => kind_label(first),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cafe() -> Place {
        Place {
            rank: place_rank("amenity=cafe", 0, false, true, 8),
            name: "Huckleberry Roasters".into(),
            kind: "amenity=cafe".into(),
            tags: vec!["cuisine=coffee_shop".into()],
            lat: 39.75812,
            lon: -104.98712,
            town: Some("Denver".into()),
            region: Some("CO".into()),
            country: Some("US".into()),
            address: Some("4301 Pecos Street".into()),
            website: Some("https://huckleberryroasters.com/?a=1;b=2".into()),
            osm: "n123".into(),
            aliases: Vec::new(),
        }
    }

    #[test]
    fn places_round_trip_through_a_line() {
        let place = cafe();
        let mut line = Vec::new();
        write_place(&mut line, &place).unwrap();
        let line = String::from_utf8(line).unwrap();
        assert_eq!(parse_place(&line).unwrap(), place);
        assert_eq!(PLACES_HEADER.split('\t').count(), line.split('\t').count());
        let town = Place {
            name: "New York".into(),
            kind: "place=city".into(),
            aliases: vec!["NYC".into(), "New York City".into()],
            osm: "r175905".into(),
            ..Place::default()
        };
        let mut line = Vec::new();
        write_place(&mut line, &town).unwrap();
        let back = parse_place(std::str::from_utf8(&line).unwrap()).unwrap();
        assert_eq!(back.aliases, town.aliases);
        assert!(back.is_town());
        assert_eq!(
            back.osm_url(),
            "https://www.openstreetmap.org/relation/175905"
        );
    }

    #[test]
    fn bad_lines_are_refused() {
        assert!(parse_place("1\tx").is_err());
        let mut line = Vec::new();
        write_place(
            &mut line,
            &Place {
                name: "Nowhere".into(),
                lat: 95.0,
                ..cafe()
            },
        )
        .unwrap();
        assert!(parse_place(std::str::from_utf8(&line).unwrap()).is_err());
    }

    #[test]
    fn kinds_have_labels_and_words() {
        assert_eq!(kind_label("amenity=cafe"), "Café");
        assert_eq!(kind_label("shop=kite"), "Kite shop");
        let pizza = Place {
            kind: "amenity=restaurant".into(),
            tags: vec!["cuisine=pizza;italian".into()],
            ..cafe()
        };
        assert_eq!(pizza.label(), "Pizza restaurant");
        assert!(cafe().kind_words().contains("coffee"));
        for word in [
            "pizza", "coffee", "hotels", "cafe", "gas", "museums", "sushi",
        ] {
            assert!(is_kind_word(word), "{word}");
        }
        for word in ["apple", "music", "times", "jobs"] {
            assert!(!is_kind_word(word), "{word}");
        }
    }

    #[test]
    fn cities_come_before_shops() {
        let city = place_rank("place=city", 700_000, false, false, 3);
        let town = place_rank("place=town", 9_000, false, false, 3);
        let known = place_rank("tourism=museum", 0, true, true, 20);
        let shop = place_rank("shop=bakery", 0, false, true, 12);
        let plain = place_rank("shop=bakery", 0, false, false, 30);
        assert!(city > town && town > known && known > shop && shop > plain);
    }

    #[test]
    fn distances_are_in_km() {
        // Denver to Boulder, about 39 km.
        let d = distance_km(39.7392, -104.9903, 40.0150, -105.2705);
        assert!((d - 39.0).abs() < 1.5, "{d}");
    }
}
