use super::*;

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

#[test]
fn initialize_agrees_on_a_protocol_version() {
    let mcp = server(Vec::new());
    let answer = |version: &str| {
        mcp.handle(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": version, "capabilities": {} },
        }))
        .unwrap()
    };
    let reply = answer("2025-03-26");
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(reply["result"]["serverInfo"]["name"], "plumb-search");
    assert!(reply["result"]["capabilities"]["tools"].is_object());
    // An unknown version gets the newest.
    assert_eq!(
        answer("2099-01-01")["result"]["protocolVersion"],
        PROTOCOL_VERSIONS[0]
    );
}

#[test]
fn notifications_get_no_answer_and_unknown_methods_an_error() {
    let mcp = server(Vec::new());
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    assert_eq!(mcp.handle(&note), None);
    let reply = mcp
        .handle(&json!({ "jsonrpc": "2.0", "id": "a", "method": "sampling/createMessage" }))
        .unwrap();
    assert_eq!(reply["id"], "a");
    assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND);
    let reply = mcp.handle(&json!([1, 2])).unwrap();
    assert_eq!(reply["error"]["code"], INVALID_REQUEST);
    let reply = mcp
        .handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }))
        .unwrap();
    assert_eq!(reply["result"], json!({}));
}

#[test]
fn lists_five_read_only_tools_with_schemas() {
    let reply = server(Vec::new())
        .handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
        .unwrap();
    let tools = reply["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "official_site",
            "check_lookalike",
            "search",
            "package",
            "site_info"
        ]
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");
        assert!(tool["description"].as_str().unwrap().len() > 40, "{tool}");
    }
}

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
    assert_eq!(answer["confidence"], "medium");
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

#[test]
fn bad_arguments_are_protocol_errors_and_failed_searches_tool_errors() {
    let mcp = server(Vec::new());
    for (tool, arguments) in [
        ("official_site", json!({})),
        ("official_site", json!({ "name": "  " })),
        ("search", json!({ "query": "x", "limit": 0 })),
        ("search", json!({ "query": "x", "country": "Narnia" })),
        ("teleport", json!({})),
    ] {
        let reply = call(&mcp, tool, arguments.clone());
        assert_eq!(reply["error"]["code"], INVALID_PARAMS, "{tool} {arguments}");
    }
    let mcp = Mcp::new(Arc::new(Broken), None);
    let reply = call(&mcp, "search", json!({ "query": "x" }));
    assert_eq!(reply["result"]["isError"], true);
    let reply = call(&mcp, "check_lookalike", json!({ "url": "mailto:a@b.com" }));
    assert_eq!(reply["result"]["isError"], true);
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
fn name_queries_read_the_brand_out_of_a_host() {
    assert_eq!(
        name_queries("usbank-login-help.com", "usbank-login-help.com"),
        ["usbank"]
    );
    assert_eq!(
        name_queries("paypal.com.secure-check.io", "secure-check.io"),
        ["secure check", "paypal", "check"]
    );
    assert_eq!(
        name_queries(
            "www.microsoft-support-helpline.com",
            "microsoft-support-helpline.com"
        ),
        ["microsoft support helpline", "microsoft", "helpline"]
    );
}

#[test]
fn resembles_spelled_out_names_and_typos() {
    let check = |host: &str, real: &str| {
        let domain = registrable_domain(host).unwrap();
        resembles(&squash(host), &squash(&domain_label(&domain)), real)
    };
    assert!(check("paypal-login.us", "paypal.com"));
    assert!(check("paypa1.com", "paypal.com"));
    assert!(check("twiter.com", "twitter.com"));
    assert!(check("rnicrosoft.com", "microsoft.com"));
    assert!(check("wellsfargo.com.account-check.io", "wellsfargo.com"));
    assert!(check("amazno.com", "amazon.com"));
    assert!(!check("deltafaucet.com", "united.com"));
    assert!(!check("bing.com", "ebay.com"));
    // Two-letter names match too much to count.
    assert!(!check("aardvark.com", "aa.com"));
    assert_eq!(edit_distance("kitten", "sitting"), 3);
    assert_eq!(edit_distance("ab", "ba"), 1);
}

#[test]
fn stdio_answers_line_by_line_and_skips_notifications() {
    let mcp = server(vec![hit("python.org", 2.0, 0.8, true)]);
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n",
        "\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        "not json\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
    );
    let mut output = Vec::new();
    serve_lines(input.as_bytes(), &mut output, |m| Ok(mcp.handle(m))).unwrap();
    let lines: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["id"], 1);
    assert_eq!(lines[1]["error"]["code"], PARSE_ERROR);
    assert_eq!(lines[2]["id"], 2);
}

#[test]
fn node_addresses_become_their_mcp_endpoint() {
    for (node, endpoint) in [
        ("https://plumbsearch.org", "https://plumbsearch.org/mcp"),
        ("https://plumbsearch.org/", "https://plumbsearch.org/mcp"),
        ("http://127.0.0.1:7586/mcp", "http://127.0.0.1:7586/mcp"),
        ("http://homelab.lan/plumb/", "http://homelab.lan/plumb/mcp"),
    ] {
        assert_eq!(mcp_endpoint(node).unwrap().as_str(), endpoint);
    }
    assert!(mcp_endpoint("ftp://plumbsearch.org").is_err());
    assert!(mcp_endpoint("plumbsearch.org").is_err());
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

fn local_reader() -> (tokio::runtime::Runtime, Reader) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let pages = PageReader::new(ReadConfig {
        allow_private_addresses: true,
        ..ReadConfig::default()
    })
    .unwrap();
    let reader = Reader::new(pages, runtime.handle().clone());
    (runtime, reader)
}

/// Serves `html` at `/` on this computer; returns the address.
fn serve_page(runtime: &tokio::runtime::Runtime, html: &'static str) -> String {
    runtime.block_on(async move {
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || async move { axum::response::Html(html) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{addr}/")
    })
}

#[test]
fn read_page_is_offered_only_with_a_reader() {
    let list = |mcp: &Mcp| {
        let reply = mcp
            .handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
            .unwrap();
        reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    let without = server(Vec::new());
    assert!(!list(&without).contains(&"read_page".to_string()));
    let reply = without.handle(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "read_page", "arguments": { "url": "https://example.com/" } },
    }));
    assert_eq!(reply.unwrap()["error"]["code"], INVALID_PARAMS);

    let (runtime, reader) = local_reader();
    let url = serve_page(
        &runtime,
        "<title>T</title><main><h1>Hi</h1><p>One two three.</p></main>",
    );
    let with = server(Vec::new()).with_reader(Some(reader));
    assert!(list(&with).contains(&"read_page".to_string()));
    let reply = call(&with, "read_page", json!({ "url": url }));
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let answer = &reply["result"]["structuredContent"];
    assert_eq!(answer["title"], "T");
    assert_eq!(answer["text"], "# Hi\n\nOne two three.");
    assert_eq!(answer["more"], false);
    // An IP address has no site to check.
    assert!(answer.get("site").is_none());

    // In parts.
    let reply = call(
        &with,
        "read_page",
        json!({ "url": url, "max_chars": 200, "start": 6 }),
    );
    let answer = &reply["result"]["structuredContent"];
    assert_eq!(answer["text"], "One two three.");
    assert_eq!(answer["start"], 6);

    // Jumping to words, from the start of their line.
    let reply = call(&with, "read_page", json!({ "url": url, "find": "THREE" }));
    let answer = &reply["result"]["structuredContent"];
    assert_eq!(answer["start"], 6);
    assert_eq!(answer["found"], true);
    let reply = call(&with, "read_page", json!({ "url": url, "find": "four" }));
    assert_eq!(reply["result"]["structuredContent"]["found"], false);

    // A bot check is not the page.
    let check = serve_page(
        &runtime,
        "<title>Just a moment...</title><p>Checking your browser before accessing.</p>",
    );
    let reply = call(&with, "read_page", json!({ "url": check }));
    assert_eq!(reply["result"]["isError"], true, "{reply}");
}

#[test]
fn plumb_mcp_node_reads_pages_itself() {
    let (runtime, reader) = local_reader();
    let url = serve_page(&runtime, "<p>Hello</p>");
    let message = json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": { "name": "read_page", "arguments": { "url": url } },
    });
    let answer = read_here(&reader, &message, |check| {
        assert_eq!(check["params"]["name"], "check_lookalike");
        Ok(Some(json!({ "result": { "structuredContent": {
            "verdict": "lookalike", "imitates": { "domain": "paypal.com" },
        } } })))
    })
    .unwrap();
    assert_eq!(answer["id"], 4);
    let text = answer["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("WARNING: this site is a look-alike of paypal.com"),
        "{text}"
    );
    assert!(text.contains("\nHello"), "{text}");
    // Other calls go to the node.
    let search = json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "search" } });
    assert!(read_here(&reader, &search, |_| unreachable!()).is_none());

    // The node's list gains read_page.
    let mut listed =
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [{ "name": "search" }] } });
    offer_read_page(&json!({ "method": "tools/list" }), &mut listed);
    assert_eq!(listed["result"]["tools"][1]["name"], "read_page");
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
fn official_site_falls_back_on_a_packages_home_page() {
    let mcp = Mcp::new(Arc::new(Packages), None);
    let reply = call(&mcp, "official_site", json!({ "name": "serde" }));
    let answer = &reply["result"]["structuredContent"];
    assert_eq!(answer["found"], false);
    assert_eq!(answer["package_home"], "https://serde.rs");
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("gives https://serde.rs as its home page"),
        "{text}"
    );
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
    assert!(text.starts_with("1. [crates.io] serde 1.0.228"), "{text}");
    let reply = call(
        &mcp,
        "search",
        json!({ "query": "serde derive macro attributes now" }),
    );
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(!text.contains("crates.io"), "{text}");
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
