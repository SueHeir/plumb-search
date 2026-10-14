//! The date of a holiday or another day kept every year: "when is
//! thanksgiving 2026", "easter 2027", "what day is christmas on", "how many
//! days until halloween". A day is a fixed date, a weekday of a month ("the
//! fourth Thursday in November"), or so many days from Easter. Days set by
//! a lunar calendar (Ramadan, Diwali, Lunar New Year) are left to the
//! results: their dates are announced, not worked out.
//!
//! A day named alone ("thanksgiving") is a search for it, not a question:
//! it gets an answer only with a year or words that ask when it is.

use crate::{simplify, Answer, Kind};

/// How a day's date is worked out from its year.
#[derive(Debug, Clone, Copy)]
enum Rule {
    /// A month and day.
    Fixed(u32, u32),
    /// The `n`th weekday (0 Monday … 6 Sunday) of a month, the last for 0.
    Weekday(u32, u32, u32),
    /// Western Easter Sunday, by the Gregorian calendar.
    Easter,
    /// Orthodox Easter Sunday, by the Julian calendar.
    Orthodox,
}

const MON: u32 = 0;
const THU: u32 = 3;
const SUN: u32 = 6;

/// A day kept every year.
struct Day {
    /// What people call it, lowercase, without apostrophes or periods.
    names: &'static [&'static str],
    /// As shown.
    shown: &'static str,
    rule: Rule,
    /// Days after the date `rule` gives: Black Friday is Thanksgiving's
    /// next day.
    after: i64,
    /// How its date is set and where, when its date alone does not say.
    how: &'static str,
}

const fn fixed(names: &'static [&'static str], shown: &'static str, month: u32, day: u32) -> Day {
    Day {
        names,
        shown,
        rule: Rule::Fixed(month, day),
        after: 0,
        how: "",
    }
}

const fn on(
    names: &'static [&'static str],
    shown: &'static str,
    rule: Rule,
    after: i64,
    how: &'static str,
) -> Day {
    Day {
        names,
        shown,
        rule,
        after,
        how,
    }
}

/// The days known. A name that is also a common search for something else
/// is left out: "independence day" is a film as often as a day, and many
/// countries'.
const DAYS: &[Day] = &[
    fixed(
        &["new years day", "new years", "new year"],
        "New Year's Day",
        1,
        1,
    ),
    on(
        &[
            "martin luther king day",
            "martin luther king jr day",
            "mlk day",
        ],
        "Martin Luther King Jr. Day",
        Rule::Weekday(3, MON, 1),
        0,
        "The third Monday in January, in the US",
    ),
    fixed(&["australia day"], "Australia Day", 1, 26),
    fixed(&["groundhog day"], "Groundhog Day", 2, 2),
    fixed(&["waitangi day"], "Waitangi Day", 2, 6),
    fixed(
        &[
            "valentines day",
            "valentines",
            "st valentines day",
            "saint valentines day",
        ],
        "Valentine's Day",
        2,
        14,
    ),
    on(
        &["presidents day", "washingtons birthday"],
        "Presidents' Day",
        Rule::Weekday(3, MON, 2),
        0,
        "The third Monday in February, in the US",
    ),
    fixed(
        &["st davids day", "saint davids day"],
        "St David's Day",
        3,
        1,
    ),
    fixed(
        &[
            "st patricks day",
            "saint patricks day",
            "st paddys day",
            "paddys day",
        ],
        "St Patrick's Day",
        3,
        17,
    ),
    fixed(
        &["april fools day", "april fools"],
        "April Fools' Day",
        4,
        1,
    ),
    fixed(&["earth day"], "Earth Day", 4, 22),
    fixed(
        &["st georges day", "saint georges day"],
        "St George's Day",
        4,
        23,
    ),
    fixed(&["anzac day"], "Anzac Day", 4, 25),
    fixed(&["cinco de mayo"], "Cinco de Mayo", 5, 5),
    on(
        &["mothers day"],
        "Mother's Day",
        Rule::Weekday(2, SUN, 5),
        0,
        "The second Sunday in May, in the US and Canada",
    ),
    on(
        &["memorial day"],
        "Memorial Day",
        Rule::Weekday(0, MON, 5),
        0,
        "The last Monday in May, in the US",
    ),
    on(
        &["fathers day"],
        "Father's Day",
        Rule::Weekday(3, SUN, 6),
        0,
        "The third Sunday in June, in the US, the UK and Canada",
    ),
    fixed(&["juneteenth"], "Juneteenth", 6, 19),
    fixed(&["canada day"], "Canada Day", 7, 1),
    fixed(
        &["fourth of july", "4th of july", "july 4th", "july fourth"],
        "Fourth of July",
        7,
        4,
    ),
    fixed(&["bastille day"], "Bastille Day", 7, 14),
    on(
        &["labor day"],
        "Labor Day",
        Rule::Weekday(1, MON, 9),
        0,
        "The first Monday in September, in the US and Canada",
    ),
    on(
        &["columbus day", "indigenous peoples day"],
        "Columbus Day",
        Rule::Weekday(2, MON, 10),
        0,
        "The second Monday in October, in the US",
    ),
    on(
        &[
            "canadian thanksgiving",
            "thanksgiving canada",
            "thanksgiving in canada",
            "canada thanksgiving",
        ],
        "Thanksgiving in Canada",
        Rule::Weekday(2, MON, 10),
        0,
        "The second Monday in October",
    ),
    fixed(&["halloween"], "Halloween", 10, 31),
    on(
        &["election day"],
        "Election Day",
        Rule::Weekday(1, MON, 11),
        1,
        "The Tuesday after the first Monday in November, in the US",
    ),
    fixed(
        &["guy fawkes night", "guy fawkes day", "bonfire night"],
        "Bonfire Night",
        11,
        5,
    ),
    fixed(
        &["veterans day", "remembrance day", "armistice day"],
        "Veterans Day",
        11,
        11,
    ),
    on(
        &["thanksgiving", "thanksgiving day"],
        "Thanksgiving",
        Rule::Weekday(4, THU, 11),
        0,
        "The fourth Thursday in November, in the US",
    ),
    on(
        &["black friday"],
        "Black Friday",
        Rule::Weekday(4, THU, 11),
        1,
        "The day after Thanksgiving in the US",
    ),
    on(
        &["cyber monday"],
        "Cyber Monday",
        Rule::Weekday(4, THU, 11),
        4,
        "The Monday after Thanksgiving in the US",
    ),
    fixed(
        &["st andrews day", "saint andrews day"],
        "St Andrew's Day",
        11,
        30,
    ),
    fixed(&["christmas eve"], "Christmas Eve", 12, 24),
    fixed(
        &["christmas", "christmas day", "xmas"],
        "Christmas Day",
        12,
        25,
    ),
    fixed(&["boxing day"], "Boxing Day", 12, 26),
    fixed(&["new years eve", "hogmanay"], "New Year's Eve", 12, 31),
    on(
        &["mardi gras", "shrove tuesday", "pancake day", "fat tuesday"],
        "Shrove Tuesday",
        Rule::Easter,
        -47,
        "47 days before Easter",
    ),
    on(
        &["ash wednesday"],
        "Ash Wednesday",
        Rule::Easter,
        -46,
        "46 days before Easter",
    ),
    on(
        &["mothering sunday", "mothers day uk", "uk mothers day"],
        "Mothering Sunday",
        Rule::Easter,
        -21,
        "Three weeks before Easter, in the UK and Ireland",
    ),
    on(
        &["palm sunday"],
        "Palm Sunday",
        Rule::Easter,
        -7,
        "The Sunday before Easter",
    ),
    on(
        &["maundy thursday", "holy thursday"],
        "Maundy Thursday",
        Rule::Easter,
        -3,
        "The Thursday before Easter",
    ),
    on(
        &["good friday"],
        "Good Friday",
        Rule::Easter,
        -2,
        "The Friday before Easter",
    ),
    on(
        &["easter", "easter sunday", "easter day"],
        "Easter Sunday",
        Rule::Easter,
        0,
        "Western churches",
    ),
    on(
        &["easter monday"],
        "Easter Monday",
        Rule::Easter,
        1,
        "The day after Easter",
    ),
    on(
        &["ascension day", "ascension thursday"],
        "Ascension Day",
        Rule::Easter,
        39,
        "39 days after Easter",
    ),
    on(
        &["pentecost", "whit sunday", "whitsunday", "whitsun"],
        "Pentecost",
        Rule::Easter,
        49,
        "Seven weeks after Easter",
    ),
    on(
        &["whit monday"],
        "Whit Monday",
        Rule::Easter,
        50,
        "The day after Pentecost",
    ),
    on(
        &["orthodox easter", "greek easter", "russian easter"],
        "Orthodox Easter",
        Rule::Orthodox,
        0,
        "Eastern Orthodox churches",
    ),
];

/// Words before a day that ask how long until it.
const COUNT_LEADS: &[&str] = &[
    "how many days until ",
    "how many days till ",
    "how many days to ",
    "how many days left until ",
    "how many sleeps until ",
    "how many sleeps till ",
    "how long until ",
    "how long till ",
    "days until ",
    "days till ",
    "days left until ",
    "sleeps until ",
    "countdown to ",
];

/// Words before a day that ask when it is.
const ASK_LEADS: &[&str] = &[
    "when is ",
    "whens ",
    "when was ",
    "when does ",
    "when do we celebrate ",
    "what day is ",
    "what day of the week is ",
    "what date is ",
    "what is the date of ",
    "date of ",
];

/// Years worked out: the Gregorian calendar's.
const YEARS: std::ops::RangeInclusive<i64> = 1600..=2999;

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

const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// The answer for `query` when it asks the date of a day kept every year,
/// or how long until it. `now` is in seconds since 1970 (UTC), and today
/// is taken to be its date in UTC.
pub(crate) fn answer(query: &str, now: i64) -> Option<Answer> {
    let query = simplify(query).replace(['\'', '’', '.'], "");
    let today = now.div_euclid(86_400);
    let this_year = civil(today).0;
    let counted = COUNT_LEADS.iter().find_map(|lead| query.strip_prefix(lead));
    let lead = counted.or_else(|| ASK_LEADS.iter().find_map(|lead| query.strip_prefix(lead)));
    let mut asked = lead.is_some();
    let mut words: Vec<&str> = lead.unwrap_or(&query).split(' ').collect();
    let mut year = None;
    if let Some(first) = words.first().and_then(|word| year_in(word)) {
        year = Some(first);
        words.remove(0);
    }
    // Words after the day: "christmas 2026", "easter this year",
    // "thanksgiving in 2027", "what day is christmas on".
    loop {
        match words.as_slice() {
            [.., "this" | "next", "year"] if year.is_none() => {
                asked = true;
                year = Some(this_year + i64::from(words[words.len() - 2] == "next"));
                words.pop();
            }
            [.., "in", last] if year.is_none() && year_in(last).is_some() => {
                year = year_in(last);
                words.pop();
            }
            [.., last] if year.is_none() && year_in(last).is_some() => year = year_in(last),
            [.., "date" | "on" | "fall" | "falls"] => asked = true,
            _ => break,
        }
        words.pop();
    }
    // A day and a past year with no word that asks is more often a film
    // ("halloween 1978") than a question.
    if !asked && year.is_none_or(|year| year < this_year) {
        return None;
    }
    let name = words.join(" ");
    let name = name.strip_prefix("the ").unwrap_or(&name);
    let day = DAYS.iter().find(|day| day.names.contains(&name))?;
    let (year, date) = match year {
        Some(year) => (year, day.date(year)),
        // The next one, or today's.
        None => [this_year, this_year + 1]
            .into_iter()
            .map(|year| (year, day.date(year)))
            .find(|(_, date)| *date >= today)?,
    };
    let away = date - today;
    let shown = format!("{} {year}", day.shown);
    let full = full_date(date);
    if counted.is_some() {
        let answer = match away {
            0 => "Today".to_string(),
            1 => "1 day".to_string(),
            -1 => "1 day ago".to_string(),
            n if n > 0 => format!("{n} days"),
            n => format!("{} days ago", -n),
        };
        let note = [full.as_str(), day.how]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        return Some(Answer {
            kind: Kind::Date,
            question: format!("Days until {shown}"),
            answer,
            note: Some(note),
        });
    }
    let when = match away {
        0 => "today".to_string(),
        1 => "tomorrow".to_string(),
        -1 => "yesterday".to_string(),
        n if n > 0 => format!("in {n} days"),
        n => format!("{} days ago", -n),
    };
    let note = if day.how.is_empty() {
        when
    } else {
        format!("{} · {when}", day.how)
    };
    Some(Answer {
        kind: Kind::Date,
        question: shown,
        answer: full,
        note: Some(note),
    })
}

/// `word` as a year worked out here.
fn year_in(word: &str) -> Option<i64> {
    let year: i64 = word.parse().ok().filter(|_| word.len() == 4)?;
    YEARS.contains(&year).then_some(year)
}

impl Day {
    /// Its date in `year`, in days since 1970-01-01.
    fn date(&self, year: i64) -> i64 {
        let base = match self.rule {
            Rule::Fixed(month, day) => days(year, month, day),
            Rule::Weekday(n, weekday, month) => {
                if n == 0 {
                    let last = first_of_next_month(year, month) - 1;
                    last - i64::from((weekday_of(last) + 7 - weekday) % 7)
                } else {
                    let first = days(year, month, 1);
                    first + i64::from((weekday + 7 - weekday_of(first)) % 7) + 7 * i64::from(n - 1)
                }
            }
            Rule::Easter => {
                // The anonymous Gregorian algorithm (Meeus).
                let (a, b, c) = (year % 19, year / 100, year % 100);
                let (d, e) = (b / 4, b % 4);
                let g = (8 * b + 13) / 25;
                let h = (19 * a + b - d - g + 15) % 30;
                let (i, k) = (c / 4, c % 4);
                let l = (32 + 2 * e + 2 * i - h - k) % 7;
                let m = (a + 11 * h + 19 * l) / 433;
                let month = (h + l - 7 * m + 90) / 25;
                let day = (h + l - 7 * m + 33 * month + 19) % 32;
                days(year, month as u32, day as u32)
            }
            Rule::Orthodox => {
                // Meeus's Julian algorithm, then from the Julian date to the
                // Gregorian one: 13 days later from 1900 to 2099.
                let (a, b, c) = (year % 4, year % 7, year % 19);
                let d = (19 * c + 15) % 30;
                let e = (2 * a + 4 * b - d + 34) % 7;
                let month = (d + e + 114) / 31;
                let day = (d + e + 114) % 31 + 1;
                days(year, month as u32, day as u32) + year / 100 - year / 400 - 2
            }
        };
        base + self.after
    }
}

/// The first day of the month after `month` of `year`.
fn first_of_next_month(year: i64, month: u32) -> i64 {
    if month == 12 {
        days(year + 1, 1, 1)
    } else {
        days(year, month + 1, 1)
    }
}

/// Days since 1970-01-01 of a Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year - era * 400;
    let month = i64::from(month);
    let of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let of_era = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    era * 146_097 + of_era - 719_468
}

/// The year, month and day of `days` since 1970-01-01.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let of_era = z - era * 146_097;
    let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * of_year + 2) / 153;
    let day = (of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// 0 for Monday … 6 for Sunday; 1970-01-01 was a Thursday.
fn weekday_of(days: i64) -> u32 {
    (days + 3).rem_euclid(7) as u32
}

/// "Thursday, November 26, 2026"
fn full_date(days: i64) -> String {
    let (year, month, day) = civil(days);
    format!(
        "{}, {} {day}, {year}",
        WEEKDAYS[weekday_of(days) as usize],
        MONTHS[month as usize - 1]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-13 00:00 UTC, a Tuesday.
    const NOW: i64 = 1_791_849_600;

    fn date(query: &str) -> String {
        answer(query, NOW)
            .unwrap_or_else(|| panic!("{query}"))
            .answer
    }

    #[test]
    fn dates_are_worked_out_for_each_kind_of_day() {
        assert_eq!(
            date("when is thanksgiving 2026"),
            "Thursday, November 26, 2026"
        );
        assert_eq!(date("easter 2027"), "Sunday, March 28, 2027");
        assert_eq!(date("easter 2026"), "Sunday, April 5, 2026");
        assert_eq!(date("orthodox easter 2026"), "Sunday, April 12, 2026");
        assert_eq!(date("orthodox easter 2027"), "Sunday, May 2, 2027");
        assert_eq!(date("Mother's Day 2026"), "Sunday, May 10, 2026");
        assert_eq!(date("mothering sunday 2026"), "Sunday, March 15, 2026");
        assert_eq!(date("memorial day 2026"), "Monday, May 25, 2026");
        assert_eq!(date("labor day 2026"), "Monday, September 7, 2026");
        assert_eq!(date("2027 mlk day"), "Monday, January 18, 2027");
        assert_eq!(date("election day 2026"), "Tuesday, November 3, 2026");
        assert_eq!(date("black friday 2026"), "Friday, November 27, 2026");
        assert_eq!(date("ash wednesday 2026"), "Wednesday, February 18, 2026");
        assert_eq!(date("good friday 2026"), "Friday, April 3, 2026");
        assert_eq!(date("pentecost 2026"), "Sunday, May 24, 2026");
        assert_eq!(date("St. Patrick's Day 2026"), "Tuesday, March 17, 2026");
        // Easter's dates over a range, from published tables.
        for (year, month, day) in [
            (1961, 4, 2),
            (2000, 4, 23),
            (2008, 3, 23),
            (2011, 4, 24),
            (2019, 4, 21),
            (2038, 4, 25),
            (2285, 3, 22),
        ] {
            let easter = DAYS.iter().find(|d| d.shown == "Easter Sunday").unwrap();
            assert_eq!(civil(easter.date(year)), (year, month, day), "{year}");
        }
    }

    #[test]
    fn without_a_year_the_next_one_is_given() {
        let a = answer("when is thanksgiving", NOW).unwrap();
        assert_eq!(a.kind, Kind::Date);
        assert_eq!(a.question, "Thanksgiving 2026");
        assert_eq!(a.answer, "Thursday, November 26, 2026");
        assert_eq!(
            a.note.unwrap(),
            "The fourth Thursday in November, in the US · in 44 days"
        );
        // Past this year: next year's.
        assert_eq!(date("when is memorial day"), "Monday, May 31, 2027");
        assert_eq!(
            date("what day is christmas on"),
            "Friday, December 25, 2026"
        );
        // "This year" is this year's, past or not.
        let a = answer("when was labor day this year", NOW).unwrap();
        assert_eq!(a.answer, "Monday, September 7, 2026");
        assert!(a.note.unwrap().ends_with("36 days ago"));
        assert_eq!(date("when is easter next year"), "Sunday, March 28, 2027");
        assert_eq!(date("christmas date"), "Friday, December 25, 2026");
        assert_eq!(
            date("when does the fourth of july fall"),
            "Sunday, July 4, 2027"
        );
    }

    #[test]
    fn counts_the_days_until_one() {
        let a = answer("how many days until christmas", NOW).unwrap();
        assert_eq!(a.question, "Days until Christmas Day 2026");
        assert_eq!(a.answer, "73 days");
        assert_eq!(a.note.unwrap(), "Friday, December 25, 2026");
        assert_eq!(
            answer("days until halloween?", NOW).unwrap().answer,
            "18 days"
        );
        // Canadian Thanksgiving was yesterday: the next one is a year off.
        assert_eq!(
            answer("how long until canadian thanksgiving", NOW)
                .unwrap()
                .answer,
            "363 days"
        );
    }

    #[test]
    fn a_day_named_alone_or_with_other_words_is_a_search() {
        for query in [
            "thanksgiving",
            "christmas",
            "easter",
            "halloween costumes",
            "black friday deals",
            "black friday 2026 deals",
            "mothers day gifts",
            "easter eggs 2027",
            "independence day 2026",
            "chinese new year 2027",
            "when is the game",
            "thanksgiving 1066",
            "halloween 1978",
        ] {
            assert_eq!(answer(query, NOW), None, "{query}");
        }
    }
}
