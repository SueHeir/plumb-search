//! Shared production page retrieval and placement; no cache or node jobs.
use crate::web::IndexBackend;
use plumb_core::Operators;
use plumb_index::pages::{
    add_named_site, drop_namesakes_of_words, lift_named_sites, options_allow, place_operator_pages,
    place_pages, PageSearcher, OPERATOR_PAGES,
};
use plumb_index::{RankConfig, SearchOptions, SearchResults};

const PAGES_PER_SEARCH: usize = 10;

/// Returns lookup errors after preserving production's partial-result behavior.
/// Offline comparisons must discard observations when this list is nonempty.
pub fn add_pages(
    searcher: &PageSearcher,
    sites: Option<&IndexBackend>,
    rank: &RankConfig,
    query: &str,
    options: &SearchOptions,
    results: &mut SearchResults,
) -> Vec<String> {
    let mut errors = Vec::new();
    let searcher = searcher.in_language(options.language.as_deref());
    let ops = Operators::parse(query);
    if ops.any() {
        if ops.words.is_empty() {
            return errors;
        }
        // "asyncio site:docs.python.org" finds what "python asyncio" does
        // there.
        match searcher.search_naming_docs(&ops.words, &ops, false, OPERATOR_PAGES) {
            Ok(mut found) => {
                found.retain(|hit| options_allow(options, &hit.page));
                results.pages = place_operator_pages(&ops, &results.hits, found);
            }
            Err(err) => errors.push(format!("searching pages: {err:#}")),
        }
        return errors;
    }
    let applied = results
        .spelling
        .as_ref()
        .filter(|spelling| spelling.applied)
        .map(|spelling| spelling.query.clone());
    let query = applied.as_deref().unwrap_or(query);
    match searcher.search(query, PAGES_PER_SEARCH) {
        Ok(mut found) => {
            if let Err(err) =
                searcher.add_other_number(query, &results.hits, &mut found, PAGES_PER_SEARCH)
            {
                errors.push(format!("searching pages in the other number: {err:#}"));
            }
            found.retain(|hit| options_allow(options, &hit.page));
            if let Some(index) = sites.filter(|_| rank.add_named_site) {
                let lookup_error = std::cell::RefCell::new(None);
                add_named_site(&mut results.hits, &found, |domain| {
                    match index.searcher().site(domain) {
                        Ok(site) => site,
                        Err(err) => {
                            *lookup_error.borrow_mut() = Some(err.to_string());
                            None
                        }
                    }
                });
                if let Some(error) = lookup_error.into_inner() {
                    errors.push(error);
                }
            }
            if rank.drop_namesakes {
                drop_namesakes_of_words(&mut results.hits, &found);
            }
            lift_named_sites(&mut results.hits, &found);
            if let Err(err) = searcher.note_demand(&mut results.hits) {
                errors.push(format!("reading what sites' articles are read: {err:#}"));
            }
            // A query that names a package or a page, or asks a question
            // in full, is spelled right: "serde crate" is not "serde
            // create", and "git undo last commit" is not "git und".
            let spelled_right = found
                .iter()
                .any(|hit| hit.page.package.is_some() || hit.named || hit.whole);
            if spelled_right && applied.is_none() {
                results.spelling = None;
            }
            // A query that is a page's whole name is about what the page
            // is, not a search inside a site its first word names:
            // "virginia woolf" is not virginia.gov's search for "woolf",
            // nor "the last of us" last.fm's.
            if let Some(link) = &results.site_search {
                if found
                    .iter()
                    .any(|hit| hit.named && hit.page.site.as_deref() != Some(link.domain.as_str()))
                {
                    results.site_search = None;
                }
            }
            if let Some(spelling) = results.spelling.take_if(|_| applied.is_none()) {
                results.spelling = match searcher.check_spelling(query, spelling.clone()) {
                    Ok(checked) => checked,
                    Err(err) => {
                        errors.push(format!("checking a spelling against pages: {err:#}"));
                        Some(spelling)
                    }
                };
            }
            // Words of things, not sites ("anubas", "budafest"): the
            // pages' names know them.
            if results.spelling.is_none() && !spelled_right {
                if let Some(index) = sites {
                    let sites = index.searcher();
                    let site_known =
                        |word: &str| sites.word_sites(word) >= plumb_index::KNOWN_WORD_SITES;
                    match searcher.suggest_spelling(query, sites.spelling_model(), &site_known) {
                        Ok(suggested) => results.spelling = suggested,
                        Err(err) => {
                            errors.push(format!("suggesting a spelling from pages: {err:#}"))
                        }
                    }
                }
            }
            results.pages = place_pages(query, &results.hits, found);
            if rank.learned {
                plumb_index::learned::reorder(
                    plumb_index::learned::Model::builtin(),
                    query,
                    &mut results.hits,
                    &mut results.pages,
                );
            }
            // The learned order knows nothing of spelling: the site a
            // suggestion names stays second.
            if let Some(site) = results.spelling.as_ref().and_then(|s| s.site.clone()) {
                plumb_index::suggested_site_second(&mut results.hits, &site);
            }
            // Only shown, after the ranking, which weighs a site's own
            // title.
            if let Err(err) = searcher.title_untitled(&mut results.hits) {
                errors.push(format!("titling sites from their articles: {err:#}"));
            }
        }
        Err(err) => errors.push(format!("searching pages: {err:#}")),
    }
    errors
}
