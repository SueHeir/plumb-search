# Plugins

A plugin adds results from a source Plumb does not crawl itself, such as a
site's own search API. Plugins are written in Rust with the `plumb-plugin`
crate, compiled to WebAssembly, and installed by a node's owner on their own
node. Their results show on that node's results page and in its JSON, MCP and
SearXNG answers, in a block named for the plugin after the first result.

Plumb comes with no plugins installed, and plumbsearch.org runs none. A plugin
is the choice of whoever installs it, and so is keeping to its source's terms:
prefer a source's official API (with your own key where it needs one) over
scraping its pages.

What a plugin finds stays on the node that ran it. It is never shared with
other nodes, never put in the node's records or index, and never sent with a
network search.

## The sandbox

Each search runs the plugin in a fresh WebAssembly sandbox
([wasmi](https://github.com/wasmi-labs/wasmi)). Inside it a plugin can:

- read the query, the safe search and language settings, what the node took
  the search to be about, and its own `config.json`;
- make requests (GET, POST, PUT, PATCH, DELETE, HEAD) to the hosts its
  `plugin.json` lists, and only those (redirects to other hosts are not
  followed), and read the answers' status, headers and body;
- hand back results, with pictures, badges and buttons;
- when the node's owner presses one of its buttons, do what the button says.

It cannot read or write files, open any other connection, read the clock or
the environment, or learn who searched. These limits hold for every search:

| Limit | Value |
| --- | --- |
| Time a search waits for a plugin's results | 4 seconds, or its `seconds` (at most 10) |
| Time a button press may take | 10 seconds |
| Requests per plugin per search or button press | 4 |
| Response size | 2 MB |
| Memory | 64 MB |
| Work | about 2 billion WebAssembly instructions |
| Results kept per plugin | 10 |
| Buttons per result | 3, with up to 4 KB of data each |
| Picture per result | 256 KB: PNG, JPEG, GIF or WebP |
| Searches one plugin runs at once | 4 (more searches go without it) |

A plugin still running after its time is left out of that search, but it is
not stopped at once: it can make no more requests (or finish one) after that,
yet it keeps computing in the background until it returns or uses up its work
limit.

Results are checked before they are shown: only `http`, `https` and `magnet`
links with a title are kept, text is put on one line and shortened, everything
is escaped like any other text from the web, and safe search leaves out
results that say they are adult. Pictures come only from the plugin's own
hosts; the node fetches them and puts them in the page, so the searcher's
browser asks nobody for them. A plugin's results for the same search are
reused for 10 minutes, or for its `cache_seconds`, so a busy node does not ask
its source the same thing over and over; a plugin showing live status (a
download's progress) sets `"cache_seconds": 0`.
A plugin that fails, runs too long or runs out of work is left out of that
search, and the node's log says why.

## When a plugin runs

A plugin runs when a search starts or ends with one of its keywords: with the
example's keywords, `hn rust async` and `rust async hacker news` both run it,
and it searches for `rust async`. A keyword alone (`hn`) does not, since that
is a search for the site. A plugin with `"always": true` runs for every
search; use it only for a source that allows that many requests.

A plugin can also run for what a search is about. When the node recognises a
search as one thing (the article its info box would show), it tells plugins
that thing's Wikidata item and its identifiers on other services, such as
`imdb`, `tmdb-movie`, `tmdb-tv`, `musicbrainz-artist` or `steam`. A plugin
whose `ids` lists one of those runs for such searches without a keyword, and
can look the thing up by its identifier rather than by its name: with
`"ids": ["tmdb-movie"]`, `paddington 2` runs it with `about.ids["tmdb-movie"]`
set to `346648`. `"ids": ["wikidata"]` runs it for anything with a Wikidata
item. Plugins start once the node's own search is done, so that this is known,
and every plugin gets it, whatever ran it. (The MCP server's `search` tool
runs plugins by their keywords alone.)

## Marking up the node's own results

A plugin can also look at the node's own results for a search and add to
them: a badge on a result, shown with the plugin's name ("Shelf: Not saved"),
buttons on it, or leaving it off the page. It exports `plumb_annotate`
(`plumb_plugin::annotate!(annotate)`), which is shown the results (up to 30):
each one's address, title, site, and, when the node knows, what it is about,
with the same Wikidata item and identifiers as a search's `about`. A Wikipedia
article about a film is about that film; a site's result is about what the
article it carries is about. The plugin answers with a `Note` for the results
it has something to say about.

A plugin with `ids` is shown only results about something with one of those
identifiers, and runs only when there is one; without `ids` it is shown every
result of every search, so mind its source's limits. It runs beside the
plugins' own searches, with the same time limit and cache. A plugin that only
marks up results needs no keywords and no `plumb_search`
(`plumb_plugin::annotate_only!(annotate)`).

```rust
use plumb_plugin::{Action, Error, Note, Shown};

fn annotate(shown: &Shown) -> Result<Vec<Note>, Error> {
    let saved = saved_ids(&shown.config)?; // one request for the whole list
    Ok(shown
        .results
        .iter()
        .filter_map(|result| {
            let id = result.about.as_ref()?.wikidata.clone()?;
            Some(if saved.contains(&id) {
                Note::new(result.id).badge("Saved")
            } else {
                Note::new(result.id)
                    .badge("Not saved")
                    .action(Action::new("Save", serde_json::json!({ "id": id })))
            })
        })
        .collect())
}

plumb_plugin::annotate!(annotate);
```

Badges show to anyone who can search the node; buttons, as everywhere, only
to its owner. A result a plugin hides is left off this node's results page, for everyone
who searches this node (notes are never sent to other nodes);
`/api/search?full=1` lists the notes, hides included, as `plugin_notes` by the
results' addresses. Results from other nodes are not marked up.

## Buttons

A result can carry up to three buttons ("Add", "Download"). Pressing one runs
the plugin's `act` with the button's data, in the same sandbox, and shows the
line it hands back ("Added to your list"); the plugin's saved results are then
dropped, since what it shows may have changed.

Buttons are for the node's owner. They show, and work, only on the computer
the node runs on, the same rule as the panel's settings: not from another
machine, not through a reverse proxy, and not from a page of another site
(each results page's buttons carry a token that only the node knows). Anyone
else who can search the node sees the results without buttons. They still see everything else a plugin shows (titles, badges,
pictures), so on a node other people can search, install only plugins whose
results you are happy for them to see. Put the plugin
on the node of the computer you browse on; its `hosts` can name other machines
on your network.

## Pages and browser extensions

A plugin whose `pages` lists sites (`"pages": ["*.example.org"]`) can say
something about a page on them. `GET /api/plugins/page?url=<address>` runs
those plugins with `page` set to the address and answers with their results,
as JSON: `{"plugins": [{"plugin", "name", "results": [...]}]}`. A browser
extension can ask it about the page open in a tab and show what comes back,
buttons included.

A program presses a button with `POST /api/plugins/act`, a JSON body of
`{"plugin": "<folder name>", "data": <the action's data>}`, answered with
`{"message": "..."}` or `{"error": "..."}`. As on the results page, only from
the computer the node runs on, to a local address such as
`http://127.0.0.1:8080`, and not from a page of another site: browser
extensions' own origins are fine.

## Install a plugin

A plugin is a folder:

```text
plugins/
  hacker-news/
    plugin.json    what it is, the hosts it may reach, its keywords
    plugin.wasm    the plugin itself
    config.json    optional: your settings for it, such as an API key
```

Put the folder in `plugins/` inside the node's data folder, then restart the
node:

- **Docker:** `/data/plugins/hacker-news/` in the container's volume, then
  `docker compose restart`.
- **Desktop app:** `plugins/hacker-news/` in the app's data folder (see
  [Where data lives](desktop.md#where-data-lives)), then quit and reopen the
  app.
- **`plumb run --data DIR`:** `DIR/plugins/hacker-news/`.
- **`plumb serve`:** pass `--plugins` with the folder that holds the plugin
  folders.

Read `plugin.json` before installing: `hosts` lists everywhere the plugin can
send your users' searches. A host is allowed on every port, so a plugin that
lists `localhost`, `127.0.0.1` or a machine on your network can reach every
service on it. On start the node lists each plugin it loaded and
its hosts in its log and the panel's activity list. A plugin that cannot load
is left out with a warning. To remove a plugin, delete its folder and restart.

Try a plugin before installing it:

```sh
plumb try-plugin --plugin plugins/hacker-news hn rust async
```

This runs it once in the same sandbox and prints what it found as JSON, or why
it found nothing. Give it a page's address to try a page lookup, and
`--act '<data>'` (an action's `data` from its results) to press a button.
`--annotate results.json` has it mark up results instead: a JSON list of
results as a node shows them (`{"id", "url", "title", "site", "about"}`).

## Plugins in this repository

Each folder in `plugins/` has a `plugin.json` and, where it needs one, a
`config.example.json` to copy to `config.json`. Build one with
`cargo build --release -p plumb-plugin-<folder> --target wasm32-unknown-unknown`
and copy `target/wasm32-unknown-unknown/release/plumb_plugin_<folder>.wasm`
into its folder as `plugin.wasm`.

| Plugin | What it adds | Needs |
| --- | --- | --- |
| `hacker-news` | `hn rust async`: Hacker News stories. | Nothing. |
| `youtube-music` | `ytm` or `yt` searches: songs and videos from YouTube and YouTube Music, and a channel's uploads for searches about someone with one. | A YouTube Data API key (see its README). |
| `reddit` | `reddit` searches: Reddit threads. | Your own Reddit app keys (see its README). |
| `github` | `gh http client`: repositories with stars, language and last push. With a token: "Starred" badges, star counts on GitHub results among the node's own, and Star and Unstar buttons. | Nothing to search (GitHub allows 10 searches a minute without a token); a [fine-grained token](https://github.com/settings/personal-access-tokens) with read and write access to Starring for the rest. |
| `steam` | `steam portal` or `my games portal`: games in your library with hours played. Results about a game get "Owned, 25 h" or "On your wishlist". | A [Steam Web API key](https://steamcommunity.com/dev/apikey) and your SteamID64. If your library comes back empty, set your Steam profile's game details to public. |

## Write a plugin

Start a library crate:

```toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
plumb-plugin = { git = "https://github.com/SueHeir/plumb-search", tag = "v0.1.0" }
serde = { version = "1", features = ["derive"] }
```

Write the search, and hand it to `plugin!`:

```rust
use plumb_plugin::{encode, get, Error, Item, Query};
use serde::Deserialize;

#[derive(Deserialize)]
struct Found {
    items: Vec<Article>,
}

#[derive(Deserialize)]
struct Article {
    title: String,
    url: String,
    summary: String,
}

fn search(query: &Query) -> Result<Vec<Item>, Error> {
    let key = query.config["api_key"]
        .as_str()
        .ok_or_else(|| Error::Other("no api_key in config.json".into()))?;
    let url = format!(
        "https://api.example.org/search?q={}&key={}",
        encode(&query.terms),
        encode(key)
    );
    let found: Found = get(&url)?.json()?;
    Ok(found
        .items
        .into_iter()
        .map(|item| Item::new(item.title, item.url).snippet(item.summary))
        .collect())
}

plumb_plugin::plugin!(search);
```

`plugins/hacker-news` in this repository is a complete example: it asks
Hacker News's public search API for stories. `plugins/youtube-music` is a bigger
one: YouTube and YouTube Music through the YouTube Data API with the owner's
own key, two requests per search, and a channel's uploads for searches about
someone with a YouTube channel. `plugins/reddit` signs in to Reddit's
official API with the owner's own app keys from `config.json` (see its
README). Build a plugin with

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
```

and copy `target/wasm32-unknown-unknown/release/my_plugin.wasm` to
`plugin.wasm` in the plugin's folder, next to its `plugin.json`:

```json
{
  "name": "Example",
  "about": "Articles from example.org, through its search API.",
  "hosts": ["api.example.org"],
  "keywords": ["ex", "example"]
}
```

| Field | Meaning |
| --- | --- |
| `name` | What the results page calls it: "From Example". Up to 60 characters. |
| `about` | One line about where its results come from. |
| `hosts` | The hosts it may fetch from: `api.example.org`, `*.example.org` for every subdomain (not `example.org` itself), a name on your network such as `nas`, or an address such as `127.0.0.1` or `[::1]`. Without a port only ports 80 and 443 are allowed; add one to reach another: `127.0.0.1:7878`, `nas:8989`. |
| `keywords` | Words or phrases that run it at the start or end of a search. |
| `always` | `true` to run it for every search. |
| `ids` | Run it, without a keyword, for searches about something with an identifier on one of these services (see [When a plugin runs](#when-a-plugin-runs)). |
| `pages` | Sites whose pages it can say something about, for [page lookups](#pages-and-browser-extensions). |
| `cache_seconds` | How long its results for a search are reused: 0 to 86400, 600 without it. |
| `seconds` | How long a search waits for it: 1 to 10, 4 without it. The page waits for its slowest plugin. |

It needs `keywords`, `ids`, `pages` or `always`, unless it marks up the
node's results.

What `search` gets ([`Query`](../crates/plumb-plugin/src/lib.rs)):

| Field | Meaning |
| --- | --- |
| `text` | The whole search, as typed. |
| `terms` | The search without the keyword that ran the plugin. |
| `keyword` | That keyword, if one did. |
| `safe` | Safe search: `off`, `moderate` or `strict`. Pass it on to sources that filter. |
| `language` | The language asked for, such as `en`, if any. |
| `config` | The node owner's `config.json`, or `null`. |
| `about` | What the node took the search to be about, if anything: its `title`, `description`, `wikidata` item and `ids` on other services (`about.id("imdb")`). |
| `page` | For a page lookup, the page's address; `terms` is then empty. |

Each `Item` has a `title` and a `url` (`http`, `https` or `magnet`), and
optionally a `snippet`, `published` (Unix seconds), an `image` (a picture's
address on one of its hosts), a `badge` (a word or two beside the title, such
as "In library" or "45%") and `actions`, its buttons.

`Request::get(url).header(name, value).send()`, `Request::post(url, body)`,
`Request::put`, `Request::delete` and `Request::new(method, url)` send
requests; requests carry a `PlumbSearch/<version> plugin <folder name>` user
agent. A `Response` has its `status`, `headers` and `body`; `header(name)`
reads one header, and `cookies()` gives the cookies it sets as a `Cookie`
header for the next request, for a source that wants a login first.
`plumb_plugin::log` writes a line to the node's log, which
`plumb try-plugin` prints.

A plugin with buttons names its `act` too:

```rust
use plumb_plugin::{Action, ActInput, Error, Item, Query, Request};

fn search(query: &Query) -> Result<Vec<Item>, Error> {
    // ...
    Ok(vec![Item::new("Big Buck Bunny", "https://example.org/bbb")
        .badge("Not saved")
        .action(Action::new("Save", serde_json::json!({ "id": 42 })))])
}

fn act(input: &ActInput) -> Result<String, Error> {
    let id = input.data["id"].as_u64().ok_or(Error::Other("no id".into()))?;
    Request::post("https://api.example.org/saved", format!(r#"{{"id":{id}}}"#))
        .header("Content-Type", "application/json")
        .send()?
        .ok()?;
    Ok("Saved.".into())
}

plumb_plugin::plugin!(search, act);
```

Plugin code builds and its own tests run on any computer, but it fetches
nothing outside a node; test the code that reads a source's answers on saved
answers, as the example does.

## The interface

The `plumb-plugin` crate covers this; it is here for reference. A plugin
module exports its `memory`, `plumb_abi() -> i32` (returning `2`; a node also
runs plugins built for interface `1`), `plumb_search()`, for buttons
`plumb_act()`, and to mark up results `plumb_annotate()` (whose input is
`{"query", "results": [...], "config"}`), and may import only these functions
from the `plumb` module:

| Import | Does |
| --- | --- |
| `input_len() -> i32`, `input_read(ptr)` | The query, as JSON; for `plumb_act`, `{"data": ..., "config": ...}`. |
| `fetch(ptr, len) -> i32` | Sends the request at `ptr` (JSON: `method`, `url`, `headers`, `body`); returns the body's length, or -1 host not allowed, -2 failed, -3 too many requests, -4 too large, -5 time up, -6 bad request. |
| `fetch_status() -> i32`, `body_read(ptr)` | The last response's status and body. |
| `headers_len() -> i32`, `headers_read(ptr)` | The last response's headers, as JSON: `[["name", "value"], ...]`. |
| `output(ptr, len)` | The answer, as JSON: `{"results": [...]}`, for `plumb_act` `{"message": "..."}`, for `plumb_annotate` `{"notes": [...]}`, or `{"error": "..."}`. |
| `log(ptr, len)` | A line for the node's log. |

A node refuses a module that imports anything else.
