//! AI apps over `/mcp`: pages of one kind, and findings shared or kept.

use super::*;

/// Calls `tool` on a node's `/mcp` as an AI app on this computer does; the
/// tool's structured answer and its text.
async fn mcp_call(
    addr: SocketAddr,
    tool: &str,
    arguments: serde_json::Value,
) -> (serde_json::Value, String) {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    })
    .to_string();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}\n{body}");
    let reply: serde_json::Value = serde_json::from_str(body).unwrap();
    let result = &reply["result"];
    assert_eq!(result["isError"], false, "{reply}");
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    (result["structuredContent"].clone(), text)
}

/// Writes `articles` as the node's whole file of the page set `set`.
fn write_set_file(dir: &Path, set: &str, articles: &[plumb_core::Article]) {
    let set = crate::pages::SetInfo::find(set).unwrap();
    let file = set.file(dir);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    write_articles_file(&file, articles).unwrap();
    let notes = crate::pages::SetFileNotes {
        lines: articles.len() as u64,
        complete: true,
        source_modified: 1,
        fetched_at: now_unix(),
        near: 0,
    };
    std::fs::write(
        crate::pages::notes_path(&file),
        serde_json::to_vec(&notes).unwrap(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_asks_for_one_kind_of_page() {
    let dir = seeded_dir();
    write_set_file(
        dir.path(),
        "wikipedia-en",
        &[plumb_core::Article {
            title: "Lighthouse".into(),
            views: 900,
            ..Default::default()
        }],
    );
    write_set_file(
        dir.path(),
        "stackoverflow",
        &[plumb_core::Article {
            title: "How do I clean a lighthouse lens?".into(),
            item: Some("42".into()),
            views: 10,
            ..Default::default()
        }],
    );
    let mut config = test_config(dir.path());
    config.settings.page_sets =
        crate::pages::PageSets::parse("wikipedia-en=all,stackoverflow=all").unwrap();
    let node = start(config).await.unwrap();
    let addr = node.addr();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !get(addr, "/search?q=lighthouse")
        .await
        .2
        .contains("wiki/Lighthouse")
    {
        assert!(Instant::now() < deadline, "no pages");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let (answer, text) = mcp_call(
        addr,
        "search",
        serde_json::json!({ "query": "lighthouse", "kind": "question" }),
    )
    .await;
    assert_eq!(answer["results"], serde_json::json!([]), "{answer}");
    let pages = answer["pages"].as_array().unwrap();
    assert_eq!(pages.len(), 1, "{answer}");
    assert_eq!(pages[0]["url"], "https://stackoverflow.com/questions/42");
    assert!(
        text.contains("[Stack Overflow] How do I clean a lighthouse lens?"),
        "{text}"
    );
    let (answer, _) = mcp_call(
        addr,
        "search",
        serde_json::json!({ "query": "lighthouse", "kind": "article" }),
    )
    .await;
    let pages = answer["pages"].as_array().unwrap();
    assert_eq!(pages.len(), 1, "{answer}");
    assert_eq!(pages[0]["title"], "Lighthouse");

    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_finds_a_page_another_node_s_agent_shared_and_reads_it() {
    // The page both agents read, served here under a public name.
    let app = axum::Router::new().route(
        "/guide",
        axum::routing::get(|| async {
            axum::response::Html(
                "<title>Lighthouse keeping</title><main><h1>Lamps</h1>\
                 <p>Fresnel lamps burn colza oil, trimmed every four hours.</p></main>",
            )
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let page_addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let page = format!("http://lighthouse-keepers.org:{}/guide", page_addr.port());
    let config_for = |dir: &Path, bootstrap: Vec<plumb_net::Multiaddr>, trusted| {
        let mut config = test_config(dir);
        let mut net = plumb_net::NetConfig::new(PathBuf::new());
        net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
        net.upnp = false;
        net.local_discovery = false;
        net.round_every = None;
        net.bootstrap = bootstrap;
        net.trusted_peers = trusted;
        net.search_scope = plumb_net::SearchScope::Trusted;
        config.network = Some(net);
        config
            .page_reader
            .resolve
            .push(("lighthouse-keepers.org".to_string(), page_addr));
        config
    };

    // Node A lets its agents share findings.
    let a_dir = seeded_dir();
    let mut a_config = config_for(a_dir.path(), Vec::new(), Vec::new());
    a_config.share_findings = true;
    let a = start(a_config).await.unwrap();
    let a_status = wait_for(a.addr(), "A's first index", ready_and_idle).await;
    let a_net = a_status.network.unwrap();
    let a_id: plumb_net::PeerId = a_net.peer_id.parse().unwrap();
    let a_addr: plumb_net::Multiaddr = a_net.listening[0].parse().unwrap();
    let a_addr = a_addr.with_p2p(a_id).unwrap();

    // Node B trusts A, and its searches take only trusted nodes' leads.
    let b_dir = seeded_dir();
    let b = start(config_for(b_dir.path(), vec![a_addr], vec![a_id]))
        .await
        .unwrap();
    wait_for(b.addr(), "B to connect to A", |s| {
        ready_and_idle(s) && s.network.as_ref().is_some_and(|n| n.connected_peers >= 1)
    })
    .await;

    // A's agent keeps one finding to itself, then shares another.
    let (kept, _) = mcp_call(
        a.addr(),
        "report_finding",
        serde_json::json!({
            "query": "lighthouse foghorn schedule",
            "url": "https://foghorn-schedules.org/harbour",
            "why": "the harbour's own timetable",
            "answer": "Every 30 seconds in fog",
        }),
    )
    .await;
    assert_eq!(kept["kept"], true, "{kept}");
    assert!(kept.get("shared").is_none(), "{kept}");
    let (shared, text) = mcp_call(
        a.addr(),
        "report_finding",
        serde_json::json!({
            "query": "lighthouse lamp oil",
            "url": page,
            "why": "the keepers' guide says what each lamp burns",
            "answer": "Colza oil, as the 1890 guide on Grandpa's shelf says",
            "task": "restoring Grandpa's lighthouse lamp",
            "share": true,
        }),
    )
    .await;
    assert_eq!(shared["shared"]["shared"], true, "{shared}");
    assert_eq!(shared["shared"]["node"], a_id.to_string());
    assert!(shared["shared"]["query"].is_null(), "{shared}");
    assert!(text.contains("Shared with other Plumb nodes"), "{text}");

    // B's agent searches for the same thing in other words and finds the
    // page, as a lead: who reported it, when, and that nobody checked it.
    let mut found = None;
    for _ in 0..300 {
        let (answer, text) = mcp_call(
            b.addr(),
            "search",
            serde_json::json!({ "query": "oil for a lighthouse lamp" }),
        )
        .await;
        if answer.get("leads").is_some() {
            found = Some((answer, text));
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (answer, text) = found.expect("B's search lists A's lead");
    let lead = &answer["leads"][0];
    assert_eq!(lead["url"], page.as_str(), "{answer}");
    assert_eq!(lead["verified"], false);
    assert_eq!(lead["why"], "the keepers' guide says what each lamp burns");
    let by = &lead["reported_by"][0];
    assert_eq!(by["node"], a_id.to_string());
    assert_eq!(by["relation"], "trusted");
    assert!(by["query"].is_null(), "{answer}");
    assert!(by["expires_at"].as_u64().unwrap() > now_unix());
    assert!(
        text.contains("Lead shared by 1 other Plumb node, 1 trusted"),
        "{text}"
    );
    // Not its search, answer or task: only what was meant to be shared.
    // "1890" alone is in some timestamps.
    let said = answer.to_string();
    for private in ["1890 guide", "Grandpa", "restoring", "lamp oil\""] {
        assert!(!said.contains(private), "{private} in {said}");
    }
    // A lead is not among the results, which come from crawls.
    assert!(!answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|hit| hit["url"] == page.as_str()));

    // B's agent reads the page itself.
    let (read, _) = mcp_call(b.addr(), "read_page", serde_json::json!({ "url": page })).await;
    assert!(
        read["text"]
            .as_str()
            .unwrap()
            .contains("Fresnel lamps burn colza oil"),
        "{read}"
    );

    // The finding A's agent kept to itself never left A.
    let (answer, _) = mcp_call(
        b.addr(),
        "search",
        serde_json::json!({ "query": "lighthouse foghorn schedule" }),
    )
    .await;
    assert!(answer.get("leads").is_none(), "{answer}");
    assert!(answer.get("found_before").is_none(), "{answer}");
    let b_leads = std::fs::read_to_string(b_dir.path().join("net/leads.jsonl")).unwrap();
    assert_eq!(b_leads.lines().count(), 1, "{b_leads}");
    assert!(!b_leads.contains("foghorn"), "{b_leads}");
    let a_leads = std::fs::read_to_string(a_dir.path().join("net/leads.jsonl")).unwrap();
    assert!(!a_leads.contains("foghorn"), "{a_leads}");
    let a_findings = std::fs::read_to_string(a_dir.path().join("findings.jsonl")).unwrap();
    assert!(a_findings.contains("foghorn"), "{a_findings}");
    assert!(!b_dir.path().join("findings.jsonl").exists());
    // A finds its own finding, not its lead.
    let (answer, _) = mcp_call(
        a.addr(),
        "search",
        serde_json::json!({ "query": "lighthouse lamp oil" }),
    )
    .await;
    assert!(answer.get("found_before").is_some(), "{answer}");
    assert!(answer.get("leads").is_none(), "{answer}");
    let b_status = get_status(b.addr()).await;
    assert_eq!(b_status.network.unwrap().leads_held, 1);

    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_does_not_share_findings_keeps_them_local() {
    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.trusted_peers = Vec::new();
    config.network = Some(net);
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "the first index", ready_and_idle).await;
    // Asked to share, it keeps the finding and says why it did not share it.
    let (answer, text) = mcp_call(
        node.addr(),
        "report_finding",
        serde_json::json!({
            "query": "tokio latest version",
            "url": "https://crates.io/crates/tokio",
            "why": "the registry's page",
            "answer": "1.47.1",
            "share": true,
        }),
    )
    .await;
    assert_eq!(answer["kept"], true);
    assert_eq!(answer["shared"]["shared"], false, "{answer}");
    assert!(
        text.contains("Not shared: this node does not share findings"),
        "{text}"
    );
    let status = get_status(node.addr()).await;
    assert_eq!(status.network.unwrap().leads_held, 0);
    assert!(!dir.path().join("net/leads.jsonl").exists());
    node.shutdown().await.unwrap();
}
