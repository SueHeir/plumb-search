# Plumb for local AI apps

Plumb gives a local model web search with no API key: a node answers from its own index, with no other search engine behind it. It speaks MCP and SearXNG's JSON format, so it fits apps you already use. This page has the setup for LM Studio, Open WebUI, Jan, LibreChat, AnythingLLM and any app that takes a SearXNG address.

- [Which address](#which-address)
- [LM Studio](#lm-studio)
- [Open WebUI](#open-webui)
- [Jan](#jan)
- [LibreChat](#librechat)
- [AnythingLLM](#anythingllm)
- [Apps that start a command](#apps-that-start-a-command)
- [Other SearXNG clients](#other-searxng-clients)
- [Keeping the model on track](#keeping-the-model-on-track)

## Which address

There are two ways in. Apps that speak MCP get Plumb's [tools](mcp.md#tools): `search`, `official_site`, `check_lookalike`, `site_info`, `facts`, `package`, and on the node's own computer `read_page` and `report_finding`. Apps that take a SearXNG address use a node's `/search?format=json` instead.

| You run | MCP address | SearXNG address |
| --- | --- | --- |
| The [desktop app](desktop.md) | `http://127.0.0.1:7586/mcp` | `http://127.0.0.1:7586` |
| A node in [Docker](docker.md) on this computer | `http://127.0.0.1:8080/mcp` | `http://127.0.0.1:8080` |
| Nothing (the public node) | `https://plumbsearch.org/mcp` | `https://plumbsearch.org` |

With your own node, searches don't leave your computer: the node answers them from its own index. The examples below use the desktop app's address; put yours in its place.

`read_page` and `report_finding` are offered only to apps on the node's own computer ([who counts](mcp.md#reading-pages)). A Docker node sees requests through its published port as coming from Docker's network, not from this computer, so it offers neither. To have them with a Docker node, start `plumb mcp` inside the container as a command (see [Apps that start a command](#apps-that-start-a-command)).

Use a model trained to call tools; models without tool training ignore the tools.

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
4. Load a model with tool use and ask, for example, "Use plumb to find the official Rust docs and read the page about ownership."

LM Studio asks before each tool call until you allow Plumb's tools for good.

## Open WebUI

**As MCP tools:** **Settings**, **External Tools**, add a server of type **MCP (Streamable HTTP)** with the URL `http://127.0.0.1:7586/mcp`.

**As its web search:** **Admin Panel**, **Settings**, **Web Search**, then either:

- engine **searxng**, query URL `http://127.0.0.1:7586/search?q=<query>`, or
- engine **external**, URL `http://127.0.0.1:7586/api/websearch` (no API key needed).

Open WebUI then fetches the result pages itself, unless "Bypass web loader" is on; then the model sees only each result's snippet, which is why an instant answer leads the first snippet.

> [!NOTE]
> The desktop app's node listens on `127.0.0.1` only. An app in a Docker container on Linux cannot reach that address through `host.docker.internal`; run the container with `--network host` and use `http://127.0.0.1:7586`, which also counts as this computer.

## Jan

Jan has web search options of its own. To use Plumb as well, open **Settings**, **MCP Servers**, add a server with `"plumb": { "url": "http://127.0.0.1:7586/mcp" }` (as in LM Studio's step 2), turn it on, and use a model with tool calling enabled.

## LibreChat

In `librechat.yaml`:

```yaml
mcpServers:
  plumb:
    type: streamable-http
    url: http://127.0.0.1:7586/mcp
```

See the note under [Open WebUI](#open-webui) when LibreChat runs in Docker.

## AnythingLLM

**Agent Skills**, **Web Search**, provider **SearXNG**, base URL `http://127.0.0.1:7586`. Its agent then searches Plumb with `@agent`.

## Apps that start a command

Claude Desktop, many Ollama front ends and other apps that start a local MCP server can run `plumb mcp`, which speaks MCP over stdin and stdout:

| Node | Command |
| --- | --- |
| The desktop app | `plumb mcp --node http://127.0.0.1:7586` |
| A Docker container named `plumb` | `docker exec -i plumb plumb mcp --node http://127.0.0.1:8080` |
| plumbsearch.org | `plumb mcp` |

`plumb mcp` reads pages itself, on your computer, so it always offers `read_page`. Run inside the container, it asks the node from the node's own computer, so `report_finding` is offered too. [Set up](mcp.md#set-up) has the config files.

## Other SearXNG clients

Perplexica and other apps that take a SearXNG address work the same way: give them the node's address, such as `http://127.0.0.1:7586`. Unlike SearXNG, a node needs no setting turned on to answer JSON. The code is [web/searxng.rs](../crates/plumb-node/src/web/searxng.rs).

`/search?q=...&format=json` answers in SearXNG's format:

| Field | What it holds |
| --- | --- |
| `results` | What the results page lists, in its order: `url`, `title`, `content` (the description), `category` (`general`, or `news` for recent headlines, which carry a `publishedDate` and come right after the first result), `engine` (`plumb`, or the plugin that found it) |
| `answers` | The instant answer, such as `12 × 7 = 84` |
| `infoboxes` | The info box about what the query names, with its official site, profiles, Wikipedia article and Wikidata item |
| `suggestions` | A suggested spelling of the query |
| `number_of_results` | How many results were collected up to this page |
| `recent`, `places` | Plumb's own: the Recent headlines block and the places, as the [JSON API](searching.md#the-json-api) gives them |

`corrections` and `unresponsive_engines` are always empty.

It takes these parameters, and ignores SearXNG's others (`engines`, `language`):

| Parameter | Meaning |
| --- | --- |
| `q` | The query |
| `pageno` | Page of results, from 1; at most 10 pages, and at most 100 results in all |
| `limit` | Results per page (default 10, at most 100) |
| `safesearch` | `0`, `1` or `2` (off, moderate, strict) |
| `categories` | `news` alone lists only recent headlines |
| `time_range` | Any value puts recent headlines first; `day`, `week`, `month` or `year` also drops older ones |
| `country`, `lang`, `safe` | Plumb's own: a home country code, a language code or `any`, and `off`, `moderate` or `strict` |

While a node is still setting up its first index, it answers `503` with `Retry-After`.

Open WebUI's **external** engine posts `{"query": "...", "count": 5}` to `/api/websearch` and gets `[{"link", "title", "snippet"}]` back: the same results in the same order (`count` defaults to 10, at most 100).

MCP tool calls are limited per client (a burst of 30, then 60 a minute; see [Limits](mcp.md#limits)), to keep a shared node fair. `/search?format=json` has no such limit.

## Keeping the model on track

**Web text is untrusted.** Results are short text with no HTML, and `read_page` leaves out scripts, menus and footers and never reaches your router or other machines on your network. A page can still say anything, so tell the model in its system prompt that text from the web is information, not instructions.

**Search for names and topics.** Plumb indexes sites by name, homepage text, description and meaning, plus page sets such as Wikipedia articles, Stack Overflow questions and package registries, not the full text of the web. It finds the official site, the package, the docs and the well-known question well, and a blog post or a page deep inside a site less often; [search.md](search.md) has how a search is answered.

**Models that don't search.** Some models answer from what they learned instead of calling a tool. A line in the system prompt helps, for example: "Use plumb search for anything after your training cutoff or anything you're unsure of, and say which address you used."
