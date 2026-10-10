# Search quality release results — October 10, 2026

## Scope and decision

This release combines scoped docs/symbol retrieval, bounded source passages, owner-evidence identity tools, direct entity facts, shared places/news assembly, language filtering before candidate caps, precise paper-date APIs, safe refresh/cache publication, provider identity validation, build attestation and the existing HPC memory fixes.

The experimental whole-query promotion is retained for diagnostics and opt-in development but disabled by default in native and private search. The final subject-guard run improved many cases yet dropped a present passport destination and substantially demoted several established owners. The bounded release uses the conservative ranking path plus a generic retention correction. No frozen labels were changed, and query/host/DOI-specific exceptions were not restored.

## Controlled evaluation

Baseline: `5b96a35f60ca028d5f2b1b75972f67fb16d93def`. Both sides use the same immutable source snapshot, 2,789,083 sites, 1,306,584 pages under the same per-set cap, embedding model, vectors, Split instruction, rank JSON, fixed clock, options and labels. Jobs, findings, plugins, external search and personalization are disabled. The 959 cases comprise 751 legacy and 208 candidate observations; these exploratory suites were used during diagnosis and are not an untouched, independently adjudicated accuracy benchmark.

The held `b5258b4` run gained 22 acceptance cases and lost one. Its legacy AI MRR delta was −0.02190, with a family-bootstrap 95% interval of [−0.03915, −0.00594]. A top-100 acceptance window concealed serious rank-one demotions. This is the reason for excluding the experimental promotion, rather than requiring every metric to improve monotonically.

The final conservative subset is `96198f58100b88f8e189a12496637e4c40d87cf0` (identical ranking files to worker commit `faa2d154`). Its 959-case run gained **14 acceptance cases with zero pass-to-fail losses** and no manual-regression flags. Legacy AI MRR delta is +0.001147, interval [−0.008036, +0.009642], with unchanged top-one rate and two additional passes. Legacy brand acceptance and top-three rate are unchanged; top-one falls by five of 388 queries and MRR by 0.008591, interval [−0.014605, −0.003007]. This small ordering tradeoff is reported rather than hidden. The experiment's ambiguous wrong-brand improvement is deferred: default-mode wrong-brand-top-three counts are unchanged from baseline.

| Candidate family | Pass delta | MRR delta |
| --- | ---: | ---: |
| Audit (8 cases) | +5 | +0.44167 |
| Navigation (20) | 0 | 0 |
| Ambiguous (20) | 0 | −0.00833 |
| Troubleshooting (20) | +1 | +0.05452 |
| Practical (20) | +1 | +0.05 |
| Facts (legacy diagnostic, 20) | 0 | unavailable |
| Research (20) | +2 | +0.10 |
| Dates (20) | +2 | +0.13333 |
| Languages (20) | 0 | 0 |
| Places (page-geography diagnostic, 20) | 0 | 0 |
| Docs (20) | +1 | +0.025 |

The final run's evaluator p95 was 222.30 ms versus 240.56 ms for baseline. That single-run comparison is not production latency evidence; exact-release HTTP/MCP performance is measured separately. The old 1,000-job LLM evaluation remains paused; these checks do not invoke paid model grading.

## Serving evidence

The final conservative subset's completed 105-case MCP appendix removed the incorrect NPS and Anthropic official claims. Conservative official-site coverage is 39/85 legacy cases; no demonstrated wrong-owner medium/high assertion remains in that reviewed set. The only strict false-official row selects Skyfield's legitimate project repository instead of its expected homepage. Lookalike coverage is 9/11 with no definitive false accusations; a suspected PayPal tenant remains a frozen-label miss. These results were reproduced with experimental promotion disabled in the final serving configuration.

Direct Japan population returns the counted year and Wikidata provenance. `tomllib` resolves to Python documentation and `padStart` to MDN. The exact React Hooks page remains fifth. Date-constrained paper requests explicitly report missing indexed publication precision instead of treating a preprint date or unverified journal claim as a verified publication date.

## Data and limitations

Code activation does not refresh source generations. The production paper file remains unchanged at 199,982 records, and its six missing primary identifiers still prevent full-corpus repair promotion. A separate 199,976-record valid-primary scratch canary passed primary-source consistency and serialization checks using cached XML; all 143 historical variant groups were preserved, one primary arXiv title was corrected and two records gained source/preprint linkage. An independent journal DOI/date claim remains unresolved. No canary corpus was promoted and `whole_corpus_verified` remains false.

The core evaluation does not load the full 24-million-place index. A separate manual Denver fixture contains 33 real OSM records with source hashes and provenance. Full places behavior requires deployed checks. More source coverage is needed for late-page API symbols, current papers and missing product/modpack pages. Curated source catalogues and fixed tool/health routing remain explicit policies; inserted homepages are not proof of task fulfillment.

## Software and operations

At final code `96198f5`, 1,696 workspace tests passed with five ignored; strict workspace Clippy and formatting passed, along with three MCP example tests, one paper helper test and ten Python contract/provider-identity tests. Focused native semantic-boundary controls, all 29 private tests and private WASM checks passed. Exact-head GitHub CI remains a separate merge check.

Main merge and HPC/Mac activation are separate milestones. Deployments preserve the old executable and consistent recoverable data outside startup index cleanup. Mac recovery must fit its 10 GB Plumb allocation, using validated nonsensitive recovery on existing HPC where needed while sensitive local state stays local. Cleanup reclaimed 111.41 GiB from verified inactive DIRT/SOIL/GRASS build trees without deleting source/Git/preserved outputs.

## Next priorities

1. General subject/task evidence and owner ranking under sparse metadata, with a new independently adjudicated holdout. Retain descriptive recall and explicit uncertainty before re-enabling experimental promotion.
2. Staged docs/reference and recent-paper coverage refresh; recover or quarantine missing historical identities with provenance before promotion.
3. Separate curated source suggestions from relevance-ranked answers, and improve conservative identity coverage without name resemblance becoming proof.
4. Measure exact-release HTTP and MCP latency, concurrency and CPU costs on frozen inputs, then prioritize bottlenecks from profiles. One-shot evaluator latency is not production performance evidence.
5. Diagnose product/service identity and numbered aliases such as YouTube Music and modpack discovery in the next iteration. These examples do not add runtime exceptions to this release.
