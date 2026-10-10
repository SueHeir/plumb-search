use anyhow::{ensure, Result};
use plumb_core::Operators;
use plumb_index::pages::{operators_allow, options_allow, Page, PageHit, PageSearcher};
use plumb_index::{Hit, RankConfig, SearchOptions, SearchResults};
use plumb_node::web::{IndexBackend, SearchBackend};
const TYPED_CANDIDATES: usize = 200;

/// Uses the same page retrieval/placement helper as the production node.
pub(super) struct FrozenBackend {
    pub(super) sites: IndexBackend,
    pub(super) pages: PageSearcher,
    pub(super) rank: RankConfig,
    pub(super) raw: std::sync::Mutex<Option<SearchResults>>,
    pub(super) errors: std::sync::Mutex<Vec<String>>,
}

impl FrozenBackend {
    fn add_pages(
        &self,
        query: &str,
        options: &SearchOptions,
        rank: &RankConfig,
        results: &mut SearchResults,
    ) -> Result<()> {
        let errors = plumb_node::page_retrieval::add_pages(
            &self.pages,
            Some(&self.sites),
            rank,
            query,
            options,
            results,
        );
        ensure!(errors.is_empty(), "page retrieval: {}", errors.join("; "));
        Ok(())
    }
}

impl SearchBackend for FrozenBackend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.sites.search(query, limit)
    }

    fn search_full(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        let mut results = self.sites.search_full(query, limit, options)?;
        if let Some(spelling) = results.spelling.as_ref().filter(|s| s.applied) {
            if self
                .pages
                .check_spelling(query, spelling.clone())?
                .is_none_or(|checked| checked.query != spelling.query)
            {
                results = self.sites.search_full(
                    query,
                    limit,
                    &SearchOptions {
                        exact: true,
                        ..options.clone()
                    },
                )?;
            }
        }
        self.add_pages(query, options, &self.rank, &mut results)?;
        *self.raw.lock().unwrap() = Some(results.clone());
        Ok(results)
    }

    fn search_ranked(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
        rank: &RankConfig,
    ) -> Result<SearchResults> {
        let mut results = self.sites.search_ranked(query, limit, options, rank)?;
        self.add_pages(query, options, rank, &mut results)?;
        Ok(results)
    }

    fn num_docs(&self) -> u64 {
        self.sites.num_docs()
    }

    fn site(&self, domain: &str) -> Option<Hit> {
        SearchBackend::site(&self.sites, domain)
    }

    fn places(
        &self,
        query: &str,
        home: Option<&str>,
        country: Option<&str>,
    ) -> Option<plumb_index::places::PlaceResults> {
        self.sites.places(query, home, country)
    }

    fn locate(&self, text: &str, country: Option<&str>) -> Option<plumb_core::place::Place> {
        self.sites.locate(text, country)
    }

    fn known_song(&self, query: &str, options: &SearchOptions) -> Option<Page> {
        match self
            .pages
            .in_language(options.language.as_deref())
            .known_song(query)
        {
            Ok(page) => page.filter(|page| options_allow(options, page)),
            Err(err) => {
                self.errors.lock().unwrap().push(err.to_string());
                None
            }
        }
    }

    fn definition(&self, name: &str) -> Option<Page> {
        match self.pages.definition(name) {
            Ok(page) => page,
            Err(err) => {
                self.errors.lock().unwrap().push(err.to_string());
                None
            }
        }
    }

    fn entities(&self, query: &str, limit: usize, options: &SearchOptions) -> Result<Vec<PageHit>> {
        let ops = Operators::parse(query);
        let words = if ops.any() { ops.words.as_str() } else { query };
        Ok(self
            .pages
            .in_language(options.language.as_deref())
            .entities(words, TYPED_CANDIDATES)
            .inspect_err(|err| {
                self.errors.lock().unwrap().push(err.to_string());
            })?
            .into_iter()
            .filter(|hit| options_allow(options, &hit.page) && operators_allow(&ops, &hit.page))
            .take(limit.min(TYPED_CANDIDATES))
            .collect())
    }

    fn pages_of(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
        docs: bool,
        keep: &dyn Fn(&Page) -> bool,
    ) -> Vec<PageHit> {
        if limit == 0 {
            return Vec::new();
        }
        let ops = Operators::parse(query);
        let words = if ops.any() { ops.words.as_str() } else { query };
        self.pages
            .in_language(options.language.as_deref())
            .search_naming_docs(words, &ops, docs, TYPED_CANDIDATES)
            .unwrap_or_else(|err| {
                self.errors
                    .lock()
                    .unwrap()
                    .push(format!("typed retrieval: {err:#}"));
                Vec::new()
            })
            .into_iter()
            .filter(|hit| {
                options_allow(options, &hit.page)
                    && operators_allow(&ops, &hit.page)
                    && keep(&hit.page)
            })
            .take(limit.min(TYPED_CANDIDATES))
            .collect()
    }

    fn papers(
        &self,
        query: &plumb_core::paper_query::PaperQuery,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<Vec<PageHit>> {
        Ok(self
            .pages
            .in_language(options.language.as_deref())
            .search_papers(query, TYPED_CANDIDATES)?
            .into_iter()
            .filter(|hit| options_allow(options, &hit.page))
            .take(limit.min(TYPED_CANDIDATES))
            .collect())
    }

    fn paper_coverage(&self) -> Option<plumb_index::pages::PaperCoverage> {
        Some(self.pages.paper_coverage().clone())
    }
}
