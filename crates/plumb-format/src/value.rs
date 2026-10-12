//! Values: the nine field types a manifest can declare, and the formats of
//! dates, places, languages and text that every item uses.
//!
//! - **Dates** are ISO 8601 in UTC: `2024`, `2024-05`, `2024-05-17` or
//!   `2024-05-17T09:30:00Z`, as precise as is known. A range is
//!   `{"from": ..., "to": ...}`.
//! - **Places** name their axes: a point is `{"lat": ..., "lon": ...}`, a
//!   box `{"s": ..., "w": ..., "n": ..., "e": ...}`.
//! - **Text** is UTF-8 in Unicode NFC, with no control characters but
//!   newlines.
//! - **Languages** are BCP 47 tags (`en`, `pt-BR`).
//!
//! Any field value may be wrapped to say when and where it is true:
//! `{"v": 31000000, "as_of": "2024", "src": "wd:Q408", "lang": "en"}`.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use unicode_normalization::{is_nfc, UnicodeNormalization};

use crate::refs::Ref;
use crate::urls::is_canonical;

/// The type of a set's field, declared once in its manifest. A closed
/// list: a new type goes in a manifest's `requires`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    /// Words that are read: searched or shown.
    Text,
    /// One exact term: a genre, a license, a status.
    Keyword,
    /// A number, with the field's unit (a UCUM code or ISO 4217 currency).
    Number,
    Bool,
    /// A date, or a range of dates.
    Date,
    /// A point or a box.
    Geo,
    /// A canonical http or https URL.
    Url,
    /// Another thing, by [`Ref`].
    Ref,
    /// One level of named values of the other types, such as a docs symbol
    /// `{"name": ..., "kind": ..., "anchor": ...}`. Values are strings,
    /// numbers, bools or lists of them, never another object.
    Object,
}

impl FieldType {
    /// Checks one value of this type, already unwrapped
    /// ([`unwrapped`]).
    pub fn check(self, value: &Value) -> Result<()> {
        match self {
            FieldType::Text => check_text(as_str(value)?),
            FieldType::Keyword => {
                let text = as_str(value)?;
                ensure!(
                    !text.is_empty() && !text.contains('\n'),
                    "{text:?} is not a keyword"
                );
                check_text(text)
            }
            FieldType::Number => {
                ensure!(value.is_number(), "{value} is not a number");
                Ok(())
            }
            FieldType::Bool => {
                ensure!(value.is_boolean(), "{value} is not true or false");
                Ok(())
            }
            FieldType::Date => match value {
                Value::String(text) => {
                    ensure!(is_date(text), "{text:?} is not a date");
                    Ok(())
                }
                Value::Object(range) => {
                    only_keys(range, &["from", "to"])?;
                    for key in ["from", "to"] {
                        let end = range.get(key).and_then(Value::as_str);
                        ensure!(
                            end.is_some_and(is_date),
                            "date range {value} has no {key} date"
                        );
                    }
                    Ok(())
                }
                _ => bail!("{value} is not a date"),
            },
            FieldType::Geo => check_geo(value),
            FieldType::Url => {
                let url = as_str(value)?;
                ensure!(is_canonical(url), "{url:?} is not a canonical URL");
                Ok(())
            }
            FieldType::Ref => {
                as_str(value)?.parse::<Ref>()?;
                Ok(())
            }
            FieldType::Object => {
                let members = value
                    .as_object()
                    .with_context(|| format!("{value} is not an object"))?;
                for (name, member) in members {
                    let scalar = |v: &Value| v.is_string() || v.is_number() || v.is_boolean();
                    let ok = scalar(member)
                        || member
                            .as_array()
                            .is_some_and(|list| list.iter().all(scalar));
                    ensure!(
                        ok,
                        "{name:?} in {value} is not a string, number, bool or list"
                    );
                    if let Some(text) = member.as_str() {
                        check_text(text)?;
                    }
                }
                Ok(())
            }
        }
    }

    /// Checks a field's whole value: each element of a list when `list`,
    /// each one optionally wrapped.
    pub fn check_field(self, list: bool, value: &Value) -> Result<()> {
        let check_one = |one: &Value| self.check(unwrapped(one, self)?);
        if list {
            let values = value
                .as_array()
                .with_context(|| format!("{value} is not a list"))?;
            values.iter().try_for_each(check_one)
        } else {
            check_one(value)
        }
    }
}

/// The value inside a `{"v": ..., "as_of": ..., "src": ..., "lang": ...}`
/// wrapper, checking the wrapper's own keys, or `value` itself when it is
/// not wrapped. For an object field, a value is a wrapper only when it has
/// `v` and no keys but the wrapper's.
pub fn unwrapped(value: &Value, field_type: FieldType) -> Result<&Value> {
    let Some(map) = value.as_object() else {
        return Ok(value);
    };
    let Some(inner) = map.get("v") else {
        return Ok(value);
    };
    let wrapper_keys = ["v", "as_of", "src", "lang"];
    if field_type == FieldType::Object && !map.keys().all(|k| wrapper_keys.contains(&k.as_str())) {
        return Ok(value);
    }
    only_keys(map, &wrapper_keys)?;
    if let Some(as_of) = map.get("as_of") {
        ensure!(
            as_of.as_str().is_some_and(is_date),
            "as_of {as_of} is not a date"
        );
    }
    if let Some(src) = map.get("src") {
        as_str(src)?.parse::<Ref>()?;
    }
    if let Some(lang) = map.get("lang") {
        ensure!(
            as_str(lang).is_ok_and(is_language),
            "lang {lang} is not a language tag"
        );
    }
    Ok(inner)
}

/// True for `YYYY`, `YYYY-MM`, `YYYY-MM-DD` and `YYYY-MM-DDTHH:MM:SSZ`.
pub fn is_date(text: &str) -> bool {
    let (day, time) = match text.split_once('T') {
        Some((day, time)) => (day, Some(time)),
        None => (text, None),
    };
    let mut parts = day.split('-');
    let year = parts.next().and_then(|y| digits(y, 4));
    let month = parts.next().map(|m| digits(m, 2));
    let date = parts.next().map(|d| digits(d, 2));
    if parts.next().is_some() || year.is_none() {
        return false;
    }
    let year = year.unwrap_or_default();
    let ok_day = match (month, date) {
        (None, None) => time.is_none(),
        (Some(Some(m)), None) => (1..=12).contains(&m) && time.is_none(),
        (Some(Some(m)), Some(Some(d))) => {
            (1..=12).contains(&m) && (1..=days_in(year, m)).contains(&d)
        }
        _ => false,
    };
    ok_day && time.is_none_or(is_time)
}

/// True for a full UTC timestamp, `YYYY-MM-DDTHH:MM:SSZ`: a manifest's
/// generation.
pub fn is_timestamp(text: &str) -> bool {
    text.len() == 20 && is_date(text)
}

/// Seconds since 1970 of a full UTC timestamp ([`is_timestamp`]), or
/// `None` for anything else.
pub fn unix_seconds(timestamp: &str) -> Option<i64> {
    if !is_timestamp(timestamp) {
        return None;
    }
    let number = |range: std::ops::Range<usize>| timestamp[range].parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    // Days from 1970-01-01 to the date, in the proleptic Gregorian calendar.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year - era * 400;
    let of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let of_era_days = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    let days = era * 146_097 + of_era_days - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// True for a BCP 47 language tag, loosely: a 2 or 3 letter language, then
/// subtags of 1 to 8 letters and digits (`en`, `pt-BR`, `zh-Hant-TW`).
pub fn is_language(text: &str) -> bool {
    let mut subtags = text.split('-');
    let first = subtags.next().unwrap_or_default();
    (2..=3).contains(&first.len())
        && first.bytes().all(|b| b.is_ascii_alphabetic())
        && subtags
            .all(|s| (1..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Checks text as items carry it: Unicode NFC, and no control characters
/// but newlines.
pub fn check_text(text: &str) -> Result<()> {
    ensure!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "{text:?} has control characters"
    );
    ensure!(is_nfc(text), "{text:?} is not in Unicode NFC");
    Ok(())
}

/// `text` made fit for an item: in Unicode NFC, with every control
/// character but a newline made a space.
pub fn normalize_text(text: &str) -> String {
    text.nfc()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect()
}

fn check_geo(value: &Value) -> Result<()> {
    let map = value
        .as_object()
        .with_context(|| format!("{value} is not a place"))?;
    let coordinate = |key: &str, limit: f64| -> Result<()> {
        let n = map.get(key).and_then(Value::as_f64);
        ensure!(
            n.is_some_and(|n| (-limit..=limit).contains(&n)),
            "{value} needs {key} within ±{limit}"
        );
        Ok(())
    };
    if map.contains_key("lat") {
        only_keys(map, &["lat", "lon"])?;
        coordinate("lat", 90.0)?;
        coordinate("lon", 180.0)
    } else {
        only_keys(map, &["s", "w", "n", "e"])?;
        coordinate("s", 90.0)?;
        coordinate("n", 90.0)?;
        coordinate("w", 180.0)?;
        coordinate("e", 180.0)
    }
}

fn only_keys(map: &Map<String, Value>, keys: &[&str]) -> Result<()> {
    if let Some(other) = map.keys().find(|k| !keys.contains(&k.as_str())) {
        bail!("unexpected key {other:?}, only {keys:?}");
    }
    Ok(())
}

fn as_str(value: &Value) -> Result<&str> {
    value
        .as_str()
        .with_context(|| format!("{value} is not a string"))
}

fn digits(text: &str, width: usize) -> Option<u32> {
    (text.len() == width && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn is_time(text: &str) -> bool {
    let Some(clock) = text.strip_suffix('Z') else {
        return false;
    };
    let mut parts = clock.split(':');
    let (h, m, s) = (parts.next(), parts.next(), parts.next());
    parts.next().is_none()
        && h.and_then(|h| digits(h, 2)).is_some_and(|h| h < 24)
        && m.and_then(|m| digits(m, 2)).is_some_and(|m| m < 60)
        && s.and_then(|s| digits(s, 2)).is_some_and(|s| s < 60)
}

fn days_in(year: u32, month: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unix_seconds_of_timestamps() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(unix_seconds("2026-10-12T06:00:00Z"), Some(1_791_784_800));
        assert_eq!(unix_seconds("2024-02-29T23:59:59Z"), Some(1_709_251_199));
        assert_eq!(unix_seconds("2026-10-12"), None);
        assert_eq!(unix_seconds("2026-02-30T00:00:00Z"), None);
    }

    #[test]
    fn dates() {
        for good in [
            "2024",
            "2024-02",
            "2024-02-29",
            "2024-05-17T09:30:00Z",
            "0800",
        ] {
            assert!(is_date(good), "{good}");
        }
        for bad in [
            "24",
            "2024-2",
            "2023-02-29",
            "2024-13",
            "2024-05-17T09:30:00",
            "2024-05-17T24:00:00Z",
            "2024T09:30:00Z",
            "2024-05-17T09:30:00+01:00",
            "May 2024",
        ] {
            assert!(!is_date(bad), "{bad}");
        }
        assert!(is_timestamp("2026-10-11T05:00:00Z"));
        assert!(!is_timestamp("2026-10-11"));
    }

    #[test]
    fn field_types() {
        use FieldType::*;
        let good = [
            (Text, json!("Canberra")),
            (Keyword, json!("rock")),
            (Number, json!(31.5)),
            (Bool, json!(true)),
            (Date, json!("1913-03-12")),
            (Date, json!({"from": "2008", "to": "2013"})),
            (Geo, json!({"lat": -35.3, "lon": 149.1})),
            (Geo, json!({"s": -44.0, "w": 112.0, "n": -10.0, "e": 154.0})),
            (Url, json!("https://www.canberra.act.gov.au/")),
            (Ref, json!("wd:Q3114")),
            (
                Object,
                json!({"name": "load", "kind": "function", "anchor": "#load", "tags": ["io"]}),
            ),
        ];
        for (field_type, value) in good {
            field_type.check(&value).unwrap();
        }
        let bad = [
            (Text, json!(3)),
            (Text, json!("e\u{301}")),
            (Text, json!("a\u{7}b")),
            (Keyword, json!("")),
            (Number, json!("3")),
            (Date, json!("yesterday")),
            (Geo, json!({"lat": 91.0, "lon": 0.0})),
            (Geo, json!([149.1, -35.3])),
            (Url, json!("https://Example.com/")),
            (Ref, json!("Q3114")),
            (Object, json!({"inner": {"no": "nesting"}})),
        ];
        for (field_type, value) in bad {
            assert!(field_type.check(&value).is_err(), "{field_type:?} {value}");
        }
    }

    #[test]
    fn wrapped_values() {
        let population = json!({"v": 26000000, "as_of": "2021", "src": "wd:Q408"});
        FieldType::Number.check_field(false, &population).unwrap();
        let names = json!([{"v": "オーストラリア", "lang": "ja"}, "Australia"]);
        FieldType::Text.check_field(true, &names).unwrap();
        assert!(FieldType::Number
            .check_field(false, &json!({"v": 1, "as_of": "soon"}))
            .is_err());
        assert!(FieldType::Number
            .check_field(false, &json!({"v": 1, "extra": 2}))
            .is_err());
        // An object field whose value has its own keys is not a wrapper.
        FieldType::Object
            .check_field(false, &json!({"v": "3.1", "name": "release"}))
            .unwrap();
    }

    #[test]
    fn normalized_text_passes() {
        let text = normalize_text("Cafe\u{301}\tbar\nbaz\u{7}");
        assert_eq!(text, "Caf\u{e9} bar\nbaz ");
        check_text(&text).unwrap();
        assert!(check_text("Cafe\u{301}").is_err());
    }
}
