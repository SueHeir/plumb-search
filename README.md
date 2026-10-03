# Plumb Search

Plumb Search is a free, open-source search engine for finding websites by name, built to run on your own machine. Type "us bank" and usbank.com comes first.

It indexes names, not pages. For each site it keeps the homepage title and description, the words other sites use when they link to it, and a few aliases. That is about 1 KB per site, so a million sites fit in roughly a gigabyte on a homelab server or a desktop.

This repository holds the **Phase 1 prototype**: a single node that builds its own index from public seed data plus its own homepage crawls, searches it locally, and measures how often the official site comes first. Sharing the index between nodes, community crawling and private popularity counts come in later phases (see [Roadmap](#roadmap)).

## Quick start with the bundled test data

The `fixtures/` folder holds a small, made-up dataset in the same formats as the real sources, so the whole pipeline runs offline.

```sh
cargo build --release
alias plumb=./target/release/plumb

plumb ingest --tranco fixtures/tranco.csv \
             --cc-ranks fixtures/cc-domain-ranks.txt \
             --wat fixtures/sample.wat \
             --wikidata fixtures/wikidata-official-sites.tsv \
             --out data/records.jsonl
plumb index --records data/records.jsonl --index data/index
plumb search --index data/index us bank
plumb eval --index data/index --queries fixtures/brand_queries.tsv
plumb serve --index data/index        # then open http://127.0.0.1:8080
```

## Run a node

`plumb run` does the work of the next section by itself, apart from the optional WAT files, and keeps going. It serves the search page at once, downloads the seed data and builds a first index, then crawls homepages and builds the index again, and from then on crawls more homepages and rebuilds the index on a schedule.

```sh
cargo build --release -p plumb-node
./target/release/plumb run --data plumb-data
```

Then open http://127.0.0.1:8080. Until the first index is ready, the page shows what the node is doing, and `/api/status` reports the same as JSON. Setup and crawling need internet access; searching works offline.

By default a node keeps the best million sites, crawls 10,000 of their homepages once its first index is built, and crawls 5,000 more every 24 hours. `--profile desktop` starts smaller (250,000 sites, 2,000 homepages at first and 1,000 more every 12 hours). `--cc-release <release-name>` also takes ranks from a Common Crawl web graph release on first start (release names are listed at https://commoncrawl.org/web-graphs; only the top rows are downloaded). `--bind 0.0.0.0:8080` serves other machines too, and since the page has no login, anyone who can reach the port can search. `plumb run --help` lists the other settings.

Everything the node keeps is in its `--data` folder. `records.jsonl` holds what it has learned. Crawls add their results to `records.jsonl.journal` as they go, and the node folds that journal into `records.jsonl` once it has grown, so back up both files together. A node started with a `records.jsonl` already in its folder skips the seed downloads.

The seed downloads go through a proxy set in the usual variables (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`), but homepages are fetched directly, which lets the crawler refuse sites whose names lead into private networks. If the machine reaches the internet only through a proxy, add `--use-system-proxy` to crawl through it too. Without it, every crawl fails and the node reports that the network seems to be down.

On a server or homelab machine, run the Docker image instead ([docs/docker.md](docs/docker.md)):

```sh
docker compose up -d
```

On a desktop or laptop, install the desktop app for Windows, macOS or Linux ([docs/desktop.md](docs/desktop.md)). It runs the same node in a window of its own, at http://127.0.0.1:7586.

### Join the Plumb network (prototype)

`plumb run --network --bootstrap <node address>` connects a node to other Plumb nodes: it crawls only the sites the network assigns it each day, shares signed crawl results with the others, takes in theirs, and can search other nodes without sending them the query (`/network?q=`, linked from every results page). No port forwarding is needed. See [docs/network.md](docs/network.md) for the design, the flags and what is not done yet.

### Use Plumb as your browser's search engine

Every Plumb page offers Plumb to the browser as a search engine. In Firefox, right-click the address bar on a Plumb page and choose **Add "Plumb Search"**. To add it by hand, use `http://127.0.0.1:8080/search?q=%s`, or `http://127.0.0.1:7586/search?q=%s` for the desktop app.

## Building an index from real data

The seed data comes from four public sources. They are only needed to get started; after that the index grows from its own crawls.

| Source | What Plumb takes from it |
| --- | --- |
| [Tranco](https://tranco-list.eu/) top 1M | A popularity rank for each site |
| [Common Crawl web graph](https://commoncrawl.org/web-graphs) domain ranks | Harmonic centrality and PageRank positions for over 100M domains |
| Common Crawl WAT files | Homepage titles and descriptions, and the text of links between sites |
| Wikidata "official website" (P856) | Which domain is an organization's real site, plus its name as an alias |

```sh
# 1. Download Tranco, the Common Crawl domain ranks and the Wikidata list.
#    Pick a web graph release name from https://commoncrawl.org/web-graphs
plumb fetch-data --dir data --cc-release <release-name>

# 2. Optional: download a few WAT files. Each crawl lists its WAT files in
#    https://data.commoncrawl.org/crawl-data/<CC-MAIN-YYYY-WW>/wat.paths.gz;
#    prefix each path with https://data.commoncrawl.org/ to download it.

# 3. Fold everything into site records, keeping the best million. Only the
#    top 2,000,000 rows of the Common Crawl ranks are read (twice --top),
#    which takes about 2 GB of memory; --limit-per-source N changes that.
plumb ingest --tranco data/tranco-top-1m.csv.zip \
             --cc-ranks data/<release-name>-domain-ranks.txt.gz \
             --wikidata data/wikidata-official-sites.tsv \
             --wat data/*.wat.gz \
             --top 1000000 --out data/records.jsonl

# 4. Crawl homepages to fill in titles and discover new sites through their links.
#    Half go to sites not tried yet and half to sites due again, best first.
#    Each batch of results is saved to data/records.jsonl.journal at once and
#    folded into records.jsonl at the end, so an interrupted crawl keeps what
#    it fetched. If this machine reaches the internet only through a proxy,
#    add --use-system-proxy.
plumb crawl --records data/records.jsonl --top 10000

# 5. Build the index and run the brand-name test.
plumb index --records data/records.jsonl --index data/index
plumb eval --index data/index --queries eval/brand_queries.tsv
```

To refresh the seed data later, run steps 1 and 3 again with `--records data/records.jsonl` added to step 3. Titles, link text and crawl times carry over, while ranks and official-site marks come only from the new files, so a domain that has expired and changed hands does not keep the trust it had.

The crawler identifies itself as `PlumbSearch/<version> (+https://github.com/SueHeir/plumb-search)`, obeys robots.txt (including `Crawl-delay`), and fetches one page per site.

## How ranking works

Every query is normalized the same way as the indexed text (lowercase, punctuation removed, "U.S." becomes "us"). Candidates come from BM25 over the domain name, aliases, title, link text and description, plus a "joined" match so that "us bank" finds the domain `usbank` and "bankofamerica" finds "Bank of America". Each candidate then gets

```
score = α · link_score + trust · ((1 − α) · text_score + name_bonus)
```

- `text_score` is the BM25 match scaled to 0–1 within the query's candidates.
- `link_score` is a 0–1 popularity prior from the site's best rank (Tranco or Common Crawl), how many other domains link to it, and whether Wikidata lists it as an official website.
- `name_bonus` rewards a site whose domain name (or, with a smaller bonus, one of its aliases) is the query, or the first words of it: `usbank.com` for "us bank", and half the bonus for "us bank login".
- `trust` protects official sites from keyword-stuffed look-alikes. When a query starts with a known site's name and adds more words ("irs refund", "us bank login"), sites with almost no popularity evidence keep only part of their text match, down to half for a site with none. Otherwise `trust` is 1, so little-known sites still rank normally when no well-known site matches.

`α` defaults to 0.35 and can be changed with `--alpha`. The crate docs in `crates/plumb-index` describe the details and tuning knobs.

## Code layout

| Crate | Purpose |
| --- | --- |
| `crates/plumb-core` | The shared site record, domain and text helpers, and the popularity prior |
| `crates/plumb-ingest` | Loaders for Tranco, Common Crawl ranks and WAT files, and Wikidata; the seed builder; downloads |
| `crates/plumb-crawl` | Polite homepage crawler that also reports link text and new domains |
| `crates/plumb-index` | Tantivy index and navigational ranking |
| `crates/plumb-net` | The Plumb network: daily crawl assignments, signed crawl batches with Merkle proofs, and the libp2p node (relays, hole punching, gossip, bucket-based network search) |
| `crates/plumb-node` | The `plumb` command line tool, the long-running node behind `plumb run`, and the web page |
| `crates/plumb-desktop` | The desktop app: a [Tauri](https://v2.tauri.app) window around a node running inside it. A plain `cargo build` leaves it out; see [docs/desktop.md](docs/desktop.md) |

`fixtures/` holds the synthetic test data and `eval/brand_queries.tsv` the brand-name test list for real data.

## Roadmap

Each phase ends at a gate that proves it works before the next begins.

1. **Prototype on one machine** (this repository). Gate: brand names on a test list return the official site first.
2. **Shared index.** Signed records synced between full and light nodes by Merkle root and daily changes; storage settings; optional network search through an Oblivious HTTP relay. Gate: two nodes stay identical using daily changes alone.
3. **Community crawling.** Random site assignments, receipts on homepage fetches, spot checks; growth from crawled links, Certificate Transparency logs and owner submissions. Gate: the list stays fresh with no new Common Crawl data.
4. **Private popularity.** Blind tokens for verified crawls, capped reports through a relay, threshold counting of what people search for and pick. Gate: a simulated bot farm cannot move a ranking without matching crawl work.
5. **Hardening.** Zero-knowledge membership if token issuers become a weak point, and a process for deciding protocol changes.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
