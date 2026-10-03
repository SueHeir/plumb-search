//! Typo correction: "amazom" finds amazon.com, "bank of amercia" finds
//! bankofamerica.com.
//!
//! A query that names no site in full is checked for typos in two ways:
//!
//! - **Names.** The query's first words, joined (`bankofamercia`), are
//!   looked up among the sites' names (`label_key` and `alias_key`) within
//!   an edit distance that grows with their length ([`max_edits`]): none
//!   up to 3 letters, 1 up to 7, then 2. A swap of two neighbouring
//!   letters counts as one edit. Only a site with a link score of at least
//!   [`MIN_FIX_LINK_SCORE`] can be the correction: look-alike sites have no
//!   popularity to show, so a typo never leads to them.
//! - **Words.** Any other word that hardly appears in the index (in fewer
//!   than [`KNOWN_WORD_DOCS`] sites) is replaced by the nearest word that
//!   appears in many more ([`FIX_DOCS_RATIO`] times as many, and at least
//!   [`MIN_FIX_DOCS`]).
//!
//! A word that the index knows is never changed, so "pizza" stays "pizza"
//! even though piazza is one letter away, except as part of a name of
//! several words that a well-known site has ("capitol one"). The search then runs again with
//! the corrected query, and [`crate::Searcher::search_meaning`] decides
//! whether to show its results or only suggest it.

use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

use anyhow::Result;
use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder, DFA, SINK_STATE};
use tantivy::schema::Field;
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{DocAddress, Term};
use tantivy_fst::Automaton;

use crate::schema::Fields;
use crate::{analysis, matching_docs, MAX_QUERY_WORDS, WELL_KNOWN_LINK_SCORE};

/// The least link score a site needs to be what a typo is corrected to:
/// roughly a site in the top million with some sites linking to it.
pub(crate) const MIN_FIX_LINK_SCORE: f32 = 0.3;
/// A word found in this many sites or more is a known word and is never
/// corrected.
pub(crate) const KNOWN_WORD_DOCS: u64 = 20;
/// A word is only corrected to a word found in at least this many sites...
pub(crate) const MIN_FIX_DOCS: u64 = 3;
/// ...and in this many times as many sites as the word typed.
pub(crate) const FIX_DOCS_RATIO: u64 = 20;
/// Most leading words joined into one name when looking for a misspelled
/// name. Names are rarely longer.
const MAX_NAME_WORDS: usize = 6;
/// Most near terms read from one field's dictionary for one word.
const MAX_NEAR_TERMS: usize = 256;

/// What a query looks like with its typos fixed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fix {
    /// The corrected query, in normalized words.
    pub(crate) query: String,
    /// The best link score among the sites the corrected name names, when
    /// a name was corrected.
    pub(crate) name_link_score: Option<f32>,
}

/// Edits allowed in a word or name of `chars` characters.
pub(crate) fn max_edits(chars: usize) -> u8 {
    match chars {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    }
}

/// The corrected form of `query`, if it has typos to fix. The first
/// `named_words` words name a site exactly and stay as they are; a name
/// correction must cover more words than that. `link_score` gives the
/// best link score among a set of documents.
pub(crate) fn correct(
    searcher: &tantivy::Searcher,
    fields: &Fields,
    analyzer: &TextAnalyzer,
    query: &str,
    named_words: usize,
    link_score: &dyn Fn(&HashSet<DocAddress>) -> f32,
) -> Result<Option<Fix>> {
    let speller = Speller { searcher, fields };
    let mut tokens = analysis::tokens(analyzer, query);
    tokens.truncate(MAX_QUERY_WORDS);
    if tokens.is_empty() {
        return Ok(None);
    }
    let mut fixed: Vec<String> = Vec::with_capacity(tokens.len());
    let mut changed = false;
    let mut name_link_score = None;
    let rest = match speller.fix_name(&tokens, named_words + 1, link_score)? {
        Some(name) => {
            fixed.extend(name.words);
            name_link_score = Some(name.link_score);
            changed = true;
            name.covers
        }
        None => {
            fixed.extend(tokens.iter().take(named_words).cloned());
            named_words.min(tokens.len())
        }
    };
    for word in &tokens[rest..] {
        match speller.fix_word(word)? {
            Some(better) => {
                fixed.push(better);
                changed = true;
            }
            None => fixed.push(word.clone()),
        }
    }
    Ok(changed.then(|| Fix {
        query: fixed.join(" "),
        name_link_score,
    }))
}

/// A corrected name.
struct NameFix {
    /// How many of the query's first words it replaces.
    covers: usize,
    /// The corrected words that replace them.
    words: Vec<String>,
    link_score: f32,
}

struct Speller<'a> {
    searcher: &'a tantivy::Searcher,
    fields: &'a Fields,
}

impl Speller<'_> {
    /// The fields whose words count for whether a word is known.
    fn word_fields(&self) -> [Field; 5] {
        let f = self.fields;
        [f.label, f.aliases, f.title, f.anchors, f.about]
    }

    /// In how many sites `word` is found, in the fields of
    /// [`Speller::word_fields`] together (a site counts once per field).
    fn docs_with(&self, word: &str) -> Result<u64> {
        let mut docs = 0;
        for field in self.word_fields() {
            docs += self
                .searcher
                .doc_freq(&Term::from_field_text(field, word))?;
        }
        Ok(docs)
    }

    /// The best correction of the query's first words (at least
    /// `min_words` of them) to the name of a popular site: most words
    /// first, then fewest edits, then most popular.
    fn fix_name(
        &self,
        tokens: &[String],
        min_words: usize,
        link_score: &dyn Fn(&HashSet<DocAddress>) -> f32,
    ) -> Result<Option<NameFix>> {
        let longest = tokens.len().min(MAX_NAME_WORDS);
        for covers in (min_words.max(1)..=longest).rev() {
            let words = &tokens[..covers];
            let key: String = words.concat();
            let edits = max_edits(key.chars().count());
            if edits == 0 {
                continue;
            }
            // (edits, first letter changed, link score, name).
            let mut found: Vec<(u8, bool, f32, String)> = Vec::new();
            for field in [self.fields.label_key, self.fields.alias_key] {
                for (name, distance) in near_terms(self.searcher, field, &key, edits)? {
                    let docs = matching_docs(
                        self.searcher,
                        vec![Term::from_field_text(self.fields.label_key, &name)],
                    )?
                    .into_iter()
                    .chain(matching_docs(
                        self.searcher,
                        vec![Term::from_field_text(self.fields.alias_key, &name)],
                    )?)
                    .collect();
                    let score = link_score(&docs);
                    if score >= MIN_FIX_LINK_SCORE {
                        let first_changed = name.chars().next() != key.chars().next();
                        found.push((distance, first_changed, score, name));
                    }
                }
            }
            // Fewest edits first; then a name with the same first letter,
            // since typos rarely start a word ("fedx" is fedex, not edx);
            // then the most popular.
            found.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| a.1.cmp(&b.1))
                    .then_with(|| b.2.total_cmp(&a.2))
                    .then_with(|| a.3.cmp(&b.3))
            });
            for (_, _, score, name) in found {
                // Each word may only be misspelled as much as its length
                // allows, and none may vanish: "americanairlines fr" is not
                // a typo of "americanairlines".
                let Some(fixed) = align(words, &name) else {
                    continue;
                };
                // A known word is what was meant, not a typo: "pizza" is
                // not "piazza". In a name of several words that a
                // well-known site has, it may be: "capitol one", "wels
                // fargo".
                let known_words_ok = covers > 1 && score >= WELL_KNOWN_LINK_SCORE;
                let mut plausible = true;
                for (word, fixed) in words.iter().zip(&fixed) {
                    if word == fixed {
                        continue;
                    }
                    if !within_edits(word, fixed)
                        || (!known_words_ok && self.docs_with(word)? >= KNOWN_WORD_DOCS)
                    {
                        plausible = false;
                        break;
                    }
                }
                if plausible {
                    return Ok(Some(NameFix {
                        covers,
                        words: fixed,
                        link_score: score,
                    }));
                }
            }
        }
        Ok(None)
    }

    /// The word `word` was probably meant to be, when the index hardly
    /// knows it and knows a near word far better.
    fn fix_word(&self, word: &str) -> Result<Option<String>> {
        let edits = max_edits(word.chars().count());
        if edits == 0 || word.chars().all(|c| c.is_ascii_digit()) {
            return Ok(None);
        }
        let docs = self.docs_with(word)?;
        if docs >= KNOWN_WORD_DOCS {
            return Ok(None);
        }
        let mut near: BTreeMap<String, u8> = BTreeMap::new();
        for field in self.word_fields() {
            for (term, distance) in near_terms(self.searcher, field, word, edits)? {
                near.insert(term, distance);
            }
        }
        let needed = MIN_FIX_DOCS.max(FIX_DOCS_RATIO.saturating_mul(docs));
        let mut best: Option<(u8, u64, String)> = None;
        for (term, distance) in near {
            let term_docs = self.docs_with(&term)?;
            if term_docs < needed {
                continue;
            }
            let better = match &best {
                None => true,
                Some((best_distance, best_docs, _)) => {
                    (distance, std::cmp::Reverse(term_docs))
                        < (*best_distance, std::cmp::Reverse(*best_docs))
                }
            };
            if better {
                best = Some((distance, term_docs, term));
            }
        }
        Ok(best.map(|(_, _, term)| term))
    }
}

/// The terms of `field` within `edits` edits of `key`, `key` itself left
/// out, each with its edit distance.
fn near_terms(
    searcher: &tantivy::Searcher,
    field: Field,
    key: &str,
    edits: u8,
) -> Result<Vec<(String, u8)>> {
    let dfa = builder(edits).build_dfa(key);
    let mut near: BTreeMap<String, u8> = BTreeMap::new();
    for segment in searcher.segment_readers() {
        let index = segment.inverted_index(field)?;
        let mut terms = index.terms().search(LevenshteinDfa(&dfa)).into_stream()?;
        while terms.advance() {
            let Ok(term) = std::str::from_utf8(terms.key()) else {
                continue;
            };
            if term == key {
                continue;
            }
            if let Distance::Exact(distance) = dfa.eval(term) {
                near.insert(term.to_string(), distance);
            }
            if near.len() >= MAX_NEAR_TERMS {
                break;
            }
        }
    }
    Ok(near.into_iter().collect())
}

/// Whether `fixed` is close enough to `word`, one word of a corrected
/// name: within [`max_edits`], or one edit for a word of two or three
/// letters ("taco bel").
fn within_edits(word: &str, fixed: &str) -> bool {
    let edits = match word.chars().count() {
        0 | 1 => 0,
        2 | 3 => 1,
        chars => max_edits(chars),
    };
    match edits {
        0 => word == fixed,
        edits => matches!(
            builder(edits).build_dfa(word).eval(fixed),
            Distance::Exact(_)
        ),
    }
}

/// Levenshtein automata builders, which are costly to make, for 1 and 2
/// edits, with a swap of neighbouring letters as one edit.
fn builder(edits: u8) -> &'static LevenshteinAutomatonBuilder {
    static ONE: OnceLock<LevenshteinAutomatonBuilder> = OnceLock::new();
    static TWO: OnceLock<LevenshteinAutomatonBuilder> = OnceLock::new();
    match edits {
        1 => ONE.get_or_init(|| LevenshteinAutomatonBuilder::new(1, true)),
        _ => TWO.get_or_init(|| LevenshteinAutomatonBuilder::new(2, true)),
    }
}

/// A Levenshtein DFA walking a term dictionary.
struct LevenshteinDfa<'a>(&'a DFA);

impl Automaton for LevenshteinDfa<'_> {
    type State = u32;

    fn start(&self) -> u32 {
        self.0.initial_state()
    }

    fn is_match(&self, state: &u32) -> bool {
        matches!(self.0.distance(*state), Distance::Exact(_))
    }

    fn can_match(&self, state: &u32) -> bool {
        *state != SINK_STATE
    }

    fn accept(&self, state: &u32, byte: u8) -> u32 {
        self.0.transition(*state, byte)
    }
}

/// `fixed`, a corrected spelling of `words` joined, split where `words` are:
/// `bank`, `of`, `amercia` and `bankofamerica` give `bank`, `of`,
/// `america`. Letters inserted at a word boundary go to the next word.
/// `None` when a word would be left empty.
fn align(words: &[String], fixed: &str) -> Option<Vec<String>> {
    let a: Vec<char> = words.concat().chars().collect();
    let b: Vec<char> = fixed.chars().collect();
    let (n, m) = (a.len(), b.len());
    // Optimal string alignment distances of every pair of prefixes.
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    // Walk back, noting for each count of typed letters the fewest fixed
    // letters it lines up with.
    let mut lines_up = vec![usize::MAX; n + 1];
    let (mut i, mut j) = (n, m);
    lines_up[n] = m;
    while i > 0 || j > 0 {
        if i > 1
            && j > 1
            && a[i - 1] == b[j - 2]
            && a[i - 2] == b[j - 1]
            && d[i][j] == d[i - 2][j - 2] + 1
        {
            i -= 2;
            j -= 2;
        } else if i > 0 && j > 0 && d[i][j] == d[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]) {
            i -= 1;
            j -= 1;
        } else if j > 0 && d[i][j] == d[i][j - 1] + 1 {
            j -= 1;
        } else {
            i -= 1;
        }
        lines_up[i] = lines_up[i].min(j);
    }
    let mut out = Vec::with_capacity(words.len());
    let mut start = 0;
    let mut typed = 0;
    for (k, word) in words.iter().enumerate() {
        typed += word.chars().count();
        let end = if k + 1 == words.len() {
            m
        } else {
            lines_up[typed]
        };
        if end == usize::MAX || end <= start {
            return None;
        }
        out.push(b[start..end].iter().collect());
        start = end;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_string).collect()
    }

    #[test]
    fn edits_grow_with_length() {
        assert_eq!(max_edits(3), 0);
        assert_eq!(max_edits(4), 1);
        assert_eq!(max_edits(7), 1);
        assert_eq!(max_edits(8), 2);
    }

    #[test]
    fn corrections_are_split_like_the_query() {
        let cases = [
            ("bank of amercia", "bankofamerica", "bank of america"),
            ("amazn prime", "amazonprime", "amazon prime"),
            ("gogle", "google", "google"),
            ("wels fargo", "wellsfargo", "wells fargo"),
            ("youtueb", "youtube", "youtube"),
            ("face bok", "facebook", "face book"),
        ];
        for (typed, fixed, expected) in cases {
            assert_eq!(
                align(&words(typed), fixed).map(|w| w.join(" ")).as_deref(),
                Some(expected),
                "{typed}"
            );
        }
    }

    #[test]
    fn corrections_that_empty_a_word_are_not_split() {
        // "a" would have to vanish.
        assert_eq!(align(&words("ama zon"), "amazon").unwrap(), ["ama", "zon"]);
        assert_eq!(align(&words("x amazon"), "amazon"), None);
    }
}
