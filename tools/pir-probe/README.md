# Offline single-server PIR probe

This research tool exercises real Spiral encryption, serialized server processing and client decoding entirely offline. It does not add PIR to Plumb's app, browser, peer protocols or deployed nodes. Its server receives public setup and encrypted selector bytes, with no plaintext row selector. The client compares the recovered row against its local fixture for a correctness benchmark; this is not a deployed private client.

The crate has its own workspace and lockfile, isolating research dependencies from shipped binaries. `spiral-rs =0.2.1-alpha.2` is pinned to the published crate, whose VCS metadata records SDK revision `56700d24cb93039eec74194a879b4811d44afe97`. Core sources match the reviewed SDK revision `fdb7206517c249603b0c91b65f1f29f95272107c`. The SDK states it has not been security reviewed. Passing this probe does not establish cryptographic security or production readiness.

From the repository root:

```sh
cargo test --release --locked --manifest-path tools/pir-probe/Cargo.toml
cargo run --release --locked --manifest-path tools/pir-probe/Cargo.toml -- --queries 5
```

The default is the upstream `get_fast_expansion_testing_params` profile: 256 rows, 8 KiB each. The second profile is the exact SDK `e2e-tests/params/v1.json` tuple, copied into `profiles/upstream-v1.json`: 16,384 rows, 32 KiB each. It requires 512 MiB of raw data plus 4 GiB of preprocessed server data, with additional query scratch. It intentionally exceeds the default allocation cap:

```sh
cargo run --release --locked --manifest-path tools/pir-probe/Cargo.toml -- \
  --profile upstream-16k --max-database-mib 4608 --queries 3
```

`--database FILE` accepts a regular fixed-row fixture of exactly the selected profile's raw size. Each row holds an 8-byte little-endian payload length, payload bytes and zero padding. Oversized payloads fail instead of truncating. Without this option, the tool generates synthetic JSON rows. No payloads, selected indices, query text or keys appear in the JSON report. The tool freezes file bytes before preprocessing. On Unix it rejects symlinks/FIFOs and uses nonblocking opens; it cannot wait indefinitely for a FIFO writer.

Requests concatenate a fixed-size setup object and encrypted query. Setup is included in every measured request; a future protocol could amortize it, but that lifecycle has not been implemented here. The server checks exact lengths and coefficient ranges before upstream deserialization. The client checks exact response length and the upstream decoder's extra zero safety tail. Both pinned profiles use little-endian native upstream serialization; this tool refuses big-endian hosts. These checks do not constitute a general audit of the upstream parser or arithmetic. No HTTP server is exposed.

Tests cover binary/empty rows, oversized/malformed framing, input file types, allocation refusal, actual retrievals across the database, constant request/response sizes and fresh randomness for repeated selectors. These are integration and correctness checks, not proofs of selector privacy. CI runs the small profile only.

Measurements are in [docs/reviews/measurements](../../docs/reviews/measurements); the design direction is in [cache-first-search.md](../../docs/cache-first-search.md).

Primary sources: [published crate](https://crates.io/crates/spiral-rs/0.2.1-alpha.2), [SDK and security-review status](https://github.com/blyssprivacy/sdk), [official v1 profile](https://github.com/blyssprivacy/sdk/blob/fdb7206517c249603b0c91b65f1f29f95272107c/e2e-tests/params/v1.json), [Spiral paper](https://eprint.iacr.org/2022/368).
