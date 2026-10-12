//! Refs: how one item names another, written `<scheme>:<value>`.
//!
//! | Ref | Names |
//! |---|---|
//! | `wd:Q408` | every item about that Wikidata item (`Q`, `P` or `L` ids) |
//! | `site:python.org` | a site, by its domain |
//! | `url:https://docs.python.org/3/` | a page, by its canonical URL ([`crate::urls`]) |
//! | `id:doi:10.1000/xyz` | a thing by an outside id: `<namespace>:<value>` |
//! | `item:<publisher>/<set>/<id>` | exactly one item of one set |
//! | `blob:sha256-<hex>` | bytes in a node's blob store, by their hash |
//!
//! In an `item:` ref each of the three parts is percent-encoded, so a `/`
//! inside one (set names are `<scope>/<name>`) is `%2F` and a `%` is `%25`.
//! The schemes are a closed list: a new one goes in a manifest's `requires`.

use std::fmt;
use std::str::FromStr;

use anyhow::{bail, ensure, Context, Error, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::urls::is_canonical;

/// A reference from one item to another thing.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Ref {
    /// A Wikidata item, property or lexeme: `Q408`, `P36`.
    Wd(String),
    /// A site: its domain, lower case, in ASCII (`xn--` for IDNs).
    Site(String),
    /// A page: its canonical URL.
    Url(String),
    /// A thing by an outside id: the namespace (`doi`, `pypi`, `osm`) and
    /// the id within it.
    Id { namespace: String, value: String },
    /// Exactly one item: its publisher's key, its set and its id there.
    Item {
        publisher: String,
        set: String,
        id: String,
    },
    /// Bytes, by the hex SHA-256 of their content.
    Blob(String),
}

impl FromStr for Ref {
    type Err = Error;

    fn from_str(text: &str) -> Result<Ref> {
        let (scheme, value) = text
            .split_once(':')
            .with_context(|| format!("ref {text:?} has no scheme"))?;
        ensure!(!value.is_empty(), "ref {text:?} is empty");
        Ok(match scheme {
            "wd" => {
                let mut chars = value.chars();
                ensure!(
                    matches!(chars.next(), Some('Q' | 'P' | 'L'))
                        && value.len() > 1
                        && chars.all(|c| c.is_ascii_digit())
                        && !value[1..].starts_with('0'),
                    "{value:?} is not a Wikidata id"
                );
                Ref::Wd(value.to_string())
            }
            "site" => {
                ensure!(is_domain(value), "{value:?} is not a site's domain");
                Ref::Site(value.to_string())
            }
            "url" => {
                ensure!(is_canonical(value), "{value:?} is not a canonical URL");
                Ref::Url(value.to_string())
            }
            "id" => {
                let (namespace, id) = value
                    .split_once(':')
                    .with_context(|| format!("id ref {value:?} has no namespace"))?;
                ensure!(
                    is_namespace(namespace),
                    "{namespace:?} is not an id namespace"
                );
                ensure!(!id.is_empty(), "id ref {value:?} has no value");
                Ref::Id {
                    namespace: namespace.to_string(),
                    value: id.to_string(),
                }
            }
            "item" => {
                let parts: Vec<&str> = value.split('/').collect();
                let [publisher, set, id] = parts[..] else {
                    bail!("item ref {value:?} is not <publisher>/<set>/<id>");
                };
                let (publisher, set, id) = (decode(publisher)?, decode(set)?, decode(id)?);
                ensure!(
                    !publisher.is_empty() && !set.is_empty() && !id.is_empty(),
                    "item ref {value:?} has an empty part"
                );
                Ref::Item { publisher, set, id }
            }
            "blob" => {
                let hex = value
                    .strip_prefix("sha256-")
                    .with_context(|| format!("blob ref {value:?} is not sha256-<hex>"))?;
                ensure!(
                    hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                    "blob ref {value:?} is not sha256-<64 lower-case hex digits>"
                );
                Ref::Blob(hex.to_string())
            }
            _ => bail!("unknown ref scheme {scheme:?}"),
        })
    }
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ref::Wd(id) => write!(f, "wd:{id}"),
            Ref::Site(domain) => write!(f, "site:{domain}"),
            Ref::Url(url) => write!(f, "url:{url}"),
            Ref::Id { namespace, value } => write!(f, "id:{namespace}:{value}"),
            Ref::Item { publisher, set, id } => {
                write!(
                    f,
                    "item:{}/{}/{}",
                    encode(publisher),
                    encode(set),
                    encode(id)
                )
            }
            Ref::Blob(hex) => write!(f, "blob:sha256-{hex}"),
        }
    }
}

impl Serialize for Ref {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Ref {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Ref, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A domain as sites are named: ASCII letters, digits and hyphens in
/// lower case, in at least two labels, none empty or starting or ending
/// with a hyphen.
pub fn is_domain(text: &str) -> bool {
    text.len() <= 253
        && text.contains('.')
        && text.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// An id namespace's name, as in an item's `ids` keys: lower-case ASCII
/// letters, digits and hyphens, starting with a letter.
pub fn is_namespace(text: &str) -> bool {
    text.len() <= 32
        && text.starts_with(|c: char| c.is_ascii_lowercase())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn encode(part: &str) -> String {
    part.replace('%', "%25").replace('/', "%2F")
}

fn decode(part: &str) -> Result<String> {
    let mut out = String::with_capacity(part.len());
    let mut rest = part;
    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);
        match rest.get(at + 1..at + 3) {
            Some("2F") => out.push('/'),
            Some("25") => out.push('%'),
            _ => bail!("{part:?} has a % that is not %2F or %25"),
        }
        rest = &rest[at + 3..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for text in [
            "wd:Q408",
            "wd:P36",
            "site:python.org",
            "site:xn--bcher-kva.de",
            "url:https://docs.python.org/3/",
            "id:doi:10.1000/xyz",
            "id:pypi:requests",
            "id:osm:node/123",
            "item:ed25519:AbC_-9/plumb%2Fwikipedia-en/1234",
            "item:ed25519:k/liz%2Fnotes/a%25b%2Fc",
            "blob:sha256-9f2c000000000000000000000000000000000000000000000000000000000000",
        ] {
            let parsed: Ref = text.parse().unwrap();
            assert_eq!(parsed.to_string(), text);
        }
    }

    #[test]
    fn item_parts_keep_their_slashes() {
        let parsed: Ref = "item:ed25519:k/plumb%2Fwikipedia-en/a%2Fb".parse().unwrap();
        assert_eq!(
            parsed,
            Ref::Item {
                publisher: "ed25519:k".into(),
                set: "plumb/wikipedia-en".into(),
                id: "a/b".into(),
            }
        );
    }

    #[test]
    fn rejects() {
        for text in [
            "Q408",
            "wd:",
            "wd:Q",
            "wd:Q0408",
            "wd:X1",
            "site:Python.org",
            "site:localhost",
            "site:-a.com",
            "url:https://Example.com/",
            "url:magnet:?xt=urn:btih:abc",
            "id:doi",
            "id:DOI:10.1/x",
            "id:doi:",
            "item:k/plumb/wikipedia-en/1",
            "item:k/s",
            "item:k/s%41/1",
            "blob:sha256-ABC",
            "blob:md5-00",
            "nope:x",
        ] {
            assert!(text.parse::<Ref>().is_err(), "{text}");
        }
    }

    #[test]
    fn serde_as_strings() {
        let parsed: Vec<Ref> = serde_json::from_str(r#"["wd:Q408","site:python.org"]"#).unwrap();
        assert_eq!(parsed[0], Ref::Wd("Q408".into()));
        assert_eq!(
            serde_json::to_string(&parsed).unwrap(),
            r#"["wd:Q408","site:python.org"]"#
        );
        assert!(serde_json::from_str::<Ref>(r#""wd:Q0""#).is_err());
    }
}
