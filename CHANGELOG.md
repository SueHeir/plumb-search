# Changelog

## 0.1.0

The first release. Downloads are on [GitHub Releases](https://github.com/SueHeir/plumb-search/releases/latest), and the Docker image is `ghcr.io/sueheir/plumb-search:0.1.0` (also `:0.1` and `:latest`).

### Search

- Homepage search: sites by name, homepage title and description, and the words other sites link to them with. Official sites come first, and look-alikes stuffed with a brand's keywords stay below the real one.
- Page sets next to sites: English Wikipedia, Stack Overflow's most viewed questions, Open Library's most read books, the most cited papers, well-starred GitHub repositories and the most used packages of eight registries. Only titles, short descriptions and counts are kept.
- Instant answers (sums, unit and currency conversions, the time in a place), info boxes with official sites and profiles, and sitelinks.
- Places from OpenStreetMap, with a map drawn from coordinates.
- Recent headlines from the RSS and Atom feeds of the best-ranked sites.
- Search operators, safe search, language, spelling suggestions and optional search by meaning.
- Private search (`/private`): the browser fetches padded buckets and ranks them itself.
- Search history and "About you" interests, kept on the node for each browser; off on public servers.

### Network

- Peer-to-peer network: each day every node crawls a random share of sites, signs each batch, and checks other nodes' batches before taking them in. Nodes behind home routers need no port forwarding.
- New nodes set up from a trusted node's sites in minutes, and keep under a storage limit.
- Network search without sending the query: hashed buckets, padded, through a relay, with a Merkle proof on every result.
- Crawl credits and anonymous tokens that buy priority when a node is busy.

### For AI apps

- MCP server at `/mcp` on every node and `plumb mcp` over stdio, with `search`, `official_site`, `check_lookalike`, `site_info`, `package`, `read_page` and `report_finding`.
- SearXNG-compatible JSON at `/search?format=json`, and Open WebUI's external search at `/api/websearch`.

### Running it

- Docker image for Linux on x86 and ARM, and a desktop app for Windows, macOS and Linux with a settings panel and control of other nodes.
- Plugins: results from other sources, written in Rust and run in a WebAssembly sandbox.

### Security fixes from the pre-release audit

- The crawler refuses private network addresses, including in redirects, icons and feeds.
- Tools and pages meant for the node's own computer check where requests come from more strictly, including requests passed on by a proxy and DNS rebinding.
- Per-client limits for MCP and network search, counting clients behind a local proxy correctly.
- Requests sent by other sites to the node's own forms are refused.
- Tighter limits on a few inputs and on what plugins can write to the log.
