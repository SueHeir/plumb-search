# Page sets: articles, repositories, questions, books and papers in results

Plumb lists single pages next to sites: English Wikipedia's articles, well-starred GitHub repositories, Stack Overflow's most viewed questions, Open Library's most read books and the most cited papers. Searching "marie curie" shows the article Marie Curie, and "python" shows python.org with the article on the Python language under it.

## What is kept

Only each article's title, up to five other titles that lead to it, Wikipedia's one-line description, its Wikidata item, the official website of that item, and its page views. That is about 100 bytes a page, file and index together. No article text is stored.

## How many

The panel's settings have a **Page sets** part with a choice for each set:

| Choice | English Wikipedia | About |
| --- | --- | --- |
| Automatic (default) | follows the storage limit | |
| | under 1 GB: 100,000 most read | 10 MB |
| | under 8 GB: 1,000,000 most read | 100 MB |
| | 8 GB or more, or no limit: all | 700 MB |
| Off | none | 0 |
| 100,000 / 1,000,000 most read | as named | 10 / 100 MB |
| All | every article read in the last week | 700 MB |

The choice is saved with the other settings (`page_sets` in `settings.json`, for example `{"wikipedia-en": "1000000"}`) and applies within seconds.

## Where the pages come from

From Wikimedia's public dumps, not from crawling Wikipedia:

```sh
plumb fetch-pages --work /big/disk/dumps --data /path/to/node-data \
  --official-sites seed/wikidata-official-sites.tsv
```

This downloads the latest `page`, `page_props` and `redirect` tables of English Wikipedia (about 3 GB) and seven days of `pageview_complete` files (about 400 MB a day), and writes `DIR/pages/sets/wikipedia-en.tsv.gz`, most read first. A running node picks it up and indexes the pages it is set to keep in `DIR/pages/index-<key>/`. Anyone can make the same file from the same dumps.

## Nodes in the network

A node with no set file, or fewer pages than it is set to keep, takes the file from a node it trusts (plumbsearch.org by default) over `/plumb/pages/1`, 1 MiB at a time, and stops once it has the pages it keeps: a node keeping 100,000 articles downloads about 10 MB, not the whole file. It asks again for a newer file after 30 days. A node passes on only whole files, made with `fetch-pages` or taken whole, so a cut file never spreads. Nodes answer at most four such requests at once and 120 a minute from each node.

## GitHub repositories

The `github` set lists public GitHub repositories with at least 500 stars: "ripgrep" finds BurntSushi/ripgrep. Each keeps only its `owner/name`, its name as another title, its description, its stars (in place of page views) and the domain of its homepage, so tauri-apps/tauri is shown under tauri.app's result. No code or README is kept. It has its own choice under **Page sets**.

```sh
GITHUB_TOKEN=... plumb fetch-pages --set github --data /path/to/node-data --min-stars 500
```

This walks GitHub's repository search, most starred first. Without a token GitHub allows 10 searches a minute, about 1,000 repositories, so a few hundred thousand repositories take some hours; a token makes it three times faster. Stars are compared with the most starred repository and page views with the most read article, so neither set crowds out the other.

## Stack Overflow questions

The `stackoverflow` set lists Stack Overflow's most viewed questions (2,000,000 by default, each with a score of at least 1): "undo last git commit" finds "How do I undo the most recent local commits in Git?". Each keeps only its title, its tags (shown as its description), its question number and its views. No question or answer text is kept. The questions come from Stack Exchange's public data dump on the Internet Archive (CC BY-SA 4.0), the 7z of Stack Overflow's posts, about 20 GB:

```sh
plumb fetch-pages --set stackoverflow --work /big/disk/dumps --data /path/to/node-data
```

`--posts PATH` reads a downloaded posts 7z instead; `--min-score` and `--max-questions` change what is kept.

Questions are also found by their words, since nobody types a question's title exactly: a query of three or more words (common words like "the" left out, "commits" counted as "commit") finds a question whose title and tags have at least three quarters of them. Such questions are listed like articles named only in part, after three sites.

## Books

The `books` set lists Open Library's works that readers shelve or rate most (1,000,000 by default, each on a reading log or rated at least 3 times): "dune frank herbert" finds Dune. Each keeps only its title, "Book by AUTHOR, YEAR" as its description, "TITLE AUTHOR" as another title, its work id and how many readers shelved or rated it. Open Library's dumps are CC0; the works, authors, reading log and ratings dumps are about 4 GB together:

```sh
plumb fetch-pages --set books --work /big/disk/dumps --data /path/to/node-data
```

`--min-shelvings` and `--max-books` change what is kept.

## Papers

The `papers` set lists the most cited scholarly works (2,000,000 by default, each cited at least 200 times), from OpenAlex's API (CC0): "attention is all you need" finds the paper. Each keeps only its title, "Paper by AUTHOR et al., YEAR, VENUE" as its description, its DOI (or OpenAlex id) and its citations. Like questions, papers are also found by most of their title's words.

```sh
plumb fetch-pages --set papers --data /path/to/node-data --min-citations 200
```

Set `OPENALEX_API_KEY` if OpenAlex asks for a key. `--max-papers` caps how many are kept.

Book and paper titles are often common words ("Python", "Apple"), so a book or paper is never listed before every site, and an article of the same name comes before it: "dune" lists the article on the novel, then the book.

## How pages and sites are listed together

- An article about a listed site (its Wikidata item's official website) goes under that site's result instead of in a place of its own.
- At most two other articles are listed, one when the first site is named by the whole query. An article whose title (or another title of it) is the whole query comes first, unless the first site is probably the website of what the query names: an article found for the query is about a company, product or service (its Wikipedia description says so), or the site is a government's, and the site is called after the article. A one-word query that spells the site but not the article ("robinhood", not "Robin Hood") also keeps the site first. So "tauri", "geico" and "robinhood" list tauri.app, geico.com and robinhood.com first, while "marie curie" lists the article before mariecurie.org. An official website, or a site better known than the article is read, also stays first. Articles named only in part come after three sites.
- Among articles, a whole-title match beats a partial one, and more read articles beat less read ones. A title that matches only without its bracketed qualifier or its punctuation ("Mozart (film)", "Mozart!") counts no more than another title of an article (the redirect "Mozart" to "Wolfgang Amadeus Mozart"), so the more read one wins, and an exact title ("Albert Einstein") beats both.
- Once an article is listed, a namesake of it ("Eiffel Tower (Six Flags)" after "Eiffel Tower" under toureiffel.paris) only comes after three sites.

## Measuring

```sh
plumb eval --index data/indexes/000123 --queries eval/article_queries.tsv \
  --pages data/pages/sets/wikipedia-en.tsv.gz --pages-top 1000000
plumb eval --index data/indexes/000123 --queries eval/repo_queries.tsv \
  --pages data/pages/sets/wikipedia-en.tsv.gz --pages data/pages/sets/github.tsv.gz
```

`--pages` also works with the brand and described query files, to check that articles do not push official sites down.
