use super::*;
use crate::mcp::stdio::{mcp_endpoint, serve_lines};
use crate::mcp::tools::MAX_URL_CHARS;

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
    assert_eq!(
        reply["result"]["_meta"]["plumb.build"],
        serde_json::to_value(crate::build_info::current()).unwrap()
    );
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
    // What a v0.2.0 node takes is still all that is required.
    let search = &tools[2]["inputSchema"];
    assert_eq!(search["required"], json!(["query"]));
    assert_eq!(search["properties"]["kind"]["enum"][0], "site");
    assert_eq!(search["properties"]["site"]["type"], "string");
    let facts = &tools[5]["inputSchema"];
    assert_eq!(facts["required"], json!(["subject"]));
    assert_eq!(facts["properties"]["more_subjects"]["type"], "array");
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
    serve_lines(input.as_bytes(), &mut output, false, |m| Ok(mcp.handle(m))).unwrap();
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
fn text_answers_leave_out_the_json_and_nothing_else() {
    let mcp = server(vec![hit("python.org", 2.0, 0.8, true)]);
    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",",
        "\"params\":{\"name\":\"site_info\",\"arguments\":{\"domain\":\"python.org\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
    );
    let answers = |text_answers| {
        let mut output = Vec::new();
        serve_lines(input.as_bytes(), &mut output, text_answers, |m| {
            Ok(mcp.handle(m))
        })
        .unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect::<Vec<Value>>()
    };
    let (full, text) = (answers(false), answers(true));
    assert_eq!(
        full[0]["result"]["structuredContent"]["domain"],
        "python.org"
    );
    assert!(
        text[0]["result"].get("structuredContent").is_none(),
        "{}",
        text[0]
    );
    // The model reads the same text either way.
    assert_eq!(text[0]["result"]["content"], full[0]["result"]["content"]);
    assert!(text[0]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("python.org"));
    assert_eq!(text[0]["result"]["isError"], false);
    // The tools are listed as before.
    assert_eq!(text[1], full[1]);
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
