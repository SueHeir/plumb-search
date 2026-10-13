//! A query parsed into the index's terms, and the Tantivy query built from
//! them with each field's weight.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use plumb_core::{is_function_word, kind_key, other_number, truncate_chars, MAX_TEXT_CHARS};
use tantivy::collector::DocSetCollector;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{DocAddress, Term};

use crate::names::typed_domain;
use crate::rank::bm25_of;
use crate::schema::Fields;
use crate::{analysis, FILLER_WORDS, MAX_QUERY_WORDS};

/// BM25 boost of a query word matching the domain label.
const LABEL_BOOST: f32 = 4.0;

/// BM25 boost of a query word matching a whole joined name.
const JOINED_BOOST: f32 = 4.0;

/// BM25 boost of a query word matching an alias.
const ALIASES_BOOST: f32 = 2.5;

/// BM25 boost of a query word matching the title.
const TITLE_BOOST: f32 = 2.0;

/// BM25 boost of a query word matching inbound link text.
const ANCHORS_BOOST: f32 = 1.5;

/// BM25 boost of a query word matching the description.
const DESCRIPTION_BOOST: f32 = 0.5;

/// BM25 boost of a query word matching Wikidata's description of the
/// organization. Higher than the site's own description: Wikidata's is
/// written by others, so look-alikes cannot stuff it with search words.
const ABOUT_BOOST: f32 = 2.0;

/// BM25 boost of a query word matching a homepage heading.
const HEADINGS_BOOST: f32 = 0.5;

/// BM25 boost of the whole query, joined (`us bank` -> `usbank`), matching a
/// joined name or a label word.
const WHOLE_QUERY_BOOST: f32 = 6.0;

/// BM25 boost of a query that is the hostname or URL of an indexed domain.
const DOMAIN_BOOST: f32 = 10.0;

/// The share of a word's boost its other number gets ("video" for
/// "videos").
const OTHER_NUMBER_SHARE: f32 = 0.8;

/// Words that set the things around them side by side ("mitosis vs
/// meiosis") and are none of them. Between other words they are matched
/// nowhere: as a word of their own, the best-known homepage saying "vs"
/// (apple.com's) came before what is compared in nearly every comparison.
pub(crate) const COMPARISON_WORDS: &[&str] = &["vs", "versus"];

/// The words of `query` after the first `words` of its analyzed words, as
/// typed: "github sueheir/plumb-search" after 1 -> "sueheir/plumb-search".
/// `None` when nothing is left, or when the typed words do not line up
/// with the analyzed ones.
pub(crate) fn words_after(analyzer: &TextAnalyzer, query: &str, words: usize) -> Option<String> {
    let typed: Vec<&str> = query.split_whitespace().collect();
    let mut seen = 0;
    for (i, word) in typed.iter().enumerate() {
        if seen == words {
            let rest = typed[i..].join(" ");
            return (!rest.is_empty()).then_some(rest);
        }
        seen += analysis::tokens(analyzer, word).len();
        if seen > words {
            return None;
        }
    }
    None
}

/// The documents containing any of `terms`.
pub(crate) fn matching_docs(
    searcher: &tantivy::Searcher,
    terms: Vec<Term>,
) -> Result<HashSet<DocAddress>> {
    if terms.is_empty() {
        return Ok(HashSet::new());
    }
    let clauses = terms
        .into_iter()
        .map(|term| {
            let query: Box<dyn Query> = Box::new(TermQuery::new(term, IndexRecordOption::Basic));
            (Occur::Should, query)
        })
        .collect();
    Ok(searcher.search(&BooleanQuery::new(clauses), &DocSetCollector)?)
}

/// A user query, analyzed the same way as the indexed text.
#[derive(Debug, PartialEq)]
pub(crate) struct ParsedQuery {
    /// Distinct words, in query order: `U.S. Bank` -> `us`, `bank`. A
    /// [`COMPARISON_WORDS`] entry between two others is left out.
    pub(crate) words: Vec<String>,
    /// Each word in its other number ([`plumb_core::other_number`]), if it
    /// has one: `videos` -> `video`. A word of Arabic script has its
    /// [`analysis::folded`] form instead.
    pub(crate) others: Vec<Option<String>>,
    /// The whole query as one joined token: `U.S. Bank` -> `usbank`.
    pub(crate) joined: Option<String>,
    /// The first word, the first two joined, and so on (at most
    /// [`MAX_QUERY_WORDS`]), each with the number of query words it covers:
    /// `us bank login` -> `us` (1), `usbank` (2), `usbanklogin` (3). The
    /// names a site can have to be named by the query. A leading "the" may
    /// be left out: `the new york times` also gives `newyorktimes` (4).
    pub(crate) leading: Vec<(String, usize)>,
    /// The whole query as a kind ([`plumb_core::kind_key`]): `banks` -> `bank`.
    pub(crate) kind: Option<String>,
    /// Number of words in the query, repeats included.
    pub(crate) len: usize,
    /// The registrable domain, when the query is a hostname or URL.
    pub(crate) domain: Option<String>,
    /// [`RankConfig::terms_boost`](crate::RankConfig::terms_boost).
    pub(crate) terms_boost: f32,
    /// [`RankConfig::filler_words`](crate::RankConfig::filler_words).
    pub(crate) filler_words: bool,
    /// Asked as a question that names nothing
    /// ([`RankConfig::questions_name_nothing`](crate::RankConfig::questions_name_nothing)).
    pub(crate) asked: bool,
}

impl ParsedQuery {
    /// `None` when the query has no letters or digits.
    pub(crate) fn new(
        query: &str,
        words: &TextAnalyzer,
        joined: &TextAnalyzer,
    ) -> Option<ParsedQuery> {
        let query = truncate_chars(query, MAX_TEXT_CHARS);
        let tokens = analysis::tokens(words, &query);
        if tokens.is_empty() {
            return None;
        }
        let mut distinct = Vec::new();
        for (i, word) in tokens.iter().enumerate() {
            let compares =
                0 < i && i + 1 < tokens.len() && COMPARISON_WORDS.contains(&word.as_str());
            if !compares && !distinct.contains(word) {
                distinct.push(word.clone());
            }
        }
        distinct.truncate(MAX_QUERY_WORDS);
        let others = distinct
            .iter()
            .map(|word| other_number(word).or_else(|| analysis::folded(word)))
            .collect();
        let prefixes = |skip: usize| {
            tokens
                .iter()
                .skip(skip)
                .take(MAX_QUERY_WORDS)
                .scan(String::new(), move |key, word| {
                    key.push_str(word);
                    Some(key.clone())
                })
                .enumerate()
                .map(move |(i, key)| (key, skip + i + 1))
        };
        let mut leading: Vec<(String, usize)> = prefixes(0)
            .filter(|(key, words)| tokens.len() == 1 || *words > 1 || !is_function_word(key))
            .collect();
        if tokens.len() > 1 && tokens[0] == "the" {
            leading.extend(prefixes(1));
        }
        let kind = analysis::tokens(joined, &kind_key(&query))
            .into_iter()
            .next();
        Some(ParsedQuery {
            words: distinct,
            others,
            joined: analysis::tokens(joined, &query).into_iter().next(),
            leading,
            kind,
            len: tokens.len(),
            domain: typed_domain(&query),
            terms_boost: 0.0,
            filler_words: false,
            asked: false,
        })
    }

    /// Whether `word` is a [`FILLER_WORDS`] entry and they are taken as
    /// such.
    pub(crate) fn is_filler(&self, word: &str) -> bool {
        self.filler_words && FILLER_WORDS.contains(&word)
    }

    /// One boosted BM25 clause per word and field, plus the joined query on
    /// the joined and label fields and, for a typed domain, the domain itself.
    ///
    /// The label and joined fields hold whole names, so in a query of `n`
    /// words a single word matching there gets `1/n` of the field's boost:
    /// for "us bank online banking", bank.com (whose whole name is one of
    /// the words) must not outweigh usbank.com matching every word elsewhere.
    /// The fields each query word is searched in, with their boosts.
    fn per_word(&self, f: &Fields) -> [(Field, f32); 9] {
        let name_share = 1.0 / self.words.len() as f32;
        [
            (f.label, LABEL_BOOST * name_share),
            (f.joined, JOINED_BOOST * name_share),
            (f.aliases, ALIASES_BOOST),
            (f.title, TITLE_BOOST),
            (f.anchors, ANCHORS_BOOST),
            (f.description, DESCRIPTION_BOOST),
            (f.headings, HEADINGS_BOOST),
            (f.about, ABOUT_BOOST),
            (f.terms, self.terms_boost),
        ]
    }

    /// The clauses for the `i`th query word: the word in every field, and
    /// its other number in the fields of free text, a little weaker, so
    /// "videos" finds a site about "video" but names stay exact. An Arabic
    /// word's folded form is taken the same way: `اخبار` finds `أخبار`, a
    /// little below what is written `اخبار`.
    ///
    /// The other number never counts for more than the word as typed: when
    /// "videos" is common and "video" rare, BM25 would make the rare form
    /// outweigh every site that has the query's own words, so its boost is
    /// scaled down to the typed word's rarity.
    pub(crate) fn word_clauses(
        &self,
        i: usize,
        searcher: &tantivy::Searcher,
        f: &Fields,
        clauses: &mut Clauses,
    ) -> Result<()> {
        let word = &self.words[i];
        // Only a word after others: "to" in "to do list" is a word of the
        // thing looked for, "in" in "ups sign in" is not.
        let joining = i > 0 && is_function_word(word);
        let names = (self.len == 1 || !joining) && !self.asked;
        // A filler word is no name, nor what a site calls itself or is
        // called by others: who.int's title and link text say "WHO".
        let filler = self.len > 1 && self.is_filler(word);
        for (field, boost) in self.per_word(f) {
            let name_field = field == f.label || field == f.joined || field == f.aliases;
            if boost <= 0.0
                || (!names && name_field)
                || (filler && (name_field || field == f.title || field == f.anchors))
            {
                continue;
            }
            clauses.add_word(field, word, boost);
        }
        let Some(other) = &self.others[i] else {
            return Ok(());
        };
        let docs = searcher.num_docs() as f32;
        let rarity = |term: &Term| -> Result<f32> {
            let found = searcher.doc_freq(term)? as f32;
            Ok((1.0 + (docs - found + 0.5) / (found + 0.5)).ln())
        };
        for (field, boost) in self.per_word(f) {
            if boost <= 0.0
                || field == f.label
                || field == f.joined
                || (filler && (field == f.aliases || field == f.title || field == f.anchors))
            {
                continue;
            }
            let other = Term::from_field_text(field, other);
            let other_rarity = rarity(&other)?;
            if other_rarity <= 0.0 {
                continue;
            }
            let share = (rarity(&Term::from_field_text(field, word))? / other_rarity).min(1.0);
            clauses.add(other, boost * OTHER_NUMBER_SHARE * share);
        }
        Ok(())
    }

    /// The share of the query's words each of `docs` has in its text, in
    /// the order given: 1 for a site whose name is the whole query joined.
    pub(crate) fn coverage(
        &self,
        searcher: &tantivy::Searcher,
        f: &Fields,
        docs: &[DocAddress],
    ) -> Result<HashMap<DocAddress, f32>> {
        let mut covered: HashMap<DocAddress, f32> = docs.iter().map(|&addr| (addr, 0.0)).collect();
        if docs.is_empty() {
            return Ok(covered);
        }
        let share = 1.0 / self.words.len() as f32;
        for i in 0..self.words.len() {
            let mut clauses = Clauses::default();
            self.word_clauses(i, searcher, f, &mut clauses)?;
            for (bm25, addr) in bm25_of(searcher, &clauses.into_query(), docs.to_vec())? {
                if bm25 > 0.0 {
                    *covered.entry(addr).or_default() += share;
                }
            }
        }
        if let Some(joined) = &self.joined {
            let mut clauses = Clauses::default();
            clauses.add(Term::from_field_text(f.joined, joined), 1.0);
            clauses.add(Term::from_field_text(f.label, joined), 1.0);
            for (bm25, addr) in bm25_of(searcher, &clauses.into_query(), docs.to_vec())? {
                if bm25 > 0.0 {
                    covered.insert(addr, 1.0);
                }
            }
        }
        for share in covered.values_mut() {
            *share = share.min(1.0);
        }
        Ok(covered)
    }

    /// Which of `docs` lack one of the query's main words: those other
    /// than [`FILLER_WORDS`] and [`plumb_core::FUNCTION_WORDS`]. None for a
    /// query of one main word or fewer.
    pub(crate) fn missing_main_words(
        &self,
        searcher: &tantivy::Searcher,
        f: &Fields,
        docs: &[DocAddress],
    ) -> Result<HashSet<DocAddress>> {
        let main: Vec<usize> = (0..self.words.len())
            .filter(|&i| {
                let word = self.words[i].as_str();
                !is_function_word(word) && !FILLER_WORDS.contains(&word)
            })
            .collect();
        let mut missing = HashSet::new();
        if main.len() < 2 || docs.is_empty() {
            return Ok(missing);
        }
        for i in main {
            let mut clauses = Clauses::default();
            self.word_clauses(i, searcher, f, &mut clauses)?;
            for (bm25, addr) in bm25_of(searcher, &clauses.into_query(), docs.to_vec())? {
                if bm25 <= 0.0 {
                    missing.insert(addr);
                }
            }
        }
        if let Some(joined) = &self.joined {
            let mut clauses = Clauses::default();
            clauses.add(Term::from_field_text(f.joined, joined), 1.0);
            clauses.add(Term::from_field_text(f.label, joined), 1.0);
            for (bm25, addr) in bm25_of(searcher, &clauses.into_query(), docs.to_vec())? {
                if bm25 > 0.0 {
                    missing.remove(&addr);
                }
            }
        }
        Ok(missing)
    }

    pub(crate) fn text_query(
        &self,
        searcher: &tantivy::Searcher,
        f: &Fields,
    ) -> Result<BooleanQuery> {
        let mut clauses = Clauses::default();
        for i in 0..self.words.len() {
            self.word_clauses(i, searcher, f, &mut clauses)?;
        }
        if let Some(joined) = &self.joined {
            clauses.add(Term::from_field_text(f.joined, joined), WHOLE_QUERY_BOOST);
            clauses.add(Term::from_field_text(f.label, joined), WHOLE_QUERY_BOOST);
        }
        if let Some(domain) = &self.domain {
            clauses.add(Term::from_field_text(f.domain, domain), DOMAIN_BOOST);
        }
        Ok(clauses.into_query())
    }
}

/// Boosted clauses of a disjunction, each of one term or of all of a
/// word's [`analysis::pieces`]. A clause added twice keeps the larger
/// boost (in a one-word query, the word is also the whole query).
#[derive(Default)]
pub(crate) struct Clauses(Vec<(Vec<Term>, f32)>);

impl Clauses {
    pub(crate) fn add(&mut self, term: Term, boost: f32) {
        self.add_all(vec![term], boost);
    }

    /// `word` in the word field `field`: a word of Chinese, Japanese or
    /// Thai by all of its pieces, which share the boost, and also whole,
    /// as an index built before it was in pieces holds it.
    pub(crate) fn add_word(&mut self, field: Field, word: &str, boost: f32) {
        let pieces = analysis::pieces(word);
        if pieces.len() > 1 {
            let terms = pieces
                .iter()
                .map(|piece| Term::from_field_text(field, piece));
            self.add_all(terms.collect(), boost);
        }
        self.add(Term::from_field_text(field, word), boost);
    }

    fn add_all(&mut self, terms: Vec<Term>, boost: f32) {
        match self.0.iter_mut().find(|(known, _)| *known == terms) {
            Some((_, known_boost)) => *known_boost = known_boost.max(boost),
            None => self.0.push((terms, boost)),
        }
    }

    pub(crate) fn into_query(self) -> BooleanQuery {
        // Fields indexed without frequencies fall back to `Basic`.
        let term_query = |term| -> Box<dyn Query> {
            Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs))
        };
        let clauses = self
            .0
            .into_iter()
            .map(|(mut terms, boost)| {
                // The pieces of a word share its boost.
                let boost = boost / terms.len() as f32;
                let query = if terms.len() == 1 {
                    term_query(terms.remove(0))
                } else {
                    let all = terms.into_iter().map(|t| (Occur::Must, term_query(t)));
                    Box::new(BooleanQuery::new(all.collect()))
                };
                let query: Box<dyn Query> = Box::new(BoostQuery::new(query, boost));
                (Occur::Should, query)
            })
            .collect();
        BooleanQuery::new(clauses)
    }
}
