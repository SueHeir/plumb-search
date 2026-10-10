# Generic paper consistency and bounded recent ingestion

The generic consistency follow-up starts at parent `bb8b59a131b74ea4ac4dda19ae428b69e6333646`. It replaces the earlier case-specific repair/gate contract. It changes scholarly metadata, verification helpers and paper publisher/promotion callbacks; it does not modify query outputs or frozen evaluation labels.

## Publisher and verification contract

`paper_names::improve` selects existing arXiv identities from the input corpus, with at most 100 unique identities (one batch) per ordinary run. Corrections lacking source snapshots are selected first, then input order supplies the rest. It never fetches a curated pair merely because those papers are well known. Missing, malformed, duplicate or contradictory selected responses fail verification and retain the prior generation. Supplementary Papers with Code methods still have their separate bound of 1,000 missing arXiv identities and archive failures remain supplementary. The existing arXiv per-request/retry/pacing budgets are unchanged; a bounded caller-owned `ConsistencyQueue` supports larger audits.

The publisher propagates enrichment failures and runs `paper_validation::validate_against_baseline` both when staging and when promoting papers. Its baseline comes from the actual current destination, never from candidate claims or a saved success report. With no existing destination, duplicate-ID validation remains strict. This generic gate validates primary DOI/OpenAlex IDs, bounded metadata, publication date/year agreement, primary-preprint date agreement, linked ID agreement and source-snapshot title/author/submission/revision agreement. Corrections require an explicit source snapshot. It does not require any particular paper, title, DOI, author or year to exist in a corpus.

Metadata-absent legacy rows sharing a primary ID can contain unverified title/subtitle, author or imported-year variants. These are diagnostics, not source-proven contradictions. `consistency_baseline` records each such group's complete sorted multiset of record hashes, titles and descriptions. The gate preserves and reports the group only when every full record is unchanged, allowing corpus order to change. New, added, removed or changed variants fail; explicit metadata/source-snapshot variants never receive a legacy exception. No row is silently selected, dropped or canonicalized. A changed legacy group can resolve only when the same number of records all carry valid, mutually consistent snapshots for their primary arXiv ID. Linking a journal DOI to a preprint cannot resolve that DOI's publication claims.

This policy allows incremental repair around untouched legacy ambiguity without marking it verified. Ordinary method/title naming skips preserved groups. A source refresh that changes an ambiguous group must retain its original records or supply primary-source resolution; replacing the group's titles, date claims or even popularity fields does not silently pass. The staged manifest includes a `paper_consistency` report with preserved variants, resolved IDs and source-snapshot coverage. Promotion recomputes the gate from current bytes even if a checksum and old success report have been updated. `whole_corpus_verified` is always false: row consistency and non-regression do not prove every historical DOI or publication date. `validate_consistency` remains the strict no-baseline entry point.

Only an authoritative response for the record's primary arXiv DOI permits replacement of that record's title/authors/dates. Original primary IDs and conflicting values remain in bounded correction reports, and associated OpenAlex IDs/count meaning remain intact. A non-arXiv DOI is never replaced or redated just because a title or year gap resembles a preprint. A unique title plus compatible first author can link a preprint while retaining the primary journal DOI and independent publication/version dates. Different full given names sharing an initial do not match; initial/full-name variants remain compatible. Multiple matching source identities or conflicting explicit links preserve the original row and report unresolved identities. `Named.retained_publications` separately reports publication IDs whose independent DOI/date claims were retained; arXiv metadata cannot adjudicate those publication claims.

The formerly special `10.65215/2q58a426` record is now handled by these generic rules. Its DOI and publication year are preserved. Source-backed preprint linking can supply its separate submitted/revised dates, but it does not prove that the record's claimed publication date is false. No claim is made here that a live row or that DOI has been repaired by the new algorithm. The earlier scratch canary used the superseded DOI-specific conversion and is not validation evidence for this generic implementation; a new staged canary must preserve original hashes and report actual results.

A `ConsistencyQueue` accepts an explicit caller-owned staged-generation key and at most 10,000 IDs; each application verifies at most 100. Queue version 2 invalidates older checkpoints that lacked unique-response/source-snapshot verification. Missing or ambiguous responses remain in `unresolved` and can seed a retry queue. Persist corrected staged articles before saving queue progress; otherwise interruption can lose a correction. Publisher-owned quarantine/retain-previous-generation behavior is unchanged.

## Explicit optional canaries and curated assumptions

`PaperCanary` expectations are supplied by the caller. `canary_ids`, `repair_canary` and `validate_canary` support bounded optional canaries without loading expectations during ordinary enrichment/publication. The Transformer/RAG expectations reside only in `crates/plumb-ingest/tests/fixtures/paper-canary.json` and test Atom feeds. They are regression fixtures or explicit caller-selected canary data, not production algorithm switches. Missing or conflicting authoritative canary responses fail before repair; runtime code never substitutes fixture metadata for a provider response.

Title/first-author corroboration is a bibliographic linking assumption, not a cross-provider identity proof or a journal publication-date validator. Papers with Code is an optional curated method-name archive; its aliases must still corroborate actual paper identities/titles. No curated evaluation answers or labels are rewritten. No HPC query, build, service operation, corpus write, promotion or fresh provider probe was performed for this generalization.

## Offline scratch canary recipe

A reusable example performs no network requests and has no embedded paper IDs/titles:

```sh
cargo run --locked -p plumb-ingest --example paper_consistency_canary -- \
  INPUT.tsv.gz AUTHORITATIVE.xml NEW_OUTPUT_DIR [OPTIONAL_CANARY.json]
```

Use a new output directory with an existing parent. The input corpus and XML are read-only; an existing output directory is refused. Source XML is bounded to 4 MiB and 1,000 identities. The helper captures a legacy-variant baseline from the immutable caller-supplied input before applying any repair. It applies the generic repair/linking rules to whatever the XML actually establishes, reports corrected primary rows, added preprints, unresolved source identities, retained independent publication claims, before/after records and structured publication/preprint date coverage, and saves original/XML/candidate hashes. Row signatures count semantic changes independently of popularity-order changes. The baseline-aware record-consistency/non-regression gate and full serialization round-trip must pass, and unresolved source identities prevent an eligible candidate from being written. Optional expectation JSON uses the same baseline and supplies diagnostics only; it never selects a repair rule or overwrites source metadata. The report explicitly records zero network calls, no promotion and `whole_corpus_verified: false`. A successful helper result is `validated_scoped_scratch_candidate`; it may still contain fully reported, unresolved legacy variants.

Parent may use its immutable `baseline-snapshot/pages/sets/papers.tsv.gz` and the cached `papers-repair-canary-09842dd/authoritative-arxiv.xml` in HPC scratch after a clean integrated build. No second provider request is needed. Parent's initial strict-gate offline run exposed untouched legacy title/subtitle/date variants; that failure is not proof that the variants contradict an authoritative source. The baseline-aware follow-up has only been tested against synthetic local temp files here; its real offline corpus canary remains an integration operation. A linked journal publication may remain internally consistent while its independent publication date is unresolved; the helper lists retained publication IDs rather than pretending those dates were corrected.

## Minimal core extension contract

`Article.paper: Option<plumb_core::papers::PaperMetadata>` is additive and defaults to absent. A profiles line carries `paper=<JSON with percent escaping of %, pipe, tab and newlines>`. The six primary columns are unchanged. Unknown keys are ignored by old readers. New readers ignore malformed/oversized paper extensions; writers reject invalid metadata before emitting the row.

The extension holds DOI, arXiv/OpenAlex IDs, bounded authors, strict publication day and raw provider date, original preprint submission day, separate record-version and arXiv-revision days, venue/source, count type and alternate URLs. `verified_arxiv` is an optional bounded source snapshot (ID/title/authors/submission/revision) populated from an explicit authoritative response, with no embedded named-paper defaults. It remains inside the existing 16 KiB extension bound. Legacy metadata defaults to no snapshot. Up to four correction snapshots preserve prior primary IDs/title/authors/dates/description. Prior unrelated titles are removed from search aliases when a primary arXiv record is authoritatively corrected.

Confirmed arXiv records reserve three identifier aliases (raw ID, arXiv label and DOI), with at most five aliases total. PWC method-use counts carry `method_uses`, OpenAlex counts carry `citations`, and legacy/unavailable counts carry `unknown`. Journal rows retain their original source metadata and count meaning when linked to a preprint.

The rich-docs worker may add its own optional fields and profile keys alongside `paper`; retain both parse/write/attachment paths. Full `Article` literals need the new field or `..Article::default()`. The extra one-line changes in existing ingestion constructors are compilation compatibility only. The separate paper-search follow-up carries this extension through `Page::from_paper`, retaining the integrated parent `Page.search` and `Page.content_language` fields.

## Recent lane contract (opt-in)

`recent_papers::RecentOptions::ending(explicit_end_date, days)` defaults to a 90-day window, 50,000 records and 1,000 requests per invocation. Bounds are 366 days, 50,000 records and 1,000 requests. Thirty-day bands and the four OpenAlex primary-topic domains receive reserved quotas; unused quotas do not spill into another domain. No citation floor applies. Unknown or out-of-window dates and wrong-domain responses are rejected before record caps.

`fetch_recent_papers` uses supported `from_publication_date`, `to_publication_date`, `primary_topic.domain.id`, `publication_date:desc`, `per_page<=100`, and cursor paging. The request timeout is 30 seconds; HTTP 429/5xx returns an incomplete resumable report without retrying indefinitely. Its cache key includes fetch version, exact filter/window/domain/quotas, sort, selected fields, page size and endpoint. Cursor/record checkpoints discard uncommitted appended rows after interruption. Citation-lane caches are separate and are invalidated once to obtain structural metadata.

The endpoint cannot exceed requested page size or a streamed 4 MiB response budget; its raw cache is capped at 1 GiB. `merge_recent` deduplicates by DOI/OpenAlex identity; a shared title is not an identity. It retains verified established records and reports differing titles on a shared ID as conflicts. Publication-date coverage is not a provider update feed: backdated works may lie outside the lane. No paid created/updated filters, abstracts or query-triggered provider requests are added.

Primary provider contracts were checked against [OpenAlex filtering](https://help.openalex.org/api/filtering/), [paging](https://help.openalex.org/api/paging/) and [sorting](https://help.openalex.org/api/sorting/). Current paging guidance supports 100 rows and marks 200 as deprecated. A single unauthenticated live date/domain/cursor probe returned HTTP 429 on October 9, 2026 local time; no successful live cursor traversal or coverage is claimed. Mock HTTP tests verify requests, cursor resume, filters, zero-citation retention, rejection, quotas, cache invalidation and refusal handling. Before enabling the lane, perform a bounded successful contract probe using the deployment's existing access; do not expand paid API usage.

Author/date acceptance fixtures use the original [Transformer](https://arxiv.org/abs/1706.03762) and [RAG](https://arxiv.org/abs/2005.11401) arXiv metadata. No production service, corpus or cache was changed. A full staged corpus repair and combined search evaluation remain integration operations. Measure extension size and ingestion memory before promotion; `SetInfo` still carries its old approximate paper size. The separate follow-up implements CLI/JSON/MCP publication constraints; retaining recent coverage under small index-profile caps remains an integration decision. `merge_recent` orders by existing citation popularity, so a truncated profile needs a recent-lane reservation to retain zero-citation records.

Focused checks use an isolated Cargo target directory. A shared target produced a foreign `plumb-core` schema artifact (the rich-docs `search` field), so that failed run is not validation. Subsequent checks use the papers target with the parent's bounded settings: one build job, incremental off and debug information off. The shared target was not cleaned or deleted.

## Generic consistency focused validation

The baseline-aware follow-up passed locally with the same one-job settings:

- `cargo test -p plumb-ingest paper --no-default-features`: 32 ingest tests.
- `cargo test -p plumb-node paper --no-default-features`: 13 node tests, including legacy-variant diagnostics and promotion revalidation after a mutated date claim with an updated checksum.
- `cargo test -p plumb-node whole_set_hook --no-default-features`: 1 semantic publication/DOI round-trip test.
- `cargo test -p plumb-ingest --example paper_consistency_canary`: 1 offline synthetic test, now including untouched title/subtitle/date variants alongside primary repair and journal linking.
- `cargo clippy -p plumb-ingest --all-targets -- -D warnings`, formatting and whitespace checks.

Arbitrary fixtures cover new, added, removed and changed legacy variants; explicit metadata and individually valid but conflicting source snapshots; full-author conflicts hidden behind unknown/initial bylines; and positive resolution of every legacy primary-preprint variant from authoritative metadata. No literal DOI/title exception was introduced. No HPC operation, production change or frozen evaluation-label edit was performed for this follow-up.

The generic follow-up passed locally with one Cargo build job, incremental compilation disabled, debug information disabled and the isolated `plumb-search-quality-20261009-papers-target` directory:

- `cargo test -p plumb-core -p plumb-ingest paper --no-default-features`: 6 core and 28 ingest tests.
- `cargo test -p plumb-node paper --no-default-features`: 12 node tests, including retain-previous-generation and generic promotion checks.
- `cargo test -p plumb-node whole_set_hook --no-default-features`: 1 semantic publication/DOI round-trip test.
- `cargo test -p plumb-ingest --example paper_consistency_canary`: 1 offline synthetic repair/linking/round-trip test, including refusal to overwrite an existing output directory.
- `cargo clippy -p plumb-core -p plumb-ingest --all-targets -- -D warnings`, `cargo fmt --all --check` and `git diff --check`.

Tests cover unseen primary identities, independent later journal dates, full-name collisions, ambiguous multi-preprint matches, malformed/duplicate responses, conflicting explicit IDs, source-snapshot round-trips and bounded queue selection. Production repair/validation code contains no Transformer/RAG IDs or titles; named expectations are explicit fixtures only. No new live-provider probe, HPC operation, full-corpus canary, search evaluation, frozen-label change or production promotion was performed. These commands overlap and their counts are not a sum of unique tests.

## Historical focused validation

The earlier ingestion/search commits passed on this worktree, in the isolated papers target with one job, incremental compilation disabled and debug information disabled:

- `cargo test -p plumb-core -p plumb-ingest paper --no-default-features`: 23 checks.
- `cargo test -p plumb-core article::tests`: 12 serialization checks.
- `cargo test -p plumb-ingest openalex::tests`: 4 provider/conversion checks.
- `cargo clippy -p plumb-core -p plumb-ingest --all-targets -- -D warnings`.
- `cargo fmt --all --check` and `git diff --check`.

The test commands overlap; their counts are not a sum of unique tests. No expensive search/model evaluation was run.

## Paper search follow-up

The ingestion implementation is pinned at `820c47044e1d8691d41233aa9bddf1bf7c1a4697`. The follow-up is developed against parent integration commit `38152dd014fa76bd7d39af8fcbb3081c0e3934af`; its merge into this worker branch only aligns dependencies. Parent should cherry-pick the ingestion and follow-up commits, not that dependency merge.

`Page.paper` is optional in stored JSON and contains the same scholarly metadata. Publication year/day are separate numeric indexed/fast fields. Missing dates have no date value, so they cannot satisfy hard constraints. Paper, site and content-language eligibility apply before candidate limits. A reserved name collector preserves canonical exact title/identifier matches. Newest requests also collect by publication day/year before the limit, then order within exact/title/byline relevance tiers, followed by popularity and deterministic URL order. This bounded topical route uses title/aliases/authors; it does not fetch providers at query time or index abstracts.

`after:YYYY`, `before:YYYY`, `after:YYYY-MM-DD`, `before:YYYY-MM-DD` use inclusive publication bounds. A year-only record can satisfy year bounds, but cannot satisfy a day bound. Publication dates describe the record's publication; preprint submission/revisions do not stand in for them. Malformed, conflicting or reversed bounds fail rather than broadening the query. An unquoted literal `papers published in YYYY` or `research published in YYYY` supplies a year window. Quoted titles, ordinary title years and software versions stay lexical. `sort:newest` changes ordering within relevance tiers; `sort:relevance` selects the paper route while keeping relevance/popularity order.

CLI: `plumb search --index PAGE_INDEX --paper --after 2025 --before 2026-09-30 --order newest transformer`, with `--json` returning paper hits and actual indexed date coverage. These commands only read an existing local page index. HTML and JSON: `/search` or `/api/search?q=transformer&kind=paper&after=2025&before=2026-09-30&order=newest`. Inline operators use the same validation. MCP `search` accepts `kind=paper`, `after`, `before`, `order`; fields and inline operators share the core parser. Supplying another explicit kind with date/order fields fails. JSON/MCP return structured paper dates and count types; HTML/CLI/MCP text label publication, preprint, record version and preprint revision separately. Legacy metadata has unknown publication/count type, not fabricated citations.

The page index schema version is `v6-paper-dates-rich-symbol-docs-scope`. The parent already uses this version in `Wanted::key`, so a rebuild selects a separate index generation and does not overwrite the old index's path. Rebuilding a scratch/local index is necessary to activate numeric date fields; no live corpus or service changes are part of this patch. The follow-up also corrects malformed language/scope wrapper signatures in the pinned parent page-search snapshot without changing their intended scope.


Follow-up focused validation passed: `cargo test -p plumb-core -p plumb-index paper --no-default-features` (5 core and 5 index checks) and `cargo test -p plumb-node paper --no-default-features` (6 node checks), plus formatting and whitespace checks. The node build has a pre-existing unused `plumb_index::Hit` import warning in `web/places.rs`; this patch does not alter it. These commands do not run a full search/model evaluation. Parent already repaired its query wrappers in `42cf9c0` and Page.search fixtures in `bf8dcac`; reconcile those identical compatibility edits when applying this commit. Parent's rich-symbol schema in `80a13c8` uses v5, so the combined paper/date schema is deliberately v6.
