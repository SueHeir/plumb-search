use super::*;
use crate::mcp::read::find_from;
use crate::mcp::stdio::{offer_read_page, read_here};

#[test]
fn find_jumps_to_the_first_match_ignoring_case() {
    let chars: Vec<char> = "Café au lait, CAFÉ noir".chars().collect();
    assert_eq!(find_from(&chars, "café", 0), Some(0));
    assert_eq!(find_from(&chars, "café", 1), Some(14));
    assert_eq!(find_from(&chars, " Noir ", 0), Some(19));
    assert_eq!(find_from(&chars, "tea", 0), None);
    assert_eq!(find_from(&chars, "café", 100), None);
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
    // search sends agents on to read_page only where they have it.
    let search_says = |read_pages| {
        tools(read_pages, false, false)[2]["description"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(!search_says(false).contains("read_page"));
    assert!(search_says(true).contains("then read a page with read_page."));
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

    // An outline: each heading, where it starts and its opening words.
    let long = serve_page(
        &runtime,
        "<title>L</title><main><p>Lead.</p><h2>Usage</h2><p>Run it.</p><p>Twice.</p>\
         <h3>Flags</h3><p>None.</p></main>",
    );
    let reply = call(&with, "read_page", json!({ "url": long, "outline": true }));
    let answer = &reply["result"]["structuredContent"];
    assert!(answer.get("text").is_none(), "{answer}");
    let sections = answer["outline"].as_array().unwrap();
    let headings: Vec<_> = sections.iter().map(|s| s["heading"].clone()).collect();
    assert_eq!(headings, ["", "Usage", "Flags"]);
    assert_eq!(sections[1]["level"], 2);
    assert_eq!(sections[1]["opening"], "Run it. Twice.");
    let start = sections[2]["start"].as_u64().unwrap();
    let reply = call(&with, "read_page", json!({ "url": long, "start": start }));
    assert_eq!(
        reply["result"]["structuredContent"]["text"],
        "### Flags\n\nNone."
    );
    // Without headings, the text instead.
    let reply = call(
        &with,
        "read_page",
        json!({ "url": serve_page(&runtime, "<p>Plain.</p>"), "outline": true }),
    );
    assert_eq!(reply["result"]["structuredContent"]["text"], "Plain.");

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
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": tools(false, false, false) } });
    offer_read_page(&json!({ "method": "tools/list" }), &mut listed);
    assert_eq!(listed["result"]["tools"][6]["name"], "read_page");
    let search = listed["result"]["tools"][2]["description"]
        .as_str()
        .unwrap();
    assert!(
        search.contains("then read a page with read_page."),
        "{search}"
    );
}
