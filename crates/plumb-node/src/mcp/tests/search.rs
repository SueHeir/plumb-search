use super::*;
use crate::mcp::search::{page_entry, source_fragment_url};

fn rich_source_page() -> Page {
    use plumb_core::article::{Article, SearchContent, SearchPassage, SearchSymbol};
    Page::from_docs(Article {
        title: "Node — Godot documentation".into(),
        item: Some("https://docs.godotengine.org/en/stable/classes/class_node.html".into()),
        language: Some("en".into()),
        search: Some(SearchContent {
            symbols: vec![SearchSymbol {
                identifier: "Node.set_multiplayer_authority".into(),
                anchor: Some("class-node-method-set-multiplayer-authority".into()),
            }],
            passages: vec![
                SearchPassage {
                    heading: "Scene tree".into(),
                    text: "Unrelated scene tree overview.".into(),
                    anchor: Some("scene-tree".into()),
                },
                SearchPassage {
                    heading: "set_multiplayer_authority".into(),
                    text: "Sets the node's multiplayer authority to the given peer identifier."
                        .into(),
                    anchor: Some("class-node-method-set-multiplayer-authority".into()),
                },
                SearchPassage {
                    heading: "Example".into(),
                    text: "Node.set_multiplayer_authority also appears in an unrelated example."
                        .into(),
                    anchor: Some("example".into()),
                },
            ],
            ..SearchContent::default()
        }),
        ..Article::default()
    })
    .unwrap()
}

#[test]
fn rich_source_exact_symbol_selects_its_own_excerpt_and_fragment() {
    let page = rich_source_page();
    for query in [
        "godot Node.set_multiplayer_authority()",
        "set_multiplayer_authority site:docs.godotengine.org",
    ] {
        let entry = page_entry(&page, query, Some("godotengine.org"), 2);
        assert_eq!(
            entry["url"],
            format!("{}#class-node-method-set-multiplayer-authority", page.url)
        );
        assert_eq!(entry["page_url"], page.url);
        assert_eq!(
            entry["matched_symbol"]["identifier"],
            "Node.set_multiplayer_authority"
        );
        assert_eq!(
            entry["source_excerpt"]["text"],
            "Sets the node's multiplayer authority to the given peer identifier."
        );
        assert_eq!(entry["source_excerpt"]["url"], entry["url"]);
        assert_eq!(entry["language"], "en");
        assert_eq!(entry["position"], 2);
        assert_eq!(entry["about_site"], "godotengine.org");
        assert!(entry.get("search").is_none());
        assert!(!entry.to_string().contains("Unrelated scene tree overview"));
    }
    for query in [
        "godot Other.set_multiplayer_authority",
        "godot",
        "unmatched topic",
    ] {
        let entry = page_entry(&page, query, None, 1);
        assert!(entry.get("source_excerpt").is_none(), "{query}: {entry}");
        assert!(entry.get("matched_symbol").is_none(), "{query}: {entry}");
        assert_eq!(entry["url"], page.url);
    }
}

#[test]
fn rich_source_prose_match_is_bounded_and_honors_phrases_and_exclusions() {
    let mut page = rich_source_page();
    page.search.as_mut().unwrap().symbols.clear();
    let entry = page_entry(&page, "godot multiplayer authority", None, 1);
    assert_eq!(
        entry["source_excerpt"]["heading"],
        "set_multiplayer_authority"
    );
    assert!(entry.get("matched_symbol").is_none());
    for query in [
        "godot multiplayer authority -authority",
        "godot \"multiplayer authority\" \"missing phrase\"",
    ] {
        assert!(page_entry(&page, query, None, 1)
            .get("source_excerpt")
            .is_none());
    }
    let excluded = page_entry(&page, "godot multiplayer authority -peer", None, 1);
    assert!(!excluded["source_excerpt"]["text"]
        .as_str()
        .unwrap()
        .contains("peer"));
    page.search.as_mut().unwrap().passages[1].text =
        format!("Multiplayer authority {}", "é".repeat(800));
    let entry = page_entry(&page, "multiplayer authority", None, 1);
    assert!(
        entry["source_excerpt"]["text"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            <= plumb_core::article::MAX_SEARCH_PASSAGE_CHARS
    );
    page.search.as_mut().unwrap().version = 99;
    assert!(page_entry(&page, "multiplayer authority", None, 1)
        .get("source_excerpt")
        .is_none());
    page.search = None;
    assert!(page_entry(&page, "multiplayer authority", None, 1)
        .get("source_excerpt")
        .is_none());
}

#[test]
fn rich_source_fragment_links_keep_the_source_and_reject_invalid_ids() {
    let source = "https://docs.godotengine.org/page?version=4#old";
    let link = source_fragment_url(source, "method-\"<authority>é").unwrap();
    let parsed = url::Url::parse(&link).unwrap();
    assert_eq!(parsed.host_str(), Some("docs.godotengine.org"));
    assert_eq!(parsed.path(), "/page");
    assert_eq!(parsed.query(), Some("version=4"));
    assert!(link.contains("%22%3Cauthority%3E%C3%A9"), "{link}");
    for anchor in ["", "bad id", "bad\nid"] {
        assert!(source_fragment_url(source, anchor).is_none());
    }
    assert!(source_fragment_url("javascript:alert(1)", "method").is_none());
    assert!(source_fragment_url("file:///tmp/page", "method").is_none());
    let mut page = rich_source_page();
    let search = page.search.as_mut().unwrap();
    search.symbols[0].anchor = Some("bad id".into());
    search.passages[1].anchor = Some("bad id".into());
    let entry = page_entry(&page, "set_multiplayer_authority", None, 1);
    assert_eq!(entry["url"], page.url);
    assert_eq!(entry["source_excerpt"]["url"], page.url);
    assert!(entry.get("page_url").is_none());
}

#[test]
fn rich_source_error_query_selects_the_matching_source_passage() {
    let mut page = rich_source_page();
    page.url = "https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/".into();
    page.title = "Pod Lifecycle".into();
    let search = page.search.as_mut().unwrap();
    search.symbols.clear();
    search.passages[1].heading = "Container failures".into();
    search.passages[1].text =
        "A container can enter CrashLoopBackOff when it repeatedly fails.".into();
    search.passages[1].anchor = Some("container-restarts".into());
    for query in ["k8s CrashLoopBackOff", "crashloopbackoff"] {
        let entry = page_entry(&page, query, None, 1);
        assert_eq!(
            entry["source_excerpt"]["text"],
            "A container can enter CrashLoopBackOff when it repeatedly fails."
        );
        assert_eq!(entry["url"], format!("{}#container-restarts", page.url));
        assert!(entry.get("matched_symbol").is_none());
    }
}

struct RichSource {
    supporting: bool,
}

impl SearchBackend for RichSource {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Hit>> {
        Ok(Vec::new())
    }
    fn num_docs(&self) -> u64 {
        1
    }
    fn search_full(
        &self,
        _query: &str,
        _limit: usize,
        _options: &SearchOptions,
    ) -> Result<SearchResults> {
        let mut page = shelved(rich_source_page());
        page.named = !self.supporting;
        page.whole = true;
        Ok(SearchResults {
            hits: if self.supporting {
                vec![hit("godotengine.org", 1.0, 0.5, true)]
            } else {
                Vec::new()
            },
            pages: vec![PlacedPage {
                hit: page,
                under: self.supporting.then(|| "godotengine.org".into()),
                at: 0,
            }],
            site_search: None,
            spelling: None,
        })
    }
    fn pages_of(
        &self,
        _query: &str,
        limit: usize,
        _options: &SearchOptions,
        _docs: bool,
        keep: &dyn Fn(&Page) -> bool,
    ) -> Vec<plumb_index::pages::PageHit> {
        [rich_source_page()]
            .into_iter()
            .filter(keep)
            .take(limit)
            .map(shelved)
            .collect()
    }
}

#[test]
fn rich_source_excerpts_reach_ordinary_typed_and_text_only_mcp_search() {
    for supporting in [false, true] {
        let mcp = Mcp::new(Arc::new(RichSource { supporting }), None);
        for kind in [None, Some("docs")] {
            let mut args = json!({"query": "godot set_multiplayer_authority"});
            if let Some(kind) = kind {
                args["kind"] = json!(kind);
            }
            let mut reply = call(&mcp, "search", args);
            let page = &reply["result"]["structuredContent"]["pages"][0];
            assert_eq!(
                page["source_excerpt"]["heading"], "set_multiplayer_authority",
                "{reply}"
            );
            if kind.is_none() && !supporting {
                assert_eq!(
                    reply["result"]["structuredContent"]["ordered_results"][0]["url"],
                    page["url"]
                );
            }
            assert!(reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(
                "Source excerpt (set_multiplayer_authority): Sets the node's multiplayer authority"
            ));
            text_only(&mut reply);
            assert!(reply["result"].get("structuredContent").is_none());
            assert!(reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("#class-node-method-set-multiplayer-authority"));
        }
    }
}

#[test]
fn search_and_site_info_return_plain_entries() {
    let mcp = server(vec![
        hit("python.org", 2.0, 0.8, true),
        hit("pypi.org", 1.0, 0.7, false),
    ]);
    let answer = &call(&mcp, "search", json!({ "query": "python", "limit": 1 }))["result"]
        ["structuredContent"];
    assert_eq!(answer["results"].as_array().unwrap().len(), 1);
    assert_eq!(answer["results"][0]["domain"], "python.org");
    assert_eq!(answer["results"][0]["well_known"], true);
    assert!(answer["results"][0].get("score").is_none());

    let answer = &call(
        &mcp,
        "site_info",
        json!({ "domain": "https://wiki.python.org/moin/" }),
    )["result"]["structuredContent"];
    assert_eq!(answer["domain"], "python.org");
    assert_eq!(answer["found"], true);
    assert_eq!(answer["popularity"], 0.8);
    let answer = &call(&mcp, "site_info", json!({ "domain": "nowhere.example" }))["result"]
        ["structuredContent"];
    assert_eq!(answer["found"], false);
}

#[test]
fn search_lists_what_plugins_found_apart_from_plumbs_results() {
    let found = crate::plugins::PluginResults {
        plugin: "hacker-news".into(),
        name: "Hacker News".into(),
        results: vec![crate::plugins::PluginItem {
            title: "Rust 2.0".into(),
            url: "https://blog.rust-lang.org/x".into(),
            site: "rust-lang.org".into(),
            snippet: Some("120 points".into()),
            ..Default::default()
        }],
    };
    let mcp = server(vec![hit("rust-lang.org", 2.0, 0.8, true)]).with_plugin_results(vec![found]);
    let result = &call(&mcp, "search", json!({ "query": "hn rust" }))["result"];
    let answer = &result["structuredContent"];
    assert_eq!(answer["plugins"][0]["plugin"], "Hacker News");
    assert_eq!(
        answer["plugins"][0]["results"][0]["url"],
        "https://blog.rust-lang.org/x"
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("From the Hacker News plugin on this node (not Plumb's index):\n- Rust 2.0 (rust-lang.org) https://blog.rust-lang.org/x\n  120 points"),
        "{text}"
    );
}

#[test]
fn search_queries_for_plugins_are_cut_like_searches() {
    let message = |query: &str| {
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "search", "arguments": { "query": query } } })
    };
    assert_eq!(
        Mcp::search_query(&message("  rust   lang ")).as_deref(),
        Some("rust lang")
    );
    assert_eq!(Mcp::search_query(&message("   ")), None);
    let long = Mcp::search_query(&message(&"a".repeat(1000))).unwrap();
    assert_eq!(long.chars().count(), MAX_QUERY_CHARS);
}

#[test]
fn search_answers_what_it_can_work_out() {
    let mcp = server(vec![hit("calculator.net", 1.0, 0.5, false)]);
    let reply = call(&mcp, "search", json!({ "query": "12 * 7" }));
    let answer = &reply["result"]["structuredContent"];
    assert_eq!(answer["answer"]["answer"], "84");
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("Answer: 12 × 7 = 84\n1. calculator.net"),
        "{text}"
    );
    // Nothing to work out: no answer.
    let reply = call(&mcp, "search", json!({ "query": "calculator" }));
    assert!(reply["result"]["structuredContent"].get("answer").is_none());
}

#[test]
fn package_cards_say_version_install_and_docs() {
    let mcp = Mcp::new(Arc::new(Packages), None);
    let reply = call(
        &mcp,
        "package",
        json!({ "name": "serde", "registry": "crates" }),
    );
    let result = &reply["result"];
    assert_eq!(result["structuredContent"]["found"], true);
    let card = &result["structuredContent"]["packages"][0];
    assert_eq!(card["version"], "1.0.228");
    assert_eq!(card["install"], "cargo add serde");
    assert_eq!(card["docs"], "https://docs.rs/serde");
    assert_eq!(
        result["content"][0]["text"],
        "[crates.io] serde 1.0.228 (2025-09-27, MIT OR Apache-2.0): A generic \
         serialization/deserialization framework Install: cargo add serde. Docs: \
         https://docs.rs/serde Code: https://github.com/serde-rs/serde Home: https://serde.rs \
         https://crates.io/crates/serde"
    );
    // Search carries the same card.
    let reply = call(&mcp, "search", json!({ "query": "serde crate" }));
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("1. [crates.io] serde 1.0.228"), "{text}");
    // So does the command that installs it.
    let reply = call(
        &mcp,
        "search",
        json!({ "query": "cargo add serde --features derive" }),
    );
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("1. [crates.io] serde 1.0.228"), "{text}");
    // Another registry's package of the name is not there.
    let reply = call(
        &mcp,
        "package",
        json!({ "name": "serde", "registry": "npm" }),
    );
    assert_eq!(reply["result"]["structuredContent"]["found"], false);
    let reply = call(
        &mcp,
        "package",
        json!({ "name": "serde", "registry": "cpan" }),
    );
    assert!(reply["error"].is_object());
}

#[test]
fn findings_are_reported_and_listed_with_the_next_search() {
    let dir = tempfile::tempdir().unwrap();
    let findings = Arc::new(crate::findings::Findings::in_dir(dir.path()).unwrap());
    let mut paypal = hit("paypal.com", 2.0, 0.9, true);
    paypal.official = true;
    let mcp = server(vec![paypal]).with_findings(Some(Arc::clone(&findings)));
    let reply = mcp
        .handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .unwrap();
    let names: Vec<&str> = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"report_finding"));
    let reply = call(
        &mcp,
        "report_finding",
        json!({
            "query": "tokio latest version",
            "url": "https://crates.io/crates/tokio",
            "why": "crates.io lists the newest release",
            "answer": "1.47.1",
            "task": "upgrading a web server",
        }),
    );
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert_eq!(findings.len(), 1);
    let reply = call(
        &mcp,
        "search",
        json!({ "query": "latest version of tokio" }),
    );
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with(
            "Found before (searched \"tokio latest version\", just now): 1.47.1 Source: \
             https://crates.io/crates/tokio (crates.io lists the newest release)"
        ),
        "{text}"
    );
    // A look-alike's page is not kept.
    let reply = call(
        &mcp,
        "report_finding",
        json!({
            "query": "paypal login",
            "url": "https://paypal-login.us/",
            "why": "it has the form",
            "answer": "log in there",
        }),
    );
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert_eq!(findings.len(), 1);
    // Without findings the tool is not there.
    let reply = call(
        &server(Vec::new()),
        "report_finding",
        json!({ "query": "x" }),
    );
    assert!(reply["error"].is_object());
}

#[test]
fn searches_list_few_results_and_fewer_after_a_direct_answer() {
    let many = |named: bool| {
        (0..10)
            .map(|i| {
                hit(
                    &format!("site{i}.com"),
                    10.0 - i as f32,
                    0.5,
                    named && i == 0,
                )
            })
            .collect::<Vec<_>>()
    };
    let count = |mcp: &Mcp, arguments: Value| {
        call(mcp, "search", arguments)["result"]["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(
        count(&server(many(false)), json!({ "query": "rust web" })),
        5
    );
    assert_eq!(count(&server(many(true)), json!({ "query": "site0" })), 3);
    assert_eq!(
        count(&server(many(true)), json!({ "query": "site0", "limit": 8 })),
        8
    );
}

/// Leaves the first `cap` results of a search, counted as its text lists
/// them: the `sites` in order, each after the `pages` placed before it
/// (`position`), and the pages placed after the last site at the end.
/// Pages listed under a site (`about_site`) go with it.
fn cap_results(sites: &mut Vec<Value>, pages: &mut Vec<Value>, cap: usize) {
    let alone = |page: &Value| page.get("about_site").is_none_or(Value::is_null);
    let position = |page: &Value| {
        page.get("position")
            .and_then(Value::as_u64)
            .map_or(usize::MAX, |at| at as usize)
    };
    let mut left = cap;
    let mut kept_sites = 0;
    let mut kept_pages = vec![true; pages.len()];
    for slot in 1..=sites.len() + 1 {
        for (kept, page) in kept_pages.iter_mut().zip(pages.iter()) {
            let here = if slot > sites.len() {
                position(page) >= slot
            } else {
                position(page) == slot
            };
            if alone(page) && here {
                if left == 0 {
                    *kept = false;
                } else {
                    left -= 1;
                }
            }
        }
        if slot <= sites.len() && left > 0 {
            left -= 1;
            kept_sites = slot;
        }
    }
    sites.truncate(kept_sites);
    let domains: Vec<&Value> = sites.iter().filter_map(|site| site.get("domain")).collect();
    let mut kept = kept_pages.into_iter();
    pages.retain(|page| {
        kept.next().unwrap_or(false)
            && (alone(page) || page.get("about_site").is_some_and(|d| domains.contains(&d)))
    });
}

#[test]
fn results_are_capped_as_listed_with_their_pages() {
    let site = |domain: &str| json!({ "domain": domain });
    let page = |title: &str, position: u64, under: Option<&str>| json!({ "title": title, "position": position, "about_site": under });
    let mut sites = vec![site("a.com"), site("b.com"), site("c.com")];
    let mut pages = vec![
        page("first", 1, None),
        page("about a", 1, Some("a.com")),
        page("about c", 3, Some("c.com")),
        page("last", 9, None),
    ];
    cap_results(&mut sites, &mut pages, 3);
    assert_eq!(sites, vec![site("a.com"), site("b.com")]);
    let titles: Vec<&str> = pages.iter().map(|p| p["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["first", "about a"]);
}

#[test]
fn searches_about_a_well_known_package_get_its_card() {
    let mcp = Mcp::new(Arc::new(Packages), None);
    let reply = call(&mcp, "search", json!({ "query": "serde derive" }));
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("Related package information:\n[crates.io] serde 1.0.228"),
        "{text}"
    );
    let reply = call(
        &mcp,
        "search",
        json!({ "query": "serde derive macro attributes now" }),
    );
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains("crates.io"), "{text}");
}

#[test]
fn search_is_in_english_unless_asked() {
    let language = |args: Value| search_language(args.as_object().unwrap());
    assert_eq!(language(json!({})), Ok(Some("en".into())));
    assert_eq!(language(json!({ "language": "any" })), Ok(None));
    assert_eq!(
        language(json!({ "language": "de-DE" })),
        Ok(Some("de".into()))
    );
    assert!(language(json!({ "language": "german!" })).is_err());
}

/// Keeps an article, a question and a repository about tokio, and notes
/// each query it is asked.
#[derive(Default)]
struct Shelves(std::sync::Mutex<Vec<String>>);

fn shelf() -> Vec<Page> {
    use plumb_core::article::Article;
    vec![
        Page::from_article(
            "en",
            Article {
                title: "Tokio (software)".into(),
                ..Default::default()
            },
        ),
        Page::from_question(Article {
            title: "How do I spawn a task in tokio?".into(),
            item: Some("123".into()),
            ..Default::default()
        }),
        Page::from_repo(Article {
            title: "tokio-rs/tokio".into(),
            ..Default::default()
        }),
    ]
}

impl SearchBackend for Shelves {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Hit>> {
        Ok(vec![hit("tokio.rs", 1.0, 0.5, false)])
    }

    fn search_full(
        &self,
        query: &str,
        _limit: usize,
        _options: &SearchOptions,
    ) -> Result<SearchResults> {
        self.0.lock().unwrap().push(query.to_string());
        let ops = plumb_core::Operators::parse(query);
        let pages = shelf()
            .into_iter()
            .map(|page| PlacedPage {
                hit: shelved(page),
                under: None,
                at: 1,
            })
            .collect();
        Ok(SearchResults {
            hits: vec![hit("tokio.rs", 1.0, 0.5, false)],
            pages,
            site_search: ops.sites.first().map(|site| plumb_index::SiteSearch {
                domain: site.clone(),
                terms: ops.site_terms.clone(),
                url: format!("https://{site}/search?q={}", ops.site_terms),
            }),
            spelling: None,
        })
    }

    fn pages_of(
        &self,
        query: &str,
        limit: usize,
        _options: &SearchOptions,
        docs: bool,
        keep: &dyn Fn(&Page) -> bool,
    ) -> Vec<plumb_index::pages::PageHit> {
        self.0.lock().unwrap().push(query.to_string());
        if docs {
            self.0.lock().unwrap().push(format!("docs: {query}"));
        }
        shelf()
            .into_iter()
            .filter(|page| keep(page))
            .take(limit)
            .map(shelved)
            .collect()
    }

    fn num_docs(&self) -> u64 {
        1
    }
}

#[test]
fn search_lists_one_kind_of_result_alone() {
    let shelves = Arc::new(Shelves::default());
    let mcp = Mcp::new(Arc::clone(&shelves) as Arc<dyn SearchBackend>, None);
    let search = |args: Value| call(&mcp, "search", args)["result"].clone();

    let result = search(json!({ "query": "tokio spawn", "kind": "question" }));
    let data = &result["structuredContent"];
    assert_eq!(data["kind"], "question", "{result}");
    assert_eq!(data["results"], json!([]));
    assert_eq!(data["pages"].as_array().unwrap().len(), 1, "{data}");
    assert_eq!(data["pages"][0]["set"], "stackoverflow");
    assert_eq!(
        result["content"][0]["text"],
        "1. [Stack Overflow] How do I spawn a task in tokio? https://stackoverflow.com/questions/123"
    );
    // As a model may write it.
    let data = &search(json!({ "query": "tokio", "kind": "Repos" }))["structuredContent"];
    assert_eq!(data["pages"][0]["url"], "https://github.com/tokio-rs/tokio");
    let result = search(json!({ "query": "tokio", "kind": "paper" }));
    assert_eq!(
        result["content"][0]["text"],
        "No results of kind paper for \"tokio\"."
    );

    // Sites alone: no pages among them.
    let data = &search(json!({ "query": "tokio", "kind": "site" }))["structuredContent"];
    assert_eq!(data["results"][0]["domain"], "tokio.rs", "{data}");
    assert_eq!(data["pages"], json!([]));
    // With neither, as before.
    let data = &search(json!({ "query": "tokio" }))["structuredContent"];
    assert!(!data["pages"].as_array().unwrap().is_empty(), "{data}");
    assert!(data.get("kind").is_none());
}

#[test]
fn paper_mcp_dates_are_validated_and_count_types_remain_explicit() {
    let mcp = Mcp::new(Arc::new(Shelves::default()) as Arc<dyn SearchBackend>, None);
    for args in [
        json!({ "query": "transformer", "kind": "paper", "after": "2026-02-29" }),
        json!({ "query": "transformer", "kind": "docs", "after": "2026" }),
        json!({ "query": "transformer", "after": 2026 }),
    ] {
        let result = call(&mcp, "search", args);
        assert_eq!(result["error"]["code"], INVALID_PARAMS);
    }
    let result = call(
        &mcp,
        "search",
        json!({ "query": "transformer", "kind": "paper", "after": "2026", "order": "newest" }),
    );
    assert_eq!(result["result"]["structuredContent"]["kind"], "paper");
    assert!(result["result"]["structuredContent"]["coverage_message"]
        .as_str()
        .unwrap()
        .contains("indexed corpus"));
    let page = Page::from_paper(plumb_core::article::Article {
        title: "Example".into(),
        item: Some("10.1234/a".into()),
        paper: Some(plumb_core::papers::PaperMetadata {
            count: 4,
            count_kind: plumb_core::papers::PaperCountKind::MethodUses,
            publication_date: Some("2026-01-02".into()),
            ..Default::default()
        }),
        ..Default::default()
    });
    let entry = page_entry(&page, "paper", None, 1);
    assert_eq!(entry["count"], json!({ "value": 4, "kind": "method_uses" }));
    assert_eq!(entry["paper"]["publication_date"], "2026-01-02");
    let rendered = super::text::render(
        "search",
        &json!({ "query": "Example", "pages": [entry], "results": [] }),
    );
    assert!(rendered.contains("4 method uses"), "{rendered}");
    assert!(rendered.contains("Published 2026-01-02"), "{rendered}");
}

#[test]
fn search_keeps_to_the_site_asked_for() {
    let shelves = Arc::new(Shelves::default());
    let mcp = Mcp::new(Arc::clone(&shelves) as Arc<dyn SearchBackend>, None);
    let search = |args: Value| call(&mcp, "search", args);

    let answer = search(json!({ "query": "spawn", "site": "github.com" }));
    assert_eq!(
        answer["result"]["structuredContent"]["query"],
        "spawn site:github.com"
    );
    // A URL names its host; a kind keeps to the site too, with the link
    // into the site's own search for pages Plumb lacks.
    let answer =
        search(json!({ "query": "spawn", "site": "https://docs.rs/tokio", "kind": "docs" }));
    let data = &answer["result"]["structuredContent"];
    assert_eq!(data["query"], "spawn site:docs.rs", "{answer}");
    assert_eq!(data["site_search"]["domain"], "docs.rs");
    assert!(answer["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Search docs.rs itself for \"spawn\""));
    let asked = shelves.0.lock().unwrap().clone();
    assert!(
        asked.contains(&"spawn site:docs.rs".to_string()),
        "{asked:?}"
    );
    // Docs pages are searched for as such.
    assert!(
        asked.contains(&"docs: spawn site:docs.rs".to_string()),
        "{asked:?}"
    );

    for args in [
        json!({ "query": "tokio", "kind": "video" }),
        json!({ "query": "tokio", "site": "docs.rs github.com" }),
        json!({ "query": "tokio", "site": "github" }),
    ] {
        let answer = search(args.clone());
        assert_eq!(answer["error"]["code"], INVALID_PARAMS, "{args}: {answer}");
    }
}

#[test]
fn troubleshooting_keeps_related_package_cards_after_main_results() {
    let mcp = scripted(|query| {
        if query == "kubernetes package" {
            let mut card = package_page(
                "go",
                "k8s.io/kubernetes",
                Some("https://kubernetes.io/"),
                None,
            );
            card.hit.popularity = 0.95;
            results(Vec::new(), vec![card])
        } else {
            results(vec![hit("kubernetes.io", 1.0, 0.8, false)], Vec::new())
        }
    });
    let reply = call(
        &mcp,
        "search",
        json!({ "query": "kubernetes CrashLoopBackOff", "limit": 3 }),
    );
    let answer = &reply["result"]["structuredContent"];
    assert_eq!(answer["results"][0]["domain"], "kubernetes.io");
    assert_eq!(answer["pages"][0]["package"]["name"], "k8s.io/kubernetes");
    assert!(answer["pages"][0]["position"].as_u64().unwrap() > 1);
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("1. kubernetes.io"), "{text}");
    assert!(!crate::assembly::promote_package(
        "kubernetes CrashLoopBackOff"
    ));
    assert!(!crate::assembly::promote_package("serde derive"));
    for query in [
        "cargo add serde",
        "serde package",
        "serde latest version",
        "@types/node npm",
    ] {
        assert!(crate::assembly::promote_package(query), "{query}");
    }
}

#[test]
fn missing_troubleshooting_results_do_not_promote_a_package_as_the_answer() {
    let mcp = scripted(|query| {
        if query == "kubernetes package" {
            let mut card = package_page(
                "go",
                "k8s.io/kubernetes",
                Some("https://kubernetes.io/"),
                None,
            );
            card.hit.popularity = 0.95;
            results(Vec::new(), vec![card])
        } else {
            results(Vec::new(), Vec::new())
        }
    });
    let reply = call(
        &mcp,
        "search",
        json!({ "query": "kubernetes CrashLoopBackOff" }),
    );
    let answer = &reply["result"]["structuredContent"];
    assert!(answer["pages"].as_array().unwrap().is_empty());
    assert_eq!(answer["related_packages"][0]["name"], "k8s.io/kubernetes");
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("No results for"), "{text}");
    assert!(text.contains("Related package information:"), "{text}");
}

#[test]
fn a_package_not_asked_for_leaves_the_other_pages_their_places() {
    let mcp = scripted(|_| {
        let mut package = package_page("npm", "left-pad", None, None);
        package.hit.popularity = 0.9;
        let mut repo = article("x", None, None);
        repo.hit.page = Page::from_repo(plumb_core::article::Article {
            title: "left-pad/left-pad".into(),
            aliases: vec!["left-pad".into()],
            views: 1_000,
            ..Default::default()
        });
        repo.hit.whole = false;
        let mut incident = article("Npm left-pad incident", None, None);
        incident.hit.score = 0.8;
        let site = Hit {
            text_score: 0.2,
            ..hit("die-linke.de", 0.5, 0.6, false)
        };
        results(vec![site], vec![package, repo, incident])
    });
    let reply = call(&mcp, "search", json!({ "query": "left-pad", "limit": 5 }));
    let answer = &reply["result"]["structuredContent"];
    let urls: Vec<&str> = answer["ordered_results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["url"].as_str().unwrap())
        .collect();
    let at = |part: &str| urls.iter().position(|url| url.contains(part));
    // The repository and the article are listed, and npm's page with them.
    let site = at("die-linke.de").unwrap();
    for part in ["github.com", "wikipedia.org", "npmjs.com"] {
        assert!(at(part).is_some_and(|page| page < site), "{part}: {urls:?}");
    }
}

#[test]
fn a_package_named_with_its_language_is_listed_within_the_limit() {
    let mcp = scripted(|_| {
        let mut package = package_page("pypi", "requests", None, None);
        (package.hit.whole, package.hit.popularity) = (false, 0.97);
        let sites = ["matplotlib.org", "python-requests.org", "bottlepy.org"]
            .into_iter()
            .chain(["trypyramid.com", "pythonspot.com", "python-poetry.org"])
            .map(|domain| Hit {
                text_score: 0.4,
                ..hit(domain, 0.6, 0.6, false)
            })
            .collect();
        results(sites, vec![package])
    });
    // With and without a limit: placed after every site, the limit cut it.
    for args in [
        json!({ "query": "requests python", "limit": 5 }),
        json!({ "query": "requests python" }),
    ] {
        let reply = call(&mcp, "search", args.clone());
        let urls = reply["result"]["structuredContent"]["ordered_results"].to_string();
        assert!(urls.contains("pypi.org/project/requests"), "{args}: {urls}");
    }
    assert!(!crate::assembly::promote_package("requests python"));
}
