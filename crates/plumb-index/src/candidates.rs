//! A query's deep site candidates with the raw signals a first stage could
//! score them by: which of the query's words each field of a site has, how
//! long its fields are, and how rare each word is in each field. It is for
//! designing the first stage offline (`plumb-tools eval --candidates-out`);
//! nodes never call it.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use plumb_core::is_function_word;
use serde::Serialize;
use tantivy::collector::TopDocs;
use tantivy::postings::Postings;
use tantivy::query::{BooleanQuery, Occur, Query};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocAddress, DocSet, Term};

use crate::query::{matching_docs, Clauses, ParsedQuery};
use crate::rank::{bm25_of, domain_of};
use crate::{pages, Meaning, RankConfig, Searcher, FILLER_WORDS};

/// The fields [`CandidateSignals`] reads, in the order its lists give them.
pub const FIELDS: [&str; 9] = [
    "label",
    "joined",
    "aliases",
    "title",
    "anchors",
    "description",
    "headings",
    "about",
    "terms",
];

/// What [`Searcher::candidate_signals`] found for a query.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CandidateSignals {
    /// The query's words as searched, then the other number of each that
    /// has one ("video" for "videos").
    pub words: Vec<String>,
    /// For each of `words`, the word it is the other number of.
    pub other_of: Vec<Option<usize>>,
    /// Sites in the index.
    pub docs: u64,
    /// Each field's average length in words, over all sites.
    pub avg_len: Vec<f32>,
    /// For each field, how many sites have each of `words` in it.
    pub df: Vec<Vec<u64>>,
    pub sites: Vec<SiteSignals>,
}

/// One candidate site of [`CandidateSignals`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct SiteSignals {
    pub domain: String,
    pub link_score: f32,
    /// Its score in today's first stage: the boosted BM25 sum over words
    /// and fields that picks [`RankConfig::candidates`].
    pub bm25: f32,
    /// Its place in today's first stage, from 1, within the depth asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub words_rank: Option<u32>,
    /// Its place among the sites with every main word of the query, by the
    /// same score, within the depth asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_rank: Option<u32>,
    /// Its place among the sites nearest the query in meaning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meaning_rank: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closeness: Option<f32>,
    /// The length in words of each field, as BM25 reads it (0 for the
    /// joined names, which are not measured).
    pub len: Vec<u32>,
    /// `(word, field, count)` for each of the query's words a field has.
    pub tf: Vec<(usize, usize, u32)>,
    /// The whole query, joined, is one of its names or its label.
    pub joined: bool,
}

impl Searcher {
    /// The first `depth` sites by today's first stage, the first `depth`
    /// with every main word of the query, the first `depth` nearest it in
    /// meaning, and the sites of `domains`, each with its raw signals.
    /// `None` for a query with no words.
    pub fn candidate_signals(
        &self,
        query_text: &str,
        depth: usize,
        cfg: &RankConfig,
        meaning: Option<&dyn Meaning>,
        domains: &[String],
    ) -> Result<Option<CandidateSignals>> {
        let Some(mut query) = ParsedQuery::new(query_text, &self.words, &self.joined) else {
            return Ok(None);
        };
        query.terms_boost = cfg.terms_boost;
        query.filler_words = cfg.filler_words;
        query.asked =
            cfg.questions_name_nothing && query.len >= 3 && pages::asked_as_question(query_text);
        let searcher = self.reader.searcher();
        let num_docs = usize::try_from(searcher.num_docs()).unwrap_or(usize::MAX);
        if num_docs == 0 {
            return Ok(None);
        }
        query.add_marked_spellings(&searcher, &self.fields)?;
        let text_query = query.text_query(&searcher, &self.fields)?;
        let top = TopDocs::with_limit(depth.max(1).min(num_docs)).order_by_score();
        let mut places: HashMap<DocAddress, [Option<u32>; 3]> = HashMap::new();
        for (at, (_, addr)) in searcher.search(&text_query, &top)?.into_iter().enumerate() {
            places.entry(addr).or_default()[0] = Some(at as u32 + 1);
        }
        let main: Vec<usize> = (0..query.words.len())
            .filter(|&i| {
                let word = query.words[i].as_str();
                !is_function_word(word) && !FILLER_WORDS.contains(&word)
            })
            .collect();
        if main.len() >= 2 {
            let mut every: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for &i in &main {
                let mut clauses = Clauses::default();
                query.word_clauses(i, &searcher, &self.fields, &mut clauses)?;
                every.push((Occur::Must, Box::new(clauses.into_query())));
            }
            every.push((Occur::Should, Box::new(text_query.clone())));
            let every = BooleanQuery::new(every);
            for (at, (_, addr)) in searcher.search(&every, &top)?.into_iter().enumerate() {
                places.entry(addr).or_default()[1] = Some(at as u32 + 1);
            }
        }
        let domain_docs = |domains: &[String]| -> Result<Vec<(String, HashSet<DocAddress>)>> {
            domains
                .iter()
                .map(|domain| {
                    let term = Term::from_field_text(self.fields.domain, domain);
                    Ok((domain.clone(), matching_docs(&searcher, vec![term])?))
                })
                .collect()
        };
        let nearest = meaning.map(|meaning| meaning.nearest()).unwrap_or_default();
        let nearest = &nearest[..nearest.len().min(depth)];
        for (at, (_, docs)) in domain_docs(nearest)?.into_iter().enumerate() {
            for addr in docs {
                places.entry(addr).or_default()[2] = Some(at as u32 + 1);
            }
        }
        for (_, docs) in domain_docs(domains)? {
            for addr in docs {
                places.entry(addr).or_default();
            }
        }

        let fields = self.dumped_fields();
        let mut words = query.words.clone();
        let mut other_of = vec![None; words.len()];
        for (i, other) in query.others.iter().enumerate() {
            if let Some(other) = other {
                words.push(other.clone());
                other_of.push(Some(i));
            }
        }
        let (avg_len, df) = field_stats(&searcher, &fields, &words)?;

        let mut addrs: Vec<DocAddress> = places.keys().copied().collect();
        addrs.sort_unstable();
        let bm25: HashMap<DocAddress, f32> = bm25_of(&searcher, &text_query, addrs.clone())?
            .into_iter()
            .map(|(score, addr)| (addr, score))
            .collect();
        let joined: HashSet<DocAddress> = match &query.joined {
            Some(joined) => matching_docs(
                &searcher,
                vec![
                    Term::from_field_text(self.fields.joined, joined),
                    Term::from_field_text(self.fields.label, joined),
                ],
            )?,
            None => HashSet::new(),
        };
        let mut tf: HashMap<DocAddress, Vec<(usize, usize, u32)>> = HashMap::new();
        for (f, &field) in fields.iter().enumerate() {
            for (w, word) in words.iter().enumerate() {
                let term = Term::from_field_text(field, word);
                for (addr, count) in term_counts(&searcher, &term, &addrs)? {
                    tf.entry(addr).or_default().push((w, f, count));
                }
            }
        }
        let mut sites = Vec::with_capacity(addrs.len());
        for addr in addrs {
            let Some(domain) = domain_of(&searcher, addr) else {
                continue;
            };
            let segment = searcher.segment_reader(addr.segment_ord);
            let mut len = Vec::with_capacity(fields.len());
            for &field in &fields {
                len.push(match segment.fieldnorms_readers().get_field(field)? {
                    Some(norms) => norms.fieldnorm(addr.doc_id),
                    None => 0,
                });
            }
            let [words_rank, all_rank, meaning_rank] = places[&addr];
            let closeness = meaning.and_then(|meaning| meaning.closeness(&domain));
            sites.push(SiteSignals {
                link_score: self.link_score(&searcher, addr)?,
                bm25: bm25.get(&addr).copied().unwrap_or(0.0),
                words_rank,
                all_rank,
                meaning_rank,
                closeness,
                len,
                tf: tf.remove(&addr).unwrap_or_default(),
                joined: joined.contains(&addr),
                domain,
            });
        }
        Ok(Some(CandidateSignals {
            words,
            other_of,
            docs: num_docs as u64,
            avg_len,
            df,
            sites,
        }))
    }

    /// The fields of [`FIELDS`], in its order.
    fn dumped_fields(&self) -> [Field; FIELDS.len()] {
        let f = &self.fields;
        [
            f.label,
            f.joined,
            f.aliases,
            f.title,
            f.anchors,
            f.description,
            f.headings,
            f.about,
            f.terms,
        ]
    }

    fn link_score(&self, searcher: &tantivy::Searcher, addr: DocAddress) -> Result<f32> {
        let column = searcher
            .segment_reader(addr.segment_ord)
            .fast_fields()
            .f64(crate::schema::LINK_SCORE)?;
        Ok(column.first(addr.doc_id).unwrap_or(0.0) as f32)
    }
}

/// Each of `fields`' average length in words, and how many sites have
/// each of `words` in it.
fn field_stats(
    searcher: &tantivy::Searcher,
    fields: &[Field],
    words: &[String],
) -> Result<(Vec<f32>, Vec<Vec<u64>>)> {
    let docs = searcher.num_docs().max(1) as f32;
    let mut avg_len = Vec::with_capacity(fields.len());
    let mut df = Vec::with_capacity(fields.len());
    for &field in fields {
        let mut tokens = 0u64;
        for segment in searcher.segment_readers() {
            tokens += segment.inverted_index(field)?.total_num_tokens();
        }
        avg_len.push(tokens as f32 / docs);
        df.push(
            words
                .iter()
                .map(|word| searcher.doc_freq(&Term::from_field_text(field, word)))
                .collect::<tantivy::Result<Vec<_>>>()?,
        );
    }
    Ok((avg_len, df))
}

/// How many times each of `addrs` (in order) has `term`, for those that
/// have it.
fn term_counts(
    searcher: &tantivy::Searcher,
    term: &Term,
    addrs: &[DocAddress],
) -> Result<Vec<(DocAddress, u32)>> {
    let mut counts = Vec::new();
    let mut at = 0;
    while at < addrs.len() {
        let segment_ord = addrs[at].segment_ord;
        let end = addrs[at..]
            .iter()
            .position(|addr| addr.segment_ord != segment_ord)
            .map_or(addrs.len(), |n| at + n);
        let inverted = searcher
            .segment_reader(segment_ord)
            .inverted_index(term.field())?;
        if let Some(mut postings) = inverted.read_postings(term, IndexRecordOption::WithFreqs)? {
            for &addr in &addrs[at..end] {
                if postings.doc() < addr.doc_id {
                    postings.seek(addr.doc_id);
                }
                if postings.doc() == addr.doc_id {
                    counts.push((addr, postings.term_freq()));
                }
            }
        }
        at = end;
    }
    Ok(counts)
}
