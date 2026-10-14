# Searching with Plumb

This guide is for people who search with Plumb, on [plumbsearch.org](https://plumbsearch.org) or on a node of their own: what a results page shows, the operators and settings a search takes, what a node can learn about you and keep, and the JSON API for scripts and apps. How the results are found and ranked is in [search.md](search.md); how to run a node is in [running-a-node.md](running-a-node.md).

- [The results page](#the-results-page)
- [Search operators](#search-operators)
- [Settings](#settings)
- [Add Plumb to your browser](#add-plumb-to-your-browser)
- [Search history](#search-history)
- [Edit these results and tune](#edit-these-results-and-tune)
- [About you and the welcome page](#about-you-and-the-welcome-page)
- [Your profile on other computers](#your-profile-on-other-computers)
- [The JSON API](#the-json-api)
- [Privacy](#privacy)

## The results page

![Plumb Search results for "nasa": nasa.gov with its key pages and Wikipedia line, a folded "Latest from nasa.gov" block, and an info box with official profiles](images/results.png)

Plumb indexes sites by their homepages, names, descriptions and the words other sites link to them with, plus sets of pages (Wikipedia articles, Stack Overflow questions, packages, papers and more). A results page puts these together, from the top:

| Part | What it is | When it shows |
|---|---|---|
| Places | Places that match, with a map the node draws itself. See [places.md](places.md). | The search asks for places somewhere ("pizza in denver", "coffee near me") |
| Official profile | A link to the profile asked for, from Wikidata or MusicBrainz | The search names someone and a site ("mrbeast youtube") |
| Instant answer | A worked-out answer (see below) | The search is a sum, a conversion, a question with a known answer |
| Source line | "From this site's own index.", or how the Plumb network was used | Always |
| Spelling | "Did you mean ...?", or "Showing results for ... Search instead for ..." | A word looks misspelled and the search has no operators |
| Results | Sites, and page rows from the [page sets](pages.md), best first | Always, or "No sites match ..." |
| Recent | Headlines from feeds, folded to one line, after the first result. See [news.md](news.md). | Feeds have current headlines that fit |
| More results | Loads ten more | Until 100 results are shown |
| Info box | Beside the results: a description, official site, country, official profiles, and links to Wikipedia (CC BY-SA) and Wikidata | One Wikipedia article or Wikidata item is clearly what the search is about; not for sums, or for operators other than quotes |

**A site result** shows the site's icon (or a colored first letter), domain, address, title and description, the country it belongs to when known, and its Wikipedia article on a line of its own. When the search names the site ("paypal", "paypal login"), up to 6 of its key pages show under it, with the one asked for first in bold. A search naming a part of a site, such as "youtube music", shows that part (music.youtube.com).

**A page row** comes from a page set: a Wikipedia article, a question, a film, a song. Package pages add the latest version and its date, the license, the install command and links to docs and code. Paper rows add publication dates and a citation count. See [pages.md](pages.md).

**More results** links to the same search with ten more results (`limit` raised by 10, up to 100 by `MAX_LIMIT`), and jumps to the first new one. The results above it stay as they were. When there is nothing new, the page says "No more results."

### Instant answers

Answers are worked out by [plumb-answer](../crates/plumb-answer/) and the node:

| Kind | Example |
|---|---|
| Calculation | `12*(3+4)`, `sqrt 2`, `20% off 50` |
| Conversion | `10 km in miles` |
| Currency | `100 usd to eur` (European Central Bank daily rates) |
| Time | `time in tokyo`, `3pm est to pst` |
| Date of a holiday | `when is thanksgiving`, `days until halloween` |
| Weather | `weather in denver` (from Open-Meteo) |
| Fact | `capital of australia` (from Wikidata) |
| Definition | `what is a manatee` (first sentence of the Wikipedia article) |
| Help | `depression help`: where to get help now |

"Weather", "what time is it" and "near me" use the town you gave in [About you](#about-you-and-the-welcome-page) when the search names none.

### Searching other engines

A word starting with `!`, first or last in the search, sends the search to another engine instead of searching Plumb. These work even while a node is still setting up.

| Bang | Engine |
|---|---|
| `!g`, `!google` | Google |
| `!ddg`, `!d` | DuckDuckGo |
| `!b`, `!bing` | Bing |
| `!brave` | Brave Search |
| `!sp` | Startpage |
| `!w` | English Wikipedia |
| `!yt` | YouTube |
| `!gh` | GitHub |
| `!a` | Amazon |
| `!m` | Google Maps |
| `!r` | Reddit |

A node started with `--web-search ENGINE` (duckduckgo, google, bing, brave or startpage) also shows a "Search the web with ..." link above the results. Plumb only links to the other engine; it never fetches its results.

## Search operators

| Operator | Keeps | Example |
|---|---|---|
| `site:host` | Results on that host and its subdomains. A value without a dot is a top-level domain. | `site:github.com rust`, `site:gov` |
| `-site:host` | Results not on that host | `laptops -site:amazon.com` |
| `"exact words"` | Results whose name, title or description holds the words in that order | `"open source" search` |
| `-word`, `-"phrase"` | Results without that word or phrase (its plural too, and in the address: `-amazon` drops amazon.com) | `jaguar -car` |

Curly quotes work, and a quote left open runs to the end of the search. Words that only look like operators stay words: `spider-man`, `--save-dev`, `-b`, a lone `-`, and `site:` with nothing after it. With `site:`, the site itself is listed after its matches, with a link into the site's own search when it has one.

A search with operators is searched exactly as typed (no spelling correction), finds no places, and gets an info box only when its operators are quotes. Operators work the same on the results page, `/api/search`, the SearXNG JSON, network searches, the MCP tools, the Recent headlines and [private search](private-search.md), where only the words pick buckets.

### Papers

A paper search lists only papers, newest or most relevant first:

| Way | Example |
|---|---|
| In the search | `transformers after:2025 before:2026-02-28 sort:newest` |
| A phrase | `diffusion models papers published in 2025` (also "research published in") |
| Address parameters | `kind=paper`, `after=2025`, `before=2026-02-28`, `order=newest` or `relevance` |

Dates are a year or a full date, and bounds include their end. A value that is not a date leaves the word a word (`before:sunset lyrics` is an ordinary search). Bounds that conflict get a 400 error. The page says how many indexed papers have a known publication year and day.

## Settings

The gear beside the search box holds the settings. They travel in the page's links as address parameters, not in cookies, so a link to a search carries its settings.

| Setting | Choices | Parameter |
|---|---|---|
| Country | Automatic, Any country, or a country, whose sites rank a little higher | `country=US`, `any`, `auto` |
| Only this country | Only sites from that country, and global ones | `only=1` |
| Language | Any language, or one of 17: en, de, es, fr, it, nl, pl, pt, sv, tr, ru, uk, ar, hi, ja, ko, zh | `lang=de`, `any` |
| Safe search | Off, Moderate (default), Strict | `safe=off`, `moderate`, `strict` |
| Recent news | Folded (default), Open, Off | `news=expanded`, `off` |
| Use the Plumb network for search | Also ask other nodes | `net=1` |
| History choices | See [Search history](#search-history) | `hist=1`, `hs`, `hr`, `hl`, `ht` |
| Private search | A switch to [/private](private-search.md), where the browser looks up the results itself | Shown where the node offers it |
| Search exactly as typed | No spelling correction | `exact=1` |
| Show ranking details | Each result's scores | `debug=1` |

**Country, automatic.** A node started with `--country` uses that country. Otherwise the country in the browser's language (`en-US` gives the US), then the computer's region settings, then the United States. A node started with `--only-country` starts pages with "Only this country" on.

**Language.** The default is the node's `--lang`, else the browser's first language when the gear offers it, else English. A site is kept when its homepage says it is in that language, or, without a declaration, when its title and description are written in that language's script or, for Latin script, use its common words. Sites in a language Plumb cannot tell are kept, and so is a site the search names, whatever its language.

**Safe search.** Moderate leaves out sites on a blocklist of adult sites (kept fresh by the node), sites Wikidata says are adult, and sites and pages whose name, title or description is plainly adult. Strict also leaves out suggestive ones. The same rules apply to places and headlines. Private search applies them without the blocklist.

**Use the Plumb network.** The node also asks other Plumb nodes, without sending the search's text (see [network.md](network.md)). Results that only other nodes found are tinted. The setting is grayed out on a node that is not in the network.

## Add Plumb to your browser

Every page names an OpenSearch description, `/opensearch.xml`, whose address template is `https://your-node/search?q={searchTerms}`. Most browsers offer to add it:

- **Chrome, Edge and others:** look for it in the browser's search engine settings and make it the default there.
- **Firefox:** right-click the address bar and choose **Add "Plumb Search"**, or open Settings > Search > Search Shortcuts > Add, with the name `Plumb Search` and the address `https://your-node/search?q=%s`. A node also explains this at `/add-to-firefox`.

The address comes from the `Host` the browser used, and is `https` when a proxy in front of the node says so with `X-Forwarded-Proto`.

## Search history

A node can keep a history for each browser that searches it. It is on in the desktop app and with `--profile desktop`, and off on servers unless started with `--search-history` or turned on with "Remember searches" in the panel.

The node knows a browser by a cookie holding a random 128-bit id (`plumb_profile`) and keeps its history in `DIR/history/<id>.json`, for that browser only. It keeps the last 200 searches and 500 sites opened. The gear then shows four choices:

| Choice | Default | What it does |
|---|---|---|
| Show my past searches | On | The home page lists up to 8 recent searches |
| Put sites I opened before first | On | A site opened before ranks higher, most for the same search, with "You opened this before" under it |
| Learn from my clicks which boxes, like maps, I use | On | Boxes passed by for searches like this one (places, headlines) fold; headlines read come open; a site passed over search after search moves down a little |
| Use my searches to train Plumb's ranking | Off | Keeps short searches (no e-mail address or long number) apart from the history and without the profile, for training the ranking. Clearing the history does not remove them. |

`/history` lists the searches and the sites opened from them, what was learned with a button to forget it, and **Clear my history**. Its buttons refuse posts from other sites.

## Edit these results and tune

On a node that keeps history, results pages link to **Edit these results** (`edit=1`). Each result then gets buttons: **Up** and **Down** (higher or lower for this search) and **Hide** (not for this search); pressing one again undoes it. Boxes get **Fold for this search**, **Fold for searches like this** and **Always open**. Hidden results are listed last while editing and left out otherwise.

What you move teaches the node which kinds of result you like: official sites, encyclopedias and dictionaries, forums and Q&A, code and software docs, video, social media, shops, news sites, government and schools, well-known sites, small lesser-known sites, and sites from your country. Every search then leans that way a little, and a result can say why ("You like code and software docs").

`/tune` runs a round of this: 5 searches picked at random from a list of 24 ("python sort a list", "pizza in denver", "taylor swift" and the like), each in edit mode, then a page saying what it learned. These searches are not kept as past searches.

## About you and the welcome page

`/about` (also reached from the gear) says what results should favor:

| Field | Effect |
|---|---|
| Your town | Used for "near me", weather and the time. The page says where it looks ("around Denver, CO, US") or that it does not know the town. |
| Topics and other interests | 33 topics to tick and your own, up to 40 in all. A matching site ranks a little higher, with "Matches your interest: ..." under it. |
| Sites you like | Up to 100, ranked first, with "One of your sites" |
| Sites never shown | Up to 100, with their subdomains |
| Kinds of results | Off, Less, Normal or More of Wikipedia articles, films and TV shows, songs and albums, podcasts, books, papers, Stack Overflow and other Stack Exchange questions, GitHub repositories, software packages and docs, reference pages, pages of universities, companies, governments and museums, official profiles, and pages the node found for its searches |

These apply to the results page after the results are found. They are never part of a network search, and the JSON API does not use them. **Forget all of this** clears them.

Where it is kept depends on the node. A node that keeps history keeps it beside the history (`DIR/history/<id>.about.json`). A node that keeps no history keeps none of it: it is stored in a cookie in your browser (`plumb_about`, at most 3,600 bytes; when it is too big, the last lines of the longest list are dropped and the page says so) and read with each search.

The home page invites a new browser to fill it in ("Tell Plumb your city and what you're into"). `/welcome` is the same form with **Save and start searching** and **Skip for now**. The invitation stops once the browser saves the form, or says **Skip for now** or **No thanks**.

All Plumb cookies are `HttpOnly`, `SameSite=Lax`, for the whole site and last a year.

## Your profile on other computers

On a node that keeps history, `/link` shares one browser's history, interests and what was learned with your other browsers and nodes:

1. In the browser that has the profile, choose **Make a link code**. The code looks like `ABCD-EFGH@<node id>` and works once, for 10 minutes.
2. Paste it on `/link` in the other browser. On the same node, that browser then uses the same profile. On another node, the profile is copied over the Plumb network and the two copies are kept alike, checked every minute and synced after changes and at least every 5 minutes.

A profile can be linked to up to 16 nodes. After 30 wrong codes in 10 minutes, the node refuses all codes until the window ends. **Stop sharing** leaves each node its copy. A node off the network can only link browsers on itself. The profile goes only to linked nodes, over encrypted connections.

## The JSON API

Scripts, apps and AI tools read the same results as JSON. For AI apps and local models, [local-llms.md](local-llms.md) and [mcp.md](mcp.md) have the setup.

| Address | Returns |
|---|---|
| `GET /api/search?q=...` | A JSON list of site results |
| `GET /api/search?q=...&full=1` | An object with everything the results page shows |
| `GET /search?q=...&format=json` | The SearXNG JSON format |
| `POST /api/websearch` | Open WebUI's external web search format |
| `GET /api/recent?q=...` | The Recent headlines block ([news.md](news.md)) |
| `GET /api/network/search?q=...` | A search of other Plumb nodes only, with what each step did; 404 when the node is not in the network |
| `GET /api/status` | The node's state: setting up or ready, index size, network, storage. Never cached. |

`/api/status`, `/api/recent` and `/api/network/search` are served by `plumb run` nodes, not by `plumb serve`.

### Parameters

The search addresses take the same parameters as the results page:

| Parameter | Meaning |
|---|---|
| `q` | The search. Cut to 200 characters. |
| `limit` | Results, 10 by default, at most 100. Not a whole number: 400. |
| `country`, `only`, `lang`, `safe`, `exact`, `net` | As in [Settings](#settings) |
| `kind`, `after`, `before`, `order` | [Paper search](#papers) |
| `full` | `1` for the whole page as an object (`/api/search`) |
| `format`, `pageno`, `safesearch`, `categories`, `time_range` | SearXNG (`/search?format=json`) |

### What comes back

A site result (`/api/search`) has `domain`, `url`, `title`, `description`, `score`, `text_score` and `link_score`, and when they apply `country`, `named` (the search names this site), `official` and `key_pages`.

With `full=1`, the object holds `hits` (the sites), `pages` (page rows), `assembled` (the page's order: `rows` of `{"kind": "site", "site", "pages"}` or `{"kind": "page", "page"}`, with `places`, `recent`, `answer` and `profile`), and, when there are any, `answer`, `profile`, `info`, `places`, `recent`, `spelling`, `site_search` and the node's plugin results. The [Zotero plugin](../zotero/) reads `assembled.rows`, `pages` and `hits`.

The SearXNG format (`results`, `answers`, `suggestions`, `infoboxes`, `number_of_results`, plus `recent` and `places`) and Open WebUI's are described in [local-llms.md](local-llms.md).

The API applies the node's defaults and the parameters given, not a browser's history or About you.

### While a node sets up

Until its first index is ready, a node's `/` and `/search` show a setup page that reloads every 5 seconds, and `/api/search` and the SearXNG JSON answer 503 with `Retry-After: 5` and `{"error", "phase", "step"}`.

### Limits

Network searches are limited per client: a burst of 20, then 20 a minute. Over that, `/api/network/search` answers 429 with `Retry-After`. A network search waits at most 4 seconds for other nodes.

## Privacy

What a node sees and keeps:

- **The search.** The node you search sees your search and your address, like any search engine. When a search fails, the node's log leaves out its text. Use [private search](private-search.md) to keep the text in your browser.
- **History.** Kept only on nodes that keep history (above), per browser, on that node. On a node that keeps none, About you lives in your browser's cookie.
- **Clicks.** Result links go through `/go` when the node keeps your history, shares popularity (`--share-popularity`, which reports picks encrypted and never with who picked) or runs a ranking experiment. `/go` notes the pick and sends you on.
- **Wanted pages.** A node with `--wanted-pages` (on in the desktop app) keeps the words of its searches for 30 days in `DIR/wanted`, to go looking for inner pages its searches want.

What a node never sends:

- **Network searches** ask other nodes for groups of sites by number, never the search's text. See [network.md](network.md).
- **Answers** fetch only what they need: currency rates from the European Central Bank, with nothing of the search; the weather for a town's coordinates rounded to about 1 km.
- **Plugins** the node's owner installed may fetch what they were written for. See [plugins.md](plugins.md).

What the pages do:

- Results pages run no JavaScript and load nothing from other servers: their Content-Security-Policy allows no outside sources, site icons are embedded in the page, and the map is drawn by the node. `/private` runs its own script, from the node only.
- Search pages send `Referrer-Policy: no-referrer` and result links carry `rel="noreferrer"`, so the sites you open do not learn what you searched for.
