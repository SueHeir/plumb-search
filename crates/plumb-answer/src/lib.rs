//! Instant answers: what a query asks that can be worked out from the query
//! alone, shown above the results.
//!
//! - sums: `12 * (3 + 4)`, `sqrt(2)`, `15% of 80`, `2^10`;
//! - unit conversions: `10 km in miles`, `100 f to c`, `how many feet in a
//!   mile`;
//! - currency conversions: `100 usd to eur`, from reference rates the
//!   caller has ([`Rates`], the European Central Bank's daily rates);
//! - where to get help now, for searches by someone who may be in crisis
//!   ("depression help", "suicide hotline");
//! - the time in a place: `time in tokyo`, `what time is it in paris`, and
//!   a time from one place in another: `3pm est to pst` (with the `zones`
//!   feature, on by default).
//!
//! Nothing here reads the network, the clock or the computer's settings:
//! the caller passes the current time and the rates it has, so the same
//! code answers on a node and in the browser (`/private`), and the time
//! zone database is built in.

use serde::{Deserialize, Serialize};

mod calc;
mod currency;
mod format;
mod help;
#[cfg(feature = "zones")]
mod places;
#[cfg(feature = "zones")]
mod time;
mod units;

pub use currency::{Rates, ECB_RATES_URL};
pub use format::format_number;

/// What kind of question an [`Answer`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Calculation,
    Conversion,
    Currency,
    Time,
    /// Where to get help now ("depression help").
    Help,
}

/// An answer to the query, as shown: the question as understood, the
/// answer, and a line of detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub kind: Kind,
    /// The question as understood: `12 × (3 + 4)`, `10 kilometres`, `Time
    /// in Tokyo, Japan`.
    pub question: String,
    /// `84`, `6.21371 miles`, `9:41 PM`.
    pub answer: String,
    /// More about the answer, e.g. the date and time zone, or where a rate
    /// came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Longest query looked at; longer ones are searches.
const MAX_QUERY: usize = 120;

/// The answer to `query`, if it asks something that can be worked out.
/// `now` is the current time, in seconds since 1970 (UTC); `rates` the
/// currency rates the caller has, if any.
pub fn answer(query: &str, now: i64, rates: Option<&Rates>) -> Option<Answer> {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > MAX_QUERY {
        return None;
    }
    if let Some(answer) = help::answer(query) {
        return Some(answer);
    }
    #[cfg(feature = "zones")]
    if let Some(answer) = time::answer(query, now) {
        return Some(answer);
    }
    #[cfg(not(feature = "zones"))]
    let _ = now;
    units::answer(query)
        .or_else(|| rates.and_then(|rates| currency::answer(query, rates)))
        .or_else(|| calc::answer(query))
}

/// Whether `query` may be a currency conversion, to know whether rates are
/// worth fetching for it before calling [`answer`].
pub fn may_need_rates(query: &str) -> bool {
    query.chars().count() <= MAX_QUERY && currency::looks_like(query)
}

/// `text` lowercased, with runs of spaces made one and some punctuation
/// people add (`?`, `!` at the end) dropped.
fn simplify(text: &str) -> String {
    let text = text
        .trim()
        .trim_end_matches(['?', '!', '.'])
        .trim()
        .to_lowercase();
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_searches_get_no_answer() {
        for query in [
            "",
            "python",
            "us bank",
            "2024",
            "9/11",
            "24/7",
            "1-800-flowers",
            "covid-19",
            "50/50",
            "web 3.0",
            "f1",
            "4chan",
            "the time machine",
            "time",
            "x",
            "pi",
        ] {
            assert_eq!(answer(query, 0, None), None, "{query:?}");
        }
    }

    #[test]
    fn answers_each_kind() {
        assert_eq!(answer("2+2", 0, None).unwrap().kind, Kind::Calculation);
        assert_eq!(
            answer("10 km in miles", 0, None).unwrap().kind,
            Kind::Conversion
        );
        #[cfg(feature = "zones")]
        assert_eq!(answer("time in tokyo", 0, None).unwrap().kind, Kind::Time);
        let rates = Rates::parse_ecb(currency::tests::ECB_SAMPLE).unwrap();
        assert_eq!(
            answer("100 usd to eur", 0, Some(&rates)).unwrap().kind,
            Kind::Currency
        );
        assert!(may_need_rates("100 usd to eur"));
        assert!(!may_need_rates("10 km in miles"));
    }
}
