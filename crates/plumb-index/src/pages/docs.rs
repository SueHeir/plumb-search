//! Documentation pages: found by product, symbol, section or topic, and
//! kept below the docs site they belong to.

use std::collections::HashSet;

use anyhow::Result;
use plumb_core::article::SearchContent;
use plumb_core::{host_of, Operators};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{IndexRecordOption, Value};
use tantivy::{TantivyDocument, Term};

use super::placing::operators_allow;
use super::scoring::{scoped_query, site_pages_after_named_article, truncate_keeping_inner_pages};
use super::sites::squash;
use super::{
    Page, PageHit, PageSearcher, ALIAS_MATCH, ASKING_WORDS, CANDIDATES, DOCS_SET, PARTIAL_MATCH,
    POPULARITY_SCALE, POPULARITY_SHARE, QUESTION_QUERY_WORDS, QUESTION_SHARE,
};
use crate::analysis;

/// Whether `hit` is a docs page the search found by most of its words
/// rather than named.
pub(crate) fn docs_found_by_words(hit: &PageHit) -> bool {
    hit.page.set == DOCS_SET && !hit.named
}

impl PageSearcher {
    /// Searches eligible pages before candidate caps. An explicit docs
    /// request (`docs`) uses docs-only candidates and allows short topic
    /// and symbol searches. A docs host or named product supplies the same
    /// context in ordinary search; a bare identifier needs strong symbol
    /// evidence. Exact operators still apply to the original page text.
    pub fn search_naming_docs(
        &self,
        words: &str,
        ops: &Operators,
        docs: bool,
        limit: usize,
    ) -> Result<Vec<PageHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let scope = self.page_scope(ops, docs);
        let mut hits = self.search_general(words, limit, scope.as_deref())?;
        for hit in self.search_docs(words, ops, docs, limit)? {
            match hits.iter_mut().find(|h| h.page == hit.page) {
                Some(kept) if kept.score >= hit.score => {}
                Some(kept) => *kept = hit,
                None => hits.push(hit),
            }
        }
        hits.retain(|hit| {
            operators_allow(ops, &hit.page) && docs_exact_query_allows(&hit.page, words)
        });
        site_pages_after_named_article(&mut hits);
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        truncate_keeping_inner_pages(&mut hits, limit);
        Ok(hits)
    }

    fn search_docs(
        &self,
        query: &str,
        ops: &Operators,
        explicit: bool,
        limit: usize,
    ) -> Result<Vec<PageHit>> {
        use plumb_core::docs::{asks_about, DOCS_SITES};
        let requested: Vec<_> = DOCS_SITES
            .iter()
            .filter(|site| asks_about(site, query) || docs_product_only(site, query))
            .collect();
        let context = explicit || !requested.is_empty() || docs_intent(query);
        let on_docs_host = DOCS_SITES.iter().any(|site| docs_host_requested(site, ops));
        let product_only =
            !requested.is_empty() && requested.iter().all(|site| docs_product_only(site, query));
        let mut clauses = Vec::new();
        let topic_stems = self.question_words(&docs_without_intent(query));
        if !topic_stems.is_empty() && !product_only && (context || on_docs_host) {
            let needed = if topic_stems.len() < QUESTION_QUERY_WORDS {
                topic_stems.len()
            } else {
                (topic_stems.len() as f32 * QUESTION_SHARE).ceil() as usize
            };
            clauses.push((
                Occur::Should,
                Box::new(BooleanQuery::with_minimum_required_clauses(
                    topic_stems
                        .iter()
                        .map(|stem| {
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(
                                    Term::from_field_text(self.fields.topic, stem),
                                    IndexRecordOption::Basic,
                                )) as Box<dyn Query>,
                            )
                        })
                        .collect(),
                    needed,
                )) as Box<dyn Query>,
            ));
        }
        // Qualified names and short suffixes are indexed separately from
        // broad topics, so popular questions cannot consume this window.
        let mut identities = Vec::new();
        let mut symbols = vec![canonical_docs_symbol(&docs_without_intent(query))];
        symbols.extend(
            requested
                .iter()
                .map(|site| canonical_docs_symbol(&docs_topic(site, query))),
        );
        for symbol in symbols.into_iter().flatten() {
            identities.push((Occur::Should, self.scope_term(&format!("symbol:{symbol}"))));
        }
        for site in &requested {
            let alias = naming_docs(site, &docs_topic(site, query));
            if let Some(key) = analysis::tokens(&self.joined, &alias).pop() {
                identities.push((Occur::Should, self.key_term(&key)));
            }
        }
        for site in requested
            .iter()
            .filter(|site| docs_product_only(site, query))
        {
            if explicit || docs_intent(query) || !ops.sites.is_empty() {
                identities.push((
                    Occur::Should,
                    self.scope_term(&format!("overview:{}", site.key)),
                ));
            }
        }
        // Pages named by a part of the topic have a window of their own,
        // so the pages named by all of it keep theirs.
        let named_parts = self.part_identities(&requested, query);
        if clauses.is_empty() && identities.is_empty() && named_parts.is_empty() {
            return Ok(Vec::new());
        }
        let mut filter = vec![(Occur::Must, self.page_scope(ops, true).expect("docs scope"))];
        if !requested.is_empty() {
            filter.push((
                Occur::Must,
                Box::new(BooleanQuery::new(
                    requested
                        .iter()
                        .map(|site| {
                            (
                                Occur::Should,
                                self.scope_term(&format!("docs:{}", site.key)),
                            )
                        })
                        .collect(),
                )) as Box<dyn Query>,
            ));
        }
        let scope = BooleanQuery::new(filter);
        let searcher = self.reader.searcher();
        let top = TopDocs::with_limit(CANDIDATES)
            .order_by_fast_field::<u64>("popularity", tantivy::Order::Desc);
        let mut addresses = Vec::new();
        for lane in [identities, named_parts, clauses]
            .into_iter()
            .filter(|lane| !lane.is_empty())
        {
            let wanted = scoped_query(&BooleanQuery::new(lane), Some(&scope));
            for (_, address) in
                searcher.search(self.language_query(wanted.as_ref()).as_ref(), &top)?
            {
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
        }
        let mut hits = Vec::new();
        let mut parts = Vec::new();
        for address in addresses {
            let document: TantivyDocument = searcher.doc(address)?;
            let Some(stored) = document
                .get_first(self.fields.page)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let page: Page = serde_json::from_str(stored)?;
            if !operators_allow(ops, &page) {
                continue;
            }
            let site = plumb_core::docs::site_of_url(&page.url);
            let host_context = site.is_some_and(|site| docs_host_requested(site, ops));
            let topic = site.map_or_else(
                || docs_without_intent(query),
                |site| docs_topic(site, query),
            );
            let product_only = site.is_some_and(|site| docs_product_only(site, query));
            let allowed = context || host_context;
            let (name, named, whole) = if product_only {
                if !allowed
                    || !(explicit || docs_intent(query) || host_context)
                    || !site.is_some_and(|site| docs_overview(&page, site))
                {
                    continue;
                }
                (PARTIAL_MATCH, false, false)
            } else if docs_symbols(&page)
                .iter()
                .any(|symbol| canonical_docs_symbol(&topic).as_ref() == Some(symbol))
                && (allowed || strong_docs_symbol(&page, &topic))
            {
                // Short identifiers remain supporting pages in ordinary
                // navigation; a product/host/typed request names them.
                (ALIAS_MATCH, allowed, allowed)
            } else if allowed && site.is_some_and(|site| self.named_by_part(site, &page, &topic)) {
                parts.push(hits.len());
                (ALIAS_MATCH, true, false)
            } else {
                if !allowed {
                    continue;
                }
                // A different receiver/namespace cannot satisfy an exact
                // qualified API symbol through ordinary word overlap.
                if canonical_docs_symbol(&topic).is_some() && topic.contains(['.', ':', '_']) {
                    continue;
                }
                let stems = self.question_words(&topic);
                let Some(text) = page.topic() else { continue };
                let words: HashSet<_> = self.question_words(&text).into_iter().collect();
                if stems.is_empty() || !stems.iter().all(|stem| words.contains(stem)) {
                    continue;
                }
                // A lone topic must occur in a title or heading, rather
                // than a passing mention in the truncated description.
                let rich = page.search.as_ref().and_then(SearchContent::bounded);
                let headings = std::iter::once(&page.title)
                    .chain(&page.sections)
                    .map(String::as_str)
                    .chain(rich.iter().flat_map(|search| {
                        search
                            .passages
                            .iter()
                            .map(|passage| passage.heading.as_str())
                    }))
                    .flat_map(|text| self.question_words(text))
                    .collect::<HashSet<_>>();
                // A case-shaped error literal in bounded source text is
                // also evidence, unlike a generic word in a description.
                let source_literal = rich.as_ref().is_some_and(|search| {
                    search.passages.iter().any(|passage| {
                        passage
                            .text
                            .split(|c: char| !c.is_alphanumeric() && c != '_')
                            .any(|word| {
                                word.chars().skip(1).any(char::is_uppercase)
                                    && word.eq_ignore_ascii_case(&topic)
                            })
                    })
                });
                if stems.len() == 1 && !headings.contains(&stems[0]) && !source_literal {
                    continue;
                }
                (PARTIAL_MATCH, false, false)
            };
            let popularity = document
                .get_first(self.fields.popularity)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as f32
                / POPULARITY_SCALE;
            hits.push(PageHit {
                score: name * (1.0 - POPULARITY_SHARE + POPULARITY_SHARE * popularity),
                page,
                named,
                popularity,
                whole,
                learned: None,
            });
        }
        self.unname_covered_parts(&mut hits, &parts, query);
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(limit);
        Ok(hits)
    }

    fn key_term(&self, key: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(self.fields.keys, key),
            IndexRecordOption::Basic,
        ))
    }

    /// What finds the pages of the `requested` sites that a part of the
    /// topic of `query` names, by its name or as a symbol.
    fn part_identities(
        &self,
        requested: &[&plumb_core::docs::DocsSite],
        query: &str,
    ) -> Vec<(Occur, Box<dyn Query>)> {
        let mut identities = Vec::new();
        for site in requested {
            for (part, _) in docs_parts(&docs_topic(site, query)) {
                if let Some(key) = analysis::tokens(&self.joined, &naming_docs(site, &part)).pop() {
                    identities.push((Occur::Should, self.key_term(&key)));
                }
                if let Some(symbol) = canonical_docs_symbol(&part) {
                    identities.push((Occur::Should, self.scope_term(&format!("symbol:{symbol}"))));
                }
            }
        }
        identities
    }

    /// Whether a part of `topic` names the docs page `page` of `site`, and
    /// the rest of `topic` is on the page or written as code: "python
    /// logging basicConfig" asks for logging's page, and "git rebase
    /// interactive" for git-rebase's, whose INTERACTIVE MODE section it
    /// is. "python random forest" is not about random's page.
    fn named_by_part(&self, site: &plumb_core::docs::DocsSite, page: &Page, topic: &str) -> bool {
        let key = |alias: &str| analysis::tokens(&self.joined, alias).pop();
        let keys: HashSet<_> = page.aliases.iter().filter_map(|alias| key(alias)).collect();
        // A whole name only: "load" is a short name of json.load and
        // pickle.load alike.
        let names = docs_whole_names(page);
        let Some((_, rest)) = docs_parts(topic).into_iter().find(|(part, _)| {
            key(&naming_docs(site, part)).is_some_and(|k| keys.contains(&k))
                || canonical_docs_symbol(part).is_some_and(|name| names.contains(&name))
        }) else {
            return false;
        };
        // A version ("3.12") is not code.
        let code = |word: &str| written_as_code(word) && word.chars().any(char::is_alphabetic);
        if topic.split_whitespace().any(code) {
            return true;
        }
        let stems = self.question_words(&rest);
        let on_page: HashSet<_> = page
            .topic()
            .map(|text| self.question_words(&text).into_iter().collect())
            .unwrap_or_default();
        !stems.is_empty() && stems.iter().all(|stem| on_page.contains(stem))
    }

    /// Leaves the pages of `hits` at `parts`, named by a part of the
    /// topic, unnamed when another's title has every word of the topic:
    /// "javascript array flat" asks for Array.prototype.flat(), not Array.
    fn unname_covered_parts(&self, hits: &mut [PageHit], parts: &[usize], query: &str) {
        for &at in parts {
            let Some(site) = plumb_core::docs::site_of_url(&hits[at].page.url) else {
                continue;
            };
            let topic = self.question_words(&docs_topic(site, query));
            let covered = hits.iter().enumerate().any(|(i, hit)| {
                let title: HashSet<_> = self.question_words(&hit.page.title).into_iter().collect();
                i != at && topic.iter().all(|stem| title.contains(stem))
            });
            if covered {
                let hit = &mut hits[at];
                hit.named = false;
                hit.score *= PARTIAL_MATCH / ALIAS_MATCH;
            }
        }
    }
}

/// Most words of a part of a docs search's topic that may name a page.
const PART_WORDS: usize = 3;

/// Most words of a topic whose parts may name a page: longer searches
/// are questions, which the page's words answer.
const PARTED_WORDS: usize = 6;

/// The parts of `topic` that may name a docs page, the longest first, each
/// with the rest of `topic`: "logging basicConfig" has "logging" (rest
/// "basicConfig") and "basicConfig" (rest "logging").
fn docs_parts(topic: &str) -> Vec<(String, String)> {
    let words: Vec<&str> = topic.split_whitespace().collect();
    if words.len() > PARTED_WORDS {
        return Vec::new();
    }
    (1..words.len().min(PART_WORDS + 1))
        .rev()
        .flat_map(|len| (0..=words.len() - len).map(move |start| (start, len)))
        .map(|(start, len)| {
            let rest = [&words[..start], &words[start + len..]].concat();
            (words[start..start + len].join(" "), rest.join(" "))
        })
        .collect()
}

/// Whether the docs page `hit` is just what `query` names in code's own
/// spelling ("Object.keys", "std::vector", "useState"), and the best of
/// `sites` is not called that, as jquery.com is for "jQuery". Such a page
/// leads: nobody types `Promise.all` for aclu.org or `std::vector` for
/// vector.dev, the sites that share a word with it. The page must hold
/// the whole name, so "macOS" is not `std::os::macos`.
pub(crate) fn docs_named_as_code(hit: &PageHit, query: &str, sites: &[crate::Hit]) -> bool {
    let query = query.trim();
    let Some(symbol) = canonical_docs_symbol(query) else {
        return false;
    };
    let name = squash(query);
    hit.page.set == DOCS_SET
        && written_as_code(query)
        && docs_whole_names(&hit.page).contains(&symbol)
        && strong_docs_symbol(&hit.page, query)
        && !sites.first().is_some_and(|best| {
            squash(&best.domain) == name
                || squash(best.domain.split('.').next().unwrap_or("")) == name
        })
}

/// Whether `query` is one name written as code: with call parentheses,
/// `::`, a `.`, `_` or `-` inside it ("read_csv", "z-index"), or a capital
/// after a small first letter ("useState"). "malloc" and "vector" may be
/// words.
fn written_as_code(query: &str) -> bool {
    let chars: Vec<char> = query.chars().collect();
    !query.contains(char::is_whitespace)
        && (query.ends_with("()")
            || query.contains("::")
            || chars.windows(3).any(|w| {
                w[0].is_alphanumeric() && matches!(w[1], '.' | '_' | '-') && w[2].is_alphanumeric()
            })
            || chars.first().is_some_and(|c| c.is_lowercase())
                && chars
                    .windows(2)
                    .any(|w| w[0].is_lowercase() && w[1].is_uppercase()))
}

/// Whether `query` asks about something in the docs of the docs page
/// `page`'s site (see [`plumb_core::docs::asks_about`]): "python sort
/// list" does for a page of Python's docs; "note taking app" for none.
pub(crate) fn docs_asked(page: &Page, query: &str) -> bool {
    plumb_core::docs::site_of_url(&page.url)
        .is_some_and(|site| plumb_core::docs::asks_about(site, query))
}

/// `words` with what `site`'s docs are of named first, as its pages'
/// other names are ("Python asyncio"), by its product's name or else by
/// a word that asks about it ("dotnet" for .NET).
pub(crate) fn naming_docs(site: &plumb_core::docs::DocsSite, words: &str) -> String {
    std::iter::once(site.product)
        .chain(site.asked_by.iter().copied())
        .map(|name| format!("{name} {words}"))
        .find(|query| plumb_core::docs::asks_about(site, query))
        .unwrap_or_else(|| format!("{} {words}", site.product))
}

const DOCS_INTENT_WORDS: &[&str] = &["doc", "docs", "documentation", "manual", "reference"];

fn docs_intent(query: &str) -> bool {
    query
        .split_whitespace()
        .any(|word| DOCS_INTENT_WORDS.contains(&word.to_lowercase().as_str()))
}

fn docs_without_intent(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|word| {
            let word = word.to_lowercase();
            !DOCS_INTENT_WORDS.contains(&word.as_str())
                && !ASKING_WORDS.contains(&word.as_str())
                && !plumb_core::is_function_word(&word)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Words that ask for docs, but only beside what the docs are about:
/// "python asyncio tutorial" asks for the asyncio page, and "python
/// tutorial" for the tutorial.
const DOCS_KIND_WORDS: &[&str] = &["tutorial", "tutorials", "guide", "example", "examples"];

fn docs_topic(site: &plumb_core::docs::DocsSite, query: &str) -> String {
    let mut kinds = Vec::new();
    let mut topic = Vec::new();
    for word in query.split_whitespace() {
        let lower = word.to_lowercase();
        let lower = lower.trim_matches(|c: char| !c.is_alphanumeric() && !"._:+#".contains(c));
        if lower == site.key
            || lower == site.product.to_lowercase()
            || site.asked_by.contains(&lower)
            || DOCS_INTENT_WORDS.contains(&lower)
            || ASKING_WORDS.contains(&lower)
            || plumb_core::is_function_word(lower)
        {
            continue;
        }
        if DOCS_KIND_WORDS.contains(&lower) {
            kinds.push(word);
        } else {
            topic.push(word);
        }
    }
    if topic.is_empty() { kinds } else { topic }.join(" ")
}

/// Substantive docs terms for selecting source excerpts from a result.
/// Uses the same product/intent rules as retrieval and removes operators.
pub fn docs_query_topic(page: &Page, query: &str) -> String {
    let words = Operators::parse(query).words;
    plumb_core::docs::site_of_url(&page.url).map_or_else(
        || docs_without_intent(&words),
        |site| docs_topic(site, &words),
    )
}

fn docs_exact_query_allows(page: &Page, query: &str) -> bool {
    if page.set != DOCS_SET {
        return true;
    }
    let topic = docs_query_topic(page, query);
    let Some(symbol) = canonical_docs_symbol(&topic) else {
        return true;
    };
    !topic.contains(['.', ':', '_']) || docs_symbols(page).contains(&symbol)
}

fn docs_product_only(site: &plumb_core::docs::DocsSite, query: &str) -> bool {
    let without = docs_without_intent(query);
    plumb_core::docs::named_site(&without).is_some_and(|named| named.key == site.key)
}

fn docs_host_requested(site: &plumb_core::docs::DocsSite, ops: &Operators) -> bool {
    site.roots
        .iter()
        .filter_map(|root| host_of(root))
        .any(|host| {
            ops.sites.iter().any(|wanted| {
                wanted.contains('.') && (host == *wanted || host.ends_with(&format!(".{wanted}")))
            })
        })
}

/// `text` as a docs symbol: lower case, without call parentheses or a
/// manual page's section ("ls(1)" documents ls), and only letters,
/// digits, `_`, `.`, `:` and inner hyphens ("border-radius",
/// "Content-Type").
fn canonical_docs_symbol(text: &str) -> Option<String> {
    let text = text.trim().trim_end_matches("()");
    let text = match text.strip_suffix(')').and_then(|t| t.rsplit_once('(')) {
        Some((name, section))
            if !section.is_empty() && section.chars().all(|c| c.is_ascii_digit()) =>
        {
            name
        }
        _ => text,
    };
    (!text.is_empty()
        && text.len() < 256
        && text.chars().any(char::is_alphabetic)
        && !text.starts_with('-')
        && !text.ends_with('-')
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || "_.:-".contains(c)))
    .then(|| text.to_lowercase())
}

/// The whole names the docs page `page` documents: its title before
/// " — ", the name its title and address share, and its rich symbols.
fn docs_whole_names(page: &Page) -> Vec<String> {
    let head = page.title.split(" — ").next().unwrap_or(&page.title);
    let rich = page.search.as_ref().and_then(SearchContent::bounded);
    std::iter::once(head)
        .chain(rich.iter().flat_map(|search| {
            search
                .symbols
                .iter()
                .map(|symbol| symbol.identifier.as_str())
        }))
        .filter_map(canonical_docs_symbol)
        .chain(docs_address_symbol(page))
        .collect()
}

/// The name a docs page's title writes as code and its address ends
/// with: MDN's "Document: querySelectorAll() method" at
/// …/Document/querySelectorAll, "border-radius CSS property" at
/// …/Properties/border-radius. Call parentheses alone don't make a name
/// code here, so "Window: open() method" doesn't name "open", nor
/// "Built-in Functions" at functions.html "functions".
fn docs_address_symbol(page: &Page) -> Option<String> {
    let leaf = docs_address_leaf(&page.url)?;
    let head = page.title.split(" — ").next().unwrap_or(&page.title);
    head.split_whitespace()
        .map(|word| word.trim_end_matches([':', ',']))
        .filter(|word| written_as_code(word.trim_end_matches("()")))
        .find_map(|word| canonical_docs_symbol(word).filter(|symbol| *symbol == leaf))
}

/// The last part of a docs page's address as a symbol: "querySelectorAll"
/// for …/Document/querySelectorAll, "tomllib" for …/library/tomllib.html.
fn docs_address_leaf(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or("");
    canonical_docs_symbol(path.rsplit('/').next()?.trim_end_matches(".html"))
}

fn docs_identifier_symbols(identifier: &str) -> Vec<String> {
    let Some(symbol) = canonical_docs_symbol(identifier) else {
        return Vec::new();
    };
    let mut symbols = vec![symbol.clone()];
    if let Some(short) = symbol
        .rsplit(['.', ':'])
        .next()
        .filter(|short| !short.is_empty() && *short != symbol)
    {
        symbols.push(short.to_string());
    }
    symbols
}

/// Exact API identifier or its unqualified suffix. A qualified query
/// never matches a different receiver through word overlap.
pub fn docs_symbol_matches(query: &str, identifier: &str) -> bool {
    canonical_docs_symbol(query)
        .is_some_and(|wanted| docs_identifier_symbols(identifier).contains(&wanted))
}

pub(crate) fn docs_symbols(page: &Page) -> Vec<String> {
    let mut symbols: Vec<String> = docs_whole_names(page)
        .iter()
        .flat_map(|name| docs_identifier_symbols(name))
        .collect();
    symbols.sort();
    symbols.dedup();
    symbols
}

fn strong_docs_symbol(page: &Page, query: &str) -> bool {
    let Some(symbol) = canonical_docs_symbol(query) else {
        return false;
    };
    if page
        .search
        .as_ref()
        .and_then(SearchContent::bounded)
        .is_some_and(|search| {
            search
                .symbols
                .iter()
                .any(|symbol| docs_symbol_matches(query, &symbol.identifier))
        })
    {
        return true;
    }
    let head = page.title.split(" — ").next().unwrap_or(&page.title);
    // Code punctuation/case, or a module with a subtitle whose URL names
    // the same identifier. Generic standalone titles such as Glossary
    // and Introduction remain ordinary navigation.
    head.contains(['.', ':', '_'])
        || head.chars().skip(1).any(char::is_uppercase)
        || (page.title.contains(" — ")
            && symbol.len() >= 5
            && docs_address_leaf(&page.url).as_ref() == Some(&symbol))
}

pub(crate) fn docs_overview(page: &Page, site: &plumb_core::docs::DocsSite) -> bool {
    let clean = |url: &str| {
        url.split(['?', '#'])
            .next()
            .unwrap_or("")
            .trim_end_matches("index.html")
            .trim_end_matches('/')
            .to_string()
    };
    let url = clean(&page.url);
    if site
        .roots
        .iter()
        .chain(site.index_pages)
        .any(|root| clean(root) == url)
    {
        return true;
    }
    // Common reference/tutorial overviews immediately below a profile
    // root. These are product entry points, rather than arbitrary deep
    // pages that happen to repeat the product in their aliases.
    page.url
        .split(['?', '#'])
        .next()
        .is_some_and(|url| url.ends_with("/index.html"))
        && site.roots.iter().any(|root| {
            url.strip_prefix(&clean(root))
                .and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(|rest| {
                    ["tutorial", "reference", "library", "guide", "manual", "api"].contains(&rest)
                })
        })
}
