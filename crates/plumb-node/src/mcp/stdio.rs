//! `plumb mcp`: the server over stdin and stdout, for AI apps on this
//! computer, answering from a local index or passing each message on to a
//! node's `/mcp`.

use super::*;

/// `plumb mcp`: the server over stdin and stdout, one JSON message per
/// line, until stdin closes. Logs go to stderr, as always.
pub fn run(args: McpArgs) -> Result<()> {
    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    let runtime = crate::runtime()?;
    // Pages are fetched from this computer, whichever node answers the rest.
    let reader = Reader::standard(runtime.handle().clone())?;
    match &args.index {
        Some(index) => {
            let searcher = Searcher::open(index)
                .with_context(|| format!("opening the index in {}", index.display()))?;
            let backend: Arc<dyn SearchBackend> =
                Arc::new(IndexBackend::new(searcher, rank_config(None)));
            let rates = answers::RatesCache::default();
            serve_lines(stdin, stdout, args.text_answers, |message| {
                let rates = Mcp::search_query(message)
                    .and_then(|query| runtime.block_on(rates.for_query(&query)));
                let mcp = Mcp::new(Arc::clone(&backend), args.country.clone())
                    .with_reader(Some(reader.clone()))
                    .with_rates(rates);
                Ok(mcp.handle(message))
            })
        }
        None => {
            let endpoint = mcp_endpoint(&args.node)?;
            let client = reqwest::Client::builder()
                .user_agent(concat!("plumb-mcp/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .context("making the HTTP client")?;
            serve_lines(stdin, stdout, args.text_answers, |message| {
                if let Some(answer) = read_here(&reader, message, |check| {
                    runtime.block_on(forward(&client, &endpoint, check))
                }) {
                    return Ok(Some(answer));
                }
                let mut answer = runtime.block_on(forward(&client, &endpoint, message))?;
                if let Some(answer) = &mut answer {
                    offer_read_page(message, answer);
                }
                Ok(answer)
            })
        }
    }
}

/// For `plumb mcp --node`: answers a `read_page` call here, asking the node
/// (through `ask`) only whether the page's site is a look-alike. `None`
/// for any other message.
pub(super) fn read_here(
    reader: &Reader,
    message: &Value,
    ask: impl FnOnce(&Value) -> Result<Option<Value>>,
) -> Option<Value> {
    let params = message.get("params")?;
    if message.get("method")?.as_str()? != "tools/call"
        || params.get("name")?.as_str()? != "read_page"
    {
        return None;
    }
    let id = message.get("id")?.clone();
    let empty = Map::new();
    let args = params
        .get("arguments")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let read = match ReadArgs::of(args) {
        Ok(read) => read,
        Err((code, why)) => return Some(error(id, code, &why)),
    };
    let answer = reader.read(&read).map(|mut answer| {
        // Only the site goes to the node, which may be a public one: the
        // path and query of a page someone reads can hold anything.
        let site = answer["url"]
            .as_str()
            .and_then(|url| url::Url::parse(url).ok())
            .and_then(|url| Some(format!("{}://{}/", url.scheme(), url.host_str()?)));
        if let Some(url) = site.as_deref() {
            let check = json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "tools/call",
                "params": { "name": "check_lookalike", "arguments": { "url": url } },
            });
            if let Ok(Some(reply)) = ask(&check) {
                let check = &reply["result"]["structuredContent"];
                if check.is_object() {
                    add_site_check(&mut answer, check);
                }
            }
        }
        answer
    });
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": tool_result("read_page", answer) }))
}

/// For `plumb mcp --node`: adds `read_page`, answered here, to what the node
/// says it offers.
pub(super) fn offer_read_page(message: &Value, answer: &mut Value) {
    let method = message.get("method").and_then(Value::as_str);
    let Some(result) = answer.get_mut("result").and_then(Value::as_object_mut) else {
        return;
    };
    match method {
        Some("tools/list") => {
            if let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) {
                if !tools.iter().any(|tool| tool["name"] == "read_page") {
                    tools.push(read_page_tool());
                }
                point_search_at_read_page(tools);
            }
        }
        Some("initialize") => {
            if let Some(Value::String(instructions)) = result.get_mut("instructions") {
                if !instructions.contains("read_page") {
                    instructions.push_str(READ_INSTRUCTIONS);
                }
            }
        }
        _ => {}
    }
}

/// The `/mcp` address of a node's web address.
pub(super) fn mcp_endpoint(node: &str) -> Result<url::Url> {
    let mut url = url::Url::parse(node).with_context(|| format!("reading the address {node:?}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("--node must be an http:// or https:// address, not {node:?}");
    }
    if !url.path().ends_with("/mcp") {
        let path = format!("{}/mcp", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }
    Ok(url)
}

/// Passes one message on to a node's `/mcp` and returns its answer.
async fn forward(
    client: &reqwest::Client,
    endpoint: &url::Url,
    message: &Value,
) -> Result<Option<Value>> {
    let response = client
        .post(endpoint.clone())
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(serde_json::to_vec(message).unwrap_or_default())
        .send()
        .await;
    let id = message.get("id").cloned();
    let failed = |why: String| Ok(id.clone().map(|id| error(id, -32000, &why)));
    let response = match response {
        Ok(response) => response,
        Err(err) => return failed(format!("cannot reach {endpoint}: {err}")),
    };
    let status = response.status();
    if status == reqwest::StatusCode::ACCEPTED {
        return Ok(None);
    }
    let body = response.text().await.unwrap_or_default();
    match serde_json::from_str::<Value>(&body) {
        // A JSON-RPC answer to this message; anything else (a plain
        // `{"error": ...}` from a refusal) would leave the client waiting.
        Ok(answer) if answer.get("jsonrpc").is_some() && answer.get("id") == id.as_ref() => {
            Ok(Some(answer))
        }
        Ok(answer) => {
            let why = answer.get("error").and_then(Value::as_str).map_or_else(
                || format!("{endpoint} answered {status}"),
                |why| format!("{endpoint}: {why}"),
            );
            failed(why)
        }
        Err(_) if status.is_success() => failed(format!("{endpoint} did not answer with JSON")),
        Err(_) => failed(format!("{endpoint} answered {status}")),
    }
}

/// Reads one JSON-RPC message per line from `input` and writes each answer
/// as one line to `output`, without the JSON copy of tool answers if
/// `text_answers` ([`text_only`]).
pub(super) fn serve_lines(
    input: impl BufRead,
    mut output: impl Write,
    text_answers: bool,
    mut answer: impl FnMut(&Value) -> Result<Option<Value>>,
) -> Result<()> {
    for line in input.lines() {
        let line = line.context("reading stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => answer(&message)?,
            Err(_) => Some(parse_error()),
        };
        if let Some(mut reply) = reply {
            if text_answers {
                text_only(&mut reply);
            }
            serde_json::to_writer(&mut output, &reply).context("writing stdout")?;
            output.write_all(b"\n").context("writing stdout")?;
            output.flush().context("writing stdout")?;
        }
    }
    Ok(())
}
