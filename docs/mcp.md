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
| `search` | `query`, `limit` (1 to 25; default 5, or 3 when the search has a direct answer such as a package card, an instant answer, an answer found before or a site it names) | the normal results: sites best first, plus pages such as Wikipedia articles and Stack Overflow questions with where they are placed. With them, what the results page shows: the instant answer (`12 * 7`, `10 km in miles`, `100 usd to eur`, `time in tokyo`), the info box about what the query names, an official profile asked for (`mrbeast youtube`), a package's card when the query asks for one (`serde crate`, `requests python`, `latest version of lodash`) or is about a well-known package's docs or functions (`lodash debounce`, `numpy release notes`) and recent headlines. Takes the search page's operators (`site:github.com`, `"exact words"`, `-word`) |
| `package` | `name` ("serde", "@types/node"), `registry` (optional: `npm`, `pypi`, `crates`, `go`, `gem`, `composer`, `nuget`, `maven`) | the package's card: latest version and its date, license, install command, and where its docs, code and homepage are, from the `packages` page set (see [pages.md](pages.md#software-packages)). One line, so an agent need not open the registry's page |
| `site_info` | `domain` | the site's title and description, whether Wikidata lists it as official, how well known it is, its country and pages about it |
| `facts` | `subject` ("Australia", "Marie Curie", "Nvidia"), `about` (optional: one kind, such as `ceo` or `population`) | what Wikidata says about it: capital, population, height, area, born, died, founded, founder, CEO, headquarters, currency, author, director, composer, creator, owner, birthplace, spouse, head of state and head of government. One line per fact, each with the Wikidata item and property it is from (`Capital of Australia: Canberra [Wikidata Q408 P36]`), so a model can cite it rather than answer from memory |
| `report_finding` | `query`, `url`, `why`, `answer`, `task` (optional), `share` and `share_query` (optional, on a node run with `--share-findings`) | keeps what an agent found: what it searched for, the page that answered it, why that page helped, and the answer. The next `search` for the same words (in any order, or most of them) lists it first as `found_before`, so no agent has to work it out again. Only offered to apps on the node's own computer (see [Findings](#findings)) |
| `read_page` | `url`, `start` (default 0), `max_chars` (200 to 30000, default 6000), `find` (words to jump to), `links` (default false), `outline` (default false) | with `outline`, the page's headings instead of its text, each with where it starts and its first 100 characters, so an agent reads only the section it needs (a page without headings returns its text). Otherwise the page's text, with headings, lists and tables marked in Markdown and menus, footers and scripts left out; where the next part starts on a long page; the page's links if asked; and `check_lookalike`'s verdict on where the page ended up. Only offered to apps on the node's own computer (see below) |

Each also takes an optional `country`, a two-letter code (`US`, `DE`) whose sites rank a little higher, or `any` for none. Without it the node's home country setting decides.

Answers come twice: as short plain text, one line per result with its address, and as JSON in `structuredContent` for programs. Which one the model reads is up to the app, and Claude Code, for one, gives it the JSON. The text is about half the size (on 176 answers from agents' test projects, 101,000 characters against 192,000), and small local models have little room, so to have the model read the text alone, add `--text-answers` to `plumb mcp` or `?answers=text` to a node's address (`https://plumbsearch.org/mcp?answers=text`). Without them, answers carry both, as before.

`check_lookalike` reads the names out of the address (`paypal-login.us` spells "paypal login", `wellsfargo.com.account-check.io` contains "wellsfargo") and looks them up. When they lead to a well-known or official site other than this one, and the address spells that site's name out or is a typo of it (`paypa1.com`, `twiter.com`), it is a look-alike. A site that is well known or official in its own right is never called one. `unknown` only means Plumb does not have the site: that proves nothing either way.

## Set up

### Claude Code

Pick the line for what you have on this computer. Apart from plumbsearch.org, each one offers `read_page` (see [Reading pages](#reading-pages)), so Claude can read a page from the results without a fetch tool of its own.

| You have | Add Plumb with |
| --- | --- |
| The desktop app | `claude mcp add --transport http plumb http://127.0.0.1:7586/mcp` |
| The `plumb` command, built from source | `claude mcp add plumb -- plumb mcp` |
| A node in Docker | `claude mcp add plumb -- docker exec -i plumb plumb mcp --node http://127.0.0.1:8080` |
| None of them | `claude mcp add --transport http plumb https://plumbsearch.org/mcp` |

`plumb mcp` asks plumbsearch.org unless you give it `--node`, and reads pages itself, on your computer. Claude Code gives the model each answer's JSON; to have it read the shorter text instead, add `--text-answers` after `plumb mcp`, or `?answers=text` to the address (quote it in the shell: `"https://plumbsearch.org/mcp?answers=text"`). A node in Docker on another computer is `http://<server>:8080/mcp`, without `read_page`.

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

A node offers it only to AI apps on its own computer: a request from this computer, sent to a local name (`localhost`, `127.0.0.1` or `[::1]`), with no forwarding headers (`Forwarded`, `X-Forwarded-For` or `X-Real-IP`). The same goes for `report_finding` and findings. A reverse proxy in front of a node must add one of those headers, as Caddy does, or every request it passes on looks local; [Behind a reverse proxy or tunnel](docker.md#behind-a-reverse-proxy-or-tunnel) has the nginx lines. plumbsearch.org and other public nodes never offer it, since anyone could make them fetch pages. `plumb mcp` offers it whichever node it asks, because it fetches pages itself, on your computer. Where `read_page` is not offered, `search` tells agents to open the page they need instead of naming a tool they lack. For a home server whose AI apps run on other computers, start the node with `--mcp-read-pages` to offer it to every client. Clients on other computers then read pages on ports 80 and 443 only, at most 10 at once and 20 a minute each, and no more than 8 pages are read for them at a time in all. Those limits keep the node from being used to probe other servers or to load the web through it, but it still fetches whatever anyone asks from its own address, so think twice before using the flag on a node the internet can reach.

## Findings

Agents search for the same things again and again: the latest version of a library, the flag that fixes an error, the page with the API they need. `report_finding` lets an agent leave what it found for the next one. A finding is the search, the page, why the page helped and the answer itself; `search` lists up to three that match the query before the results:

```text
Found before (searched "tokio latest version", 2 days ago): 1.47.1 Source: https://crates.io/crates/tokio (crates.io lists the newest release)
```

Searches say a lot about whoever makes them, so findings never leave the node unless an agent shares one (below). They are kept in `findings.jsonl` in its data folder (the newest 5,000), and only apps on the node's own computer can report them or see them, even on a node run with `--mcp-read-pages`. A page that `check_lookalike` calls a look-alike is not kept. `plumb serve` keeps no findings. Delete the file to forget them all.

### Sharing a finding with other nodes

On a node in the network run with `plumb run --network --share-findings` (off by default), `report_finding` takes `share: true`, and the finding is also sent to other Plumb nodes as a **lead**: a page an agent found useful for a search, for agents searching other nodes for the same thing. Both have to say so, the node's owner with the flag and the agent with each finding; without the flag the finding is kept and the answer says why it was not shared.

A lead carries the page, why it helped (`why`, at most 300 characters), when it was reported and that it holds for 30 days, signed with the node's key. It never carries the answer or the task, and not the search either: each of the search's words goes as a number shared by many words, enough for another node to match its own searches (the same words, or most of them, as findings match) but not to read the search back. Common words can still be guessed from their numbers, so share findings for searches you would not mind being seen. `share_query: true` sends the search as typed too. Pages on a private network (`localhost`, `192.168.x.x`, `*.local`, `*.internal`, a name without a dot) and addresses with a user name or password are not shared. A node shares at most 50 leads a day.

`search` on any node in the network then lists the leads other nodes shared for the query as `leads`, after any `found_before` and apart from the results:

```text
Lead shared by 1 other Plumb node, 1 trusted (2 days ago; unchecked, read it first): https://tokio.rs/blog/2025-07-tokio-1-47 (the release notes list every change)
```

Each lead says which nodes reported it, how this node stands to each (`trusted`, `friend_of_friend` or `other`), when, and until when it holds, with `verified: false`: a lead is someone's report, not a crawl other crawlers checked, so an agent should read the page (`read_page`) before relying on it. Only leads from nodes the node's search scope asks are listed ([Who a search asks](network.md#agreement-between-crawlers)), never the node's own, and none whose page `check_lookalike` calls a look-alike. Like findings, leads are listed only to apps on the node's own computer. How they travel and how many a node keeps: [Shared findings](network.md#shared-findings).

## Following relations (experiment)

`plumb relations` learns each kind of Wikidata fact whose value is another thing (capital, founder, CEO, headquarters, currency and the rest) as a map between the EmbeddingGemma vectors of Wikipedia articles, made from each article's title and description. It reads a Wikipedia articles file with facts (from `plumb fetch-facts`), embeds the articles in its facts, fits the maps, and reports how often they find the facts it held out:

```sh
plumb relations --articles wikipedia-en.tsv.gz --model <EmbeddingGemma dir> --out relations/
```

`plumb mcp --relations relations/` then offers a tool `relate` on top of the others: `relate(subject, relation)` lists the likeliest objects, each with a probability and whether Wikidata states it, and finds them for things Wikidata has no such fact about; `relation` can chain steps (`headquarters > capital`), followed by vector arithmetic in one call, so the step in between never fills a model's context; with `object` it says how likely a claim is instead. Add `--relations-model <dir>` to start from names that are not among the maps' articles. Answers that are not stated are guesses: on 2,000 facts of each kind, the right object came first for 21% (CEO) to 67% (currency) of held-out facts.

## How it is served

- `POST /mcp` takes one JSON-RPC message and answers it with one JSON object; a notification gets `202 Accepted`. There is no event stream (`GET /mcp` answers 405) and no session.
- No token: the tools read nothing that the search page doesn't show, and `read_page` is only offered as described above.
- A request from a web page of another site (an `Origin` other than the node's own host) is refused.
- Tool calls are limited to a burst of 30 per client, then 60 a minute, answered with `429` and `Retry-After` beyond that. The client is the address the request comes from, or, when that is a proxy on the same computer or a private network (Caddy in front of a Docker container, as on plumbsearch.org), the last address in its `X-Forwarded-For`. An IPv6 client counts as its /64 network, which is what one home or server gets. The counts are kept in memory only.
- Like the search page, the node logs no queries.

## Checking it

The end-to-end tests run every tool over the fixture index, through `plumb mcp --index` and through `/mcp` (`cargo test -p plumb-node --test end_to_end mcp`). `official_site` returns the first result of an ordinary search for the name, so `plumb eval --queries eval/ai_queries.tsv` measures it on real data.
