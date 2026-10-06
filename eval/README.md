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

## Format

One `query<TAB>answer[,another_ok_answer]` per line; blank lines and lines
starting with `#` are skipped. An answer is a registrable domain
(`chase.com`), a page's address, or an address ending in `*` that takes
any page it starts. A fact answer is lowercase text the answer must
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
