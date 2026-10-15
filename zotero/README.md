# Plumb Search for Zotero

A Zotero plugin that searches a Plumb node from inside Zotero and adds what you pick to your library: papers with their full details, Wikipedia articles, books, Stack Overflow questions, software packages and sites. It works with Zotero 8, 9 and 10. This page is for people who use it and for contributors; the plugin is JavaScript, the one language Zotero runs plugins in, and talks to a node only through `/api/search`.

![The Plumb Search window in Zotero](../docs/images/zotero.png)

- [Install](#install)
- [Use](#use)
- [Settings](#settings)
- [What an added item holds](#what-an-added-item-holds)
- [Privacy](#privacy)
- [How it reads a node's answer](#how-it-reads-a-nodes-answer)
- [Develop](#develop)
- [Release](#release)

The other direction is the [Zotero plugin for Plumb](../plugins/zotero/README.md), which lists items from your Zotero library in your own node's results.

## Install

1. Download `plumb-search-<version>.xpi` from the [releases](https://github.com/SueHeir/plumb-search/releases) tagged `zotero-v...`. If your browser offers to install it, save it instead.
2. In Zotero, open **Tools → Plugins**, click the gear, choose **Install Plugin From File...** and pick the file.

Zotero then keeps it up to date from [updates.json](updates.json).

## Use

- **Tools → Search Plumb...** opens the search window. Type a search and press Enter. Tick **Papers only** to search only the papers the node holds.
- Tick the results you want and click **Add to Zotero**. They go to the library or collection selected in Zotero's main window, which the window names next to the button; when that library cannot be edited, they go to My Library.
- **More results** asks again for more of the same search: 25 results at first, then 50, then 100.
- Right-click an item and choose **Search Plumb for This Item** to search for its title (a book's with its first author's last name).
- Click a result's address to open it in your browser.

## Settings

**Zotero → Settings → Plumb Search** (**Edit → Settings** on Windows and Linux):

| Setting | What it does |
| --- | --- |
| **Plumb node** | Where searches go: `https://plumbsearch.org` at first. For your own node, `http://127.0.0.1:7586` for the desktop app, `http://127.0.0.1:8080` for Docker and `plumb run`, or the address of a node on your network. An address typed without a scheme uses `http` for an IP address or a local name and `https` otherwise |
| **Look up full details when adding** | On at first; see below |

## What an added item holds

With **Look up full details when adding** on:

- A paper with a DOI or an arXiv ID is looked up by it, as **Add Item by Identifier** does, so it gets all its authors, its journal, volume and pages. Zotero then also fetches an open-access PDF when its settings say to.
- Any other result is read with Zotero's translators, as the Zotero Connector would save the page. A page that lists several items, such as many sites' front pages, and a page on your computer or local network are not read this way.
- When neither finds it, the item is made from what Plumb returned.

With it off, every item is made from what Plumb returned, and Zotero contacts no other site:

| Result | Zotero item |
| --- | --- |
| Paper | Journal article, or conference paper when the venue is a conference's, with its authors (the first author alone from older page sets), date, venue and DOI; a preprint when it is on arXiv with no other DOI |
| Book | Book with its author and year |
| Wikipedia | Encyclopedia article, with the article's first sentences as its abstract |
| Q&A | Forum post, with the question's tags as automatic tags |
| Code | Computer program, with the repository's owner as programmer |
| Package | Computer program with its version, release date and license |
| Podcast, music, film | Podcast, audio recording or film |
| Docs, site or page | Web page with its description as the abstract |

Each has its address and the date it was added. Items made this way say "Plumb Search" in their Library Catalog field, where the item type has one.

## Privacy

Searches go to the node you set and nowhere else; plumbsearch.org keeps no access log ([site/README.md](../site/README.md)). Adding an item with lookups on contacts what Zotero's own lookups would: the DOI or arXiv services for a paper, and otherwise the page's own site.

## How it reads a node's answer

[content/plumb.js](content/plumb.js) builds the request and reads the answer:

| Search | Request |
| --- | --- |
| Everything | `GET <node>/api/search?q=<query>&limit=<25, 50 or 100>&full=1` |
| Papers only | `GET <node>/api/search?q=<query>&limit=<n>&kind=paper` |

`results()` reads the rows in the node's order, each address once:

1. `assembled.rows`, from `full=1`. A `site` row is a site result, followed by its own `pages`; a `page` row is a page placed among the sites. Each page's fields (`set`, `url`, `title`, `description`, `lead`, `paper`, `package`) are under its `page`.
2. Without `assembled`, `pages` and then `hits`. A paper search answers with `pages` alone.

The page's `set` gives a result's kind: `wikipedia-*` is an article, `papers` a paper, `books` a book, and so on (`KINDS`). `listed()` counts the rows, and **More results** shows when the node listed as many as it was asked for. A 503 answer means the node is still building its index.

Only `http` and `https` addresses are kept, and the search window sets every title, description and address as text.

> [!IMPORTANT]
> This is the plugin's only contract with a node. A change to the shape of `/api/search`'s answer (`assembled.rows`, `pages`, `hits`) must keep it readable here, or change this plugin in the same pull request.

## Develop

| File | What it does |
| --- | --- |
| [content/plumb.js](content/plumb.js) | What a node answers and the Zotero items made from it. It uses nothing from Zotero, so its tests run in Node |
| [content/search.xhtml](content/search.xhtml), [search.js](content/search.js), [search.css](content/search.css) | The search window, and adding items |
| [bootstrap.js](bootstrap.js) | The menu items and the settings pane ([content/preferences.xhtml](content/preferences.xhtml)), added when the plugin starts and taken away when it stops |
| [prefs.js](prefs.js) | The settings' defaults |
| [locale/en-US/plumb-search.ftl](locale/en-US/plumb-search.ftl) | Every string the plugin shows |
| [manifest.json](manifest.json) | Its id, `plumb-search@plumbsearch.org`, version and the Zotero versions it runs on |
| [test/plumb.test.js](test/plumb.test.js) | Tests of `plumb.js`, on answers from plumbsearch.org in [test/fixtures/](test/fixtures), cut to the fields the plugin reads |

With Node.js 18 or newer, from the repository root:

```sh
node --test zotero/test/plumb.test.js   # the tests
zotero/build.sh                         # writes target/zotero/plumb-search-<version>.xpi
```

`build.sh` zips `manifest.json`, `bootstrap.js`, `prefs.js`, `icon.svg`, `content/` and `locale/` into the `.xpi` Zotero installs.

To run Zotero with the plugin straight from this folder, quit Zotero, put a file named `plumb-search@plumbsearch.org` in the `extensions` folder of your Zotero profile holding this folder's full path, delete the `extensions.lastAppBuildId` line from the profile's `prefs.js`, and start Zotero. Zotero's [plugin development guide](https://www.zotero.org/support/dev/client_coding/plugin_development) has more.

The [Zotero plugin workflow](../.github/workflows/zotero.yml) runs the tests and builds the `.xpi` for every pull request and push to `main` that changes `zotero/`, and uploads it as the `plumb-search-zotero` artifact.

## Release

1. Raise `version` in `manifest.json`.
2. Push the tag `zotero-v<version>`. The workflow checks that it matches `manifest.json`, builds the `.xpi` and attaches it to a draft release that is not marked as the latest, so the desktop app's downloads stay the latest release.
3. Publish the release.
4. Add the new version to `updates.json` on `main`, with its download address. Installed copies look for new versions there, so it goes in once the download is up.
