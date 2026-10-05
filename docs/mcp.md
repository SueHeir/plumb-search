# Plumb for AI assistants (MCP)

AI assistants guess web addresses, and guesses are how they end up on look-alike sites. Plumb answers "what is the real site for X?" over the [Model Context Protocol](https://modelcontextprotocol.io), so Claude and other MCP apps can ask before they open a page, fill in a login or cite a source.

Every node serves it at `/mcp` on its web port: `https://plumbsearch.org/mcp`, `http://127.0.0.1:7586/mcp` for the desktop app, port 8080 for the Docker image. `plumb mcp` serves the same over stdin and stdout for apps that start a local command.

## Tools

All four only read the index.

| Tool | Takes | Returns |
| --- | --- | --- |
| `official_site` | `name` ("PayPal", "rust docs", "chase login") | the domain and URL, a confidence (`high`, `medium`, `low`), the reasons (Wikidata lists it as an official website, the name is the site's own, it is well known, other sites share the name) and other candidates |
| `check_lookalike` | `url` (a URL or a domain) | a verdict (`official`, `known_site`, `little_known`, `lookalike` or `unknown`), the reasons, and the real site a look-alike imitates |
| `search` | `query`, `limit` (1 to 25, default 10) | the normal results: sites best first, plus pages such as Wikipedia articles with where they are placed |
| `site_info` | `domain` | the site's title and description, whether Wikidata lists it as official, how well known it is, its country and pages about it |

Each also takes an optional `country`, a two-letter code (`US`, `DE`) whose sites rank a little higher, or `any` for none. Without it the node's home country setting decides.

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

## How it is served

- `POST /mcp` takes one JSON-RPC message and answers it with one JSON object; a notification gets `202 Accepted`. There is no event stream (`GET /mcp` answers 405) and no session.
- No token: the tools read nothing that the search page doesn't show.
- A request from a web page of another site (an `Origin` other than the node's own host) is refused.
- Tool calls are limited to a burst of 30 per client, then 60 a minute, answered with `429` and `Retry-After` beyond that. Behind a reverse proxy on the same computer (plumbsearch.org's Caddy), the client is the proxy's `X-Forwarded-For`.
- Like the search page, the node logs no queries.

## Checking it

The end-to-end tests run every tool over the fixture index, through `plumb mcp --index` and through `/mcp` (`cargo test -p plumb-node --test end_to_end mcp`). `official_site` returns the first result of an ordinary search for the name, so `plumb eval --queries eval/ai_queries.tsv` measures it on real data.
