# Changelog

## Unreleased

### Search

- Learned ranking: the first ten results are put in order by a small model trained on the 2,463 test searches in `eval/` (never on what anyone searched). It reads what the ranking already knew about each result, such as its place, score, name match and the pages under a site, and on the half of the test searches it never saw it puts the expected result first for 83% of them instead of 74%: articles before namesake sites ("mars"), the package page for "chalk npm", the site described for "team chat". `plumb train-rank` trains it again, and `plumb eval --rank '{"learned": false}'` measures the order without it.
- Learning from clicks, on the node for each browser with search history on: places and maps you seldom open for searches like one you make are folded to one line, "Recent" headlines you often read come unfolded, and sites you always pass over move down a little. `/history` shows what was learned and forgets it; the settings gear turns it off.
- Edit mode ("Edit these results" on a results page): small buttons put a result higher or lower for that search or hide it from that search, and fold places or headlines for that search or every search like it. What you say wins over what your clicks say.
- Tune your search (`/tune`): go through five random searches on the real results page in edit mode, and Plumb learns from the results you move up or down and hide what kinds of result you like (official sites, encyclopedias, forums, code, video, shops, social media, news, government and schools, small or well-known sites, your country's) and whether maps and headlines help you, then leans every search that way. A kind counts only when results of that kind were moved up (or down) clearly more often than the other results on the same pages, so hiding a few off-topic results no longer reads as disliking every kind they belong to. `/tune` and `/history` say what it learned.
- Sites opened before also come up for searches that share words with the one they were opened for ("us bank login" after "us bank").
- Songs and albums: the `music` page set lists the 150,000 songs and 30,000 albums people listen to most, from MusicBrainz's data with ListenBrainz's listener counts. "hey jude beatles", "hey jude song" and "abbey road album" find them, and "bohemian rhapsody lyrics" links the song's lyrics on Genius (or searches Genius for them when MusicBrainz knows no page). No lyrics are kept. Make it with `plumb fetch-pages --set music`.
- Films and TV shows: the `films` page set lists the 150,000 films and shows Wikidata knows best, each with its year (or the years a show ran), director or creator, best-known cast and where it is listed (IMDb, Letterboxd, Rotten Tomatoes, Netflix and others). "dune 2021", "dune david lynch", "inception dicaprio", "dune movie" and "breaking bad tv show" ask for one, found as its Wikipedia article when there is one; films and shows with no English article are found by their English or original titles. Make it with `plumb fetch-pages --set films`.
- Software docs: the `docs` page set lists the pages of MDN, Python's docs, the Rust book and standard library, Node.js, React, PostgreSQL, Docker, Kubernetes, Git and 29 more docs sites, read from their sitemaps and tables of contents with robots.txt obeyed. A page is named by its product and title ("python sorting techniques", "javascript array.prototype.sort()") and also found by most of its words, like a question ("sort a list in python"). It is the first set of inner pages from good sites; each kind of site gets its own switch under Page sets. Make it with `plumb fetch-pages --set docs`.
- "us bank" no longer lists banks in a town called Us: a town guessed from a query that names a site is left out. "Banks in denver" still lists them.

### Crawling

- New sites are held back: crawls refresh the sites a node holds and no longer add the domains they find linked, and records other nodes share only refresh sites already held, while the network makes the sites it has searchable first. Filling free space from trusted nodes, the seed and searches still add sites the network knows. `plumb run --take-new-sites` adds new sites as before.
- Dead sites: `plumb run --drop-dead-sites` takes sites out of the index that no crawl has reached for 60 days after 6 tries in a row that got no answer. Never the best 10,000 sites, official websites or ones you chose. A dead site keeps a small record and comes back when it answers again. Off by default; without it the node only counts them in its log, and `plumb dead-sites --data DIR` counts them without changing anything.
- Reading sites anew: when crawlers learn to read more from a homepage, the crawl version goes up and nodes crawl the sites read by an older crawler again, best-known first.

### Nodes

- Much less memory: crawl rounds keep about 150 bytes of each site instead of its whole record, and index builds, search by meaning and the network's records read the records file a record at a time. With two million sites a crawl round holds about 300 MB of site data instead of 3 GB or more, and an index build about 0.6 GB instead of about 5 GB, so a 4 GB server runs a node. `plumb index` reads a record at a time too when the records file has no journal.
- The records' journal is folded into the file at every index build, and once it reaches 128 MB, so it no longer grows for days on a big node.

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
