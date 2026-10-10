# Scratch MCP surface contracts

`plumb-node/examples/eval_mcp.rs` opens a frozen site index and an **already
built** bounded page index. It serves the production `web::router_with` and
MCP handlers with a `SearchBackend` implementing page blending, typed retrieval,
entity lookup, packages, definitions, songs, and optional prebuilt places.
It never creates a `Node`, builds indexes, refreshes data, or starts node jobs.
All input paths must resolve inside the explicit scratch root. Opening Tantivy
readers may create lock files there; use the scratch copy of the site index.

The adapter requires the combined candidate APIs (`SearchBackend::entities`,
`PageSearcher::entities`, and `PageSearcher::in_language`). The frozen harness-only baseline predates this example; the integrated candidate
includes it in normal all-target checks.
Do not add feature APIs to the baseline just to run this appendix: compare core
reports first, then run candidate surface contracts. This is not a paired
baseline/candidate MCP result by itself.

Retain the core run's derived page index with `run_batch.py --pages-cache
"$QUALITY_ROOT/page-cache-candidate"`. The evaluator prints the selected cache
directory in `core-resource.log`; choose its directory containing `pages.json`
for `--page-index`. The baseline cache is separate (`page-cache-baseline`).
Opening a baseline cache with a changed candidate schema may fail and must not
trigger a rebuild in this server. Keep the chosen directory out of concurrent
cache pruning. Core corpus hashes, page input order/caps and actual counts stay
in `core.jsonl`; this server's `/eval/status` describes the opened indexes and
can include the original snapshot manifest. It does not independently hash or
attest the snapshot files.

Build the example in the clean integrated candidate checkout, using the same
revision as the candidate `plumb` executable. Serialize Linux builds and use the
coordinator's memory/time limits. This command is a build recipe, not part of
`run_batch.py`:

```sh
QUALITY_ROOT=/home/suehr/scratch/plumb-quality-20261009
CARGO_TARGET_DIR="$QUALITY_ROOT/candidate-target" \
  CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 CARGO_PROFILE_RELEASE_DEBUG=0 \
  cargo build --release -p plumb-node --example eval_mcp
```

Once core comparison is recorded and the retained page cache is ready, run the
example with its explicit generation directory. The example defaults to 15
minutes and accepts at most one hour; `--seconds 900` covers the bounded
400-call/600-second identity appendix. It refuses non-loopback binds, automatic
home-country inference, embedding servers and model files outside scratch.
The rank JSON, local model/vectors and query instruction must match the core run.

```sh
QUALITY_ROOT=/home/suehr/scratch/plumb-quality-20261009
PAGE_INDEX="$QUALITY_ROOT/page-cache-candidate/REPLACE_WITH_GENERATION_FROM_LOG"
"$QUALITY_ROOT/candidate-target/release/examples/eval_mcp" \
  --scratch-root "$QUALITY_ROOT" \
  --index "$QUALITY_ROOT/baseline-snapshot/indexes/000145" \
  --page-index "$PAGE_INDEX" \
  --model "$QUALITY_ROOT/baseline-snapshot/model" \
  --vectors "$QUALITY_ROOT/baseline-snapshot/vectors.bin" \
  --query-instruction split --rank '{}' --country any --language en \
  --snapshot-manifest "$QUALITY_ROOT/baseline-snapshot/snapshot.json" \
  --bind 127.0.0.1:18081 --seconds 900
```

An optional `--place-index "$QUALITY_ROOT/small-place-index"` opens a separately
prepared fixture or prebuilt place index. Omit it for this batch's page/identity
appendix; no place coverage claim follows from that run. Never pass the
24-million-record places gzip here.

Save `/eval/status` beside the appendix, check its build and index counts, then
use the parent's integrated identity auditor. The serving example and candidate
binary must both embed the same known clean revision. Use a fresh output path;
do not rerun the core batch solely to add an appendix.

```sh
curl --fail --silent http://127.0.0.1:18081/eval/status \
  > "$QUALITY_ROOT/results/candidate/scratch-status.json"
REVISION=$("$QUALITY_ROOT/candidate-target/release/plumb" build-info \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["revision"])')
python3 eval/official_site/run.py --mcp http://127.0.0.1:18081/mcp \
  --require-revision "$REVISION" --max-calls 400 --time-budget 600 \
  --out "$QUALITY_ROOT/results/candidate/identity.jsonl" \
  eval/contracts/audit.jsonl eval/official_site/queries.tsv eval/official_site/heldout.tsv
```

Requests have no `ConnectInfo` extension, so production MCP grants no local
page-reader/findings/leads privileges even with a localhost Host header. The
outer guard allows only read-only search surfaces and six tools (`search`,
`official_site`, `check_lookalike`, `site_info`, `facts`, `package`). It rejects
external-search routes, management routes, page reads, finding writes, currency
rate queries, weather queries and external bang redirects before routing them.
No plugins, personalization, history or peer network are attached. Tool calls
are paced 1,050 ms apart through the production limiter (60/minute); appendix
latency therefore includes deliberate pacing. Use core reports for query latency.

Production web/MCP handlers use their wall clock. This adapter does not claim
fixed-clock temporal acceptance; date/freshness gates use the fixed-clock core
reports. Its page assembly mirrors the bounded public primitives in
`node/pages.rs`; it does not exercise a live Node's background lifecycle or
prove exact deployment equivalence. Review the adapter if that assembly changes.
Retrieval/placement errors fail a full search rather than silently omitting page
enrichment, while typed retrieval follows production's empty-result error path.

The focused example tests create only four synthetic pages and one site, then
exercise the **production HTTP router** for docs, package and facts responses,
build metadata, web page enrichment and disabled outbound paths:

```sh
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 \
  CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
  cargo test -p plumb-node --example eval_mcp
```
