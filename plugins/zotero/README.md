# Zotero plugin

Your Zotero library in your node's results, on its search page and in its JSON and MCP answers. It asks the Zotero on the node's own computer, through the local API that Zotero 7 and later serve at `http://127.0.0.1:23119`, so nothing leaves the computer.

| Search | Shows |
| --- | --- |
| Every search: `attention is all you need`, `vaswani 2017` | Up to 5 items whose titles, authors or year hold all of its words, in a "From Zotero" block after the first result |
| `zotero gene drive`, `my papers transformers` | Up to 10 items, searching every field, your notes and the text of your PDFs |

Each result links to the item's own address, else its DOI, else its book at Open Library (by ISBN), else the item in your zotero.org web library when Zotero syncs there. An item with none of these is left out. It searches My Library, not group libraries. The node reuses its results for a minute (`cache_seconds` 60).

It finds things only while Zotero is open. When Zotero is closed, searches show nothing from it; a `zotero` search's empty answer is explained in the node's log (Zotero is not open, or its local API is off).

This is the other direction from the [Plumb Search plugin for Zotero](../../zotero/README.md), which searches Plumb from inside Zotero.

## Needs

- Zotero 7 or later, open on the node's computer.
- In Zotero, **Settings → Advanced**, tick **Allow other applications on this computer to communicate with Zotero**.

No `config.json`.

## Install

From the repository root:

```sh
rustup target add wasm32-unknown-unknown
cargo build --release -p plumb-plugin-zotero --target wasm32-unknown-unknown
```

Make a folder `zotero` in your node's `plugins/` folder ([Install a plugin](../../docs/plugins.md#install-a-plugin) says where; for the desktop app, see [Where data lives](../../docs/desktop.md#where-data-lives)) with:

- `plugin.json` from this folder;
- `plugin.wasm`: `target/wasm32-unknown-unknown/release/plumb_plugin_zotero.wasm`.

Restart the node (quit and reopen the desktop app), then search `zotero` followed by something in your library. To try it first:

```sh
plumb try-plugin --plugin path/to/zotero zotero gene drive
```

Its `plugin.json` lists `127.0.0.1:23119` as its one host, so it can reach Zotero's local API and nothing else. Like every plugin's results, your items show to anyone who can search the node: install it on the node of your own computer, such as the desktop app's, not on one other people use.
