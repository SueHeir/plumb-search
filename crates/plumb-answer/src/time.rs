//! The time in a place: `time in tokyo`, `what time is it in paris`,
//! `london time`; and a time somewhere in another place: `3pm est to pst`,
//! `15:00 london in tokyo`.

use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

use crate::units::sides;
use crate::{places, simplify, Answer, Kind};

/// A place's name as shown and its time zone.
struct Place {
    shown: String,
    zone: TimeZone,
}

/// The place `name` (simplified) names: a known place or zone name, an
/// IANA zone (`europe/berlin`), or an offset (`utc+5:30`).
fn place(name: &str) -> Option<Place> {
    let name = name
        .trim()
        .trim_start_matches("the ")
        .trim_end_matches(" now");
    if name.is_empty() {
        return None;
    }
    if let Some((shown, zone)) = places::find(name) {
        return Some(Place {
            shown: shown.to_string(),
            zone: TimeZone::get(zone).ok()?,
        });
    }
    // "Paris, France": the part before the comma.
    if let Some((first, _)) = name.split_once(',') {
        if let Some((shown, zone)) = places::find(first.trim()) {
            return Some(Place {
                shown: shown.to_string(),
                zone: TimeZone::get(zone).ok()?,
            });
        }
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
            let (hours, minutes) = match rest.split_once(':') {
                Some((h, m)) => (h.trim().parse::<i32>().ok()?, m.trim().parse::<i32>().ok()?),
                None => (rest.trim().parse::<i32>().ok()?, 0),
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

/// The place a question about the time now is about, if `text` asks one.
fn asked_place(text: &str) -> Option<&str> {
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
            return Some(place);
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
            return Some(place);
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
    if let Some(answer) = asked_place(&text).and_then(|name| time_in(name, now)) {
        return Some(answer);
    }
    sides(&text)
        .into_iter()
        .find_map(|(from, to)| time_from(from, to, now))
}

fn time_in(name: &str, now: Timestamp) -> Option<Answer> {
    let place = place(name)?;
    let time = now.to_zoned(place.zone);
    Some(Answer {
        kind: Kind::Time,
        question: format!("Time in {}", place.shown),
        answer: clock(&time),
        note: Some(format!(
            "{} · {}",
            time.strftime("%A, %B %-d, %Y"),
            zone_text(&time)
        )),
    })
}

/// "3pm est to pst": the time `from` (a time and a place) in the place `to`.
fn time_from(from: &str, to: &str, now: Timestamp) -> Option<Answer> {
    let ((hour, minute), from_place) = time_of_day(from)?;
    let from_place = place(from_place)?;
    let to_place = place(to)?;
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
        ] {
            assert_eq!(answer(query, NOW), None, "{query:?}");
        }
    }
}
