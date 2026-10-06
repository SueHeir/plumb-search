# YouTube plugin

Songs and videos from YouTube and YouTube Music in your node's results,
through the official [YouTube Data API v3](https://developers.google.com/youtube/v3)
with your own free API key. Nothing is scraped.

| Search | Shows |
| --- | --- |
| `ytm creep`, `creep youtube music` | Songs and music videos (YouTube's Music category), opening in YouTube Music |
| `yt rust async`, `rust async youtube` | Videos, channels and playlists, opening on YouTube |
| `radiohead` (anything the node knows has a YouTube channel) | That channel's latest uploads |

Each video shows its length, channel and views, with its thumbnail.

## What it can and cannot do

YouTube Music has no public API, so this plugin uses the YouTube Data API,
which sees the same songs and videos: YouTube Music's songs are YouTube videos,
and a video link on music.youtube.com opens it there. It cannot find YouTube
Music's own album or artist pages. A YouTube Premium or YouTube Music
subscription adds nothing to the API, but the links open in your browser, where
you are signed in, so they play without ads.

The free key allows 10,000 quota units a day. A keyword search costs 101 units
(about 100 searches a day); a channel's uploads cost 2. Your node reuses a
search's results for 10 minutes. When the quota is used up the node's log says
so, and it comes back at midnight Pacific time. It never costs money: the key
needs no billing account.

## Get a key

1. Open [the YouTube Data API v3 page](https://console.cloud.google.com/apis/library/youtube.googleapis.com),
   signed in with any Google account, create a project if it asks, and press
   **Enable**.
2. Open [Credentials](https://console.cloud.google.com/apis/credentials), press
   **Create credentials**, then **API key**, and copy it.

## Install

Build it (or take `plugin.wasm` from someone who did):

```sh
rustup target add wasm32-unknown-unknown
cargo build --release -p plumb-plugin-youtube-music --target wasm32-unknown-unknown
```

Make a `youtube-music` folder in your node's `plugins/` folder (see
[Install a plugin](../../docs/plugins.md#install-a-plugin)) with:

- `plugin.json` from here;
- `plugin.wasm`: `target/wasm32-unknown-unknown/release/plumb_plugin_youtube_music.wasm`;
- `config.json` with your key:

```json
{
  "api_key": "your key"
}
```

Add `"channel_uploads": false` to leave out channels' uploads on searches
without a keyword. Restart the node, then try `ytm creep`.

To try it first:

```sh
plumb try-plugin --plugin path/to/youtube-music ytm creep
```

Anyone who can search your node sees its results, as with every plugin, but
your key stays on your node.
