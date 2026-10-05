//! Currency conversions, from the European Central Bank's euro foreign
//! exchange reference rates: published each working day for about 30
//! currencies, free to reuse with the source named, and needing no key.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::format::format_number;
use crate::units::{amount, sides};
use crate::{simplify, Answer, Kind};

/// Where the day's rates are: a small XML file (about 2 KB).
pub const ECB_RATES_URL: &str = "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml";

/// Rates of one day: how much of each currency one euro buys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rates {
    /// The day the rates are of, `2026-10-02`.
    pub date: String,
    /// Units of each currency (ISO 4217 code) per euro; the euro's own is 1.
    pub per_euro: BTreeMap<String, f64>,
}

impl Rates {
    /// Reads the ECB's daily file: `<Cube time='2026-10-02'>` and
    /// `<Cube currency='USD' rate='1.1652'/>` elements.
    pub fn parse_ecb(xml: &str) -> Option<Rates> {
        let attribute = |element: &str, name: &str| -> Option<String> {
            let at = element.find(&format!("{name}="))? + name.len() + 1;
            let quote = element[at..].chars().next()?;
            let rest = &element[at + quote.len_utf8()..];
            Some(rest[..rest.find(quote)?].to_string())
        };
        let mut date = None;
        let mut per_euro = BTreeMap::new();
        for element in xml.split('<').filter(|e| e.starts_with("Cube ")) {
            if let Some(time) = attribute(element, "time") {
                date = Some(time);
            }
            if let (Some(code), Some(rate)) =
                (attribute(element, "currency"), attribute(element, "rate"))
            {
                let rate: f64 = rate.trim().parse().ok()?;
                if code.len() == 3 && rate.is_finite() && rate > 0.0 {
                    per_euro.insert(code.to_ascii_uppercase(), rate);
                }
            }
        }
        if per_euro.is_empty() {
            return None;
        }
        per_euro.insert("EUR".to_string(), 1.0);
        Some(Rates {
            date: date?,
            per_euro,
        })
    }
}

/// Currencies the ECB publishes, by code, with their names.
static CURRENCIES: &[(&str, &str, &str)] = &[
    ("EUR", "euro", "euros"),
    ("USD", "US dollar", "US dollars"),
    ("JPY", "Japanese yen", "Japanese yen"),
    ("BGN", "Bulgarian lev", "Bulgarian leva"),
    ("CZK", "Czech koruna", "Czech korunas"),
    ("DKK", "Danish krone", "Danish kroner"),
    ("GBP", "pound sterling", "pounds sterling"),
    ("HUF", "Hungarian forint", "Hungarian forints"),
    ("PLN", "Polish złoty", "Polish złoty"),
    ("RON", "Romanian leu", "Romanian lei"),
    ("SEK", "Swedish krona", "Swedish kronor"),
    ("CHF", "Swiss franc", "Swiss francs"),
    ("ISK", "Icelandic króna", "Icelandic krónur"),
    ("NOK", "Norwegian krone", "Norwegian kroner"),
    ("TRY", "Turkish lira", "Turkish lira"),
    ("AUD", "Australian dollar", "Australian dollars"),
    ("BRL", "Brazilian real", "Brazilian reais"),
    ("CAD", "Canadian dollar", "Canadian dollars"),
    ("CNY", "Chinese yuan", "Chinese yuan"),
    ("HKD", "Hong Kong dollar", "Hong Kong dollars"),
    ("IDR", "Indonesian rupiah", "Indonesian rupiah"),
    ("ILS", "Israeli shekel", "Israeli shekels"),
    ("INR", "Indian rupee", "Indian rupees"),
    ("KRW", "South Korean won", "South Korean won"),
    ("MXN", "Mexican peso", "Mexican pesos"),
    ("MYR", "Malaysian ringgit", "Malaysian ringgit"),
    ("NZD", "New Zealand dollar", "New Zealand dollars"),
    ("PHP", "Philippine peso", "Philippine pesos"),
    ("SGD", "Singapore dollar", "Singapore dollars"),
    ("THB", "Thai baht", "Thai baht"),
    ("ZAR", "South African rand", "South African rand"),
];

/// Other words and signs for currencies.
static OTHER_NAMES: &[(&str, &str)] = &[
    ("$", "USD"),
    ("dollar", "USD"),
    ("dollars", "USD"),
    ("bucks", "USD"),
    ("€", "EUR"),
    ("£", "GBP"),
    ("pound", "GBP"),
    ("pounds", "GBP"),
    ("quid", "GBP"),
    ("british pound", "GBP"),
    ("british pounds", "GBP"),
    ("¥", "JPY"),
    ("yen", "JPY"),
    ("yuan", "CNY"),
    ("renminbi", "CNY"),
    ("rmb", "CNY"),
    ("₹", "INR"),
    ("rupee", "INR"),
    ("rupees", "INR"),
    ("₩", "KRW"),
    ("won", "KRW"),
    ("franc", "CHF"),
    ("francs", "CHF"),
    ("real", "BRL"),
    ("reais", "BRL"),
    ("peso", "MXN"),
    ("pesos", "MXN"),
    ("baht", "THB"),
    ("rand", "ZAR"),
    ("shekel", "ILS"),
    ("shekels", "ILS"),
    ("₪", "ILS"),
    ("lira", "TRY"),
    ("zloty", "PLN"),
    ("forint", "HUF"),
    ("ringgit", "MYR"),
    ("rupiah", "IDR"),
    ("canadian dollar", "CAD"),
    ("canadian dollars", "CAD"),
    ("aussie dollar", "AUD"),
    ("australian dollar", "AUD"),
    ("australian dollars", "AUD"),
    ("swiss franc", "CHF"),
    ("swiss francs", "CHF"),
];

/// The code of the currency `name` names: a code, a sign or a name.
fn currency(name: &str) -> Option<&'static str> {
    let name = name.trim();
    if let Some((code, ..)) = CURRENCIES.iter().find(|(code, one, many)| {
        code.eq_ignore_ascii_case(name)
            || one.eq_ignore_ascii_case(name)
            || many.eq_ignore_ascii_case(name)
    }) {
        return Some(code);
    }
    OTHER_NAMES
        .iter()
        .find(|(other, _)| *other == name)
        .map(|(_, code)| *code)
}

fn names(code: &str) -> (&'static str, &'static str) {
    CURRENCIES
        .iter()
        .find(|(c, ..)| *c == code)
        .map(|(_, one, many)| (*one, *many))
        .unwrap_or(("", ""))
}

/// The amount and currency of `text`: "100 usd", "$100", "100$", "€ 20".
fn money(text: &str) -> Option<(f64, &'static str)> {
    let text = text.trim();
    // A sign before the number: "$100".
    for sign in ["$", "€", "£", "¥", "₹", "₩", "₪"] {
        if let Some(rest) = text.strip_prefix(sign) {
            let (value, after) = amount(rest)?;
            if after.is_empty() {
                return Some((value, currency(sign)?));
            }
        }
    }
    let (value, name) = amount(text)?;
    Some((value, currency(name)?))
}

/// Whether `query` names a currency on each side of a conversion.
pub(crate) fn looks_like(query: &str) -> bool {
    let text = simplify(query);
    sides(&text)
        .into_iter()
        .any(|(from, to)| money(from).is_some() && currency(to).is_some())
}

fn shown(value: f64) -> Option<String> {
    if value.abs() >= 1.0 {
        // Two decimals, as money is written.
        let cents = (value * 100.0).round() / 100.0;
        let text = format_number(cents, 15)?;
        Some(match text.split_once('.') {
            Some((_, fraction)) if fraction.len() == 1 => format!("{text}0"),
            _ => text,
        })
    } else {
        format_number(value, 4)
    }
}

fn named(value: f64, code: &str) -> Option<String> {
    named_as(shown(value)?, code)
}

fn named_as(number: String, code: &str) -> Option<String> {
    let (one, many) = names(code);
    Some(format!(
        "{number} {}",
        if number == "1" || number == "1.00" {
            one
        } else {
            many
        }
    ))
}

pub(crate) fn answer(query: &str, rates: &Rates) -> Option<Answer> {
    let text = simplify(query);
    let text = text.strip_prefix("convert ").unwrap_or(&text);
    sides(text).into_iter().find_map(|(from, to)| {
        let (value, from) = money(from)?;
        let to = currency(to)?;
        if from == to {
            return None;
        }
        let per_unit = rates.per_euro.get(to)? / rates.per_euro.get(from)?;
        Some(Answer {
            kind: Kind::Currency,
            question: format!("{} =", named_as(format_number(value, 15)?, from)?),
            answer: named(value * per_unit, to)?,
            note: Some(format!(
                "1 {from} = {} {to} · European Central Bank reference rate of {}",
                format_number(per_unit, 5)?,
                rates.date
            )),
        })
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const ECB_SAMPLE: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <gesmes:Envelope xmlns:gesmes=\"http://www.gesmes.org/xml/2002-08-01\">\n\
        <gesmes:subject>Reference rates</gesmes:subject>\n\
        <Cube>\n<Cube time='2026-10-02'>\n\
        <Cube currency='USD' rate='1.1650'/>\n\
        <Cube currency='JPY' rate='172.50'/>\n\
        <Cube currency='GBP' rate='0.8700'/>\n\
        </Cube>\n</Cube>\n</gesmes:Envelope>";

    fn rates() -> Rates {
        Rates::parse_ecb(ECB_SAMPLE).unwrap()
    }

    #[test]
    fn reads_the_ecb_file() {
        let rates = rates();
        assert_eq!(rates.date, "2026-10-02");
        assert_eq!(rates.per_euro["USD"], 1.165);
        assert_eq!(rates.per_euro["EUR"], 1.0);
        assert_eq!(Rates::parse_ecb("<html>not it</html>"), None);
    }

    #[test]
    fn converts_money() {
        let a = answer("100 usd to eur", &rates()).unwrap();
        assert_eq!(a.question, "100 US dollars =");
        assert_eq!(a.answer, "85.84 euros");
        assert_eq!(
            a.note.unwrap(),
            "1 USD = 0.85837 EUR · European Central Bank reference rate of 2026-10-02"
        );
        assert_eq!(
            answer("$20 in pounds", &rates()).unwrap().answer,
            "14.94 pounds sterling"
        );
        assert_eq!(
            answer("1000 yen to dollars", &rates()).unwrap().answer,
            "6.75 US dollars"
        );
        assert_eq!(answer("100 usd to cad", &rates()), None, "no rate");
        assert_eq!(answer("10 pounds to kg", &rates()), None);
    }
}
