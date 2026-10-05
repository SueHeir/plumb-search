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

- read the query, the safe search and language settings, and its own
  `config.json`;
- make GET and POST requests to the hosts its `plugin.json` lists, and only
  those (redirects to other hosts are not followed);
- hand back results.

It cannot read or write files, open any other connection, read the clock or
the environment, or learn who searched. These limits hold for every search:

| Limit | Value |
| --- | --- |
| Time a search waits for plugins' results, from its start | 4 seconds |
| Requests per plugin per search | 4 |
| Response size | 2 MB |
| Memory | 64 MB |
| Work | about 2 billion WebAssembly instructions |
| Results kept per plugin | 10 |
| Searches one plugin runs at once | 4 (more searches go without it) |

A plugin still running after 4 seconds is left out of that search, but it is
not stopped at once: it can make no more requests (or finish one) after the 4
seconds, yet it keeps computing in the background until it returns or uses up
its work limit.

Results are checked before they are shown: only `http` and `https` links with
a title are kept, text is put on one line and shortened, everything is escaped
like any other text from the web, and safe search leaves out results that say
they are adult. A plugin's results for the same search are reused for 10
minutes, so a busy node does not ask its source the same thing over and over.
A plugin that fails, runs too long or runs out of work is left out of that
search, and the node's log says why.

## When a plugin runs

A plugin runs when a search starts or ends with one of its keywords: with the
example's keywords, `hn rust async` and `rust async hacker news` both run it,
and it searches for `rust async`. A keyword alone (`hn`) does not, since that
is a search for the site. A plugin with `"always": true` runs for every
search; use it only for a source that allows that many requests.

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
it found nothing.

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
plumb-plugin = { git = "https://github.com/SueHeir/plumb-search" }
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
Hacker News's public search API for stories. Build a plugin with

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
| `hosts` | The hosts it may fetch from: `api.example.org`, or `*.example.org` for every subdomain (not `example.org` itself). |
| `keywords` | Words or phrases that run it at the start or end of a search. |
| `always` | `true` to run it for every search. |

What `search` gets ([`Query`](../crates/plumb-plugin/src/lib.rs)):

| Field | Meaning |
| --- | --- |
| `text` | The whole search, as typed. |
| `terms` | The search without the keyword that ran the plugin. |
| `keyword` | That keyword, if one did. |
| `safe` | Safe search: `off`, `moderate` or `strict`. Pass it on to sources that filter. |
| `language` | The language asked for, such as `en`, if any. |
| `config` | The node owner's `config.json`, or `null`. |

Each `Item` has a `title`, a `url`, and optionally a `snippet` and `published`
(Unix seconds). `Request::get(url).header(name, value).send()` and
`Request::post(url, body)` send requests with headers; requests carry a
`PlumbSearch/<version> plugin <folder name>` user agent. `plumb_plugin::log`
writes a line to the node's log, which `plumb try-plugin` prints.

Plugin code builds and its own tests run on any computer, but it fetches
nothing outside a node; test the code that reads a source's answers on saved
answers, as the example does.

## The interface

The `plumb-plugin` crate covers this; it is here for reference. A plugin
module exports its `memory`, `plumb_abi() -> i32` (returning `1`) and
`plumb_search()`, and may import only these functions from the `plumb`
module:

| Import | Does |
| --- | --- |
| `input_len() -> i32`, `input_read(ptr)` | The query, as JSON. |
| `fetch(ptr, len) -> i32` | Sends the request at `ptr` (JSON: `method`, `url`, `headers`, `body`); returns the body's length, or -1 host not allowed, -2 failed, -3 too many requests, -4 too large, -5 time up, -6 bad request. |
| `fetch_status() -> i32`, `body_read(ptr)` | The last response's status and body. |
| `output(ptr, len)` | The answer, as JSON: `{"results": [...]}` or `{"error": "..."}`. |
| `log(ptr, len)` | A line for the node's log. |

A node refuses a module that imports anything else.
