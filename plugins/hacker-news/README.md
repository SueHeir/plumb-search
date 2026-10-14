# Hacker News plugin

Hacker News stories in your node's results, from the public search API that Algolia runs for Hacker News at `hn.algolia.com`. It needs no key, and it is the smallest complete plugin in this repository: [src/lib.rs](src/lib.rs) is a good place to start writing your own ([Write a plugin](../../docs/plugins.md#write-a-plugin)).

| Search | Shows |
| --- | --- |
| `hn rust async`, `rust async hacker news` | Up to 6 stories, each with its points, comments, author and age, linked to the story's own address (a text post links to its discussion) |

A search must start or end with `hn` or `hacker news`; the keyword alone searches for the site instead.

## Needs

Nothing: no key and no `config.json`.

## Install

From the repository root:

```sh
rustup target add wasm32-unknown-unknown
cargo build --release -p plumb-plugin-hacker-news --target wasm32-unknown-unknown
```

Make a folder `hacker-news` in your node's `plugins/` folder ([Install a plugin](../../docs/plugins.md#install-a-plugin) says where) with:

- `plugin.json` from this folder;
- `plugin.wasm`: `target/wasm32-unknown-unknown/release/plumb_plugin_hacker_news.wasm`.

Restart the node, then search `hn rust async`. To try it first:

```sh
plumb try-plugin --plugin path/to/hacker-news hn rust async
```
