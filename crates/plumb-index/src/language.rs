//! Whether a search is written in another language than the one chosen,
//! judged by the language the sites using its words declare.

use tantivy::schema::IndexRecordOption;
use tantivy::{DocSet, Term, TERMINATED};

use crate::{analysis, language_code, schema, SearchOptions, Searcher};

/// Most sites read for one word. A site declares its language about one
/// time in eight, so this leaves hundreds for a common word.
const SITES_READ: u32 = 4_000;

/// Fewest sites declaring a language that a word is judged by.
const MIN_DECLARED: u32 = 10;

/// A word at least this share of whose sites declare the chosen language
/// is a word of it: on hpc "hotels" is 0.79 English, "weather" 0.99,
/// "tokyo" 0.55. About 0.6 of all sites that declare are English.
const CHOSEN_SHARE: u32 = 50;

/// A word at most this share of whose sites declare the chosen language
/// is a word of another: "wetter" 0 English, "zeitung" 0.01, "lyon"
/// 0.10, "nacional" 0.05, "de" 0.07. Names shared by many languages
/// ("paris" 0.24, "berlin" 0.29) are neither.
const OTHER_SHARE: u32 = 15;

impl Searcher {
    /// [`SearchOptions::language`] for `query`: none when the query is
    /// written in a script the language is not ([`SearchOptions::language_for`]),
    /// or in another language that uses its letters. That is when a word of
    /// the query is found almost only on sites in other languages and none
    /// mostly on sites in the chosen one: "wetter berlin" or "sueddeutsche
    /// zeitung" with English chosen, not "hotels lyon".
    pub fn language_for(&self, options: &SearchOptions, query: &str) -> Option<String> {
        let wanted = options.language_for(query).and_then(language_code)?;
        let mut other = false;
        for word in analysis::tokens(&self.words, query) {
            if !word.chars().any(char::is_alphabetic) {
                continue;
            }
            let (chosen, declared) = self.declared(&word, &wanted);
            if declared < MIN_DECLARED {
                continue;
            }
            if chosen * 100 >= declared * CHOSEN_SHARE {
                return Some(wanted);
            }
            other |= chosen * 100 <= declared * OTHER_SHARE;
        }
        (!other).then_some(wanted)
    }

    /// Of the sites with `word` in their title or description (at most
    /// [`SITES_READ`]), how many declare `language`, and how many declare
    /// any.
    fn declared(&self, word: &str, language: &str) -> (u32, u32) {
        let searcher = self.reader.searcher();
        let (mut chosen, mut declared, mut read) = (0, 0, 0);
        for segment in searcher.segment_readers() {
            let Ok(Some(languages)) = segment.fast_fields().str(schema::LANGUAGE) else {
                continue;
            };
            let wanted = languages.dictionary().term_ord(language).ok().flatten();
            for field in [self.fields.title, self.fields.description] {
                let term = Term::from_field_text(field, word);
                let Ok(Some(mut postings)) = segment
                    .inverted_index(field)
                    .and_then(|index| Ok(index.read_postings(&term, IndexRecordOption::Basic)?))
                else {
                    continue;
                };
                let mut doc = postings.doc();
                while doc != TERMINATED && read < SITES_READ {
                    if let Some(ord) = languages.term_ords(doc).next() {
                        declared += 1;
                        chosen += u32::from(Some(ord) == wanted);
                    }
                    read += 1;
                    doc = postings.advance();
                }
            }
        }
        (chosen, declared)
    }
}
