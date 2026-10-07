# Reddit plugin

Reddit threads in your node's results for searches that start or end with
`reddit` (`reddit rust async`, `best hiking boots reddit`). `reddit r/rust
async` searches only r/rust.

It uses only Reddit's official Data API, signed in as your own Reddit app,
and never fetches reddit.com's pages. Like every plugin, it runs only on
your node and its results are never shared with other nodes.

## Before you start

Since November 2025 Reddit's
[Responsible Builder Policy](https://support.reddithelp.com/hc/en-us/articles/42728983564564-Responsible-Builder-Policy)
has asked every app, personal ones included, to request access and be
approved before it uses the API. Until Reddit approves your app, the plugin
finds nothing and your node's log says Reddit refused the app's keys.

Reddit Premium does not change API access or limits. The free limit is 100
requests a minute per app; this plugin makes two per search and your node
reuses its results for an hour.

## Set it up

1. Signed in to Reddit, open <https://www.reddit.com/prefs/apps>, choose
   "create another app", pick **script**, give it a name, and set the
   redirect uri to `http://localhost`.
2. If Reddit asks you to request API access, do so for personal,
   non-commercial use: "adds Reddit threads to searches on my own
   self-hosted search node; read only; a few requests an hour".
3. Copy this folder's `plugin.json` and the built `plugin.wasm` into
   `plugins/reddit/` in your node's data folder, and write a `config.json`
   next to them with the id under your app's name, its secret, and your
   username (see `config.example.json`):

   ```json
   { "client_id": "...", "client_secret": "...", "username": "..." }
   ```

4. Restart the node, then search `reddit something`.

Build `plugin.wasm` with

```sh
cargo build --release -p plumb-plugin-reddit --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/plumb_plugin_reddit.wasm plugins/reddit/plugin.wasm
```

and try it with `plumb try-plugin --plugin plugins/reddit reddit rust async`.
