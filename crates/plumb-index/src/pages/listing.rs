//! What a search lists with the sites it found: the pages, placed among
//! them, and the spelling the pages suggest. A node lists them so, and the
//! evals list them the same way, so that they measure what a node serves.

use anyhow::Result;
use plumb_core::place::Place;
use plumb_core::Operators;

use super::{
    add_named_site, drop_far_namesakes, drop_namesakes_of_words, lift_named_sites, options_allow,
    place_operator_pages, place_pages, site_pages_after_named_article, PageHit, PageSearcher,
    OPERATOR_PAGES,
};
use crate::{Hit, RankConfig, SearchOptions, SearchResults, Searcher, Spelling};

/// Pages looked at per search, before [`place_pages`] picks.
pub const PAGES_PER_SEARCH: usize = 10;
/// Most pages each of [`Listing::more`] adds to one search.
const MORE_PER_SEARCH: usize = 3;

/// What a search's pages are listed with.
pub struct Listing<'a> {
    /// The sites' index: the sites pages name, the language a search is
    /// in and the words it knows.
    pub sites: &'a Searcher,
    /// The page sets.
    pub pages: &'a PageSearcher,
    /// More pages to take in, as a node takes those found for its own
    /// searches and those its owner's AI apps asked for.
    pub more: &'a [&'a PageSearcher],
    pub rank: &'a RankConfig,
    /// Whether the learned ranking orders the rows, and what follows it is
    /// done. Not when an eval writes the rows the model reads, to train it.
    pub learned_order: bool,
    /// Whether a page found may be listed, past what the search's options
    /// allow: a node checks the pages of [`Listing::more`] against its
    /// adult blocklist.
    pub keep: &'a dyn Fn(&PageHit) -> bool,
    /// The town a name is, to list the sites of its namesakes last.
    pub town: &'a dyn Fn(&str) -> Option<Place>,
}

impl Listing<'_> {
    /// Adds the pages found for `query` to `results`, the sites found for
    /// it. When the results are for a spelling of it
    /// ([`Spelling::applied`]), the pages are for that spelling too.
    ///
    /// Returns the spelling whose results to show instead, when the pages
    /// are sure of it ([`PageSearcher::sure_spelling`]): "ballrat" is
    /// Ballarat.
    pub fn add_pages(
        &self,
        query: &str,
        options: &SearchOptions,
        results: &mut SearchResults,
    ) -> Result<Option<Spelling>> {
        // Pages keep or drop a language for a search as sites do.
        let options = &SearchOptions {
            language: self.sites.language_for(options, query),
            ..options.clone()
        };
        let pages = self.pages.in_language(options.language.as_deref());
        let typed = results.hits.clone();
        let ops = Operators::parse(query);
        if !ops.any() {
            self.add_found(&pages, query, options, results)?;
        } else if !ops.words.is_empty() {
            // "asyncio site:docs.python.org" finds what "python asyncio"
            // does there.
            let mut found = pages.search_naming_docs(&ops.words, &ops, false, OPERATOR_PAGES)?;
            found.retain(|hit| options_allow(options, &hit.page));
            results.pages = place_operator_pages(&ops, &results.hits, found);
        }
        let Some(spelling) = &results.spelling else {
            return Ok(None);
        };
        let site_words = |word: &str| self.sites.word_sites(word);
        let sure = pages.sure_spelling(query, spelling, options, &typed, &site_words)?;
        Ok(sure.then(|| spelling.clone()))
    }

    /// [`Listing::add_pages`] for a query without search operators, with
    /// `pages` in its language.
    fn add_found(
        &self,
        pages: &PageSearcher,
        query: &str,
        options: &SearchOptions,
        results: &mut SearchResults,
    ) -> Result<()> {
        let more: Vec<PageSearcher> = self
            .more
            .iter()
            .map(|more| more.in_language(options.language.as_deref()))
            .collect();
        let applied = results
            .spelling
            .as_ref()
            .filter(|spelling| spelling.applied)
            .map(|spelling| spelling.query.clone());
        let query = applied.as_deref().unwrap_or(query);
        let mut found = pages.search(query, PAGES_PER_SEARCH)?;
        pages.add_other_number(query, &results.hits, &mut found, PAGES_PER_SEARCH)?;
        let meant = pages.add_meant(query, &results.meant, &mut found)?;
        for more in &more {
            add_more(more, query, &mut found)?;
        }
        found.retain(|hit| options_allow(options, &hit.page));
        if self.rank.add_named_site {
            add_named_site(&mut results.hits, &found, |domain| {
                self.sites.site(domain).ok().flatten()
            });
        }
        drop_far_namesakes(&mut results.hits, &found, self.rank.meaning_only_relevance);
        if self.rank.drop_namesakes {
            drop_namesakes_of_words(&mut results.hits, &found);
        }
        lift_named_sites(&mut results.hits, &found);
        add_named_site_page(pages, &more, query, &results.hits, &mut found)?;
        found.retain(|hit| options_allow(options, &hit.page) && (self.keep)(hit));
        pages.note_demand(&mut results.hits)?;
        self.spell(pages, query, options, &found, applied.is_some(), results)?;
        results.pages = place_pages(query, &results.hits, found);
        if !self.learned_order {
            return Ok(());
        }
        crate::learned::reorder_results(
            self.rank,
            query,
            results.sites_ordered,
            meant.as_deref(),
            &mut results.hits,
            &mut results.pages,
        );
        self.after_learned_order(pages, query, options, results)
    }

    /// Checks `results`' spelling of `query` against the pages `found` for
    /// it: the words they know are put back
    /// ([`PageSearcher::check_spelling`]), and words of things, not sites
    /// ("anubas", "budafest"), are spelled from the pages' names, which may
    /// stand in for the sites' spelling ([`PageSearcher::pick_spelling`]:
    /// "rooling in the deep"). Not when the results are for a spelling
    /// already, and only the check for a search spelled right or of exact
    /// words.
    fn spell(
        &self,
        pages: &PageSearcher,
        query: &str,
        options: &SearchOptions,
        found: &[PageHit],
        applied: bool,
        results: &mut SearchResults,
    ) -> Result<()> {
        let spelled_right = pages.spelled_right(query, found, results.spelling.as_ref());
        if spelled_right && !applied {
            results.spelling = None;
        }
        // A query that is a page's whole name is about what the page is,
        // not a search inside a site its first word names: "virginia
        // woolf" is not virginia.gov's search for "woolf", nor "the last of
        // us" last.fm's.
        if let Some(link) = &results.site_search {
            if found
                .iter()
                .any(|hit| hit.named && hit.page.site.as_deref() != Some(link.domain.as_str()))
            {
                results.site_search = None;
            }
        }
        if applied {
            return Ok(());
        }
        if let Some(spelling) = &results.spelling {
            results.spelling = pages.check_spelling(query, spelling.clone())?;
        }
        if spelled_right || options.exact {
            return Ok(());
        }
        let site_words = |word: &str| self.sites.word_sites(word);
        let by_pages = pages.suggest_spelling(query, self.sites.spelling_model(), &site_words)?;
        let by_sites = results.spelling.clone();
        results.spelling = pages.pick_spelling(query, by_sites, by_pages, options)?;
        Ok(())
    }

    /// What follows the learned order, which knows nothing of states,
    /// towns, spelling or typed addresses.
    fn after_learned_order(
        &self,
        pages: &PageSearcher,
        query: &str,
        options: &SearchOptions,
        results: &mut SearchResults,
    ) -> Result<()> {
        // A county search that ends in its state lists that state's county
        // before its namesakes elsewhere.
        crate::asked_state_first(query, &mut results.hits);
        // And a search about a town lists the sites of its namesakes
        // elsewhere last: "berlin wall memorial" is not Berlin, NH.
        crate::namesakes_last(query, &mut results.hits, self.town);
        // The site a suggestion names stays second, and so does the page.
        if let Some(spelling) = &results.spelling {
            if let Some(site) = &spelling.site {
                crate::suggested_site_second(&mut results.hits, site);
            }
            pages.list_suggested_page(spelling, options, &mut results.pages)?;
        }
        // Only shown, after the ranking, which weighs a site's own title.
        pages.title_untitled(&mut results.hits)?;
        // A pasted address is the page at it, whatever the ranking says.
        pages.list_typed_page(query, options, &mut results.pages)
    }
}

/// Puts first in `found` the page of a site `query` names
/// ([`PageSearcher::add_named_site_page`]): the page sets' when they have
/// one, else the first of `more` that has one.
fn add_named_site_page(
    pages: &PageSearcher,
    more: &[PageSearcher],
    query: &str,
    sites: &[Hit],
    found: &mut Vec<PageHit>,
) -> Result<()> {
    let first = found.first().map(|hit| hit.page.url.clone());
    pages.add_named_site_page(query, sites, found)?;
    for more in more {
        if found.first().map(|hit| &hit.page.url) != first.as_ref() {
            break;
        }
        more.add_named_site_page(query, sites, found)?;
    }
    Ok(())
}

/// Adds to `found` the pages of `more` that `query` finds, best first
/// among the page sets' by score: both are scored the same way, each set's
/// popularity against its own most viewed page, and none goes above the
/// article the query names.
fn add_more(more: &PageSearcher, query: &str, found: &mut Vec<PageHit>) -> Result<()> {
    let before = found.len();
    for hit in more.search(query, MORE_PER_SEARCH)? {
        if !found.iter().any(|known| known.page.url == hit.page.url) {
            found.push(hit);
        }
    }
    if found.len() > before {
        site_pages_after_named_article(found);
        found.sort_by(|a, b| b.score.total_cmp(&a.score));
    }
    Ok(())
}
