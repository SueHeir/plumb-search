//! Entity linking: the article a part of the query means when the rest of
//! the query says more than any article does. "how did napoleon become
//! emperor" means Napoleon, "hamstring insertion point" Hamstring, though
//! neither article is named by the whole query or covers most of it.
//!
//! A part links to an article when it names that article far more often
//! than any other (page views stand in for how often a name means each
//! article), no longer part naming a page holds it, and either the article
//! says another word of the query or the part is the article's plain title
//! and has what the query is most about.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{DocAddress, Searcher, TantivyDocument, Term};

use super::scoring::{scoped_query, Described};
use super::sites::{short_titles, title_qualifier};
use super::{Page, PageSearcher, ASKING_WORDS, MEANT_READS, TITLE_INSIDE_CANDIDATES};
use crate::analysis;

impl PageSearcher {
    /// The articles within `scope` that parts of the query `words` mean,
    /// each with its part, added to `addresses` when not found otherwise.
    ///
    /// The longest part naming a page wins: "aws lambda" names AWS Lambda,
    /// so is not about Lambda, nor "moby dick herman melville" about Moby.
    /// A part with the query's heaviest word ([`PageSearcher::heaviest_word`])
    /// links only when more words follow it, or it follows a word like
    /// "in": the last word of a query names what is wanted of the rest
    /// ("chase login", "dunoon webcam"), unless it is what is asked about
    /// ("cities in malta").
    pub(super) fn linked(
        &self,
        searcher: &Searcher,
        words: &[String],
        sharped: &[String],
        described: &[Described],
        scope: Option<&dyn Query>,
        addresses: &mut Vec<DocAddress>,
    ) -> Result<HashMap<DocAddress, Vec<String>>> {
        let mut named = Vec::new();
        for start in 0..words.len() {
            for end in start + 1..=words.len() {
                let span = &words[start..end];
                if span.iter().all(|word| {
                    crate::is_function_word(word) || ASKING_WORDS.contains(&word.as_str())
                }) {
                    continue;
                }
                let Some(key) = self.span_key(span, sharped)? else {
                    continue;
                };
                let names = |field| searcher.doc_freq(&Term::from_field_text(field, &key));
                if names(self.fields.keys)? > 0 || names(self.fields.aliases)? > 0 {
                    named.push((start, end, key));
                }
            }
        }
        let heaviest = self.heaviest_word(searcher, described)?;
        let mut linked = HashMap::new();
        for (start, end, key) in &named {
            let (start, end) = (*start, *end);
            let held = named
                .iter()
                .any(|&(s, e, _)| s <= start && end <= e && e - s > end - start);
            // A name joins its words, so "a cast" would be Acast.
            let joins = |word: &String| crate::is_function_word(word) && word != "the";
            let span = &words[start..end];
            if end - start == words.len()
                || held
                || span.first().is_some_and(joins)
                || span
                    .last()
                    .is_some_and(|word| crate::is_function_word(word))
            {
                continue;
            }
            let Some((address, page)) = self.most_meant(searcher, key, scope)? else {
                continue;
            };
            let stems: HashSet<String> = span
                .iter()
                .flat_map(|word| analysis::tokens(&self.stemmed, word))
                .collect();
            let fits = self.said(&page).is_some_and(|said| {
                described.iter().any(|word| {
                    !stems.contains(&word.stem)
                        && !crate::is_function_word(&word.form)
                        && said.says(word)
                })
            });
            let subject = self.titled(&page, key)
                && heaviest.is_some_and(|heaviest| stems.contains(heaviest))
                && (end < words.len() || start > 0 && crate::is_function_word(&words[start - 1]));
            if fits || subject {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
                linked.insert(address, span.to_vec());
            }
        }
        Ok(linked)
    }

    /// Whether `key` is the article's plain title, or a short form of it
    /// ("no way home"). Wikipedia gives a name's plain title to what the
    /// name usually means, so an alias or a title with a qualifier needs
    /// the article to say more of the query to be linked: "how to fix a
    /// squeaky floor" is not about Squeaky Fromme, nor "maven central"
    /// about Maven (wrestler).
    fn titled(&self, page: &Page, key: &str) -> bool {
        title_qualifier(&page.title).is_none()
            && std::iter::once(page.title.as_str())
                .chain(short_titles(&page.title))
                .any(|name| analysis::tokens(&self.joined, name).pop().as_deref() == Some(key))
    }

    /// The stem of `described` the fewest pages say of themselves (their
    /// names and descriptions), of those some page says: what the query is
    /// most about. "hamstring" of "hamstring insertion point".
    fn heaviest_word<'a>(
        &self,
        searcher: &Searcher,
        described: &'a [Described],
    ) -> Result<Option<&'a str>> {
        let mut heaviest: Option<(u64, &str)> = None;
        for word in described {
            let found = searcher.doc_freq(&Term::from_field_text(self.fields.about, &word.stem))?
                + searcher.doc_freq(&Term::from_field_text(self.fields.lead, &word.stem))?;
            if found > 0 && heaviest.is_none_or(|(least, _)| found < least) {
                heaviest = Some((found, &word.stem));
            }
        }
        Ok(heaviest.map(|(_, stem)| stem))
    }

    /// The article `key` names, by a name or an alias, that is read
    /// [`MEANT_READS`] times as much as any other article of its language
    /// that `key` names: "napoleon" is Napoleon, not Napoleon (2023 film);
    /// "mercury" is no one article; "trephining" is Trepanning.
    fn most_meant(
        &self,
        searcher: &Searcher,
        key: &str,
        scope: Option<&dyn Query>,
    ) -> Result<Option<(DocAddress, Page)>> {
        let named = |field| -> (Occur, Box<dyn Query>) {
            (
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(field, key),
                    IndexRecordOption::Basic,
                )),
            )
        };
        let articles = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(BooleanQuery::new(vec![
                    named(self.fields.keys),
                    named(self.fields.aliases),
                ])) as Box<dyn Query>,
            ),
            (Occur::Must, self.scope_term("under:wikipedia.org")),
        ]);
        let mut most: Option<(DocAddress, Page)> = None;
        for (_, address) in searcher.search(
            self.language_query(scoped_query(&articles, scope).as_ref())
                .as_ref(),
            &TopDocs::with_limit(TITLE_INSIDE_CANDIDATES)
                .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc),
        )? {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            match &most {
                _ if !page.is_article() => {}
                None => most = Some((address, page)),
                Some((_, first)) if first.set == page.set => {
                    return Ok(most.filter(|(_, first)| first.views >= MEANT_READS * page.views));
                }
                Some(_) => {}
            }
        }
        Ok(most)
    }
}
