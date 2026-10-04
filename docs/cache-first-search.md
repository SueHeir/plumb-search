# Cache-first search: existing behavior and changes

The desktop already searches its local index, and native network search already reuses a bucket cache. This work extends those existing components; it does not introduce a second search engine or deploy servers.

## Native searches and scheduled downloads

With background rounds enabled, network searches read retained buckets locally and queue missing or overdue buckets. Searches do not start a network round, advance a deadline or skip a later round. The background task selects queued buckets at its next scheduled slot and fills the remainder with background buckets. Queue entries contain bucket numbers, not queries, and are not persisted or exposed through public status.

The cadence remains 10 minutes for a server and 30 minutes for the desktop by default, now at fixed deadlines. An in-flight round is allowed to finish; missed slots are skipped without catch-up bursts. Peer availability, network failures and response size classes still affect observable traffic. These changes remove search-triggered request bursts; they do not establish complete traffic-analysis resistance.

An uncached search can wait within its request timeout for a round. If needed buckets are still absent, the page says more results are waiting for a background download. Saved results are usable while overdue and are labeled as potentially out of date. Proofs are rechecked at the current time; retaining data does not extend a signature's validity. Empty bucket answers are cached too, so an unsuccessful search does not repeatedly fetch the same bucket.

`--round-minutes 0` explicitly disables background rounds and retains immediate network retrieval. That setting does not hide when searches trigger requests. Normal hosted searches still send their query to the serving node. The browser `/private` client has its own fetch path and is not covered by this native scheduler change.

## Retention

The existing 12-hour freshness window becomes a refresh threshold rather than a deletion deadline. Buckets survive restarts until cleared, corrupted or evicted under storage limits: 4,096 buckets, 512 MiB total serialized data, and 16 MiB per entry, oldest fetch first. A failed or unanswered refresh preserves the previous data. Unix directories use mode 0700 and files use 0600 because the retained bucket set can reveal interests even without plaintext queries. Multiple processes must not share a cache directory.

## Measuring the working corpus

The read-only sizing command separates useful record payload from indexes, models, icons and peer data:

```sh
plumb storage --data /path/to/plumb-data --json > storage-report.json
```

Claude can run this on the HPC after updating the binary. It does not reconfigure, refresh or delete the node. The report contains aggregate byte counts, full/slim main-record sizes and bucket membership distributions. It excludes symlinks and special files, bounds malformed input, and does not open identity, token or history files. It measures logical lengths, not allocated blocks; records are sized without replaying their journal. A live directory is not an atomic snapshot, and an incomplete report exits with an error after showing its omissions.

These measurements will tell us what starter dataset and cache budget are practical. No claim that the reported 2 GB is all searchable client payload is made, and no HPC data was inspected from this workspace.

## Remaining bucket-privacy work

The existing sealed relay hides bucket contents from the relay, but the answering node decrypts the bucket number. Public bucket hashes can support dictionary inference. Scheduled retrieval and persistent caching do not solve this disclosure.

Private Information Retrieval is a separate protocol step. A two-server design requires replica operators that do not collude; several machines owned by one operator do not satisfy that assumption. A single-server design instead needs measured cryptographic computation and communication costs. Neither is installed or enabled by this change. A common starter snapshot also needs sizing and versioning before its download path can be implemented.

The subsequent [PIR handoff](reviews/pir-handoff.md) records the direct single-server design, an isolated offline Rust probe, synthetic measurements at 16,384 rows and the production integration checklist. The storage report now estimates equal-row padding from actual bucket metadata; this estimate excludes proof and cryptographic overhead.

See the [privacy review](reviews/privacy-security.md) and [architecture map](reviews/architecture.md) for the source evidence and remaining trust assumptions.

## Validation

On macOS, 666 workspace tests passed with three existing ignored tests, plus 13 desktop tests. Network integration tests exercised connected-peer misses sending no requests before a due slot, scheduled completion, retained stale data, empty-answer reuse, and normal proof expiry/forgery rejection. Formatting and clippy passed with warnings denied. The browser module passed WASM clippy, an optimized build and pinned wasm-bindgen processing; five matching node tests passed with the built script embedded. A synthetic CLI smoke check produced valid aggregate JSON without exposing record or history contents. No HPC or other deployed node was changed or scanned.
