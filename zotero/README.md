# Plumb Search for Zotero

A Zotero plugin that searches Plumb from inside Zotero and adds what you pick to your library: papers with their full details, Wikipedia articles, books, Stack Overflow questions, software packages and sites. It works with Zotero 8, 9 and 10.

![The Plumb Search window in Zotero](../docs/images/zotero.png)

## Install

1. Download `plumb-search-<version>.xpi` from the [releases](https://github.com/SueHeir/plumb-search/releases) (the ones tagged `zotero-v…`). If your browser offers to install it, save it instead.
2. In Zotero, open **Tools → Plugins**, click the gear and choose **Install Plugin From File…**, then pick the file.

Zotero then keeps it up to date.

## Use

- **Tools → Search Plumb…** opens the search window. Type a search and press Enter. Tick **Papers only** to search only the papers Plumb holds.
- Tick the results you want and click **Add to Zotero**. They go to the library or collection selected in Zotero's main window, which the window names next to the button; when that library can't be edited, they go to My Library.
- **More results** asks for more of the same search, up to 100.
- Right-click an item and choose **Search Plumb for This Item** to search for its title (a book's with its author).
- Click a result's address to open it in your browser.

## What an added item holds

With **Look up full details when adding** on, as it is at first:

- A paper with a DOI or an arXiv ID is looked up by it, as **Add Item by Identifier** does, so it gets all its authors, its journal, volume and pages. Zotero then also fetches an open-access PDF when its settings say to.
- Any other result is read with Zotero's translators, as the Zotero Connector would save the page: Wikipedia, Stack Overflow, GitHub and many news and journal sites have their own, and most other pages have embedded metadata. A site's front page that lists many items is not read this way.
- When neither finds it, the item is made from what Plumb returned.

With it off, every item is made from what Plumb returned, and Zotero contacts no other site:

| Result | Zotero item |
| --- | --- |
| Paper | Journal article or conference paper with its first author, year, venue and DOI; a preprint when it is on arXiv with no other DOI |
| Book | Book with its author and year |
| Wikipedia | Encyclopedia article with the article's first sentences as its abstract |
| Q&A | Forum post, with the question's tags as automatic tags |
| Package | Computer program with its version, release date and license |
| Code, podcast, music, film | Computer program, podcast, audio recording or film |
| Site or page | Web page with its description as the abstract |

Each has its address and the date it was added. Items made this way say "Plumb Search" in their Library Catalog field, where the item type has one.

## Settings

**Zotero → Settings → Plumb Search** (**Edit → Settings** on Windows and Linux):

- **Plumb node**: where searches go, `https://plumbsearch.org` at first. To search your own node, give its address: `http://127.0.0.1:7586` for the desktop app, `http://127.0.0.1:8080` for Docker and `plumb run`, or the address of a node on your network.
- **Look up full details when adding**: as above.

## Privacy

Searches go to the node you set and nowhere else; plumbsearch.org keeps no access log. Adding an item with lookups on contacts what Zotero itself would: doi.org and Crossref or DataCite for a DOI, arXiv for an arXiv ID, and otherwise the page's own site, unless it is on your computer or local network.

## Development

The plugin is JavaScript, the one language Zotero runs plugins in.

- `content/plumb.js`: what a node answers and the Zotero items made from it. It uses nothing from Zotero, so its tests run in Node.
- `content/search.xhtml`, `search.js` and `search.css`: the search window, and adding items.
- `bootstrap.js`: the menu items and the settings pane (`content/preferences.xhtml`), added when the plugin starts and taken away when it stops.
- `locale/en-US/plumb-search.ftl`: every string the plugin shows.

With Node.js 18 or newer:

```sh
node --test zotero/test/plumb.test.js   # the tests
zotero/build.sh                         # target/zotero/plumb-search-<version>.xpi
```

To run Zotero with the plugin straight from this folder, quit Zotero, put a file named `plumb-search@plumbsearch.org` in the `extensions` folder of your Zotero profile holding this folder's full path, delete the `extensions.lastAppBuildId` line from the profile's `prefs.js`, and start Zotero. Zotero's [plugin development guide](https://www.zotero.org/support/dev/client_coding/plugin_development) has more.

## Releasing

1. Raise `version` in `manifest.json`.
2. Push the tag `zotero-v<version>`. The Zotero plugin workflow checks that it matches `manifest.json`, builds the `.xpi` and attaches it to a draft release that is not marked as the latest, so the desktop app's downloads stay the latest release.
3. Publish the release.
4. Add the new version to `updates.json` on `main`, with its download address. Installed copies look for new versions there, so it goes in once the download is up.
