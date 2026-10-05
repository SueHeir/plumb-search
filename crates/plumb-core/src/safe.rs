//! Safe search: leaving adult sites out of results.
//!
//! Plumb knows a site by its name, homepage title and description, and
//! what Wikidata says it is, so that is what [`adult_level`] reads. Nodes
//! also leave out the domains of an adult blocklist (see
//! [`ADULT_LIST_URL`]), which this module only parses.

use serde::{Deserialize, Serialize};

use crate::{domain_label, kind_key, normalize_text, registrable_domain, SiteRecord};

/// How much a search leaves out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SafeSearch {
    /// Leave nothing out.
    Off,
    /// Leave out sites that are plainly adult: on the blocklist, of an
    /// adult kind, or saying so in their name, title or description.
    #[default]
    Moderate,
    /// Also leave out sites whose text is only suggestive ("sexy",
    /// "nude", "escort"), and pages that are.
    Strict,
}

impl SafeSearch {
    /// Reads `off`, `moderate` or `strict` (any case); `None` otherwise.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "0" => Some(SafeSearch::Off),
            "moderate" | "on" | "1" => Some(SafeSearch::Moderate),
            "strict" | "2" => Some(SafeSearch::Strict),
            _ => None,
        }
    }

    /// The name used in addresses and settings: `off`, `moderate`, `strict`.
    pub fn as_str(self) -> &'static str {
        match self {
            SafeSearch::Off => "off",
            SafeSearch::Moderate => "moderate",
            SafeSearch::Strict => "strict",
        }
    }

    /// Whether a result whose [`adult_level`] is `level` is left out.
    pub fn hides(self, level: AdultLevel) -> bool {
        match self {
            SafeSearch::Off => false,
            SafeSearch::Moderate => level == AdultLevel::Explicit,
            SafeSearch::Strict => level != AdultLevel::None,
        }
    }
}

/// How adult a site or page looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AdultLevel {
    None,
    /// Suggestive words only.
    Suggestive,
    /// Plainly adult.
    Explicit,
}

/// Where nodes download the adult blocklist: the Block List Project's
/// porn list (public domain), one domain per line.
pub const ADULT_LIST_URL: &str =
    "https://raw.githubusercontent.com/blocklistproject/Lists/master/alt-version/porn-nl.txt";

/// Words that make a site plainly adult wherever they appear.
const EXPLICIT_WORDS: &[&str] = &[
    "porn",
    "porno",
    "pornography",
    "pornographic",
    "xxx",
    "hentai",
    "nsfw",
    "bdsm",
    "camgirl",
    "camgirls",
    "sexcam",
    "sexcams",
    "milf",
    "milfs",
    "xvideos",
    "xhamster",
    "onlyfans",
];

/// Parts of a domain label that make the site plainly adult: labels run
/// words together (`freeporn`, `xxxvideos`).
const EXPLICIT_LABEL_PARTS: &[&str] = &["porn", "xxx", "hentai", "sexcam", "camgirl"];

/// Words that are only suggestive.
const SUGGESTIVE_WORDS: &[&str] = &[
    "sex",
    "sexy",
    "nude",
    "nudes",
    "naked",
    "nudity",
    "erotic",
    "erotica",
    "escort",
    "escorts",
    "fetish",
    "lingerie",
    "stripper",
    "strippers",
    "hookup",
    "hookups",
    "playboy",
    "swingers",
    "boobs",
];

/// Wikidata kinds of adult sites, as [`kind_key`]s.
const ADULT_KINDS: &[&str] = &[
    "pornographic website",
    "pornographic film studio",
    "pornographic magazine",
    "pornography",
    "adult website",
    "pornographic video sharing website",
    "webcam model",
];

/// The [`kind_key`]s of Wikidata kinds of adult sites, for an index to
/// look up.
pub fn adult_kind_keys() -> impl Iterator<Item = String> {
    ADULT_KINDS.iter().map(|kind| kind_key(kind))
}

/// Whether `kind` (a Wikidata kind of a site) is an adult one.
pub fn is_adult_kind(kind: &str) -> bool {
    let key = kind_key(kind);
    adult_kind_keys().any(|adult| adult == key)
}

/// How adult the site `domain` looks from its `texts` (title,
/// description, Wikidata's description, other names).
pub fn adult_level<'a>(domain: &str, texts: impl IntoIterator<Item = &'a str>) -> AdultLevel {
    let label = domain_label(domain);
    if EXPLICIT_LABEL_PARTS.iter().any(|part| label.contains(part)) {
        return AdultLevel::Explicit;
    }
    let mut level = text_level(&label);
    for text in texts {
        level = level.max(text_level(text));
        if level == AdultLevel::Explicit {
            break;
        }
    }
    level
}

/// How adult the site of `record` looks: [`AdultLevel::Explicit`] for
/// an adult Wikidata kind, else [`adult_level`] of its title, description,
/// Wikidata description and other names.
pub fn record_adult_level(record: &SiteRecord) -> AdultLevel {
    if record.kinds.iter().any(|kind| is_adult_kind(kind)) {
        return AdultLevel::Explicit;
    }
    let texts = [&record.title, &record.description, &record.about]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .chain(record.aliases.iter().map(String::as_str));
    adult_level(&record.domain, texts)
}

/// [`adult_level`] of a text alone.
fn text_level(text: &str) -> AdultLevel {
    let mut level = AdultLevel::None;
    for word in normalize_text(text).split(' ') {
        if EXPLICIT_WORDS.contains(&word) {
            return AdultLevel::Explicit;
        }
        if SUGGESTIVE_WORDS.contains(&word) {
            level = AdultLevel::Suggestive;
        }
    }
    level
}

/// The registrable domains of an adult blocklist in `text`: one domain
/// per line, or hosts-file lines (`0.0.0.0 example.com`); `#` comments.
/// Only lines that name a whole site count: `someone.tumblr.com` is a
/// page on tumblr.com, which stays.
pub fn parse_adult_list(text: &str) -> Vec<String> {
    let mut domains: Vec<String> = text
        .lines()
        .filter_map(|line| {
            let line = line.split('#').next()?.trim();
            let host = line.split_whitespace().last()?;
            let host = host.strip_prefix("www.").unwrap_or(host);
            let domain = registrable_domain(host)?;
            (domain == host.to_ascii_lowercase()).then_some(domain)
        })
        .collect();
    domains.sort_unstable();
    domains.dedup();
    domains
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_setting() {
        assert_eq!(SafeSearch::parse("Strict"), Some(SafeSearch::Strict));
        assert_eq!(SafeSearch::parse("off"), Some(SafeSearch::Off));
        assert_eq!(SafeSearch::parse("bogus"), None);
        assert_eq!(SafeSearch::default(), SafeSearch::Moderate);
        for level in [SafeSearch::Off, SafeSearch::Moderate, SafeSearch::Strict] {
            assert_eq!(SafeSearch::parse(level.as_str()), Some(level));
        }
    }

    #[test]
    fn levels_from_names_and_text() {
        assert_eq!(adult_level("freeporn.example", []), AdultLevel::Explicit);
        assert_eq!(
            adult_level("example.com", ["Hot XXX videos"]),
            AdultLevel::Explicit
        );
        assert_eq!(
            adult_level("example.com", ["Sexy lingerie for every day"]),
            AdultLevel::Suggestive
        );
        assert_eq!(
            adult_level("essex.ac.uk", ["University of Essex"]),
            AdultLevel::None
        );
        assert_eq!(
            adult_level("chase.com", ["Credit cards, mortgages"]),
            AdultLevel::None
        );
        assert!(is_adult_kind("Pornographic websites"));
        assert!(!is_adult_kind("bank"));
    }

    #[test]
    fn what_each_setting_hides() {
        use AdultLevel::*;
        assert!(!SafeSearch::Off.hides(Explicit));
        assert!(SafeSearch::Moderate.hides(Explicit));
        assert!(!SafeSearch::Moderate.hides(Suggestive));
        assert!(SafeSearch::Strict.hides(Suggestive));
        assert!(!SafeSearch::Strict.hides(None));
    }

    #[test]
    fn blocklists_keep_whole_sites_only() {
        let list = "# Title: test\n\n0.0.0.0 adult.example\nsomeone.tumblr.com\n\
                    www.other.example # note\nbad line here!\nadult.example\n";
        assert_eq!(parse_adult_list(list), ["adult.example", "other.example"]);
    }
}
