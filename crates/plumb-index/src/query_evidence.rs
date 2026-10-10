//! Query evidence kept separately from the blended/display score. A domain
//! word and a place in the nearest-vector list do not establish a subject.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tantivy::{DocAddress, Term};

use crate::{
    bm25_of, is_function_word, without_intent_words, Clauses, Fields, ParsedQuery, FILLER_WORDS,
};

/// Evidence for one site in [`crate::CandidatePool`], before pruning,
/// spelling replacement, source routing, or learned reordering. In
/// particular, `lexical_score` never becomes a display/order score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateEvidence {
    pub domain: String,
    /// Raw BM25, before normalization relative to this candidate pool.
    pub lexical_score: f32,
    /// Absolute semantic similarity, including when name controls disable
    /// score blending. Missing vectors remain unknown rather than becoming
    /// zero relevance.
    pub semantic_closeness: Option<f32>,
    /// Weighted coverage in any searched field, ignoring question/filler words.
    pub query_coverage: f32,
    /// Coverage in title, description, Wikidata description, headings, terms and aliases.
    pub substantive_coverage: f32,
    /// Coverage of the substantive words after a leading partial name.
    pub remaining_coverage: f32,
    pub full_name: bool,
    pub typed_domain: bool,
    pub partial_name_words: usize,
    /// Convincing lexical or semantic evidence precedes popularity. Zero
    /// denotes a weak fallback, not proof that the site is irrelevant.
    pub relevance_tier: u8,
    /// Explainable source evidence, when present; never a TLD/popularity verdict.
    pub source_quality: Option<crate::health::SourceQualityEvidence>,
}

#[derive(Default)]
pub(crate) struct LexicalEvidence {
    /// Legacy unweighted coverage, including function words, used for the
    /// existing missing-vector fallback and exact-name controls.
    pub(crate) words: f32,
    pub(crate) coverage: f32,
    pub(crate) substantive: f32,
    matched: Vec<bool>,
    weights: Vec<f32>,
}

impl LexicalEvidence {
    pub(crate) fn remaining(&self, prefix_words: usize) -> f32 {
        let total: f32 = self.weights.iter().skip(prefix_words).sum();
        if total <= 0.0 {
            return 1.0;
        }
        self.weights
            .iter()
            .zip(&self.matched)
            .skip(prefix_words)
            .filter_map(|(&weight, &matched)| matched.then_some(weight))
            .sum::<f32>()
            .max(0.0)
            / total
    }
}

impl ParsedQuery {
    /// Read postings for the bounded candidate pool, with all candidates
    /// treated alike whether they have vectors or name matches. Rarity
    /// weights are bounded so a rare modifier cannot dominate the subject.
    pub(crate) fn whole_query_evidence(
        &self,
        searcher: &tantivy::Searcher,
        fields: &Fields,
        docs: &[DocAddress],
    ) -> Result<HashMap<DocAddress, LexicalEvidence>> {
        // Only an actual trailing intent is a modifier. "Center" in
        // Chase Center must not lose its subject weight merely because
        // "help center" also occurs in the existing intent vocabulary.
        let subject_words = without_intent_words(&self.words.join(" ")).map(|subject| {
            subject
                .split_whitespace()
                .map(str::to_string)
                .collect::<HashSet<_>>()
        });
        let mut weights = Vec::with_capacity(self.words.len());
        for word in &self.words {
            let weight = if is_function_word(word) || FILLER_WORDS.contains(&word.as_str()) {
                0.0
            } else if ["online", "watch", "read", "find", "learn"].contains(&word.as_str()) {
                0.1
            } else if subject_words
                .as_ref()
                .is_some_and(|subject| !subject.contains(word))
            {
                0.25
            } else {
                let frequency =
                    fields
                        .substantive()
                        .into_iter()
                        .try_fold(0u64, |count, field| {
                            searcher
                                .doc_freq(&Term::from_field_text(field, word))
                                .map(|n| count.saturating_add(n))
                        })?;
                (1.0 + (searcher.num_docs() as f32 / (1.0 + frequency as f32)).ln_1p())
                    .clamp(1.0, 3.0)
            };
            weights.push(weight);
        }
        let total: f32 = weights.iter().sum();
        let mut evidence: HashMap<_, _> = docs
            .iter()
            .map(|&addr| {
                (
                    addr,
                    LexicalEvidence {
                        matched: vec![false; weights.len()],
                        weights: weights.clone(),
                        ..Default::default()
                    },
                )
            })
            .collect();
        if docs.is_empty() {
            return Ok(evidence);
        }
        for (i, &weight) in weights.iter().enumerate() {
            let mut clauses = Clauses::default();
            self.word_clauses(i, searcher, fields, &mut clauses)?;
            for (bm25, addr) in bm25_of(searcher, &clauses.into_query(), docs.to_vec())? {
                if bm25 > 0.0 {
                    let entry = evidence.get_mut(&addr).expect("bounded candidate");
                    entry.words += 1.0 / self.words.len() as f32;
                    entry.coverage += weight;
                }
            }
            if weight <= 0.0 {
                continue;
            }
            let mut substantive = Clauses::default();
            for field in fields.substantive() {
                if field == fields.terms && self.terms_boost <= 0.0 {
                    continue;
                }
                substantive.add(Term::from_field_text(field, &self.words[i]), 1.0);
                if let Some(other) = &self.others[i] {
                    substantive.add(Term::from_field_text(field, other), 1.0);
                }
            }
            for (bm25, addr) in bm25_of(searcher, &substantive.into_query(), docs.to_vec())? {
                if bm25 > 0.0 {
                    let entry = evidence.get_mut(&addr).expect("bounded candidate");
                    entry.substantive += weight;
                    entry.matched[i] = true;
                }
            }
        }
        for entry in evidence.values_mut() {
            entry.words = entry.words.min(1.0);
            entry.coverage = if total > 0.0 {
                (entry.coverage / total).min(1.0)
            } else {
                entry.words
            };
            entry.substantive = if total > 0.0 {
                (entry.substantive / total).min(1.0)
            } else {
                entry.words
            };
        }
        Ok(evidence)
    }
}
