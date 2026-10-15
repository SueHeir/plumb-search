# How search is doing

Plumb's results are measured on 1,000 real searches, graded one by one. This page shows where search stands now, how it got there, and where it still falls short. Every number names the run, the date and the build it was measured on.

- [Now](#now)
- [Over time](#over-time)
- [By kind of search](#by-kind-of-search)
- [Where it falls short](#where-it-falls-short)
- [Speed](#speed)
- [How it is measured](#how-it-is-measured)
- [The index behind it](#the-index-behind-it)
- [Updating this page](#updating-this-page)

## Now

Measured in **run 5**, on 2026-10-14, on build [`11bab11`](https://github.com/SueHeir/plumb-search/commit/11bab11). Release 0.3.1 is this build plus release notes and a refactor meant to leave results unchanged.

| | Run 5 | Run 1 (2026-10-13) |
| --- | --- | --- |
| Mean grade, 0 to 3 | **2.06** | 1.86 |
| Searches graded great (3) | **415** of 1,000 | 338 |
| Searches graded useless (0) | **102** of 1,000 | 152 |
| Known right answer at #1 | **346** of 464 (75%) | 282 (61%) |
| Known right answer in the top 3 | **389** of 464 (84%) | 370 (80%) |
| Median time per search | **0.50 s** | 0.54 s |

In short: Plumb is good at finding a thing by its name (the official site, the Wikipedia article, the package, the well-known Stack Overflow question) and much weaker when a search describes what it wants in other words.

## Over time

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="search-quality/mean-grade-dark.svg">
  <img alt="Mean grade of the 1,000 searches by run: 1.86, 1.87, 1.97, 2.00 and 2.06" src="search-quality/mean-grade.svg">
</picture>

The mean passed the project's goal of 2.0 in run 5. The grader itself drifts a little between runs: regrading about 50 unchanged searches each time moved the mean by 0.02 to 0.03, so smaller moves than that are noise.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="search-quality/known-answers-dark.svg">
  <img alt="Share of the 464 searches with a known answer that had it at #1: 61%, 60%, 69%, 70%, 75%; in the top 3: 80%, 79%, 81%, 81%, 84%" src="search-quality/known-answers.svg">
</picture>

464 of the searches have a right answer known in advance. Where it lands needs no grader, so this is the steadiest measure here.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="search-quality/grades-dark.svg">
  <img alt="Grades by run. Run 1: 34% great, 33% fine, 18% poor, 15% useless. Run 5: 42% great, 33% fine, 16% poor, 10% useless" src="search-quality/grades.svg">
</picture>

## By kind of search

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="search-quality/lanes-dark.svg">
  <img alt="Mean grade by kind of search, run 1 against run 5: all 1.86 to 2.06, popular 2.44 to 2.48, test sets 2.37 to 2.54, AI agent 1.55 to 1.98, everyday 1.43 to 1.57" src="search-quality/lanes.svg">
</picture>

| Kind | Searches | What they are | Run 1 | Run 5 |
| --- | ---: | --- | ---: | ---: |
| Popular | 184 | The most searched names and topics in the US: sites, people, trends, tools | 2.44 | 2.48 |
| Test sets | 226 | Drawn from the project's test files in [`eval/`](../eval/README.md): brands, packages, papers, songs, facts, typos and more | 2.37 | 2.54 |
| AI agent | 240 | What an AI agent looks up while working: official sites, docs, data and long technical searches | 1.55 | 1.98 |
| Everyday | 350 | Real people's searches from ORCAS, a public research set of Bing searches | 1.43 | 1.57 |

For popular searches the reviewer found the best answer at #1 in 169 of 184. Everyday searches, which describe what they want rather than name it, are where Plumb is weakest: it matches names, titles and descriptions, not the full text of pages.

## Where it falls short

Problems the reviewer tagged in run 5, with the number of searches each touched and their mean grade. One search can have several. The counts come from the reviewer's tags and keyword rules over its notes, so they are approximate.

| Problem | Searches | Mean grade | Example |
| --- | ---: | ---: | --- |
| One word of the query pulls in sites named after it | 446 | 1.94 | "Barcomi's Berlin": every result matches only "Berlin" |
| A namesake beats the well-known meaning | 382 | 1.87 | "Wirecutter": the info box describes wire cutters, the pliers |
| The homepage instead of the page asked for | 190 | 1.80 | "IRS 2026 retirement plan contribution limits news release": the IRS homepage |
| The official site is missing | 147 | 1.06 | "MeteoSchweiz": meteoschweiz.admin.ch is not shown |
| Fewer than five results | 136 | 2.57 | |
| Junk or spam among the results | 101 | 1.92 | |
| Results in the wrong language | 61 | 1.93 | "Skyscanner": the Spanish, Italian and Indian sites come before skyscanner.com |
| Typos the spelling fix misses or gets wrong | 35 | 0.26 | "shien": no "Did you mean shein" |

Typos are the worst-graded problem, and a missing official site is the next. To see how a search is answered and ranked, and how to measure a change, read [search.md](search.md).

## Speed

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="search-quality/speed-dark.svg">
  <img alt="Median and 95th percentile time per search by run. Median: 0.54, 0.78, 0.89, 0.42, 0.50 seconds. 95th percentile: 1.39, 1.39, 1.55, 0.65, 0.84 seconds" src="search-quality/speed.svg">
</picture>

Times are for whole searches through `/api/search` on the project's test server, which holds about 2.8 million sites with page sets and places (places were off in run 1). No search in any run failed.

## How it is measured

1. **The searches.** 1,000 searches in the four kinds above, fixed since run 1. Each runs against a node built from the commit being measured, through `/api/search?full=1`.
2. **The grade.** An LLM reviewer (Claude Haiku) reads each search's results as an AI agent receives them, 25 searches at a time, and grades it as the person or agent who typed it would: **3** great (the best answer is #1, or the top 3 are all strong), **2** fine (a good answer in the top 3, some noise), **1** poor (a good answer only at rank 4 to 10, or results only partly useful), **0** useless (nothing shown meets the need). It also tags each problem it sees and writes a one-line note.
3. **Only changes are regraded.** A search whose results match an earlier run keeps that run's grade. To size the reviewer's own drift, about 50 unchanged searches are graded again each run.
4. **Known answers.** For the 464 searches with a right answer known in advance, the rank where it lands is counted directly, with no reviewer.

The grades say how good results look to a careful reader, not whether every link is right, and one reviewer's taste is built in. Comparing two builds on the same searches is what the review is good for. For changes to ranking code, the frozen evals in [`eval/`](../eval/README.md) are the stricter check: `plumb-tools eval` must give identical results for a refactor.

| Run | Date | Build | Mean grade | Great | Fine | Poor | Useless |
| ---: | --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 2026-10-13 | `7405be6` | 1.86 | 338 | 334 | 176 | 152 |
| 2 | 2026-10-13 | `7f3e2e0` | 1.87 | 344 | 326 | 190 | 140 |
| 3 | 2026-10-13 | `0e67130` | 1.97 | 407 | 282 | 180 | 131 |
| 4 | 2026-10-14 | `874a662` | 2.00 | 406 | 306 | 169 | 119 |
| 5 | 2026-10-14 | `11bab11` | 2.06 | 415 | 327 | 156 | 102 |

Every number behind the charts is in [`search-quality/runs.tsv`](search-quality/runs.tsv).

## The index behind it

What plumbsearch.org held on 2026-10-14, running 0.3.1 (from its [`/api/status`](https://plumbsearch.org/api/status)):

| | |
| --- | ---: |
| Sites | 2,811,021 |
| Sites with a vector for search by meaning | 2,224,213 |
| Pages from page sets | 13,304,410 |
| Places | 24,014,360 |

## Updating this page

After a new run, add its row to [`search-quality/runs.tsv`](search-quality/runs.tsv), run

```sh
python3 docs/search-quality/charts.py
```

to redraw every chart in light and dark, and update the numbers in the text here and in the [README](../README.md#how-search-is-doing). The script needs only Python 3.
