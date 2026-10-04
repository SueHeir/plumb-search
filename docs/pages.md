# Page sets: Wikipedia articles in results

Plumb lists single pages next to sites, starting with English Wikipedia's articles. Searching "marie curie" shows the article Marie Curie, and "python" shows python.org with the article on the Python language under it.

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

## How pages and sites are listed together

- An article about a listed site (its Wikidata item's official website) goes under that site's result instead of in a place of its own.
- At most two other articles are listed, one when the first site is named by the whole query. An article whose title (or another title of it) is the whole query comes right after the first site when the query names that site, the site's name spells the article's title (cvs.com for "CVS Pharmacy", capitalone.com for "Capital One"), the site is an official website, or it is better known than the article is read. Otherwise the article comes first. Articles named only in part come after three sites.
- Among articles, a whole-title match beats a partial one, and more read articles beat less read ones.

## Measuring

```sh
plumb eval --index data/indexes/000123 --queries eval/article_queries.tsv \
  --pages data/pages/sets/wikipedia-en.tsv.gz --pages-top 1000000
```

`--pages` also works with the brand and described query files, to check that articles do not push official sites down.
