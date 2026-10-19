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
    Page, PageHit, PageSearcher, ASKING_WORDS, DOCS_SET, ENTITY_MATCH, GITHUB_SET, MEANT_READS,
    MIN_PARTIAL_SCORE, PACKAGES_SET, POPULARITY_SHARE, STACKEXCHANGE_SET, STACKOVERFLOW_SET,
    TITLE_INSIDE_CANDIDATES,
};
use crate::{analysis, MeantPage};

/// Sets whose pages meaning lists only when words found a page of one of
/// them too, so the search is about code or asks a question. Their short
/// titles stood nearest many searches about something else on hpc:
/// "einwohnerzahl tokio" listed tokio-rs/tokio, "cequ" ceil(3), "burger
/// kng" a repository.
const MEANT_WITH_WORDS: [&str; 5] = [
    DOCS_SET,
    GITHUB_SET,
    PACKAGES_SET,
    STACKOVERFLOW_SET,
    STACKEXCHANGE_SET,
];

/// Least cosine at which meaning lists a page that is not an article. On
/// hpc's review searches, of the books, papers, songs, films, site pages,
/// Wiktionary entries and Wikidata items that would have taken the place,
/// 86 of 95 below 0.75 were wrong and none right ("harvard" a page on
/// chessprogramming.org), while 24 of 44 from 0.85 were what the search
/// wanted ("levitating by dua lipa", "who dengue fact sheet"), 10 wrong:
/// a better share than the articles meaning lists.
const MEANT_OTHER_LEAST: f32 = 0.85;

/// Least popularity ([`PageHit::popularity`]) of an article meaning lists,
/// about 300 reads in the week the article set counts. Of the articles
/// #718's file listed for hpc's review and everyday searches, checked by
/// hand, none read less was right: 8 were wrong ("tesla cincinnati ohio"
/// to Diaz v. Tesla, Inc.) and 3 on the topic.
const MEANT_ARTICLE_LEAST: f32 = 0.36;

/// Least sum of an article's cosine, how far it stands out of its set
/// ([`MeantPage::gap`]) and popularity for meaning to list it. Each tells
/// right from wrong only somewhat, and wrong articles were read about as
/// much as the right ones that mattered; together they tell better. Of
/// the articles #718's file listed for hpc's review and everyday searches,
/// checked by hand, those under 1.15 were 28 wrong ("24 and me login" to
/// 24 (number)), 7 on the topic and 3 right.
const MEANT_ARTICLE_SUM: f32 = 1.15;

/// How near in meaning an article meaning lists usually is to the nearest
/// of the first sites listed ([`MeantPage::sites`]): the median on hpc. An
/// article farther from all of them needs [`MEANT_SITES_SHARE`] of the
/// difference more than [`MEANT_ARTICLE_SUM`]: "synonyms for broken"
/// lists merriam-webster.com and thesaurus.com, far from Broken (1993
/// film). Of the articles the sum kept for hpc's review and everyday
/// searches, checked by hand, this left out 35 wrong ("www.supershuttle.com"
/// to super.com), 11 on the topic and 5 right. The right ones were mostly
/// the answer to a search whose sites are poor ("detritous" to Detritus),
/// but the node's closeness of those sites to the search did not tell
/// them apart.
const MEANT_SITES_TYPICAL: f32 = 0.72;

/// How much of an article's distance from the sites past
/// [`MEANT_SITES_TYPICAL`] it must make up.
const MEANT_SITES_SHARE: f32 = 0.5;

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

    /// Adds to `found` (best first) the first page of `meant` the index
    /// holds, the pages nearest the query in meaning with their cosines
    /// ([`crate::Meaning::nearest_pages`]), scored as a linked article is
    /// ([`ENTITY_MATCH`]) and marked [`PageHit::meant`], so it comes after
    /// the first sites. Words found it too when it is in `found`: it is
    /// then listed as they scored it, or as meant when only meaning lists
    /// it, though a page they list that is not an article gives way to the
    /// next. Nothing is added when a page found is named by the whole
    /// query or answers it whole, as words find those better, a page of
    /// [`MEANT_WITH_WORDS`] only when words found one too, an article only
    /// from [`MEANT_ARTICLE_LEAST`] and [`MEANT_ARTICLE_SUM`] (higher when
    /// it is far from the sites listed, [`MEANT_SITES_TYPICAL`]), and any
    /// page that is not an article, those of [`MEANT_WITH_WORDS`] too, only
    /// from [`MEANT_OTHER_LEAST`] and after the pages words list. A page
    /// that would score too little to be placed ([`MIN_PARTIAL_SCORE`])
    /// gives way to the next. A page only meaning found is never placed
    /// under its site. Callers check the searcher's options
    /// ([`super::options_allow`]) after, as for any page found, and give
    /// the address of the page added, if any, to the learned order
    /// ([`crate::learned::reorder_results`]).
    pub fn add_meant(
        &self,
        meant: &[MeantPage],
        found: &mut Vec<PageHit>,
    ) -> Result<Option<String>> {
        if found.iter().any(|hit| hit.named || hit.whole) {
            return Ok(None);
        }
        let with_words = |page: &Page| MEANT_WITH_WORDS.contains(&page.set.as_str());
        let about_code = found.iter().any(|hit| with_words(&hit.page));
        let weight = |hit: &PageHit| 1.0 - POPULARITY_SHARE + POPULARITY_SHARE * hit.popularity;
        for MeantPage {
            url,
            cosine,
            gap,
            sites,
        } in meant
        {
            let far = sites.map_or(0.0, |near| (MEANT_SITES_TYPICAL - near).max(0.0));
            // An article counts when it is read enough and near enough,
            // another page when it is near enough. Either must score
            // enough as meant for placing to keep it, or the search would
            // be left with none: a paper cited a few times scores too
            // little however near it is.
            let counts = |hit: &PageHit| {
                let kept = ENTITY_MATCH * weight(hit) >= MIN_PARTIAL_SCORE;
                kept && if hit.page.is_article() {
                    hit.popularity >= MEANT_ARTICLE_LEAST
                        && cosine + gap + hit.popularity
                            >= MEANT_ARTICLE_SUM + MEANT_SITES_SHARE * far
                } else {
                    *cosine >= MEANT_OTHER_LEAST
                }
            };
            let hit = match found.iter().position(|hit| hit.page.url == *url) {
                Some(at) => {
                    let listed = found[at].score >= MIN_PARTIAL_SCORE;
                    if listed && found[at].page.is_article() {
                        return Ok(None);
                    }
                    // A page of another kind words list, or one that does
                    // not count, does not keep out the next nearest: on hpc
                    // a question words listed kept Robinhood Markets from
                    // "robin hood stocks app".
                    if listed || !counts(&found[at]) {
                        continue;
                    }
                    found.remove(at)
                }
                None => {
                    // The next nearest stands in for one the index lacks.
                    let Some(mut hit) = self.page_at(url)?.filter(|hit| {
                        hit.page.url == *url
                            && counts(hit)
                            && (about_code || !with_words(&hit.page))
                    }) else {
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
            let hit = PageHit {
                score: hit.score.max(ENTITY_MATCH * weight(&hit)),
                meant: true,
                ..hit
            };
            // A page that is not an article takes no place of one words
            // list: on hpc the isbn gem pushed out the Stack Overflow
            // question "convert ISBN-10 to ISBN-13" wanted, and certbot's
            // repository a Server Fault question on getting a certificate.
            let article = hit.page.is_article();
            let at = found
                .iter()
                .position(|kept| {
                    kept.score < hit.score && (article || kept.score < MIN_PARTIAL_SCORE)
                })
                .unwrap_or(found.len());
            found.insert(at, hit);
            return Ok(Some(url.clone()));
        }
        Ok(None)
    }

    /// The page stored at `address`.
    fn stored_page(&self, searcher: &Searcher, address: DocAddress) -> Result<Option<Page>> {
        let document: TantivyDocument = searcher.doc(address)?;
        self.read_page(&document)
    }
}
