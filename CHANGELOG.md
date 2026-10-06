# Changelog

## Unreleased

### Search

- Learning from clicks, on the node for each browser with search history on: places and maps you seldom open for searches like one you make are folded to one line, "Recent" headlines you often read come unfolded, and sites you always pass over move down a little. `/history` shows what was learned and forgets it; the settings gear turns it off.
- Edit mode ("Edit these results" on a results page): small buttons put a result higher or lower for that search or hide it from that search, and fold places or headlines for that search or every search like it. What you say wins over what your clicks say.
- Tune your search (`/tune`): go through five random searches on the real results page in edit mode, and Plumb learns from the results you move up or down and hide what kinds of result you like (official sites, encyclopedias, forums, code, video, shops, social media, news, government and schools, small or well-known sites, your country's) and whether maps and headlines help you, then leans every search that way. A kind counts only when results of that kind were moved up (or down) clearly more often than the other results on the same pages, so hiding a few off-topic results no longer reads as disliking every kind they belong to. `/tune` and `/history` say what it learned.
- Sites opened before also come up for searches that share words with the one they were opened for ("us bank login" after "us bank").
- "us bank" no longer lists banks in a town called Us: a town guessed from a query that names a site is left out. "Banks in denver" still lists them.

## 0.1.0

The first release. Downloads are on [GitHub Releases](https://github.com/SueHeir/plumb-search/releases/latest), and the Docker image is `ghcr.io/sueheir/plumb-search:0.1.0` (also `:0.1` and `:latest`).

### Search

- Homepage search: sites by name, homepage title and description, and the words other sites link to them with. Official sites come first, and look-alikes stuffed with a brand's keywords stay below the real one.
- Page sets next to sites: English Wikipedia, Stack Overflow's most viewed questions, Open Library's most read books, the most cited papers, well-starred GitHub repositories, Podcast Index's most popular podcasts and the most used packages of eight registries. Only titles, short descriptions and counts are kept.
- Instant answers (sums, unit and currency conversions, the time in a place), info boxes with official sites, profiles and where a film, show, game or album is listed, and sitelinks.
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
- A crawl found by a network search is kept only when its crawler is trusted or confirmed by others, so one new key cannot rewrite a site's title.
- Network searches ask only nodes that take sealed requests when relays are available, and nodes limit connections and how much they serve at once.
- Feeds are read without document type definitions, so a feed cannot expand into gigabytes.

### Fixes from the pre-release audit

- Searches with curly quotes, and a few calculator and time-zone inputs, no longer fail.
- On nodes with a storage limit, the places file is no longer cut at every start, and trimming never drops more sites than it can free.
- Unit symbols keep their case (`mW`, `Gb`), and `gr` is grain.
- robots.txt redirects to another host are followed, and large downloads resume.
