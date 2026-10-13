//! The `search` tool: the results, with what is listed beside them (an
//! answer, a profile, headlines, package cards and earlier findings).

use super::*;

/// Results `search` returns when the caller does not say...
const DEFAULT_SEARCH_LIMIT: usize = 5;

/// ...and when the search already has a direct answer: an answer found
/// before, a package's card, an instant answer or a site or page named by
/// the query. The rest are mostly sites with one of the query's words, and
/// cost a model tokens for nothing.
const DIRECT_SEARCH_LIMIT: usize = 3;

/// Headlines `search` returns.
const MAX_HEADLINES: usize = 5;

/// Most packages the `package` tool lists.
const MAX_PACKAGES: usize = 5;

/// Results asked for when looking up whose profile a query asks for.
const PROFILE_SEARCH_LIMIT: usize = 5;

/// Most words of a search whose package `search` guesses ("lodash
/// debounce", "numpy release notes")...
const MAX_GUESS_WORDS: usize = 4;

/// ...and the least popularity of the guessed package: only the best known
/// of its registry, so "express delivery" is not about a package.
const GUESSED_PACKAGE: f32 = 0.75;

/// Whether `page` is of the kind `kind` (one of [`KINDS`] other than
/// `site`).
fn kind_keeps(kind: &str, page: &Page) -> bool {
    use plumb_index::pages::{BOOKS_SET, DOCS_SET, GITHUB_SET, PAPERS_SET};
    match kind {
        "article" => page.is_article(),
        "question" => page.is_question(),
        "package" => page.package.is_some(),
        "repo" => page.set == GITHUB_SET,
        "book" => page.set == BOOKS_SET,
        "paper" => page.set == PAPERS_SET,
        "docs" => page.set == DOCS_SET,
        _ => false,
    }
}

/// Findings listed with a search's results.
const MAX_FOUND_BEFORE: usize = 3;

impl Mcp {
    /// `search`: the results a person would see, sites and pages, and
    /// what the results page shows with them. Without a `limit`, at most
    /// [`DEFAULT_SEARCH_LIMIT`] results, or [`DIRECT_SEARCH_LIMIT`] when
    /// the search has a direct answer. A search for a well-known package's
    /// docs or one of its functions gets the package's card too.
    pub fn search(
        &self,
        query: &str,
        limit: Option<usize>,
        options: &SearchOptions,
    ) -> Result<Value> {
        self.search_with(query, limit, options, false)
    }

    /// `search` with `kind` `site`: the sites alone, without the pages
    /// placed among them.
    pub fn search_sites(
        &self,
        query: &str,
        limit: Option<usize>,
        options: &SearchOptions,
    ) -> Result<Value> {
        let mut answer = self.search_with(query, limit, options, true)?;
        answer["kind"] = json!("site");
        Ok(answer)
    }

    /// `search` with another `kind`: the best pages of that kind alone
    /// ([`kind_keeps`]), from more pages than a search places among sites,
    /// and for a `site:` search the link into the site's own search.
    pub fn search_pages(
        &self,
        query: &str,
        kind: &str,
        limit: Option<usize>,
        options: &SearchOptions,
    ) -> Result<Value> {
        let limit = limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
        let found = if kind == "paper" {
            self.backend.papers(
                &plumb_core::paper_query::PaperQuery::parse(query)?,
                limit,
                options,
            )?
        } else {
            self.backend
                .pages_of(query, limit, options, kind == "docs", &|page| {
                    kind_keeps(kind, page)
                })
        };
        // Pages Plumb lacks may still be on the site.
        let site_search = if plumb_core::Operators::parse(query).sites.is_empty() {
            None
        } else {
            self.lookup(query, 1, options)?.site_search
        };
        let pages: Vec<Value> = found
            .iter()
            .enumerate()
            .map(|(i, hit)| page_entry(&hit.page, query, None, i + 1))
            .collect();
        Ok(json!({
            "query": query,
            "kind": kind,
            "results": [],
            "pages": pages,
            "site_search": site_search,
            "spelling": Value::Null,
            "paper_coverage": if kind == "paper" { self.backend.paper_coverage() } else { None },
            "coverage_message": if kind == "paper" && found.is_empty() && plumb_core::paper_query::PaperQuery::parse(query)?.constrained { Some("No matching papers in the indexed corpus with the requested publication precision; research may exist outside indexed coverage.") } else { None },
        }))
    }

    /// [`Mcp::search`], without any pages if `sites_only`.
    fn search_with(
        &self,
        query: &str,
        limit: Option<usize>,
        options: &SearchOptions,
        sites_only: bool,
    ) -> Result<Value> {
        let mut results = self.lookup(query, limit.unwrap_or(DEFAULT_SEARCH_LIMIT), options)?;
        if sites_only {
            results.pages.clear();
        }
        let guessed = match plumb_core::packages::install_command(query) {
            _ if sites_only => None,
            // "cargo add serde" asks for serde's card, whatever else is found.
            Some((registry, name)) => self.installed_package(registry, &name, options),
            None if results.pages.iter().any(|p| p.hit.page.package.is_some()) => None,
            None => self.guess_package(query, options),
        };
        if guessed.is_some() {
            // "fastapi docs" is not "fastai docs".
            results.spelling = None;
        }
        let places = if sites_only
            || plumb_core::Operators::parse(query).any()
            || plumb_index::places::parse_place_query(query).is_none()
        {
            None
        } else {
            crate::assembly::places(
                query,
                self.backend.places(query, None, options.country.as_deref()),
                &results.hits,
                options,
                |domain| {
                    self.node
                        .as_ref()
                        .is_some_and(|node| node.blocks_adult(domain))
                },
            )
        };
        if let Some(places) = &places {
            let local = crate::assembly::linked_sites(
                &places.found,
                |domain| self.backend.site(domain),
                |name| self.backend.search(name, 5).unwrap_or_default(),
            )
            .into_iter()
            .filter(|site| {
                options.safe == plumb_core::SafeSearch::Off
                    || !self
                        .node
                        .as_ref()
                        .is_some_and(|node| node.blocks_adult(&site.domain))
            })
            .collect();
            crate::assembly::local_first(
                &places.found,
                &mut results.hits,
                local,
                limit.unwrap_or(DEFAULT_SEARCH_LIMIT),
            );
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let answer = plumb_answer::answer(
            query,
            i64::try_from(now).unwrap_or(i64::MAX),
            self.rates.as_ref(),
        )
        .or_else(|| {
            let town = plumb_answer::asked_town(query)?;
            let place = self.backend.locate(&town, options.country.as_deref())?;
            crate::web::answers::time_in_place(&place)
        });
        crate::assembly::route_sources(
            query,
            answer.as_ref().map(|a| a.kind),
            options.country.as_deref(),
            &mut results,
            limit.unwrap_or(DEFAULT_SEARCH_LIMIT),
            |domain| self.backend.site(domain),
        );
        // Placed as the results page places them, which the info box needs.
        let found_pages = results.pages.iter().map(|p| p.hit.clone()).collect();
        let operators = plumb_core::Operators::parse(query);
        let mut placed = if operators.any() {
            place_operator_pages(&operators, &results.hits, found_pages)
        } else {
            place_pages(query, &results.hits, found_pages)
        };
        let promote_package = crate::assembly::promote_package(query);
        if !promote_package {
            let mut supplementary = Vec::new();
            placed.retain(|page| {
                if page.hit.page.package.is_some() {
                    supplementary.push(page.clone());
                    false
                } else {
                    true
                }
            });
            let after = results
                .hits
                .len()
                .max(placed.iter().map(|p| p.at + 1).max().unwrap_or(0));
            for mut page in supplementary {
                page.at = after;
                page.under = None;
                placed.push(page);
            }
        }
        if let Some(page) = &guessed {
            placed.retain(|p| p.hit.page.url != page.url);
            let card = PlacedPage {
                hit: plumb_index::pages::PageHit {
                    page: page.clone(),
                    score: 0.0,
                    named: false,
                    popularity: 0.0,
                    whole: false,
                    learned: None,
                },
                under: None,
                at: if promote_package {
                    0
                } else {
                    results
                        .hits
                        .len()
                        .max(placed.iter().map(|p| p.at + 1).max().unwrap_or(0))
                },
            };
            if promote_package {
                placed.insert(0, card);
            } else {
                placed.push(card);
            }
        }
        // A fact the query asks about something ("capital of australia").
        let answer = match (answer, plumb_core::facts::fact_asked(query)) {
            (None, Some(asked)) => self
                .backend
                .entities(&asked.subject, 20, options)
                .ok()
                .and_then(|found| {
                    match answers::resolve_entity(&asked.subject, &found, Some(&asked.kinds)) {
                        answers::EntityResolution::Resolved(page) => {
                            answers::fact_answer_from_page(&asked, page, now)
                        }
                        _ => None,
                    }
                }),
            (answer, _) => answer,
        };
        // What something is ("what is a manatee").
        let answer = match (answer, answers::definition_asked(query)) {
            (None, Some(name)) => {
                let word = self
                    .backend
                    .definition(&name)
                    .and_then(|page| answers::word_answer(&page));
                if word.is_some() && answers::asks_word(query) {
                    word
                } else {
                    self.lookup(&name, PROFILE_SEARCH_LIMIT, options)
                        .ok()
                        .and_then(|found| answers::definition_answer(&found.pages))
                        .or(word)
                }
            }
            (answer, _) => answer,
        };
        // A task with no answer-bearing rows must not turn its related package into the answer.
        let mut related_packages = Vec::new();
        if !promote_package
            && query.split_whitespace().count() > 1
            && answer.is_none()
            && results.hits.is_empty()
            && placed.iter().all(|p| p.hit.page.package.is_some())
        {
            related_packages = placed
                .iter()
                .take(limit.unwrap_or(DEFAULT_SEARCH_LIMIT).min(MAX_PACKAGES))
                .map(|p| package_card(&p.hit.page))
                .collect();
            placed.clear();
        }
        let names_a_page = placed.iter().any(|placed| placed.hit.named);
        let profile = answers::profile_lookups(query).iter().find_map(|name| {
            let entities = self.backend.entities(name, 20, options).ok()?;
            let entities = answers::profile_candidates(query, name, entities);
            let resolution = answers::resolve_entity(name, &entities, None);
            if let answers::EntityResolution::Resolved(page) = &resolution {
                let profile = answers::profile_answer_from_page(query, page);
                if profile.is_some() {
                    return profile;
                }
            }
            if !answers::searches_songs(query, &resolution) {
                return None;
            }
            self.lookup(name, PROFILE_SEARCH_LIMIT, options)
                .ok()
                .and_then(|found| answers::songs_profile(query, &resolution, found.pages))
        });
        let info = match &profile {
            Some(profile) => answers::info_from_page(&profile.page, &results.hits),
            None if operators.any() => None,
            None => answers::info_box(&results.hits, &placed),
        };
        let recent = (!sites_only)
            .then_some(self.node.as_ref())
            .flatten()
            .and_then(|node| {
                let top = results
                    .hits
                    .first()
                    .map(|hit| (hit.domain.as_str(), hit.named));
                crate::assembly::recent(
                    query,
                    node.recent(query, top),
                    options,
                    limit.unwrap_or(DEFAULT_SEARCH_LIMIT),
                    |domain| node.blocks_adult(domain),
                )
            });
        let headlines: Vec<Value> = recent
            .iter()
            .flat_map(|recent| recent.headlines.iter().take(MAX_HEADLINES))
            .map(|headline| {
                json!({
                    "title": headline.title,
                    "url": headline.url,
                    "site": headline.domain,
                    "published": crate::web::time_ago(headline.at, now),
                    "published_at": headline.at,
                    "published_date": crate::news::published_date(headline.at),
                    "source": "publisher_feed",
                })
            })
            .collect();
        let found_before: Vec<Value> = self
            .findings
            .iter()
            .flat_map(|findings| findings.for_query(query, MAX_FOUND_BEFORE))
            .map(|finding| {
                json!({
                    "query": finding.query,
                    "url": finding.url,
                    "why": finding.why,
                    "answer": finding.answer,
                    "task": finding.task,
                    "reported": crate::web::time_ago(finding.at, now),
                    "reported_at": finding.at,
                    "source": "user_reported_finding",
                })
            })
            .collect();
        let known: Vec<&str> = found_before
            .iter()
            .filter_map(|found| found["url"].as_str())
            .collect();
        let leads = self.leads_for(query, &known, now, options);
        let direct = !found_before.is_empty()
            || answer.is_some()
            || (promote_package && placed.iter().any(|page| page.hit.page.package.is_some()))
            || names_a_page
            || results.hits.iter().any(|hit| hit.named);
        let cap = match limit {
            Some(limit) => limit,
            None if direct => DIRECT_SEARCH_LIMIT,
            None => DEFAULT_SEARCH_LIMIT,
        };
        let assembled = crate::assembly::Assembled {
            rows: crate::assembly::ordered_rows(&results.hits, &placed, cap),
            places,
            recent: recent.clone(),
            answer: answer.as_ref(),
            profile: profile.as_ref(),
        };
        let mut sites = Vec::new();
        let mut pages = Vec::new();
        for row in &assembled.rows {
            match row {
                crate::assembly::Row::Site {
                    site,
                    pages: supporting,
                } => {
                    sites.push(brief_with(site, &results.pages));
                    pages.extend(supporting.iter().map(|page| {
                        page_entry(&page.hit.page, query, page.under.as_deref(), page.at + 1)
                    }));
                }
                crate::assembly::Row::Page { page } => pages.push(page_entry(
                    &page.hit.page,
                    query,
                    page.under.as_deref(),
                    page.at + 1,
                )),
            }
        }
        let ordered_results: Vec<_> = assembled
            .rows
            .iter()
            .map(|row| match row {
                crate::assembly::Row::Site { site, .. } => json!({"kind": "site", "url": site.url}),
                crate::assembly::Row::Page { page } => {
                    let url = pages
                        .iter()
                        .find(|entry| {
                            entry["page_url"].as_str().or_else(|| entry["url"].as_str())
                                == Some(page.hit.page.url.as_str())
                        })
                        .and_then(|entry| entry["url"].as_str())
                        .unwrap_or(&page.hit.page.url);
                    json!({"kind": "page", "url": url})
                }
            })
            .collect();
        let mut answer_json = json!({
            "query": query,
            "results": sites,
            "pages": pages,
            "site_search": results.site_search,
            "spelling": results.spelling,
        });
        let fields = answer_json.as_object_mut().expect("an object");
        fields.insert("ordered_results".into(), json!(ordered_results));
        if !related_packages.is_empty() {
            fields.insert("related_packages".into(), json!(related_packages));
        }
        if let Some(places) = &assembled.places {
            fields.insert("places".into(), json!(places));
        }
        if let Some(recent) = &recent {
            fields.insert(
                "news_status".into(),
                json!({ "site": recent.site,
                "status": recent.status, "sources": recent.sources }),
            );
        }
        drop(assembled);
        if let Some(answer) = answer {
            fields.insert("answer".into(), json!(answer));
        }
        if let Some(profile) = profile {
            fields.insert("profile".into(), json!(profile));
        }
        if let Some(info) = info {
            fields.insert("about".into(), json!(info));
        }
        // Headlines with a package's name are rarely about the package:
        // "react latest" is not footballers' kids reacting to a new kit.
        if !headlines.is_empty()
            && (!promote_package || !pages.iter().any(|page| page.get("package").is_some()))
        {
            fields.insert("recent".into(), json!(headlines));
        }
        let from_plugins: Vec<Value> = self
            .plugins
            .iter()
            .map(|found| {
                let results: Vec<Value> = found
                    .results
                    .iter()
                    .map(|item| {
                        json!({
                            "title": item.title,
                            "url": item.url,
                            "site": item.site,
                            "snippet": item.snippet,
                            "published": item.published.map(|at| crate::web::time_ago(at, now)),
                        })
                    })
                    .collect();
                json!({ "plugin": found.name, "results": results })
            })
            .collect();
        if !from_plugins.is_empty() {
            fields.insert("plugins".into(), json!(from_plugins));
        }
        if !found_before.is_empty() {
            fields.insert("found_before".into(), json!(found_before));
        }
        if !leads.is_empty() {
            fields.insert("leads".into(), json!(leads));
        }
        Ok(answer_json)
    }

    /// The well-known package a search is about when it says no registry:
    /// its first word other than filler, of a search of a few words
    /// ("lodash debounce", "numpy release notes", "fastapi docs").
    fn guess_package(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Option<plumb_index::pages::Page> {
        if plumb_core::Operators::parse(query).any() {
            return None;
        }
        let words: Vec<&str> = query.split_whitespace().collect();
        if !(2..=MAX_GUESS_WORDS).contains(&words.len()) {
            return None;
        }
        let name = words.into_iter().find(|word| {
            !plumb_core::packages::FILLER_WORDS.contains(&word.to_lowercase().as_str())
        })?;
        let found = self.lookup(&format!("{name} package"), 1, options).ok()?;
        found
            .pages
            .into_iter()
            .map(|placed| placed.hit)
            .find(|hit| {
                hit.popularity >= GUESSED_PACKAGE
                    && hit.page.package.as_ref().is_some_and(|p| {
                        p.name.eq_ignore_ascii_case(name)
                            || p.registry()
                                .and_then(|r| r.short_name(&p.name))
                                .is_some_and(|short| short.eq_ignore_ascii_case(name))
                    })
            })
            .map(|hit| hit.page)
    }

    /// The package an install command names, from its registry.
    fn installed_package(
        &self,
        registry: &plumb_core::packages::Registry,
        name: &str,
        options: &SearchOptions,
    ) -> Option<plumb_index::pages::Page> {
        let mut query = format!("{name} package");
        if let Some(word) = registry.words.first().or(registry.languages.first()) {
            query.push(' ');
            query.push_str(word);
        }
        let found = self.lookup(&query, 1, options).ok()?;
        found
            .pages
            .into_iter()
            .map(|placed| placed.hit.page)
            .find(|page| {
                page.package.as_ref().is_some_and(|package| {
                    package.registry().is_some_and(|r| r.key == registry.key)
                        && package.name.eq_ignore_ascii_case(name)
                })
            })
    }

    /// `package`: the cards of the packages called `name`, of `registry`
    /// or of any, the most used first.
    pub fn package(
        &self,
        name: &str,
        registry: Option<&plumb_core::packages::Registry>,
        options: &SearchOptions,
    ) -> Result<Value> {
        // "package" asks for any package of the name, the registry's
        // word or language for its own.
        let mut query = format!("{name} package");
        if let Some(word) = registry.and_then(|r| r.words.first().or(r.languages.first())) {
            query.push(' ');
            query.push_str(word);
        }
        let results = self.lookup(&query, 1, options)?;
        let packages: Vec<Value> = results
            .pages
            .iter()
            .map(|placed| &placed.hit.page)
            .filter(|page| page.package.is_some())
            .take(MAX_PACKAGES)
            .map(package_card)
            .collect();
        Ok(json!({
            "name": name,
            "found": !packages.is_empty(),
            "packages": packages,
        }))
    }
}

/// A page among `search`'s results: listed under the site `under`, or
/// alone at `position` (from 1) among the sites.
pub(super) fn page_entry(page: &Page, query: &str, under: Option<&str>, position: usize) -> Value {
    let mut entry = json!({
        "title": page.title,
        "url": page.url,
        "description": page.description.as_deref().map(short),
        "set": page.set,
        "about_site": under,
        "position": position,
    });
    if page.package.is_some() {
        entry["package"] = package_card(page);
    }
    if let Some(free) = page.free_copy() {
        entry["free_copy"] = json!(free);
    }
    if page.set == plumb_index::pages::PAPERS_SET {
        entry["paper"] = json!(page.paper);
        entry["count"] = page.paper.as_ref().map_or_else(
            || json!({ "value": page.views, "kind": "unknown" }),
            |paper| json!({ "value": paper.count, "kind": paper.count_kind }),
        );
    }
    if let Some(language) = &page.content_language {
        entry["language"] = json!(language);
    }
    add_source_excerpt(&mut entry, page, query);
    entry
}

/// One bounded, query-matched source passage and at most one exact API
/// symbol. Metadata is source evidence, not a generated answer.
fn add_source_excerpt(entry: &mut Value, page: &Page, query: &str) {
    let Some(search) = page
        .search
        .as_ref()
        .and_then(plumb_core::article::SearchContent::bounded)
    else {
        return;
    };
    let ops = plumb_core::Operators::parse(query);
    let topic = docs_query_topic(page, query);
    if topic.is_empty() {
        return;
    }
    let symbol = search
        .symbols
        .iter()
        .filter(|symbol| docs_symbol_matches(&topic, &symbol.identifier))
        .min_by_key(|symbol| symbol.anchor.is_none());
    // Do not turn an unmatched qualified identifier into a loose prose
    // match for a different receiver or namespace.
    if symbol.is_none() && topic.contains(['.', ':', '_']) && docs_symbol_matches(&topic, &topic) {
        return;
    }
    let terms: HashSet<_> = plumb_core::normalize_text(&topic)
        .split_whitespace()
        .filter(|word| !plumb_core::is_function_word(word))
        .map(str::to_string)
        .collect();
    let passage = search
        .passages
        .iter()
        .enumerate()
        .filter_map(|(i, passage)| {
            if !ops.allows_text([passage.heading.as_str(), passage.text.as_str()]) {
                return None;
            }
            let source = format!("{} {}", passage.heading, passage.text);
            let exact = symbol.map_or(0, |symbol| {
                if symbol.anchor.is_some() && symbol.anchor == passage.anchor {
                    2
                } else if source
                    .split(|c: char| !c.is_ascii_alphanumeric() && !"_.:".contains(c))
                    .any(|word| {
                        docs_symbol_matches(word.trim_matches(['.', ':']), &symbol.identifier)
                    })
                {
                    1
                } else {
                    0
                }
            });
            let normalized = plumb_core::normalize_text(&source);
            let words: HashSet<_> = normalized.split_whitespace().collect();
            let matched = terms
                .iter()
                .filter(|term| {
                    words.contains(term.as_str())
                        || plumb_core::other_number(term)
                            .is_some_and(|other| words.contains(other.as_str()))
                })
                .count();
            let needed = if terms.len() <= 3 {
                terms.len()
            } else {
                (terms.len() * 3).div_ceil(4)
            };
            // With an exact symbol, require its own section or literal. The
            // same query words in another method's prose are weaker evidence.
            ((symbol.is_some() && exact > 0)
                || (symbol.is_none() && !terms.is_empty() && matched >= needed))
                .then_some(((exact, matched, std::cmp::Reverse(i)), passage))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, passage)| passage);
    let fragment = symbol
        .and_then(|symbol| symbol.anchor.as_deref())
        .or_else(|| passage.and_then(|passage| passage.anchor.as_deref()));
    if let Some(url) = fragment.and_then(|anchor| source_fragment_url(&page.url, anchor)) {
        entry["page_url"] = json!(page.url);
        entry["url"] = json!(url);
    }
    if let Some(symbol) = symbol {
        let url = symbol
            .anchor
            .as_deref()
            .and_then(|anchor| source_fragment_url(&page.url, anchor))
            .unwrap_or_else(|| page.url.clone());
        entry["matched_symbol"] = json!({"identifier": symbol.identifier, "url": url});
    }
    if let Some(passage) = passage {
        let url = passage
            .anchor
            .as_deref()
            .and_then(|anchor| source_fragment_url(&page.url, anchor))
            .unwrap_or_else(|| page.url.clone());
        entry["source_excerpt"] = json!({
            "heading": passage.heading,
            "text": passage.text,
            "url": url,
        });
    }
}

/// A fragment always stays on the original HTTP(S) source. URL parsing
/// encodes fragment punctuation; whitespace/control IDs are discarded.
pub(super) fn source_fragment_url(source: &str, anchor: &str) -> Option<String> {
    let anchor = plumb_core::article::search_anchor(anchor)?;
    let mut url = url::Url::parse(source).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_fragment(Some(&anchor));
    Some(url.into())
}

/// A package's card in a tool's answer.
fn package_card(page: &plumb_index::pages::Page) -> Value {
    let Some(package) = &page.package else {
        return Value::Null;
    };
    json!({
        "name": package.name,
        "registry": page.set_name(),
        "description": page.description.as_deref().map(short),
        "version": package.version,
        "released": package.released,
        "license": package.license,
        "install": package.install(),
        "docs": package.docs(),
        "repo": package.repo,
        "homepage": package.homepage,
        "url": page.url,
    })
}

/// A site in a tool's answer.
pub(super) fn brief(hit: &Hit) -> Value {
    json!({
        "domain": hit.domain,
        "url": hit.url,
        "title": hit.title,
        "description": hit.description.as_deref().map(short),
        "official": hit.official,
        "well_known": hit.link_score >= WELL_KNOWN_LINK_SCORE,
        "country": hit.country,
    })
}

/// [`brief`], official too when Wikidata gives the site as the official
/// website of an article among `pages`.
pub(super) fn brief_with(hit: &Hit, pages: &[PlacedPage]) -> Value {
    let mut site = brief(hit);
    if !hit.official && article_of(&hit.domain, pages).is_some() {
        site["official"] = json!(true);
    }
    site
}
