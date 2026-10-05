# Plumb for AI assistants (MCP)

AI assistants guess web addresses, and guesses are how they end up on look-alike sites. Plumb answers "what is the real site for X?" over the [Model Context Protocol](https://modelcontextprotocol.io), so Claude and other MCP apps can ask before they open a page, fill in a login or cite a source.

Every node serves it at `/mcp` on its web port: `https://plumbsearch.org/mcp`, `http://127.0.0.1:7586/mcp` for the desktop app, port 8080 for the Docker image. `plumb mcp` serves the same over stdin and stdout for apps that start a local command.

Local AI apps (LM Studio, Open WebUI, Ollama front ends) can use it too, and with the desktop app every search stays on your computer. [Local AI apps](local-llms.md) has the setup for each, including Open WebUI's SearXNG setting.

## Tools

All but `read_page` only read the index.

| Tool | Takes | Returns |
| --- | --- | --- |
| `official_site` | `name` ("PayPal", "rust docs", "chase login") | the domain and URL, a confidence (`high`, `medium`, `low`), the reasons (Wikidata lists it as an official website, the name is the site's own, it is well known, other sites share the name) and other candidates |
| `check_lookalike` | `url` (a URL or a domain) | a verdict (`official`, `known_site`, `little_known`, `lookalike` or `unknown`), the reasons, and the real site a look-alike imitates |
| `search` | `query`, `limit` (1 to 25, default 10) | the normal results: sites best first, plus pages such as Wikipedia articles and Stack Overflow questions with where they are placed. With them, what the results page shows: the instant answer (`12 * 7`, `10 km in miles`, `100 usd to eur`, `time in tokyo`), the info box about what the query names, an official profile asked for (`mrbeast youtube`), a package's card when the query asks for one (`serde crate`, `requests python`, `latest version of lodash`) and recent headlines. Takes the search page's operators (`site:github.com`, `"exact words"`, `-word`) |
| `package` | `name` ("serde", "@types/node"), `registry` (optional: `npm`, `pypi`, `crates`, `go`, `gem`, `composer`, `nuget`, `maven`) | the package's card: latest version and its date, license, install command, and where its docs, code and homepage are, from the `packages` page set (see [pages.md](pages.md#software-packages)). One line, so an agent need not open the registry's page |
| `site_info` | `domain` | the site's title and description, whether Wikidata lists it as official, how well known it is, its country and pages about it |
| `report_finding` | `query`, `url`, `why`, `answer`, `task` (optional) | keeps what an agent found: what it searched for, the page that answered it, why that page helped, and the answer. The next `search` for the same words (in any order, or most of them) lists it first as `found_before`, so no agent has to work it out again. Only offered to apps on the node's own computer (see [Findings](#findings)) |
| `read_page` | `url`, `start` (default 0), `max_chars` (200 to 30000, default 6000), `find` (words to jump to), `links` (default false) | the page's text, with headings, lists and tables marked in Markdown and menus, footers and scripts left out; where the next part starts on a long page; the page's links if asked; and `check_lookalike`'s verdict on where the page ended up. Only offered to apps on the node's own computer (see below) |

Each also takes an optional `country`, a two-letter code (`US`, `DE`) whose sites rank a little higher, or `any` for none. Without it the node's home country setting decides.

Answers come as short plain text, one line per result with its address, which is what the model reads; small local models have little room, and the same answer as JSON takes three to four times as many tokens. The JSON is in `structuredContent` for programs.

`check_lookalike` reads the names out of the address (`paypal-login.us` spells "paypal login", `wellsfargo.com.account-check.io` contains "wellsfargo") and looks them up. When they lead to a well-known or official site other than this one, and the address spells that site's name out or is a typo of it (`paypa1.com`, `twiter.com`), it is a look-alike. A site that is well known or official in its own right is never called one. `unknown` only means Plumb does not have the site: that proves nothing either way.

## Set up

### Claude Code

```sh
claude mcp add --transport http plumb https://plumbsearch.org/mcp
```

To ask your own node instead, give its address: `http://127.0.0.1:7586/mcp` for the desktop app, or `http://<server>:8080/mcp` for a Docker node. With the `plumb` command installed you can also use stdio: `claude mcp add plumb -- plumb mcp`.

### Claude Desktop

With the `plumb` command installed, add it to `claude_desktop_config.json` (Settings, Developer, Edit Config) and restart Claude:

```json
{
  "mcpServers": {
    "plumb": {
      "command": "plumb",
      "args": ["mcp"]
    }
  }
}
```

`plumb mcp` passes each request on to plumbsearch.org. Add `"--node", "http://127.0.0.1:7586"` to `args` to ask the desktop app's node, or `"--index", "/path/to/index"` to answer from an index on disk without any network. For a node running in Docker on the same computer:

```json
{
  "mcpServers": {
    "plumb": {
      "command": "docker",
      "args": ["exec", "-i", "plumb", "plumb", "mcp", "--node", "http://127.0.0.1:8080"]
    }
  }
}
```

Plans that offer custom connectors can instead add `https://plumbsearch.org/mcp` under Settings, Connectors, Add custom connector. Connectors are reached from the internet, so give a public node's address there, not `127.0.0.1`.

### Other apps

Any MCP client that speaks Streamable HTTP can use a node's `/mcp` address, and any that starts local servers can run `plumb mcp`. Plain HTTP works too:

```sh
curl -s https://plumbsearch.org/mcp -H 'content-type: application/json' -d '{
  "jsonrpc": "2.0", "id": 1, "method": "tools/call",
  "params": { "name": "check_lookalike", "arguments": { "url": "paypal-login.us" } }
}'
```

## Reading pages

`read_page` fetches the page from wherever it runs, keeps nothing, and adds nothing to the index: Plumb still crawls homepages only. Like the crawler it stays off private networks, so a page cannot send it to your router or a cloud metadata address, and it reads web pages and plain text only, not PDFs or images. A bot check (Cloudflare's "Just a moment..." and the like) is reported as an error rather than returned as the page.

A node offers it only to AI apps on its own computer: a request from a loopback address (`127.0.0.1`, `::1`) that no proxy passed on. plumbsearch.org and other public nodes never offer it, since anyone could make them fetch pages. `plumb mcp` offers it whichever node it asks, because it fetches pages itself, on your computer. For a home server whose AI apps run on other computers, start the node with `--mcp-read-pages` to offer it to every client; never do that on a node the internet can reach.

## Findings

Agents search for the same things again and again: the latest version of a library, the flag that fixes an error, the page with the API they need. `report_finding` lets an agent leave what it found for the next one. A finding is the search, the page, why the page helped and the answer itself; `search` lists up to three that match the query before the results:

```text
Found before (searched "tokio latest version", 2 days ago): 1.47.1 Source: https://crates.io/crates/tokio (crates.io lists the newest release)
```

Searches say a lot about whoever makes them, so findings never leave the node. They are kept in `findings.jsonl` in its data folder (the newest 5,000), and only apps on the node's own computer can report them or see them, even on a node run with `--mcp-read-pages`. A page that `check_lookalike` calls a look-alike is not kept. `plumb serve` keeps no findings. Delete the file to forget them all.

## How it is served

- `POST /mcp` takes one JSON-RPC message and answers it with one JSON object; a notification gets `202 Accepted`. There is no event stream (`GET /mcp` answers 405) and no session.
- No token: the tools read nothing that the search page doesn't show, and `read_page` is only offered as described above.
- A request from a web page of another site (an `Origin` other than the node's own host) is refused.
- Tool calls are limited to a burst of 30 per client, then 60 a minute, answered with `429` and `Retry-After` beyond that. Behind a reverse proxy on the same computer (plumbsearch.org's Caddy), the client is the proxy's `X-Forwarded-For`.
- Like the search page, the node logs no queries.

## Checking it

The end-to-end tests run every tool over the fixture index, through `plumb mcp --index` and through `/mcp` (`cargo test -p plumb-node --test end_to_end mcp`). `official_site` returns the first result of an ordinary search for the name, so `plumb eval --queries eval/ai_queries.tsv` measures it on real data.
