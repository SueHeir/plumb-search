# Frozen12 in-process observations (interface v1)

This example runs the twelve original `run-9fe-r1` queries through the current
production `/api/search` router using `Router::oneshot`. It creates no listener,
Node, cache manager, refresh job, model download, raw-set scan, or settings store.
It writes one new report outside the retained inputs. It cannot grade results.

The adapter is based on exact main
`54122379c4f25956d9b4f8cbb88d617f33693eca`. Its baseline is **541 plus the adapter**
on an externally retained corpus. Each separately validated contribution is
composed with that identical adapter and run on those same bindings. The sealed
9fe captures remain historical evidence; generation names alone do not establish
byte equality. Frozen labels, scorer v4, reports and separate candidate/probe
worktrees are not inputs to this runner and are not modified.

## Source and CI boundary

Implementation, source review, an isolated draft PR and existing synthetic CI are
authorized. Local/HPC compilation and real corpus reads require a separately
issued build/execution lease. A manifest field is a recorded attestation, not a
permission grant. This document does not grant that lease.

Existing `cargo test --workspace --exclude plumb-desktop` runs the retained-reader
library tests. The existing test job also explicitly runs:

```text
cargo test --locked -p plumb-node --example frozen12 -- --test-threads=1
```

Those fixtures create their own tiny indexes in temporary directories. They test
real native/page retrieval, the production router boundary, raw strong-page
evidence, matching bindings, failure/unknown observations and denied mutations.
The navigation candidate's selector tests separately establish its raw-page veto;
the 541 baseline does not contain that selector. No test reads a production index.

## Invocation and bindings

The example accepts exactly `--manifest PATH --report NEW_PATH`, optionally
`--baseline-report PATH`. There is no query, server, cache or result override.
The manifest and baseline reads are limited to 1 MiB and 16 MiB respectively.
Manifest v1 denies unknown top-level fields and contains:

| Field | Required binding |
| --- | --- |
| `version` | `1` |
| `execution_receipt` | Separate reviewed run/resource lease identifier |
| `revision`, `tree` | Exact 40-digit candidate commit/tree; tree externally verified |
| `source_sha256`, `binary_sha256` | Clean embedded source identity and verified actual executable SHA |
| `sites`, `pages` | Each `{path, binding}` for an existing retained generation |
| `places` | Same structure, or explicit `null` when places are disabled |
| `meaning` | Bound local model/vector inputs described below, or explicit `null` |
| `rank` | Serialized production `RankConfig`; identical across contribution comparisons |

Each generation binding contains `generation`, `retention_receipt`,
`corpus_manifest_sha256_attested`, `directory_device`, `directory_inode`, and a
`files` object keyed by single-component file names. Each file binds `bytes`,
`modified_ns`, `device`, `inode`, and `sha256` (nullable). Catalogs contain at most
4096 files; the adapter never enumerates an input directory. `meta.json` requires
a SHA. Every small file Tantivy reads atomically, including `.managed.json` when
catalogued, and `pages.json`/`places.json`, requires a verified SHA and a size at
most 1 MiB. Segment files bind identity, size and time, plus the externally
attested corpus manifest; they are not rehashed. Native spelling-file presence
and readability must agree with the catalog. An uncatalogued required file,
symlink, missing file, changed identity/size/time, or bad metadata SHA fails.

Meaning inputs contain `model`, exact `model_files` bindings, `vectors`,
`vectors_binding`, `retention_receipt`, and `query_instruction` (off/on/mix/min/
split). Only the production local model files are accepted, including Gemma's
two-file format. An embedding-server configuration is rejected. Large model and
vector SHA values are external attestations; size/time/identity are checked before
and after queries. The production model loader reads and hashes its model files
as part of loading; its memory and I/O are included in the execution budget.
The production loader checks that the vectors belong to the model.

The binding comparison includes the fixed query list, exact request options,
all corpus/model/vector catalogs, rank settings and the compiled learned-model
SHA. Source/binary identity is separate so a contribution can differ in code.
Network popularity, adult-list filtering (safe off), plugins, findings and news
are disabled. A places-disabled or meaning-disabled baseline only compares with
the same configuration; it is not an attestation of the historical deployment.

The original request options are fixed:

```text
full=1&limit=10&lang=en&country=US&only=0&exact=0&safe=off&news=off&net=0
```

The cohort contains no time questions. The production router uses the observed
system clock; start/end Unix time is recorded. There is no historical clock
replay claim. A future clock-sensitive cohort needs a separate interface review.

## Read-only and retention contract

`Searcher`, `PageSearcher` and `PlaceSearcher` use their existing construction,
schema, analyzers and manual reload policy with a retained directory. Every write,
delete, sync, writer lock and watch request is denied. Tantivy's META_LOCK uses
a guard shared among clones of that reader directory, with no filesystem lock.
It cannot protect against another opener, process, live writer or pruner.

The operator must first obtain and preserve an external immutable-generation
retention guarantee for the reader lifetime. Catalog/hash checks detect changes;
they do not establish that guarantee and cannot detect a same-identity segment
rewrite whose metadata is restored. Missing full corpus identity stays a blocker
for a claim of a matched-corpus quality comparison. The runner checks bindings
before opening, between requests and after the last request. Any failure clears
all observations and writes `status: incomplete` when it can finish its report.
Killed processes can leave an empty/partial new report; those are incomplete.

Production page retrieval/placement is shared with `node/pages` through
`page_retrieval::add_pages`; the old `eval_mcp` example no longer carries its own
copy. Production's partial-result warning behavior is preserved. This runner
turns those lookup errors into an incomplete report. Raw backend results are
retained before the router's display row budget, so stronger retrieved pages are
not inferred absent from a limited response.

## Observations and scoring boundary

A complete report contains twelve ordered `PG01`–`PG12` observations with their
request, raw retrieval results, unmodified production full-response body and
latency. Organic main rows and navigation provenance come from `assembled.rows`;
places, answers and other native blocks remain separate in the body. The report
labels judgments unknown and supplies no grades, URL-result maps or metric code.

Only existing exact per-query canonical URL judgments can later be joined under
a separately reviewed output/rank adapter. New URLs remain unknown unless blind
evidence and versioned judgments are separately authorized. Parent-domain,
anchor or neighboring-page grades do not transfer. Duplicate handling and frozen
nDCG/main-destination calculations remain outside this change. This is a known,
tuned regression cohort, not held-out quality evidence.

## Concrete resource and cleanup plan before real execution

These are proposed limits to freeze in the **future separate lease**, not current
execution approval. The original build group's 4,831,838,208-byte growth cap had
4,136,980,480 bytes used and 694,857,728 bytes remaining. Those historical values
must be freshly verified. A prior 1,133,096,960-byte allocated test ELF does not
prove that a new example or its link peak fits. Build timeout, temporary peak,
compiler/incremental growth and protected-process reserves remain unknown.

| Quantity | Proposed limit / required evidence |
| --- | --- |
| One owned run group RSS | 6,442,450,944 bytes; external hard watchdog |
| Query wall time | 30 seconds, including router response body |
| Whole run | 600 seconds, including executable hash and startup |
| Report | At most 16 MiB; new file only |
| Runtime temporary space | At most 64 MiB, task-owned TMPDIR; runner creates no temp files |
| Free-space floor | 47,244,640,256 bytes, including every retained artifact |
| MemAvailable floor | 3,221,225,472 bytes plus fresh protected/transient reserves |

No local or HPC build starts until physical growth and peak fit are reviewed.
Prefer one serial task-owned ELF after separately authorizing retirement of the
previous closed owned group. Do not reset the cumulative budget, exempt compiler
artifacts or change build profile to manufacture fit. Count source, target,
linker/incremental/temp files, retained ELFs and unlinked open files physically.

Before execution, freeze this interface, exact adapter commit/tree/source-file
hashes, composed candidate identities, executable hash, corpus/model/options
bindings and fresh protected PID/birth identities. The external watchdog must
measure owned-group RSS/HWM, wall time, physical I/O, growth/temp space, open
unlinked files and host floors. Cooperative code checks are not hard resource
enforcement; a blocked `spawn_blocking` lookup can outlive Tokio's timeout.

On a breach, stop only the owned group and mark its report incomplete. Do not
retry, enlarge limits, restart a protected service, or touch live indexes/cache.
After the group exits and closed-file accounting is verified, retain the bounded
report, source/runtime hashes and watchdog receipt; remove only the explicitly
leased task-owned temporary files and obsolete owned ELF. Preserve frozen
historical artifacts and every independent worktree. No merge/deploy is included.
