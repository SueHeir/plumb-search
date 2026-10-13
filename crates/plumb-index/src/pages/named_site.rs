//! The inner page of a site that a search names along with the site:
//! "census population clock" is census.gov's Population Clock, "who
//! malaria" who.int's page on malaria.

use std::collections::HashSet;

use anyhow::Result;
use plumb_core::{domain_label, Operators};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{TantivyDocument, Term};

use super::placing::subpage_asked;
use super::sites::squash;
use super::{
    asked_as_question, Page, PageHit, PageSearcher, ASKING_WORDS, PARTIAL_MATCH, POPULARITY_SCALE,
    POPULARITY_SHARE, REFERENCE2_SET, REFERENCE_SET, SUBPAGES_SET,
};
use crate::analysis;

/// Least share of a page's title, not counting the site's own name, that
/// the rest of the query must be: "malaria" is who.int's "Malaria", not
/// its "Malaria rapid diagnostic test performance".
const NAMED_SITE_PAGE_SHARE: f32 = 0.5;

/// Best sites looked through for one whose name starts or ends the
/// search.
const NAMED_SITES: usize = 5;

/// Pages of the site looked at for each way of finding them.
const NAMED_SITE_CANDIDATES: usize = 20;

/// Where a title's subject ends and a subtitle or the site's name starts:
/// "Plantar fasciitis - Symptoms and causes - Mayo Clinic".
const TITLE_BREAKS: &[&str] = &[":", " - ", " | ", " -- ", " — ", " – ", " · "];

impl PageSearcher {
    /// Puts under the first of the best [`NAMED_SITES`] of `sites` whose
    /// name starts or ends `query` its inner page (of the subpages and
    /// reference sets) that the rest of the query names: "census population clock"
    /// carries census.gov's Population Clock, which the site's own name
    /// would otherwise leave to a newsroom story with "population" in it.
    /// The page's title has every word of the rest, and they are at least
    /// [`NAMED_SITE_PAGE_SHARE`] of it besides the site's name. Placed,
    /// the site's other inner pages are left out
    /// ([`place_pages`](super::place_pages)): they match the same search
    /// less. Nothing changes when the whole query names the site or a page
    /// ("amazon rainforest" is the article, not amazon.com's page on it).
    pub fn add_named_site_page(
        &self,
        query: &str,
        sites: &[crate::Hit],
        found: &mut Vec<PageHit>,
    ) -> Result<()> {
        if sites.first().is_none_or(|site| site.named) || found.iter().any(|hit| hit.named) {
            return Ok(());
        }
        let words = analysis::tokens(&self.words, query);
        // Sites on the rest may come before it: who.int for "who
        // diabetes".
        let Some((site, rest)) = sites
            .iter()
            .take(NAMED_SITES)
            .filter(|site| !site.named)
            .find_map(|site| Some((site, rest_of_name(&words, site)?)))
        else {
            return Ok(());
        };
        // The "s" of "where's" says nothing.
        let rest: Vec<String> = rest
            .into_iter()
            .filter(|word| {
                word.chars().count() > 1
                    && !crate::is_function_word(word)
                    && !ASKING_WORDS.contains(&word.as_str())
            })
            .collect();
        let asked: HashSet<String> = analysis::tokens(&self.stemmed, &rest.join(" "))
            .into_iter()
            .collect();
        if asked.is_empty() {
            return Ok(());
        }
        // The site's name in the query and its title, which its pages'
        // titles repeat: "U.S. Census Bureau QuickFacts".
        let mut site_words: HashSet<String> = analysis::tokens(&self.stemmed, query)
            .into_iter()
            .chain(analysis::tokens(
                &self.stemmed,
                site.title.as_deref().unwrap_or(""),
            ))
            .collect();
        site_words.retain(|word| !asked.contains(word));
        let named = Named {
            query,
            site,
            sites,
            asked: &asked,
            site_words: &site_words,
        };
        let Some(best) = self.best_named_site_page(&named, &rest)? else {
            return Ok(());
        };
        found.retain(|hit| hit.page.url != best.page.url);
        found.insert(0, best);
        Ok(())
    }

    /// The page of `named.site` whose title the words `rest` name best, as
    /// [`PageSearcher::add_named_site_page`] takes it, with [`Page::site`]
    /// set to the site: one placing would list.
    fn best_named_site_page(&self, named: &Named, rest: &[String]) -> Result<Option<PageHit>> {
        let site = named.site;
        let ops = Operators {
            sites: vec![site.domain.clone()],
            ..Operators::default()
        };
        let Some(scope) = self.page_scope(&ops, false) else {
            return Ok(None);
        };
        let sets: Box<dyn Query> = Box::new(BooleanQuery::new(
            [REFERENCE_SET, REFERENCE2_SET, SUBPAGES_SET]
                .iter()
                .map(|set| (Occur::Should, self.scope_term(&format!("set:{set}"))))
                .collect(),
        ));
        let every = |field, option, terms: Vec<&String>| -> Box<dyn Query> {
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = terms
                .into_iter()
                .map(|term| {
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(Term::from_field_text(field, term), option))
                            as Box<dyn Query>,
                    )
                })
                .collect();
            clauses.push((Occur::Must, scope.box_clone()));
            clauses.push((Occur::Must, sets.box_clone()));
            self.language_query(&BooleanQuery::new(clauses))
        };
        // Titles with every word as typed, the shortest scoring best, else
        // pages about every word, whose titles may say it another way
        // ("shareholder letter").
        let searcher = self.reader.searcher();
        let ways = [
            (
                self.fields.words,
                IndexRecordOption::WithFreqs,
                rest.iter().collect(),
            ),
            (
                self.fields.topic,
                IndexRecordOption::Basic,
                named.asked.iter().collect(),
            ),
        ];
        for (field, option, terms) in ways {
            let mut best: Option<(f32, PageHit)> = None;
            for (_, address) in searcher.search(
                every(field, option, terms).as_ref(),
                &TopDocs::with_limit(NAMED_SITE_CANDIDATES).order_by_score(),
            )? {
                let document: TantivyDocument = searcher.doc(address)?;
                let Some(stored) = document
                    .get_first(self.fields.page)
                    .and_then(|v| v.as_str())
                else {
                    continue;
                };
                let mut page: Page = serde_json::from_str(stored)?;
                let share = self.named_share(&page.title, named.asked, named.site_words);
                if !page.is_site_page()
                    || share < NAMED_SITE_PAGE_SHARE
                    || best.as_ref().is_some_and(|(kept, _)| share < *kept)
                {
                    continue;
                }
                let popularity = document
                    .get_first(self.fields.popularity)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as f32
                    / POPULARITY_SCALE;
                if best
                    .as_ref()
                    .is_some_and(|(kept, hit)| share == *kept && popularity <= hit.popularity)
                {
                    continue;
                }
                page.site = Some(site.domain.clone());
                let hit = PageHit {
                    page,
                    score: PARTIAL_MATCH
                        * share
                        * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity),
                    named: false,
                    popularity,
                    whole: false,
                    learned: None,
                };
                // One placing would leave out is no use: one in another
                // language, or a drug offered without a prescription.
                if crate::health::page_allowed(named.query, &hit.page)
                    && subpage_asked(named.query, named.sites, &hit)
                {
                    best = Some((share, hit));
                }
            }
            if let Some((_, hit)) = best {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }

    /// The share of `title`, or of a part of it before or after a break
    /// ([`TITLE_BREAKS`]), that the words `asked` are, when it has all of
    /// them: words of the site's name (`site_words`) do not count. 0 when
    /// no part has every word.
    fn named_share(
        &self,
        title: &str,
        asked: &HashSet<String>,
        site_words: &HashSet<String>,
    ) -> f32 {
        let parts = TITLE_BREAKS
            .iter()
            .filter_map(|mark| title.split_once(mark))
            .flat_map(|(head, tail)| [head, tail]);
        std::iter::once(title)
            .chain(parts)
            .map(|part| {
                let words: HashSet<String> = analysis::tokens(&self.stemmed, part)
                    .into_iter()
                    .filter(|word| !site_words.contains(word))
                    .collect();
                if asked.iter().all(|word| words.contains(word)) {
                    asked.len() as f32 / words.len() as f32
                } else {
                    0.0
                }
            })
            .fold(0.0, f32::max)
    }
}

/// What [`PageSearcher::best_named_site_page`] looks for: the search, the
/// site it names and all its sites, the stemmed words of the rest of it,
/// and the words of the site's name, which pages' titles repeat.
struct Named<'a> {
    query: &'a str,
    site: &'a crate::Hit,
    sites: &'a [crate::Hit],
    asked: &'a HashSet<String>,
    site_words: &'a HashSet<String>,
}

/// The words of `words` other than the name of `site` that they start or
/// end with: "population clock" of "census population clock" for
/// census.gov. The name the ranking found the query to start with comes
/// first, which may be another name of the site ("census bureau",
/// "world health organization"), then the site's name in its domain at
/// either end. `None` when neither end is the site's name, or when a
/// question starts with it: "who is the president" asks nothing of
/// who.int.
fn rest_of_name(words: &[String], site: &crate::Hit) -> Option<Vec<String>> {
    let label = squash(&domain_label(site.domain.trim_start_matches("www.")));
    let lead = site
        .query_evidence
        .as_ref()
        .map_or(0, |evidence| evidence.partial_name_words);
    let (rest, leading) = if (1..words.len()).contains(&lead) {
        (&words[lead..], true)
    } else {
        (1..words.len()).find_map(|n| {
            if squash(&words[..n].concat()) == label {
                Some((&words[n..], true))
            } else {
                let end = words.len() - n;
                (squash(&words[end..].concat()) == label).then(|| (&words[..end], false))
            }
        })?
    };
    let asks = leading
        && asked_as_question(&words.join(" "))
        && rest.first().is_some_and(|word| {
            crate::is_function_word(word) || ASKING_WORDS.contains(&word.as_str())
        });
    (!asks).then(|| rest.to_vec())
}
