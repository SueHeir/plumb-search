# Page sets: articles, repositories, questions, books and papers in results

Plumb lists single pages next to sites: English Wikipedia's articles, well-starred GitHub repositories, Stack Overflow's and other Stack Exchange sites' most viewed questions, Open Library's most read books, Podcast Index's most popular podcasts, the songs and albums most listened to and the most cited papers. Searching "marie curie" shows the article Marie Curie, and "python" shows python.org with the article on the Python language under it.

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

### Facts

To answer searches that ask a fact ("capital of australia", "how tall is mount everest", "when was albert einstein born", "how old is elon musk", "who is the ceo of nvidia", "who founded tesla", "japan population"), add a few facts about each article's item from Wikidata:

```sh
plumb fetch-facts --data /path/to/node-data
```

It asks for twenty-one properties, only their best-ranked statements, a page of 50,000 at a time: capital, population (the latest count), elevation, height, area, birth and death dates, founding date, founders, CEO, headquarters, currency, author, director, composer, creator, owner, birthplace, spouse, head of state and head of government. Authors and creators (millions of papers and paintings) are asked for only for the most read articles, by name. Wikidata's query service times out on the deep pages of the biggest properties (birth and death dates, population), so a property it stops answering is read on from [QLever](https://qlever.dev)'s copy of Wikidata (`--deep-endpoint URL` to use another, `--wikidata-only` for none); one that still stops is asked for only for the most read articles, by name. A capital, CEO, spouse or leader that ended is left out, and one that applies only to a part (South Africa's three capitals) counts only when there is no other. Quantities are kept in metres and square metres whatever unit Wikidata has them in, dates only as precise as Wikidata knows them, and items (Canberra, a founder) by their English labels. They ride on the article's `profiles` line as `f-capital=Canberra` entries, which readers made before facts leave out, so `fetch-profiles` and `fetch-facts` keep each other's entries and can run in either order. The answer is shown above the results, with "From Wikidata", only when the search's subject names an article that has the fact; the node never guesses one.

### Leads and other names

To find an article by what its first sentences say ("triassic jurassic cretaceous" finds Mesozoic) and by the other titles that lead to it ("manubrium" leads to a section of Sternum), add each article's lead and other names from Wikimedia's weekly dump of its search index:

```sh
plumb fetch-leads --data /path/to/node-data --work /path/to/scratch
```

It downloads the dump's files (about 66 of 600 MB for English, three at a time), reads each into a small file in `--work` and deletes it, so a stopped run carries on from the files already read. The 2,000,000 most read articles (`--top`) get their lead, as many whole sentences of the paragraph before the first heading as fit in 300 characters, and up to ten other names: the titles that lead to the article but are not among its five most read aliases, the shortest first, including those that lead to one of its sections, which `fetch-pages` leaves out. They ride on the `profiles` line as `lead=` and `name=` entries, which readers made before leave out; `fetch-profiles` and `fetch-facts` keep them. A query that is one of an article's other names lists the article, though not as named, after the first sites. A query whose words the lead has with the article's names and description lists the article the same way, the lead that matches best first. The info box shows the lead, and a query that asks what something is ("what is a manatee", "define photosynthesis", "who was ada lovelace") is answered above the results with the first sentence of the article it names, "From Wikipedia".

### Word definitions

To answer searches that ask what a word means ("define anadromous", "prioritize meaning", "what is a portmanteau"), keep the `wiktionary` set of English words:

```sh
plumb fetch-pages --set wiktionary --data /path/to/node-data --work /path/to/scratch
```

It reads kaikki.org's English dictionary (about 3.3 GB of JSON lines that wiktextract makes from Wiktionary's dumps every few days) and keeps each word with the first usual sense of up to three of its parts of speech, in at most 300 characters, the words Wiktionary says most of (senses and translations) first. Senses that only point at another word ("plural of mouse") and proper names are left out. The set's pages are never listed among the results: a word is only looked up, by itself, for a query that asks what it means, and its meaning shown above the results, "From Wiktionary". A query with "define", "definition" or "meaning" gets the word first; "what is ..." gets the first sentence of the Wikipedia article it names first, and the word when there is none.

## Nodes in the network

A node with no set file (unless `--set-updates` leaves the set out, below), or fewer pages than it is set to keep, takes the file from a node it trusts (plumbsearch.org by default) over `/plumb/pages/1`, 1 MiB at a time, and stops once it has the pages it keeps: a node keeping 100,000 articles downloads about 10 MB, not the whole file. Every six hours it asks its trusted nodes how old their files are (a request for no bytes) and takes the newest one that is newer than its own, its own made file included, so a set made on one node reaches the others within hours, with no copying by hand. A newer file is taken only when it was made at least two hours ago (so one being built in steps, `fetch-pages` then `fetch-profiles`, `fetch-facts` and `fetch-leads`, is not taken half-way), is at least 90% the size of the node's own whole file, and, for the articles and `wikidata` sets, holds every kind of entry the node's own file does (facts by property, leads, other names, websites, profiles): a plain articles file never replaces one with facts and leads added, however new. A newer file that holds the same pages as the node's own is not taken either: copying a file between machines by hand gives it a later time but changes nothing in it. Each node works out what its whole files hold once per file (the kinds of entries in articles files and a SHA-256 digest of the text inside the gzip, so the same pages compressed again match), notes it next to the file (`<file>.layers`), and says so with the file's time. The file replaced is kept as `<file>.prev`, one step to roll back; to roll a bad set back on every node, put the good file back on the node that made it with a newer time (`touch`). A taken file keeps its maker's time, so it is handed on with that time. A newer file at most a quarter bigger than the node's own is taken by itself; a set that grew more (a Wikipedia file of 6.8 million pages in place of 2 million) can take more memory than the machine has, so it waits until the set is named in `plumb run --set-updates films,wikipedia-en,map` (those sets only, any size). `--set-updates off` turns this off: set files then change only by hand, and a set the node has no file of is not taken either (with a list, only the sets it names are), though one it has too few pages of still fills up. plumbsearch.org runs with it off for now. A node passes on only whole files, made with `fetch-pages` or taken whole, so a cut file never spreads. Nodes answer at most four such requests at once and 120 a minute from each node.

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

## Other Stack Exchange questions

The `stackexchange` set lists the most viewed questions of 33 other Stack Exchange sites where people ask practical things in plain words: Super User, Ask Ubuntu, Server Fault, Unix & Linux, Ask Different, Home Improvement, Seasoned Advice (cooking), Motor Vehicle Maintenance & Repair, Travel, Personal Finance & Money, The Workplace, English Language & Usage and more (`plumb_core::stack_exchange::SITES`). "how to unclog a drain" finds Home Improvement's question. Sites whose titles are mostly formulas, like Mathematics, are left out. Each question keeps what a Stack Overflow question does, plus its site, and is shown with its site's name. They are searched and placed the same way as Stack Overflow's.

The set is made from each site's whole dump in the same Internet Archive collection, a few GB in all, downloaded one site at a time:

```sh
plumb fetch-pages --set stackexchange --work /big/disk/dumps --data /path/to/node-data
```

`--max-per-site` (100,000 by default) caps each site's questions, `--min-score` works as for Stack Overflow, and `--drop-dumps` deletes each dump once it is read. A site whose dump can't be downloaded is left out with a warning.

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

## Songs and albums

The `music` set lists the songs and albums people listen to most (150,000 songs and 30,000 albums by default): "hey jude beatles", "hey jude by the beatles" and "hey jude song" find the song, "abbey road album" the album. It is made from MusicBrainz's core data dump (CC0, `mbdump.tar.bz2`, about 7 GB; nothing of the derived dumps is read) and how many people listened to each song and album, which ListenBrainz's popularity API (CC0) gives for 1,000 at a time:

```sh
plumb fetch-pages --set music --work /big/disk/dumps --data /path/to/node-data
```

A song is all the recordings of one title by one artist credit, remasters and live versions too, and its listeners are its recordings' together. ListenBrainz is asked about every song on at least two release groups (an album, a single, a compilation) and every song of the 30,000 albums most listened to, so an album's best-known songs are found even when no single put them out. ListenBrainz counts a song's listens on one canonical recording of it, often a single's or a compilation's, so the canonical recording of each one asked about is asked about too, from ListenBrainz's canonical data dump (CC0, `canonical_recording_redirect.csv`). Each song keeps its title, "Song by ARTIST, YEAR" as its description, "TITLE ARTIST" as another title, its most listened to recording's MusicBrainz id (its address on musicbrainz.org), its listeners, and the links MusicBrainz has for it: its lyrics on Genius, its Spotify track and its music video. Each album keeps the same, with "Album by ARTIST, YEAR", and its Spotify and Apple Music albums. A compilation, live album or DJ mix is no album here; a soundtrack is.

No lyrics are kept: they are copyrighted. "bohemian rhapsody lyrics" links the song's page on Genius, which MusicBrainz links the song's work to, shown first like a profile ("Listing, from MusicBrainz"); a song MusicBrainz knows no such page for gets a link to Genius's search for its title and artist instead. Nothing is fetched from Genius.

Song and album titles are too often the names of other things ("Dead Sea", "Notion", "Lord of the Flies"), so a song or album is only listed when asked for: by its title and its artist, or its title and "song" or "album". Its title alone lists the article of that name, never the song. For "TITLE lyrics", the song is looked up as "TITLE song" when no article of that name has a Genius page.

The answers ListenBrainz gives are kept in `--work` (`listenbrainz-albums.tsv`, `listenbrainz-recordings.tsv`) as they come, so a run that stops carries on where it left off. It keeps to the rate ListenBrainz sets. `--max-songs`, `--max-albums`, `--min-song-releases`, `--min-listeners` (20 by default) and `--musicbrainz-dump` (a downloaded `mbdump.tar.bz2`, or a directory of its tables), `--listenbrainz-canonical` (a downloaded canonical data dump) change what is read and kept. Reading the dump takes a few GB of memory.

## Films and TV shows

The `films` set lists the films and TV shows Wikidata knows best (150,000 by default, each with a page on at least 3 Wikipedias or other wikis, the most such pages first), from Wikidata's query service (CC0):

```sh
plumb fetch-pages --set films --data /path/to/node-data
```

Each keeps its English name (or else its English article's title, or its title in its own language), its other English names and original titles, "Film by DIRECTOR, YEAR · with CAST" or "TV series by CREATOR, 2008–2013 · with CAST" as its description (the three best-known cast members that fit), its Wikidata item, its English Wikipedia article when it has one, how many wikis have a page on it, and its listings: IMDb, Rotten Tomatoes, Metacritic, Letterboxd, TMDB, MyAnimeList and Netflix. Only Wikidata's identifiers for those are kept, which make links; nothing is copied from IMDb or TMDB. Films are items of film, animated film, documentary film or TV film; shows of TV series, miniseries, animated series, anime series or web series. Each class's label is checked before it is asked for.

Most films and shows with an English article are already listed by their titles as that article, so the set adds what tells them apart: their title followed by their year, a director's, creator's or cast member's name, or "movie" ("film") or "tv show" ("series", "tv") asks for one ("dune 2021", "dune david lynch", "breaking bad tv show"), which is then listed first as its article. One with no English article is listed by its title like a book, never before every site, and links its Wikidata item. "les dents de la nuit imdb" links its IMDb page like a profile.

`--max-films` and `--min-film-sitelinks` change what is kept. The fetch asks Wikidata a few hundred questions, waiting between them, and leaves out a batch Wikidata keeps failing to answer.

## Papers

The `papers` set lists the most cited scholarly works (2,000,000 by default, each cited at least 200 times), from OpenAlex's API (CC0): "attention is all you need" finds the paper. Each keeps only its title, "Paper by AUTHOR et al., YEAR, VENUE" as its description, its DOI (or OpenAlex id) and its citations. Like questions, papers are also found by most of their title's words. A paper cited more than 40,000 times for each year since it came out is left out as a data error. A paper's whole title of four words or more ("basic local alignment search tool"), or its title followed by its first author's name, its year, its venue or "paper" ("random forests breiman", "deep learning lecun nature"), asks for the paper, which then comes first like an asked-for book.

```sh
plumb fetch-pages --set papers --work /big/disk/dumps --data /path/to/node-data --min-citations 200
```

A paper that can be read free also keeps where, shown under it as "Free to read on arxiv.org" (and given to AI apps as `free_copy`): the publisher's own free PDF, else its copy on arXiv, else the best free copy Unpaywall's data knows of (an accepted version in a university repository, PubMed Central), all from OpenAlex, which carries Unpaywall's data. For a paper OpenAlex knows no free copy of, CORE (core.ac.uk), which gathers the papers of thousands of repositories, is asked by DOI when `CORE_API_KEY` holds a CORE API key (free for personal and research use; ask for one at core.ac.uk/services/api). CORE allows a free key about a thousand requests a day, so a run asks at most `--max-core-requests` times (900 by default, fifty papers each, the most cited first) and keeps what CORE answered in `DIR/core/` under `--work`, so each later run asks only about the papers left and gives every paper the copies found before. Only the copy's address is kept, never its text.

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

## Software docs

The `docs` set lists the pages of software docs sites: MDN, Python's docs, Rust's standard library and books, Go, Node.js, TypeScript, React, Vue, Next.js, Tailwind CSS, Django, Flask, NumPy, pandas, PyTorch, PostgreSQL, MySQL, SQLite, Docker, Kubernetes, Git, GitHub Docs, Java, C++ (cppreference), .NET, Kotlin, PHP, Ruby, Rails, Linux man pages, Bash, the Arch Wiki, nginx, Redis, MongoDB, Terraform, Godot and Bootstrap (`plumb_core::docs::DOCS_SITES`):

```sh
plumb fetch-pages --set docs --work /path/to/work --data /path/to/node-data
```

Each site's pages are those its sitemaps (named in its robots.txt, or `/sitemap.xml`) and its table of contents link to under its docs addresses (`https://docs.python.org/3/`), the shallowest first, at most 20,000 a site (`--max-docs-per-site`). Each page is fetched like a homepage in a crawl: robots.txt is obeyed for every address, a site is asked one page at a time with a second (or its `Crawl-delay`) between answers, and only redirects on the same site are followed. Sixteen sites are fetched at once. Each page keeps its title without the site's name ("Sorting Techniques — Python 3.14 documentation" is "Sorting Techniques"), its description (or else the start of its text), and its address. Its views are the site's weight (1 to 10) over how deep the page is under the site's docs, so a site's main pages come first. With `--work`, each site's pages are kept there (`docs-python.json`) as it finishes, and a run that stops carries on with the sites not yet done. `--docs-sites python,mdn` fetches only some.

A docs page's title alone ("Glossary") names nothing. It is named by its product's name and title ("python glossary", "glossary python") or, for pages whose title names a section ("Array.prototype.sort() — JavaScript"), by the section and title ("javascript array.prototype.sort()"), and then may come first. Like a question, it is also found by most of the words of its title, names and description ("sort a list in python") and listed after the best site, or first when the query asks for the whole page.

This is the first set of inner pages of good sites; universities, professors, companies and other kinds follow the same way (`plumb_crawl::fetch_site_pages`), each as a set of its own that a node turns on or off under Page sets.

## How pages and sites are listed together

- An article about a listed site (its Wikidata item's official website) goes under that site's result instead of in a place of its own.
- At most two other articles are listed, one when the first site is named by the whole query. An article whose title (or another title of it) is the whole query comes first, unless the first site is probably the website of what the query names: an article found for the query is about a company, product or service (its Wikipedia description says so), or the site is a government's, and the site is called after the article. A one-word query that spells the site but not the article ("robinhood", not "Robin Hood") also keeps the site first. So "tauri", "geico" and "robinhood" list tauri.app, geico.com and robinhood.com first, while "marie curie" lists the article before mariecurie.org. An official website, or a site better known than the article is read, also stays first, unless Wikipedia has an article about what that site is and the article the query names is read about twice as much, or read more at all when the site and the article are both called just what was searched for (Wikipedia gives the planet the title "Mars"): "mars" lists the planet before mars.com (Mars Inc.), and "napoleon" the emperor before napoleonnd.com (Napoleon, North Dakota). A site the query does not name in full and matches only weakly (text match under 0.5) never stays first over the article: "nikola tesla" lists the article before tesla.com. A site called exactly what was searched for stays first when the best page named so is about an organization or is a repository ("us bank" lists usbank.com before the article "U.S. Bancorp", "regex101" regex101.com before its repository). Repositories count a fifth less than articles, so "sonnet" lists the article on the poem before google-deepmind/sonnet. Articles named only in part come after three sites.
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

Each run builds an index of the pages first, which with every set takes minutes and several GB. `--pages-cache DIR` (or `PLUMB_EVAL_PAGES_CACHE=DIR`) keeps that index in `DIR` and reuses it while the page set files and the `plumb` binary are unchanged, so only the first run of a build pays for it; runs started together wait for one build, and the four most recently used indexes are kept. Scores are the same as without it.
