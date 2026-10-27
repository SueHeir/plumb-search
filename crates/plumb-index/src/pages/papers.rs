//! Searching papers by title, author, year, venue and date.

use std::collections::HashSet;
use std::ops::Bound;

use anyhow::Result;
use plumb_core::paper_query::{day_number, publication_year, PaperQuery};
use plumb_core::Operators;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, RangeQuery, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{TantivyDocument, Term};

use super::placing::operators_allow;
use super::search::{hinted_name, Hint};
use super::{
    Page, PageHit, PageSearcher, CANDIDATES, PARTIAL_MATCH, POPULARITY_SCALE, POPULARITY_SHARE,
};
use crate::analysis;

impl PageSearcher {
    /// Paper eligibility, language and publication bounds apply before
    /// both popularity and date candidate limits. Newest ordering stays
    /// within relevance tiers: exact names, title matches, then bylines.
    /// The extra date collector prevents older popular papers from
    /// exhausting a newest request's candidate budget.
    pub fn search_papers(&self, query: &PaperQuery, limit: usize) -> Result<Vec<PageHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let ops = Operators::parse(&query.query);
        // "gene family" is a topic, not a person's, "transformer review" a
        // kind of paper, not a film's, and "trial results" not an event's.
        let words = hinted_name(&ops.words)
            .filter(|(_, hint)| {
                !matches!(
                    hint,
                    Hint::Aspect | Hint::Event | Hint::Person | Hint::Work(_)
                )
            })
            .map_or_else(|| ops.words.clone(), |(name, _)| name);
        let mut clauses: Vec<(Occur, Box<dyn Query>)> =
            vec![(Occur::Must, self.scope_term("set:papers"))];
        if let Some(scope) = self.page_scope(&ops, false) {
            clauses.push((Occur::Must, scope));
        }
        for (bound, after) in [(query.after, true), (query.before, false)] {
            if let Some(bound) = bound {
                let (field, value) = bound
                    .day
                    .map_or((self.fields.paper_year, bound.year), |day| {
                        (self.fields.paper_day, day)
                    });
                let low = if after { value } else { 1 };
                let high = if after { u64::MAX } else { value };
                clauses.push((
                    Occur::Must,
                    Box::new(RangeQuery::new(
                        Bound::Included(Term::from_field_u64(field, low)),
                        Bound::Included(Term::from_field_u64(field, high)),
                    )),
                ));
            }
        }
        let stems = analysis::tokens(&self.stemmed, &words);
        for stem in &stems {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.topic, stem),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        // A stop-word-only title remains an exact name query, while a
        // date-only request intentionally lists the constrained corpus.
        if stems.is_empty() && !words.trim().is_empty() {
            if let Some(key) = analysis::tokens(&self.joined, &words).pop() {
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.keys, &key),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
        }
        let eligible = self.language_query(&BooleanQuery::new(clauses));
        let searcher = self.reader.searcher();
        let mut addresses: Vec<_> = searcher
            .search(
                eligible.as_ref(),
                &TopDocs::with_limit(CANDIDATES)
                    .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc),
            )?
            .into_iter()
            .map(|(_, addr)| addr)
            .collect();
        if query.newest {
            for field in ["paper_day", "paper_year"] {
                for (_, addr) in searcher.search(
                    eligible.as_ref(),
                    &TopDocs::with_limit(CANDIDATES)
                        .order_by_fast_field::<u64>(field, tantivy::Order::Desc),
                )? {
                    if !addresses.contains(&addr) {
                        addresses.push(addr);
                    }
                }
            }
        }
        let joined = analysis::tokens(&self.joined, &words)
            .pop()
            .unwrap_or_default();
        // Exact identity/title candidates get a reserved collector, so
        // popularity and recent topical matches cannot hide a named paper.
        if !joined.is_empty() {
            let named = BooleanQuery::new(vec![
                (Occur::Must, eligible.box_clone()),
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.keys, &joined),
                        IndexRecordOption::Basic,
                    )),
                ),
            ]);
            for (_, address) in searcher.search(
                &named,
                &TopDocs::with_limit(CANDIDATES)
                    .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc),
            )? {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        let raw_words = analysis::tokens(&self.words, &words);
        let word_set: HashSet<_> = raw_words.iter().map(String::as_str).collect();
        let mut ranked = Vec::new();
        for address in addresses {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(page) = self.read_page(&document)? else {
                continue;
            };
            if !query.allows(page.paper.as_ref()) || !operators_allow(&ops, &page) {
                continue;
            }
            let popularity = document
                .get_first(self.fields.popularity)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as f32
                / POPULARITY_SCALE;
            let (name, named) = self.name_match(&page, &words, &joined, &word_set);
            let whole = named || self.book_match(&page, &raw_words);
            let title_stems = analysis::tokens(&self.stemmed, &page.title);
            let title_match = stems.iter().all(|s| title_stems.contains(s));
            let tier = if whole {
                2
            } else if title_match {
                1
            } else {
                0
            };
            let relevance = if named {
                name
            } else if title_match {
                PARTIAL_MATCH
            } else {
                PARTIAL_MATCH * 0.8
            };
            ranked.push((
                tier,
                PageHit {
                    page,
                    score: relevance * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity),
                    named,
                    whole,
                    popularity,
                    learned: None,
                    meant: false,
                    unsure: false,
                },
            ));
        }
        ranked.sort_by(|(ta, a), (tb, b)| {
            tb.cmp(ta)
                .then_with(|| {
                    if query.newest {
                        let date = |p: &Page| {
                            p.paper.as_ref().and_then(|p| {
                                publication_year(p).map(|y| {
                                    (
                                        y,
                                        p.publication_date
                                            .as_deref()
                                            .and_then(day_number)
                                            .unwrap_or(0),
                                    )
                                })
                            })
                        };
                        date(&b.page).cmp(&date(&a.page))
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then_with(|| b.score.total_cmp(&a.score))
                .then_with(|| a.page.url.cmp(&b.page.url))
        });
        Ok(ranked.into_iter().take(limit).map(|(_, hit)| hit).collect())
    }
}
