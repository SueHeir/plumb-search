//! A site's search terms, picked from its whole homepage with YAKE, a
//! statistical keyword extractor: words a page stresses (often, early, in
//! many sentences, capitalized) that its title and description may not
//! use. No model, a few milliseconds a page, and the same terms on every
//! machine for the same text.

use std::collections::HashSet;
use std::sync::LazyLock;

use plumb_core::MAX_TERMS;

/// Distinct words picked from a page.
pub const TERM_WORDS: usize = 30;

static STOP_WORDS: LazyLock<yake_rust::StopWords> = LazyLock::new(|| {
    yake_rust::StopWords::predefined("en").expect("YAKE ships English stop words")
});

/// Up to `count` distinct words of `text` that YAKE ranks best, best
/// first, the best repeated so they weigh more (see [`repeated`]); at most
/// [`MAX_TERMS`] entries in all.
pub fn pick_terms(text: &str, count: usize) -> Vec<String> {
    let config = yake_rust::Config {
        ngrams: 2,
        ..yake_rust::Config::default()
    };
    let mut phrases = yake_rust::get_n_best(count * 2, text, &STOP_WORDS, &config);
    // Equal scores are ordered by the phrase, not by hash map order.
    phrases.sort_by(|a, b| {
        a.score
            .total_cmp(&b.score)
            .then_with(|| a.keyword.cmp(&b.keyword))
    });
    let mut seen = HashSet::new();
    let words: Vec<String> = phrases
        .iter()
        .flat_map(|phrase| words_of(&phrase.keyword).collect::<Vec<_>>())
        .filter(|w| seen.insert(w.clone()))
        .take(count)
        .collect();
    repeated(&words)
}

/// Lowercased words of letters and digits, two characters or more.
pub fn words_of(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 1)
        .map(str::to_lowercase)
}

/// `words` (best first) as terms: the first third stand three times, the
/// second third twice, the rest once; at most [`MAX_TERMS`] entries.
/// YAKE's scores do not compare between pages; its order does.
fn repeated(words: &[String]) -> Vec<String> {
    let n = words.len();
    let mut terms = Vec::new();
    for (i, word) in words.iter().enumerate() {
        let times = 1 + usize::from(i * 3 < n * 2) + usize::from(i * 3 < n);
        for _ in 0..times {
            if terms.len() == MAX_TERMS {
                return terms;
            }
            terms.push(word.clone());
        }
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn earlier_words_stand_more_times() {
        let words: Vec<String> = ["tesla", "car", "the"].map(String::from).to_vec();
        assert_eq!(
            repeated(&words),
            ["tesla", "tesla", "tesla", "car", "car", "the"]
        );
    }

    #[test]
    fn picks_the_words_a_page_stresses() {
        let text = "Tesla. Electric cars. Tesla builds electric cars, solar roofs and \
                    batteries. Order a Tesla electric car online today.";
        let terms = pick_terms(text, 4);
        let distinct: HashSet<_> = terms.iter().collect();
        assert!(!terms.is_empty() && distinct.len() <= 4);
        assert!(terms.iter().any(|w| w == "tesla"), "{terms:?}");
        assert!(!terms.iter().any(|w| w == "and" || w == "a"), "{terms:?}");
        assert_eq!(pick_terms(text, 4), terms);
        assert!(pick_terms("", 4).is_empty());
    }
}
