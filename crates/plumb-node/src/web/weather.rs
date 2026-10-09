//! The weather answer: "weather", "weather in denver", "denver weather
//! tomorrow" (see [`plumb_answer::weather`]).
//!
//! The place is a town the query names, found in the node's places, or
//! else the town the searcher gave on About you; Plumb never works out
//! where someone is by itself. Its forecast comes from Open-Meteo, free and
//! without a key; the request carries the town's coordinates, rounded to
//! about a kilometre, and nothing else of the query or the searcher.
//! Forecasts are kept for a while, so a town's weather is fetched at most
//! every [`FRESH`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plumb_answer::weather::{self, Day, Forecast};
use plumb_answer::Answer;
use plumb_core::place::Place;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use super::AppState;
use crate::country::country_name;

/// Where forecasts come from.
const FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";
/// A forecast older than this is fetched again when next asked for.
const FRESH: Duration = Duration::from_secs(30 * 60);
/// After a failed fetch, how long to answer without the weather.
const RETRY: Duration = Duration::from_secs(5 * 60);
/// Longest wait for a forecast while a results page waits on it.
const TIMEOUT: Duration = Duration::from_secs(3);
/// Most places whose forecasts are kept.
const KEPT: usize = 2_000;
/// Countries that measure temperature in Fahrenheit.
const FAHRENHEIT: &[&str] = &[
    "US", "LR", "BS", "KY", "PW", "FM", "MH", "PR", "GU", "VI", "AS",
];

/// A place rounded to a hundredth of a degree.
type Spot = (i32, i32);

/// A forecast fetched (or `None`, failed to), and when.
type Fetched = (Instant, Option<Forecast>);

/// Forecasts fetched, by place.
#[derive(Default)]
pub(crate) struct WeatherCache {
    kept: Mutex<HashMap<Spot, Fetched>>,
}

impl WeatherCache {
    /// The forecast at `lat`, `lon`, if it can be had.
    async fn forecast(&self, lat: f64, lon: f64) -> Option<Forecast> {
        let key: Spot = ((lat * 100.0).round() as i32, (lon * 100.0).round() as i32);
        {
            let kept = self.kept.lock().await;
            if let Some((at, forecast)) = kept.get(&key) {
                let fresh = match forecast {
                    Some(_) => FRESH,
                    None => RETRY,
                };
                if at.elapsed() < fresh {
                    return forecast.clone();
                }
            }
        }
        let fetched = fetch(f64::from(key.0) / 100.0, f64::from(key.1) / 100.0).await;
        let forecast = match fetched {
            Ok(forecast) => Some(forecast),
            Err(err) => {
                warn!("could not fetch the weather: {err:#}");
                None
            }
        };
        let mut kept = self.kept.lock().await;
        if kept.len() >= KEPT {
            kept.retain(|_, (at, _)| at.elapsed() < FRESH);
            if kept.len() >= KEPT {
                kept.clear();
            }
        }
        kept.insert(key, (Instant::now(), forecast.clone()));
        forecast
    }
}

/// Open-Meteo's answer, as far as it is read.
#[derive(Deserialize)]
struct Response {
    current: Current,
    daily: Daily,
}

#[derive(Deserialize)]
struct Current {
    temperature_2m: f64,
    weather_code: u8,
}

#[derive(Deserialize)]
struct Daily {
    time: Vec<String>,
    temperature_2m_max: Vec<Option<f64>>,
    temperature_2m_min: Vec<Option<f64>>,
    weather_code: Vec<Option<u8>>,
    #[serde(default)]
    precipitation_probability_max: Vec<Option<u8>>,
}

async fn fetch(lat: f64, lon: f64) -> anyhow::Result<Forecast> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("plumb/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let url = format!(
        "{FORECAST_URL}?latitude={lat:.2}&longitude={lon:.2}\
         &current=temperature_2m,weather_code\
         &daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max\
         &timezone=auto&forecast_days=5"
    );
    let text = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let forecast = parse(&text)?;
    debug!("weather at {lat:.2},{lon:.2} fetched");
    Ok(forecast)
}

/// Reads Open-Meteo's answer.
fn parse(text: &str) -> anyhow::Result<Forecast> {
    let response: Response = serde_json::from_str(text)?;
    let daily = response.daily;
    let days = daily
        .time
        .iter()
        .enumerate()
        .filter_map(|(i, date)| {
            Some(Day {
                date: date.clone(),
                high: (*daily.temperature_2m_max.get(i)?)?,
                low: (*daily.temperature_2m_min.get(i)?)?,
                code: (*daily.weather_code.get(i)?)?,
                wet_chance: daily
                    .precipitation_probability_max
                    .get(i)
                    .copied()
                    .flatten(),
            })
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(!days.is_empty(), "a forecast without days");
    Ok(Forecast {
        now: response.current.temperature_2m,
        now_code: response.current.weather_code,
        days,
    })
}

/// The place as the answer names it: "Denver, CO", "Paris, France".
fn place_label(place: &Place) -> String {
    let mut label = place.name.clone();
    match (place.region.as_deref(), place.country.as_deref()) {
        (Some(region), Some("US" | "CA" | "AU")) if region.len() <= 3 => {
            label.push_str(", ");
            label.push_str(region);
        }
        (_, Some(country)) => {
            label.push_str(", ");
            label.push_str(country_name(country));
        }
        _ => {}
    }
    label
}

/// The weather answer to `query`, if it asks for the weather somewhere
/// Plumb can find: a town it names, or else `town`, the searcher's own.
/// `country` is the searcher's, to tell towns of the same name apart and
/// to pick Fahrenheit when the place says no country.
pub(crate) async fn answer(
    state: &AppState,
    query: &str,
    town: Option<&str>,
    country: Option<&str>,
) -> Option<Answer> {
    let asked = weather::asked(query)?;
    let (text, guessed) = match &asked.place {
        Some(place) => (place.clone(), !asked.said_where),
        None => (town?.to_string(), false),
    };
    let backend = Arc::clone(&state.backend);
    let owned_country = country.map(str::to_string);
    let place =
        tokio::task::spawn_blocking(move || backend.locate(&text, owned_country.as_deref()))
            .await
            .ok()
            .flatten()?;
    // "denver weather" is Denver's; "skyrim weather" is no town's.
    if guessed && !place.is_town() {
        return None;
    }
    let forecast = state.weather.forecast(place.lat, place.lon).await?;
    let fahrenheit = place
        .country
        .as_deref()
        .or(country)
        .is_some_and(|c| FAHRENHEIT.contains(&c));
    weather::answer(&asked, &place_label(&place), &forecast, fahrenheit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_open_meteo() {
        let text = r#"{"latitude":39.74,"longitude":-104.99,
            "current":{"time":"2026-10-09T02:45","temperature_2m":12.3,"weather_code":2},
            "daily":{"time":["2026-10-09","2026-10-10","2026-10-11"],
              "weather_code":[2,61,null],
              "temperature_2m_max":[16.0,10.1,18.2],
              "temperature_2m_min":[4.0,1.2,6.3],
              "precipitation_probability_max":[5,60,null]}}"#;
        let forecast = parse(text).unwrap();
        assert_eq!(forecast.now, 12.3);
        assert_eq!(forecast.now_code, 2);
        // A day missing its weather is left out.
        assert_eq!(forecast.days.len(), 2);
        assert_eq!(forecast.days[1].wet_chance, Some(60));
        assert!(parse("{}").is_err());
    }

    #[test]
    fn labels_places_as_people_write_them() {
        let place = |name: &str, region: Option<&str>, country: &str| Place {
            rank: 0,
            name: name.into(),
            kind: "place=city".into(),
            tags: Vec::new(),
            lat: 0.0,
            lon: 0.0,
            town: None,
            region: region.map(str::to_string),
            country: Some(country.into()),
            address: None,
            website: None,
            osm: "n1".into(),
            aliases: Vec::new(),
        };
        assert_eq!(
            place_label(&place("Denver", Some("CO"), "US")),
            "Denver, CO"
        );
        assert_eq!(
            place_label(&place("Paris", Some("IDF"), "FR")),
            "Paris, France"
        );
    }
}
