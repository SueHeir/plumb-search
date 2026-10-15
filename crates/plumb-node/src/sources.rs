//! The sites that serve a search for a tool or a quick fact, rather than
//! for a site: "weather", "time in london", "20 usd to eur", "calculator",
//! "nvidia stock", "define prioritize", "food near me".
//!
//! Such a search names no site, so ranking by the words finds sites whose
//! names hold them: time.com for "time", eur.nl for "20 usd to eur",
//! zaxbys.com for "food near me". [`route`] knows which sites do the job
//! (free to use, without signing in), and [`lead_with`] lists the ones the
//! index has first, under the instant answer when there is one. A site
//! the query names in full ("safeway near me", "nvidia stock") keeps its
//! place at the top.

use plumb_answer::Kind;
use plumb_core::normalize_text;
use plumb_index::pages::PlacedPage;
use plumb_index::Hit;

/// A site to list first, or one page of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The site, as the index keys it.
    pub domain: &'static str,
    /// The page to link to instead of the homepage, and its title.
    pub page: Option<(String, String)>,
}

impl Source {
    fn site(domain: &'static str) -> Source {
        Source { domain, page: None }
    }
}

/// The sites a search goes to first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Best first.
    pub sources: Vec<Source>,
}

/// Whether the searcher is in the United States, or did not say.
fn in_us(country: Option<&str>) -> bool {
    country.is_none_or(|c| c.eq_ignore_ascii_case("us"))
}

/// The national weather service of `country`, free and without ads.
fn weather_service(country: &str) -> Option<&'static str> {
    Some(match country.to_ascii_uppercase().as_str() {
        "US" => "weather.gov",
        "GB" => "metoffice.gov.uk",
        "CA" => "weather.gc.ca",
        "AU" => "bom.gov.au",
        "NZ" => "metservice.com",
        "IE" => "met.ie",
        "DE" => "dwd.de",
        "FR" => "meteofrance.com",
        "ES" => "aemet.es",
        "NL" => "knmi.nl",
        "JP" => "jma.go.jp",
        "NO" => "yr.no",
        "SE" => "smhi.se",
        "DK" => "dmi.dk",
        _ => return None,
    })
}

/// The sites that serve `query` best, when it asks for a tool or a quick
/// fact. `answer` is the kind of the instant answer shown for it, if any;
/// `country` the searcher's.
pub fn route(query: &str, answer: Option<Kind>, country: Option<&str>) -> Option<Route> {
    let text = normalize_text(query);
    let sites = |domains: &[&'static str]| {
        Some(Route {
            sources: domains.iter().map(|d| Source::site(d)).collect(),
        })
    };
    if let Some(asked) = plumb_answer::weather::asked(query) {
        // The country's own weather service, for "weather" alone or a
        // country named ("uk weather"); a town's could be anywhere.
        let country = match &asked.place {
            None => country,
            Some(place) => plumb_core::country_of_name(place),
        };
        let mut domains: Vec<&'static str> =
            country.and_then(weather_service).into_iter().collect();
        domains.extend(["weather.com", "accuweather.com"]);
        return sites(&domains);
    }
    if asks_the_time(&text) {
        return if in_us(country) {
            sites(&["time.gov", "time.is", "timeanddate.com"])
        } else {
            sites(&["time.is", "timeanddate.com"])
        };
    }
    match answer {
        Some(Kind::Time) => return sites(&["time.is", "timeanddate.com"]),
        Some(Kind::Currency) => return sites(&["xe.com", "wise.com"]),
        _ => {}
    }
    if let Some((word, asked)) = word_asked(&text) {
        let encoded: String = url::form_urlencoded::byte_serialize(word.as_bytes()).collect();
        let encoded = encoded.replace('+', "%20");
        let wiki = word.replace(' ', "_");
        let wiki: String = url::form_urlencoded::byte_serialize(wiki.as_bytes()).collect();
        let page = |domain, url: String, title: String| Source {
            domain,
            page: Some((url, title)),
        };
        let wiktionary = page(
            "wiktionary.org",
            format!("https://en.wiktionary.org/wiki/{wiki}"),
            format!("{word}: Wiktionary, the free dictionary"),
        );
        let sources = match asked {
            WordAsk::Meaning => vec![
                page(
                    "merriam-webster.com",
                    format!("https://www.merriam-webster.com/dictionary/{encoded}"),
                    format!("{word}: Merriam-Webster dictionary"),
                ),
                page(
                    "dictionary.com",
                    format!("https://www.dictionary.com/browse/{encoded}"),
                    format!("{word}: Dictionary.com"),
                ),
                wiktionary,
            ],
            WordAsk::Others => vec![
                page(
                    "merriam-webster.com",
                    format!("https://www.merriam-webster.com/thesaurus/{encoded}"),
                    format!("{word}: Merriam-Webster thesaurus"),
                ),
                page(
                    "thesaurus.com",
                    format!("https://www.thesaurus.com/browse/{encoded}"),
                    format!("{word}: Thesaurus.com"),
                ),
                wiktionary,
            ],
        };
        return Some(Route { sources });
    }
    if let Some(route) = tools(&text) {
        return Some(route);
    }
    if let Some(route) = government(&text, country) {
        return Some(route);
    }
    near_me(&text)
}

/// Whether `text` (normalized) asks the time without saying where.
pub fn asks_the_time(text: &str) -> bool {
    matches!(
        text,
        "time"
            | "time now"
            | "the time"
            | "current time"
            | "local time"
            | "time right now"
            | "what time is it"
            | "what time is it now"
            | "what time is it right now"
            | "whats the time"
            | "what is the time"
            | "what s the time"
            | "what time is it here"
    )
}

/// What a search asks of a word ([`word_asked`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordAsk {
    /// Its meaning, forms or spelling, which a dictionary gives.
    Meaning,
    /// Words that mean the same or the opposite, which a thesaurus gives.
    Others,
}

/// Words before a word that ask what a dictionary gives of it.
const MEANING_LEADS: &[&str] = &[
    "define ",
    "definition of ",
    "meaning of ",
    "what is the meaning of ",
    "what is the definition of ",
    "plural of ",
    "the plural of ",
    "what is the plural of ",
    "past tense of ",
    "the past tense of ",
    "what is the past tense of ",
    "how to spell ",
    "how do you spell ",
    "how do i spell ",
    "spelling of ",
    "how to pronounce ",
    "how do you pronounce ",
    "how do i pronounce ",
    "pronunciation of ",
    "pronounce ",
];
/// Words after a word that ask what a dictionary gives of it. Not
/// " plural" or " past tense": "first person plural" and "simple past
/// tense" ask about grammar.
const MEANING_TAILS: &[&str] = &[" definition", " meaning", " define"];
/// Words before a word that ask what a thesaurus gives of it.
const OTHERS_LEADS: &[&str] = &[
    "synonym for ",
    "synonyms for ",
    "synonym of ",
    "synonyms of ",
    "another word for ",
    "what is another word for ",
    "other words for ",
    "antonym for ",
    "antonyms for ",
    "antonym of ",
    "antonyms of ",
    "opposite of ",
    "the opposite of ",
    "what is the opposite of ",
];
/// Words after a word that ask what a thesaurus gives of it.
const OTHERS_TAILS: &[&str] = &[" synonym", " synonyms", " antonym", " antonyms"];

/// The word or two `text` (normalized) asks about, and what it asks:
/// "define prioritize", "prioritize definition", "what does ubiquitous
/// mean", "what does asap stand for", "plural of mouse", "how to spell
/// necessary", "how do you pronounce gif", "is irregardless a word" ask a
/// dictionary; "synonyms for good", "opposite of brave" a thesaurus.
fn word_asked(text: &str) -> Option<(String, WordAsk)> {
    let text = text
        .strip_prefix("whats ")
        .map_or(text.to_string(), |rest| format!("what is {rest}"));
    let lead = |leads: &[&str]| leads.iter().find_map(|lead| text.strip_prefix(lead));
    let tail = |tails: &[&str]| tails.iter().find_map(|tail| text.strip_suffix(tail));
    let what_does = || {
        let rest = text.strip_prefix("what does ")?;
        [" mean", " stand for", " stands for"]
            .iter()
            .find_map(|tail| rest.strip_suffix(tail))
    };
    let is_word = || {
        let rest = text.strip_prefix("is ")?;
        [" a word", " a real word"]
            .iter()
            .find_map(|tail| rest.strip_suffix(tail))
    };
    let meaning = |word| (word, WordAsk::Meaning);
    let others = |word| (word, WordAsk::Others);
    let (word, asked) = lead(MEANING_LEADS)
        .map(meaning)
        .or_else(|| lead(OTHERS_LEADS).map(others))
        .or_else(|| {
            tail(MEANING_TAILS)
                .or_else(what_does)
                .or_else(is_word)
                .map(meaning)
        })
        .or_else(|| tail(OTHERS_TAILS).map(others))?;
    let word = word.trim();
    let word = word.strip_prefix("the word ").unwrap_or(word);
    // "meaning of shiloh name" asks what a name means, not a dictionary,
    // and "how to spell check" for a spelling checker.
    if word.ends_with(" name") || word == "check" {
        return None;
    }
    let n = word.split(' ').count();
    (!word.is_empty()
        && n <= 2
        && word
            .chars()
            .all(|c| c.is_alphabetic() || c == ' ' || c == '-'))
    .then(|| (word.to_string(), asked))
}

/// Languages people translate between, as they type them.
const LANGUAGES: &[&str] = &[
    "english",
    "spanish",
    "french",
    "german",
    "italian",
    "portuguese",
    "chinese",
    "japanese",
    "korean",
    "russian",
    "arabic",
    "hindi",
    "dutch",
    "polish",
    "turkish",
    "vietnamese",
    "greek",
    "swedish",
    "ukrainian",
    "tagalog",
    "latin",
    "hebrew",
    "persian",
    "thai",
    "indonesian",
    "ingles",
    "espanol",
    "frances",
    "aleman",
];

/// Calculators people look for by what they work out.
const CALCULATORS: &[&str] = &[
    "bmi",
    "mortgage",
    "loan",
    "tip",
    "percentage",
    "percent",
    "age",
    "gpa",
    "grade",
    "calorie",
    "tdee",
    "compound interest",
    "interest",
    "retirement",
    "salary",
    "paycheck",
    "fraction",
    "date",
    "time",
    "pregnancy",
    "due date",
    "ovulation",
    "body fat",
    "macro",
    "car loan",
    "auto loan",
    "investment",
    "inflation",
    "square footage",
    "sales tax",
    "tax",
    "income tax",
    "hours",
    "time card",
    "bac",
    "concrete",
    "pace",
];

/// Tools people search for by what they do: "calculator", "translate",
/// "speed test", "nvidia stock".
fn tools(text: &str) -> Option<Route> {
    let sites = |domains: &[&'static str]| {
        Some(Route {
            sources: domains.iter().map(|d| Source::site(d)).collect(),
        })
    };
    let words: Vec<&str> = text.split(' ').collect();
    match text {
        "calculator" | "calc" | "online calculator" | "scientific calculator" | "calculadora" => {
            return sites(&["desmos.com", "calculator.net"])
        }
        "graphing calculator" => return sites(&["desmos.com", "geogebra.org"]),
        "speed test"
        | "speedtest"
        | "internet speed test"
        | "wifi speed test"
        | "internet speed"
        | "speed test internet"
        | "net speed test" => return sites(&["speedtest.net", "fast.com", "measurementlab.net"]),
        "translate" | "translator" | "translation" | "traductor" | "traducteur" | "tradutor"
        | "traduttore" | "ubersetzer" | "traduction" | "traducir" | "traduccion" => {
            return if text.starts_with("tradu") && !text.starts_with("traduction") {
                sites(&["translate.google.com", "deepl.com", "spanishdict.com"])
            } else {
                sites(&["translate.google.com", "deepl.com"])
            };
        }
        "stock market" | "stock market today" | "stocks" | "dow jones" | "dow" | "djia"
        | "dow jones today" | "s p 500" | "sp 500" | "sp500" | "s p" | "nasdaq composite"
        | "stock market news" | "market today" => {
            return sites(&["finance.yahoo.com", "stockanalysis.com"])
        }
        _ => {}
    }
    // "english to spanish", "spanish translation", "translate to french".
    let is_language = |w: &str| LANGUAGES.contains(&w);
    let translation = match words.as_slice() {
        [a, "to", b] => is_language(a) && is_language(b),
        ["translate", rest @ ..] => !rest.is_empty() && rest.len() <= 4,
        [a, "translation" | "translator" | "translate"] => is_language(a),
        [a, "to", b, "translation" | "translator" | "translate"] => {
            is_language(a) && is_language(b)
        }
        // "how do you say thank you in german", "happy birthday in spanish
        // translation", "mood swings meaning in hindi". A bare "bible in
        // spanish" may ask for one written in it, so it is not one.
        ["how", "do" | "would" | "can", "you" | "i" | "we", "say", phrase @ .., "in", to]
        | ["how", "to", "say", phrase @ .., "in", to]
        | [phrase @ .., "in", to, "translation"]
        | [phrase @ .., "meaning", "in", to] => {
            !phrase.is_empty() && phrase.len() <= 5 && is_language(to)
        }
        _ => false,
    };
    if translation {
        let spanish = words
            .iter()
            .any(|w| matches!(*w, "spanish" | "espanol" | "ingles"));
        return if spanish {
            sites(&["spanishdict.com", "translate.google.com", "deepl.com"])
        } else {
            sites(&["translate.google.com", "deepl.com"])
        };
    }
    // "bmi calculator", "mortgage calculator".
    if text
        .strip_suffix(" calculator")
        .is_some_and(|what| CALCULATORS.contains(&what))
    {
        return sites(&["calculator.net", "omnicalculator.com"]);
    }
    // "nvidia stock", "apple stock price", "tesla share price".
    let stock = [
        "stock",
        "stocks",
        "stock price",
        "share price",
        "shares",
        "stock quote",
    ]
    .iter()
    .any(|end| {
        text.strip_suffix(end)
            .is_some_and(|name| name.ends_with(' ') && name.trim().split(' ').count() <= 3)
    });
    if stock {
        return sites(&["finance.yahoo.com", "stockanalysis.com"]);
    }
    None
}

/// What people eat out, as they search for it near them.
const FOOD: &[&str] = &[
    "food",
    "restaurants",
    "restaurant",
    "places to eat",
    "food places",
    "takeout",
    "take out",
    "delivery",
    "food delivery",
    "breakfast",
    "brunch",
    "lunch",
    "dinner",
    "pizza",
    "sushi",
    "chinese food",
    "mexican food",
    "thai food",
    "indian food",
    "italian food",
    "fast food",
    "burgers",
    "tacos",
    "bbq",
    "seafood",
    "buffet",
    "bars",
    "bar",
    "pubs",
    "steakhouse",
    "ramen",
    "pho",
    "bakery",
    "donuts",
    "ice cream",
    "wings",
    "chinese restaurant",
    "mexican restaurant",
    "italian restaurant",
    "thai restaurant",
    "indian restaurant",
];

/// "food near me", "gas station near me": the sites that list such places
/// around the searcher. A list of places for their town is shown above
/// these when they gave one.
fn near_me(text: &str) -> Option<Route> {
    let what = plumb_index::places::without_near_me(text)?;
    let what = what
        .strip_prefix("best ")
        .or_else(|| what.strip_prefix("good "))
        .or_else(|| what.strip_prefix("cheap "))
        .unwrap_or(&what);
    let sites = |domains: &[&'static str]| {
        Some(Route {
            sources: domains.iter().map(|d| Source::site(d)).collect(),
        })
    };
    if FOOD.contains(&what) {
        return if what.starts_with("restaurant") || what == "dinner" || what == "brunch" {
            sites(&["yelp.com", "opentable.com", "tripadvisor.com"])
        } else {
            sites(&["yelp.com", "tripadvisor.com"])
        };
    }
    match what {
        "gas" | "gas station" | "gas stations" | "gas prices" | "cheap gas" | "petrol station" => {
            sites(&["gasbuddy.com"])
        }
        "coffee" | "coffee shop" | "coffee shops" | "cafe" | "cafes" | "coffee places" => {
            sites(&["yelp.com"])
        }
        _ => None,
    }
}

/// A government service's page for one country: the country's code, the
/// site's domain as the index keys it, the page's address and its title.
type OfficialPage = (&'static str, &'static str, &'static str, &'static str);

/// Government services that paid middlemen also sell under names that
/// look official ("U.S. Passport Online Application", "US Birth
/// Certificates and Vital Records"), which rank well for the service's
/// words: what the service is called, and its official page by country
/// ([`OfficialPage`]).
const GOVERNMENT: &[(&[&str], &[OfficialPage])] = &[
    (
        &["passport", "passports"],
        &[
            (
                "US",
                "travel.state.gov",
                "https://travel.state.gov/content/travel/en/passports.html",
                "U.S. Passports | Travel.State.Gov",
            ),
            (
                "GB",
                "www.gov.uk",
                "https://www.gov.uk/apply-renew-passport",
                "Apply for or renew a British passport - GOV.UK",
            ),
        ],
    ),
    (
        &[
            "birth certificate",
            "birth certificates",
            "death certificate",
            "marriage certificate",
            "vital records",
        ],
        &[
            (
                "US",
                "cdc.gov",
                "https://www.cdc.gov/nchs/w2w/index.htm",
                "Where to Write for Vital Records | CDC",
            ),
            (
                "GB",
                "www.gov.uk",
                "https://www.gov.uk/order-copy-birth-death-marriage-certificate",
                "Order a copy of a birth, death or marriage certificate - GOV.UK",
            ),
        ],
    ),
    (
        &["real id"],
        &[(
            "US",
            "usa.gov",
            "https://www.usa.gov/real-id",
            "REAL ID | USAGov",
        )],
    ),
    (
        &["social security card", "social security cards"],
        &[(
            "US",
            "ssa.gov",
            "https://www.ssa.gov/number-card",
            "Social Security number and card | SSA",
        )],
    ),
];

/// Words around a service's name that ask how to get or use it.
const GOVERNMENT_ASKING: &[&str] = &[
    "how",
    "where",
    "what",
    "when",
    "get",
    "getting",
    "apply",
    "applying",
    "application",
    "renew",
    "renewing",
    "renewal",
    "replace",
    "replacement",
    "lost",
    "stolen",
    "order",
    "request",
    "copy",
    "certified",
    "expedite",
    "expedited",
    "status",
    "check",
    "cost",
    "fee",
    "fees",
    "need",
    "required",
    "requirements",
    "documents",
    "long",
    "much",
    "take",
    "new",
    "first",
    "online",
    "form",
    "forms",
];

/// Words that may go with those, saying nothing themselves: "my" alone
/// does not ask for a passport ("my passport" is a hard drive).
const GOVERNMENT_FILLER: &[&str] = &[
    "can", "do", "does", "is", "i", "you", "it", "to", "a", "an", "the", "my", "for", "of", "s",
];

/// Words that say whose service is meant, and the country's code.
const GOVERNMENT_COUNTRIES: &[(&str, &str)] = &[
    ("us", "US"),
    ("usa", "US"),
    ("u", "US"),
    ("american", "US"),
    ("uk", "GB"),
    ("british", "GB"),
    ("britain", "GB"),
];

/// The official page of the government service `text` (normalized) asks
/// how to get: "how to renew passport", "lost passport", "how to get a
/// birth certificate", "real id requirements". Only the service's name
/// with words that ask for it or say whose it is, and one of those: "us
/// passport" or "passport renewal", not "passport photo", "laravel
/// passport" or "my passport". The country said, else the searcher's
/// (the United States when not given).
fn government(text: &str, country: Option<&str>) -> Option<Route> {
    let words: Vec<&str> = text.split(' ').collect();
    GOVERNMENT.iter().find_map(|(names, pages)| {
        let rest = names.iter().find_map(|name| {
            let name: Vec<&str> = name.split(' ').collect();
            let at = words.windows(name.len()).position(|run| run == name)?;
            Some([&words[..at], &words[at + name.len()..]].concat())
        })?;
        let said = rest.iter().find_map(|word| {
            GOVERNMENT_COUNTRIES
                .iter()
                .find(|(country_word, _)| country_word == word)
                .map(|(_, code)| *code)
        });
        let asks = rest.iter().any(|word| GOVERNMENT_ASKING.contains(word));
        let known = |word: &&str| {
            GOVERNMENT_ASKING.contains(word)
                || GOVERNMENT_FILLER.contains(word)
                || GOVERNMENT_COUNTRIES.iter().any(|(name, _)| name == word)
        };
        if !rest.iter().all(known) || !(asks || said.is_some()) {
            return None;
        }
        let country = said.or(country).unwrap_or("US");
        let &(_, domain, url, title) = pages
            .iter()
            .find(|(code, ..)| code.eq_ignore_ascii_case(country))?;
        Some(Route {
            sources: vec![Source {
                domain,
                page: Some((url.to_string(), title.to_string())),
            }],
        })
    })
}

/// Lists `route`'s sources first among `hits`, below a first site the
/// query names in full, each as the index has it (`site` looks one up) or
/// moved up from further down. Sources the index lacks are left out. The
/// pages placed among the sites keep their place relative to the other
/// sites.
pub fn lead_with(
    hits: &mut Vec<Hit>,
    pages: &mut [PlacedPage],
    route: &Route,
    site: impl Fn(&str) -> Option<Hit>,
) {
    let start = usize::from(hits.first().is_some_and(|hit| hit.named));
    let before: Vec<String> = hits.iter().map(|hit| hit.domain.clone()).collect();
    let mut sources = Vec::new();
    for source in &route.sources {
        if hits[..start].iter().any(|hit| hit.domain == source.domain)
            || sources.iter().any(|hit: &Hit| hit.domain == source.domain)
        {
            continue;
        }
        let found = match hits.iter().position(|hit| hit.domain == source.domain) {
            Some(at) => Some(hits.remove(at)),
            None => site(source.domain),
        };
        let Some(mut hit) = found else { continue };
        if let Some((url, title)) = &source.page {
            hit.url = url.clone();
            hit.title = Some(title.clone());
            hit.key_pages.clear();
        }
        sources.push(hit);
    }
    if sources.is_empty() {
        return;
    }
    let moved: Vec<String> = sources.iter().map(|hit| hit.domain.clone()).collect();
    hits.splice(start..start, sources);
    // A page listed before a site stays before it; one listed before a
    // source that moved up goes before the next site that did not.
    for page in pages.iter_mut().filter(|page| page.under.is_none()) {
        let next = before
            .iter()
            .enumerate()
            .skip(page.at)
            .find(|(i, domain)| *i < start || !moved.contains(domain));
        page.at = match next {
            Some((_, domain)) => hits
                .iter()
                .position(|hit| &hit.domain == domain)
                .unwrap_or(hits.len()),
            None => hits.len(),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domains(route: Option<Route>) -> Vec<&'static str> {
        route
            .map(|r| r.sources.iter().map(|s| s.domain).collect())
            .unwrap_or_default()
    }

    #[test]
    fn routes_tools_and_quick_facts() {
        assert_eq!(
            domains(route("weather", None, Some("US"))),
            ["weather.gov", "weather.com", "accuweather.com"]
        );
        assert_eq!(
            domains(route("weather in paris", None, Some("FR"))),
            ["weather.com", "accuweather.com"]
        );
        assert_eq!(
            domains(route("uk weather", None, Some("US"))),
            ["metoffice.gov.uk", "weather.com", "accuweather.com"]
        );
        assert_eq!(domains(route("clima", None, Some("ES")))[0], "aemet.es");
        assert_eq!(
            domains(route("what time is it", None, Some("DE")))[0],
            "time.is"
        );
        assert_eq!(
            domains(route("time in london", Some(Kind::Time), None)),
            ["time.is", "timeanddate.com"]
        );
        assert_eq!(
            domains(route("20 usd to eur", Some(Kind::Currency), None)),
            ["xe.com", "wise.com"]
        );
        assert_eq!(domains(route("calculator", None, None))[0], "desmos.com");
        assert_eq!(
            domains(route("bmi calculator", None, None))[0],
            "calculator.net"
        );
        assert_eq!(
            domains(route("nvidia stock", None, None))[0],
            "finance.yahoo.com"
        );
        assert_eq!(
            domains(route("english to spanish", None, None))[0],
            "spanishdict.com"
        );
        assert_eq!(
            domains(route("traductor", None, None))[0],
            "translate.google.com"
        );
        // Asking how to say something in a language.
        for (query, first) in [
            ("how do you say thank you in german", "translate.google.com"),
            ("how to say good morning in french", "translate.google.com"),
            ("How do you say “cat” in Spanish?", "spanishdict.com"),
            ("happy birthday in spanish translation", "spanishdict.com"),
            ("mood swings meaning in hindi", "translate.google.com"),
        ] {
            assert_eq!(domains(route(query, None, None))[0], first, "{query}");
        }
        // Something written in a language, or said in a place, is not.
        for query in [
            "bible in spanish",
            "movies in hindi",
            "how do you say goodbye in a letter",
            "how to say no in a meeting",
        ] {
            assert!(route(query, None, None).is_none(), "{query}");
        }
        assert_eq!(domains(route("speed test", None, None))[0], "speedtest.net");
        assert_eq!(domains(route("food near me", None, None))[0], "yelp.com");
        assert_eq!(
            domains(route("restaurants near me", None, None)),
            ["yelp.com", "opentable.com", "tripadvisor.com"]
        );
        assert_eq!(
            domains(route("gas station near me", None, None)),
            ["gasbuddy.com"]
        );
        let define = route("define prioritize", None, None).unwrap();
        assert_eq!(
            define.sources[0].page.as_ref().unwrap().0,
            "https://www.merriam-webster.com/dictionary/prioritize"
        );
        assert_eq!(
            define.sources[2].page.as_ref().unwrap().0,
            "https://en.wiktionary.org/wiki/prioritize"
        );
    }

    #[test]
    fn routes_government_services_to_their_official_page() {
        let page = |query: &str, country: Option<&str>| {
            route(query, None, country)
                .and_then(|route| route.sources[0].page.clone())
                .map(|(url, _)| url)
        };
        let passports = "https://travel.state.gov/content/travel/en/passports.html";
        for query in [
            "how to renew passport",
            "how to get a passport",
            "passport renewal",
            "lost passport",
            "how long does it take to get a passport",
            "us passport",
            "u.s. passport application",
        ] {
            assert_eq!(page(query, None).as_deref(), Some(passports), "{query}");
        }
        assert_eq!(
            page("how to renew my passport", Some("GB")).as_deref(),
            Some("https://www.gov.uk/apply-renew-passport")
        );
        // Whose passport is said: a British one from the United States.
        assert_eq!(
            page("uk passport renewal", Some("US")).as_deref(),
            Some("https://www.gov.uk/apply-renew-passport")
        );
        assert_eq!(
            page("how to get a birth certificate", None).as_deref(),
            Some("https://www.cdc.gov/nchs/w2w/index.htm")
        );
        assert_eq!(
            page("order a copy of my birth certificate", Some("GB")).as_deref(),
            Some("https://www.gov.uk/order-copy-birth-death-marriage-certificate")
        );
        assert_eq!(
            page("real id requirements", None).as_deref(),
            Some("https://www.usa.gov/real-id")
        );
        for query in [
            "how to get a social security card",
            "replace social security card",
            "lost social security card",
        ] {
            assert_eq!(
                page(query, None).as_deref(),
                Some("https://www.ssa.gov/number-card"),
                "{query}"
            );
        }
        // No page for that country, or words that ask for something else.
        assert_eq!(page("how to renew passport", Some("FR")), None);
        for query in [
            "passport",
            "my passport",
            "passport photo",
            "laravel passport get token",
            "passport index",
            "passport office near me",
            "obama birth certificate",
            "birth certificate translation",
            "social security card template",
        ] {
            assert_eq!(page(query, None), None, "{query}");
        }
    }

    #[test]
    fn leaves_other_searches_alone() {
        for query in [
            "python",
            "old navy",
            "safeway",
            "weather channel",
            "time magazine",
            "netflix",
            "pizza in denver",
            "urgent care near me",
            "how to define a function in python",
            "translate python to rust code in my project today",
            "weather data api",
            "square fee calculator",
            "meaning of shiloh name",
        ] {
            assert_eq!(route(query, None, None), None, "{query}");
        }
        for query in [
            "define",
            "how to spell check",
            "plural of",
            "first person plural",
            "simple past tense",
            "how to pronounce my name",
            "english pronunciation",
            "is this a good word to use here",
        ] {
            assert_eq!(word_asked(query), None, "{query}");
        }
        for (query, word, asked) in [
            ("prioritize meaning", "prioritize", WordAsk::Meaning),
            ("what does ubiquitous mean", "ubiquitous", WordAsk::Meaning),
            ("what does asap stand for", "asap", WordAsk::Meaning),
            (
                "whats the meaning of serendipity",
                "serendipity",
                WordAsk::Meaning,
            ),
            ("plural of mouse", "mouse", WordAsk::Meaning),
            ("whats the plural of cactus", "cactus", WordAsk::Meaning),
            ("past tense of swim", "swim", WordAsk::Meaning),
            ("how to spell necessary", "necessary", WordAsk::Meaning),
            (
                "how do you spell definitely",
                "definitely",
                WordAsk::Meaning,
            ),
            ("define synonym", "synonym", WordAsk::Meaning),
            ("how do you pronounce gif", "gif", WordAsk::Meaning),
            (
                "pronunciation of worcestershire",
                "worcestershire",
                WordAsk::Meaning,
            ),
            ("is irregardless a word", "irregardless", WordAsk::Meaning),
            ("is alot a real word", "alot", WordAsk::Meaning),
            ("synonyms for good", "good", WordAsk::Others),
            ("another word for very happy", "very happy", WordAsk::Others),
            (
                "whats another word for important",
                "important",
                WordAsk::Others,
            ),
            ("opposite of brave", "brave", WordAsk::Others),
            ("antonym of generous", "generous", WordAsk::Others),
            ("happy synonyms", "happy", WordAsk::Others),
        ] {
            assert_eq!(word_asked(query), Some((word.into(), asked)), "{query}");
        }
    }

    fn hit(domain: &str, named: bool) -> Hit {
        Hit {
            domain: domain.into(),
            url: format!("https://{domain}/"),
            title: None,
            description: None,
            score: 0.0,
            text_score: 0.0,
            link_score: 0.0,
            placing_text_score: None,
            country: None,
            named,
            official: false,
            key_pages: Vec::new(),
            demand: None,
            missing_words: false,
            query_evidence: None,
        }
    }

    fn placed(at: usize) -> PlacedPage {
        PlacedPage {
            hit: plumb_index::pages::PageHit {
                page: plumb_index::pages::Page {
                    paper: None,
                    set: "wikipedia-en".to_string(),
                    url: "https://en.wikipedia.org/wiki/Weather".to_string(),
                    title: "Weather".to_string(),
                    description: None,
                    site: None,
                    views: 1000,
                    aliases: Vec::new(),
                    item: None,
                    profiles: Vec::new(),
                    website: None,
                    package: None,
                    facts: Vec::new(),
                    lead: None,
                    names: Vec::new(),
                    sections: Vec::new(),
                    content_language: None,
                    search: None,
                },
                score: 1.0,
                named: true,
                popularity: 0.0,
                whole: false,
                learned: None,
                meant: false,
            },
            under: None,
            at,
        }
    }

    #[test]
    fn sources_come_first_and_pages_keep_their_sites() {
        let route = route("weather", None, Some("US")).unwrap();
        let mut hits = vec![
            hit("wikipedia-ish.org", false),
            hit("weather.com", false),
            hit("zaxbys.com", false),
        ];
        // Before the first site, and before weather.com, which moves up.
        let mut pages = vec![placed(0), placed(1), placed(9)];
        lead_with(&mut hits, &mut pages, &route, |domain| {
            (domain == "weather.gov").then(|| hit(domain, false))
        });
        let order: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
        // accuweather.com is not in the index.
        assert_eq!(
            order,
            [
                "weather.gov",
                "weather.com",
                "wikipedia-ish.org",
                "zaxbys.com"
            ]
        );
        assert_eq!(pages.iter().map(|p| p.at).collect::<Vec<_>>(), [2, 3, 4]);
    }

    #[test]
    fn a_named_site_stays_first() {
        let route = route("safeway near me", None, None);
        assert_eq!(route, None);
        let route = super::route("nvidia stock", None, None).unwrap();
        let mut hits = vec![hit("nvidia.com", true), hit("hetzner.com", false)];
        lead_with(&mut hits, &mut [], &route, |domain| {
            Some(hit(domain, false))
        });
        let order: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
        assert_eq!(
            order,
            [
                "nvidia.com",
                "finance.yahoo.com",
                "stockanalysis.com",
                "hetzner.com"
            ]
        );
    }

    #[test]
    fn a_definition_links_to_the_word() {
        let route = route("define prioritize", None, None).unwrap();
        let mut hits = vec![hit("prioritize.io", false)];
        lead_with(&mut hits, &mut [], &route, |domain| {
            Some(hit(domain, false))
        });
        assert_eq!(
            hits[0].url,
            "https://www.merriam-webster.com/dictionary/prioritize"
        );
        assert_eq!(hits[1].url, "https://www.dictionary.com/browse/prioritize");
        assert_eq!(hits[2].url, "https://en.wiktionary.org/wiki/prioritize");
        assert_eq!(hits[3].domain, "prioritize.io");
    }

    #[test]
    fn synonyms_and_opposites_link_to_the_thesauruses() {
        let route = route("opposite of brave", None, None).unwrap();
        let mut hits = vec![hit("brave.com", false), hit("thesaurus.com", false)];
        lead_with(&mut hits, &mut [], &route, |domain| {
            Some(hit(domain, false))
        });
        assert_eq!(
            hits[0].url,
            "https://www.merriam-webster.com/thesaurus/brave"
        );
        assert_eq!(hits[1].url, "https://www.thesaurus.com/browse/brave");
        assert_eq!(hits[2].url, "https://en.wiktionary.org/wiki/brave");
        assert_eq!(hits[3].domain, "brave.com");
    }
}
