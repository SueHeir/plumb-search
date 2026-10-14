# Plumb Search documentation

Every page about Plumb Search, grouped by what you want to do. New here? Start with the [README](../README.md), then [using search](searching.md) or [running a node](running-a-node.md). To change the code, start with [the architecture](architecture.md).

## Searching

| Page | What it covers |
| --- | --- |
| [Using search](searching.md) | The search page, operators, filters, history and About you, and the JSON API |
| [Private search](private-search.md) | The `/private` page, where the browser ranks results and the node never sees the query |
| [Places](places.md) | Places from OpenStreetMap, "near me" and the map |
| [Headlines](news.md) | Recent headlines from the feeds sites publish |

## AI apps

| Page | What it covers |
| --- | --- |
| [MCP server](mcp.md) | The tools at `/mcp` and `plumb mcp`, and how to add them to Claude and other apps |
| [Local models and AI apps](local-llms.md) | Setup for LM Studio, Open WebUI and other apps, through MCP or a SearXNG address |

## Running a node

| Page | What it covers |
| --- | --- |
| [Running a node](running-a-node.md) | `plumb run`, its settings, the data folder, upgrading, and building an index by hand |
| [Docker](docker.md) | The image, Compose, updating and running behind a proxy |
| [Desktop app](desktop.md) | The app for Windows, macOS and Linux, and where it keeps its data |
| [Remote control](remote-control-security.md) | How the desktop app controls another node safely |
| [Plugins](plugins.md) | Installing plugins and writing your own in Rust |

## How it works

| Page | What it covers |
| --- | --- |
| [Architecture](architecture.md) | The programs, the crates, what runs inside a node, and how a crawl and a search travel through it |
| [Search and ranking](search.md) | How a search is answered: matching, ranking, page sets, places and answers, and how to measure a change |
| [The network](network.md) | Sharing crawls, trust, signed sets and searching other nodes |
| [Cache-first network search](cache-first-search.md) | Network search's bucket cache and scheduled rounds |
| [Page sets](pages.md) | The signed sets of single pages: Wikipedia, GitHub, Stack Exchange, packages, books, papers and podcasts |
| [How search is doing](search-quality.md) | Results of the 1,000-search review, run by run, with charts |

## The code

Each crate has a README with its design, modules and tests.

| Crate | What it is |
| --- | --- |
| [plumb-core](../crates/plumb-core/README.md) | The site record, text and domain helpers, storage and locks every crate shares |
| [plumb-format](../crates/plumb-format/README.md) | The signed set format nodes and set builders exchange |
| [plumb-answer](../crates/plumb-answer/README.md) | Instant answers worked out from the query alone |
| [plumb-ingest](../crates/plumb-ingest/README.md) | Seed data loaders and downloads |
| [plumb-crawl](../crates/plumb-crawl/README.md) | The homepage crawler |
| [plumb-index](../crates/plumb-index/README.md) | The site index, page sets, places and ranking |
| [plumb-embed](../crates/plumb-embed/README.md) | Embeddings for search by meaning |
| [plumb-net](../crates/plumb-net/README.md) | The peer-to-peer network |
| [plumb-plugin](../crates/plumb-plugin/README.md) | The kit for writing plugins |
| [plumb-node](../crates/plumb-node/README.md) | `plumb`, the node: jobs, the worker and the web layer |
| [plumb-tools](../crates/plumb-tools/README.md) | `plumb-tools`: building sets, evals, ranking training |
| [plumb-private](../crates/plumb-private/README.md) | Private search in the browser, compiled to WebAssembly |
| [plumb-desktop](../crates/plumb-desktop/README.md) | The desktop app |
| [plumb-e2e](../crates/plumb-e2e/README.md) | End-to-end tests against the Docker image |

Outside the crates: [evals](../eval/README.md), [test data](../fixtures/README.md), [plugins](../plugins), the [Zotero plugin](../zotero/README.md) and [plumbsearch.org's setup](../site/README.md). [CONTRIBUTING.md](../CONTRIBUTING.md) has the rules for changes and [CLAUDE.md](../CLAUDE.md) how node code is written.

## Records

Dated reviews and plans, kept as they were written. The code has moved on since, so check anything in them against the pages above.

| Page | What it is |
| --- | --- |
| [reviews/architecture.md](reviews/architecture.md) | An architecture and data-flow map from 2026-10-04 |
| [reviews/privacy-security.md](reviews/privacy-security.md) | A privacy and security review from 2026-10-04 |
| [reviews/search-improvement-plan.md](reviews/search-improvement-plan.md) | A plan for improving relevance and the data pipeline |
| [reviews/search-generality-audit.md](reviews/search-generality-audit.md) | An audit that search rules are general, not fitted to test cases |
| [reviews/search-quality-batch.md](reviews/search-quality-batch.md) | A batch of search-quality changes from 2026-10-09 |
| [reviews/search-quality-results.md](reviews/search-quality-results.md) | Results of that batch's release, 2026-10-10 |
| [reviews/paper-repair-integration.md](reviews/paper-repair-integration.md) | Consistency checks for paper metadata |
| [operations/release-supervision.md](operations/release-supervision.md) | A runbook for 0.2 search-quality releases |
