# Serving frozen indexes for surface checks

The `eval_mcp` example in plumb-node serves an existing site index and page index through the node's own web and MCP routes, so checks can see what an API or MCP client gets from a fixed corpus. For contributors comparing what [a report](adapter-notes.md) measures with what the routes return.

It opens indexes and nothing else: it never creates a node, builds or refreshes an index, or starts a node's jobs. Source: [eval_mcp.rs](../../crates/plumb-node/examples/eval_mcp.rs).

## Build and run

```sh
cargo build --release -p plumb-node --example eval_mcp
target/release/examples/eval_mcp \
  --scratch-root SCRATCH --index SCRATCH/index --page-index SCRATCH/page-cache/KEY \
  --model SCRATCH/model --vectors SCRATCH/vectors.bin --rank '{}' \
  --country any --language en --bind 127.0.0.1:18081 --seconds 900
```

| Flag | Meaning |
| --- | --- |
| `--scratch-root DIR` | Every other path must resolve inside it. Opening an index can create lock files there, so point it at copies, never at a running node's data |
| `--index DIR` | The site index |
| `--page-index DIR` | A built page index: the folder holding `pages.json`. `plumb-tools eval --pages-cache CACHE` keeps one in `CACHE/<hash>/` and logs which folder it used |
| `--place-index DIR` | Optional: a built place index |
| `--vectors FILE --model DIR` | Optional: search by meaning, with local model files only (an embedding server is refused) |
| `--rank JSON` | Ranking knobs, as for `eval --rank` (default `{}`) |
| `--country CODE` | A fixed home country or `any` (the default); `auto` is refused, so the host's region cannot change results |
| `--language CODE` | The search pages' language (default `en`) |
| `--bind ADDR` | A loopback address only (default `127.0.0.1:18081`) |
| `--seconds N` | How long it serves before it stops, 1 to 3600 (default 900) |
| `--snapshot-manifest FILE` | Optional: a JSON file of at most 1 MiB, shown as-is in `/eval/status` |

Use the same ranking knobs, model, vectors and page index as the report you compare it with, so both see the same corpus.

## What it serves

- **Routes**: `GET /`, `/search`, `/api/search`, `/opensearch.xml` and `/eval/status`, and `POST /mcp`. Everything else is refused.
- **MCP**: the methods `initialize`, `ping`, `notifications/initialized`, `tools/list` and `tools/call`, and only the tools `search`, `official_site`, `check_lookalike`, `site_info`, `facts` and `package`. Tool calls are spaced 1,050 ms apart (`TOOL_GAP`), so their latency includes that wait; take latency from reports instead.
- **Searches** that would need currency rates, the weather or an outside search engine (a bang) are refused, on the pages and in MCP alike.
- **No local privileges**: requests carry no client address, so the node's MCP treats every caller as remote and offers no page reading, findings or leads, even from localhost.
- **`/eval/status`**: the build (as `plumb build-info` prints it), the indexes opened and their site and page counts, the model, vectors, ranking knobs, country and language, and which features are off. It describes what was opened; it does not hash the files.

The routes answer with the wall clock, not a fixed one, so date-dependent checks belong in fixed-clock reports.

## Checking the tools against it

[official_site/run.py](../official_site/README.md) runs the identity suites against its `/mcp`. `--require-revision` refuses a server whose embedded revision differs or is not clean:

```sh
python3 eval/official_site/run.py --mcp http://127.0.0.1:18081/mcp \
  --require-revision "$(target/release/plumb build-info | python3 -c 'import json,sys; print(json.load(sys.stdin)["revision"])')" \
  --max-calls 400 --time-budget 600 --out identity.jsonl \
  eval/contracts/audit.jsonl eval/official_site/queries.tsv eval/official_site/heldout.tsv
```

## Tests

The example's tests serve synthetic pages and sites through the node's router and check docs, package and facts answers, the build metadata, the refused routes and the scratch-root check:

```sh
cargo test -p plumb-node --example eval_mcp
```
