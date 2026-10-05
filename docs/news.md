# Recent headlines

Plumb keeps no article text, so it cannot search the news the way a full-text engine does. What it can do is read the feeds sites publish for exactly this purpose (RSS and Atom), keep each new post's title, link and date for a week, and show the ones that fit a search in a **Recent** block after the first result.

## What a search shows

* **A site's latest posts.** When the search names a site ("bbc", "theverge.com"), or asks for its news ("verge news") and no headline is about the other words, the block is "Latest from *site*": its four newest posts.
* **Headlines about the words.** Otherwise the block lists the five newest headlines (at most two per site) that hold every word of the search in their title, plurals folded ("election" finds "Elections"). It shows only when two or more sites published such a headline in the last three days, or when the search asks for news ("election news", "latest rust"), which looks back the whole week.

`GET /api/recent?q=...` returns the same block as JSON.

## Where the headlines come from

* **Feeds of the best-ranked sites.** At each index build a node picks its best-ranked sites that answered their last crawl and redirect nowhere: 3,000 for a server, 300 for the desktop app, `--news-feeds N` to change, 0 for none. A homepage crawl notes the feed a page names (`<link rel="alternate" type="application/rss+xml">` or Atom); a watched site with none known has its homepage fetched once for it, and a site without a feed is looked at again a week later.
* **Checked politely.** Each feed is checked at most once an hour, with robots.txt obeyed and the feed's `ETag` and `Last-Modified` sent back, so an unchanged feed costs one short answer. A quiet feed is checked half as often each time, down to twice a day; one with news goes back to hourly. Checks pause with the crawl (background updates off, a pause, crawl hours, the day's download limit) and count towards the download limit.
* **Shared with the network.** New headlines go out in a signed batch, each site's on a line of its own (`{"news_of": ...}`), like icons. A node takes them only from its own and trusted crawlers, never into its site records, and keeps only headlines on the site itself, from the last week, at most ten per site.

## What is kept

`DIR/news/headlines.json` (headlines by site) and `DIR/news/feeds.json` (the watched sites and each feed's state). A week of 3,000 sites' headlines is a few MB.
