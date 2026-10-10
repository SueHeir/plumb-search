# Test searches

Every ranking change is measured with these files: `plumb eval` searches
for each query and checks where the expected answer lands. Each file holds
one kind of search, so a change that helps one kind and hurts another
shows up.

| File | Kind of search | Expected answer | Needs |
| --- | --- | --- | --- |
| `brand_queries.tsv` | an organization's name ("chase") | its official site | |
| `typo_queries.tsv` | a misspelled name ("chsae") | the brand's site | `--follow-suggestions` to measure "Did you mean" |
| `ai_queries.tsv` | what an AI agent looks up ("rust docs", "paypal login") | the official site | |
| `utility_queries.tsv` | a tool or a quick fact ("weather", "20 usd to eur", "define prioritize", "food near me") | a site that does the job, or the word's dictionary page | |
| `described_queries.tsv` | a site described, not named ("cheap flights") | any of several fitting sites | |
| `article_queries.tsv` | a person, place or idea | its Wikipedia article | `--pages wikipedia-en.tsv.gz` |
| `fact_queries.tsv` | a fact ("capital of japan") | text the instant answer has | `--facts --pages wikipedia-en.tsv.gz` (made with fetch-facts) |
| `howto_queries.tsv` | a how-to question | any question of the fitting Stack Exchange site | `--pages stackexchange.tsv.gz` |
| `question_queries.tsv` | a programming question | that Stack Overflow question | `--pages stackoverflow.tsv.gz` |
| `repo_queries.tsv` | a tool that lives on GitHub | its repository | `--pages github.tsv.gz` |
| `package_queries.tsv` | a software package ("serde crate") | its registry page | `--pages packages.tsv.gz` |
| `paper_queries.tsv` | a well-known paper | its DOI | `--pages papers.tsv.gz` |
| `book_queries.tsv` | a well-known book | its Open Library work | `--pages books.tsv.gz` |
| `film_queries.tsv` | a film or show with its year, director or cast ("dune 2021") | its Wikipedia article | `--pages wikipedia-en.tsv.gz --pages films.tsv.gz` |
| `music_queries.tsv` | a song or album with its artist ("hey jude beatles", "abbey road album") | its MusicBrainz page | `--pages music.tsv.gz` |
| `topic_queries.tsv` | the news ("world news") or a topic in it ("tariffs") | a major news site, or the topic's Wikipedia article | `--pages wikipedia-en.tsv.gz` |
| `lyrics_queries.tsv` | a song's lyrics ("jolene lyrics") | its lyrics page on Genius, shown above the results | `--pages music.tsv.gz --profiles` |
| `subpage_queries.tsv` | one page deep inside a well-known site ("perft results", "nist sp 811", "nba standings") | that page | `--pages subpages2.tsv.gz` |

## Format

One `query<TAB>answer[,another_ok_answer]` per line; blank lines and lines
starting with `#` are skipped. An answer is a registrable domain
(`chase.com`), a page's address, or an address ending in `*` that takes
any page it starts. A comma inside an address (`Tesla,_Inc.`) is
written `%2C`. A fact answer is lowercase text the answer must
contain, commas left out. The test `repository_query_files_are_valid`
checks every file here: it parses, no query is asked twice, and every
answer is written the way results are keyed.

## Where the searches come from

There are no search logs, and there never will be. The searches are
written by hand from public lists of what people look for: the most-read
Wikipedia articles, the most-visited sites, the most-viewed Stack Overflow
and Stack Exchange questions, the most-cited papers, the most-starred
repositories and most-used packages, plus the ways people and AI agents
phrase searches (a name alone, a name and "login" or "docs", a
description, a question).

Every answer can be checked: a domain is the organization's own site, a
page is in the page set the file names, and a fact is Wikidata's. Before
adding page answers, run

    plumb check-labels --queries eval/question_queries.tsv \
      --pages stackoverflow.tsv.gz

which prints each expected page's title (to see it is the page the search
means) and any address no set has. `--find "Dune"` lists the pages with a
title, to look one up.

## Tune and held-out halves

Each query is in one of two halves, picked from its words alone (a hash
of the lowercased query), so adding queries never moves one from half to
half. Tune ranking changes on one half and check them on the other, so a
change that only fits these exact searches shows up:

    plumb eval --index DIR --queries eval/brand_queries.tsv --half tune
    plumb eval --index DIR --queries eval/brand_queries.tsv --half held-out

Without `--half`, both halves are measured.

## Learned ranking

The first ten results of every search are put in order by a small model
trained on these searches (`crates/plumb-index/src/learned.rs`, the model
in `learned_model.json` next to it). It reads the signals the hand-made
ranking already has for each result: its place, score, text match, link
score, whether the query names it, the pages under a site, a page's set
and how read it is. It is trained on the **tune half only**, so the
held-out half still says how well it does on searches it never saw.

To train it again after the searches or the ranking change:

    plumb eval --index DIR --queries eval/ai_queries.tsv ... \
      --pages ... --limit 20 --features-out features.jsonl
    plumb eval --index DIR --queries eval/typo_queries.tsv \
      --follow-suggestions --limit 20 --features-out features-typo.jsonl
    plumb train-rank --features features.jsonl --features features-typo.jsonl \
      --out crates/plumb-index/src/learned_model.json

Searches are embedded after the model's search instruction by default
(`--query-instruction split`), so train on features written that way;
the model in `learned_model.json` was (October 2026, main 58b70f7).

`--features-out` writes the hand-made order (the model is never trained
on its own output), and `train-rank` prints top-1, top-3 and MRR of both
halves before and after. `--folds 5` also trains on four fifths of the
training half and judges the fifth left out, in turn, so options can be
chosen without looking at the held-out half.

`--objective` picks the loss (`lambda-rank`, the default; `lambda-loss`,
LambdaLoss's NDCG-Loss2++; or `softmax`) and `--learner net` trains a
small neural ranker instead of trees (log-scaled, standardized inputs
with noise, ReLU layers, as in Qin et al. 2021). On the run 10 features
(October 2026) none of them beat the default trees by more than noise:
every loss, trees or net, landed at 74-75% top-1 in 5-fold
cross-validation and 76-77% on the held-out half, while only 81.5% of
held-out searches have an expected result in the first ten rows at all.
The order within the first ten is close to what the rows allow; the
misses are mostly searches whose answer is not among them. `plumb train-rank --judge builtin` measures the
model nodes use. To compare with the hand-made order in a full eval, use a
sweep line `hand	{"learned": false}`. `--rerank-model DIR` (with
`--features-out`) also scores the first 20 results with a cross-encoder
model, for trying a second pass.

## Trying another embedding model

`--model DIR` can name an embedding server instead of the pinned model:
put an `embedding-server.json` in DIR (see `plumb_embed::SERVER_FILE`)
with the server's `/v1/embeddings` address, the model's name, and
optionally `dim` (values kept, for Matryoshka models) and the model's
`query_prefix` and `text_prefix`. `plumb embed --model DIR` then makes the
sites' vectors with it and `plumb eval --model DIR --vectors FILE` embeds
the searches with it, so a model Plumb cannot run yet (llama.cpp's
`llama-server --embedding`) can be measured before anyone ports it. Server
vectors are for evals only; nodes keep the pinned model.

## Reproducible contracts and one combined batch

`plumb eval --report run.jsonl --eval-time UNIX` extends the existing evaluator.
TSV suites still use the original query hash; `--acceptance file.jsonl` uses a
stable **family** hash, or an explicit `tune`/`held-out` split. All paraphrases
of a family must share a split, including across files. Acceptance files cannot
be passed to `--features-out` or `train-rank`. Do not use held-out observations
to tune or train; revise the development suite instead.

Each JSONL record has `id`, `family`, `category`, `query`, `label_status`,
`relevant: [{identity, grade}]` (grades 1–3), and optional `options`, `expect`,
`negatives: [{identity, max_rank, reason}]`. A negative fails its contract even
when a positive answer also appears. Domain negatives match hostname boundaries;
URL labels are exact or end in `*`. Supported options are `kind`, `site`,
`language`, `country`, `only_country`, and `exact`. Expectations can constrain
`kind`, `site`, `language`, `country`, `answer_contains`, `answer_excludes`,
`date_contains`, or `abstain`. Unknown metadata fails an explicit metadata
expectation. Date/answer checks inspect the answer and its note, never the query
or label. `tool`, `arguments`, and `expect.verdict` cover identity-tool fixtures.

`label_status` is `candidate`, `legacy`, or `manual`. Manual labels require a
`reviewer` and `evidence`. `self_grade`, `evidence`, and `root_cause` are separate
fields preserved in the response record. A self-grade alone never makes a label
manual. `audit.jsonl` contains exact plan queries and proposed negative labels;
it explicitly records that original complete RPCs were unavailable.
`family_heldout.jsonl` has 40 independently authored families / 200 prompts
separate from the audit families. Its identity labels need human full-response
adjudication before release gating. `offline.jsonl` is a tiny controlled fixture
suite, not a real-corpus accuracy estimate.

The DeepSeek report's 17% yes rate was call-weighted and self-graded, including
retries and saved findings. It is not independent-query accuracy. Preserve full
request/response transcripts when supplied; do not treat the summary or a
1,500-character excerpt as adjudicated relevance. Family pass counts keep retries
and paraphrases from multiplying category success. The original 1,000-job model
runner stays paused; these commands never invoke it or a paid grader.

A report starts with embedded build revision/dirty status, rank configuration,
learned model checksum, embedding model ID/vector checksum, query instruction,
fixed clock, enabled source files, full corpus checksums/bytes, indexed counts,
and disabled findings/personalization/plugins/external/peer features. Files are
stream-hashed before and after; a changed corpus invalidates the run. Neither
source mtimes nor a runtime checkout attest a serving binary's revision.

Every query keeps complete hits/pages/answer/spelling/rows, score, response bytes,
and explicit diagnostic stages: record membership, exact label-address lookup,
lexical and semantic candidates, source filtering, page selection, blended rows,
learned order, serialization and loss attribution. Production entity lookup is
marked unobserved. Candidate recall at 10/50/100 is a diagnostic search separate
from the real candidate window. Serialized recall is `null` above the requested
limit. Typed and operator paths use the existing page search/filter primitives;
they are observations of the **offline evaluator**, not claims of HTML/API/MCP
parity on a live node. Surface parity tests must use the same in-process fixture
backend after shared assembly is integrated.

Per-category summaries keep label statuses separate, report top-one/top-three,
MRR, graded NDCG@10, family pass counts, wrong-domain and wrong-brand-top-three
counts. Manual contract failures fail the process after writing the report.
Latency excludes diagnostic searches. Response bytes, corpus storage and process
peak RSS are separate observations. Peak evaluator RSS includes loading/index
building; measure steady-state node RSS separately on a representative server
and desktop before enforcing the proposed +10% p95 / +20% RSS gates. No performance
gate has been measured by adding this harness.

Print artifact identity with `plumb build-info`; `/api/status` adds `build` and
MCP initialization adds `_meta["plumb.build"]`. Worktrees resolve Git's own HEAD,
ref and index paths. Archive/release-container builders must supply a full SHA:

```sh
docker build --build-arg PLUMB_BUILD_REVISION="$(git rev-parse HEAD)" \
  --build-arg PLUMB_BUILD_DIRTY=false -t plumb-search:review .
```

Only use `false` after establishing a clean source tree. Missing/invalid revision
is `unknown`, and an explicit revision without a cleanliness assertion has
`dirty: null`. A matching SHA with unknown cleanliness does not establish parity. Git builds
also embed a checksum of tracked and non-ignored source files to identify dirty
artifacts. Archive builds report that source checksum as unknown.

The bounded runner requires an already-built binary and writes only its new
output directory. First establish the tiny deterministic/offline baseline:

```sh
CARGO_TARGET_DIR=/Users/suehr/.codex/cache/plumb-search-quality-20261009-batch-evaluation-target \
  CARGO_BUILD_JOBS=4 cargo build --locked -p plumb-node
python3 eval/contracts/run_batch.py \
  --plumb /Users/suehr/.codex/cache/plumb-search-quality-20261009-batch-evaluation-target/debug/plumb \
  --fixture-baseline --out-dir /tmp/plumb-search-offline-baseline --seconds 120
```

Run the combined batch **once after all worker commits are integrated**, using
an immutable index snapshot and the same page files/rank/model as the baseline:

```sh
python3 eval/contracts/run_batch.py --plumb /scratch/plumb-quality/target/release/plumb \
  --index /scratch/plumb-quality/snapshot/index \
  --pages /scratch/plumb-quality/snapshot/docs.tsv.gz \
  --pages /scratch/plumb-quality/snapshot/papers.tsv.gz \
  --pages /scratch/plumb-quality/snapshot/wikipedia-en.tsv.gz \
  --queries eval/brand_queries.tsv --queries eval/ai_queries.tsv \
  --acceptance eval/contracts/audit.jsonl \
  --acceptance eval/contracts/family_heldout.jsonl \
  --rank '{}' --eval-time 1791586800 --seconds 1800 \
  --out-dir /scratch/plumb-quality/results/combined
```

The paths are explicit staging inputs, not a request to copy production data.
Keep any baseline/candidate ranker comparison on identical hashes. To measure
meaning, append `--model SNAPSHOT_MODEL --vectors SNAPSHOT_VECTORS
--query-instruction split`; offline fixture mode has no embedding model or network.
Use an independently frozen corpus for corpus-change experiments, keeping the
ranker fixed, and report those effects separately.

If the coordinator prepares an isolated scratch candidate with the same full
corpus and build, append `--mcp http://127.0.0.1:18081/mcp --identity-calls 400
--identity-seconds 600`. The runner requires loopback and an exact clean embedded
revision. It runs the existing official-site suites plus the audit's own official,
lookalike and facts contracts, records complete initialization and RPCs, and
reports false medium/high-confidence official answers and false lookalike
accusations separately. Loopback alone does not isolate production: provide a
scratch backend with findings/history/plugins/peer/external results disabled.
The coordinator reports the production HPC moved to `739dd11` during dispatch
and now supports findings-off. Record the actually serving build and corpus;
findings-off alone does not isolate history/plugins/peer results or data refresh.
Do not point this runner at its production listener. `plumb serve` alone has no page-set backend
and cannot validate package/docs evidence; use the integrated scratch node or
in-process MCP fixture tests.

Re-score identity transcripts offline with:

```sh
python3 eval/official_site/run.py --responses results/identity.jsonl \
  --out results/identity-regraded.jsonl --max-calls 400 --time-budget 60 \
  eval/contracts/audit.jsonl eval/official_site/queries.tsv eval/official_site/heldout.tsv
```

A run that hits its call/time cap exits 2 and records `incomplete`; no runner
silently resumes calls. Core timeout kills the evaluator's process group and
leaves `batch.json` incomplete. `core-resource.log` contains `/usr/bin/time` peak
RSS and wall time; `core.jsonl` contains search latency and storage separately.

HPC staging constraints: read-only inspection found 24 GiB root free initially,
a 3.8 GiB release target and a 45 GiB debug target. External cleanup/restart
during dispatch reclaimed over 81 GiB according to the coordinator; that original
24 GiB reading is historical, not current. Recheck available disk/RSS before
staging and retain explicit resource caps. Do not copy the ~50 GB
production data, mutate caches/sets, reuse its release binary path, or start
multiple Linux builds. After coordinator readiness, one scratch release build
with debug/incremental off has a 12 GiB disk cap, 8 GiB RSS cap, four jobs and a
30-minute wall cap. A reference-only manifest or read-only bind view of a frozen
generation avoids corpus duplication; indexes can create lockfiles, so attach an
immutable view with lockfiles handled in scratch rather than opening a live
writer's generation. Corpus migration/index rebuilds need their own measured
space budget and must not be hidden in this evaluation cap. If a cap is exceeded,
stop and report it; never prune production artifacts to make room.


Compare an already-recorded baseline without rerunning it by adding
`--compare /scratch/plumb-quality/results/baseline/core.jsonl` to the combined
batch command. The runner verifies corpus/suite/vector checksums, model,
instruction, fixed clock, options and isolation before producing
`comparison.json`. It reports paired category deltas and a deterministic
500-resample family bootstrap interval, keeping candidate/legacy/manual labels
separate. Manual contract regressions are named explicitly. For a separate
corpus experiment (same rank settings and learned model), use:

```sh
python3 eval/contracts/compare.py baseline/core.jsonl corpus-candidate/core.jsonl \
  --mode corpus --out corpus-comparison.json
```

Establish the fixed-corpus baseline from the pinned integration base plus the
harness commit, then apply the other worker commits and run the candidate once.
Do not equate the tiny fixture baseline with a production-corpus baseline. The
coordinator owns the final page backend and combined run. Workspace builds must
use an isolated target directory: shared worktree targets were observed reusing
foreign schema/build-script artifacts during this batch. Worker checks use one
job, incremental off, and dev/test debug info off; the final integrated checks
can use four jobs after worker builds finish.
