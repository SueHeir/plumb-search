//! Searches that name a town or county with namesakes elsewhere: the
//! sites of the one meant before those of the others.

use std::collections::HashMap;
use std::sync::LazyLock;

use plumb_core::place::{Place, CANADIAN_PROVINCES, US_STATES};
use plumb_core::{country_of_any_name, normalize_country, normalize_text};

use crate::Hit;

/// Words for a county: Louisiana's are parishes, Alaska's boroughs.
const COUNTY_WORDS: [&str; 3] = ["county", "parish", "borough"];
/// How Wikidata starts describing a town: "city in Coos County, New
/// Hampshire", "county seat of Hampden County".
const TOWN_DESCRIPTIONS: &[&str] = &[
    "city",
    "town",
    "village",
    "township",
    "borough",
    "county seat",
    "civil town",
    "capital",
    "census designated place",
    "unincorporated community",
    "municipality",
];
/// What comes before a town's name in its own site's title: "City of
/// Paris".
const TOWN_TITLES: &[&str] = &["city of", "town of", "village of", "township of"];
/// Words a town's name does not start or end with.
const NOT_TOWNS: &[&str] = &[
    "city", "town", "village", "county", "township", "of", "the", "and", "a", "in",
];
/// Australia's states and territories, which addresses write as the US's
/// and Canada's: "Perth, WA".
const AUSTRALIAN_STATES: &[(&str, &str)] = &[
    ("ACT", "Australian Capital Territory"),
    ("NSW", "New South Wales"),
    ("NT", "Northern Territory"),
    ("QLD", "Queensland"),
    ("SA", "South Australia"),
    ("TAS", "Tasmania"),
    ("VIC", "Victoria"),
    ("WA", "Western Australia"),
];
/// The countries whose towns' addresses give a state or province, and
/// theirs.
const STATES: [(&str, &[(&str, &str)]); 3] = [
    ("US", US_STATES),
    ("CA", CANADIAN_PROVINCES),
    ("AU", AUSTRALIAN_STATES),
];
/// Words after which a place's name is not where the town before them is:
/// "rome to new york", "plymouth new england".
const OTHER_PLACE_AFTER: &[&str] = &["to", "from", "vs", "versus", "and", "new"];

/// A run of a query's words: where it starts and how many words it has.
type Run = (usize, usize);

/// Puts the sites that say they are in another state than the one `query`
/// ends in after the rest, when it names a county: "franklin county sheriff
/// ohio" wants Ohio's Franklin County, not one of the 23 others that share
/// its name. Sites that say nothing of a state keep their places among the
/// rest. The sites trade scores so that scores still go down the list.
pub fn asked_state_first(query: &str, hits: &mut Vec<Hit>) {
    let Some(asked) = asked_state(query) else {
        return;
    };
    let away: Vec<bool> = hits
        .iter()
        .map(|hit| {
            let states = site_states(hit);
            !hit.named && !states.is_empty() && !states.contains(&asked)
        })
        .collect();
    put_last(hits, &away);
}

/// Puts the sites of other towns of the name of the town `query` names
/// after the rest: "berlin wall memorial" wants no site of Berlin, New
/// Hampshire, nor "springfield city hall illinois" Springfield,
/// Massachusetts's. The town meant is the one the query places
/// ([`asked_where`]), or else the one `town` finds by its name (Berlin,
/// Germany, the biggest) when a site that names it is of it, as one is
/// not for "plymouth thanksgiving turkey". A town's own site, which says
/// "Berlin, NH", "City of Berlin" or "town in Vermont", goes last when it
/// is somewhere else ([`site_places`]), unless the query names that place
/// too ("university of idaho moscow").
pub fn namesakes_last(query: &str, hits: &mut Vec<Hit>, town: &dyn Fn(&str) -> Option<Place>) {
    let query = normalize_text(query);
    let words: Vec<&str> = query.split(' ').filter(|w| !w.is_empty()).collect();
    // Each run of up to three words, the longest first, then the first,
    // with the places it is the name of; not "england" in "new england".
    let runs: Vec<(usize, usize, Vec<Where>)> = (1..=3.min(words.len()))
        .rev()
        .flat_map(|length| (0..=words.len() - length).map(move |at| (at, length)))
        .filter(|&(at, _)| at == 0 || words[at - 1] != "new")
        .map(|(at, length)| {
            (
                at,
                length,
                regions_called(&words[at..at + length].join(" ")),
            )
        })
        .collect();
    let asked: Vec<&Where> = runs.iter().flat_map(|(_, _, places)| places).collect();
    // The runs that may name a town: not a state or country ("Seattle,
    // Washington, United States"), nor starting or ending in "city".
    let names: Vec<Run> = runs
        .iter()
        .filter(|(at, length, places)| {
            let name = &words[*at..at + length];
            places.is_empty()
                && !NOT_TOWNS.contains(&name[0])
                && !NOT_TOWNS.contains(&name[length - 1])
                && name.concat().len() >= 3
        })
        .map(|&(at, length, _)| (at, length))
        .collect();
    let sites: Vec<SiteText> = hits.iter().map(SiteText::of).collect();
    // The town each site is the site of, by its name's run, and where it
    // says it is.
    let towns: Vec<Option<(Run, Vec<Where>)>> = hits
        .iter()
        .zip(&sites)
        .map(|(hit, site)| {
            names.iter().find_map(|&(at, length)| {
                let said = site.is_of(&words[at..at + length])?;
                Some(((at, length), site_places(hit, &said)))
            })
        })
        .collect();
    // Whether a site that names the run's town is in `meant`.
    let named_in = |run: Run, meant: &Where| {
        let name = &words[run.0..run.0 + run.1];
        hits.iter()
            .zip(&sites)
            .zip(&towns)
            .any(|((hit, site), of)| {
                let places = match of {
                    Some((known, places)) if *known == run => places.clone(),
                    _ => site_places(hit, &[]),
                };
                site.says(name) && places.iter().any(|place| place.is_in(meant))
            })
    };
    let mut meant: HashMap<Run, Vec<Where>> = HashMap::new();
    let away: Vec<bool> = hits
        .iter()
        .zip(&towns)
        .map(|(hit, of)| {
            let Some((run, places)) = of.as_ref().filter(|_| !hit.named) else {
                return false;
            };
            // "london ontario", "university of idaho moscow".
            if places
                .iter()
                .any(|place| asked.iter().any(|asked| place.may_be(asked)))
            {
                return false;
            }
            let found = meant.entry(*run).or_insert_with(|| {
                let name = words[run.0..run.0 + run.1].join(" ");
                let found = asked_where(&words, &name, run.0 + run.1, town);
                if !found.is_empty() {
                    return found;
                }
                town(&name)
                    .and_then(Where::of)
                    .filter(|meant| named_in(*run, meant))
                    .into_iter()
                    .collect()
            });
            !found.is_empty()
                && !places.is_empty()
                && !places
                    .iter()
                    .any(|place| found.iter().any(|meant| place.may_be(meant)))
        })
        .collect();
    put_last(hits, &away);
}

/// Moves the hits `away` marks after the rest, keeping their order. The
/// sites trade scores so that scores still go down the list.
fn put_last(hits: &mut Vec<Hit>, away: &[bool]) {
    if !away.contains(&true) {
        return;
    }
    let mut scores: Vec<f32> = hits.iter().map(|hit| hit.score).collect();
    scores.sort_by(|a, b| b.total_cmp(a));
    let mut tagged: Vec<(bool, Hit)> = away.iter().copied().zip(std::mem::take(hits)).collect();
    // A stable sort, so each part keeps its order.
    tagged.sort_by_key(|(away, _)| *away);
    *hits = tagged
        .into_iter()
        .zip(scores)
        .map(|((_, mut hit), score)| {
            hit.score = score;
            hit
        })
        .collect();
}

/// Where a town is: its country, and in the US, Canada or Australia its
/// state or province.
#[derive(Debug, Clone, PartialEq)]
struct Where {
    country: String,
    state: Option<&'static str>,
}

/// The states and provinces of [`STATES`] by name, as [`normalize_text`]
/// writes it.
static REGIONS: LazyLock<HashMap<String, Where>> = LazyLock::new(|| {
    STATES
        .into_iter()
        .flat_map(|(country, regions)| {
            regions.iter().map(move |(code, name)| {
                let place = Where {
                    country: country.into(),
                    state: Some(*code),
                };
                (normalize_text(name), place)
            })
        })
        .collect()
});

impl Where {
    fn of(place: Place) -> Option<Where> {
        let country = place.country?;
        let region = place.region.unwrap_or_default();
        let state = STATES
            .iter()
            .filter(|(code, _)| *code == country)
            .flat_map(|(_, regions)| regions.iter())
            .find(|(code, name)| {
                region.eq_ignore_ascii_case(code) || region.eq_ignore_ascii_case(name)
            })
            .map(|(code, _)| *code);
        Some(Where { country, state })
    }

    /// Whether a town said to be in `self` may be the one in `meant`: the
    /// same country, and the same state when both say one. The resolver
    /// takes "washington" for the capital, DC.
    fn may_be(&self, meant: &Where) -> bool {
        self.country == meant.country
            && match (self.state, meant.state) {
                (Some(state), Some(meant)) => state == meant || (meant == "DC" && state == "WA"),
                _ => true,
            }
    }

    /// Whether a site said to be in `self` is in `meant`: as
    /// [`Where::may_be`], and in its state when it has one.
    fn is_in(&self, meant: &Where) -> bool {
        self.may_be(meant) && (meant.state.is_none() || self.state.is_some())
    }
}

/// Where `hit` says it is: in the places it `said` after its town's name,
/// else in the US states it names (Georgia's being a country's name too),
/// else in its country; and only in its country, when that is known:
/// "Perth, WA" in Australia is not in Washington.
fn site_places(hit: &Hit, said: &[Where]) -> Vec<Where> {
    let mut places: Vec<Where> = if said.is_empty() {
        site_states(hit)
            .into_iter()
            .flat_map(|code| US_STATES.iter().filter(move |(state, _)| *state == code))
            .flat_map(|(_, name)| regions_called(name))
            .collect()
    } else {
        said.to_vec()
    };
    if let Some(country) = &hit.country {
        places.retain(|place| place.country == *country);
        if places.is_empty() {
            places.push(Where {
                country: country.clone(),
                state: None,
            });
        }
    }
    places
}

/// Where the query places the town `name`, whose words end before word
/// `end`: by a state's or province's name right after it ("portland
/// maine", "london ontario", "athens georgia", which is a country too); or
/// where `town` finds a town of the name and what follows it, when that
/// is the place it says: a postal code as the last word ("springfield il",
/// as "in" and "or" are words), a country right after the name ("paris
/// france"), or any place at the query's end ("springfield city hall
/// illinois"; not "chicago turkey trot", "rome to new york" or "plymouth
/// new england").
fn asked_where(
    words: &[&str],
    name: &str,
    end: usize,
    town: &dyn Fn(&str) -> Option<Place>,
) -> Vec<Where> {
    let rest = &words[end..];
    for length in (1..=3.min(rest.len())).rev() {
        let found = regions_called(&rest[..length].join(" "));
        if found.iter().any(|place| place.state.is_some()) {
            return found;
        }
    }
    let confirmed = |place: &str, readings: Vec<Where>| {
        if readings.is_empty() {
            return None;
        }
        let found = town(&format!("{name} {place}")).and_then(Where::of)?;
        readings
            .iter()
            .any(|reading| found.may_be(reading))
            .then_some(found)
    };
    if let [last] = rest {
        let codes = regions_named(&[last.to_ascii_uppercase()], true)
            .into_iter()
            .filter(|place| place.state.is_some())
            .collect();
        if let Some(found) = confirmed(last, codes) {
            return vec![found];
        }
    }
    (1..=3.min(rest.len()))
        .rev()
        .find_map(|length| {
            let part = |at: usize| {
                let part = rest[at..at + length].join(" ");
                confirmed(&part, regions_called(&part))
            };
            let at = rest.len() - length;
            part(0).or_else(|| {
                (at > 0 && !OTHER_PLACE_AFTER.contains(&rest[at - 1]))
                    .then(|| part(at))
                    .flatten()
            })
        })
        .into_iter()
        .collect()
}

/// A site's words: its title's parts ("Home - City of Paris" is "Home"
/// and "City of Paris"), normalized; the words of its title and then its
/// description as written, each comma a word of its own, and normalized;
/// and whether its description starts like a town's ("town in Vermont").
struct SiteText {
    parts: Vec<String>,
    words: Vec<String>,
    plain: Vec<String>,
    description: usize,
    town: bool,
}

impl SiteText {
    fn of(hit: &Hit) -> SiteText {
        let split = |text: &str| -> Vec<String> {
            text.replace(',', " , ")
                .split(|c: char| !c.is_alphanumeric() && c != ',')
                .filter(|word| !word.is_empty())
                .map(str::to_string)
                .collect()
        };
        let title = hit.title.as_deref().unwrap_or("");
        let description = hit.description.as_deref().unwrap_or("");
        let mut words = split(title);
        let at = words.len();
        words.extend(split(description));
        let described = normalize_text(description);
        SiteText {
            // Not "Winston-Salem".
            parts: title
                .replace(" - ", "|")
                .split(['|', '\u{2013}', '\u{2014}', ':', '\u{bb}'])
                .map(normalize_text)
                .collect(),
            plain: words.iter().map(|word| normalize_text(word)).collect(),
            words,
            description: at,
            town: TOWN_DESCRIPTIONS.iter().any(|kind| {
                described
                    .strip_prefix(kind)
                    .is_some_and(|rest| rest.starts_with(' '))
            }),
        }
    }

    /// The places this site says it is in, when it is the site of a town
    /// called `name`: it says the name and, after a comma, a state,
    /// province or country ("Paris, Kentucky", "Berlin, NH"); a part of
    /// its title is the name after "City of" or the like ("City of
    /// London", "City of Springfield Ohio Official Website"), or the name
    /// and a state ("Austin Indiana", not "Eastern Illinois University");
    /// or a part of its title is the name and its description a town's
    /// ("Berlin": "town in Vermont"). A band called Berlin says none of
    /// these.
    fn is_of(&self, name: &[&str]) -> Option<Vec<Where>> {
        let said: Vec<Where> = self
            .after_comma(name)
            .flat_map(|rest| regions_named(rest, true))
            .collect();
        if !said.is_empty() {
            return Some(said);
        }
        let plain = name.join(" ");
        self.parts.iter().find_map(|part| {
            let after_town = TOWN_TITLES
                .iter()
                .find_map(|before| part.strip_prefix(before)?.strip_prefix(' '));
            let rest = after_town.unwrap_or(part).strip_prefix(plain.as_str())?;
            if rest.is_empty() {
                return (after_town.is_some() || self.town).then(Vec::new);
            }
            let rest = rest.strip_prefix(' ')?;
            let said = match after_town {
                Some(_) => {
                    let rest: Vec<String> = rest.split(' ').map(str::to_string).collect();
                    regions_named(&rest, false)
                }
                None => regions_called(rest),
            };
            (!said.is_empty()).then_some(said)
        })
    }

    /// Whether the site's words from word `at` on are `name`.
    fn names_at(&self, at: usize, name: &[&str]) -> bool {
        self.plain
            .get(at..at + name.len())
            .is_some_and(|words| words.iter().zip(name).all(|(word, name)| word == name))
    }

    /// Whether the site's title or description says `name`.
    fn says(&self, name: &[&str]) -> bool {
        (0..self.plain.len()).any(|at| self.names_at(at, name))
    }

    /// The words after each "NAME," in its title, or at the start of its
    /// description: "Berlin, NH", not "university in Moscow, Idaho".
    fn after_comma<'a>(&'a self, name: &'a [&str]) -> impl Iterator<Item = &'a [String]> + 'a {
        (0..=self.description).filter_map(move |at| {
            let end = at + name.len();
            (self.names_at(at, name) && self.words.get(end).is_some_and(|word| word == ","))
                .then(|| &self.words[end + 1..])
        })
    }
}

/// The places `words` start with the name of, or after a comma with the
/// code of as addresses write it: a state's or province's ("NH", not
/// "in") or a country's ("Berlin, DE").
fn regions_named(words: &[String], comma: bool) -> Vec<Where> {
    let mut found: Vec<Where> = (1..=3.min(words.len()))
        .flat_map(|length| regions_called(&words[..length].join(" ")))
        .collect();
    if let Some(code) = words.first().filter(|_| comma) {
        for (country, regions) in STATES {
            if let Some((state, _)) = regions.iter().find(|(state, _)| state == code) {
                found.push(Where {
                    country: country.into(),
                    state: Some(state),
                });
            }
        }
        if code.len() == 2 && code.chars().all(|c| c.is_ascii_uppercase()) {
            found.extend(normalize_country(code).map(|country| Where {
                country,
                state: None,
            }));
        }
    }
    found
}

/// The places called `name`: a state or province of [`STATES`] or a
/// country, or more than one ("georgia").
fn regions_called(name: &str) -> Vec<Where> {
    let name = normalize_text(name);
    // Not "us" or "uk": "contact us".
    let country = (name.len() > 2)
        .then(|| country_of_any_name(&name))
        .flatten()
        .map(|country| Where {
            country: country.into(),
            state: None,
        });
    REGIONS
        .get(&name)
        .cloned()
        .into_iter()
        .chain(country)
        .collect()
}

/// The code of the state `query` ends in, after a word for a county:
/// "OH" for "franklin county sheriff ohio". Not for a state named first or
/// alone, where it is a county's name ("ohio county") or a person's.
fn asked_state(query: &str) -> Option<&'static str> {
    let query = normalize_text(query);
    let words: Vec<&str> = query.split(' ').collect();
    let county = words.iter().position(|word| COUNTY_WORDS.contains(word))?;
    STATE_NAMES.iter().find_map(|(code, name)| {
        let length = name.split(' ').count();
        let after = words.len().checked_sub(length)?;
        (after > county && words[after..].join(" ") == *name).then_some(*code)
    })
}

/// The states' codes and names as [`normalize_text`] writes them, the
/// longest names first, so "west virginia" is not taken for Virginia.
static STATE_NAMES: LazyLock<Vec<(&str, String)>> = LazyLock::new(|| {
    let mut names: Vec<(&str, String)> = US_STATES
        .iter()
        .map(|(code, name)| (*code, normalize_text(name)))
        .collect();
    names.sort_by_key(|(_, name)| std::cmp::Reverse(name.len()));
    names
});

/// The codes of the states a site says it is in: by name in its title or
/// description ("county in Idaho, United States"), by code after a comma
/// or "County" ("Jefferson County, IN"), or by its domain
/// (franklincounty.ky.gov, lincolncountynv.org). A state's name before a
/// word for a county is a county's: "Washington County" is in Oregon too.
fn site_states(hit: &Hit) -> Vec<&'static str> {
    let raw = format!(
        "{} {}",
        hit.title.as_deref().unwrap_or(""),
        hit.description.as_deref().unwrap_or("")
    );
    let mut states = Vec::new();
    let mut found = |code: &'static str| {
        if !states.contains(&code) {
            states.push(code);
        }
    };
    let names = &*STATE_NAMES;
    let mut text = format!(" {} ", normalize_text(&raw));
    for (code, name) in names {
        let needle = format!(" {name} ");
        while let Some(at) = text.find(&needle) {
            let next = text[at + needle.len()..].split(' ').next().unwrap_or("");
            if !COUNTY_WORDS.contains(&next) {
                found(code);
            }
            text.replace_range(at..at + needle.len() - 1, " _");
        }
    }
    // A code only where addresses write one, as "IN" and "OR" are words.
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    for pair in tokens.windows(2) {
        let word = pair[1].trim_end_matches(|c: char| !c.is_alphanumeric());
        let after = pair[0].ends_with(',') || pair[0].eq_ignore_ascii_case("county");
        if let Some(&(code, _)) = US_STATES.iter().find(|(code, _)| *code == word) {
            if after {
                found(code);
            }
        }
    }
    let spells = |text: &str| {
        names
            .iter()
            .find(|(code, name)| text == code.to_ascii_lowercase() || text == name.replace(' ', ""))
            .map(|(code, _)| *code)
    };
    let labels: Vec<&str> = hit.domain.split('.').collect();
    // A state's own part of .gov or .us: franklincounty.ky.gov,
    // madisonco.virginia.gov, co.lincoln.or.us.
    if let [.., state, "gov" | "us"] = labels[..] {
        if let Some(code) = spells(state) {
            found(code);
        }
    }
    // Or what follows "county" in the name: lincolncountynv.org.
    let label = labels.first().unwrap_or(&"").replace('-', "");
    if let Some(code) = label
        .rfind("county")
        .and_then(|at| spells(&label[at + "county".len()..]))
    {
        found(code);
    }
    states
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(domain: &str, title: &str, description: &str, score: f32) -> Hit {
        Hit {
            domain: domain.into(),
            url: format!("https://{domain}/"),
            title: Some(title.into()),
            description: (!description.is_empty()).then(|| description.into()),
            score,
            text_score: 0.5,
            link_score: 0.3,
            placing_text_score: None,
            country: None,
            named: false,
            official: true,
            key_pages: Vec::new(),
            demand: None,
            missing_words: true,
            query_evidence: None,
        }
    }

    fn domains(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.domain.as_str()).collect()
    }

    #[test]
    fn a_county_search_lists_its_states_county_first() {
        // As plumbsearch.org listed them for "lincoln county sheriff oregon".
        let mut hits = vec![
            site(
                "lincolncountynv.org",
                "Lincoln County, Nevada Government -",
                "county in Nevada, United States",
                0.9,
            ),
            site("lincolncountywv.org", "Lincoln County WV | Home", "", 0.8),
            site(
                "lincolncity.org",
                "Lincoln City",
                "city in Lincoln County, Oregon, United States",
                0.7,
            ),
            site("lincolncountyid.us", "Lincoln County", "", 0.6),
            site("washingtoncounty.in.gov", "Washington County", "", 0.5),
            site(
                "mcso.us",
                "Home | Multnomah County Sheriff's Office",
                "",
                0.4,
            ),
            site("washingtoncountyor.gov", "Washington County, OR", "", 0.3),
        ];
        asked_state_first("lincoln county sheriff oregon", &mut hits);
        assert_eq!(
            domains(&hits),
            [
                "lincolncity.org",
                "mcso.us",
                "washingtoncountyor.gov",
                "lincolncountynv.org",
                "lincolncountywv.org",
                "lincolncountyid.us",
                "washingtoncounty.in.gov",
            ]
        );
        let scores: Vec<f32> = hits.iter().map(|hit| hit.score).collect();
        assert_eq!(scores, [0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3]);
    }

    #[test]
    fn only_a_state_after_a_county_is_asked_for() {
        assert_eq!(asked_state("franklin county sheriff ohio"), Some("OH"));
        assert_eq!(
            asked_state("Jefferson County clerk, West Virginia"),
            Some("WV")
        );
        assert_eq!(asked_state("orleans parish louisiana"), Some("LA"));
        // A county named for a state, a state first, and none at all.
        for query in [
            "ohio county",
            "washington county jail",
            "virginia beach county",
            "tom holland oregon",
        ] {
            assert_eq!(asked_state(query), None, "{query}");
        }
    }

    #[test]
    fn a_site_says_its_state_by_name_code_or_domain() {
        let states = |domain: &str, title: &str, description: &str| {
            site_states(&site(domain, title, description, 1.0))
        };
        assert_eq!(
            states("franklincounty.ky.gov", "Franklin County | KENTUCKY", ""),
            ["KY"]
        );
        assert_eq!(
            states("example.org", "Lincoln County WV | Home", ""),
            ["WV"]
        );
        assert_eq!(
            states("example.org", "Franklin County AL Sheriff's Office", ""),
            ["AL"]
        );
        assert_eq!(
            states("example.org", "Clay County Info", "Piggott, AR"),
            ["AR"]
        );
        assert_eq!(
            states("madisonco.virginia.gov", "Madison County", ""),
            ["VA"]
        );
        assert_eq!(states("co.lincoln.or.us", "Lincoln County", ""), ["OR"]);
        assert_eq!(states("franklincountyohio.gov", "Home", ""), ["OH"]);
        assert_eq!(
            states("washco-md.net", "Washington County", ""),
            Vec::<&str>::new()
        );
        assert_eq!(
            states("example.org", "Welcome to West Virginia", ""),
            ["WV"]
        );
        // A county named for a state, and words that are codes.
        assert!(states("example.org", "Washington County", "").is_empty());
        assert!(states("example.org", "Life IN the city OR the county", "").is_empty());
    }

    fn town(country: &str, region: &str) -> Option<Place> {
        Some(Place {
            kind: "place=city".into(),
            country: Some(country.into()),
            region: Some(region.into()).filter(|region: &String| !region.is_empty()),
            ..Place::default()
        })
    }

    fn abroad(domain: &str, title: &str, description: &str, country: &str) -> Hit {
        Hit {
            country: Some(country.into()),
            ..site(domain, title, description, 0.5)
        }
    }

    #[test]
    fn a_towns_namesakes_elsewhere_go_last() {
        // As hpc listed them for "berlin wall memorial".
        let mut hits = vec![
            site(
                "berlinpage.com",
                "Berlin - Home",
                "The official site of synth electro-pop band Berlin.",
                0.9,
            ),
            site(
                "berlinnh.gov",
                "Berlin, NH | Official Website",
                "city in Coos County, New Hampshire, United States",
                0.8,
            ),
            site(
                "berlinvt.gov",
                "Berlin",
                "town in Washington County, Vermont, United States",
                0.7,
            ),
            abroad(
                "stiftung-berliner-mauer.de",
                "Berlin Wall Memorial",
                "memorial in Berlin, Germany",
                "DE",
            ),
        ];
        let berlin = |name: &str| town("DE", "").filter(|_| name == "berlin");
        namesakes_last("berlin wall memorial", &mut hits, &berlin);
        assert_eq!(
            domains(&hits),
            [
                "berlinpage.com",
                "stiftung-berliner-mauer.de",
                "berlinnh.gov",
                "berlinvt.gov"
            ]
        );
        let scores: Vec<f32> = hits.iter().map(|hit| hit.score).collect();
        assert_eq!(scores, [0.9, 0.8, 0.7, 0.5]);

        // The state the query ends in is the one meant, whatever town the
        // place index would find.
        let mut hits = vec![
            site(
                "springfield.il.us",
                "City of Springfield, IL City of Springfield - Home",
                "capital city of the U.S. state of Illinois",
                0.9,
            ),
            site(
                "springfield-ma.gov",
                "City of Springfield, MA: City of Springfield, MA",
                "county seat of Hampden County, Massachusetts, United States",
                0.8,
            ),
            site(
                "springfieldmo.gov",
                "Springfield, MO - Official Website",
                "",
                0.7,
            ),
            site(
                "sangamonil.gov",
                "Sangamon County Springfield Illinois | Government",
                "Sangamon County, Illinois (Springfield, IL) website",
                0.6,
            ),
            site(
                "springfield-or.gov",
                "Home - City of Springfield Oregon",
                "",
                0.5,
            ),
        ];
        let springfield = |name: &str| match name {
            "springfield illinois" => town("US", "IL"),
            _ => town("US", "MO"),
        };
        namesakes_last("springfield city hall illinois", &mut hits, &springfield);
        assert_eq!(
            domains(&hits),
            [
                "springfield.il.us",
                "sangamonil.gov",
                "springfield-ma.gov",
                "springfieldmo.gov",
                "springfield-or.gov"
            ]
        );
    }

    #[test]
    fn a_town_the_query_places_keeps_its_site() {
        let london = |_: &str| town("GB", "");
        let hits = || {
            vec![
                abroad(
                    "london.ca",
                    "Home | City of London",
                    "The official website for the City of London, Ontario.",
                    "CA",
                ),
                site("cityoflondonky.org", "London, Kentucky", "", 0.4),
                abroad("london.gov.uk", "Home page - London City Hall", "", "GB"),
            ]
        };
        let mut tube = hits();
        namesakes_last("london tube map", &mut tube, &london);
        assert_eq!(
            domains(&tube),
            ["london.gov.uk", "london.ca", "cityoflondonky.org"]
        );
        let mut ontario = hits();
        namesakes_last("london ontario", &mut ontario, &london);
        assert_eq!(
            domains(&ontario),
            ["london.ca", "london.gov.uk", "cityoflondonky.org"]
        );
        // Kentucky is not what "london new jersey" asks for either.
        let mut jersey = hits();
        namesakes_last("london new jersey", &mut jersey, &london);
        assert_eq!(
            domains(&jersey),
            ["london.gov.uk", "london.ca", "cityoflondonky.org"]
        );

        // A university in a town is no town's site, and one that says its
        // town's state keeps its place when the query names it.
        let moscow = |_: &str| town("RU", "");
        let mut idaho = vec![
            site(
                "uidaho.edu",
                "University of Idaho | Moscow, Idaho",
                "public university in Moscow, Idaho, United States",
                0.9,
            ),
            site(
                "dartmouth.edu",
                "Home | Dartmouth",
                "private research university in Hanover, New Hampshire",
                0.8,
            ),
            site("ci.moscow.id.us", "Moscow, ID - Official Website", "", 0.7),
            site(
                "moscow.example",
                "City of Moscow",
                "city in Latah County, Idaho, United States",
                0.6,
            ),
            abroad("mos.ru", "Moscow City Government", "", "RU"),
        ];
        namesakes_last("university of idaho moscow", &mut idaho, &moscow);
        assert_eq!(
            domains(&idaho),
            [
                "uidaho.edu",
                "dartmouth.edu",
                "ci.moscow.id.us",
                "moscow.example",
                "mos.ru"
            ]
        );
        namesakes_last("moscow metro", &mut idaho, &moscow);
        assert_eq!(
            domains(&idaho),
            [
                "dartmouth.edu",
                "mos.ru",
                "uidaho.edu",
                "ci.moscow.id.us",
                "moscow.example"
            ]
        );

        // Georgia is a state and a country.
        let tbilisi = |_: &str| town("GE", "");
        let mut georgia = vec![
            site("example.com", "Tbilisi, Georgia - Travel Guide", "", 0.9),
            site("tbilisi.ga", "Tbilisi, GA", "", 0.8),
            site("tbilisi.example", "Tbilisi", "capital of Georgia", 0.7),
        ];
        namesakes_last("tbilisi hotels", &mut georgia, &tbilisi);
        assert_eq!(
            domains(&georgia),
            ["example.com", "tbilisi.example", "tbilisi.ga"]
        );

        // "rome to new york" starts in Italy, though there is a Rome, New
        // York.
        let rome = |name: &str| match name {
            "rome new york" => town("US", "NY"),
            _ => town("IT", ""),
        };
        let mut italy = vec![
            site(
                "romenewyork.com",
                "Rome, NY",
                "city in Oneida County, New York",
                0.9,
            ),
            abroad("comune.roma.it", "City of Rome", "", "IT"),
        ];
        namesakes_last("rome to new york", &mut italy, &rome);
        assert_eq!(domains(&italy), ["romenewyork.com", "comune.roma.it"]);

        // Western Australia's code is Washington's too.
        let perth = |_: &str| town("AU", "WA");
        let mut wa = vec![
            site("perthnd.example", "Perth, ND", "", 0.9),
            abroad("perth.wa.gov.au", "City of Perth", "", "AU"),
            abroad("perthdentist.com.au", "Dentist in Perth, WA", "", "AU"),
        ];
        namesakes_last("perth wa", &mut wa, &perth);
        assert_eq!(
            domains(&wa),
            ["perth.wa.gov.au", "perthdentist.com.au", "perthnd.example"]
        );
    }

    #[test]
    fn a_guess_at_the_town_needs_a_site_of_it() {
        // Plymouth, England, is the biggest, but "plymouth thanksgiving
        // turkey" lists sites of Massachusetts's alone.
        let plymouth = |_: &str| town("GB", "");
        let hits = || {
            vec![
                site(
                    "plimoth.org",
                    "Plimoth Patuxet Museums",
                    "museum in Plymouth, Massachusetts",
                    0.9,
                ),
                site(
                    "plymouth-ma.gov",
                    "Plymouth, MA - Official Website",
                    "",
                    0.8,
                ),
                abroad("bbcgoodfood.com", "Turkey recipes", "", "GB"),
            ]
        };
        let mut turkey = hits();
        namesakes_last("plymouth thanksgiving turkey", &mut turkey, &plymouth);
        assert_eq!(
            domains(&turkey),
            ["plimoth.org", "plymouth-ma.gov", "bbcgoodfood.com"]
        );
        let mut council = hits();
        council.push(abroad("plymouth.gov.uk", "Plymouth City Council", "", "GB"));
        namesakes_last("plymouth city council", &mut council, &plymouth);
        assert_eq!(
            domains(&council),
            [
                "plimoth.org",
                "bbcgoodfood.com",
                "plymouth.gov.uk",
                "plymouth-ma.gov"
            ]
        );
    }

    #[test]
    fn a_site_is_where_it_says_in_its_own_country() {
        let place = |country: &str, state: Option<&'static str>| Where {
            country: country.into(),
            state,
        };
        let said = [place("US", Some("WA")), place("AU", Some("WA"))];
        let dentist = abroad("perthdentist.com.au", "Dentist in Perth, WA", "", "AU");
        assert_eq!(site_places(&dentist, &said), [place("AU", Some("WA"))]);
        let city = abroad("perth.wa.gov.au", "City of Perth", "", "AU");
        assert_eq!(site_places(&city, &[]), [place("AU", None)]);
        let tbilisi = site("tbilisi.example", "Tbilisi", "capital of Georgia", 0.5);
        assert_eq!(
            site_places(&tbilisi, &[]),
            [place("US", Some("GA")), place("GE", None)]
        );
        let band = site("berlinpage.com", "Berlin - Home", "synth-pop band", 0.5);
        assert_eq!(site_places(&band, &[]), []);
    }

    #[test]
    fn only_a_site_that_says_it_is_a_towns_is_one() {
        let of = |title: &str, description: &str, name: &str| {
            let name: Vec<&str> = name.split(' ').collect();
            SiteText::of(&site("example.org", title, description, 1.0))
                .is_of(&name)
                .is_some()
        };
        assert!(of("Paris, Illinois – A Place to Call Home", "", "paris"));
        assert!(of("Berlin, NH | Official Website", "", "berlin"));
        assert!(of("Home | City of London", "", "london"));
        assert!(of("Home - City of Springfield Oregon", "", "springfield"));
        assert!(of("Austin Indiana", "city in Scott County", "austin"));
        assert!(of("Berlin", "town in Washington County, Vermont", "berlin"));
        assert!(!of(
            "Berlin - Home",
            "The official site of synth electro-pop band Berlin.",
            "berlin"
        ));
        assert!(!of("Eastern Illinois University", "", "eastern"));
        assert!(!of(
            "George Washington's Mount Vernon",
            "his historic Virginia estate",
            "george"
        ));
        assert!(!of(
            "Winston-Salem, North Carolina",
            "city in and the county seat of Forsyth County",
            "winston"
        ));
        assert!(!of("Bush Center", "George W. Bush, in Dallas", "bush"));
        assert!(!of(
            "Home | University of Idaho",
            "public university in Moscow, Idaho",
            "moscow"
        ));
        assert!(of("Home", "Berlin, NH official site", "berlin"));
        assert!(of("Berlin, DE", "", "berlin"));
    }

    #[test]
    fn a_query_places_a_town_by_a_state_or_country_after_it() {
        let towns = |name: &str| match name {
            "springfield illinois" | "springfield il" => town("US", "IL"),
            "paris france" => town("FR", ""),
            "perth wa" => town("AU", "WA"),
            "rome new york" | "toronto new york" => town("US", "NY"),
            "plymouth england" => town("GB", ""),
            _ => None,
        };
        let asked = |query: &str, name: &str| {
            let words: Vec<&str> = query.split(' ').collect();
            let length = name.split(' ').count();
            let end = (0..words.len())
                .find(|&at| words[at..].starts_with(&name.split(' ').collect::<Vec<_>>()))
                .map_or(0, |at| at + length);
            asked_where(&words, name, end, &towns)
        };
        let place = |country: &str, state: Option<&'static str>| Where {
            country: country.into(),
            state,
        };
        assert_eq!(
            asked("portland maine lobster", "portland"),
            [place("US", Some("ME"))]
        );
        assert_eq!(
            asked("kansas city west virginia", "kansas city"),
            [place("US", Some("WV"))]
        );
        assert_eq!(asked("london ontario", "london"), [place("CA", Some("ON"))]);
        assert_eq!(
            asked("newcastle new south wales", "newcastle"),
            [place("AU", Some("NSW"))]
        );
        assert_eq!(
            asked("athens georgia", "athens"),
            [place("US", Some("GA")), place("GE", None)]
        );
        // Postal codes, countries and places at the end only where there
        // is such a town.
        assert_eq!(
            asked("springfield il", "springfield"),
            [place("US", Some("IL"))]
        );
        assert_eq!(asked("perth wa", "perth"), [place("AU", Some("WA"))]);
        assert_eq!(
            asked("springfield city hall illinois", "springfield"),
            [place("US", Some("IL"))]
        );
        assert_eq!(asked("paris france metro", "paris"), [place("FR", None)]);
        // "in" and "us" are words, a state before the name is not it, and
        // a place after "to" or "new" is another.
        for (query, name) in [
            ("paris in spring", "paris"),
            ("berlin contact us", "berlin"),
            ("springfield mo", "springfield"),
            ("ohio columbus", "columbus"),
            ("paris metro map", "paris"),
            ("chicago turkey trot", "chicago"),
            ("toronto to new york", "toronto"),
            ("rome to new york", "rome"),
            ("plymouth new england", "plymouth"),
        ] {
            assert_eq!(asked(query, name), [], "{query}");
        }
    }
}
