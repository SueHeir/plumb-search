# PIR probe

An offline research tool that times single-server private information retrieval (PIR) with the Spiral scheme: real encryption, server processing and client decoding, all in one process. It measures whether a node could answer "give me row `i`" without learning `i`. For contributors working on [private search](../../docs/private-search.md) and [cache-first search](../../docs/cache-first-search.md).

It adds nothing to Plumb: no node, browser, app or peer protocol uses it, and it opens no network port. Passing it says nothing about cryptographic security.

## Run it

From the repository root:

```sh
cargo test --release --locked --manifest-path tools/pir-probe/Cargo.toml
cargo run --release --locked --manifest-path tools/pir-probe/Cargo.toml -- --queries 5
```

It prints a JSON report: the profile, row count and size, raw and preprocessed database bytes, setup, request and response bytes, the time to preprocess and set up, and for each retrieval the time to make the query, answer it and decode it. It never prints payloads, row numbers, query text or keys.

| Option | Default | What it does |
| --- | --- | --- |
| `--profile upstream-small\|upstream-16k` | `upstream-small` | The parameter set (below) |
| `--queries N` | 3 | Retrievals, 1 to 32, spread evenly over the rows, each with fresh randomness |
| `--max-database-mib N` | 256 | Refuse to start when raw plus preprocessed data would exceed this, 1 to 8192; query scratch space is extra |
| `--database FILE` | synthetic rows | Read rows from a file instead (below) |

## Profiles

Both are upstream test parameters, used unchanged:

| Profile | Source | Rows | Raw data | Preprocessed |
| --- | --- | --- | --- | --- |
| `upstream-small` | spiral-rs's `get_fast_expansion_testing_params` | 256 of 8 KiB | 2 MiB | 16 MiB |
| `upstream-16k` | The SDK's `e2e-tests/params/v1.json`, copied into [profiles/upstream-v1.json](profiles/upstream-v1.json) | 16,384 of 32 KiB | 512 MiB | 4 GiB |

`upstream-16k` needs more than the default cap allows:

```sh
cargo run --release --locked --manifest-path tools/pir-probe/Cargo.toml -- \
  --profile upstream-16k --max-database-mib 4608 --queries 3
```

## Rows from a file

`--database FILE` takes a regular file of exactly the profile's raw size: fixed rows, each an 8-byte little-endian payload length, the payload, then zero padding. A payload too big for its row is an error, never cut. The probe refuses symlinks and FIFOs and opens without blocking, and reads the whole file before it starts. [pir_sizing](../../crates/plumb-tools/examples/pir_sizing.rs) in plumb-tools writes such a file from a node's real buckets (`--probe-db`).

## What it checks

- Each request is a fixed-size public setup followed by the encrypted query; the server sees no row number. Setup is sent with every request.
- The server checks the exact lengths and that every coefficient is below the modulus before it hands bytes to the upstream deserializer. The client checks the exact response length and the zero tail the upstream decoder adds.
- Each decoded row must equal the row read directly, and request and response sizes must not change with the row asked for.
- The serialization is upstream's native little-endian form, so the probe refuses big-endian hosts.

These are correctness checks, not an audit of the upstream parser or a proof that row choice stays private. The tests cover binary and empty rows, bad framing, input file types, the allocation cap, round trips across the database, constant sizes and fresh randomness for the same row; they use the small profile only, as CI does.

## Dependencies

The probe is its own Cargo workspace with its own lockfile, so its research dependencies never reach the shipped binaries. It pins [spiral-rs](https://crates.io/crates/spiral-rs/0.2.1-alpha.2) at `=0.2.1-alpha.2`; see the [Blyss SDK](https://github.com/blyssprivacy/sdk) for that code's security-review status and the [Spiral paper](https://eprint.iacr.org/2022/368) for the scheme.

Measurements from a run are in [docs/reviews/measurements](../../docs/reviews/measurements).
