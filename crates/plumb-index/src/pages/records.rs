//! Records a search names by their identifier: "rfc 3339", "pep 8",
//! "oeis a001316", "CVE-2021-44228". Each scheme keeps its records at an
//! address built from the identifier, so the record can lead the results
//! whether or not a page set holds it, the way an official profile is
//! linked from the identifier Wikidata keeps ([`plumb_core::profiles`]).
//! Without it the scheme's homepage was the best Plumb had: "rfc 3339"
//! listed rfc-editor.org, "pep 8" an unofficial copy of the guide.
//!
//! Only identifiers that look right make a link, so a strange query can
//! never build an address somewhere else; a well-formed one that names no
//! record leads to the scheme's own "not found" page.

use super::{Page, PageHit, RECORDS_SET};

/// Most records listed for one search.
const MAX_RECORDS: usize = 3;

/// Words after a number that make it a quantity, not an identifier: "pep
/// 28 days" is post-exposure prophylaxis, not PEP 28.
const QUANTITY_WORDS: &[&str] = &[
    "day", "days", "hour", "hours", "week", "weeks", "month", "months", "year", "years", "mg",
];

/// A kind of identifier and where its records are.
struct Scheme {
    /// The word that names the scheme in a query, or `None` when the
    /// identifier's shape says enough ("CVE-2021-44228").
    word: Option<&'static str>,
    /// What the record is called before its identifier: "RFC".
    name: &'static str,
    /// What the record is, shown with it.
    description: &'static str,
    shape: Shape,
    /// The record's address, with `{}` for the identifier.
    pattern: &'static str,
}

#[derive(Clone, Copy)]
enum Shape {
    /// A number of at most this many digits, written with at least the
    /// second count of them in the address (`pep-0008`).
    Number(usize, usize),
    /// An OEIS sequence: `A` and up to six digits.
    Sequence,
    /// A Wikidata item (`Q42`) or property (`P498`).
    Entity,
    /// An arXiv identifier of 2007 on: `1706.03762`, maybe with a version.
    Arxiv,
    /// A CVE record: `CVE-2021-44228`.
    Cve,
    /// An ISBN of 13 digits, or of 10 when the query says "isbn", with a
    /// right check digit.
    Isbn,
    /// A DOI: `10.1038/nature14539`.
    Doi,
}

const SCHEMES: &[Scheme] = &[
    Scheme {
        word: Some("rfc"),
        name: "RFC",
        description: "Request for Comments, from the RFC Editor",
        shape: Shape::Number(5, 0),
        pattern: "https://www.rfc-editor.org/rfc/rfc{}.html",
    },
    Scheme {
        word: Some("pep"),
        name: "PEP",
        description: "Python Enhancement Proposal",
        shape: Shape::Number(4, 4),
        pattern: "https://peps.python.org/pep-{}/",
    },
    Scheme {
        word: Some("oeis"),
        name: "OEIS",
        description: "Integer sequence in the On-Line Encyclopedia of Integer Sequences",
        shape: Shape::Sequence,
        pattern: "https://oeis.org/{}",
    },
    Scheme {
        word: Some("wikidata"),
        name: "Wikidata",
        description: "Wikidata entity",
        shape: Shape::Entity,
        pattern: "https://www.wikidata.org/wiki/{}",
    },
    Scheme {
        word: Some("arxiv"),
        name: "arXiv",
        description: "Paper on arXiv",
        shape: Shape::Arxiv,
        pattern: "https://arxiv.org/abs/{}",
    },
    Scheme {
        word: Some("pmid"),
        name: "PMID",
        description: "Article on PubMed",
        shape: Shape::Number(9, 0),
        pattern: "https://pubmed.ncbi.nlm.nih.gov/{}/",
    },
    Scheme {
        word: None,
        name: "",
        description: "Vulnerability record of the CVE Program",
        shape: Shape::Cve,
        pattern: "https://www.cve.org/CVERecord?id={}",
    },
    Scheme {
        word: None,
        name: "ISBN",
        description: "Book at Open Library",
        shape: Shape::Isbn,
        pattern: "https://openlibrary.org/isbn/{}",
    },
    Scheme {
        word: None,
        name: "DOI",
        description: "Work found by its DOI",
        shape: Shape::Doi,
        pattern: "https://doi.org/{}",
    },
];

/// An identifier as it is shown and as it goes in the address.
struct Id {
    shown: String,
    path: String,
}

impl Shape {
    /// The identifier `token` (lowercased) is, if it has this shape.
    fn read(self, token: &str) -> Option<Id> {
        let digits = |s: &str, most: usize| {
            !s.is_empty() && s.len() <= most && s.bytes().all(|b| b.is_ascii_digit())
        };
        match self {
            Shape::Number(most, width) => {
                if !digits(token, most) {
                    return None;
                }
                let number = token.trim_start_matches('0');
                let number = if number.is_empty() { "0" } else { number };
                Some(Id {
                    shown: number.to_string(),
                    path: format!("{number:0>width$}"),
                })
            }
            Shape::Sequence => {
                let number = token.strip_prefix('a').filter(|n| digits(n, 6))?;
                let id = format!("A{number:0>6}");
                Some(Id {
                    shown: id.clone(),
                    path: id,
                })
            }
            Shape::Entity => {
                let (kind, number) = token.split_at_checked(1)?;
                if !digits(number, 10) || number.starts_with('0') {
                    return None;
                }
                let shown = format!("{}{number}", kind.to_uppercase());
                let path = match kind {
                    "q" => shown.clone(),
                    "p" => format!("Property:{shown}"),
                    _ => return None,
                };
                Some(Id { shown, path })
            }
            Shape::Arxiv => {
                let (month, rest) = token.split_once('.')?;
                let (number, version) = rest.split_once('v').unwrap_or((rest, "1"));
                (digits(month, 4)
                    && month.len() == 4
                    && (4..=5).contains(&number.len())
                    && digits(number, 5)
                    && digits(version, 3))
                .then(|| Id {
                    shown: token.to_string(),
                    path: token.to_string(),
                })
            }
            Shape::Cve => {
                let rest = token.strip_prefix("cve-")?;
                let (year, number) = rest.split_once('-')?;
                (year.len() == 4 && digits(year, 4) && number.len() >= 4 && digits(number, 7)).then(
                    || {
                        let id = token.to_uppercase();
                        Id {
                            shown: id.clone(),
                            path: id,
                        }
                    },
                )
            }
            Shape::Isbn => {
                let isbn: String = token.chars().filter(|c| *c != '-').collect();
                is_isbn(&isbn).then(|| Id {
                    shown: isbn.to_uppercase(),
                    path: isbn.to_uppercase(),
                })
            }
            Shape::Doi => {
                let (prefix, suffix) = token.split_once('/')?;
                let registrant = prefix.strip_prefix("10.")?;
                (digits(registrant, 9) && registrant.len() >= 4 && !suffix.is_empty()).then(|| Id {
                    shown: token.to_string(),
                    path: token.to_string(),
                })
            }
        }
    }

    /// Whether this shape is read anywhere in the query, rather than only
    /// right after its scheme's word: "oeis a000108 catalan numbers",
    /// "wikidata property iso 4217 code p498".
    fn anywhere(self) -> bool {
        !matches!(self, Shape::Number(..))
    }
}

/// Whether `isbn` (digits, and `x` last for ISBN-10) has a right check
/// digit: an ISBN-13 of the 978 or 979 prefixes, or an ISBN-10.
fn is_isbn(isbn: &str) -> bool {
    let values: Vec<u32> = isbn
        .chars()
        .enumerate()
        .map_while(|(i, c)| match c {
            'x' if i == 9 && isbn.len() == 10 => Some(10),
            c => c.to_digit(10),
        })
        .collect();
    if values.len() != isbn.len() {
        return false;
    }
    match values.len() {
        13 if isbn.starts_with("978") || isbn.starts_with("979") => {
            let sum: u32 = values
                .iter()
                .enumerate()
                .map(|(i, v)| if i % 2 == 0 { *v } else { 3 * v })
                .sum();
            sum.is_multiple_of(10)
        }
        10 => {
            let sum: u32 = values
                .iter()
                .enumerate()
                .map(|(i, v)| (10 - i as u32) * v)
                .sum();
            sum.is_multiple_of(11)
        }
        _ => false,
    }
}

/// The records `query` names by their identifiers, in the order it names
/// them, as pages to list first ([`super::keep_page_rules`]).
pub(crate) fn records_named(query: &str) -> Vec<PageHit> {
    let lower = query.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .map(|t| t.trim_matches(|c: char| matches!(c, '(' | ')' | '"' | '?' | '!')))
        .map(|t| t.strip_suffix('.').unwrap_or(t))
        .filter(|t| !t.is_empty())
        .collect();
    let says_isbn = tokens.iter().any(|t| t.starts_with("isbn"));
    let mut found: Vec<(usize, PageHit)> = Vec::new();
    for scheme in SCHEMES {
        let Some((at, id)) = scheme_id(scheme, &tokens, says_isbn) else {
            continue;
        };
        let page = record_page(scheme, &id);
        if !found.iter().any(|(_, hit)| hit.page.url == page.url) {
            found.push((
                at,
                PageHit {
                    page,
                    score: 1.0,
                    named: false,
                    popularity: 0.0,
                    whole: false,
                    learned: None,
                },
            ));
        }
    }
    found.sort_by_key(|(at, _)| *at);
    found
        .into_iter()
        .map(|(_, hit)| hit)
        .take(MAX_RECORDS)
        .collect()
}

/// Where in `tokens` an identifier of `scheme` is, and the identifier.
fn scheme_id(scheme: &Scheme, tokens: &[&str], says_isbn: bool) -> Option<(usize, Id)> {
    let read = |at: usize, token: &str| {
        let id = scheme.shape.read(token)?;
        if matches!(scheme.shape, Shape::Isbn) && id.path.len() == 10 && !says_isbn {
            return None;
        }
        let quantity = matches!(scheme.shape, Shape::Number(..))
            && tokens
                .get(at + 1)
                .is_some_and(|next| QUANTITY_WORDS.contains(next));
        (!quantity).then_some((at, id))
    };
    let Some(word) = scheme.word else {
        // "isbn:9780141439518", "doi:10.1038/nature14539".
        let label = format!("{}:", scheme.name.to_lowercase());
        return tokens
            .iter()
            .enumerate()
            .find_map(|(at, token)| read(at, token.strip_prefix(&label).unwrap_or(token)));
    };
    for (at, token) in tokens.iter().enumerate() {
        let Some(rest) = token.strip_prefix(word) else {
            continue;
        };
        // "rfc3339", "rfc-3339", "arxiv:1706.03762"; not "pepsi".
        let rest = rest.trim_start_matches(['-', ':']);
        if !rest.is_empty() {
            match read(at, rest) {
                Some(found) => return Some(found),
                None => continue,
            }
        }
        let next = tokens.get(at + 1).and_then(|next| read(at + 1, next));
        let anywhere = || {
            tokens
                .iter()
                .enumerate()
                .filter(|_| scheme.shape.anywhere())
                .find_map(|(at, token)| read(at, token))
        };
        if let Some(found) = next.or_else(anywhere) {
            return Some(found);
        }
    }
    None
}

/// The page of the record `id` of `scheme`.
fn record_page(scheme: &Scheme, id: &Id) -> Page {
    let title = if scheme.name.is_empty() {
        id.shown.clone()
    } else {
        format!("{} {}", scheme.name, id.shown)
    };
    Page {
        paper: None,
        set: RECORDS_SET.to_string(),
        url: scheme.pattern.replace("{}", &id.path),
        title,
        description: Some(scheme.description.to_string()),
        site: None,
        views: 0,
        aliases: Vec::new(),
        item: None,
        profiles: Vec::new(),
        website: None,
        package: None,
        facts: Vec::new(),
        lead: None,
        names: Vec::new(),
        sections: Vec::new(),
        content_language: None,
        search: None,
    }
}
