//! Text analysis shared by indexing and querying.
//!
//! Both analyzers run [`plumb_core::normalize_text`] themselves, so a field
//! can store the original text for display while the index and every query
//! see exactly the same tokens. After normalization, tokens are ASCII-folded
//! (`nestlé` -> `nestle`) and lowercased again, because a few characters fold
//! to uppercase ASCII.
//!
//! Token offsets point into the normalized text, not the original, so they
//! must not be used for highlighting.

use plumb_core::{joined, normalize_text};
use tantivy::tokenizer::{
    AsciiFoldingFilter, Language, LowerCaser, RemoveLongFilter, Stemmer, StopWordFilter,
    TextAnalyzer, Token, TokenStream, Tokenizer, TokenizerManager,
};

/// Analyzer for word fields: one token per word of the normalized text,
/// `U.S. Bank | Nestlé` -> `us`, `bank`, `nestle`.
pub(crate) const WORDS_ANALYZER: &str = "plumb_words";
/// Analyzer for joined fields: the whole value becomes a single token with
/// the spaces removed, `U.S. Bank` -> `usbank`.
pub(crate) const JOINED_ANALYZER: &str = "plumb_joined";
/// Analyzer for the words of a question: English stems without the most
/// common words, `How do I undo commits?` -> `how`, `do`, `i`, `undo`,
/// `commit`.
pub(crate) const STEMMED_ANALYZER: &str = "plumb_stemmed";

/// Tokens of this many bytes or more are dropped. Names are far shorter.
const TOKEN_BYTES_LIMIT: usize = 256;

/// Registers both analyzers under their names.
pub(crate) fn register(manager: &TokenizerManager) {
    manager.register(WORDS_ANALYZER, words_analyzer());
    manager.register(JOINED_ANALYZER, joined_analyzer());
    manager.register(STEMMED_ANALYZER, stemmed_analyzer());
}

/// The analyzer registered as [`STEMMED_ANALYZER`].
pub(crate) fn stemmed_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(WordTokenizer::default())
        .filter(AsciiFoldingFilter)
        .filter(LowerCaser)
        .filter(RemoveLongFilter::limit(TOKEN_BYTES_LIMIT))
        .filter(StopWordFilter::new(Language::English).expect("English stop words"))
        .filter(Stemmer::new(Language::English))
        .build()
}

/// The analyzer registered as [`WORDS_ANALYZER`].
pub(crate) fn words_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(WordTokenizer::default())
        .filter(AsciiFoldingFilter)
        .filter(LowerCaser)
        .filter(RemoveLongFilter::limit(TOKEN_BYTES_LIMIT))
        .build()
}

/// The analyzer registered as [`JOINED_ANALYZER`].
pub(crate) fn joined_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(JoinedTokenizer::default())
        .filter(AsciiFoldingFilter)
        .filter(LowerCaser)
        .filter(RemoveLongFilter::limit(TOKEN_BYTES_LIMIT))
        .build()
}

/// The tokens `analyzer` produces for `text`.
pub(crate) fn tokens(analyzer: &TextAnalyzer, text: &str) -> Vec<String> {
    let mut analyzer = analyzer.clone();
    let mut stream = analyzer.token_stream(text);
    let mut out = Vec::new();
    while let Some(token) = stream.next() {
        out.push(token.text.clone());
    }
    out
}

/// Splits the output of [`normalize_text`] into words. Characters that are
/// not letters or digits are dropped from each word: lowercasing `İ` leaves
/// a combining dot behind, and `İstanbul` should still match `istanbul`.
#[derive(Clone, Default)]
struct WordTokenizer {
    normalized: String,
    token: Token,
}

struct WordStream<'a> {
    words: std::str::Split<'a, char>,
    offset: usize,
    token: &'a mut Token,
}

impl Tokenizer for WordTokenizer {
    type TokenStream<'a> = WordStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> WordStream<'a> {
        self.normalized = normalize_text(text);
        self.token.reset();
        WordStream {
            words: self.normalized.split(' '),
            offset: 0,
            token: &mut self.token,
        }
    }
}

impl TokenStream for WordStream<'_> {
    fn advance(&mut self) -> bool {
        for word in self.words.by_ref() {
            let from = self.offset;
            // `normalize_text` separates words with exactly one space.
            self.offset += word.len() + 1;
            self.token.text.clear();
            self.token
                .text
                .extend(word.chars().filter(|c| c.is_alphanumeric()));
            if self.token.text.is_empty() {
                continue;
            }
            self.token.offset_from = from;
            self.token.offset_to = from + word.len();
            self.token.position = self.token.position.wrapping_add(1);
            return true;
        }
        false
    }

    fn token(&self) -> &Token {
        self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

/// Emits the [`joined`] form of the whole text as one token, or nothing when
/// the text has no letters or digits.
#[derive(Clone, Default)]
struct JoinedTokenizer {
    token: Token,
}

struct SingleTokenStream<'a> {
    token: &'a mut Token,
    pending: bool,
}

impl Tokenizer for JoinedTokenizer {
    type TokenStream<'a> = SingleTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> SingleTokenStream<'a> {
        self.token.reset();
        self.token
            .text
            .extend(joined(text).chars().filter(|c| c.is_alphanumeric()));
        self.token.position = 0;
        self.token.offset_to = self.token.text.len();
        let pending = !self.token.text.is_empty();
        SingleTokenStream {
            token: &mut self.token,
            pending,
        }
    }
}

impl TokenStream for SingleTokenStream<'_> {
    fn advance(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }

    fn token(&self) -> &Token {
        self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        self.token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        tokens(&words_analyzer(), text)
    }

    fn joined_token(text: &str) -> Vec<String> {
        tokens(&joined_analyzer(), text)
    }

    #[test]
    fn words_are_normalized_and_folded() {
        assert_eq!(
            words("U.S. Bank | Personal Banking"),
            ["us", "bank", "personal", "banking"]
        );
        assert_eq!(words("Nestlé Café"), ["nestle", "cafe"]);
        assert_eq!(words("McDonald's"), ["mcdonalds"]);
        assert_eq!(words("İstanbul"), ["istanbul"]);
        assert_eq!(words("STRASSE straße"), ["strasse", "strasse"]);
        assert!(words("  --- ... !!! ").is_empty());
        assert!(words("").is_empty());
    }

    #[test]
    fn stems_drop_common_words() {
        assert_eq!(
            tokens(
                &stemmed_analyzer(),
                "How do I undo the most recent commits?"
            ),
            ["how", "do", "i", "undo", "most", "recent", "commit"]
        );
    }

    #[test]
    fn joined_is_one_token() {
        assert_eq!(joined_token("U.S. Bank"), ["usbank"]);
        assert_eq!(joined_token("Bank of America"), ["bankofamerica"]);
        assert_eq!(joined_token("us-bank"), ["usbank"]);
        assert_eq!(joined_token("Nestlé"), ["nestle"]);
        assert!(joined_token(" | ").is_empty());
    }

    #[test]
    fn positions_count_words() {
        let mut analyzer = words_analyzer();
        let mut stream = analyzer.token_stream("Bank of America");
        let mut positions = Vec::new();
        while let Some(token) = stream.next() {
            positions.push(token.position);
        }
        assert_eq!(positions, [0, 1, 2]);
    }

    #[test]
    fn overlong_tokens_are_dropped() {
        let long = "a".repeat(TOKEN_BYTES_LIMIT);
        assert_eq!(words(&format!("{long} bank")), ["bank"]);
        assert!(joined_token(&long).is_empty());
    }
}
