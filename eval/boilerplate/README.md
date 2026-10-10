# Homepage boilerplate recovery

This recovers PR #325 on main `786eb55`, preserving the two original commits' authorship. The extraction conflict keeps current main's section-heading spacing and the proposed block flushing. The follow-up narrows destructive rules: short facts, phone numbers, counts, braces in prose, and descriptions of policy or JavaScript products remain.

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
