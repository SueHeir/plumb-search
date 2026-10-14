# YouTube plugin

Songs and videos from YouTube and YouTube Music in your node's results, through the official [YouTube Data API v3](https://developers.google.com/youtube/v3) with your own free API key. Nothing is scraped.

| Search | Shows |
| --- | --- |
| `ytm creep`, `creep youtube music` | Songs and music videos (YouTube's Music category), opening in YouTube Music |
| `yt rust async`, `rust async youtube` | Videos, channels and playlists, opening on YouTube |
| `creep`, `radiohead creep` (a song or album the node knows) | That song or album, searched for by its artist and title, with the node's own results |
| `radiohead`, `creep lyrics`, `dune trailer` (a search that fits it) | A "Show results from YouTube" link; following it shows the latest uploads of the artist's channel when the node knows it, or else songs or videos for the search |

Each video shows its length, channel and views, with its thumbnail.

**What fits.** A search fits the plugin when the node knows it is about an artist, album or song (it has a MusicBrainz, Spotify, Apple Music, SoundCloud, Discogs or Genius identifier) or a YouTube video, or when it has a word such as `song`, `lyrics`, `album`, `music video` or `trailer`. A search for a song or album runs the plugin on its own (its `run_ids`); anything else that fits shows the link, to save your quota. Under **Search & browser → Plugins** in the panel you can have every search that fits run it, or only its keywords (which also stops songs and albums running it).

## Needs

A YouTube Data API key, in `config.json`:

```json
{
  "api_key": "your key"
}
```

Add `"channel_uploads": false` to search for the words, rather than show a channel's uploads, on a search about someone with a YouTube channel.

To get a key:

1. Open [the YouTube Data API v3 page](https://console.cloud.google.com/apis/library/youtube.googleapis.com), signed in with any Google account, create a project if it asks, and press **Enable**.
2. Open [Credentials](https://console.cloud.google.com/apis/credentials), press **Create credentials**, then **API key**, and copy it.

**Quota.** The free key allows 10,000 quota units a day. A search costs 101 units (100 for the search, 1 for the lengths and views), so about 100 searches a day; a channel's uploads cost 2. A link the results page shows costs nothing until it is followed. The node reuses a search's results for 10 minutes, and a song's or album's for a day. When the quota is used up the node's log says so; it resets at midnight Pacific time.

## What it cannot do

YouTube Music has no public API, so the plugin sees YouTube Music's songs as the YouTube videos they are, and a video link on music.youtube.com opens it there. It cannot find YouTube Music's own album or artist pages.

## Install

From the repository root:

```sh
rustup target add wasm32-unknown-unknown
cargo build --release -p plumb-plugin-youtube-music --target wasm32-unknown-unknown
```

Make a folder `youtube-music` in your node's `plugins/` folder ([Install a plugin](../../docs/plugins.md#install-a-plugin) says where) with:

- `plugin.json` from this folder;
- `plugin.wasm`: `target/wasm32-unknown-unknown/release/plumb_plugin_youtube_music.wasm`;
- `config.json` with your key.

Restart the node, then search `ytm creep`. To try it first:

```sh
plumb try-plugin --plugin path/to/youtube-music ytm creep
```

Anyone who can search your node sees its results, as with every plugin, but your key stays on your node.
