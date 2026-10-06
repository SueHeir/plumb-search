//! A few facts about the thing a Wikipedia article is about, from
//! Wikidata: a country's capital, a mountain's elevation, a person's birth
//! date, a company's CEO. Searches that ask for one ("capital of
//! australia", "how tall is mount everest") get it as an instant answer.
//!
//! Facts ride on an article's line of profiles ([`crate::article`]) as
//! `f-KEY=VALUE` pairs, which readers made before them leave out as
//! services they do not know:
//!
//! ```text
//! profiles  Q408  f-capital=Canberra|f-population=27204809;2024
//! ```
//!
//! A value is kept as Wikidata gives it, in one plain form per kind
//! ([`ValueType`]): a name, a number in SI units (metres, square metres,
//! people), or a date as precise as Wikidata knows it (`1879-03-14`,
//! `1879-03`, `1879`; `-0500` for 500 BC). A population may say the year
//! it was counted after a `;`.

use serde::{Deserialize, Serialize};

/// What a fact's value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    /// Another item, kept by its English name.
    Item,
    /// A number in SI units: metres, square metres, or a count.
    Quantity,
    /// A date.
    Time,
}

/// The kinds of facts kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactKind {
    Capital,
    Population,
    Elevation,
    Height,
    Area,
    Born,
    Died,
    Founded,
    Founder,
    Ceo,
    Headquarters,
    Currency,
}

/// Every kind, in the order an article's facts are written.
pub const KINDS: &[FactKind] = &[
    FactKind::Capital,
    FactKind::Population,
    FactKind::Elevation,
    FactKind::Height,
    FactKind::Area,
    FactKind::Born,
    FactKind::Died,
    FactKind::Founded,
    FactKind::Founder,
    FactKind::Ceo,
    FactKind::Headquarters,
    FactKind::Currency,
];

/// Most values kept of one kind (a company's founders).
pub const MAX_VALUES: usize = 3;

/// What starts a fact's key on a line of profiles.
pub const FACT_PREFIX: &str = "f-";

impl FactKind {
    /// The kind's key on a line of profiles, after [`FACT_PREFIX`].
    pub fn key(self) -> &'static str {
        match self {
            FactKind::Capital => "capital",
            FactKind::Population => "population",
            FactKind::Elevation => "elevation",
            FactKind::Height => "height",
            FactKind::Area => "area",
            FactKind::Born => "born",
            FactKind::Died => "died",
            FactKind::Founded => "founded",
            FactKind::Founder => "founder",
            FactKind::Ceo => "ceo",
            FactKind::Headquarters => "headquarters",
            FactKind::Currency => "currency",
        }
    }

    pub fn from_key(key: &str) -> Option<FactKind> {
        KINDS.iter().copied().find(|kind| kind.key() == key)
    }

    /// Wikidata's property for it.
    pub fn property(self) -> &'static str {
        match self {
            FactKind::Capital => "P36",
            FactKind::Population => "P1082",
            FactKind::Elevation => "P2044",
            FactKind::Height => "P2048",
            FactKind::Area => "P2046",
            FactKind::Born => "P569",
            FactKind::Died => "P570",
            FactKind::Founded => "P571",
            FactKind::Founder => "P112",
            FactKind::Ceo => "P169",
            FactKind::Headquarters => "P159",
            FactKind::Currency => "P38",
        }
    }

    pub fn value_type(self) -> ValueType {
        match self {
            FactKind::Capital
            | FactKind::Founder
            | FactKind::Ceo
            | FactKind::Headquarters
            | FactKind::Currency => ValueType::Item,
            FactKind::Population | FactKind::Elevation | FactKind::Height | FactKind::Area => {
                ValueType::Quantity
            }
            FactKind::Born | FactKind::Died | FactKind::Founded => ValueType::Time,
        }
    }

    /// How many values of the kind an item keeps.
    pub fn most_values(self) -> usize {
        match self.value_type() {
            ValueType::Item => MAX_VALUES,
            _ => 1,
        }
    }

    /// The question it answers, about `subject`: "Capital of Australia".
    pub fn question(self, subject: &str) -> String {
        match self {
            FactKind::Capital => format!("Capital of {subject}"),
            FactKind::Population => format!("Population of {subject}"),
            FactKind::Elevation => format!("Elevation of {subject}"),
            FactKind::Height => format!("Height of {subject}"),
            FactKind::Area => format!("Area of {subject}"),
            FactKind::Born => format!("{subject}, born"),
            FactKind::Died => format!("{subject}, died"),
            FactKind::Founded => format!("{subject}, founded"),
            FactKind::Founder => format!("Founder of {subject}"),
            FactKind::Ceo => format!("CEO of {subject}"),
            FactKind::Headquarters => format!("Headquarters of {subject}"),
            FactKind::Currency => format!("Currency of {subject}"),
        }
    }
}

/// One fact about an article's item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub kind: FactKind,
    /// The value, in the kind's form (see the module docs).
    pub value: String,
}

/// `text` made to fit a value on a line of profiles: no tabs, line
/// breaks or `|`, and whitespace collapsed.
fn value_field(text: &str) -> String {
    crate::collapse_whitespace(&text.replace(['\t', '\n', '\r', '|'], " "))
}

/// Whether `value` reads as a value of a kind of `value_type`.
pub fn value_fits(value_type: ValueType, value: &str) -> bool {
    match value_type {
        ValueType::Item => !value.is_empty() && value.chars().count() <= 120,
        ValueType::Quantity => {
            let (number, year) = value.split_once(';').unwrap_or((value, ""));
            number.parse::<f64>().is_ok_and(f64::is_finite)
                && (year.is_empty() || year.parse::<i32>().is_ok())
        }
        ValueType::Time => Date::parse(value).is_some(),
    }
}

/// Writes `facts` as `|`-separated pairs for a line of profiles, leaving
/// out values that don't fit their kind.
pub fn write_facts(facts: &[Fact]) -> String {
    facts
        .iter()
        .map(|fact| (fact.kind, value_field(&fact.value)))
        .filter(|(kind, value)| value_fits(kind.value_type(), value))
        .map(|(kind, value)| format!("{FACT_PREFIX}{}={value}", kind.key()))
        .collect::<Vec<_>>()
        .join("|")
}

/// Reads the facts among the pairs of a line of profiles, leaving out
/// kinds this build does not know and values that do not read, at most
/// [`FactKind::most_values`] of a kind.
pub fn parse_facts(text: &str) -> Vec<Fact> {
    let mut facts: Vec<Fact> = Vec::new();
    for pair in text.split('|') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let Some(kind) = key
            .trim()
            .strip_prefix(FACT_PREFIX)
            .and_then(FactKind::from_key)
        else {
            continue;
        };
        let value = value.trim();
        if !value_fits(kind.value_type(), value)
            || facts.iter().filter(|f| f.kind == kind).count() >= kind.most_values()
        {
            continue;
        }
        facts.push(Fact {
            kind,
            value: value.to_string(),
        });
    }
    facts
}

/// A date as precise as Wikidata knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    /// Negative before year 1 (`-500` is 500 BC).
    pub year: i32,
    pub month: Option<u8>,
    pub day: Option<u8>,
}

impl Date {
    /// Reads `1879-03-14`, `1879-03`, `1879` or `-0500`.
    pub fn parse(text: &str) -> Option<Date> {
        let (negative, rest) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let mut parts = rest.split('-');
        let year_text = parts.next()?;
        if year_text.is_empty() || !year_text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let year: i32 = year_text.parse().ok()?;
        let year = if negative { -year } else { year };
        let mut part = |most: u8| -> Option<Option<u8>> {
            match parts.next() {
                None => Some(None),
                Some(p) if p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit()) => {
                    let n: u8 = p.parse().ok()?;
                    (1..=most).contains(&n).then_some(Some(n))
                }
                Some(_) => None,
            }
        };
        let month = part(12)?;
        let day = part(31)?;
        if (month.is_none() && day.is_some()) || parts.next().is_some() || year == 0 {
            return None;
        }
        Some(Date { year, month, day })
    }

    /// Reads Wikidata's time value (`+1879-03-14T00:00:00Z`) at its
    /// precision (11 a day, 10 a month, 9 a year); `None` for less precise
    /// ones (a decade, a century).
    pub fn from_wikidata(time: &str, precision: u8) -> Option<Date> {
        let time = time.strip_prefix('+').unwrap_or(time);
        let (negative, rest) = match time.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, time),
        };
        let date = rest.split('T').next()?;
        let mut parts = date.split('-');
        let year: i32 = parts.next()?.parse().ok()?;
        let month: u8 = parts.next()?.parse().ok()?;
        let day: u8 = parts.next()?.parse().ok()?;
        let year = if negative { -year } else { year };
        if year == 0 {
            return None;
        }
        match precision {
            11 if month > 0 && day > 0 => Some(Date {
                year,
                month: Some(month),
                day: Some(day),
            }),
            10 if month > 0 => Some(Date {
                year,
                month: Some(month),
                day: None,
            }),
            9 => Some(Date {
                year,
                month: None,
                day: None,
            }),
            _ => None,
        }
    }

    /// As kept in a fact: `1879-03-14`, `-0500`.
    pub fn write(&self) -> String {
        let sign = if self.year < 0 { "-" } else { "" };
        let mut out = format!("{sign}{:04}", self.year.unsigned_abs());
        if let Some(month) = self.month {
            out.push_str(&format!("-{month:02}"));
            if let Some(day) = self.day {
                out.push_str(&format!("-{day:02}"));
            }
        }
        out
    }

    /// As people read it: "March 14, 1879", "March 1879", "1879", "500 BC".
    pub fn display(&self) -> String {
        const MONTHS: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        let year = if self.year < 0 {
            format!("{} BC", self.year.unsigned_abs())
        } else {
            self.year.to_string()
        };
        match (self.month, self.day) {
            (Some(m), Some(d)) => format!("{} {d}, {year}", MONTHS[usize::from(m) - 1]),
            (Some(m), None) => format!("{} {year}", MONTHS[usize::from(m) - 1]),
            _ => year,
        }
    }

    /// Whole years from `self` to `to`, when both are known to the day;
    /// a birthday later in the year not yet counted.
    pub fn years_until(&self, to: &Date) -> Option<i32> {
        let (Some(m), Some(d), Some(tm), Some(td)) = (self.month, self.day, to.month, to.day)
        else {
            return None;
        };
        let mut years = to.year - self.year;
        if self.year < 0 && to.year > 0 {
            years -= 1;
        }
        if (tm, td) < (m, d) {
            years -= 1;
        }
        (years >= 0).then_some(years)
    }
}

/// What a query asks: the kinds of fact, the likeliest first, and about
/// what ("mount everest" of "how tall is mount everest").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactQuestion {
    pub kinds: Vec<FactKind>,
    pub subject: String,
    /// "how old is X": the age, worked out from the birth date.
    pub age: bool,
}

/// The fact `query` asks for, if it asks for one in a way people do:
/// "capital of australia", "how tall is mount everest", "when was einstein
/// born", "who is the ceo of nvidia", "tesla founder".
pub fn fact_asked(query: &str) -> Option<FactQuestion> {
    let q = query
        .trim()
        .trim_end_matches(['?', '.', '!'])
        .to_lowercase()
        .replace('’', "'");
    let q = crate::collapse_whitespace(&q);
    let q = ["what is ", "what's ", "whats ", "what are ", "tell me "]
        .iter()
        .find_map(|lead| q.strip_prefix(lead))
        .unwrap_or(&q)
        .to_string();
    let q = q.strip_prefix("the ").unwrap_or(&q);
    use FactKind::*;
    // Words before the subject.
    let before: &[(&str, &[FactKind], bool)] = &[
        ("capital city of ", &[Capital], false),
        ("capital of ", &[Capital], false),
        ("population of ", &[Population], false),
        ("how many people live in ", &[Population], false),
        ("how tall is ", &[Elevation, Height], false),
        ("how high is ", &[Elevation, Height], false),
        ("height of ", &[Height, Elevation], false),
        ("elevation of ", &[Elevation], false),
        ("area of ", &[Area], false),
        ("size of ", &[Area], false),
        ("how big is ", &[Area], false),
        ("how old is ", &[Born], true),
        ("age of ", &[Born], true),
        ("birthday of ", &[Born], false),
        ("founder of ", &[Founder], false),
        ("founders of ", &[Founder], false),
        ("who founded ", &[Founder], false),
        ("who started ", &[Founder], false),
        ("who created ", &[Founder], false),
        ("ceo of ", &[Ceo], false),
        ("who is the ceo of ", &[Ceo], false),
        ("who's the ceo of ", &[Ceo], false),
        ("who runs ", &[Ceo], false),
        ("headquarters of ", &[Headquarters], false),
        ("where is the headquarters of ", &[Headquarters], false),
        ("currency of ", &[Currency], false),
        ("currency in ", &[Currency], false),
    ];
    // Words after it.
    let after: &[(&str, &[FactKind], bool)] = &[
        (" capital city", &[Capital], false),
        (" capital", &[Capital], false),
        (" population", &[Population], false),
        (" elevation", &[Elevation], false),
        (" height", &[Height, Elevation], false),
        (" birthday", &[Born], false),
        (" date of birth", &[Born], false),
        (" birth date", &[Born], false),
        (" age", &[Born], true),
        (" founder", &[Founder], false),
        (" founders", &[Founder], false),
        (" ceo", &[Ceo], false),
        (" headquarters", &[Headquarters], false),
        (" hq", &[Headquarters], false),
        (" currency", &[Currency], false),
    ];
    // Words around it.
    let around: &[(&str, &str, &[FactKind])] = &[
        ("when was ", " born", &[Born]),
        ("when is ", " birthday", &[Born]),
        ("where was ", " born", &[]),
        ("when did ", " die", &[Died]),
        ("when was ", " founded", &[Founded]),
        ("when was ", " established", &[Founded]),
        ("when was ", " built", &[Founded]),
        ("when was ", " formed", &[Founded]),
        ("where is ", " headquartered", &[Headquarters]),
        ("where is ", " based", &[Headquarters]),
        ("what currency does ", " use", &[Currency]),
    ];
    let found = around
        .iter()
        .find_map(|(lead, tail, kinds)| {
            let subject = q.strip_prefix(lead)?.strip_suffix(tail)?;
            Some((subject, kinds.to_vec(), false))
        })
        .or_else(|| {
            before
                .iter()
                .find_map(|(lead, kinds, age)| Some((q.strip_prefix(lead)?, kinds.to_vec(), *age)))
        })
        .or_else(|| {
            after
                .iter()
                .find_map(|(tail, kinds, age)| Some((q.strip_suffix(tail)?, kinds.to_vec(), *age)))
        })?;
    let (subject, kinds, age) = found;
    let subject = subject.strip_prefix("the ").unwrap_or(subject).trim();
    if kinds.is_empty() || subject.is_empty() || subject.split(' ').count() > 6 {
        return None;
    }
    Some(FactQuestion {
        kinds,
        subject: subject.to_string(),
        age,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(kind: FactKind, value: &str) -> Fact {
        Fact {
            kind,
            value: value.into(),
        }
    }

    #[test]
    fn facts_round_trip_and_bad_values_are_left_out() {
        let facts = vec![
            fact(FactKind::Capital, "Canberra"),
            fact(FactKind::Population, "27204809;2024"),
            fact(FactKind::Born, "1879-03-14"),
            fact(FactKind::Founded, "-0753"),
        ];
        let text = write_facts(&facts);
        assert_eq!(
            text,
            "f-capital=Canberra|f-population=27204809;2024|f-born=1879-03-14|f-founded=-0753"
        );
        assert_eq!(parse_facts(&text), facts);
        // Unknown kinds, profiles and values that don't read are skipped.
        let parsed = parse_facts("youtube-handle=x|f-mass=3|f-born=yesterday|f-elevation=8848.86");
        assert_eq!(parsed, vec![fact(FactKind::Elevation, "8848.86")]);
        // A name with a | in it can't break the line.
        assert_eq!(write_facts(&[fact(FactKind::Ceo, "A|B")]), "f-ceo=A B");
    }

    #[test]
    fn dates_read_at_their_precision() {
        assert_eq!(
            Date::from_wikidata("+1879-03-14T00:00:00Z", 11)
                .unwrap()
                .write(),
            "1879-03-14"
        );
        assert_eq!(
            Date::from_wikidata("+1879-03-14T00:00:00Z", 9)
                .unwrap()
                .write(),
            "1879"
        );
        assert_eq!(
            Date::from_wikidata("-0753-00-00T00:00:00Z", 9)
                .unwrap()
                .write(),
            "-0753"
        );
        assert_eq!(Date::from_wikidata("+1800-00-00T00:00:00Z", 7), None);
        assert_eq!(
            Date::parse("1879-03-14").unwrap().display(),
            "March 14, 1879"
        );
        assert_eq!(Date::parse("1879-03").unwrap().display(), "March 1879");
        assert_eq!(Date::parse("-0753").unwrap().display(), "753 BC");
        assert_eq!(Date::parse("1879-13"), None);
        assert_eq!(Date::parse("1879--1"), None);
    }

    #[test]
    fn ages_count_whole_years() {
        let born = Date::parse("1971-06-28").unwrap();
        assert_eq!(
            born.years_until(&Date::parse("2026-06-27").unwrap()),
            Some(54)
        );
        assert_eq!(
            born.years_until(&Date::parse("2026-06-28").unwrap()),
            Some(55)
        );
        let year_only = Date::parse("1971").unwrap();
        assert_eq!(
            year_only.years_until(&Date::parse("2026-10-06").unwrap()),
            None
        );
    }

    #[test]
    fn questions_people_type_are_read() {
        let asked = |q: &str| fact_asked(q).map(|f| (f.kinds[0], f.subject, f.age));
        use FactKind::*;
        assert_eq!(
            asked("capital of australia"),
            Some((Capital, "australia".into(), false))
        );
        assert_eq!(
            asked("What is the capital of Australia?"),
            Some((Capital, "australia".into(), false))
        );
        assert_eq!(
            asked("how tall is mount everest"),
            Some((Elevation, "mount everest".into(), false))
        );
        assert_eq!(
            asked("when was albert einstein born"),
            Some((Born, "albert einstein".into(), false))
        );
        assert_eq!(
            asked("how old is elon musk"),
            Some((Born, "elon musk".into(), true))
        );
        assert_eq!(
            asked("who is the ceo of nvidia"),
            Some((Ceo, "nvidia".into(), false))
        );
        assert_eq!(
            asked("tesla founder"),
            Some((Founder, "tesla".into(), false))
        );
        assert_eq!(
            asked("when did freddie mercury die"),
            Some((Died, "freddie mercury".into(), false))
        );
        assert_eq!(
            asked("when was ibm founded"),
            Some((Founded, "ibm".into(), false))
        );
        assert_eq!(
            asked("where is nike headquartered"),
            Some((Headquarters, "nike".into(), false))
        );
        assert_eq!(
            asked("japan population"),
            Some((Population, "japan".into(), false))
        );
        assert_eq!(
            asked("what currency does japan use"),
            Some((Currency, "japan".into(), false))
        );
        // Not questions of a fact.
        assert_eq!(asked("capital one"), None);
        assert_eq!(asked("where was einstein born"), None);
        assert_eq!(asked("python"), None);
        assert_eq!(asked("capital"), None);
    }
}
