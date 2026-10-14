# Steam plugin

Your Steam library in your node's results, through the Steam Web API with your own key, and which results are about games you own.

| Search | Shows |
| --- | --- |
| `steam portal`, `my games portal` | Up to 8 games in your library whose names hold every word, most played first, each with its picture and hours played, linked to its store page |
| A search with a result about a game, such as its Wikipedia article | A badge on that result: "Steam: Owned, 25 h", "Steam: Owned" (under an hour played) or "Steam: On your wishlist" |

The badges go by the game's Steam identifier, which the node knows from the game's Wikidata item. The node reuses the plugin's answers for an hour (`cache_seconds` 3600).

## Needs

- A [Steam Web API key](https://steamcommunity.com/dev/apikey).
- Your 17-digit SteamID64.

Both go in `config.json` (see [config.example.json](config.example.json)):

```json
{
  "key": "your Steam Web API key",
  "steamid": "76561197960287930"
}
```

A private library reads as empty. If your library comes back empty, set your Steam profile's game details to public. A private wishlist only leaves out the wishlist badges.

## Install

From the repository root:

```sh
rustup target add wasm32-unknown-unknown
cargo build --release -p plumb-plugin-steam --target wasm32-unknown-unknown
```

Make a folder `steam` in your node's `plugins/` folder ([Install a plugin](../../docs/plugins.md#install-a-plugin) says where) with:

- `plugin.json` from this folder;
- `plugin.wasm`: `target/wasm32-unknown-unknown/release/plumb_plugin_steam.wasm`;
- `config.json` with your key and SteamID64.

Restart the node, then search `steam portal`. To try it first:

```sh
plumb try-plugin --plugin path/to/steam steam portal
```

Like every plugin's results, your library and badges show to anyone who can search the node. Install it on a node only you search, such as the desktop app's.
