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
//!
//! A node with page vectors also adds the page nearest the whole query in
//! meaning ([`PageSearcher::add_meant`]): "micro naps" means Microsleep,
//! "dextocardia" Dextrocardia, which no word names.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::{DocAddress, Searcher, TantivyDocument, Term};

use super::scoring::{scoped_query, Described};
use super::sites::{base_title, short_titles, title_qualifier};
use super::{
    Page, PageHit, PageSearcher, ASKING_WORDS, ENTITY_MATCH, MEANT_READS, MIN_PARTIAL_SCORE,
    POPULARITY_SHARE, TITLE_INSIDE_CANDIDATES,
};
use crate::analysis;

impl PageSearcher {
    /// The articles within `scope` that parts of the query `words` mean,
    /// each with its part, added to `addresses` when not found otherwise.
    ///
    /// The longest part naming a page wins: "aws lambda" names AWS Lambda,
    /// so is not about Lambda, nor "moby dick herman melville" about Moby.
    /// A part the article says nothing more of the query for links by the
    /// query's heaviest word ([`PageSearcher::heaviest_word`]) only when
    /// more words follow it, or it follows a word like "in": the last word
    /// of a query names what is wanted of the rest ("chase login", "dunoon
    /// webcam"), unless it is what is asked about ("cities in malta").
    pub(super) fn linked(
        &self,
        searcher: &Searcher,
        words: &[String],
        sharped: &[String],
        described: &[Described],
        scope: Option<&dyn Query>,
        addresses: &mut Vec<DocAddress>,
    ) -> Result<HashMap<DocAddress, Vec<String>>> {
        // A name joins its words, so "a lien" would be Alien and "a cast"
        // Acast: a part with a word like "a" at its edge names only a page
        // spelled with it ("a star is born"), and is never linked, as "go
        // to" would be The Mikado (one of its names is "Go to").
        let joins = |word: &String| crate::is_function_word(word) && word != "the";
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
                let edged = span.first().is_some_and(joins)
                    || span
                        .last()
                        .is_some_and(|word| crate::is_function_word(word));
                if (names(self.fields.keys)? > 0 || names(self.fields.aliases)? > 0)
                    && (!edged || self.spelled(searcher, &key, span.len())?)
                {
                    named.push((start, end, key, edged));
                }
            }
        }
        let heaviest = self.heaviest_word(searcher, described)?;
        let mut linked = HashMap::new();
        for &(start, end, ref key, edged) in &named {
            let held = named
                .iter()
                .any(|&(s, e, _, _)| s <= start && end <= e && e - s > end - start);
            if end - start == words.len() || held || edged {
                continue;
            }
            let span = &words[start..end];
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
    /// names, descriptions and leads), of those some page says: what the
    /// query is most about. "hamstring" of "hamstring insertion point".
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

    /// Whether the most read page `key` names has a name of `count` words
    /// joining to it, as the part of the query does.
    fn spelled(&self, searcher: &Searcher, key: &str, count: usize) -> Result<bool> {
        let Some((_, address)) = searcher
            .search(
                &self.naming(key),
                &TopDocs::with_limit(1)
                    .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc),
            )?
            .pop()
        else {
            return Ok(false);
        };
        let Some(page) = self.stored_page(searcher, address)? else {
            return Ok(false);
        };
        let title = std::iter::once(page.title.as_str()).chain(Some(base_title(&page.title)));
        Ok(title
            .chain(short_titles(&page.title))
            .chain(page.names.iter().chain(&page.aliases).map(String::as_str))
            .any(|name| {
                analysis::tokens(&self.words, name).len() == count
                    && analysis::tokens(&self.joined, name).pop().as_deref() == Some(key)
            }))
    }

    /// The pages `key` names, by a name or an alias.
    fn naming(&self, key: &str) -> BooleanQuery {
        let named = |field| -> (Occur, Box<dyn Query>) {
            (
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(field, key),
                    IndexRecordOption::Basic,
                )),
            )
        };
        BooleanQuery::new(vec![named(self.fields.keys), named(self.fields.aliases)])
    }

    /// The article `key` names, by a name or an alias, that is read
    /// [`MEANT_READS`] times as much as any other article of its language
    /// that `key` names: "napoleon" is Napoleon, not Napoleon (2023 film);
    /// "mercury" is no one article; "trephining" is Trepanning. None when
    /// the most read pages it names hold no other article of that
    /// language to weigh it against, but there may be more.
    fn most_meant(
        &self,
        searcher: &Searcher,
        key: &str,
        scope: Option<&dyn Query>,
    ) -> Result<Option<(DocAddress, Page)>> {
        let articles = BooleanQuery::new(vec![
            (Occur::Must, Box::new(self.naming(key)) as Box<dyn Query>),
            (Occur::Must, self.scope_term("under:wikipedia.org")),
        ]);
        let found = searcher.search(
            self.language_query(scoped_query(&articles, scope).as_ref())
                .as_ref(),
            &TopDocs::with_limit(TITLE_INSIDE_CANDIDATES)
                .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc),
        )?;
        let more = found.len() == TITLE_INSIDE_CANDIDATES;
        let mut most: Option<(DocAddress, Page)> = None;
        for (_, address) in found {
            let Some(page) = self.stored_page(searcher, address)? else {
                continue;
            };
            match &most {
                _ if !page.is_article() => {}
                None => most = Some((address, page)),
                Some((_, first)) if first.set == page.set => {
                    return Ok(most.filter(|(_, first)| {
                        first.views >= MEANT_READS.saturating_mul(page.views)
                    }));
                }
                Some(_) => {}
            }
        }
        Ok(most.filter(|_| !more))
    }

    /// Adds to `found` (best first) the first page of `urls` the index
    /// holds, the pages nearest the query in meaning
    /// ([`crate::Meaning::nearest_pages`]), scored as a linked article is
    /// ([`ENTITY_MATCH`]) and marked [`PageHit::meant`], so it comes after
    /// the first sites. Words found it too when it is in `found`: it is
    /// then listed as they scored it, or as meant when only meaning lists
    /// it. Nothing is added when a page found is named by the whole query
    /// or answers it whole, as words find those better. A page only
    /// meaning found is never placed under its site. Callers check the
    /// searcher's options ([`super::options_allow`]) after, as for any
    /// page found.
    pub fn add_meant(&self, urls: &[String], found: &mut Vec<PageHit>) -> Result<()> {
        if found.iter().any(|hit| hit.named || hit.whole) {
            return Ok(());
        }
        for url in urls {
            let hit = match found.iter().position(|hit| hit.page.url == *url) {
                Some(at) if found[at].score >= MIN_PARTIAL_SCORE => return Ok(()),
                Some(at) => found.remove(at),
                None => {
                    // The next nearest stands in for one the index lacks.
                    let Some(mut hit) = self.page_at(url)?.filter(|hit| hit.page.url == *url)
                    else {
                        continue;
                    };
                    // Found by meaning alone, it is listed on its own and
                    // never under its site, where it would lift that site
                    // in the learned order: "spectrum jacksonville nc" put
                    // spectrumcentercharlotte.com third under Spectrum
                    // Center.
                    hit.page.site = None;
                    hit.score = 0.0;
                    (hit.named, hit.whole) = (false, false);
                    hit
                }
            };
            let weight = 1.0 - POPULARITY_SHARE + POPULARITY_SHARE * hit.popularity;
            let hit = PageHit {
                score: hit.score.max(ENTITY_MATCH * weight),
                meant: true,
                ..hit
            };
            let at = found
                .iter()
                .position(|kept| kept.score < hit.score)
                .unwrap_or(found.len());
            found.insert(at, hit);
            return Ok(());
        }
        Ok(())
    }

    /// The page stored at `address`.
    fn stored_page(&self, searcher: &Searcher, address: DocAddress) -> Result<Option<Page>> {
        let document: TantivyDocument = searcher.doc(address)?;
        self.read_page(&document)
    }
}
