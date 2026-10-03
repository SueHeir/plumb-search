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

# 3. Fold everything into site records, keeping the top million.
plumb ingest --tranco data/tranco-top-1m.csv.zip \
             --cc-ranks data/<release-name>-domain-ranks.txt.gz \
             --wikidata data/wikidata-official-sites.tsv \
             --wat data/*.wat.gz \
             --top 1000000 --out data/records.jsonl

# 4. Crawl homepages to fill in titles and discover new sites through their links.
plumb crawl --records data/records.jsonl --top 10000

# 5. Build the index and run the brand-name test.
plumb index --records data/records.jsonl --index data/index
plumb eval --index data/index --queries eval/brand_queries.tsv
```

The crawler identifies itself as `PlumbSearch/<version> (+https://github.com/SueHeir/plumb-search)`, obeys robots.txt (including `Crawl-delay`), and fetches one page per site.

## How ranking works

Every query is normalized the same way as the indexed text (lowercase, punctuation removed, "U.S." becomes "us"). Candidates come from BM25 over the domain name, aliases, title, link text and description, plus a "joined" match so that "us bank" finds the domain `usbank` and "bankofamerica" finds "Bank of America". Each candidate then gets

```
score = α · link_score + (1 − α) · text_score
```

where `text_score` is the BM25 match scaled to 0–1 within the query's candidates, and `link_score` is a 0–1 popularity prior from the site's best rank (Tranco or Common Crawl), how many other domains link to it, and whether Wikidata lists it as an official website. An exact match between the query and the domain name adds a bonus. `α` defaults to 0.35 and can be changed with `--alpha`.

## Code layout

| Crate | Purpose |
| --- | --- |
| `crates/plumb-core` | The shared site record, domain and text helpers, and the popularity prior |
| `crates/plumb-ingest` | Loaders for Tranco, Common Crawl ranks and WAT files, and Wikidata; the seed builder; downloads |
| `crates/plumb-crawl` | Polite homepage crawler that also reports link text and new domains |
| `crates/plumb-index` | Tantivy index and navigational ranking |
| `crates/plumb-node` | The `plumb` command line tool and the local web page |

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
