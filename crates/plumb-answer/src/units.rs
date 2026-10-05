//! Unit conversions: `10 km in miles`, `100 f to c`, `5 ft 2 in to cm` is
//! not understood but `62 inches to cm` is, `how many feet in a mile`.

use crate::format::format_number;
use crate::{Answer, Kind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dimension {
    Length,
    Mass,
    Volume,
    Temperature,
    Area,
    Speed,
    Time,
    Data,
    Energy,
    Pressure,
    Power,
}

/// A unit: `value * factor + offset` is the value in the dimension's base
/// unit (metres, kilograms, litres, kelvins, ...).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Unit {
    names: &'static [&'static str],
    one: &'static str,
    many: &'static str,
    dimension: Dimension,
    factor: f64,
    offset: f64,
}

const fn unit(
    names: &'static [&'static str],
    one: &'static str,
    many: &'static str,
    dimension: Dimension,
    factor: f64,
) -> Unit {
    Unit {
        names,
        one,
        many,
        dimension,
        factor,
        offset: 0.0,
    }
}

use Dimension::*;

const INCH: f64 = 0.0254;
const FOOT: f64 = 0.3048;
const POUND: f64 = 0.453_592_37;
const US_GALLON: f64 = 3.785_411_784;
const IMPERIAL_GALLON: f64 = 4.546_09;

static UNITS: &[Unit] = &[
    // Length, in metres.
    unit(&["m", "meter", "metre"], "metre", "metres", Length, 1.0),
    unit(
        &["km", "kilometer", "kilometre", "kms"],
        "kilometre",
        "kilometres",
        Length,
        1000.0,
    ),
    unit(
        &["cm", "centimeter", "centimetre"],
        "centimetre",
        "centimetres",
        Length,
        0.01,
    ),
    unit(
        &["mm", "millimeter", "millimetre"],
        "millimetre",
        "millimetres",
        Length,
        0.001,
    ),
    unit(
        &["µm", "um", "micrometer", "micrometre", "micron"],
        "micrometre",
        "micrometres",
        Length,
        1e-6,
    ),
    unit(
        &["nm", "nanometer", "nanometre"],
        "nanometre",
        "nanometres",
        Length,
        1e-9,
    ),
    unit(&["mi", "mile"], "mile", "miles", Length, 1609.344),
    unit(&["yd", "yard"], "yard", "yards", Length, 0.9144),
    unit(&["ft", "foot", "feet", "'"], "foot", "feet", Length, FOOT),
    unit(
        &["in", "inch", "inches", "\""],
        "inch",
        "inches",
        Length,
        INCH,
    ),
    unit(
        &["nmi", "nautical mile"],
        "nautical mile",
        "nautical miles",
        Length,
        1852.0,
    ),
    unit(
        &["ly", "light year", "lightyear"],
        "light year",
        "light years",
        Length,
        9.460_730_472_580_8e15,
    ),
    unit(
        &["au", "astronomical unit"],
        "astronomical unit",
        "astronomical units",
        Length,
        1.495_978_707e11,
    ),
    // Mass, in kilograms.
    unit(
        &["kg", "kilogram", "kilo", "kgs"],
        "kilogram",
        "kilograms",
        Mass,
        1.0,
    ),
    unit(&["g", "gram", "gramme"], "gram", "grams", Mass, 0.001),
    unit(&["mg", "milligram"], "milligram", "milligrams", Mass, 1e-6),
    unit(&["gr", "grain"], "grain", "grains", Mass, 64.798_91e-6),
    unit(
        &["µg", "ug", "mcg", "microgram"],
        "microgram",
        "micrograms",
        Mass,
        1e-9,
    ),
    unit(
        &["t", "tonne", "metric ton", "metric tonne"],
        "tonne",
        "tonnes",
        Mass,
        1000.0,
    ),
    unit(&["lb", "lbs", "pound"], "pound", "pounds", Mass, POUND),
    unit(&["oz", "ounce"], "ounce", "ounces", Mass, POUND / 16.0),
    unit(&["st", "stone"], "stone", "stone", Mass, POUND * 14.0),
    unit(
        &["ton", "short ton", "us ton"],
        "short ton",
        "short tons",
        Mass,
        POUND * 2000.0,
    ),
    unit(
        &["long ton", "uk ton", "imperial ton"],
        "long ton",
        "long tons",
        Mass,
        POUND * 2240.0,
    ),
    // Volume, in litres.
    unit(
        &["l", "liter", "litre", "ltr"],
        "litre",
        "litres",
        Volume,
        1.0,
    ),
    unit(
        &["ml", "milliliter", "millilitre", "cc"],
        "millilitre",
        "millilitres",
        Volume,
        0.001,
    ),
    unit(
        &["cl", "centiliter", "centilitre"],
        "centilitre",
        "centilitres",
        Volume,
        0.01,
    ),
    unit(
        &["dl", "deciliter", "decilitre"],
        "decilitre",
        "decilitres",
        Volume,
        0.1,
    ),
    unit(
        &["m3", "m^3", "cubic meter", "cubic metre"],
        "cubic metre",
        "cubic metres",
        Volume,
        1000.0,
    ),
    unit(
        &["cm3", "cm^3", "cubic centimeter", "cubic centimetre"],
        "cubic centimetre",
        "cubic centimetres",
        Volume,
        0.001,
    ),
    unit(
        &["ft3", "cu ft", "cubic foot", "cubic feet"],
        "cubic foot",
        "cubic feet",
        Volume,
        FOOT * FOOT * FOOT * 1000.0,
    ),
    unit(
        &["in3", "cu in", "cubic inch", "cubic inches"],
        "cubic inch",
        "cubic inches",
        Volume,
        INCH * INCH * INCH * 1000.0,
    ),
    unit(
        &["gal", "gallon", "us gallon"],
        "US gallon",
        "US gallons",
        Volume,
        US_GALLON,
    ),
    unit(
        &["imperial gallon", "uk gallon", "imp gal"],
        "imperial gallon",
        "imperial gallons",
        Volume,
        IMPERIAL_GALLON,
    ),
    unit(
        &["qt", "quart", "us quart"],
        "US quart",
        "US quarts",
        Volume,
        US_GALLON / 4.0,
    ),
    unit(
        &["pt", "pint", "us pint"],
        "US pint",
        "US pints",
        Volume,
        US_GALLON / 8.0,
    ),
    unit(
        &["imperial pint", "uk pint"],
        "imperial pint",
        "imperial pints",
        Volume,
        IMPERIAL_GALLON / 8.0,
    ),
    unit(
        &["cup", "us cup"],
        "US cup",
        "US cups",
        Volume,
        US_GALLON / 16.0,
    ),
    unit(
        &["fl oz", "floz", "fluid ounce", "us fluid ounce"],
        "US fluid ounce",
        "US fluid ounces",
        Volume,
        US_GALLON / 128.0,
    ),
    unit(
        &["tbsp", "tablespoon"],
        "tablespoon",
        "tablespoons",
        Volume,
        US_GALLON / 256.0,
    ),
    unit(
        &["tsp", "teaspoon"],
        "teaspoon",
        "teaspoons",
        Volume,
        US_GALLON / 768.0,
    ),
    // Temperature, in kelvins.
    Unit {
        names: &[
            "c",
            "°c",
            "celsius",
            "centigrade",
            "degree celsius",
            "degrees celsius",
            "degrees c",
            "deg c",
        ],
        one: "degree Celsius",
        many: "degrees Celsius",
        dimension: Temperature,
        factor: 1.0,
        offset: 273.15,
    },
    Unit {
        names: &[
            "f",
            "°f",
            "fahrenheit",
            "degree fahrenheit",
            "degrees fahrenheit",
            "degrees f",
            "deg f",
        ],
        one: "degree Fahrenheit",
        many: "degrees Fahrenheit",
        dimension: Temperature,
        factor: 5.0 / 9.0,
        offset: 459.67 * 5.0 / 9.0,
    },
    unit(
        &["k", "kelvin", "kelvins"],
        "kelvin",
        "kelvins",
        Temperature,
        1.0,
    ),
    // Area, in square metres.
    unit(
        &["m2", "m^2", "sq m", "square meter", "square metre", "sqm"],
        "square metre",
        "square metres",
        Area,
        1.0,
    ),
    unit(
        &[
            "km2",
            "km^2",
            "sq km",
            "square kilometer",
            "square kilometre",
        ],
        "square kilometre",
        "square kilometres",
        Area,
        1e6,
    ),
    unit(
        &[
            "cm2",
            "cm^2",
            "sq cm",
            "square centimeter",
            "square centimetre",
        ],
        "square centimetre",
        "square centimetres",
        Area,
        1e-4,
    ),
    unit(&["ha", "hectare"], "hectare", "hectares", Area, 1e4),
    unit(&["acre", "ac"], "acre", "acres", Area, 4_046.856_422_4),
    unit(
        &["ft2", "ft^2", "sq ft", "square foot", "square feet", "sqft"],
        "square foot",
        "square feet",
        Area,
        FOOT * FOOT,
    ),
    unit(
        &["in2", "in^2", "sq in", "square inch", "square inches"],
        "square inch",
        "square inches",
        Area,
        INCH * INCH,
    ),
    unit(
        &["mi2", "mi^2", "sq mi", "square mile"],
        "square mile",
        "square miles",
        Area,
        1609.344 * 1609.344,
    ),
    unit(
        &["yd2", "sq yd", "square yard"],
        "square yard",
        "square yards",
        Area,
        0.9144 * 0.9144,
    ),
    // Speed, in metres a second.
    unit(
        &[
            "m/s",
            "mps",
            "meters per second",
            "metres per second",
            "meter per second",
            "metre per second",
        ],
        "metre per second",
        "metres per second",
        Speed,
        1.0,
    ),
    unit(
        &[
            "km/h",
            "kmh",
            "kph",
            "kmph",
            "km per hour",
            "kilometers per hour",
            "kilometres per hour",
            "kilometer per hour",
            "kilometre per hour",
        ],
        "kilometre per hour",
        "kilometres per hour",
        Speed,
        1000.0 / 3600.0,
    ),
    unit(
        &["mph", "mi/h", "miles per hour", "mile per hour"],
        "mile per hour",
        "miles per hour",
        Speed,
        1609.344 / 3600.0,
    ),
    unit(
        &["kn", "kt", "knot", "knots"],
        "knot",
        "knots",
        Speed,
        1852.0 / 3600.0,
    ),
    unit(
        &["ft/s", "fps", "feet per second", "foot per second"],
        "foot per second",
        "feet per second",
        Speed,
        FOOT,
    ),
    // Time, in seconds.
    unit(
        &["s", "sec", "secs", "second"],
        "second",
        "seconds",
        Time,
        1.0,
    ),
    unit(
        &["ms", "millisecond"],
        "millisecond",
        "milliseconds",
        Time,
        0.001,
    ),
    unit(&["min", "mins", "minute"], "minute", "minutes", Time, 60.0),
    unit(&["h", "hr", "hrs", "hour"], "hour", "hours", Time, 3600.0),
    unit(&["d", "day"], "day", "days", Time, 86_400.0),
    unit(&["wk", "week"], "week", "weeks", Time, 604_800.0),
    unit(
        &["month"],
        "month",
        "months",
        Time,
        86_400.0 * 365.2425 / 12.0,
    ),
    unit(&["yr", "year"], "year", "years", Time, 86_400.0 * 365.2425),
    unit(&["decade"], "decade", "decades", Time, 86_400.0 * 3652.425),
    unit(
        &["century", "centuries"],
        "century",
        "centuries",
        Time,
        86_400.0 * 36_524.25,
    ),
    // Data, in bytes.
    unit(&["b", "byte"], "byte", "bytes", Data, 1.0),
    unit(&["bit"], "bit", "bits", Data, 0.125),
    unit(&["kb", "kilobyte"], "kilobyte", "kilobytes", Data, 1e3),
    unit(&["mb", "megabyte"], "megabyte", "megabytes", Data, 1e6),
    unit(
        &["gb", "gigabyte", "gig"],
        "gigabyte",
        "gigabytes",
        Data,
        1e9,
    ),
    unit(&["tb", "terabyte"], "terabyte", "terabytes", Data, 1e12),
    unit(&["pb", "petabyte"], "petabyte", "petabytes", Data, 1e15),
    unit(&["kib", "kibibyte"], "kibibyte", "kibibytes", Data, 1024.0),
    unit(
        &["mib", "mebibyte"],
        "mebibyte",
        "mebibytes",
        Data,
        1_048_576.0,
    ),
    unit(
        &["gib", "gibibyte"],
        "gibibyte",
        "gibibytes",
        Data,
        1_073_741_824.0,
    ),
    unit(
        &["tib", "tebibyte"],
        "tebibyte",
        "tebibytes",
        Data,
        1_099_511_627_776.0,
    ),
    unit(&["kbit", "kilobit"], "kilobit", "kilobits", Data, 125.0),
    unit(&["mbit", "megabit"], "megabit", "megabits", Data, 125_000.0),
    unit(
        &["gbit", "gigabit"],
        "gigabit",
        "gigabits",
        Data,
        125_000_000.0,
    ),
    unit(
        &["tbit", "terabit"],
        "terabit",
        "terabits",
        Data,
        125_000_000_000.0,
    ),
    // Energy, in joules.
    unit(&["j", "joule"], "joule", "joules", Energy, 1.0),
    unit(
        &["kj", "kilojoule"],
        "kilojoule",
        "kilojoules",
        Energy,
        1000.0,
    ),
    unit(
        &["cal", "calorie", "small calorie"],
        "calorie",
        "calories",
        Energy,
        4.184,
    ),
    unit(
        &["kcal", "kilocalorie", "food calorie"],
        "kilocalorie",
        "kilocalories",
        Energy,
        4184.0,
    ),
    unit(
        &["wh", "watt hour", "watt-hour"],
        "watt-hour",
        "watt-hours",
        Energy,
        3600.0,
    ),
    unit(
        &["kwh", "kilowatt hour", "kilowatt-hour"],
        "kilowatt-hour",
        "kilowatt-hours",
        Energy,
        3.6e6,
    ),
    unit(&["btu"], "BTU", "BTUs", Energy, 1_055.055_852_62),
    unit(
        &["ev", "electronvolt"],
        "electronvolt",
        "electronvolts",
        Energy,
        1.602_176_634e-19,
    ),
    // Pressure, in pascals.
    unit(&["pa", "pascal"], "pascal", "pascals", Pressure, 1.0),
    unit(
        &["kpa", "kilopascal"],
        "kilopascal",
        "kilopascals",
        Pressure,
        1000.0,
    ),
    unit(
        &["hpa", "hectopascal", "mbar", "millibar"],
        "hectopascal",
        "hectopascals",
        Pressure,
        100.0,
    ),
    unit(&["bar"], "bar", "bars", Pressure, 1e5),
    unit(&["psi"], "psi", "psi", Pressure, 6_894.757_293_168),
    unit(
        &["atm", "atmosphere"],
        "atmosphere",
        "atmospheres",
        Pressure,
        101_325.0,
    ),
    unit(
        &["mmhg"],
        "millimetre of mercury",
        "millimetres of mercury",
        Pressure,
        133.322_387_415,
    ),
    unit(
        &["inhg"],
        "inch of mercury",
        "inches of mercury",
        Pressure,
        3386.389,
    ),
    // Power, in watts.
    unit(&["w", "watt"], "watt", "watts", Power, 1.0),
    unit(&["milliwatt"], "milliwatt", "milliwatts", Power, 0.001),
    unit(&["kw", "kilowatt"], "kilowatt", "kilowatts", Power, 1000.0),
    unit(&["mw", "megawatt"], "megawatt", "megawatts", Power, 1e6),
    unit(
        &["hp", "horsepower"],
        "horsepower",
        "horsepower",
        Power,
        745.699_871_582_270_2,
    ),
];

/// Symbols whose case decides the unit, as typed, with the name of the
/// unit they stand for: "mW" is a milliwatt where "MW" and "mw" are
/// megawatts, and "Gb" a gigabit where "GB" and "gb" are gigabytes. A
/// symbol in lowercase keeps its everyday meaning.
static CASED_SYMBOLS: &[(&str, &str)] = &[
    ("mW", "milliwatt"),
    ("Kb", "kilobit"),
    ("Mb", "megabit"),
    ("Gb", "gigabit"),
    ("Tb", "terabit"),
];

/// The unit `name` names, where `typed` is `name` as typed, before
/// lowercasing: a symbol whose case matters ([`CASED_SYMBOLS`]) is read
/// as typed, anything else by [`find`].
fn find_typed(name: &str, typed: &str) -> Option<&'static Unit> {
    let typed = typed.trim().trim_end_matches('.');
    CASED_SYMBOLS
        .iter()
        .find(|(symbol, _)| *symbol == typed)
        .and_then(|(_, unit)| find(unit))
        .or_else(|| find(name))
}

/// The unit named `name` (already simplified): its symbol, its name, or
/// its name in the plural.
pub(crate) fn find(name: &str) -> Option<&'static Unit> {
    let name = name.trim().trim_end_matches('.');
    let name = name
        .strip_prefix("degrees ")
        .filter(|rest| matches!(*rest, "c" | "f" | "celsius" | "fahrenheit"))
        .unwrap_or(name);
    let name = name.strip_prefix("°").unwrap_or(name);
    let lookup = |name: &str| {
        UNITS.iter().find(|u| {
            u.names.contains(&name)
                || u.many.eq_ignore_ascii_case(name)
                || u.one.eq_ignore_ascii_case(name)
        })
    };
    if let Some(unit) = lookup(name) {
        return Some(unit);
    }
    // "kilograms", "inches"; never a symbol ("ms" is not many "m").
    if name.chars().count() > 3 {
        for plural in ["es", "s"] {
            if let Some(one) = name.strip_suffix(plural) {
                if let Some(unit) = lookup(one).filter(|u| {
                    u.names.iter().any(|n| n.len() >= 3 && *n == one)
                        || u.one.eq_ignore_ascii_case(one)
                }) {
                    return Some(unit);
                }
            }
        }
    }
    None
}

/// The number at the start of `text` and what follows it: "10 km" -> (10,
/// "km"), "1.5kg" -> (1.5, "kg"), "a mile" -> (1, "mile"). Without a
/// number, 1.
pub(crate) fn amount(text: &str) -> Option<(f64, &str)> {
    let text = text.trim();
    for word in ["a ", "an ", "one "] {
        if let Some(rest) = text.strip_prefix(word) {
            return Some((1.0, rest.trim()));
        }
    }
    let end = text
        .char_indices()
        .find(|&(_, c)| !(c.is_ascii_digit() || c == '.' || c == ',' || c == '-' || c == '+'))
        .map_or(text.len(), |(i, _)| i);
    let number = &text[..end];
    let rest = text[end..].trim();
    if number.is_empty() {
        return Some((1.0, rest));
    }
    let value: f64 = number.replace(',', "").parse().ok()?;
    Some((value, rest))
}

/// The number written for an answer: up to 6 significant digits.
fn shown(value: f64) -> Option<String> {
    format_number(value, 6)
}

fn named(value: f64, unit: &Unit) -> Option<String> {
    let number = shown(value)?;
    let name = if number == "1" { unit.one } else { unit.many };
    Some(format!("{number} {name}"))
}

/// `value` of `from` in `to`.
fn convert(value: f64, from: &Unit, to: &Unit) -> f64 {
    let base = value * from.factor + from.offset;
    (base - to.offset) / to.factor
}

/// Splits `text` at a word joining two sides of a conversion, trying each
/// place: "10 in in cm" is ("10 in", "cm").
pub(crate) fn sides(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    for joiner in [
        " to ",
        " in ",
        " into ",
        " as ",
        " = ",
        " -> ",
        " → ",
        " is how many ",
    ] {
        let mut from = 0;
        while let Some(at) = text[from..].find(joiner) {
            let at = from + at;
            out.push((text[..at].trim(), text[at + joiner.len()..].trim()));
            from = at + 1;
        }
    }
    out
}

pub(crate) fn answer(query: &str) -> Option<Answer> {
    // As [`simplify`], but lowercasing only ASCII, so that `lower` and
    // `typed` keep the same byte offsets and a unit can be read as typed.
    let typed = query.trim().trim_end_matches(['?', '!', '.']).trim();
    let typed = typed.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = typed.to_ascii_lowercase();
    let as_typed = |part: &str| {
        let at = part.as_ptr() as usize - lower.as_ptr() as usize;
        &typed[at..at + part.len()]
    };
    let conversion = |from: &str, to: &str| conversion(from, to, as_typed);
    let text = lower.strip_prefix("convert ").unwrap_or(&lower);
    // "how many feet in a mile", "how many cm are in 5 inches"
    if let Some(rest) = text.strip_prefix("how many ") {
        for joiner in [" are in ", " is in ", " in ", " per "] {
            if let Some((to, from)) = rest.split_once(joiner) {
                if let Some(answer) = conversion(from, to) {
                    return Some(answer);
                }
            }
        }
        return None;
    }
    sides(text)
        .into_iter()
        .find_map(|(from, to)| conversion(from, to))
}

/// The conversion of `from` to `to`, parts of the lowercased query that
/// `typed` gives back as typed.
fn conversion<'a>(from: &str, to: &str, typed: impl Fn(&str) -> &'a str) -> Option<Answer> {
    let (value, from_name) = amount(from)?;
    let from_unit = find_typed(from_name, typed(from_name))?;
    let to_unit = find_typed(to, typed(to))?;
    if from_unit.dimension != to_unit.dimension || std::ptr::eq(from_unit, to_unit) {
        return None;
    }
    let result = convert(value, from_unit, to_unit);
    let note = (shown(value)? != "1").then(|| {
        Some(format!(
            "1 {} = {}",
            from_unit.one,
            named(convert(1.0, from_unit, to_unit), to_unit)?
        ))
    });
    Some(Answer {
        kind: Kind::Conversion,
        question: format!("{} =", named(value, from_unit)?),
        answer: named(result, to_unit)?,
        note: note
            .flatten()
            .filter(|_| from_unit.dimension != Temperature),
    })
}

#[cfg(test)]
mod tests {
    use super::answer;

    fn convert(query: &str) -> Option<String> {
        answer(query).map(|a| format!("{} {}", a.question, a.answer))
    }

    #[test]
    fn converts_units() {
        assert_eq!(
            convert("10 km in miles").unwrap(),
            "10 kilometres = 6.21371 miles"
        );
        assert_eq!(
            convert("100 f to c").unwrap(),
            "100 degrees Fahrenheit = 37.7778 degrees Celsius"
        );
        assert_eq!(
            convert("-40 °C to °F").unwrap(),
            "-40 degrees Celsius = -40 degrees Fahrenheit"
        );
        assert_eq!(
            convert("0 c in k").unwrap(),
            "0 degrees Celsius = 273.15 kelvins"
        );
        assert_eq!(
            convert("how many feet in a mile").unwrap(),
            "1 mile = 5,280 feet"
        );
        assert_eq!(
            convert("how many cm are in 5 inches?").unwrap(),
            "5 inches = 12.7 centimetres"
        );
        assert_eq!(
            convert("10 in in cm").unwrap(),
            "10 inches = 25.4 centimetres"
        );
        assert_eq!(
            convert("5 lbs to kg").unwrap(),
            "5 pounds = 2.26796 kilograms"
        );
        assert_eq!(
            convert("1 gallon to liters").unwrap(),
            "1 US gallon = 3.78541 litres"
        );
        assert_eq!(
            convert("1.5kg to grams").unwrap(),
            "1.5 kilograms = 1,500 grams"
        );
        assert_eq!(
            convert("60 mph to km/h").unwrap(),
            "60 miles per hour = 96.5606 kilometres per hour"
        );
        assert_eq!(
            convert("1 gib in mb").unwrap(),
            "1 gibibyte = 1,073.74 megabytes"
        );
        assert_eq!(
            convert("convert 3 cups to ml").unwrap(),
            "3 US cups = 709.765 millilitres"
        );
        assert_eq!(
            convert("km to miles").unwrap(),
            "1 kilometre = 0.621371 miles"
        );
        assert_eq!(
            convert("2 hours in minutes").unwrap(),
            "2 hours = 120 minutes"
        );
        assert_eq!(
            convert("6 feet to meters").unwrap(),
            "6 feet = 1.8288 metres"
        );
    }

    #[test]
    fn symbols_keep_their_case() {
        assert_eq!(convert("5 mW to W").unwrap(), "5 milliwatts = 0.005 watts");
        assert_eq!(
            convert("5 MW to W").unwrap(),
            "5 megawatts = 5,000,000 watts"
        );
        assert_eq!(convert("1 Gb to MB").unwrap(), "1 gigabit = 125 megabytes");
        assert_eq!(
            convert("1 GB to Mb").unwrap(),
            "1 gigabyte = 8,000 megabits"
        );
        assert_eq!(
            convert("1 gb to mb").unwrap(),
            "1 gigabyte = 1,000 megabytes"
        );
        assert_eq!(convert("8 Kb to B").unwrap(), "8 kilobits = 1,000 bytes");
        assert_eq!(
            convert("Convert 2 KM To Miles").unwrap(),
            "2 kilometres = 1.24274 miles"
        );
    }

    #[test]
    fn gr_is_a_grain() {
        assert_eq!(
            convert("100 gr to g").unwrap(),
            "100 grains = 6.47989 grams"
        );
        assert_eq!(
            convert("1 grain in mg").unwrap(),
            "1 grain = 64.7989 milligrams"
        );
    }

    #[test]
    fn notes_the_rate() {
        assert_eq!(
            answer("10 km in miles").unwrap().note.unwrap(),
            "1 kilometre = 0.621371 miles"
        );
        assert_eq!(answer("100 f to c").unwrap().note, None);
    }

    #[test]
    fn leaves_searches_alone() {
        for query in [
            "10 km in kg",
            "made in china",
            "time in tokyo",
            "log in to gmail",
            "back to the future",
            "km",
            "10 km",
            "2 fast 2 furious",
            "how many people in china",
            "in to in",
        ] {
            assert_eq!(answer(query), None, "{query:?}");
        }
    }
}
