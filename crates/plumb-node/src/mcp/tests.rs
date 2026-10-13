use super::*;

mod info;
mod lookalike;
mod official_site;
mod owners;
mod protocol;
mod read;
mod search;

/// Answers every query with the same hits.
struct Fixed(Vec<Hit>);

impl SearchBackend for Fixed {
    fn search(&self, _query: &str, limit: usize) -> Result<Vec<Hit>> {
        Ok(self.0.iter().take(limit).cloned().collect())
    }

    fn num_docs(&self) -> u64 {
        self.0.len() as u64
    }
}

/// Fails every search.
struct Broken;

impl SearchBackend for Broken {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Hit>> {
        bail!("index is broken")
    }

    fn num_docs(&self) -> u64 {
        0
    }
}

fn hit(domain: &str, score: f32, link_score: f32, named: bool) -> Hit {
    Hit {
        demand: None,
        missing_words: false,
        query_evidence: None,
        placing_text_score: None,
        domain: domain.to_string(),
        url: format!("https://www.{domain}/"),
        title: Some(domain.to_string()),
        description: None,
        score,
        text_score: 1.0,
        link_score,
        country: None,
        named,
        official: false,
        key_pages: Vec::new(),
    }
}

fn server(hits: Vec<Hit>) -> Mcp {
    Mcp::new(Arc::new(Fixed(hits)), None)
}

fn call(mcp: &Mcp, tool: &str, arguments: Value) -> Value {
    mcp.handle(&json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    }))
    .expect("a call gets an answer")
}

/// Finds the crate serde for queries that ask for a package, and no site.
struct Packages;

impl SearchBackend for Packages {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Hit>> {
        Ok(Vec::new())
    }

    fn search_full(
        &self,
        query: &str,
        _limit: usize,
        _options: &SearchOptions,
    ) -> Result<SearchResults> {
        let mut pages = Vec::new();
        if plumb_core::packages::package_query(query).is_some_and(|asked| asked.wants("crates")) {
            let page = plumb_index::pages::Page::from_package(plumb_core::article::Article {
                title: "serde".into(),
                description: Some("A generic serialization/deserialization framework".into()),
                item: Some("crates:serde".into()),
                views: 1_000_000_000,
                package: Some(plumb_core::packages::PackageInfo {
                    registry: "crates".into(),
                    name: "serde".into(),
                    version: Some("1.0.228".into()),
                    released: Some("2025-09-27".into()),
                    license: Some("MIT OR Apache-2.0".into()),
                    repo: Some("https://github.com/serde-rs/serde".into()),
                    homepage: Some("https://serde.rs".into()),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();
            pages.push(plumb_index::pages::PlacedPage {
                hit: plumb_index::pages::PageHit {
                    page,
                    score: 0.9,
                    named: true,
                    popularity: 1.0,
                    whole: false,
                    learned: None,
                },
                under: None,
                at: 0,
            });
        }
        Ok(SearchResults {
            hits: Vec::new(),
            pages,
            site_search: None,
            spelling: None,
        })
    }

    fn num_docs(&self) -> u64 {
        0
    }
}

fn shelved(page: Page) -> plumb_index::pages::PageHit {
    plumb_index::pages::PageHit {
        page,
        score: 0.5,
        named: false,
        popularity: 0.5,
        whole: false,
        learned: None,
    }
}

/// Answers each query with what `answer` gives for it.
struct Scripted(Box<dyn Fn(&str) -> SearchResults + Send + Sync>);

impl SearchBackend for Scripted {
    fn search(&self, query: &str, _limit: usize) -> Result<Vec<Hit>> {
        Ok((self.0)(query).hits)
    }

    fn search_full(
        &self,
        query: &str,
        _limit: usize,
        _options: &SearchOptions,
    ) -> Result<SearchResults> {
        Ok((self.0)(query))
    }

    fn num_docs(&self) -> u64 {
        1
    }
}

fn scripted(answer: impl Fn(&str) -> SearchResults + Send + Sync + 'static) -> Mcp {
    Mcp::new(Arc::new(Scripted(Box::new(answer))), None)
}

fn titled(domain: &str, title: &str, link_score: f32, named: bool) -> Hit {
    Hit {
        title: Some(title.to_string()),
        ..hit(domain, 1.0, link_score, named)
    }
}

/// A Wikipedia article named by the query, whose item's official website
/// is on `site` (at `website` when that is a subdomain or inner page).
fn article(title: &str, site: Option<&str>, website: Option<&str>) -> PlacedPage {
    let page = Page::from_article(
        "en",
        plumb_core::article::Article {
            title: title.into(),
            description: Some(format!("about {title}")),
            item: Some("Q1".into()),
            site: site.map(str::to_string),
            website: website.map(str::to_string),
            views: 1000,
            ..Default::default()
        },
    );
    PlacedPage {
        hit: plumb_index::pages::PageHit {
            page,
            score: 0.9,
            named: true,
            popularity: 0.5,
            whole: true,
            learned: None,
        },
        under: None,
        at: 0,
    }
}

fn results(hits: Vec<Hit>, pages: Vec<PlacedPage>) -> SearchResults {
    SearchResults {
        hits,
        pages,
        site_search: None,
        spelling: None,
    }
}

fn official(mcp: &Mcp, name: &str) -> Value {
    call(mcp, "official_site", json!({ "name": name }))["result"]["structuredContent"].clone()
}

/// A package card of `name` from `registry`, found for "<name> package".
fn package_page(
    registry: &str,
    name: &str,
    homepage: Option<&str>,
    docs: Option<&str>,
) -> PlacedPage {
    let page = Page::from_package(plumb_core::article::Article {
        title: name.into(),
        item: Some(format!("{registry}:{name}")),
        views: 1_000,
        package: Some(plumb_core::packages::PackageInfo {
            registry: registry.into(),
            name: name.into(),
            homepage: homepage.map(str::to_string),
            docs: docs.map(str::to_string),
            ..Default::default()
        }),
        ..Default::default()
    })
    .unwrap();
    let mut placed = article("x", None, None);
    placed.hit.page = page;
    placed
}
