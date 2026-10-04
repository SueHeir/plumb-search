# Plumb Search: handoff back to Claude

Written 2026-10-04. Repository: <https://github.com/SueHeir/plumb-search>. Pull `main` before continuing. The code, report changes, probe and this handoff are committed together; use `git log -1 -- docs/reviews/pir-handoff.md` to identify the handoff commit.

## User direction and division of work

The user authorized Codex to develop, test and push to main, while Claude handled servers. Codex has not deployed, logged into, scanned or changed any live server. The user is now moving development back to Claude. Follow the user's current authorization for subsequent deployment work; this document supplies context rather than new permissions.

Keep application/tooling in Rust, and do not add autocomplete. The earlier Claude handoff remains the source of fleet details and credentials; no credentials or live addresses are reproduced here. Codex's checkout was `/Users/suehr/.codex/worktrees/624a/plumb-network`, on `codex/security-fixes`, pushed with `git push origin HEAD:main` without force.

## Desired search architecture

The user wants useful searches without requiring everyone to hold 10 GB locally. They reported roughly 2 GB on the HPC and judged the results decent; neither that size nor the live data was independently measured. They accept download delay because retrieved data should stay on the device.

The latest decision is **direct single-server computational PIR** as the core retrieval direction: device -> PIR server -> persistent local cache -> local search. A forwarding relay becomes optional address privacy. PIR can hide which database row was selected under its cryptographic assumptions; it does not hide the client's IP, traffic timing, request count or other application disclosures. Preserve query-independent scheduled downloads for the timing objective.

Avoid a two-server XOR scheme for the current same-owner fleet: distinct hosts and keys do not establish non-colluding operators. A prior two-server sketch was removed from the repo before committing and is not an adopted implementation.

## Already on main before this handoff

The important commits, oldest first:

| Commit | Result |
| --- | --- |
| `827fb42` | Sanitize off-domain links received through trusted seed/fill paths. |
| `d29df46` | Require verified HTTPS for off-host bearer-token remote control. Loopback handling remains local. Existing fleet HTTP-control arrangements need Claude's review. |
| `24c0a10` | Remove query-bearing diagnostic text. |
| `25428c3` | Compile the crashed-process index-test helper only on Linux. |
| `bf59972` | Architecture diagrams, privacy/security review and offline bucket-inference probe. |
| `95b59ef` | Claude's PR #113 merged: crawler final-homepage handling and worker detection. Codex changes were built on top. |
| `ea926d4` | Extend existing native cache and round scheduler; add read-only storage sizing; correct privacy/status copy. |

The desktop/local index and network bucket cache already existed. Codex extended those components, rather than introducing another search engine.

With native background rounds enabled, searches read retained buckets and enqueue real missing/overdue IDs. They do not launch requests, advance deadlines or skip future rounds. Fixed intervals remain **10 minutes for nodes, 30 minutes for desktop** by default. Up to four queued IDs fill a round, with background fillers. Pending IDs are bounded/deduplicated; failed refreshes go to the tail. Slow rounds skip slots, and there are no catch-up bursts. Missing results can remain pending beyond a caller's timeout and beyond one slot if the queue/network requires it.

Twelve hours now marks cached buckets stale instead of deleting them. They survive restarts within limits of 4,096 buckets, 512 MiB total and 16 MiB per entry, oldest fetch first. Valid empty answers are cached; unanswered refreshes preserve old data. Unix directories/files use 0700/0600. Proofs are rechecked at the current time; retention does not extend proof validity. Expired proofs lose verified/confirmed status. Do not share one cache directory between processes.

Important exceptions: `--round-minutes 0` still enables legacy immediate retrieval. Normal hosted `/search?q=` sends the full query to its serving node. The browser `/private` path has separate retrieval, HTTP caching and sealed-to-direct fallback; it has not received native scheduling or PIR. The cache clear call clears pending IDs, but an already in-flight round can still finish storing data.

Validation for `ea926d4`: 666 workspace tests passed, three existing ignored tests, plus 13 desktop tests; strict clippy/formatting passed. Browser WASM clippy/build/pinned wasm-bindgen processing passed, with matching node tests against the embedded script. Connected-peer regressions confirmed misses send no request before a due slot. No deployment was performed.

Remote CI and Docker-image build passed for that commit, but [the separate two-node Docker E2E job failed](https://github.com/SueHeir/plumb-search/actions/runs/37227038150): its three-minute retry window expired before the new ten-minute scheduled slot, leaving `pending: 1`. This handoff changes that transport fixture to explicitly choose `--round-minutes 0`; scheduler behavior remains covered by native network integration tests. The fixture was compile-checked locally; Docker is unavailable in Codex's current host environment, so the latest main E2E run must confirm it. Check all workflows for the handoff commit before deploying.

## New in the handoff commit

### Useful sizing for the real corpus

`crates/plumb-node/src/storage.rs` now adds `bucket_tables[].object_array_payload`: aggregate/distribution of prospective UTF-8 `[record_json,...]` row sizes, an 8-byte length prefix, the maximum padded row size and all 16,384 equal rows' padded total.

After building the new binary, Claude can run this on the actual HPC data directory:

```sh
plumb storage --data /path/to/plumb-data --json > storage-report.json
```

This is read-only. It separates logical file sizes from searchable payload, models, indexes, icons and network/history storage. Bucket payload sizing reads index offsets/membership IDs and only stats `records.dat`; it does not open that file's contents. The main `records.jsonl` stream separately gives full/slim serialized sizes, without replaying its journal. No identity, token or history contents are opened. Symlinks/special files are excluded. Corrupt IDs/offsets, overflow or metadata beyond the 1 GiB scan limit produce an incomplete report and nonzero exit, rather than guesses. Record-length caching is bounded to 8 MiB. A live scan is not an atomic snapshot.

The proposed object-array sizes assume valid record JSON and exclude HTTP string escaping, crawl-proof envelopes, compression and cryptographic expansion. They are layout estimates, not actual PIR wire measurements. Sixteen matching storage tests passed, including five new cases covering framing, malformed IDs/offsets, large metadata, bounded fallback reads and inaccessible record contents.

### Real, offline single-server PIR probe

`tools/pir-probe` is a standalone Rust workspace with its own lockfile and pinned `spiral-rs =0.2.1-alpha.2`. Research dependencies do not enter shipped binaries. The published crate records VCS `56700d24cb93039eec74194a879b4811d44afe97`; its core sources match reviewed SDK `fdb7206517c249603b0c91b65f1f29f95272107c`.

It generates real encrypted selectors, serializes public setup and queries, executes the server, decodes and verifies selected rows against a frozen local fixture. The server function gets no plaintext selector or search query. All of this happens offline in one process; no HTTP/peer endpoint or app mode has been added. Synthetic payloads have varying lengths inside equal-size rows. It prints aggregate costs rather than payloads or selected IDs. Setup is included in every request; future setup amortization is not implemented. Input validation checks sizes, coefficient ranges, allocation limits, decoded padding and regular-file types. Tests do not establish a cryptographic security proof or malicious-server authenticity.

Reproduce the default smoke profile:

```sh
cargo test --release --locked --manifest-path tools/pir-probe/Cargo.toml
cargo run --release --locked --manifest-path tools/pir-probe/Cargo.toml -- --queries 5
```

Reproduce the exact upstream v1 profile:

```sh
cargo run --release --locked --manifest-path tools/pir-probe/Cargo.toml -- \
  --profile upstream-16k --max-database-mib 4608 --queries 3
```

The latter explicitly allows 4.5 GiB of raw plus preprocessed database storage; query scratch is additional. Do not run it concurrently on a memory-constrained host. CI exercises the small profile only.

Observed release measurements on **Apple M5 Pro, 18 physical cores, 24 GiB RAM**, using synthetic data and the library's default Rayon pool:

| Measurement | Small upstream test profile | Exact upstream v1 profile |
| --- | --- | --- |
| Rows x row bytes | 256 x 8,192 | 16,384 x 32,768 |
| Raw / preprocessed DB | 2 MiB / 16 MiB | 512 MiB / 4 GiB |
| Preprocessing | 47.6 ms | 10.61 s |
| Public setup | 1,966,112 bytes | 1,114,144 bytes |
| Encrypted query | 16,416 bytes | 16,416 bytes |
| Reply | 20,480 bytes | 86,016 bytes |
| Server per retrieval | 34.2–36.0 ms, five samples | 331–585 ms, three samples |
| Local query / decode | about 0.45 / 0.23 ms | about 0.45 / 0.64 ms |
| Correct retrievals | 5/5 | 3/3 |

The server timings include public setup deserialization. These runs are not HPC, network, production-load or real-bucket measurements. Synthetic rows mostly contain padding, and sample counts are small. Raw reports are in `docs/reviews/measurements/`. Five probe tests, strict clippy and formatting passed. RustSec scanning of its locked dependencies found no reported advisory; that is not a cryptographic audit.

## Protocol choice and actual blockers

The Spiral SDK explicitly states it has not been security reviewed. The probe preserves upstream parameter tuples and makes no 128-bit security certification. Its noise estimator uses an empirical simplification; passing its error estimate is not independent security validation. The upstream codec is native-endian and panic-heavy. The probe limits itself to little-endian hosts and fixed profiles; a production boundary needs explicit canonical encoding, fallible parsing, approved profiles and resource limits. Do not expose upstream deserializers directly on HTTP.

Other candidates were researched from primary sources. Brave FrodoPIR's small upstream native test passed, but it is explicitly a research prototype with database-dependent client hints. A ChalametPIR native comparator retrieved 16,384 synthetic 1,024-byte values successfully: setup download about 5.46 MB, query 81,928 bytes, reply 3,084 bytes, online server 0.34–0.73 ms, but retained public matrix alone about 139 MiB. Its pinned browser feature uses a default noncryptographic, zero-seeded RNG for secret/error sampling; **do not adopt that browser implementation as shipped**. None of these alternatives is a production dependency. Poulpy/Respire remain untested alternatives for Plumb.

Primary sources: [Spiral SDK](https://github.com/blyssprivacy/sdk), [official v1 profile](https://github.com/blyssprivacy/sdk/blob/fdb7206517c249603b0c91b65f1f29f95272107c/e2e-tests/params/v1.json), [noise estimator](https://github.com/blyssprivacy/sdk/blob/fdb7206517c249603b0c91b65f1f29f95272107c/lib/spiral-rs/src/noise_estimate.rs), [Brave FrodoPIR](https://github.com/brave-experiments/frodo-pir), [Chalamet affected sampler](https://github.com/itzmeanjan/ChalametPIR/blob/448698f7c314fd4eb36e889f6a6ec7fba64db03d/chalametpir_common/src/matrix.rs#L580), [tinyrand source](https://docs.rs/crate/tinyrand/0.5.0/source/src/wyrand.rs).

## Concrete next implementation steps

1. Pull main, inspect the storage report on the HPC, and rerun the representative probe there. Decide a practical fixed row/page layout from the real maximum and distribution, including proof overhead. Public size classes, bucket-specific shards, overflow counts or page URLs can disclose the selector. Use one complete snapshot per independently chosen server, with a global fixed pages-per-bucket policy; never silently truncate oversized buckets.
2. Build an immutable authenticated snapshot. The existing `BucketTable` freezes raw `SiteRecord`s; native `lookup` dynamically attaches crawl proofs from `BatchStore`. Freeze complete `BucketRecord` envelopes and proof material. Commit to all row bytes, key-mapping version, protocol/profile, dimensions, snapshot identity and validity policy. The current browser table name hashes only `buckets.idx`, so it is insufficient. Carry row authentication inside private retrieval or download a complete public commitment table; do not fetch row-specific proof URLs.
3. Add explicit strict PIR policy independent of `round_every`. Integrate retrieval at `background_rounds` / `search::background_round_for`; preserve `NetHandle::search`'s local cache, queue and ranking. Server selection must be independent of bucket/query, unlike existing bucket routing. Use a static endpoint with encrypted selectors and fixed wire shapes. On errors, timeout, disabled rounds, absent manifests or snapshot rotation, never fall back to plaintext bucket requests. Keep any address-anonymous policy separate and fail closed when its relay is unavailable.
4. Namespace caches and in-flight work by source identity, protocol/profile digest and snapshot commitment. Rotation must atomically switch active context and pending generation; old workers may only populate their old namespace. Enforce one total retention budget across all namespaces, rather than another 512 MiB for every version.
5. Preserve existing proof validation, crawler agreement and expiry. Authenticated snapshot membership does not make the operator truthful or negative results globally complete. Bound request/response/snapshot sizes, CPU, concurrency and setup lifecycle. A timed-out blocking computation must retain its worker permit until it finishes.
6. Prevent secondary disclosures. Public `RoundStatus.bytes_fetched` currently counts selected plaintext record lengths; PIR status should count fixed wire bytes or omit that field. Keep favicon access local/data URLs, avoid selected-result telemetry/proof requests, and review popularity reporting separately. Browser WASM is supplied by the serving site; an adversarial site can replace its client. An independently distributed native client is the first integration target.

Required regression checks: real/dummy/repeated/empty buckets have constant shapes/counts; a spy legacy handler sees zero calls in strict PIR even through errors and rotation; searches cannot advance scheduled slots; bad manifests/parameters/wrong rows/altered responses/oversized inputs fail; namespace budgets survive rotation/restart; honest/forged/expired/agreeing crawl proofs keep their current semantics; URLs, logs, public status and telemetry contain no selected IDs, query text or decoded size signals.

## Other open privacy/security work

Read `docs/reviews/privacy-security.md`, `docs/reviews/architecture.md` and their diagrams. The existing offline inference probe reduced a synthetic `us bank` search to one candidate out of its 148-entry test dictionary; that is a demonstration, not a general recovery-rate measurement. Browser silent fallback, browser proof verification, popularity disclosure, history retention and dependency findings remain open. Existing relay encryption hides bucket contents from the relay while the answering peer still sees bucket IDs; the native scheduler/cache changes did not fix that.

No PIR is integrated or deployed, no starter snapshot download exists, no new Mac app bundle was produced for these commits, and no live-fleet behavior was verified. The earlier Mac build predates these privacy changes. Build a fresh desktop artifact from the final main commit before evaluating these changes in the GUI.
