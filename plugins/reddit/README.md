# Reddit plugin

Reddit threads in your node's results, through Reddit's official Data API, signed in as your own Reddit app. It never fetches reddit.com's pages.

| Search | Shows |
| --- | --- |
| `reddit rust async`, `best hiking boots reddit` | Up to 8 threads, each with its subreddit, points and comments, and the start of a text post |
| `reddit r/rust async` | The same, from r/rust only |

Each search makes two requests (a token, then the search), and the node reuses its results for an hour (`cache_seconds` 3600), well inside Reddit's free limit of 100 requests a minute per app. Threads marked NSFW show only when safe search is off.

## Needs

Your own Reddit app's id and secret, and your username, in `config.json` (see [config.example.json](config.example.json)):

```json
{
  "client_id": "the id under your app's name",
  "client_secret": "the app's secret",
  "username": "your Reddit username, without u/"
}
```

To make the app:

1. Signed in to Reddit, open <https://www.reddit.com/prefs/apps>, choose **create another app**, pick **script**, give it a name, and set the redirect URI to `http://localhost`.
2. If Reddit asks you to request API access, do so for personal, non-commercial use, such as "adds Reddit threads to searches on my own self-hosted search node; read only; a few requests an hour". Reddit's [Responsible Builder Policy](https://support.reddithelp.com/hc/en-us/articles/42728983564564-Responsible-Builder-Policy) covers this.

Until Reddit approves the app, the plugin finds nothing and the node's log says Reddit refused the app's keys or the request.

## Install

From the repository root:

```sh
rustup target add wasm32-unknown-unknown
cargo build --release -p plumb-plugin-reddit --target wasm32-unknown-unknown
```

Make a folder `reddit` in your node's `plugins/` folder ([Install a plugin](../../docs/plugins.md#install-a-plugin) says where) with:

- `plugin.json` from this folder;
- `plugin.wasm`: `target/wasm32-unknown-unknown/release/plumb_plugin_reddit.wasm`;
- `config.json` with your app's keys and your username.

Restart the node, then search `reddit something`. To try it first:

```sh
plumb try-plugin --plugin path/to/reddit reddit rust async
```
