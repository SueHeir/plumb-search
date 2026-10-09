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
