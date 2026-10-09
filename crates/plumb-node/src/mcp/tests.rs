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
        missing_words: false,
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
fn lists_six_read_only_tools_with_schemas() {
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
            "site_info",
            "facts"
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
fn responses_get_no_answer_and_junk_with_an_id_an_error() {
    let mcp = server(vec![hit("python.org", 2.0, 0.8, true)]);
    assert_eq!(
        mcp.handle(&json!({ "jsonrpc": "2.0", "id": 1, "result": {} })),
        None
    );
    assert_eq!(
        mcp.handle(&json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": 1, "message": "x" } })),
        None
    );
    let junk = mcp.handle(&json!({ "jsonrpc": "2.0", "id": 1 })).unwrap();
    assert_eq!(junk["error"]["code"], INVALID_REQUEST);
}

#[test]
fn long_urls_are_kept_whole_and_numbers_read_as_models_send_them() {
    let long = format!("https://example.com/a?{}", "x".repeat(300));
    let args = json!({ "url": format!("  {long} "), "start": 5.0, "max_chars": "1000" });
    let args = args.as_object().unwrap();
    assert_eq!(url_arg(args, "url").unwrap(), long);
    let read = ReadArgs::of(args).unwrap();
    assert_eq!(read.url, long);
    assert_eq!(read.start, 5);
    assert_eq!(read.max_chars, 1000);
    let too_long = json!({ "url": "x".repeat(MAX_URL_CHARS + 1) });
    assert!(url_arg(too_long.as_object().unwrap(), "url").is_err());
    assert!(as_whole(&json!(2.5)).is_none());
    assert!(as_whole(&json!(-1)).is_none());
}

#[test]
fn find_jumps_to_the_first_match_ignoring_case() {
    let chars: Vec<char> = "Café au lait, CAFÉ noir".chars().collect();
    assert_eq!(find_from(&chars, "café", 0), Some(0));
    assert_eq!(find_from(&chars, "café", 1), Some(14));
    assert_eq!(find_from(&chars, " Noir ", 0), Some(19));
    assert_eq!(find_from(&chars, "tea", 0), None);
    assert_eq!(find_from(&chars, "café", 100), None);
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

/// Finds Wikipedia's article on Australia, with its facts, for any query
/// naming it.
struct Australia;

impl SearchBackend for Australia {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Hit>> {
        Ok(Vec::new())
    }

    fn search_full(
        &self,
        query: &str,
        _limit: usize,
        _options: &SearchOptions,
    ) -> Result<SearchResults> {
        use plumb_core::facts::{Fact, FactKind};
        let mut pages = Vec::new();
        if query.to_lowercase().contains("australia") {
            let page = plumb_index::pages::Page::from_article(
                "en",
                plumb_core::article::Article {
                    title: "Australia".into(),
                    description: Some("country in Oceania".into()),
                    item: Some("Q408".into()),
                    facts: vec![
                        Fact {
                            kind: FactKind::Capital,
                            value: "Canberra".into(),
                        },
                        Fact {
                            kind: FactKind::Population,
                            value: "27204809;2024".into(),
                        },
                    ],
                    ..Default::default()
                },
            );
            pages.push(plumb_index::pages::PlacedPage {
                hit: plumb_index::pages::PageHit {
                    page,
                    score: 0.9,
                    named: true,
                    popularity: 1.0,
                    whole: true,
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
fn facts_come_with_the_wikidata_item_and_property_they_are_from() {
    let mcp = Mcp::new(Arc::new(Australia), None);
    let answer = call(&mcp, "facts", json!({ "subject": "Australia" }));
    let result = &answer["result"];
    assert_eq!(result["isError"], false, "{answer}");
    let data = &result["structuredContent"];
    assert_eq!(data["item"], "Q408");
    assert_eq!(data["facts"][0]["question"], "Capital of Australia");
    assert_eq!(data["facts"][0]["value"], "Canberra");
    assert_eq!(
        data["facts"][0]["source"],
        "https://www.wikidata.org/wiki/Q408#P36"
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        text,
        "Australia, country in Oceania https://en.wikipedia.org/wiki/Australia\n\
         Capital of Australia: Canberra [Wikidata Q408 P36]\n\
         Population of Australia: 27,204,809 (Counted in 2024) [Wikidata Q408 P1082]\n\
         Source: https://www.wikidata.org/wiki/Q408"
    );

    // One kind, by its key or as people ask it.
    for about in ["capital", "capital city"] {
        let answer = call(
            &mcp,
            "facts",
            json!({ "subject": "australia", "about": about }),
        );
        let facts = answer["result"]["structuredContent"]["facts"]
            .as_array()
            .unwrap();
        assert_eq!(facts.len(), 1, "{about}: {answer}");
        assert_eq!(facts[0]["kind"], "capital");
    }
    // A kind it has no fact of, a kind it never keeps, a subject it lacks.
    let answer = call(
        &mcp,
        "facts",
        json!({ "subject": "australia", "about": "ceo" }),
    );
    assert_eq!(answer["result"]["structuredContent"]["found"], false);
    let answer = call(
        &mcp,
        "facts",
        json!({ "subject": "australia", "about": "favourite colour" }),
    );
    assert_eq!(answer["result"]["isError"], true);
    let text = answer["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("capital, population"), "{text}");
    let answer = call(&mcp, "facts", json!({ "subject": "Atlantis" }));
    assert_eq!(
        answer["result"]["content"][0]["text"],
        "Plumb has no facts about Atlantis."
    );
}

#[test]
fn relate_is_answered_here_and_offered_with_the_tools() {
    let dir = tempfile::tempdir().unwrap();
    crate::relations::tests::write_store(dir.path());
    let store = crate::relations::RelationStore::load(dir.path(), None).unwrap();
    let call = |args: Value| {
        relate_here(
            &store,
            &json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                     "params": { "name": "relate", "arguments": args } }),
        )
        .unwrap()
    };
    let answer = call(json!({ "subject": "Fiji", "relation": "Capital", "limit": 1 }));
    let text = answer["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("capital of Fiji, likeliest first:\nSuva ("),
        "{text}"
    );
    assert!(text.ends_with("a learned guess) [Wikidata K5]"), "{text}");
    let answer = call(json!({ "subject": "France", "relation": "capital", "object": "Paris" }));
    let text = answer["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("capital of France: Paris has probability"),
        "{text}"
    );
    assert!(text.ends_with("(stated in Wikidata)."), "{text}");
    let answer = call(json!({ "subject": "France", "relation": "spouse" }));
    assert_eq!(answer["result"]["isError"], true);
    // Other tools are not answered here.
    assert!(relate_here(
        &store,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                 "params": { "name": "search", "arguments": { "query": "x" } } }),
    )
    .is_none());

    let mut listed = json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": [] } });
    offer_relate(
        &store,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        &mut listed,
    );
    assert_eq!(listed["result"]["tools"][0]["name"], "relate");
    let description = listed["result"]["tools"][0]["description"]
        .as_str()
        .unwrap();
    assert!(
        description.ends_with("Relations: capital."),
        "{description}"
    );
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
    assert_eq!(answer["domain"], "gollo.com");
    assert_eq!(answer["confidence"], "low", "{answer}");
    assert!(answer["why"].to_string().contains("Golly (program)"));
}

#[test]
fn official_site_prefers_a_site_whose_title_is_the_name() {
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
    assert_eq!(answer["domain"], "chessprogramming.org", "{answer}");
    assert_eq!(answer["confidence"], "medium");
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
fn official_site_trusts_no_unrelated_official_site() {
    // github.com is official, but not Pillow's; Pillow's package says
    // where its docs are.
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
    assert_eq!(answer["url"], "https://pillow.readthedocs.io", "{answer}");
    assert_eq!(answer["confidence"], "medium");
}

#[test]
fn official_site_finds_the_site_of_an_abbreviation_in_the_name() {
    let mcp = scripted(|query| match query {
        "NPS" => results(
            vec![titled("nps.gov", "National Park Service", 0.7, true)],
            Vec::new(),
        ),
        _ => results(vec![titled("npmjs.org", "npm", 0.8, false)], Vec::new()),
    });
    let answer = official(&mcp, "NPS API developer");
    assert_eq!(answer["domain"], "nps.gov", "{answer}");
    assert_eq!(answer["confidence"], "medium");
    assert_eq!(answer["alternatives"][0]["domain"], "npmjs.org");
}

#[test]
fn site_info_says_what_it_folds_and_whose_official_site_it_is() {
    let mcp = scripted(|_| {
        let mut placed = article("National Park Service", Some("nps.gov"), None);
        placed.under = Some("nps.gov".into());
        results(
            vec![titled("nps.gov", "NPS.gov Homepage", 0.69, true)],
            vec![placed],
        )
    });
    let reply = call(
        &mcp,
        "site_info",
        json!({ "domain": "https://developer.nps.gov/api/" }),
    );
    let answer = &reply["result"]["structuredContent"];
    assert_eq!(answer["domain"], "nps.gov");
    assert_eq!(answer["host"], "developer.nps.gov");
    assert_eq!(answer["part_of"], "nps.gov");
    assert_eq!(answer["official"], true);
    assert_eq!(answer["official_for"], "National Park Service");
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("Plumb keeps no entry of its own for developer.nps.gov"),
        "{text}"
    );
    assert!(
        text.contains("official website of National Park Service"),
        "{text}"
    );

    // www. is the site itself.
    let answer =
        &call(&mcp, "site_info", json!({ "domain": "www.nps.gov" }))["result"]["structuredContent"];
    assert_eq!(answer.get("host"), None);
}

#[test]
fn names_lose_what_is_wanted_of_their_sites() {
    assert_eq!(bare_name("Pillow docs").as_deref(), Some("pillow"));
    assert_eq!(bare_name("NPS API developer").as_deref(), Some("nps"));
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
