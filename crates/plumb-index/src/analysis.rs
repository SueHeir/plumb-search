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

use std::borrow::Cow;

use plumb_core::{joined, normalize_text};
use tantivy::tokenizer::{
    AsciiFoldingFilter, Language, LowerCaser, PreTokenizedString, RemoveLongFilter, Stemmer,
    StopWordFilter, TextAnalyzer, Token, TokenStream, Tokenizer, TokenizerManager,
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

/// [`register`] for the sites' index, whose word fields hold a run of
/// [`unspaced`] script as its [`pieces`], and a word of Arabic script also
/// [`folded`]. Queries are read with [`words_analyzer`] still, and a word
/// is looked for by its pieces ([`crate::query::Clauses::add_word`]), so
/// the names a query is matched to stay whole.
pub(crate) fn register_sites(manager: &TokenizerManager) {
    register(manager);
    manager.register(
        WORDS_ANALYZER,
        TextAnalyzer::builder(WordTokenizer {
            for_sites: true,
            ..WordTokenizer::default()
        })
        .filter(AsciiFoldingFilter)
        .filter(LowerCaser)
        .filter(RemoveLongFilter::limit(TOKEN_BYTES_LIMIT))
        .build(),
    );
}

/// [`register`] for the pages' index, whose names are keyed with a sharp
/// sign after a letter read out ([`page_joined_analyzer`]). The page index
/// is rebuilt whenever [`crate::pages::PAGE_INDEX_VERSION`] changes; a
/// sites' index is not, and would still hold "C# Corner" as "ccorner".
pub(crate) fn register_pages(manager: &TokenizerManager) {
    register(manager);
    manager.register(JOINED_ANALYZER, page_joined_analyzer());
}

/// `text` with a sharp sign after a lone letter read out, as English writes
/// it where the sign cannot go (Wikipedia's "C Sharp", "C-sharp minor"):
/// `C# 12` -> `C sharp 12`. [`normalize_text`] drops the sign, and "c#"
/// would name C.
fn read_sharps(text: &str) -> Cow<'_, str> {
    let sharp = |c: &char| matches!(c, '#' | '\u{266F}');
    if !text.chars().any(|c| sharp(&c)) {
        return Cow::Borrowed(text);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    for (i, c) in chars.iter().enumerate() {
        let lone_letter =
            i > 0 && chars[i - 1].is_alphabetic() && (i < 2 || !chars[i - 2].is_alphanumeric());
        let ends = chars.get(i + 1).is_none_or(|next| !next.is_alphanumeric());
        if sharp(c) && lone_letter && ends {
            out.push_str(" sharp");
        } else {
            out.push(*c);
        }
    }
    Cow::Owned(out)
}

/// Whether `c` belongs to a script written without spaces between words:
/// Chinese and Japanese, Thai, Lao, Khmer and Burmese.
fn unspaced(c: char) -> bool {
    matches!(c as u32,
        0x0E00..=0x0EFF // Thai, Lao
        | 0x1000..=0x109F // Burmese
        | 0x1780..=0x17FF // Khmer
        | 0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F // kana
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF // Han
    )
}

/// The terms a word is indexed as in the sites' word fields: the word,
/// except that a run of more than two [`unspaced`] characters becomes its
/// overlapping pairs, and such a run is split from letters of other
/// scripts. A title's "全国の天気予報" has no spaces to find the word
/// "天気予報" by, but holds all of its pairs: 天気, 気予, 予報.
pub(crate) fn pieces(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut pieces = Vec::new();
    for run in chars.chunk_by(|a, b| unspaced(*a) == unspaced(*b)) {
        if unspaced(run[0]) && run.len() > 2 {
            pieces.extend(run.windows(2).map(|pair| pair.iter().collect::<String>()));
        } else {
            pieces.push(run.iter().collect());
        }
    }
    pieces
}

/// Starts a [`folded`] word, which no word as typed can: words hold only
/// letters and digits.
pub(crate) const FOLDED: char = '~';

/// A word of Arabic script with the letters it is written either way made
/// one, after [`FOLDED`]. Arabic is often typed without the hamza or madda
/// over an alef (`اخبار` for `أخبار`), with a dotless yeh for a final yeh
/// (`مصرى` for `مصري`) and with Persian yeh and kaf for the Arabic ones,
/// and the tatweel only stretches a word (`اخـبـار`). The sites' word
/// fields hold each such word folded too, and a query looks for the folded
/// form a little weaker than the word as typed, so either spelling finds
/// the other but the one typed comes first. `None` for other words.
pub(crate) fn folded(word: &str) -> Option<String> {
    if !word.chars().any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)) {
        return None;
    }
    let letters = word.chars().filter_map(|letter| match letter {
        'أ' | 'إ' | 'آ' | 'ٱ' => Some('ا'),
        'ى' | 'ی' => Some('ي'),
        'ک' => Some('ك'),
        'ـ' => None,
        _ => Some(letter),
    });
    Some(std::iter::once(FOLDED).chain(letters).collect())
}

/// The analyzer registered as [`STEMMED_ANALYZER`].
pub(crate) fn stemmed_analyzer() -> TextAnalyzer {
    stemmed_in(Language::English)
}

/// Bounded language support for prose fields. Names and symbols keep the
/// unstemmed analyzers. Other explicitly declared languages stay lexical.
pub(crate) fn content_analyzer(language: Option<&str>) -> TextAnalyzer {
    match language {
        Some("es") => translated_analyzer(Language::Spanish),
        Some("de") => translated_analyzer(Language::German),
        None | Some("en") => stemmed_analyzer(),
        _ => words_analyzer(),
    }
}

fn translated_analyzer(language: Language) -> TextAnalyzer {
    TextAnalyzer::builder(WordTokenizer::default())
        .filter(LowerCaser)
        .filter(RemoveLongFilter::limit(TOKEN_BYTES_LIMIT))
        // Remove accented stop words before folding: "für", "más".
        .filter(StopWordFilter::new(language).expect("supported stop words"))
        .filter(AsciiFoldingFilter)
        .filter(LowerCaser)
        .filter(Stemmer::new(language))
        .build()
}

fn stemmed_in(language: Language) -> TextAnalyzer {
    TextAnalyzer::builder(WordTokenizer::default())
        .filter(AsciiFoldingFilter)
        .filter(LowerCaser)
        .filter(RemoveLongFilter::limit(TOKEN_BYTES_LIMIT))
        .filter(StopWordFilter::new(language).expect("supported stop words"))
        .filter(Stemmer::new(language))
        .build()
}

/// Tokenize prose using the page's own language, bypassing the field's
/// default English analyzer. No extra multilingual name field is needed.
pub(crate) fn content_tokens(language: Option<&str>, text: &str) -> PreTokenizedString {
    let text = normalize_text(text);
    let mut analyzer = content_analyzer(language);
    let mut tokens = Vec::new();
    {
        let mut stream = analyzer.token_stream(&text);
        while let Some(token) = stream.next() {
            tokens.push(token.clone());
        }
    }
    PreTokenizedString { text, tokens }
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
    joined_reading(false)
}

/// [`joined_analyzer`] reading a sharp sign after a letter
/// ([`read_sharps`]): `C#` -> `csharp`, as "C Sharp" is keyed.
pub(crate) fn page_joined_analyzer() -> TextAnalyzer {
    joined_reading(true)
}

fn joined_reading(sharps: bool) -> TextAnalyzer {
    TextAnalyzer::builder(JoinedTokenizer {
        sharps,
        ..JoinedTokenizer::default()
    })
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
    /// Each word as the sites' index holds it: as its [`pieces`], or with
    /// its [`folded`] form at the same position.
    for_sites: bool,
}

struct WordStream<'a> {
    words: std::str::Split<'a, char>,
    offset: usize,
    token: &'a mut Token,
    for_sites: bool,
    /// The pieces of the current word still to come, last first.
    pending: Vec<String>,
    /// The current word folded, still to come.
    folded: Option<String>,
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
            for_sites: self.for_sites,
            pending: Vec::new(),
            folded: None,
        }
    }
}

impl TokenStream for WordStream<'_> {
    fn advance(&mut self) -> bool {
        if let Some(folded) = self.folded.take() {
            self.token.text = folded;
            return true;
        }
        if let Some(piece) = self.pending.pop() {
            self.token.text = piece;
            self.token.position = self.token.position.wrapping_add(1);
            return true;
        }
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
            if self.for_sites && self.token.text.chars().any(unspaced) {
                self.pending = pieces(&self.token.text);
                self.pending.reverse();
                return self.advance();
            }
            if self.for_sites {
                self.folded = folded(&self.token.text);
            }
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
    /// Whether a sharp after a letter is read out ([`read_sharps`]).
    sharps: bool,
    token: Token,
}

struct SingleTokenStream<'a> {
    token: &'a mut Token,
    pending: bool,
}

impl Tokenizer for JoinedTokenizer {
    type TokenStream<'a> = SingleTokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> SingleTokenStream<'a> {
        let text = if self.sharps {
            read_sharps(text)
        } else {
            text.into()
        };
        self.token.reset();
        self.token
            .text
            .extend(joined(&text).chars().filter(|c| c.is_alphanumeric()));
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
    fn the_page_index_keys_a_sharp_after_a_letter_read_out() {
        let page_key = |text| tokens(&page_joined_analyzer(), text);
        assert_eq!(page_key("C#"), ["csharp"]);
        assert_eq!(page_key("C Sharp"), ["csharp"]);
        assert_eq!(page_key("F\u{266F} minor"), ["fsharpminor"]);
        // Only after a lone letter at a word's end.
        assert_eq!(page_key("#metoo issue #5"), ["metooissue5"]);
        assert_eq!(page_key("abc# c#d"), ["abccd"]);
        // Words, and the sites' index, keep dropping it.
        assert_eq!(words("C# Corner"), ["c", "corner"]);
        assert_eq!(joined_token("C#"), ["c"]);
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
    fn spanish_and_german_prose_uses_native_stop_words_and_inflections() {
        assert!(tokens(&content_analyzer(Some("de")), "für die und").is_empty());
        assert!(tokens(&content_analyzer(Some("es")), "más de la").is_empty());
        assert_eq!(
            tokens(&content_analyzer(Some("es")), "receta fácil galleta"),
            tokens(&content_analyzer(Some("es")), "recetas fáciles galletas")
        );
        assert_eq!(
            tokens(&content_analyzer(Some("de")), "gesunde bewegung"),
            tokens(&content_analyzer(Some("de")), "gesunden bewegungen")
        );
        // Unsupported languages keep exact neutral words rather than
        // applying English morphology to them.
        assert_eq!(tokens(&content_analyzer(Some("fr")), "types"), ["types"]);
    }

    #[test]
    fn unspaced_scripts_are_indexed_in_pieces() {
        assert_eq!(pieces("天気予報"), ["天気", "気予", "予報"]);
        assert_eq!(pieces("楽天"), ["楽天"]);
        assert_eq!(pieces("協会tenki"), ["協会", "tenki"]);
        assert_eq!(pieces("weather"), ["weather"]);
        let manager = TokenizerManager::default();
        register_sites(&manager);
        let sites = manager.get(WORDS_ANALYZER).unwrap();
        assert_eq!(
            tokens(&sites, "全国の天気予報 tenki.jp"),
            ["全国", "国の", "の天", "天気", "気予", "予報", "tenki", "jp"]
        );
        // Queries keep their words whole.
        assert_eq!(words("全国の天気予報"), ["全国の天気予報"]);
        let mut sites = sites.clone();
        let mut stream = sites.token_stream("天気予報 tenki");
        let mut positions = Vec::new();
        while let Some(token) = stream.next() {
            positions.push(token.position);
        }
        assert_eq!(positions, [0, 1, 2, 3]);
    }

    #[test]
    fn arabic_is_indexed_folded_too() {
        assert_eq!(folded("أخبار").as_deref(), Some("~اخبار"));
        assert_eq!(folded("اخـبـار").as_deref(), Some("~اخبار"));
        assert_eq!(folded("مصرى").as_deref(), Some("~مصري"));
        assert_eq!(folded("ایران").as_deref(), Some("~ايران"));
        assert_eq!(folded("اخبار").as_deref(), Some("~اخبار"));
        assert_eq!(folded("news"), None);
        let manager = TokenizerManager::default();
        register_sites(&manager);
        let mut sites = manager.get(WORDS_ANALYZER).unwrap();
        let mut stream = sites.token_stream("أخبار news");
        let mut indexed = Vec::new();
        while let Some(token) = stream.next() {
            indexed.push((token.text.clone(), token.position));
        }
        let expected = [("أخبار", 0), ("~اخبار", 0), ("news", 1)];
        assert_eq!(indexed, expected.map(|(text, at)| (text.to_string(), at)));
        // Queries keep the spelling typed.
        assert_eq!(words("أخبار"), ["أخبار"]);
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
