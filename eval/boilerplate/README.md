# Homepage boilerplate recovery

This recovers PR #325 on merged main `9fe493b`, preserving the two original commits' authorship. The extraction conflict keeps current main's section-heading spacing and the proposed block flushing. The follow-up narrows destructive rules: short facts, phone numbers, counts, braces in prose, and descriptions of policy or JavaScript products remain.

`fixtures.json` contains 21 small synthetic HTML pages with explicit phrases that must stay or disappear. The crawler's tests read these fixtures through the real streaming extractor. They cover notices, plain-div link menus, repeated blocks, inline formatting, headings, structured names, brief business details, programming/policy products, mathematical prose, and Chinese, Thai and Arabic text.

Build a saved-page extraction probe:

```sh
cargo build --locked --offline -p plumb-crawl --example boilerplate_probe
target/debug/examples/boilerplate_probe < eval/boilerplate/fixtures.json > candidate.jsonl
```

Run the identical probe against the same HTML on main and on the unrefined recovery. Compare `page`, `body` and metadata fields. Do not refetch pages for each variant.

## Bounded comparison

The synthetic comparison used main `786eb55`, unrefined recovery `b37b4b4`, and the conservative follow-up. Main retained every required useful phrase but leaked four unwanted phrases. The original recovery lost ten required phrases across eight fixtures; the follow-up lost none and leaked none. Extracted whitespace word totals were 232, 123 and 184 respectively. These are extraction controls, not relevance scores.

Seven public pages were captured once with a 512 KiB maximum and a 12-second timeout per request. Eight were attempted; Home Depot returned HTTP 403. The JSON manifest records final URLs, response sizes, capture time and SHA-256 checksums. No model or paid API was called.

| Saved page | Main words | Original recovery | Conservative recovery |
| --- | ---: | ---: | ---: |
| Rust | 387 | 344 | 387 |
| Python | 344 | 0 | 344 |
| Wikipedia | 515 | 441 | 493 |
| MDN JavaScript | 801 | 613 | 801 |
| Termly | 439 | 750 | 883 |
| Khan Academy | 35 | 35 | 35 |
| Domino's | 0 | 0 | 0 |

The original patch removes Python's entire extracted body. The follow-up preserves it and MDN's programming prose. Termly has fewer early menu words, allowing later explanatory and testimonial text into the bounded extraction window. Wikipedia removes repeated text and separator-only blocks. The inspected title, description, heading, section and structured-name fields were unchanged on all seven pages. Domino's is a JavaScript-heavy empty-body control, not a successful extraction case. Khan Academy's small server response is weak evidence for its normal homepage. Lower word counts alone are not a quality win.

## Validation and limits

The conservative recovery passes 124 plumb-crawl tests, crate Clippy with all targets and warnings denied, workspace formatting and diff checks. Loopback crawler tests require permission to bind local sockets. The fixture and live A/B outputs are retained with the review evidence outside source control.

The sample is small, manually chosen and has no ranking/index/embedding A/B. Rules remain heuristic: plain-div menus can also be useful directories, notice-like prose can be misclassified, and action labels remain fallback text on empty pages. Crawl version 2 requests fresh homepages best-known first and will change indexed text and embeddings as those records arrive. The active quality batch modifies extraction too; integrate on its eventual head, preserve rich extraction and sections, rerun checks and measure retrieval before release. Do not merge this recovery directly while that batch is active.

## Combined main comparison

The recovery was replayed on main `9fe493b062fa2e1c41c324cefe2228b797436865` after PR #416 merged. Only the changelog needed conflict resolution. Rich docs extraction, section spacing, language metadata fallback and docs extractor version 2 remain. The block filter changes compact text; the rich docs collector sees the original token stream independently.

The same 21 fixtures and seven previously saved public pages were extracted by main and the combined candidate. All required fixture phrases remained and all four unwanted phrases were removed. Rich docs payloads were identical for all 28 pages, as were inspected titles, descriptions, headings, sections and structured names on the seven public pages. The public pages' page-text word counts were unchanged from the earlier conservative comparison above. These counts describe `page_text`, the input for term selection; `body_text`, kept for embeddings, is shorter.

`retrieval-queries.json` freezes 33 probes before comparison: 21 synthetic extraction controls plus 12 public-page queries. Synthetic controls have distinct neutral test domains so they can coexist in one index. The example copies extracted title, description, aliases, language, headings, body and terms into records, with equal default popularity and no graph signals. Both snapshots are ranked by the same merged-main Tantivy engine with exact-query options and no embeddings. This measures a small controlled corpus, not production relevance or a held-out benchmark. Short body-only and non-Latin synthetic controls often have no lexical result because body text is used for embeddings rather than directly searched.

Reproduce after building the saved-page probe and extraction outputs:

```sh
cargo build --locked --offline -p plumb-index --example boilerplate_retrieval
cat fixtures.jsonl live.jsonl | target/debug/examples/boilerplate_retrieval \
  eval/boilerplate/retrieval-queries.json > retrieval.jsonl
```

Default ranking (terms boost 1) and a diagnostic terms-disabled run are reported separately. At default settings, both snapshots find the labelled page first in 10 of 33 probes, with top-3/top-10 also 10 and MRR 0.303. No labelled rank worsens. The only changed default result list removes Termly from the vague synthetic query `platform helps`; neither snapshot finds that query's synthetic expected page. Unmatched controls and this small, fixture-derived sample limit the conclusion: no measured lexical regression, no established relevance improvement.

The crawler/index combined tests include the batch's exact-symbol, late-passage, language, hidden-markup, cache-version and ranking tests. Crawl version 2 still causes fresh homepage records and embeddings to replace older ones gradually. No embedding model, network fetch, paid API, HPC job, production node or live desktop app was used in this second comparison. A broader real-corpus semantic evaluation remains a release consideration.
