use anyhow::{ensure, Result};
use plumb_core::Operators;
use plumb_index::pages::{operators_allow, options_allow, Page, PageHit, PageSearcher};
use plumb_index::{Hit, RankConfig, SearchOptions, SearchResults};
use plumb_node::web::{IndexBackend, SearchBackend};
const TYPED_CANDIDATES: usize = 200;

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct PrimaryRequest {
    query: String,
    limit: usize,
    options: SearchOptions,
}

impl PrimaryRequest {
    fn matches(&self, query: &str, limit: usize, options: &SearchOptions) -> bool {
        self.query == query && self.limit == limit && self.options == *options
    }
}

/// Uses the same page retrieval/placement helper as the production node.
pub(super) struct FrozenBackend {
    pub(super) sites: IndexBackend,
    pub(super) pages: PageSearcher,
    pub(super) rank: RankConfig,
    pub(super) places: Option<plumb_index::places::PlaceSearcher>,
    pub(super) raw: std::sync::Mutex<Option<SearchResults>>,
    pub(super) primary: std::sync::Mutex<Option<PrimaryRequest>>,
    pub(super) errors: std::sync::Mutex<Vec<String>>,
    #[cfg(test)]
    pub(super) injected_auxiliary_error: std::sync::Mutex<Option<String>>,
}

impl FrozenBackend {
    pub(super) fn begin_primary(&self, query: &str, limit: usize, options: SearchOptions) {
        *self.primary.lock().unwrap() = Some(PrimaryRequest {
            query: query.into(),
            limit,
            options,
        });
        *self.raw.lock().unwrap() = None;
        self.errors.lock().unwrap().clear();
    }
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
    fn retrieve(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
        rank: Option<&RankConfig>,
    ) -> Result<SearchResults> {
        let mut results = self
            .sites
            .search_full_checked(query, limit, options, rank)?;
        if let Some(spelling) = results.spelling.as_ref().filter(|s| s.applied) {
            if self
                .pages
                .check_spelling(query, spelling.clone())?
                .is_none_or(|checked| checked.query != spelling.query)
            {
                results = self.sites.search_full_checked(
                    query,
                    limit,
                    &SearchOptions {
                        exact: true,
                        ..options.clone()
                    },
                    rank,
                )?;
            }
        }
        self.add_pages(query, options, rank.unwrap_or(&self.rank), &mut results)?;
        Ok(results)
    }

    fn observed_retrieval(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
        rank: Option<&RankConfig>,
    ) -> Result<SearchResults> {
        let primary = self
            .primary
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|request| request.matches(query, limit, options));
        let retrieve = || self.retrieve(query, limit, options, rank);
        #[cfg(test)]
        let result = if !primary {
            match self.injected_auxiliary_error.lock().unwrap().take() {
                Some(error) => Err(anyhow::anyhow!(error)),
                None => retrieve(),
            }
        } else {
            retrieve()
        };
        #[cfg(not(test))]
        let result = retrieve();
        match &result {
            Ok(results) => {
                if primary {
                    let mut raw = self.raw.lock().unwrap();
                    if raw.is_none() {
                        *raw = Some(results.clone());
                    }
                }
            }
            Err(error) => self.errors.lock().unwrap().push(format!("{error:#}")),
        }
        result
    }
}

impl SearchBackend for FrozenBackend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        Ok(self
            .search_full(query, limit, &SearchOptions::default())?
            .hits)
    }

    fn search_full(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        self.observed_retrieval(query, limit, options, None)
    }

    fn search_ranked(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
        rank: &RankConfig,
    ) -> Result<SearchResults> {
        self.observed_retrieval(query, limit, options, Some(rank))
    }

    fn num_docs(&self) -> u64 {
        self.sites.num_docs()
    }

    fn site(&self, domain: &str) -> Option<Hit> {
        match self.sites.searcher().site(domain) {
            Ok(site) => site,
            Err(err) => {
                self.errors.lock().unwrap().push(err.to_string());
                None
            }
        }
    }

    fn places(
        &self,
        query: &str,
        home: Option<&str>,
        country: Option<&str>,
    ) -> Option<plumb_index::places::PlaceResults> {
        match self.places.as_ref()?.search(query, home, country, 8) {
            Ok(places) => places,
            Err(err) => {
                self.errors.lock().unwrap().push(err.to_string());
                None
            }
        }
    }

    fn locate(&self, text: &str, country: Option<&str>) -> Option<plumb_core::place::Place> {
        match self.places.as_ref()?.locate(text, country) {
            Ok(place) => place,
            Err(err) => {
                self.errors.lock().unwrap().push(err.to_string());
                None
            }
        }
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
