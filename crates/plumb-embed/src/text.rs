//! The text of a site that is embedded.

use std::collections::HashSet;

use plumb_core::{collapse_whitespace, SiteRecord};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// Most words of a site's text that are embedded.
pub const MAX_TEXT_WORDS: usize = 100;

/// SHA-256 of a site's text ([`text_hash`]).
pub type TextHash = [u8; 32];

/// The text of `record` to embed: its names, homepage title and
/// description, what Wikidata says the organization is, its homepage
/// headings and the start of its homepage text, in that order, joined by ". " and cut at [`MAX_TEXT_WORDS`]
/// words. Each part is put in Unicode NFC and its whitespace collapsed, and
/// a part equal to an earlier one (ignoring case) is left out. The same
/// record always gives the same text; a record with none of these gives an
/// empty one.
pub fn site_text(record: &SiteRecord) -> String {
    let parts = record
        .aliases
        .iter()
        .chain(&record.title)
        .chain(&record.description)
        .chain(&record.about)
        .chain(&record.headings)
        .chain(&record.body_text);
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    for part in parts {
        let part = collapse_whitespace(&part.nfc().collect::<String>());
        if !part.is_empty() && seen.insert(part.to_lowercase()) {
            kept.push(part);
        }
    }
    let text = kept.join(". ");
    let words: Vec<&str> = text.split_whitespace().take(MAX_TEXT_WORDS).collect();
    words.join(" ")
}

/// SHA-256 of `text`, so nodes can tell whether they embedded the same text.
pub fn text_hash(text: &str) -> TextHash {
    Sha256::digest(text.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_text_is_fixed_and_short() {
        let mut record = SiteRecord::new("tesla.com");
        record.aliases = vec!["Tesla".into(), "Tesla, Inc.".into()];
        record.title = Some("Electric  Cars,\tSolar & Clean Energy | Tesla".into());
        record.description = Some("tesla".into());
        record.about = Some("American electric vehicle and clean energy company".into());
        record.headings = vec!["Model 3".into(), "Model Y".into()];
        assert_eq!(
            site_text(&record),
            "Tesla. Tesla, Inc.. Electric Cars, Solar & Clean Energy | Tesla. \
             American electric vehicle and clean energy company. Model 3. Model Y"
        );
        assert_eq!(
            text_hash(&site_text(&record)),
            text_hash(&site_text(&record.clone()))
        );

        // Composed and decomposed accents give the same text.
        let mut composed = SiteRecord::new("cafe.fr");
        composed.title = Some("Caf\u{e9}".into());
        let mut decomposed = composed.clone();
        decomposed.title = Some("Cafe\u{301}".into());
        assert_eq!(site_text(&composed), site_text(&decomposed));

        record.headings = vec!["word ".repeat(500)];
        assert_eq!(
            site_text(&record).split_whitespace().count(),
            MAX_TEXT_WORDS
        );
        assert_eq!(site_text(&SiteRecord::new("empty.com")), "");
    }
}
