# Canonical papers and bounded recent ingestion integration

This patch starts at `e33582cd8e30c5c288629b58f68348e56c9e91d7`. It owns scholarly ingestion and metadata; it does not modify `plumb-node/src/fetch.rs` or publish a dataset.

## Publisher contract (required)

`paper_names::improve(client, &mut papers, cache)` now verifies arXiv `1706.03762` and `2005.11401` before consulting Papers with Code. It returns an error when either required authoritative response is missing or inconsistent. Papers with Code failures remain supplementary. Its `Named` report adds `corrected` for existing primary arXiv rows; `redated` remains the audited Transformer DOI correction.

The ingestion worker must propagate an `improve` error, replacing the existing warn-and-write behavior, and call `paper_validation::validate_landmarks(&papers)?` before publishing the staged generation. An optional recent lane must also require `RecentFetched.complete`, reject/queue `Merged.conflicting_ids`, and then run the same landmark validation. Preserve the previous good generation on failure. This explicit handoff avoids overlapping the ingestion worker's cache/publication rewrite.

For a narrow existing-file repair without fetching the full methods archive, fetch `paper_validation::required_arxiv_ids()` with `paper_names::fetch_arxiv`, then call `paper_validation::repair_landmarks`. The returned metadata must first pass the fixed ID/title/first-author/first-submission checks; missing data causes an error. Network metadata is not replaced with embedded fixture metadata at runtime.

A `ConsistencyQueue` accepts an explicit caller-owned staged-generation key and at most 10,000 IDs. `verify_batch(..., budget)` verifies at most 100 identities per call. Persist corrected staged articles before calling `queue.save`; otherwise interruption could advance the queue without retaining corrections. `unresolved` is an explicit report and a retry input, not successful verification.

## Minimal core extension contract

`Article.paper: Option<plumb_core::papers::PaperMetadata>` is additive and defaults to absent. A profiles line carries `paper=<JSON with percent escaping of %, pipe, tab and newlines>`. The six primary columns are unchanged. Unknown keys are ignored by old readers. New readers ignore malformed/oversized paper extensions; writers reject invalid metadata before emitting the row.

The extension holds DOI, arXiv/OpenAlex IDs, bounded authors, strict publication day and raw provider date, original preprint submission day, separate record-version and arXiv-revision days, venue/source, count type and alternate URLs. Up to four correction snapshots preserve previous item/title/authors/dates/description. Conflicting old titles are removed from aliases/names when a primary arXiv identity is corrected.

Journal/preprint linking needs normalized title and first-author corroboration, including an initial/full-name variant. A year gap alone never replaces a journal DOI or publication date. The audited bad Transformer DOI is checked even when another canonical Transformer row is already present; an uncorroborated surviving audited DOI fails the generation gate. The narrowly audited `10.65215/2q58a426` Transformer copy can be replaced after corroboration. Same-title unrelated authors remain separate. Confirmed arXiv records reserve three bounded identifier aliases (raw ID, arXiv label and DOI) in the existing name index; at most five aliases are retained. PWC method-use counts carry `method_uses`, OpenAlex counts carry `citations`, and legacy/unavailable counts carry `unknown`.

The rich-docs worker may add its own optional fields and profile keys alongside `paper`; retain both parse/write/attachment paths. Full `Article` literals need the new field or `..Article::default()`. The extra one-line changes in existing ingestion constructors are compilation compatibility only. The index `Page::from_paper` currently does not carry this extension; date filtering/rendering requires a separately coordinated index/node change and is not claimed by this ingestion slice.

## Recent lane contract (opt-in)

`recent_papers::RecentOptions::ending(explicit_end_date, days)` defaults to a 90-day window, 50,000 records and 1,000 requests per invocation. Bounds are 366 days, 50,000 records and 1,000 requests. Thirty-day bands and the four OpenAlex primary-topic domains receive reserved quotas; unused quotas do not spill into another domain. No citation floor applies. Unknown or out-of-window dates and wrong-domain responses are rejected before record caps.

`fetch_recent_papers` uses supported `from_publication_date`, `to_publication_date`, `primary_topic.domain.id`, `publication_date:desc`, `per_page<=100`, and cursor paging. The request timeout is 30 seconds; HTTP 429/5xx returns an incomplete resumable report without retrying indefinitely. Its cache key includes fetch version, exact filter/window/domain/quotas, sort, selected fields, page size and endpoint. Cursor/record checkpoints discard uncommitted appended rows after interruption. Citation-lane caches are separate and are invalidated once to obtain structural metadata.

The endpoint cannot exceed requested page size or a streamed 4 MiB response budget; its raw cache is capped at 1 GiB. `merge_recent` deduplicates by DOI/OpenAlex identity; a shared title is not an identity. It retains verified established records and reports differing titles on a shared ID as conflicts. Publication-date coverage is not a provider update feed: backdated works may lie outside the lane. No paid created/updated filters, abstracts or query-triggered provider requests are added.

Primary provider contracts were checked against [OpenAlex filtering](https://help.openalex.org/api/filtering/), [paging](https://help.openalex.org/api/paging/) and [sorting](https://help.openalex.org/api/sorting/). Current paging guidance supports 100 rows and marks 200 as deprecated. A single unauthenticated live date/domain/cursor probe returned HTTP 429 on October 9, 2026 local time; no successful live cursor traversal or coverage is claimed. Mock HTTP tests verify requests, cursor resume, filters, zero-citation retention, rejection, quotas, cache invalidation and refusal handling. Before enabling the lane, perform a bounded successful contract probe using the deployment's existing access; do not expand paid API usage.

Author/date acceptance fixtures use the original [Transformer](https://arxiv.org/abs/1706.03762) and [RAG](https://arxiv.org/abs/2005.11401) arXiv metadata. No production service, corpus or cache was changed. A full staged corpus repair and combined search evaluation remain integration operations. Measure extension size and ingestion memory before promotion; `SetInfo` still carries its old approximate paper size. Explicit CLI/JSON/MCP date constraints and coverage under small index-profile caps remain work for the index/node integration. `merge_recent` orders by existing citation popularity, so a truncated profile needs a recent-lane reservation to retain zero-citation records.

Focused checks use an isolated Cargo target directory. A shared target produced a foreign `plumb-core` schema artifact (the rich-docs `search` field), so that failed run is not validation. Subsequent checks use the papers target with the parent's bounded settings: one build job, incremental off and debug information off. The shared target was not cleaned or deleted.

## Focused validation

Passed on this worktree, in the isolated papers target with one job, incremental compilation disabled and debug information disabled:

- `cargo test -p plumb-core -p plumb-ingest paper --no-default-features`: 23 checks.
- `cargo test -p plumb-core article::tests`: 12 serialization checks.
- `cargo test -p plumb-ingest openalex::tests`: 4 provider/conversion checks.
- `cargo clippy -p plumb-core -p plumb-ingest --all-targets -- -D warnings`.
- `cargo fmt --all --check` and `git diff --check`.

The test commands overlap; their counts are not a sum of unique tests. No expensive search/model evaluation was run.
