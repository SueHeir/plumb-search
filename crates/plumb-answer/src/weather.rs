//! The weather somewhere: "weather", "weather in denver", "denver weather
//! tomorrow", "forecast", "clima en madrid".
//!
//! [`asked`] reads what the query asks; the caller finds the place, fetches
//! its forecast (a node uses Open-Meteo) and hands it to [`answer`], which
//! words it. Nothing here reads the network.

use crate::{simplify, Answer, Kind};

/// What a weather query asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeatherAsked {
    /// The place named, as typed: "denver" of "weather in denver". `None`
    /// for "weather" alone, which is about the searcher's own town.
    pub place: Option<String>,
    /// Whether the place was named with "in", "for" or "en", rather than
    /// guessed from the words beside "weather" ("denver weather"), which
    /// may be a name instead.
    pub said_where: bool,
    /// Tomorrow's weather rather than today's.
    pub tomorrow: bool,
}

/// Words that ask for the weather.
const WEATHER_WORDS: &[&str] = &[
    "weather",
    "forecast",
    "weather forecast",
    "clima",
    "el tiempo",
    "tiempo",
    "wetter",
    "meteo",
    "météo",
    "temperature",
];
/// Words that say when or which part of the weather, left out of the place.
const WHEN_WORDS: &[&str] = &[
    "today",
    "tomorrow",
    "now",
    "right now",
    "tonight",
    "this week",
    "this weekend",
    "weekend",
    "hourly",
    "10 day",
    "7 day",
    "10 days",
    "7 days",
    "radar",
    "report",
    "map",
    "alerts",
    "warnings",
    "outside",
    "hoy",
    "mañana",
    "manana",
    "heute",
    "morgen",
];
/// Words after "weather" that make a name, not a weather query: "weather
/// channel", "weather underground", "weather app".
const NAMES: &[&str] = &[
    "channel",
    "underground",
    "app",
    "apps",
    "network",
    "nation",
    "girls",
    "man",
    "api",
    "apis",
    "data",
    "dataset",
    "widget",
    "station",
    "stations",
    "service",
    "company",
    "balloon",
    "vane",
    "eye",
    "tech",
    "bug",
    "strip",
    "stripping",
    "proof",
    "proofing",
    "pattern",
    "patterns",
    "wise",
    "front",
    "fronts",
    "the storm",
    "with you",
    "song",
    "lyrics",
    "movie",
    "film",
    "book",
    "definition",
    "meaning",
    "vs climate",
    "and climate",
    "of",
    "how",
    "what",
    "why",
    "is",
    "does",
];
/// Words beside "weather" that make it about weather in general, not
/// somewhere: "under the weather", "nice weather", "space weather".
const NOT_PLACE_WORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "under", "stormy", "bad", "good", "nice", "cold", "hot", "fair",
    "rainy", "sunny", "extreme", "space", "severe", "wet", "dry", "warm", "my", "your", "this",
    "that", "is", "are", "how", "what", "why", "when", "does", "do", "to",
];
/// Words joining "weather" to the place: "weather in denver".
const WHERE_WORDS: &[&str] = &["in", "for", "at", "near", "en", "de", "à", "in der", "of"];

/// What `query` asks of the weather, if it asks for it.
pub fn asked(query: &str) -> Option<WeatherAsked> {
    let text = simplify(query);
    if text.chars().count() > 60 {
        return None;
    }
    let mut words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
    if words.first() == Some(&"what's") || words.first() == Some(&"whats") {
        words.remove(0);
    }
    if words.len() >= 2 && words[..2] == ["what", "is"] {
        words.drain(..2);
    }
    if words.first() == Some(&"the") {
        words.remove(0);
    }
    let mut tomorrow = false;
    // When-words at either end: "weather tomorrow", "tomorrow weather".
    loop {
        let before = words.len();
        for when in WHEN_WORDS {
            let n = when.split(' ').count();
            if words.len() > n && words[words.len() - n..].join(" ") == *when {
                tomorrow |= matches!(*when, "tomorrow" | "mañana" | "manana" | "morgen");
                words.truncate(words.len() - n);
            } else if words.len() > n && words[..n].join(" ") == *when {
                tomorrow |= matches!(*when, "tomorrow" | "mañana" | "manana" | "morgen");
                words.drain(..n);
            }
        }
        if words.len() == before {
            break;
        }
    }
    let joined = words.join(" ");
    // The longest weather word at the start or the end.
    let mut found = None;
    for weather in WEATHER_WORDS {
        let n = weather.split(' ').count();
        if words.len() >= n && words[..n].join(" ") == *weather {
            let rest = &words[n..];
            if found.as_ref().is_none_or(|(m, _, _)| n > *m) {
                found = Some((n, rest.to_vec(), true));
            }
        } else if words.len() > n && words[words.len() - n..].join(" ") == *weather {
            let rest = &words[..words.len() - n];
            if found.as_ref().is_none_or(|(m, _, _)| n > *m) {
                found = Some((n, rest.to_vec(), false));
            }
        }
    }
    let (_, rest, first) = found?;
    if rest.is_empty() {
        // "tiempo" and "temperature" alone are words, not weather.
        return matches!(
            joined.as_str(),
            "weather"
                | "forecast"
                | "weather forecast"
                | "clima"
                | "el tiempo"
                | "wetter"
                | "meteo"
                | "météo"
        )
        .then_some(WeatherAsked {
            place: None,
            said_where: false,
            tomorrow,
        })
        .or_else(|| {
            (tomorrow && joined == "temperature").then_some(WeatherAsked {
                place: None,
                said_where: false,
                tomorrow,
            })
        });
    }
    let rest = rest.join(" ");
    if NAMES
        .iter()
        .any(|name| rest == *name || rest.starts_with(&format!("{name} ")))
    {
        return None;
    }
    // "weather in denver", "forecast for paris", "clima en madrid".
    if first {
        for joiner in WHERE_WORDS {
            if let Some(place) = rest.strip_prefix(&format!("{joiner} ")) {
                return Some(WeatherAsked {
                    place: Some(place.trim().to_string()),
                    said_where: true,
                    tomorrow,
                });
            }
        }
    }
    // "temperature" asks for the weather only with "in": "body temperature"
    // and "temperature of the sun" do not.
    let weather_word = text.contains("weather")
        || text.contains("forecast")
        || text.contains("clima")
        || text.contains("tiempo")
        || text.contains("wetter")
        || text.contains("meteo")
        || text.contains("météo");
    if !weather_word {
        return None;
    }
    // "denver weather", "weather denver": a place, if it is one; "under
    // the weather" and "nice weather" are not.
    let not_a_place = rest.split(' ').any(|w| NOT_PLACE_WORDS.contains(&w));
    (!not_a_place && rest.split(' ').count() <= 4).then_some(WeatherAsked {
        place: Some(rest),
        said_where: false,
        tomorrow,
    })
}

/// A forecast for a place, in degrees Celsius.
#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    /// The temperature now.
    pub now: f64,
    /// What it is like now, a WMO weather code.
    pub now_code: u8,
    /// Today first.
    pub days: Vec<Day>,
}

/// One day of a [`Forecast`].
#[derive(Debug, Clone, PartialEq)]
pub struct Day {
    /// `2026-10-09`.
    pub date: String,
    pub high: f64,
    pub low: f64,
    /// A WMO weather code.
    pub code: u8,
    /// Chance of rain or snow, in percent.
    pub wet_chance: Option<u8>,
}

/// Where a forecast comes from, as the answer credits it.
pub const SOURCE: &str = "Open-Meteo.com (CC BY 4.0)";

/// The answer for `asked` from `forecast`, the forecast for `place` (as
/// shown: "Denver, CO"), in Fahrenheit when `fahrenheit`.
pub fn answer(
    asked: &WeatherAsked,
    place: &str,
    forecast: &Forecast,
    fahrenheit: bool,
) -> Option<Answer> {
    let degrees = |c: f64| {
        let value = if fahrenheit { c * 9.0 / 5.0 + 32.0 } else { c };
        format!(
            "{}°{}",
            value.round() as i64,
            if fahrenheit { "F" } else { "C" }
        )
    };
    let short = |c: f64| {
        let value = if fahrenheit { c * 9.0 / 5.0 + 32.0 } else { c };
        format!("{}°", value.round() as i64)
    };
    let wet = |day: &Day| {
        day.wet_chance
            .filter(|chance| *chance >= 20)
            .map(|chance| format!(", {chance}% chance of {}", wet_word(day.code)))
            .unwrap_or_default()
    };
    let today = forecast.days.first()?;
    let (question, answer) = if asked.tomorrow {
        let day = forecast.days.get(1)?;
        (
            format!("Weather tomorrow in {place}"),
            format!(
                "{}, high {}, low {}{}",
                capitalized(describe(day.code)),
                degrees(day.high),
                degrees(day.low),
                wet(day)
            ),
        )
    } else {
        (
            format!("Weather in {place}"),
            format!(
                "{}, {}. Today high {}, low {}{}",
                degrees(forecast.now),
                describe(forecast.now_code),
                short(today.high),
                short(today.low),
                wet(today)
            ),
        )
    };
    let start = if asked.tomorrow { 2 } else { 1 };
    let days: Vec<String> = forecast
        .days
        .iter()
        .skip(start)
        .take(3)
        .filter_map(|day| {
            Some(format!(
                "{} {}/{} {}",
                weekday(&day.date)?,
                short(day.high),
                short(day.low),
                describe(day.code)
            ))
        })
        .collect();
    let mut note = days.join(" · ");
    if !note.is_empty() {
        note.push_str(" · ");
    }
    note.push_str("Forecast by ");
    note.push_str(SOURCE);
    Some(Answer {
        kind: Kind::Weather,
        question,
        answer,
        note: Some(note),
    })
}

/// What WMO weather code `code` means, in a few words.
pub fn describe(code: u8) -> &'static str {
    match code {
        0 => "clear",
        1 => "mostly clear",
        2 => "partly cloudy",
        3 => "cloudy",
        45 | 48 => "fog",
        51 | 53 | 55 => "drizzle",
        56 | 57 => "freezing drizzle",
        61 => "light rain",
        63 => "rain",
        65 => "heavy rain",
        66 | 67 => "freezing rain",
        71 => "light snow",
        73 => "snow",
        75 => "heavy snow",
        77 => "snow grains",
        80 => "light showers",
        81 => "showers",
        82 => "heavy showers",
        85 | 86 => "snow showers",
        95 => "thunderstorms",
        96 | 99 => "thunderstorms with hail",
        _ => "mixed",
    }
}

/// "rain" or "snow", for the chance of either on a day like `code`.
fn wet_word(code: u8) -> &'static str {
    match code {
        71..=77 | 85 | 86 => "snow",
        _ => "rain",
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// "Fri" for `2026-10-09`.
fn weekday(date: &str) -> Option<&'static str> {
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days since 1970-01-01 (a Thursday), by Howard Hinnant's method.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    const NAMES: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    Some(NAMES[days.rem_euclid(7) as usize])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place_of(query: &str) -> Option<Option<String>> {
        asked(query).map(|a| a.place)
    }

    #[test]
    fn reads_weather_queries() {
        assert_eq!(place_of("weather"), Some(None));
        assert_eq!(place_of("Weather?"), Some(None));
        assert_eq!(place_of("weather today"), Some(None));
        assert_eq!(place_of("forecast"), Some(None));
        assert_eq!(place_of("clima"), Some(None));
        assert_eq!(place_of("weather radar"), Some(None));
        assert_eq!(place_of("what's the weather"), Some(None));
        assert_eq!(place_of("weather in denver"), Some(Some("denver".into())));
        assert_eq!(
            place_of("weather for paris france"),
            Some(Some("paris france".into()))
        );
        assert_eq!(place_of("denver weather"), Some(Some("denver".into())));
        assert_eq!(
            place_of("weather denver tomorrow"),
            Some(Some("denver".into()))
        );
        assert_eq!(place_of("clima en madrid"), Some(Some("madrid".into())));
        assert_eq!(
            place_of("temperature in london"),
            Some(Some("london".into()))
        );
        assert_eq!(place_of("weather tomorrow"), Some(None));
        assert!(asked("weather tomorrow").unwrap().tomorrow);
        assert!(!asked("weather today").unwrap().tomorrow);
        assert!(asked("weather in denver").unwrap().said_where);
        assert!(!asked("denver weather").unwrap().said_where);
    }

    #[test]
    fn names_and_other_questions_are_not_weather() {
        for query in [
            "weather channel",
            "the weather channel",
            "weather underground",
            "weather data api",
            "weather app",
            "temperature",
            "body temperature",
            "temperature of the sun",
            "tiempo",
            "how does weather work",
            "weather vs climate",
            "stormy weather song",
            "under the weather",
            "python",
        ] {
            assert_eq!(asked(query), None, "{query}");
        }
    }

    fn forecast() -> Forecast {
        Forecast {
            now: 12.0,
            now_code: 2,
            days: vec![
                Day {
                    date: "2026-10-09".into(),
                    high: 16.0,
                    low: 4.0,
                    code: 2,
                    wet_chance: Some(5),
                },
                Day {
                    date: "2026-10-10".into(),
                    high: 10.0,
                    low: 1.0,
                    code: 61,
                    wet_chance: Some(60),
                },
                Day {
                    date: "2026-10-11".into(),
                    high: 18.0,
                    low: 6.0,
                    code: 0,
                    wet_chance: None,
                },
            ],
        }
    }

    #[test]
    fn words_the_forecast() {
        let today = answer(&asked("weather").unwrap(), "Denver, CO", &forecast(), true).unwrap();
        assert_eq!(today.kind, Kind::Weather);
        assert_eq!(today.question, "Weather in Denver, CO");
        assert_eq!(today.answer, "54°F, partly cloudy. Today high 61°, low 39°");
        let note = today.note.unwrap();
        assert!(
            note.starts_with("Sat 50°/34° light rain · Sun 64°/43° clear"),
            "{note}"
        );
        assert!(note.ends_with(SOURCE));
        let tomorrow = answer(
            &asked("weather tomorrow").unwrap(),
            "Paris, France",
            &forecast(),
            false,
        )
        .unwrap();
        assert_eq!(
            tomorrow.answer,
            "Light rain, high 10°C, low 1°C, 60% chance of rain"
        );
    }

    #[test]
    fn days_of_the_week() {
        assert_eq!(weekday("2026-10-09"), Some("Fri"));
        assert_eq!(weekday("2000-02-29"), Some("Tue"));
        assert_eq!(weekday("1970-01-01"), Some("Thu"));
        assert_eq!(weekday("nope"), None);
    }
}
