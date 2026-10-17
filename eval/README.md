# Test searches

The searches every ranking change is measured with, and the scripts around them. `plumb-tools eval` searches for each query and checks where the expected answer lands. This page is for contributors who change ranking, page sets or fetching; for how search is doing now, see [search quality](../docs/search-quality.md).

- [What is here](#what-is-here)
- [Run a suite](#run-a-suite)
- [The suites](#the-suites)
- [File format](#file-format)
- [Tune and held-out halves](#tune-and-held-out-halves)
- [Where the searches come from](#where-the-searches-come-from)
- [Options that change what is measured](#options-that-change-what-is-measured)
- [Sweeps](#sweeps)
- [Training the learned ranking](#training-the-learned-ranking)
- [Trying another embedding model](#trying-another-embedding-model)
- [Contracts and reproducible reports](#contracts-and-reproducible-reports)
- [Tests](#tests)

## What is here

| Path | What it holds |
| --- | --- |
| `*_queries.tsv` | The suites: one kind of search each, so a change that helps one kind and hurts another shows up |
| [sweeps/](sweeps/) | Ranking variants to compare in one run, for `eval --sweep` |
| [contracts/](contracts/) | JSONL acceptance contracts, and scripts that run and compare reproducible reports |
| [official_site/](official_site/README.md) | Checks of the MCP `official_site` and `check_lookalike` tools against a running node |
| [boilerplate/](boilerplate/README.md) | Fixtures for the text a crawl keeps from a homepage |
| [llm/](llm/README.md) | Whether Plumb helps a small local model answer questions, and what it costs in context |

## Run a suite

```sh
plumb-tools eval --index DIR --queries eval/brand_queries.tsv
```

`DIR` is a site index: a node's `DATA/indexes/<id>/` or one built with `plumb-tools index` (see [plumb-tools](../crates/plumb-tools/README.md#workflows)). For each queries file `eval` prints the number of queries, the share answered first (top-1) and in the first three (top-3), and the mean reciprocal rank within the results fetched (`MRR@10` by default), with a `miss:` line for each query it got wrong. Give `--queries` several times to measure several files in one run; each is measured on its own. `--min-top1 0.9` makes it exit with an error when a file's top-1 is below 90%.

## The suites

| File | Kind of search | Expected answer | Run with |
| --- | --- | --- | --- |
| `brand_queries.tsv` | An organization's name ("chase") | Its official site | |
| `typo_queries.tsv` | A misspelled name ("chsae") | The brand's site | `--follow-suggestions` to measure what one click on "Did you mean" finds |
| `ai_queries.tsv` | What an AI agent looks up ("rust docs", "paypal login") | The official site | |
| `utility_queries.tsv` | A tool or a quick fact ("weather", "20 usd to eur") | Any site that does the job | |
| `described_queries.tsv` | A site described, not named ("cheap flights") | Any of several fitting sites | `--vectors`, `--model` for search by meaning |
| `article_queries.tsv` | A person, place or idea | Its Wikipedia article | `--pages wikipedia-en.tsv.gz` |
| `fact_queries.tsv` | A fact ("capital of japan") | Text the instant answer holds | `--facts --pages wikipedia-en.tsv.gz`, with facts from `fetch-facts` |
| `topic_queries.tsv` | The news ("world news") or a topic in it ("tariffs") | A major news site, or the topic's Wikipedia article | `--pages wikipedia-en.tsv.gz` |
| `film_queries.tsv` | A film or show with its year, director or cast ("dune 2021") | Its Wikipedia article | `--pages wikipedia-en.tsv.gz --pages films.tsv.gz` |
| `docs_queries.tsv` | What a software docs page answers ("sort a list in python") | Any page of the right docs site | `--pages docs.tsv.gz` |
| `question_queries.tsv` | A programming question | That Stack Overflow question | `--pages stackoverflow.tsv.gz` |
| `howto_queries.tsv` | A how-to question | Any question of the fitting Stack Exchange site | `--pages stackexchange.tsv.gz` |
| `repo_queries.tsv` | A tool that lives on GitHub | Its repository | `--pages github.tsv.gz` |
| `package_queries.tsv` | A software package ("serde crate") | Its registry page | `--pages packages.tsv.gz` |
| `paper_queries.tsv` | A well-known paper | Its DOI, as OpenAlex writes it | `--pages papers.tsv.gz` |
| `book_queries.tsv` | A well-known book | Its Open Library work | `--pages books.tsv.gz` |
| `music_queries.tsv` | A song or album with its artist ("hey jude beatles") | Its MusicBrainz page | `--pages music.tsv.gz` |
| `lyrics_queries.tsv` | A song's lyrics ("jolene lyrics") | Its Genius page, shown above the results | `--pages music.tsv.gz --profiles` |
| `subpage_queries.tsv` | One page deep inside a well-known site ("nist sp 811") | That page | `--pages subpages2.tsv.gz` |

Page set files are what `plumb-tools fetch-pages` writes, and what a node keeps in `DATA/pages/sets/`. `eval` tells a file's set from the start of its name (`github-new.tsv.gz` is the GitHub set), and takes a name it does not know for English Wikipedia, so keep the names fetch-pages gives them. Each file's own comment lines say more about how its searches were picked.

## File format

```text
# A comment.
chase	chase.com
hey jude by the beatles	https://musicbrainz.org/recording/2bc34fa0-e20f-4a1a-8a00-c7c672c40a6e
how to unclog a drain	https://diy.stackexchange.com/questions/*
```

One `query<TAB>answer[,another_ok_answer]` per line; blank lines and lines starting with `#` are skipped. An answer is one of:

- a registrable domain (`chase.com`);
- a page's `https` address, not a site's root;
- an address ending in `*`, which takes any page whose address starts with the rest;
- for `fact_queries.tsv`, lowercase text the instant answer must contain, commas left out.

A comma inside an address (`Tesla,_Inc.`) is written `%2C`, since a bare comma separates answers. The test `repository_query_files_are_valid` in [eval.rs](../crates/plumb-tools/src/eval.rs) checks every `.tsv` file here: it parses, holds at least 40 queries, asks no query twice, writes every answer the way results are keyed, and has between a quarter and three quarters of its queries in each half.

## Tune and held-out halves

Each query is in one of two halves, picked from its words alone (a hash of the lowercased query with its spaces collapsed, `half_of` in [eval.rs](../crates/plumb-tools/src/eval.rs)). Adding queries never moves one from half to half, and a query asked in two files is in the same half of both. Tune a ranking change on one half and check it on the other, so a change that only fits these exact searches shows up:

```sh
plumb-tools eval --index DIR --queries eval/brand_queries.tsv --half tune
plumb-tools eval --index DIR --queries eval/brand_queries.tsv --half held-out
```

Without `--half`, both halves are measured.

## Where the searches come from

The suites are written by hand, not taken from search logs. They come from public lists of what people look for (the most read Wikipedia articles, the most visited sites, the most viewed Stack Overflow and Stack Exchange questions, the most cited papers, the most starred repositories and most used packages) and from the ways people and AI agents phrase searches: a name alone, a name and "login" or "docs", a description, a question.

Every answer can be checked: a domain is the organization's own site, a page is in the page set its file names, and a fact is Wikidata's. Before adding page answers, run `check-labels`:

```sh
plumb-tools check-labels --queries eval/question_queries.tsv --pages stackoverflow.tsv.gz
plumb-tools check-labels --pages books.tsv.gz --find "Dune"
```

It prints `ok` with each expected page's title (to see it is the page the search means), `case` when a set has the address in another case, or `missing`. `--find TITLE` lists the pages with that title, most read first, to look one up.

## Options that change what is measured

| Option | What it does |
| --- | --- |
| `--pages FILE` | List pages from these page set files among the sites, as a node does. Repeat for more |
| `--pages-top N` | Keep only each page set's N most read pages |
| `--pages-cache DIR` | Keep the page index built from `--pages` and reuse it while the files and the `plumb-tools` binary are the same (also `PLUMB_EVAL_PAGES_CACHE`) |
| `--limit N` | Results fetched per query; an answer further down counts as not found (default 10) |
| `--rank JSON` | Change ranking knobs (`RankConfig` in [plumb-index](../crates/plumb-index/src/lib.rs)), such as `'{"exact_label_bonus": 0.1}'` |
| `--alpha A` | Weight of the popularity prior, 0 to 1; wins over an `alpha` in `--rank` |
| `--country CODE`, `--lang CODE` | Search as from a home country, or only sites in a language |
| `--vectors FILE --model DIR` | Search by meaning too, with vectors from `plumb-tools embed` |
| `--exact` | Never suggest a spelling |
| `--follow-suggestions` | Measure the results of the "Did you mean" suggestion when a query gets one |
| `--show-suggestions` | Print every suggestion a query gets |
| `--facts` | Check the instant answers to fact searches instead of ranks |
| `--profiles` | Count the profile or listing shown above the results ("mrbeast youtube") as the first result |
| `--explain` | For each miss, show how the first result and the expected one scored |
| `--recall` | Find where each missed answer was lost before ranking: not indexed, how far down the word or meaning candidates, or never looked at |
| `--candidates-out FILE` | Write each query's deep site candidates as JSON lines, with the raw signals a first stage could score them by: which query words each field has, field lengths, how rare each word is per field, link score and closeness. `--candidates-depth N` (500) sets how far down today's first stage, the sites with every main word and the sites nearest in meaning it goes. Only queries that expect a site |
| `--show N` | Print the first N results of every query |
| `--half tune\|held-out` | Measure one half only |

## Sweeps

`--sweep FILE` measures several rankings in one run and prints one table of every queries file under every ranking, with the queries each one moved against the ranking unchanged (`base`). A sweep file has one `name<TAB>{"knob": value}` line per variant, each changing knobs of the ranking `--rank` gives; a knob `RankConfig` does not have is an error. `--ranks-out FILE` also writes every query's rank under every variant as TSV (`variant suite line query rank`, 0 when not found).

```sh
plumb-tools eval --index DIR --pages wikipedia-en.tsv.gz \
  --queries eval/brand_queries.tsv --queries eval/ai_queries.tsv \
  --sweep eval/sweeps/rank_knobs.tsv --ranks-out ranks.tsv
```

| File | Variants |
| --- | --- |
| [rank_knobs.tsv](sweeps/rank_knobs.tsv) | Ranking knobs one at a time, a step either side of the default |
| [learned.tsv](sweeps/learned.tsv) | The learned ranking against the hand-made order (`hand`), with the site model (`sites`), and switches it depends on |
| [meaning.tsv](sweeps/meaning.tsv) | Search-by-meaning knobs, for fitting them to another embedding model (needs `--vectors`, `--model`) |
| [partial.tsv](sweeps/partial.tsv) | How close in meaning a site lacking some of a longer query's words must be to stay listed (needs `--vectors`, `--model`) |

## Training the learned ranking

The first ten results of a search are put in order by a small model trained on these suites ([learned.rs](../crates/plumb-index/src/learned.rs), with the model in `learned_model.json` beside it). It reads the signals the hand-made ranking already has for each result. To train it again after the suites or the ranking change:

```sh
plumb-tools eval --index DIR --pages wikipedia-en.tsv.gz --pages github.tsv.gz ... \
  --queries eval/ai_queries.tsv --queries eval/article_queries.tsv ... --features-out features.jsonl
plumb-tools eval --index DIR --queries eval/typo_queries.tsv --follow-suggestions \
  --features-out features-typo.jsonl
plumb-tools train-rank --features features.jsonl --features features-typo.jsonl \
  --out crates/plumb-index/src/learned_model.json
```

- `--features-out` writes one JSON line per search (`suite`, `half`, `query`, `searched`, `expected` and the listed `rows` with their signals) in the hand-made order, so the model never trains on its own output. It writes the first 30 rows, or every site `--limit` asks for when that is more (`train-rank --rows`). It cannot be combined with `--sweep` or `--facts`.
- `train-rank` trains on the tune half and prints, for each file and half, top-1, top-3 and MRR of the expected answers within the first ten rows, before and after.
- A row can also carry an LLM's `grade` of how well it meets the search, 0 to 3; `train-rank` then reports NDCG@10 too, an expected answer counting as grade 3. A search marked `"half": "test"` is only judged, never trained on. The graded searches are not in this repository.
- `--folds K` also trains on all but one of K parts of the training half and judges the part left out, in turn, to choose options without looking at the held-out half. `--learner net` trains a small neural ranker instead of trees; `--objective` picks the loss (`lambda-rank`, `lambda-loss` or `softmax`).
- `--judge builtin` measures the model nodes use; `--orders-out FILE` writes the order the model gives each search, to see what it moved.
- In a full eval, compare with the hand-made order through the `hand` line of [sweeps/learned.tsv](sweeps/learned.tsv).
- `--rerank-model DIR` (with `--features-out`) also scores the first 20 results with a cross-encoder model (a folder with `config.json`, `tokenizer.json` and `model.safetensors`), to try a second pass.

`plumb click-labels --data DIR --out clicks.jsonl --queries clicks.tsv` writes a queries file from the searches a node's users chose to keep as training examples, for `--features-out` in the same way.

With `--vectors` and `--model`, searches are embedded the way nodes embed them (`QueryInstruction::Split` in plumb-node's [cli.rs](../crates/plumb-node/src/cli.rs)), so the features match what a node serves.

## Trying another embedding model

`--model DIR` can name an embedding server instead of a model Plumb runs: put an `embedding-server.json` in `DIR` with the server's `/v1/embeddings` address and the model's name, and optionally `dim` and the model's `query_prefix` and `text_prefix` ([server.rs](../crates/plumb-embed/src/server.rs) has the format). Then `plumb-tools embed --model DIR` makes the sites' vectors with it and `plumb-tools eval --model DIR --vectors FILE` embeds the searches with it, so a model Plumb cannot run yet (llama.cpp's `llama-server --embedding`, for one) can be measured first. Vectors from a server are for evals, not for sharing with other nodes.

## Contracts and reproducible reports

Report mode writes everything a run saw to one JSON-lines file, so two runs can be compared exactly:

```sh
plumb-tools eval --index DIR --pages docs.tsv.gz --pages wikipedia-en.tsv.gz \
  --queries eval/brand_queries.tsv \
  --acceptance eval/contracts/audit.jsonl --acceptance eval/contracts/family_heldout.jsonl \
  --report run/core.jsonl --eval-time 1791586800 --limit 100
```

- `--report FILE` needs `--eval-time UNIX`, a fixed clock for answers that depend on the date. It runs in-process with findings, personalization, plugins, external results and peers off, and cannot be combined with `--sweep`, `--features-out`, `--facts`, `--profiles`, `--follow-suggestions` or `--rerank-model`. It needs a local model with `--model`, not an embedding server.
- `--acceptance FILE` adds JSONL contracts (below). TSV suites given with `--queries` run as `legacy` contracts.
- The report has a manifest line (build, ranking knobs, the learned model's SHA-256, the embedding model and vectors, a SHA-256 of the index, every page file and every suite, indexed counts), one line per case with the complete response and where the answer was lost ([how a report observes a search](contracts/adapter-notes.md)), and a summary line: for each `label_status:category` group top-1, top-3, MRR, NDCG@10, the families that passed whole and the wrong-domain and wrong-brand counts, then the run's median and p95 latency.
- The run fails when the index or page files changed while it ran, when a `manual` contract fails, or when a group is below `--min-top1`. The report file must be outside the index and the page files.

### Contract format

One JSON object per line:

```json
{"id": "offline-chase", "family": "offline-chase", "category": "fixture", "query": "chase",
 "label_status": "manual", "reviewer": "fixture-author", "evidence": {"source": "fixtures/brand_queries.tsv"},
 "relevant": [{"identity": "chase.com", "grade": 3}],
 "negatives": [{"identity": "usbank-login-help.com", "max_rank": 3, "reason": "wrong_brand"}]}
```

| Field | Meaning |
| --- | --- |
| `id`, `family`, `category`, `query` | Required. Cases of one family are paraphrases of one search |
| `label_status` | `candidate`, `legacy` or `manual`; `manual` needs `reviewer` and `evidence` |
| `relevant` | `{identity, grade}`: a domain, an address, or an address ending in `*`; grade 1 to 3 |
| `negatives` | `{identity, max_rank, reason}`: fails the case if it shows in the first `max_rank` results, even when a relevant answer is there too |
| `options` | `kind`, `site`, `language`, `country`, `only_country`, `exact` |
| `expect` | `kind`, `site`, `language`, `country`, `answer_contains`, `answer_excludes`, `date_contains`, `abstain`, `verdict` |
| `split` | `tune` or `held-out`; otherwise the family's words pick the half, with the same hash as the TSV suites |
| `tool` | `search` (the default), or `official_site`, `check_lookalike`, `facts` for [official_site/run.py](official_site/README.md) |
| `self_grade`, `root_cause`, `arguments` | Kept with the case in the report |

A case needs a relevant answer, an expected answer text, a verdict or `abstain`. Every case of a family must fall in the same half, or the file is refused. Domain identities match on hostname boundaries (`brand.com` takes `api.brand.com`, never `brand.com.evil`). `answer_contains` and the date checks read the answer and its note, never the query or the label. `kind` is one of `site`, `article`, `question`, `package`, `repo`, `book`, `paper`, `docs`, `reference`, `subpage`.

| File | Cases |
| --- | --- |
| [contracts/audit.jsonl](contracts/audit.jsonl) | 17 `candidate` cases from an earlier search improvement plan: 8 searches, the rest for the identity tools |
| [contracts/family_heldout.jsonl](contracts/family_heldout.jsonl) | 200 `candidate` cases in 40 families, separate from the audit's, each with negatives |
| [contracts/offline.jsonl](contracts/offline.jsonl) | 3 `manual` cases for the synthetic [fixtures](../fixtures/README.md) |

`candidate` and `legacy` results are observations, not accuracy figures: only `manual` labels gate anything.

### Comparing two reports

```sh
python3 eval/contracts/compare.py baseline/core.jsonl candidate/core.jsonl --mode ranker --out comparison.json
```

Both reports must have the same suites and cases, clock, embedding model, query instruction, `--limit`, `--pages-top` and isolation. `--mode ranker` (the default) requires the same index, page files and vectors and allows ranking knobs and the learned model to differ, listing each difference. `--mode corpus` requires the same knobs and model and allows the corpus to differ. The comparison reports, per group and metric, the mean change over families with a 95% interval from 500 family resamples, and names every `manual` case that newly fails.

> [!WARNING]
> [contracts/run_batch.py](contracts/run_batch.py), which wraps a report run, a comparison and the identity checks, still calls `plumb index` and `plumb eval`. Those commands are in `plumb-tools` now, so the script fails; run `plumb-tools eval --report` and `compare.py` directly.

[contracts/scratch-mcp.md](contracts/scratch-mcp.md) describes the `eval_mcp` example, which serves frozen indexes through the node's own web and MCP routes for checks of what an API or MCP client sees.

## Tests

The suites are checked by `cargo test -p plumb-tools` (see [plumb-tools](../crates/plumb-tools/README.md#tests)). The Python scripts have their own tests, which need no network or node:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s eval/official_site -p 'test_*.py'
PYTHONDONTWRITEBYTECODE=1 python3 eval/contracts/test_harness.py
```
