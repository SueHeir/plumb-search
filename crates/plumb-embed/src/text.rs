//! The text of a site that is embedded.

use std::collections::HashSet;

use plumb_core::{collapse_whitespace, SiteRecord};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// Most words of a site's text that are embedded.
pub const MAX_TEXT_WORDS: usize = 100;
/// Most link texts (other sites' words for the site, most used first) in a
/// site's text.
pub const MAX_LINK_TEXTS: usize = 5;

/// SHA-256 of a site's text ([`text_hash`]).
pub type TextHash = [u8; 32];

/// The text of `record` to embed: its names, homepage title and
/// description, what Wikidata and Wikipedia say the organization is (or,
/// for a site with none of that, a model's one-sentence summary), the
/// [`MAX_LINK_TEXTS`] words other sites link to it with most, its homepage
/// headings and the start of its homepage text, in that order, joined by ". " and cut at [`MAX_TEXT_WORDS`]
/// words. Each part is put in Unicode NFC and its whitespace collapsed, and
/// a part equal to an earlier one (ignoring case) is left out. The same
/// record always gives the same text; a record with none of these gives an
/// empty one.
pub fn site_text(record: &SiteRecord) -> String {
    site_text_words(record, MAX_TEXT_WORDS)
}

/// [`site_text`] cut at `words` words instead of [`MAX_TEXT_WORDS`], for
/// models that read longer texts.
pub fn site_text_words(record: &SiteRecord, words: usize) -> String {
    let link_texts = record
        .link_texts
        .iter()
        .take(MAX_LINK_TEXTS)
        .map(|link| &link.text);
    let summary = record.summary.as_ref().filter(|_| {
        record.description.is_none() && record.about.is_none() && record.intro.is_none()
    });
    let parts = record
        .aliases
        .iter()
        .chain(&record.title)
        .chain(&record.description)
        .chain(&record.about)
        .chain(&record.intro)
        .chain(summary)
        .chain(link_texts)
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
    let kept: Vec<&str> = text.split_whitespace().take(words).collect();
    kept.join(" ")
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

        // Other sites' words for a site give it text before any crawl.
        let mut linked = SiteRecord::new("chicagotribune.com");
        linked.link_texts = ["Chicago Tribune", "chicago news", "a", "b", "c", "d"]
            .iter()
            .map(|text| plumb_core::LinkText::from_linkers(*text, 1))
            .collect();
        assert_eq!(site_text(&linked), "Chicago Tribune. chicago news. a. b. c");
    }
}
