# Page sets: articles, repositories, questions, books and papers in results

Plumb lists single pages next to sites: English Wikipedia's articles, well-starred GitHub repositories, Stack Overflow's most viewed questions, Open Library's most read books, Podcast Index's most popular podcasts and the most cited papers. Searching "marie curie" shows the article Marie Curie, and "python" shows python.org with the article on the Python language under it.

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

### Official profiles

Then, to add each article's official profiles (YouTube, Twitch, TikTok, Instagram, X, Bluesky, Mastodon, Threads, Facebook, LinkedIn, GitHub, Reddit, Spotify, Apple Music, SoundCloud, Patreon, Steam, the App Store and Google Play) from Wikidata's external identifiers, and where a film, show, game, album, song or podcast is listed or can be watched or heard (IMDb, Rotten Tomatoes, Metacritic, Letterboxd, TMDB, IGDB, MyAnimeList, Netflix, MusicBrainz, Discogs, Genius, Spotify and Apple Music albums and songs, Apple Podcasts and a song's music video on YouTube):

```sh
plumb fetch-profiles --data /path/to/node-data
```

It asks Wikidata's query service for each service's property, a page of 200,000 statements at a time, and adds a `profiles` line after each article whose item has any (see `plumb_core::article`). Readers made before profiles skip those lines. Each property's formatter URL is checked first, so a service whose property points elsewhere is left out. Only the identifier is kept; the node builds the address and links only identifiers of the right shape. Run it again after each `fetch-pages`, which writes the file without profiles.

It also asks for every item's official website (P856) and keeps it for an article whose website is a subdomain or an inner page of the article's site rather than a front page: `https://music.youtube.com/` for YouTube Music, whose site is youtube.com. A query naming such an article ("youtube music", "music youtube", "google maps") shows that link first, in bold, under the site's result. Readers made before websites leave the `website=` entry out.

Some items with profiles have no English article: Linus Tech Tips the YouTube channel is a Wikidata item of its own, apart from the article on Linus Media Group. `fetch-profiles` also writes those that have an English name and an official website of their own (a front page, not a profile on one of the services) as the `wikidata` set, `wikidata.tsv.gz` beside the articles file: each with its name, English aliases ("LTT"), description, website and profiles, its sitelinks counted as its views. A page of that set is only ever listed under its website's result, never on its own, so a channel can't stand in for a namesake.

The results page lists an article's profiles in its info box, its own accounts apart from the places it is listed, and a query ending in a service's name ("mrbeast youtube", "valve steam", "spotify android app", "dune part two imdb", "bohemian rhapsody lyrics") shows that profile or listing first when the words before it name an article.

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

Questions are also found by their words, since nobody types a question's title exactly: a query of three or more words (common words like "the" left out, "commits" counted as "commit") finds a question whose title and tags have at least three quarters of them. Such questions come after the best site. A question with every word of the query, which in turn has at least half of the question title's, is what was asked ("delete a git branch locally and remotely") and comes first, unless the first site is named by the whole query.

## Books

The `books` set lists Open Library's works that readers shelve or rate most (1,000,000 by default, each on a reading log or rated at least 3 times): "dune frank herbert" finds Dune. Each keeps only its title, "Book by AUTHOR, YEAR" as its description, "TITLE AUTHOR" as another title, its work id and how many readers shelved or rated it. Open Library's dumps are CC0; the works, authors, reading log and ratings dumps are about 4 GB together:

```sh
plumb fetch-pages --set books --work /big/disk/dumps --data /path/to/node-data
```

`--min-shelvings` and `--max-books` change what is kept.

## Podcasts

The `podcasts` set lists the podcasts Podcast Index rates most popular (300,000 by default, each scoring at least 4 of 9 and answering when last fetched): "hardcore history podcast" and "dan carlin podcast" find Dan Carlin's Hardcore History. Podcast Index keeps an open index of podcast feeds, "available for free, for any use", and publishes it whole as a SQLite database of about 1.8 GB (5 GB unpacked):

```sh
plumb fetch-pages --set podcasts --work /big/disk/dumps --data /path/to/node-data
```

Each podcast keeps its title, "Podcast by AUTHOR · CATEGORY" as its description, "AUTHOR podcast" as another title, its Podcast Index id (its page at podcastindex.org lists its episodes and the apps that play it), its Apple Podcasts id, and its website's domain when that is a site of its own, so it goes under that site's result. Many podcasts share Podcast Index's popularity score, so an Apple Podcasts listing, then the years a show has run, then its episodes (counted up to 999) break ties, and a show listed twice under one title and site (or author) is kept once. Like a book, a podcast is never listed before every site by its title alone, but its title or the end of it followed by "podcast", or by its author's name, asks for it. `--min-podcast-score`, `--max-podcasts` and `--podcast-db` (an unpacked database) change what is read and kept.

## Papers

The `papers` set lists the most cited scholarly works (2,000,000 by default, each cited at least 200 times), from OpenAlex's API (CC0): "attention is all you need" finds the paper. Each keeps only its title, "Paper by AUTHOR et al., YEAR, VENUE" as its description, its DOI (or OpenAlex id) and its citations. Like questions, papers are also found by most of their title's words. A paper cited more than 40,000 times for each year since it came out is left out as a data error. A paper's whole title of four words or more ("basic local alignment search tool"), or its title followed by its first author's name, its year, its venue or "paper" ("random forests breiman", "deep learning lecun nature"), asks for the paper, which then comes first like an asked-for book.

```sh
plumb fetch-pages --set papers --work /big/disk/dumps --data /path/to/node-data --min-citations 200
```

Set `OPENALEX_API_KEY` if OpenAlex asks for a key. `--max-papers` caps how many are kept. With `--work DIR`, the papers so far are kept in `DIR/openalex/` as they come: when OpenAlex keeps refusing (it limits how much one address may ask for), the run waits as it is told, then writes the most cited papers it has, and running it again with the same `--work` carries on where it stopped.

Book and paper titles are often common words ("Python", "Apple"), so a book or paper named by its title alone is never listed before every site, and an article of the same name comes before it: "dune" lists the article on the novel, then the book. A query of a book's title followed by words of its author's name or by "book" or "novel" ("dune frank herbert", "dune book") asks for the book, which then comes first unless the first site is named by the whole query; other editions come after the best site.

## Software packages

The `packages` set lists the most used packages of eight registries: npm, PyPI, crates.io, Go modules, RubyGems, Packagist, NuGet and Maven Central (20,000 of each by default). It is for coding agents, which mostly search to look up a library: its latest version, how to install it and where its docs are. Each package keeps its name, its description, its latest version and when that came out, its license, and the addresses of its docs, code and homepage. The install command (`cargo add serde`) and the registry page are made from the registry and the name, and docs.rs, pkg.go.dev, rubydoc.info and javadoc.io stand in for docs a crate, module, gem or Maven package names none of.

```sh
plumb fetch-pages --set packages --data /path/to/node-data
```

The lists come from [ecosyste.ms](https://packages.ecosyste.ms)'s open API (CC BY-SA 4.0), most downloaded first, or most depended on for Go and Maven, which count no downloads. It allows 5,000 requests an hour, and the whole set takes about 650. `--registries npm,pypi,crates` lists only some registries, and `--max-per-registry` changes how many of each are kept. Registries count downloads in very different numbers, so a package's popularity is its downloads as a share of its registry's most downloaded package: the top crate weighs as much as the top npm package.

A package is listed only when the query asks for one, so "react" is still the site and the article. The query asks by naming a registry ("serde crate", "react npm", "requests pip", "lodash package") or a language ("requests python", "gin golang", "tokio rust"), or by asking for a version ("lodash version", "latest version of tokio"). Words like "latest", "docs" and "install" are left out of the name. A language or "version" alone only finds well-known packages, so "rust book" does not find a crate called `book`. Such a package comes first unless the best site is official or better known, with its card under its title:

> serde · crates.io: A generic serialization/deserialization framework
> Latest 1.0.228 (2025-09-27) · MIT OR Apache-2.0 · `cargo add serde` · Docs · Code · Home

The MCP server's `package` tool returns the same card by name ([mcp.md](mcp.md)). In the SearXNG-style JSON it follows the description in `content`.

## How pages and sites are listed together

- An article about a listed site (its Wikidata item's official website) goes under that site's result instead of in a place of its own.
- At most two other articles are listed, one when the first site is named by the whole query. An article whose title (or another title of it) is the whole query comes first, unless the first site is probably the website of what the query names: an article found for the query is about a company, product or service (its Wikipedia description says so), or the site is a government's, and the site is called after the article. A one-word query that spells the site but not the article ("robinhood", not "Robin Hood") also keeps the site first. So "tauri", "geico" and "robinhood" list tauri.app, geico.com and robinhood.com first, while "marie curie" lists the article before mariecurie.org. An official website, or a site better known than the article is read, also stays first, and so does a site called exactly what was searched for when the best page named so is about an organization or is a repository ("us bank" lists usbank.com before the article "U.S. Bancorp", "regex101" regex101.com before its repository). Repositories count a fifth less than articles, so "sonnet" lists the article on the poem before google-deepmind/sonnet. Articles named only in part come after three sites.
- Among articles, a whole-title match beats a partial one, and more read articles beat less read ones. A title that matches only without its bracketed qualifier or its punctuation ("Mozart (film)", "Mozart!") counts no more than another title of an article (the redirect "Mozart" to "Wolfgang Amadeus Mozart"), so the more read one wins, and an exact title ("Albert Einstein") beats both.
- Once an article is listed, a namesake of it in the same set ("Eiffel Tower (Six Flags)" after "Eiffel Tower" under toureiffel.paris) only comes after three sites.

## Measuring

```sh
plumb eval --index data/indexes/000123 --queries eval/article_queries.tsv \
  --pages data/pages/sets/wikipedia-en.tsv.gz --pages-top 1000000
plumb eval --index data/indexes/000123 --queries eval/repo_queries.tsv \
  --pages data/pages/sets/wikipedia-en.tsv.gz --pages data/pages/sets/github.tsv.gz
```

`--pages` also works with the brand and described query files, to check that articles do not push official sites down.
