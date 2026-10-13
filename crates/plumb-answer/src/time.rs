//! The time in a place: `time in tokyo`, `what time is it in paris`,
//! `london time`; and a time somewhere in another place: `3pm est to pst`,
//! `15:00 london in tokyo`. A town not in the list in `places.rs` is
//! looked up by the caller ([`asked_town`], [`time_in_town`]).

use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

use crate::units::sides;
use crate::{places, simplify, zones, Answer, Kind};

/// A place's name as shown and its time zone.
struct Place {
    shown: String,
    zone: TimeZone,
}

/// The place `name` (simplified) names: a known place or zone name, an
/// IANA zone (`europe/berlin`), or an offset (`utc+5:30`).
fn place(name: &str, now: Timestamp) -> Option<Place> {
    let name = name
        .trim()
        .trim_start_matches("the ")
        .trim_end_matches(" now");
    if name.is_empty() {
        return None;
    }
    if let Some(place) = known(name) {
        return Some(place);
    }
    // "Paris, France", "St. John's Newfoundland": a place and where it
    // is, when the second keeps the same time, if this list knows it.
    // "Paris, Texas" is no Paris this list knows.
    let split = name.split_once(',').or_else(|| {
        name.rsplit_once(' ')
            .filter(|(_, region)| known(region).is_some())
    });
    if let Some((town, region)) = split {
        let place = known(town.trim())?;
        let agrees = known(region.trim())
            .is_none_or(|region| zones::same_clock(&region.zone, &place.zone, now));
        return agrees.then_some(place);
    }
    if name.contains('/') {
        let zone = TimeZone::get(name).ok()?;
        let shown = zone.iana_name()?.replace('_', " ");
        return Some(Place { shown, zone });
    }
    for prefix in ["utc", "gmt"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            let rest = rest.trim();
            let first = rest.chars().next()?;
            let sign = match first {
                '+' => 1,
                '-' | '−' => -1,
                _ => return None,
            };
            let rest = &rest[first.len_utf8()..];
            // Bare digits only: "utc+-2000000" is not an offset.
            let number = |text: &str| {
                let text = text.trim();
                (!text.is_empty() && text.len() <= 2 && text.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| text.parse::<i32>().ok())
                    .flatten()
            };
            let (hours, minutes) = match rest.split_once(':') {
                Some((h, m)) => (number(h)?, number(m)?),
                None => (number(rest)?, 0),
            };
            if hours > 14 || minutes >= 60 {
                return None;
            }
            let seconds = sign * (hours * 3600 + minutes * 60);
            let offset = jiff::tz::Offset::from_seconds(seconds).ok()?;
            return Some(Place {
                shown: format!("UTC{}", offset_text(seconds)),
                zone: TimeZone::fixed(offset),
            });
        }
    }
    None
}

/// A place in the list of places, and its zone.
fn known(name: &str) -> Option<Place> {
    let (shown, zone) = places::find(name)?;
    Some(Place {
        shown: shown.to_string(),
        zone: TimeZone::get(zone).ok()?,
    })
}

/// `+9`, `+5:30`, `-3`, or nothing for UTC itself.
fn offset_text(seconds: i32) -> String {
    if seconds == 0 {
        return String::new();
    }
    let sign = if seconds < 0 { '-' } else { '+' };
    let (hours, minutes) = (seconds.abs() / 3600, seconds.abs() % 3600 / 60);
    if minutes == 0 {
        format!("{sign}{hours}")
    } else {
        format!("{sign}{hours}:{minutes:02}")
    }
}

/// "9:41 PM"
fn clock(time: &Zoned) -> String {
    time.strftime("%-I:%M %p").to_string()
}

/// "JST, UTC+9"; just "UTC" for UTC.
fn zone_text(time: &Zoned) -> String {
    let info = time.time_zone().to_offset_info(time.timestamp());
    let offset = format!("UTC{}", offset_text(info.offset().seconds()));
    let abbreviation = info.abbreviation();
    // Zones without a name of their own are written as "+04".
    if abbreviation.starts_with(['+', '-']) || abbreviation == offset || abbreviation == "UTC" {
        offset
    } else {
        format!("{abbreviation}, {offset}")
    }
}

/// The place a question about the time now is about, if `text` asks one,
/// and whether it surely names a place: "boulder time" may be a band.
fn asked_place(text: &str) -> Option<(&str, bool)> {
    for prefix in [
        "what time is it in ",
        "what time is it at ",
        "what is the time in ",
        "what's the time in ",
        "whats the time in ",
        "current time in ",
        "local time in ",
        "the time in ",
        "time now in ",
        "time in ",
        "time at ",
        "time zone in ",
        "time zone of ",
        "timezone in ",
        "timezone of ",
        "timezone ",
        "time zone ",
    ] {
        if let Some(place) = text.strip_prefix(prefix) {
            return Some((place, true));
        }
    }
    for suffix in [
        " time now",
        " local time",
        " current time",
        " time zone",
        " timezone",
        " time",
    ] {
        if let Some(place) = text.strip_suffix(suffix) {
            return Some((place, suffix != " time"));
        }
    }
    None
}

/// A time of day: `3pm`, `3:30 pm`, `15:00`, `noon`, `midnight`, and the
/// rest of `text` after it.
fn time_of_day(text: &str) -> Option<((i8, i8), &str)> {
    for (word, at) in [("noon", (12, 0)), ("midnight", (0, 0))] {
        if let Some(rest) = text.strip_prefix(word) {
            return Some((at, rest.trim()));
        }
    }
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == ':'))
        .unwrap_or(text.len());
    let (digits, after) = text.split_at(end);
    let rest = after.trim_start();
    let (half, rest) = if let Some(rest) = rest.strip_prefix("am").or(rest.strip_prefix("a.m.")) {
        (Some(false), rest)
    } else if let Some(rest) = rest.strip_prefix("pm").or(rest.strip_prefix("p.m.")) {
        (Some(true), rest)
    } else {
        (None, after)
    };
    // "3pm london", "15:00 london"; not "3pmx" or "15:00london".
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let (hour, minute) = match digits.split_once(':') {
        Some((h, m)) if m.len() == 2 => (h.parse::<i8>().ok()?, m.parse::<i8>().ok()?),
        // A bare "3" is no time; "3pm" is.
        None if half.is_some() && !digits.is_empty() && digits.len() <= 2 => {
            (digits.parse::<i8>().ok()?, 0)
        }
        _ => return None,
    };
    let hour = match half {
        Some(_) if !(1..=12).contains(&hour) => return None,
        Some(false) => hour % 12,
        Some(true) => hour % 12 + 12,
        None => hour,
    };
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return None;
    }
    Some(((hour, minute), rest.trim()))
}

pub(crate) fn answer(query: &str, now: i64) -> Option<Answer> {
    let text = simplify(query);
    let now = Timestamp::from_second(now).ok()?;
    if let Some(answer) = asked_place(&text).and_then(|(name, _)| time_in(name, now)) {
        return Some(answer);
    }
    sides(&text)
        .into_iter()
        .find_map(|(from, to)| time_from(from, to, now))
}

fn time_in(name: &str, now: Timestamp) -> Option<Answer> {
    let place = place(name, now)?;
    Some(time_answer(&place.shown, now.to_zoned(place.zone)))
}

/// The town a question about the time surely asks about ("time in
/// boulder", "what time is it in hobart", not "boulder time"), for a
/// caller that can look up towns [`crate::answer`] does not know.
pub fn asked_town(query: &str) -> Option<String> {
    let text = simplify(query);
    let (name, surely) = asked_place(&text)?;
    let name = name.trim().trim_end_matches(" now");
    (surely && !name.is_empty()).then(|| name.to_string())
}

/// A town found by the caller, for [`time_in_town`].
pub struct Town<'a> {
    /// As the answer names it: "Boulder, CO".
    pub shown: &'a str,
    /// Its country's two-letter code.
    pub country: &'a str,
    /// Its state or region, as its address gives it.
    pub region: Option<&'a str>,
    pub lat: f64,
    pub lon: f64,
}

/// The time in `town`, when the clock it keeps can be told from its
/// country or state (see `zones.rs`).
pub fn time_in_town(town: &Town, now: i64) -> Option<Answer> {
    let now = Timestamp::from_second(now).ok()?;
    let zone = zones::town_zone(town.country, town.region, town.lat, town.lon, now)?;
    Some(time_answer(town.shown, now.to_zoned(zone)))
}

/// "Time in Tokyo, Japan: 12:45 PM".
fn time_answer(shown: &str, time: Zoned) -> Answer {
    Answer {
        kind: Kind::Time,
        question: format!("Time in {shown}"),
        answer: clock(&time),
        note: Some(format!(
            "{} · {}",
            time.strftime("%A, %B %-d, %Y"),
            zone_text(&time)
        )),
    }
}

/// "3pm est to pst": the time `from` (a time and a place) in the place `to`.
fn time_from(from: &str, to: &str, now: Timestamp) -> Option<Answer> {
    let ((hour, minute), from_place) = time_of_day(from)?;
    let from_place = place(from_place, now)?;
    let to_place = place(to, now)?;
    let today = now.to_zoned(from_place.zone.clone()).date();
    let start = today
        .at(hour, minute, 0, 0)
        .to_zoned(from_place.zone)
        .ok()?;
    let end = start.with_time_zone(to_place.zone);
    let day = match (end.date() - start.date()).get_days() {
        0 => String::new(),
        1 => " (next day)".to_string(),
        -1 => " (day before)".to_string(),
        _ => return None,
    };
    Some(Answer {
        kind: Kind::Time,
        question: format!("{} in {} =", clock(&start), from_place.shown),
        answer: format!("{} in {}{day}", clock(&end), to_place.shown),
        note: Some(format!("{} → {}", zone_text(&start), zone_text(&end))),
    })
}

#[cfg(test)]
mod tests {
    use super::answer;

    /// 2026-10-05 03:45:00 UTC, a Monday.
    const NOW: i64 = 1_791_171_900;

    #[test]
    fn tells_the_time_in_a_place() {
        let a = answer("time in tokyo", NOW).unwrap();
        assert_eq!(a.question, "Time in Tokyo, Japan");
        assert_eq!(a.answer, "12:45 PM");
        assert_eq!(a.note.unwrap(), "Monday, October 5, 2026 · JST, UTC+9");
        let a = answer("What time is it in New York?", NOW).unwrap();
        assert_eq!(a.answer, "11:45 PM");
        assert_eq!(a.note.unwrap(), "Sunday, October 4, 2026 · EDT, UTC-4");
        assert_eq!(answer("london time", NOW).unwrap().answer, "4:45 AM");
        assert_eq!(
            answer("time in mumbai", NOW).unwrap().note.unwrap(),
            "Monday, October 5, 2026 · IST, UTC+5:30"
        );
        assert_eq!(answer("time in utc+3", NOW).unwrap().answer, "6:45 AM");
        assert_eq!(answer("time in utc-14", NOW).unwrap().answer, "1:45 PM");
        assert_eq!(
            answer("time in europe/berlin", NOW).unwrap().question,
            "Time in Europe/Berlin"
        );
        assert_eq!(
            answer("time in utc", NOW).unwrap().note.unwrap(),
            "Monday, October 5, 2026 · UTC"
        );
        assert_eq!(
            answer("time in paris, france", NOW).unwrap().answer,
            "5:45 AM"
        );
    }

    #[test]
    fn a_place_and_where_it_is_must_agree() {
        let shown = |query: &str| answer(query, NOW).map(|a| a.question);
        // Newfoundland keeps its own half hour.
        let a = answer("time in St. John's Newfoundland", NOW).unwrap();
        assert_eq!(a.question, "Time in St. John's, Canada");
        assert_eq!(a.note.unwrap(), "Monday, October 5, 2026 · NDT, UTC-2:30");
        assert_eq!(
            answer("time in lord howe island", NOW)
                .unwrap()
                .note
                .unwrap(),
            "Monday, October 5, 2026 · UTC+11"
        );
        assert_eq!(
            answer("time in chatham islands", NOW).unwrap().answer,
            "5:30 PM"
        );
        assert_eq!(
            shown("time in kansas city missouri").as_deref(),
            Some("Time in Kansas City, US")
        );
        // A qualifier the list does not know leaves the place as it was.
        assert_eq!(
            shown("time in sydney, nsw").as_deref(),
            Some("Time in Sydney, Australia")
        );
        // Paris, Texas is not Paris, France; nor London, Ontario London.
        for query in [
            "time in paris, texas",
            "time in paris texas",
            "time in london, ontario",
            "time in perth, scotland",
            "time in portland maine",
        ] {
            assert_eq!(shown(query), None, "{query:?}");
        }
    }

    #[test]
    fn a_town_the_caller_finds() {
        assert_eq!(
            super::asked_town("time in boulder").as_deref(),
            Some("boulder")
        );
        assert_eq!(
            super::asked_town("What time is it in Hobart now?").as_deref(),
            Some("hobart")
        );
        assert_eq!(
            super::asked_town("boulder time zone").as_deref(),
            Some("boulder")
        );
        // "boulder time" may be no question about the time.
        assert_eq!(super::asked_town("boulder time"), None);
        assert_eq!(super::asked_town("game time"), None);
        let town = super::Town {
            shown: "Boulder, CO",
            country: "US",
            region: Some("CO"),
            lat: 40.01,
            lon: -105.27,
        };
        let a = super::time_in_town(&town, NOW).unwrap();
        assert_eq!(a.question, "Time in Boulder, CO");
        assert_eq!(a.answer, "9:45 PM");
        assert_eq!(a.note.unwrap(), "Sunday, October 4, 2026 · MDT, UTC-6");
        let el_paso = super::Town {
            region: Some("TX"),
            ..town
        };
        assert_eq!(super::time_in_town(&el_paso, NOW), None);
    }

    #[test]
    fn moves_a_time_between_places() {
        let a = answer("3pm est to pst", NOW).unwrap();
        assert_eq!(a.question, "3:00 PM in Eastern Time (US) =");
        assert_eq!(a.answer, "12:00 PM in Pacific Time (US)");
        assert_eq!(a.note.unwrap(), "EDT, UTC-4 → PDT, UTC-7");
        let a = answer("15:00 london in tokyo", NOW).unwrap();
        assert_eq!(a.answer, "11:00 PM in Tokyo, Japan");
        let a = answer("9pm pacific to london", NOW).unwrap();
        assert_eq!(a.answer, "5:00 AM in London, United Kingdom (next day)");
    }

    #[test]
    fn leaves_searches_alone() {
        for query in [
            "time",
            "the time machine",
            "game time",
            "time in a bottle",
            "prime time",
            "showtime",
            "3 in to cm",
            "time out london",
            "times new roman",
            "time in utc+-2000000",
            "time in utc-+3",
            "time in utc+15",
            "time in utc+5:60",
            "time in gmt+99999999999",
        ] {
            assert_eq!(answer(query, NOW), None, "{query:?}");
        }
    }
}
