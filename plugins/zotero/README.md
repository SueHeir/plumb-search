# Zotero plugin

Your Zotero library in your node's results, on its search page and in its
MCP and JSON answers:

- Every search lists up to five items whose titles, authors or year hold
  all of its words, in a "From Zotero" block after the first result:
  `attention is all you need`, `vaswani 2017`.
- A search that starts or ends with `zotero` or `my papers`
  (`my papers transformers`, `zotero gene drive`) searches every field,
  your notes and the text of your PDFs, and lists up to ten items.

Each result links to the item's own address, else its DOI, else its book at
Open Library (by ISBN), else the item in your zotero.org web library when
Zotero syncs there. An item with none of these is left out.

It asks the Zotero on the node's own computer, through the local API that
Zotero 7 and later serve at `http://127.0.0.1:23119`, so nothing leaves the
computer. It finds things only while Zotero is open; when Zotero is closed,
searches simply show nothing from it. It searches My Library, not group
libraries.

Like every plugin's results, your items show to anyone who can search the
node. Install it on the node of your own computer, such as the desktop
app's, not on one other people use.

## Set it up

1. In Zotero, open **Settings → Advanced** and tick **Allow other
   applications on this computer to communicate with Zotero**.
2. Build the plugin:

   ```sh
   cargo build --release -p plumb-plugin-zotero --target wasm32-unknown-unknown
   ```

3. Make a folder `plugins/zotero/` in your node's data folder (for the
   desktop app, see [Where data lives](../../docs/desktop.md#where-data-lives)),
   and copy this folder's `plugin.json` into it, with
   `target/wasm32-unknown-unknown/release/plumb_plugin_zotero.wasm` as
   `plugin.wasm`.
4. Restart the node (quit and reopen the desktop app), then search
   `zotero` followed by something in your library.

When a `zotero` search finds nothing, the node's log says why: Zotero is
not open, or its local API is off. `plumb try-plugin --plugin <the folder>
zotero <words>` shows it too.
