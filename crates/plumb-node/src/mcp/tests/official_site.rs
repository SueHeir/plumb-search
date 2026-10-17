use super::*;
use crate::mcp::info::asked_page;

#[test]
fn official_site_says_how_sure_it_is_and_why() {
    let mut top = hit("paypal.com", 2.0, 0.9, true);
    top.official = true;
    let mcp = server(vec![top, hit("paypal-login.us", 0.8, 0.1, false)]);
    let reply = call(&mcp, "official_site", json!({ "name": "PayPal" }));
    let answer = &reply["result"]["structuredContent"];
    assert_eq!(reply["result"]["isError"], false);
    assert_eq!(answer["domain"], "paypal.com");
    assert_eq!(answer["url"], "https://www.paypal.com/");
    assert_eq!(answer["confidence"], "high");
    let why = answer["why"].to_string();
    assert!(why.contains("Wikidata"), "{why}");
    assert_eq!(answer["alternatives"][0]["domain"], "paypal-login.us");
    // The text for the model is the same answer, short.
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with(
            "Official site for \"PayPal\": paypal.com https://www.paypal.com/ (confidence high)\n"
        ),
        "{text}"
    );
    assert!(
        text.ends_with("Other candidates: paypal-login.us"),
        "{text}"
    );

    // Two sites by the same name, neither ahead: not sure.
    let mcp = server(vec![
        hit("delta.com", 1.0, 0.3, true),
        hit("deltafaucet.com", 0.98, 0.3, true),
    ]);
    let answer =
        &call(&mcp, "official_site", json!({ "name": "delta" }))["result"]["structuredContent"];
    assert_eq!(answer["confidence"], "low");
    assert!(answer["why"].to_string().contains("deltafaucet.com"));

    // Only words in common.
    let mcp = server(vec![hit("example.org", 1.0, 0.1, false)]);
    let answer = &call(&mcp, "official_site", json!({ "name": "some thing" }))["result"]
        ["structuredContent"];
    assert_eq!(answer["confidence"], "low");

    let answer = &call(
        &server(Vec::new()),
        "official_site",
        json!({ "name": "zzqx" }),
    )["result"]["structuredContent"];
    assert_eq!(answer["found"], false);
}

/// Lists a popular site first for a search, and the site the name names
/// first when asked whose site it is, as the site model may.
struct Ordered;

impl SearchBackend for Ordered {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Hit>> {
        Ok(vec![
            hit("any-video-converter.com", 1.0, 0.9, false),
            hit("converter.app", 0.9, 0.3, true),
        ])
    }

    fn search_names(
        &self,
        query: &str,
        limit: usize,
        _options: &SearchOptions,
    ) -> Result<SearchResults> {
        let mut hits = self.search(query, limit)?;
        hits.reverse();
        Ok(SearchResults {
            hits,
            ..SearchResults::default()
        })
    }

    fn num_docs(&self) -> u64 {
        2
    }
}

#[test]
fn official_site_looks_names_up_without_the_search_order() {
    let mcp = Mcp::new(Arc::new(Ordered), None);
    let answer =
        &call(&mcp, "official_site", json!({ "name": "Converter" }))["result"]["structuredContent"];
    assert_eq!(answer["domain"], "converter.app");
}

#[test]
fn official_site_falls_back_on_a_packages_home_page() {
    let mcp = Mcp::new(Arc::new(Packages), None);
    let reply = call(&mcp, "official_site", json!({ "name": "serde" }));
    let answer = &reply["result"]["structuredContent"];
    assert_eq!(answer["found"], true);
    assert_eq!(answer["url"], "https://serde.rs");
    assert_eq!(answer["package_home"], "https://serde.rs");
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("gives https://serde.rs as its home page"),
        "{text}"
    );
}

/// [`Packages`], with a site that only matches the words of every query.
struct PackagesAndAGuess;

impl SearchBackend for PackagesAndAGuess {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Hit>> {
        Ok(vec![hit("xapo.com", 0.4, 0.2, false)])
    }

    fn search_full(
        &self,
        query: &str,
        limit: usize,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        let mut results = Packages.search_full(query, limit, options)?;
        results.hits = self.search(query, limit)?;
        Ok(results)
    }

    fn num_docs(&self) -> u64 {
        1
    }
}

#[test]
fn official_site_prefers_a_packages_home_page_to_a_guess() {
    let mcp = Mcp::new(Arc::new(PackagesAndAGuess), None);
    let answer =
        &call(&mcp, "official_site", json!({ "name": "serde" }))["result"]["structuredContent"];
    assert_eq!(answer["url"], "https://serde.rs");
    assert_eq!(answer["domain"], "serde.rs");
    assert_eq!(answer["confidence"], "medium");
    assert_eq!(answer["alternatives"][0]["domain"], "xapo.com");
}

#[test]
fn official_site_takes_the_site_wikidata_gives_the_named_article() {
    // LifeWiki is conwaylife.com/wiki, not life-wiki.com.
    let mcp = scripted(|_| {
        results(
            vec![titled("life-wiki.com", "Free encyclopedia", 0.1, true)],
            vec![article(
                "LifeWiki",
                Some("conwaylife.com"),
                Some("https://conwaylife.com/wiki/"),
            )],
        )
    });
    let answer = official(&mcp, "LifeWiki");
    assert_eq!(answer["domain"], "conwaylife.com", "{answer}");
    assert_eq!(answer["url"], "https://conwaylife.com/wiki/");
    assert_eq!(answer["confidence"], "high");
    assert!(answer["why"].to_string().contains("LifeWiki"), "{answer}");
    assert_eq!(answer["alternatives"][0]["domain"], "life-wiki.com");

    // MathWorld is a subdomain of wolfram.com.
    let mcp = scripted(|_| {
        results(
            vec![titled("wolfram.com", "Wolfram", 0.6, false)],
            vec![article(
                "MathWorld",
                Some("wolfram.com"),
                Some("https://mathworld.wolfram.com/"),
            )],
        )
    });
    let answer = official(&mcp, "Wolfram MathWorld");
    assert_eq!(answer["domain"], "mathworld.wolfram.com", "{answer}");
    assert_eq!(answer["url"], "https://mathworld.wolfram.com/");
    assert_eq!(answer["confidence"], "high");

    // A senator's website is a page of senate.gov, which is not theirs, so
    // their article has no site; the website is still their official one.
    let mcp = scripted(|_| {
        results(
            vec![titled("senate.gov", "U.S. Senate", 0.6, false)],
            vec![article(
                "Bernie Sanders",
                None,
                Some("https://www.sanders.senate.gov/"),
            )],
        )
    });
    let answer = official(&mcp, "Bernie Sanders");
    assert_eq!(answer["domain"], "sanders.senate.gov", "{answer}");
    assert_eq!(answer["url"], "https://www.sanders.senate.gov/");

    // A well-known site of exactly the name keeps it: "zoom" is zoom.us,
    // whatever film is called Zoom.
    let mcp = scripted(|_| {
        let mut zoom = titled("zoom.us", "Zoom", 0.9, true);
        zoom.official = true;
        results(
            vec![zoom],
            vec![article("Zoom (film)", Some("zoomfilm.example"), None)],
        )
    });
    assert_eq!(official(&mcp, "zoom")["domain"], "zoom.us");
}

#[test]
fn official_site_is_unsure_of_a_namesake_of_an_article_without_a_site() {
    // Golly is a program whose article names no site; gollo.com is a shop.
    let mcp = scripted(|_| {
        results(
            vec![titled("gollo.com", "Gollo Costa Rica: Compras", 0.2, true)],
            vec![article("Golly (program)", None, None)],
        )
    });
    let answer = official(&mcp, "Golly");
    assert_eq!(answer["found"], false);
    assert_eq!(answer["status"], "unresolved");
    assert_eq!(answer["alternatives"][0]["domain"], "gollo.com");
    assert_eq!(answer["confidence"], "low", "{answer}");
    assert!(answer["why"].to_string().contains("Golly (program)"));
}

#[test]
fn official_site_keeps_an_unsupported_title_match_as_a_suggestion() {
    let mcp = scripted(|_| {
        let mut chess = titled("chess.com", "Chess.com - Play Chess Online", 0.9, false);
        chess.official = true;
        results(
            vec![
                chess,
                titled(
                    "chessprogramming.org",
                    "Main Page - Chess Programming Wiki",
                    0.25,
                    false,
                ),
            ],
            Vec::new(),
        )
    });
    let answer = official(&mcp, "Chess Programming Wiki");
    assert_eq!(answer["found"], false, "{answer}");
    assert_eq!(answer["alternatives"][0]["domain"], "chessprogramming.org");
    assert_eq!(answer["confidence"], "low");
}

#[test]
fn official_site_leaves_out_alternatives_with_nothing_of_the_name() {
    let mcp = server(vec![
        titled("norvig.com", "Peter Norvig", 0.3, true),
        titled("x.com", "X", 0.9, false),
        titled("norvig-fans.example", "Fans", 0.1, false),
    ]);
    let answer = official(&mcp, "norvig.com");
    assert_eq!(answer["domain"], "norvig.com");
    let others: Vec<&str> = answer["alternatives"]
        .as_array()
        .unwrap()
        .iter()
        .map(|alt| alt["domain"].as_str().unwrap())
        .collect();
    assert_eq!(others, ["norvig-fans.example"]);
}

#[test]
fn official_site_package_docs_need_a_matching_indexed_owner() {
    // An unrelated official site cannot verify a cached docs host.
    let mcp = scripted(|query| {
        if query.ends_with("package") {
            let page = Page::from_package(plumb_core::article::Article {
                title: "pillow".into(),
                item: Some("pypi:pillow".into()),
                views: 1_000,
                package: Some(plumb_core::packages::PackageInfo {
                    registry: "pypi".into(),
                    name: "pillow".into(),
                    homepage: Some("https://python-pillow.github.io".into()),
                    docs: Some("https://pillow.readthedocs.io".into()),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();
            let mut placed = article("x", None, None);
            placed.hit.page = page;
            return results(Vec::new(), vec![placed]);
        }
        let mut github = titled("github.com", "GitHub", 0.95, false);
        github.official = true;
        results(vec![github], Vec::new())
    });
    let answer = official(&mcp, "Pillow docs");
    assert_eq!(answer["found"], false, "{answer}");
    assert_eq!(answer["package_home"], "https://pillow.readthedocs.io");
    assert_eq!(answer["confidence"], "low");
}

#[test]
fn official_site_an_abbreviation_owner_does_not_prove_its_requested_api() {
    let mcp = scripted(|query| match query {
        "NPS" => {
            let mut owner = titled("nps.gov", "National Park Service", 0.7, true);
            owner.official = true;
            results(vec![owner], Vec::new())
        }
        _ => results(vec![titled("npmjs.org", "npm", 0.8, false)], Vec::new()),
    });
    let answer = official(&mcp, "NPS API developer");
    assert_eq!(answer["found"], false, "{answer}");
    assert_eq!(answer["confidence"], "low");
    assert!(answer["alternatives"]
        .as_array()
        .unwrap()
        .iter()
        .any(|alt| alt["domain"] == "nps.gov"));
}

#[test]
fn names_separate_navigation_but_keep_technical_qualifiers() {
    assert_eq!(bare_name("Pillow docs").as_deref(), Some("pillow"));
    assert_eq!(bare_name("NPS API developer"), None);
    assert_eq!(bare_name("Orion API docs").as_deref(), Some("orion api"));
    assert_eq!(bare_name("paypal login").as_deref(), Some("paypal"));
    assert_eq!(bare_name("Pillow"), None);
    assert_eq!(bare_name("docs"), None);
    assert_eq!(
        asked_page("https://www.nps.gov/yose/index.htm").as_deref(),
        Some("https://www.nps.gov/yose/index.htm")
    );
    assert_eq!(asked_page("nps.gov"), None);
    assert_eq!(asked_page("https://nps.gov/"), None);
}

#[test]
fn official_site_takes_the_named_article_over_a_site_that_shows_nothing_of_the_name() {
    // mo.gov goes by "USNO" somewhere, but shows nothing of it.
    let mcp = scripted(|_| {
        let mut mo = titled("mo.gov", "Home | MO.gov", 0.8, true);
        mo.official = true;
        let mut navy = titled("navy.mil", "United States Navy", 0.8, false);
        navy.official = true;
        results(
            vec![mo, navy],
            vec![article(
                "United States Naval Observatory",
                Some("navy.mil"),
                Some("https://www.usno.navy.mil/"),
            )],
        )
    });
    let answer = official(&mcp, "USNO");
    assert_eq!(answer["domain"], "usno.navy.mil", "{answer}");
}

#[test]
fn official_site_trusts_a_package_over_a_site_named_only_by_its_address() {
    let mcp = scripted(|query| {
        if query.ends_with("package") {
            return results(
                Vec::new(),
                vec![package_page(
                    "pypi",
                    "skyfield",
                    Some("https://rhodesmill.org/skyfield/"),
                    None,
                )],
            );
        }
        results(
            vec![titled("skyfield.cloud", "Cloud", 0.1, true)],
            Vec::new(),
        )
    });
    let answer = official(&mcp, "Skyfield");
    assert_eq!(
        answer["url"], "https://rhodesmill.org/skyfield/",
        "{answer}"
    );

    // A registry's own pages are no project's home: TOML is not docs.rs.
    let mcp = scripted(|query| {
        if query.ends_with("package") {
            return results(
                Vec::new(),
                vec![package_page(
                    "crates",
                    "toml",
                    None,
                    Some("https://docs.rs/toml"),
                )],
            );
        }
        results(
            vec![titled("toml.example", "Tom's", 0.1, false)],
            Vec::new(),
        )
    });
    let answer = official(&mcp, "TOML");
    assert_ne!(answer["domain"], "docs.rs", "{answer}");
}

#[test]
fn official_site_sends_docs_to_a_docs_site_of_its_own() {
    let mcp = scripted(|_| {
        let mut python = titled("python.org", "Welcome to Python.org", 0.9, true);
        python.official = true;
        results(vec![python], Vec::new())
    });
    let answer = official(&mcp, "Python docs");
    assert_eq!(answer["domain"], "docs.python.org", "{answer}");
    assert_eq!(answer["url"], "https://docs.python.org/");
    assert_eq!(official(&mcp, "Python")["domain"], "python.org");
}

#[test]
fn official_site_takes_no_package_docs_for_a_home() {
    // PyPI gives pandas only docs, pandas.readthedocs.io, which is no home.
    let mcp = scripted(|query| {
        if query.ends_with("package") {
            return results(
                Vec::new(),
                vec![package_page(
                    "pypi",
                    "pandas",
                    None,
                    Some("https://pandas.readthedocs.io/"),
                )],
            );
        }
        results(vec![titled("pydata.org", "PyData", 0.1, true)], Vec::new())
    });
    let answer = official(&mcp, "pandas");
    assert_eq!(answer["found"], false, "{answer}");
    assert_eq!(answer["package_home"], "https://pandas.readthedocs.io/");

    // A matching indexed project homepage corroborates the package's docs.
    let mcp = scripted(|query| {
        if query.ends_with("package") {
            return results(
                Vec::new(),
                vec![package_page(
                    "pypi",
                    "pillow",
                    Some("https://python-pillow.org/"),
                    Some("https://pillow.readthedocs.io/"),
                )],
            );
        }
        let mut pillow = titled("python-pillow.org", "Python Pillow", 0.3, false);
        pillow.official = true;
        results(vec![pillow], Vec::new())
    });
    let answer = official(&mcp, "Pillow docs");
    assert_eq!(answer["domain"], "pillow.readthedocs.io", "{answer}");
    assert_eq!(answer["alternatives"][0]["domain"], "python-pillow.org");
}

#[test]
fn official_site_is_unsure_of_a_name_its_site_does_not_show() {
    // lifewire.com goes by "LifeWiki" somewhere, but shows nothing of it.
    let mcp = scripted(|_| {
        let mut lifewire = titled("lifewire.com", "Lifewire: Tech News", 0.8, true);
        lifewire.official = true;
        results(vec![lifewire], Vec::new())
    });
    assert_ne!(official(&mcp, "LifeWiki")["confidence"], "high");

    // Two official, well-known sites of the name.
    let mcp = scripted(|_| {
        let mut europe = titled("elixir-europe.org", "Welcome to ELIXIR", 0.6, true);
        europe.official = true;
        let mut lang = titled(
            "elixir-lang.org",
            "The Elixir programming language",
            0.6,
            true,
        );
        lang.official = true;
        lang.score = 0.5;
        results(vec![europe, lang], Vec::new())
    });
    let answer = official(&mcp, "elixir");
    assert_eq!(answer["confidence"], "low", "{answer}");
    assert_eq!(answer["status"], "ambiguous");
    assert_eq!(answer["found"], false);

    // One alone is sure.
    let mcp = scripted(|_| {
        let mut lang = titled(
            "elixir-lang.org",
            "The Elixir programming language",
            0.6,
            true,
        );
        lang.official = true;
        results(vec![lang], Vec::new())
    });
    assert_eq!(official(&mcp, "elixir")["confidence"], "high");
}
