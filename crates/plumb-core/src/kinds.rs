//! What kind of thing a site is ("bank", "airline"), so a query naming a
//! kind, like "banks", can list the sites of that kind.

use crate::normalize_text;

/// Most kinds kept per site.
pub const MAX_KINDS: usize = 6;

/// Kinds that say nothing a searcher would ask for: nearly every official
/// site is one of these.
const GENERIC_KINDS: &[&str] = &[
    "brand",
    "business",
    "company",
    "corporation",
    "enterprise",
    "holding company",
    "juridical person",
    "legal person",
    "organisation",
    "organization",
    "private company",
    "public company",
    "subsidiary",
    "web page",
    "website",
];

/// The matching key of a kind or of a query naming one: normalized, each
/// word made singular, spaces removed. `Banks` -> `bank`, `Credit Unions`
/// -> `creditunion`, `airline companies` -> `airlinecompany`.
pub fn kind_key(text: &str) -> String {
    normalize_text(text)
        .split(' ')
        .map(singular)
        .collect::<Vec<_>>()
        .concat()
}

/// Whether `kind` is too general to be worth keeping ([`GENERIC_KINDS`]).
pub fn is_generic_kind(kind: &str) -> bool {
    let key = kind_key(kind);
    key.is_empty() || GENERIC_KINDS.iter().any(|generic| kind_key(generic) == key)
}

/// Words whose singular is not the word less its `s`.
const SAME_IN_BOTH: &[&str] = &["news", "series", "species", "means", "sports", "always"];

/// The same word in the other number, singular for plural and plural for
/// singular, so a query for "videos" finds "video" and the other way
/// round: `videos` -> `video`, `video` -> `videos`, `company` ->
/// `companies`, `church` -> `churches`. `None` for short words, words that
/// are not plain ASCII letters, and words like "news" with no other number.
pub fn other_number(word: &str) -> Option<String> {
    if word.len() <= 3 || !word.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    if SAME_IN_BOTH.contains(&word) {
        return None;
    }
    let one = singular(word);
    if one != word {
        return (one.len() > 2).then_some(one);
    }
    if word.ends_with("ss") || word.ends_with("us") || word.ends_with("is") {
        return None;
    }
    let len = word.len();
    let before_y = word.as_bytes()[len - 2];
    let plural = if word.ends_with('y') && !b"aeiou".contains(&before_y) {
        format!("{}ies", &word[..len - 1])
    } else if ["s", "x", "z", "ch", "sh"]
        .iter()
        .any(|end| word.ends_with(end))
    {
        format!("{word}es")
    } else {
        format!("{word}s")
    };
    // Only a form that comes back to the word is safe to search for.
    (singular(&plural) == word).then_some(plural)
}

/// A rough English singular, the same on both sides of a match:
/// `banks` -> `bank`, `companies` -> `company`, `churches` -> `church`,
/// `glasses` -> `glass`. Words ending in `ss`, `us` or `is`, and short
/// words, stay as they are.
fn singular(word: &str) -> String {
    let len = word.len();
    if len <= 3 || !word.is_ascii() {
        return word.to_string();
    }
    if let Some(stem) = word.strip_suffix("ies") {
        if len > 4 {
            return format!("{stem}y");
        }
    }
    for ending in ["sses", "ches", "shes", "xes", "zes"] {
        if word.ends_with(ending) {
            return word[..len - 2].to_string();
        }
    }
    if word.ends_with('s')
        && !word.ends_with("ss")
        && !word.ends_with("us")
        && !word.ends_with("is")
    {
        return word[..len - 1].to_string();
    }
    word.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_have_another_number() {
        let other = |word: &str| other_number(word);
        assert_eq!(other("videos").as_deref(), Some("video"));
        assert_eq!(other("video").as_deref(), Some("videos"));
        assert_eq!(other("company").as_deref(), Some("companies"));
        assert_eq!(other("companies").as_deref(), Some("company"));
        assert_eq!(other("church").as_deref(), Some("churches"));
        assert_eq!(other("games").as_deref(), Some("game"));
        assert_eq!(other("journey").as_deref(), Some("journeys"));
        for word in ["news", "campus", "glass", "bus", "car", "café", "mp3s"] {
            assert_eq!(other(word), None, "{word}");
        }
    }

    #[test]
    fn plurals_match_singulars() {
        assert_eq!(kind_key("Banks"), "bank");
        assert_eq!(kind_key("bank"), "bank");
        assert_eq!(kind_key("Credit Unions"), "creditunion");
        assert_eq!(kind_key("airline companies"), "airlinecompany");
        assert_eq!(kind_key("churches"), "church");
        assert_eq!(kind_key("glasses"), "glass");
        assert_eq!(kind_key("bus"), "bus");
        assert_eq!(kind_key("campus"), "campus");
        assert_eq!(kind_key("news websites"), kind_key("news website"));
    }

    #[test]
    fn generic_kinds_are_dropped() {
        assert!(is_generic_kind("Public company"));
        assert!(is_generic_kind("businesses"));
        assert!(is_generic_kind("  "));
        assert!(!is_generic_kind("bank"));
        assert!(!is_generic_kind("airline"));
    }
}
