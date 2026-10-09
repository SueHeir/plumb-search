//! English words and what they mean, from Wiktionary as wiktextract reads
//! it ([`DUMP_URL`], kaikki.org's English dictionary, about 3.3 GB of JSON
//! lines, rebuilt from Wiktionary's dumps every few days): the `wiktionary`
//! page set, for searches that ask what a word means ("define anadromous",
//! "prioritize meaning").
//!
//! Each word is written as an article: its title the word, its description
//! the first sense of each part of speech it has, at most
//! [`MAX_PARTS_OF_SPEECH`] and [`MAX_DEFINITION_CHARS`] characters
//! ("(adjective) Of fish, migrating up rivers from the sea to breed in
//! fresh water."), and its views how much Wiktionary says of it (senses and
//! translations), so the best-known words come first. Senses that only
//! point at another word ("plural of mouse"), and obsolete and archaic
//! ones when there are others, are left out; so are proper names, which
//! Wikipedia covers.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, Result};
use plumb_core::article::Article;
use serde::Deserialize;

use crate::open_maybe_gz;

/// kaikki.org's English entries of English Wiktionary.
pub const DUMP_URL: &str =
    "https://kaikki.org/dictionary/English/kaikki.org-dictionary-English.jsonl";

/// Most parts of speech a word's description gives a sense of.
pub const MAX_PARTS_OF_SPEECH: usize = 3;

/// Longest description of a word, in characters.
pub const MAX_DEFINITION_CHARS: usize = 300;

/// Most words in an entry kept: phrases longer than this are left out.
const MAX_ENTRY_WORDS: usize = 4;

/// Parts of speech that are not words looked up for what they mean.
const LEFT_OUT: &[&str] = &[
    "name",
    "character",
    "symbol",
    "punct",
    "romanization",
    "prefix",
    "suffix",
    "infix",
    "interfix",
    "affix",
    "circumfix",
];

/// Tags of senses left out when a word has others.
const UNUSUAL: &[&str] = &["obsolete", "archaic", "rare", "dated"];

#[derive(Debug, Deserialize)]
struct Entry {
    word: String,
    #[serde(default)]
    pos: String,
    #[serde(default)]
    lang_code: String,
    #[serde(default)]
    senses: Vec<Sense>,
    #[serde(default)]
    translations: Vec<serde::de::IgnoredAny>,
}

#[derive(Debug, Deserialize)]
struct Sense {
    #[serde(default)]
    glosses: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    form_of: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    alt_of: Vec<serde::de::IgnoredAny>,
}

/// One word as read so far.
#[derive(Debug, Default)]
struct Word {
    /// Each part of speech's first sense, in the order met.
    senses: Vec<(String, String)>,
    weight: u64,
}

/// The first usual sense of `senses`, or the first at all.
fn first_sense(senses: &[Sense]) -> Option<String> {
    let defining: Vec<&Sense> = senses
        .iter()
        .filter(|s| s.form_of.is_empty() && s.alt_of.is_empty())
        .filter(|s| !s.tags.iter().any(|t| t == "form-of" || t == "alt-of"))
        .filter(|s| s.glosses.iter().any(|g| !g.trim().is_empty()))
        .collect();
    let usual = defining
        .iter()
        .find(|s| !s.tags.iter().any(|t| UNUSUAL.contains(&t.as_str())))
        .or(defining.first())?;
    // A sub-sense lists its parents' glosses first, the outermost
    // sometimes only a heading ("Terms relating to animals."): its own,
    // the last, is the sense.
    let gloss = usual
        .glosses
        .iter()
        .rev()
        .find(|g| !g.trim().is_empty() && !g.starts_with("Terms relating to"))?;
    Some(plumb_core::collapse_whitespace(gloss))
}

/// The words of kaikki.org's English dictionary `path` (JSON lines,
/// gzipped or not), as articles, the best-known first.
pub fn read_words(path: &Path) -> Result<Vec<Article>> {
    let reader = open_maybe_gz(path)?;
    let mut words: HashMap<String, Word> = HashMap::new();
    for line in reader.lines() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        let Ok(entry) = serde_json::from_str::<Entry>(&line) else {
            continue;
        };
        if entry.lang_code != "en"
            || LEFT_OUT.contains(&entry.pos.as_str())
            || entry.word.split_whitespace().count() > MAX_ENTRY_WORDS
            || entry.word.contains(['\t', '|'])
        {
            continue;
        }
        let Some(sense) = first_sense(&entry.senses) else {
            continue;
        };
        let word = words.entry(entry.word.trim().to_string()).or_default();
        word.weight += entry.senses.len() as u64 + entry.translations.len() as u64;
        if !word.senses.iter().any(|(pos, _)| *pos == entry.pos) {
            word.senses.push((entry.pos, sense));
        }
    }
    let mut articles: Vec<Article> = words
        .into_iter()
        .filter(|(title, _)| !title.is_empty())
        .map(|(title, word)| Article {
            description: Some(definition(&word.senses)),
            views: word.weight,
            title,
            ..Article::default()
        })
        .collect();
    articles.sort_by(|a, b| b.views.cmp(&a.views).then_with(|| a.title.cmp(&b.title)));
    Ok(articles)
}

/// A word's senses as its description: "(noun) … (verb) …".
fn definition(senses: &[(String, String)]) -> String {
    let mut text = String::new();
    for (pos, sense) in senses.iter().take(MAX_PARTS_OF_SPEECH) {
        let part = format!("({}) {sense}", pos_name(pos));
        if !text.is_empty() {
            if text.chars().count() + part.chars().count() + 2 > MAX_DEFINITION_CHARS {
                break;
            }
            text.push_str(if text.ends_with(['.', '!', '?']) {
                " "
            } else {
                "; "
            });
        }
        text.push_str(&part);
    }
    if text.chars().count() > MAX_DEFINITION_CHARS {
        let cut = plumb_core::truncate_chars(&text, MAX_DEFINITION_CHARS - 1);
        let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
        text = format!("{cut}…");
    }
    text
}

/// A part of speech as people say it: "adjective" for "adj".
fn pos_name(pos: &str) -> &str {
    match pos {
        "adj" => "adjective",
        "adv" => "adverb",
        "prep" => "preposition",
        "conj" => "conjunction",
        "intj" => "interjection",
        "pron" => "pronoun",
        "det" => "determiner",
        "num" => "numeral",
        "abbrev" => "abbreviation",
        "phrase" | "prep_phrase" => "phrase",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_first_usual_sense_of_each_part_of_speech() {
        let lines = [
            r#"{"word": "anadromous", "pos": "adj", "lang_code": "en", "senses": [{"glosses": ["Of fish, migrating up rivers from the sea to breed in fresh water."]}]}"#,
            r#"{"word": "free", "pos": "adj", "lang_code": "en", "translations": [{}, {}, {}], "senses": [{"glosses": ["Not costing anything."], "tags": ["obsolete"]}, {"glosses": ["Unconstrained."]}]}"#,
            r#"{"word": "free", "pos": "verb", "lang_code": "en", "senses": [{"glosses": ["To make free."]}]}"#,
            r#"{"word": "cat", "pos": "noun", "lang_code": "en", "senses": [{"glosses": ["Terms relating to animals.", "A mammal of the family Felidae."]}]}"#,
            r#"{"word": "mice", "pos": "noun", "lang_code": "en", "senses": [{"glosses": ["plural of mouse"], "form_of": [{"word": "mouse"}], "tags": ["form-of", "plural"]}]}"#,
            r#"{"word": "Paris", "pos": "name", "lang_code": "en", "senses": [{"glosses": ["The capital of France."]}]}"#,
            r#"{"word": "frei", "pos": "adj", "lang_code": "de", "senses": [{"glosses": ["free"]}]}"#,
        ]
        .join("\n");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("en.jsonl");
        std::fs::write(&path, lines).unwrap();
        let words = read_words(&path).unwrap();
        let shown: Vec<(&str, &str)> = words
            .iter()
            .map(|a| (a.title.as_str(), a.description.as_deref().unwrap()))
            .collect();
        assert_eq!(
            shown,
            [
                ("free", "(adjective) Unconstrained. (verb) To make free."),
                (
                    "anadromous",
                    "(adjective) Of fish, migrating up rivers from the sea to breed in fresh water."
                ),
                ("cat", "(noun) A mammal of the family Felidae."),
            ]
        );
    }

    #[test]
    fn long_definitions_are_cut() {
        let long = "word ".repeat(100);
        let text = definition(&[("noun".into(), long)]);
        assert!(text.ends_with('…') && text.chars().count() <= MAX_DEFINITION_CHARS);
    }
}
