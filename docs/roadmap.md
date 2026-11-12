# Roadmap

What is left to do, as of 2026-11-12. It gathers the open items of the [search improvement plan](reviews/search-improvement-plan.md), the weak spots the [1,000-search review](search-quality.md#where-it-falls-short) found, and problems seen on plumbsearch.org. Pick an item, check that it still holds against the code, and read [CONTRIBUTING.md](../CONTRIBUTING.md) before starting. A change that can move results goes through the evals ([search.md](search.md) says how).

- [Search quality](#search-quality)
- [The improvement plan](#the-improvement-plan)
- [Data](#data)
- [Measuring](#measuring)

## Search quality

The review's problem list, in order of how many searches each touches, with examples still seen on plumbsearch.org on 2026-11-12.

| Problem | Still seen | Where to look |
| --- | --- | --- |
| One word of the query pulls in sites named after it | "Barcomi's Berlin" lists only Berlin sites | Whole-query evidence, `plumb-index/src/query_evidence.rs` |
| A namesake beats the well-known meaning | "Wirecutter" finds no page of the New York Times' Wirecutter at all | Entities and subpages, `pages/search.rs`, `subpage_sites.rs` |
| The homepage instead of the page asked for | | Destination pages ([plan change 11](#the-improvement-plan)) |
| The official site is missing | | Seeds and Wikidata official sites, `plumb-ingest` |
| A brand's country sites before its main site | "Skyscanner" puts skyscanner.es and .it before skyscanner.com, searched from the US in English | Country and language placing, `plumb-index` |
| Typos the spelling fix misses or gets wrong | "telsa lease" lists no Tesla site in the first five; "wikipeida" puts wikisource.org first | The spelling fix in `plumb-index` |
| Look-alike domains for a misspelled brand | "facebok login" lists facebok.com second | Look-alike checks (`official_site.rs`) and spam grades |

## The improvement plan

The [plan](reviews/search-improvement-plan.md) from 2026-10-13 has fifteen changes. Changes 2, 3, 5, 7, 8, 9, 12 and 14 are in. These are open:

| Change | What is left |
| --- | --- |
| 1. Reproducible baseline | Grade the audit and held-out labels by hand (all are still `candidate`), and measure memory in eval reports |
| 4. Shared query intent | One query-plan type in `plumb-core`, used by site retrieval, page retrieval and assembly, instead of separate intent checks |
| 6. Candidates and page blending | Page counts set by intent instead of the fixed `MAX_PAGES_LISTED`, and a bounded candidate budget shared by sites and pages |
| 10. Dependable facts | Where each fact came from and when it was read, and a state for facts that may be out of date |
| 11. Specific destination pages | Task kinds and places for subpage sites, more docs hosts, and text from PDFs |
| 13. Language and gaps | Say in answers when a search falls in a gap (a set off or partial, a language with little coverage), and add sources beyond English |
| 15. Source quality | Show quality reasons in eval traces, and measure spam grades against a reviewed set instead of trusting a language model's grades alone |

## Data

- **Paper dates.** plumbsearch.org's papers set reports a publication year for none of its papers (the `coverage` field of a `kind=paper` search), so a date filter there (`transformer after:2020`) finds no papers. Rebuilding the set with `PaperMetadata` filled in would fix it.
- **Canonical papers.** "Attention Is All You Need" finds a 2025 re-registered DOI before the 2017 paper. The repair code (plan change 8) is in; the set needs a rebuild through it.

## Measuring

- Grade the eval labels by hand, so a change can be judged on more than candidates ([plan change 1](#the-improvement-plan)).
- After each review run on a build from main, update [search-quality.md](search-quality.md) and this page.
