# Plumb for local AI apps

A model running on your own computer can search with Plumb and read the pages it finds, without anything going to Google. With the [desktop app](desktop.md) running, the searches don't leave your computer either: its node answers them from its own index.

There are two ways in. Apps that speak MCP get Plumb's [tools](mcp.md): `search`, `official_site`, `check_lookalike`, `site_info`, `package`, `read_page` and `report_finding`. Apps that take a SearXNG address for web search can use a node's `/search?format=json` instead.

Use a model that can call tools: Qwen3 (8B or bigger), gpt-oss, Llama 3.1 or 3.3, Mistral Small. Small models without tool training ignore the tools.

The addresses below are the desktop app's. For a node in Docker on the same computer use `http://127.0.0.1:8080`, and for any other node its address. Those offer every tool but `read_page` and `report_finding`: Docker passes connections on from its own network, so the node cannot tell they come from this computer. To have those tools with a Docker node, start `docker exec -i plumb plumb mcp --node http://127.0.0.1:8080` as a command (see [below](#claude-desktop-ollama-front-ends-and-anything-that-starts-a-command)); `read_page` alone also comes with `--mcp-read-pages` (see [Reading pages](mcp.md#reading-pages)).

## LM Studio

1. In the right sidebar, open the **Program** tab, click **Install**, then **Edit mcp.json** (it is `~/.lmstudio/mcp.json`).
2. Add Plumb and save:

   ```json
   {
     "mcpServers": {
       "plumb": { "url": "http://127.0.0.1:7586/mcp" }
     }
   }
   ```

3. Turn on **mcp/plumb** in the Program tab.
4. Load a model with the hammer icon (tool use) and ask, for example, "Use plumb to find the official Rust docs and read the page about ownership."

LM Studio asks before each tool call until you allow Plumb's tools for good.

## Open WebUI

As MCP tools: **Settings**, **External Tools**, add a server of type **MCP (Streamable HTTP)** with the URL `http://127.0.0.1:7586/mcp`. Open WebUI in Docker reaches the host as `http://host.docker.internal:7586/mcp`; requests from there do not come from `127.0.0.1`, so start the node with `--mcp-read-pages` if you want `read_page`, and only on a computer the internet cannot reach.

As its web search: **Admin Panel**, **Settings**, **Web Search**, engine **searxng**, query URL `http://127.0.0.1:7586/search?q=<query>`. Or engine **external**, URL `http://127.0.0.1:7586/api/websearch` (no API key needed). Open WebUI then fetches the result pages itself, unless "Bypass web loader" is on; then the model sees only each result's snippet, which is why an instant answer leads the first snippet.

## Jan

**Settings**, **MCP Servers**, add a server with the JSON from LM Studio's step 2 (`"plumb": { "url": "http://127.0.0.1:7586/mcp" }`), turn it on, and use a model with tool calling enabled.

## LibreChat

In `librechat.yaml`:

```yaml
mcpServers:
  plumb:
    type: streamable-http
    url: http://127.0.0.1:7586/mcp
```

LibreChat in Docker reaches the host as `http://host.docker.internal:7586/mcp` (see the note on `read_page` under Open WebUI).

## AnythingLLM

**Agent Skills**, **Web Search**, provider **SearXNG**, base URL `http://127.0.0.1:7586`. Its agent then searches Plumb with `@agent`.

## Other SearXNG clients

Perplexica (now Vane) and others that take a SearXNG address work the same way: give them the node's address, such as `http://127.0.0.1:7586`. A node answers `/search?q=...&format=json` in SearXNG's format:

- `results`: what the results page lists, in its order, each with `url`, `title` and `content` (the description); Wikipedia articles, Stack Overflow questions and other pages sit where the page puts them, and recent headlines come after the best result, with `category` `news`;
- `answers`: the instant answer, such as `12 × 7 = 84`;
- `infoboxes`: the info box about what the query names, with its official site and profiles;
- `suggestions`: a suggested spelling ("Did you mean ..."). Results are always for the query as typed.

It takes SearXNG's `pageno`, `safesearch` (0, 1 or 2), `categories` (`news` alone lists only recent headlines, with `publishedDate`) and `time_range` (any value puts recent headlines first), and Plumb's own `limit`, `country`, `safe` and `lang`. Other SearXNG parameters are ignored. Unlike SearXNG, JSON needs no setting turned on.

## Claude Desktop, Ollama front ends and anything that starts a command

`plumb mcp` speaks MCP over stdin and stdout. `plumb mcp --node http://127.0.0.1:7586` asks the desktop app's node; with no `--node` it asks plumbsearch.org. Either way it reads pages itself, on your computer. For a node in Docker, run it inside the container: `docker exec -i plumb plumb mcp --node http://127.0.0.1:8080`. See [Set up](mcp.md#set-up).
