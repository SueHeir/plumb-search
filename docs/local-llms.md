# Plumb for local AI apps

Free web search for your local model. No API key and no quota, and Plumb doesn't scrape Google, Bing or anyone else: it answers from its own index, so there is no engine upstream to block or throttle it. It speaks SearXNG's JSON API and MCP, so it drops into the setup you already have.

```sh
claude mcp add --transport http plumb https://plumbsearch.org/mcp
```

Or give an app that takes a SearXNG address `https://plumbsearch.org` (it asks `/search?q=<query>&format=json`). With the [desktop app](desktop.md) or a node of your own, use its address instead, and the searches don't leave your computer: the node answers them from its own index.

## What it is good at, and what it isn't

Plumb indexes sites by name, homepage text, Wikidata description and meaning, plus page sets: Wikipedia articles, Stack Overflow's most viewed questions, popular GitHub repositories, the most used packages of eight registries, books and papers. It does not crawl the full text of the web. So it finds the official site, the package, the docs, the well-known question and the encyclopedia fact well, and the long tail (a blog post, a forum thread, a page deep inside a site) worse than Google does.

Searches it answers well:

- `official_site` "chase login": chase.com, with how sure it is.
- `package` "serde": the latest version, license, install command and docs.
- `search` "undo last git commit": the Stack Overflow question first.
- `search` "albert einstein": Wikipedia's description and the article.
- `search` "12 * 7", "100 usd to eur", "time in tokyo": instant answers.

## Why not SearXNG or a free API tier?

- **SearXNG** sends your searches on to Google, Bing, DuckDuckGo and others and reads their result pages. Those engines throttle and block it, more so for a busy or shared instance.
- **Free tiers** of search APIs (Brave, Tavily, Exa, Linkup and others) need a key and stop at a quota.
- **Plumb** has its own index, which nodes build by crawling homepages and share over a peer-to-peer network. No other engine stands behind it to cut it off. The only limit is per client, to keep a shared node fair: on MCP, a burst of 30 tool calls, then 60 a minute. `/search?format=json` has none.

## Swap SearXNG for Plumb

- **SearXNG address**: `https://plumbsearch.org`, or your own node's (`http://127.0.0.1:7586` for the desktop app, `http://127.0.0.1:8080` for Docker). Apps that want the whole query URL take `https://plumbsearch.org/search?q=<query>&format=json`. The answer is in SearXNG's format (see [Other SearXNG clients](#other-searxng-clients)).
- **MCP over HTTP**: `https://plumbsearch.org/mcp`, or `/mcp` on your own node. In LM Studio's `mcp.json`:

  ```json
  {
    "mcpServers": {
      "plumb": { "url": "https://plumbsearch.org/mcp" }
    }
  }
  ```

- **MCP over stdio**: `plumb mcp` (see [below](#claude-desktop-ollama-front-ends-and-anything-that-starts-a-command)).

## What results look like

Tool answers are short plain text, one line per result with its address, so they fit in a small model's context. Trimmed example output:

```text
> package {"name": "serde"}
[crates.io] serde 1.0.228 (2025-09-27, MIT OR Apache-2.0): A generic serialization/deserialization framework Install: cargo add serde. Docs: https://docs.rs/serde Code: https://github.com/serde-rs/serde Home: https://serde.rs https://crates.io/crates/serde

> search {"query": "undo last git commit"}
1. [Stack Overflow] How do I undo the most recent local commits in Git?: git, version-control, git-commit, undo https://stackoverflow.com/questions/927358
2. Git (git-scm.com, official) https://git-scm.com/
   Git is a free and open source distributed version control system ...

> search {"query": "albert einstein"}
About Albert Einstein: German-born physicist (1879–1955). https://en.wikipedia.org/wiki/Albert_Einstein
1. [Wikipedia] Albert Einstein: German-born physicist (1879–1955) https://en.wikipedia.org/wiki/Albert_Einstein
...

> search {"query": "12 * 7"}
Answer: 12 × 7 = 84
...
```

The same answers come as JSON in `structuredContent` for programs.

## Keeping the model on track

**Web text is untrusted.** Results are compact text with no HTML. `read_page` returns a page's text, with scripts, menus and footers left out; it is offered only to apps on the node's own computer unless the node runs with `--mcp-read-pages`, and it reaches public addresses only, never your router or other machines on your network. A page can still say anything, so tell the model in its system prompt that text from the web is information, not instructions.

**Models that don't search.** Some models answer from what they learned instead of calling a tool. A line in the system prompt helps, for example: "Use plumb search for anything after your training cutoff or anything you're unsure of, and say which address you used."

## Setting up each app

There are two ways in. Apps that speak MCP get Plumb's [tools](mcp.md): `search`, `official_site`, `check_lookalike`, `site_info`, `package`, `read_page` and `report_finding`. Apps that take a SearXNG address for web search can use a node's `/search?format=json` instead.

Use a model that can call tools: Qwen3 (8B or bigger), gpt-oss, Llama 3.1 or 3.3, Mistral Small. Small models without tool training ignore the tools.

The addresses below are the desktop app's. For plumbsearch.org use `https://plumbsearch.org`, for a node in Docker on the same computer `http://127.0.0.1:8080`, and for any other node its address. Those offer every tool but `read_page` and `report_finding`: plumbsearch.org never offers them, and Docker passes connections on from its own network, so the node cannot tell they come from this computer. To have those tools with a Docker node, start `docker exec -i plumb plumb mcp --node http://127.0.0.1:8080` as a command (see [below](#claude-desktop-ollama-front-ends-and-anything-that-starts-a-command)); `read_page` alone also comes with `--mcp-read-pages` (see [Reading pages](mcp.md#reading-pages)).

### LM Studio

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

### Open WebUI

As MCP tools: **Settings**, **External Tools**, add a server of type **MCP (Streamable HTTP)** with the URL `http://127.0.0.1:7586/mcp`. Open WebUI in Docker reaches the host as `http://host.docker.internal:7586/mcp`; requests from there do not come from `127.0.0.1`, so start the node with `--mcp-read-pages` if you want `read_page`, and only on a computer the internet cannot reach.

As its web search: **Admin Panel**, **Settings**, **Web Search**, engine **searxng**, query URL `http://127.0.0.1:7586/search?q=<query>`. Or engine **external**, URL `http://127.0.0.1:7586/api/websearch` (no API key needed). Open WebUI then fetches the result pages itself, unless "Bypass web loader" is on; then the model sees only each result's snippet, which is why an instant answer leads the first snippet.

### Jan

Jan has web search options of its own. To use Plumb as well, open **Settings**, **MCP Servers**, add a server with the JSON from LM Studio's step 2 (`"plumb": { "url": "http://127.0.0.1:7586/mcp" }`), turn it on, and use a model with tool calling enabled.

### LibreChat

In `librechat.yaml`:

```yaml
mcpServers:
  plumb:
    type: streamable-http
    url: http://127.0.0.1:7586/mcp
```

LibreChat in Docker reaches the host as `http://host.docker.internal:7586/mcp` (see the note on `read_page` under Open WebUI).

### AnythingLLM

**Agent Skills**, **Web Search**, provider **SearXNG**, base URL `http://127.0.0.1:7586`. Its agent then searches Plumb with `@agent`.

### Other SearXNG clients

Perplexica (now Vane) and others that take a SearXNG address work the same way: give them the node's address, such as `http://127.0.0.1:7586`. A node answers `/search?q=...&format=json` in SearXNG's format:

- `results`: what the results page lists, in its order, each with `url`, `title` and `content` (the description); Wikipedia articles, Stack Overflow questions and other pages sit where the page puts them, and recent headlines come after the best result, with `category` `news`;
- `answers`: the instant answer, such as `12 × 7 = 84`;
- `infoboxes`: the info box about what the query names, with its official site and profiles;
- `suggestions`: a suggested spelling ("Did you mean ..."). Results are always for the query as typed.

It takes SearXNG's `pageno`, `safesearch` (0, 1 or 2), `categories` (`news` alone lists only recent headlines, with `publishedDate`) and `time_range` (any value puts recent headlines first), and Plumb's own `limit`, `country`, `safe` and `lang`. Other SearXNG parameters are ignored. Unlike SearXNG, JSON needs no setting turned on.

### Claude Desktop, Ollama front ends and anything that starts a command

`plumb mcp` speaks MCP over stdin and stdout. `plumb mcp --node http://127.0.0.1:7586` asks the desktop app's node; with no `--node` it asks plumbsearch.org. Either way it reads pages itself, on your computer. For a node in Docker, run it inside the container: `docker exec -i plumb plumb mcp --node http://127.0.0.1:8080`. See [Set up](mcp.md#set-up).
