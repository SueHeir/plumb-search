//! A Plumb Search plugin: songs and videos from YouTube and YouTube Music,
//! through the official YouTube Data API v3
//! (<https://developers.google.com/youtube/v3>) with the node owner's own
//! API key in `config.json`.
//!
//! - `ytm <search>` or `<search> youtube music`: songs and music videos
//!   (YouTube's Music category), linked to music.youtube.com.
//! - `yt <search>` or `<search> youtube`: videos, channels and playlists,
//!   linked to youtube.com.
//! - A search that fits it without a keyword (see `plugin.json`'s `ids`
//!   and `hints`), which by default the results page offers as a link: for
//!   someone with a YouTube channel (Wikidata's P2397), such as an artist,
//!   that channel's latest uploads, unless `config.json` sets
//!   `"channel_uploads": false`; otherwise songs when it is about music,
//!   or videos.
//!
//! The free key allows 10,000 quota units a day. A keyword search costs
//! 101 (a search is 100, the lengths and views 1), so about 100 a day; a
//! channel's uploads cost 2. The node reuses a search's results for 10
//! minutes.
//!
//! Build it with
//! `cargo build --release -p plumb-plugin-youtube-music --target wasm32-unknown-unknown`,
//! then see `README.md` here to install it.

use std::collections::HashMap;

use plumb_plugin::{encode, get, Error, Item, Query};
use serde::Deserialize;

const API: &str = "https://www.googleapis.com/youtube/v3";

/// Results asked for.
const RESULTS: usize = 8;

/// Keywords that ask for music rather than any video.
const MUSIC_KEYWORDS: &[&str] = &["ytm", "youtube music"];

/// Identifiers that make a search, without a keyword, about music: the
/// node knows it is about an artist, an album or a song.
const MUSIC_IDS: &[&str] = &[
    "musicbrainz-artist",
    "musicbrainz-album",
    "spotify",
    "spotify-album",
    "spotify-track",
    "apple-music",
    "apple-music-album",
    "soundcloud",
    "discogs",
    "genius-song",
    "genius-artist",
];

/// Words that make a search, without a keyword, about music. The same as
/// `hints` in `plugin.json`, less the ones for any video.
const MUSIC_WORDS: &[&str] = &[
    "song", "songs", "lyrics", "album", "remix", "acoustic", "playlist", "cover",
];

#[derive(Debug, Deserialize)]
struct SearchList {
    #[serde(default)]
    items: Vec<SearchResult>,
}

#[derive(Debug, Deserialize)]
struct SearchResult {
    id: ResourceId,
    snippet: Snippet,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourceId {
    kind: String,
    video_id: Option<String>,
    channel_id: Option<String>,
    playlist_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snippet {
    #[serde(default)]
    title: String,
    channel_title: Option<String>,
    published_at: Option<String>,
    thumbnails: Option<Thumbnails>,
    resource_id: Option<ResourceId>,
    video_owner_channel_title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Thumbnails {
    default: Option<Thumbnail>,
}

#[derive(Debug, Deserialize)]
struct Thumbnail {
    url: String,
}

#[derive(Debug, Deserialize)]
struct PlaylistItems {
    #[serde(default)]
    items: Vec<PlaylistItem>,
}

#[derive(Debug, Deserialize)]
struct PlaylistItem {
    snippet: Snippet,
}

#[derive(Debug, Deserialize)]
struct VideoList {
    #[serde(default)]
    items: Vec<Video>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Video {
    id: String,
    content_details: Option<ContentDetails>,
    statistics: Option<Statistics>,
}

#[derive(Debug, Deserialize)]
struct ContentDetails {
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Statistics {
    view_count: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: ApiErrorBody,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    #[serde(default)]
    message: String,
    #[serde(default)]
    errors: Vec<ApiErrorReason>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorReason {
    #[serde(default)]
    reason: String,
}

/// One thing found, before it becomes an [`Item`].
#[derive(Debug, Clone, PartialEq)]
struct Found {
    kind: Kind,
    id: String,
    title: String,
    channel: Option<String>,
    published: Option<u64>,
    thumbnail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video,
    Channel,
    Playlist,
}

/// A video's length and views, from `videos.list`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Details {
    length: Option<String>,
    views: Option<u64>,
}

fn search(query: &Query) -> Result<Vec<Item>, Error> {
    let terms = query.terms.trim();
    let music = match &query.keyword {
        Some(keyword) => MUSIC_KEYWORDS.contains(&keyword.to_lowercase().as_str()),
        None => about_music(query),
    };
    let uploads = query
        .about
        .as_ref()
        .filter(|_| query.keyword.is_none() && query.config["channel_uploads"] != false)
        .and_then(|about| about.id("youtube"))
        .and_then(uploads_playlist);
    let found = match uploads {
        Some(uploads) => {
            let key = api_key(query)?;
            let url = format!(
                "{API}/playlistItems?part=snippet&maxResults={RESULTS}&playlistId={}&key={}",
                encode(&uploads),
                encode(key)
            );
            playlist_results(call(&url)?)
        }
        None if !terms.is_empty() => {
            let key = api_key(query)?;
            let mut url = format!(
                "{API}/search?part=snippet&maxResults={RESULTS}&q={}&safeSearch={}&key={}",
                encode(terms),
                safe_search(&query.safe),
                encode(key)
            );
            if music {
                url.push_str("&type=video&videoCategoryId=10");
            }
            if let Some(language) = &query.language {
                url.push_str(&format!("&relevanceLanguage={}", encode(language)));
            }
            search_results(call(&url)?)
        }
        None => return Ok(Vec::new()),
    };
    let details = details(query, &found).unwrap_or_else(|error| {
        // Lengths and views are extras: show the results without them.
        plumb_plugin::log(&format!("no lengths or views: {error}"));
        HashMap::new()
    });
    Ok(items(found, &details, music))
}

/// Whether a search without a keyword is about music: about an artist,
/// album or song, or with a music word in it.
fn about_music(query: &Query) -> bool {
    let known = query
        .about
        .as_ref()
        .is_some_and(|about| MUSIC_IDS.iter().any(|id| about.id(id).is_some()));
    known
        || query
            .terms
            .split_whitespace()
            .any(|word| MUSIC_WORDS.contains(&word.to_lowercase().as_str()))
}

fn api_key(query: &Query) -> Result<&str, Error> {
    query.config["api_key"]
        .as_str()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| {
            Error::Other("no api_key in config.json: see plugins/youtube-music/README.md".into())
        })
}

/// The API's `safeSearch` for Plumb's setting.
fn safe_search(safe: &str) -> &'static str {
    match safe {
        "off" => "none",
        "strict" => "strict",
        _ => "moderate",
    }
}

/// A channel's uploads playlist: its id with `UU` for `UC`.
fn uploads_playlist(channel: &str) -> Option<String> {
    channel.strip_prefix("UC").map(|rest| format!("UU{rest}"))
}

/// Sends a request to the API, with Google's reason when it says no
/// ("quotaExceeded", "API key not valid").
fn call<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, Error> {
    let response = get(url)?;
    if !(200..300).contains(&response.status) {
        return Err(match serde_json::from_slice::<ApiError>(&response.body) {
            Ok(error) => Error::Other(api_error(response.status, &error.error)),
            Err(_) => Error::Status(response.status),
        });
    }
    response.json()
}

fn api_error(status: u16, error: &ApiErrorBody) -> String {
    let reason = error.errors.first().map_or("", |e| e.reason.as_str());
    match reason {
        "quotaExceeded" | "dailyLimitExceeded" => {
            "the API key's daily quota is used up; it resets at midnight Pacific time".into()
        }
        _ => format!("YouTube answered {status}: {} {reason}", error.message)
            .trim()
            .to_string(),
    }
}

fn search_results(list: SearchList) -> Vec<Found> {
    list.items
        .into_iter()
        .filter_map(|result| found(result.id, result.snippet))
        .collect()
}

fn playlist_results(list: PlaylistItems) -> Vec<Found> {
    list.items
        .into_iter()
        .filter_map(|item| {
            let mut snippet = item.snippet;
            let id = snippet.resource_id.take()?;
            // A playlist's own channel title is the playlist's owner; the
            // video's is its uploader.
            if snippet.video_owner_channel_title.is_some() {
                snippet.channel_title = snippet.video_owner_channel_title.take();
            }
            found(id, snippet)
        })
        .collect()
}

fn found(id: ResourceId, snippet: Snippet) -> Option<Found> {
    let (kind, id) = match id.kind.as_str() {
        "youtube#video" => (Kind::Video, id.video_id?),
        "youtube#channel" => (Kind::Channel, id.channel_id?),
        "youtube#playlist" => (Kind::Playlist, id.playlist_id?),
        _ => return None,
    };
    let title = unescape(&snippet.title);
    // Uploads that were removed or made private stay in playlists.
    if title.trim().is_empty() || title == "Private video" || title == "Deleted video" {
        return None;
    }
    Some(Found {
        kind,
        id,
        title,
        channel: snippet.channel_title.map(|c| unescape(&c)),
        published: snippet.published_at.as_deref().and_then(unix_seconds),
        thumbnail: snippet
            .thumbnails
            .and_then(|t| t.default)
            .map(|t| t.url)
            .filter(|url| url.starts_with("https://i.ytimg.com/")),
    })
}

/// The lengths and views of the videos in `found`, in one request.
fn details(query: &Query, found: &[Found]) -> Result<HashMap<String, Details>, Error> {
    let ids: Vec<&str> = found
        .iter()
        .filter(|f| f.kind == Kind::Video)
        .map(|f| f.id.as_str())
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let url = format!(
        "{API}/videos?part=contentDetails,statistics&id={}&key={}",
        encode(&ids.join(",")),
        encode(api_key(query)?)
    );
    Ok(video_details(call(&url)?))
}

fn video_details(list: VideoList) -> HashMap<String, Details> {
    list.items
        .into_iter()
        .map(|video| {
            let details = Details {
                length: video
                    .content_details
                    .and_then(|c| c.duration)
                    .and_then(|d| length(&d)),
                views: video
                    .statistics
                    .and_then(|s| s.view_count)
                    .and_then(|v| v.parse().ok()),
            };
            (video.id, details)
        })
        .collect()
}

fn items(found: Vec<Found>, details: &HashMap<String, Details>, music: bool) -> Vec<Item> {
    let site = if music {
        "https://music.youtube.com"
    } else {
        "https://www.youtube.com"
    };
    found
        .into_iter()
        .map(|found| {
            let detail = details.get(&found.id).cloned().unwrap_or_default();
            let (url, what) = match found.kind {
                Kind::Video => (format!("{site}/watch?v={}", found.id), None),
                Kind::Channel => (
                    format!("https://www.youtube.com/channel/{}", found.id),
                    Some("Channel"),
                ),
                Kind::Playlist => (
                    format!("{site}/playlist?list={}", found.id),
                    Some("Playlist"),
                ),
            };
            let mut about: Vec<String> = Vec::new();
            if let Some(what) = what {
                about.push(what.into());
            }
            if let Some(channel) = found.channel.filter(|_| found.kind != Kind::Channel) {
                about.push(channel);
            }
            if let Some(views) = detail.views {
                about.push(format!("{} views", short_count(views)));
            }
            let mut item = Item::new(found.title, url);
            if !about.is_empty() {
                item = item.snippet(about.join(" · "));
            }
            if let Some(length) = detail.length {
                item = item.badge(length);
            }
            if let Some(at) = found.published {
                item = item.published(at);
            }
            if let Some(thumbnail) = found.thumbnail {
                item = item.image(thumbnail);
            }
            item
        })
        .collect()
}

/// `PT1H2M3S` as `1:02:03`, `PT3M5S` as `3:05`; `None` for a live stream
/// (`P0D`) or anything else.
fn length(duration: &str) -> Option<String> {
    let mut seconds: u64 = 0;
    let mut number = String::new();
    let mut time = false;
    for c in duration.strip_prefix('P')?.chars() {
        match c {
            '0'..='9' => number.push(c),
            'T' if !time && number.is_empty() => time = true,
            _ => {
                let unit = match (time, c) {
                    (false, 'D') => 86_400,
                    (true, 'H') => 3600,
                    (true, 'M') => 60,
                    (true, 'S') => 1,
                    _ => return None,
                };
                seconds += number.parse::<u64>().ok()? * unit;
                number.clear();
            }
        }
    }
    if seconds == 0 || !number.is_empty() {
        return None;
    }
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    Some(if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    })
}

/// 950, 12K, 3.4M, 1.2B.
fn short_count(n: u64) -> String {
    let short = |n: u64, unit: u64, suffix: &str| {
        let tenths = n * 10 / unit;
        if tenths < 100 && !tenths.is_multiple_of(10) {
            format!("{}.{}{suffix}", tenths / 10, tenths % 10)
        } else {
            format!("{}{suffix}", n / unit)
        }
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => short(n, 1_000, "K"),
        1_000_000..=999_999_999 => short(n, 1_000_000, "M"),
        _ => short(n, 1_000_000_000, "B"),
    }
}

/// `2009-10-25T06:57:33Z` as Unix seconds.
fn unix_seconds(at: &str) -> Option<u64> {
    let number = |range: std::ops::Range<usize>| at.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

/// The API escapes titles for HTML; the node escapes them itself.
fn unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

plumb_plugin::plugin!(search);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_search_links_to_youtube_music_with_lengths_and_views() {
        let list: SearchList = serde_json::from_str(
            r#"{"items":[
                {"id":{"kind":"youtube#video","videoId":"u5CVsCnxyXg"},
                 "snippet":{"title":"Radiohead - Creep","channelTitle":"Radiohead",
                  "publishedAt":"2009-10-25T06:57:33Z",
                  "thumbnails":{"default":{"url":"https://i.ytimg.com/vi/u5CVsCnxyXg/default.jpg"}}}},
                {"id":{"kind":"youtube#video","videoId":"abc"},
                 "snippet":{"title":"Guns N&#39; Roses &amp; friends","channelTitle":"GNR - Topic"}}
            ]}"#,
        )
        .unwrap();
        let videos: VideoList = serde_json::from_str(
            r#"{"items":[{"id":"u5CVsCnxyXg","contentDetails":{"duration":"PT3M59S"},
                "statistics":{"viewCount":"1234567890"}}]}"#,
        )
        .unwrap();
        let items = items(search_results(list), &video_details(videos), true);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].url,
            "https://music.youtube.com/watch?v=u5CVsCnxyXg"
        );
        assert_eq!(items[0].badge.as_deref(), Some("3:59"));
        assert_eq!(items[0].snippet.as_deref(), Some("Radiohead · 1.2B views"));
        assert_eq!(items[0].published, Some(1_256_453_853));
        assert_eq!(
            items[0].image.as_deref(),
            Some("https://i.ytimg.com/vi/u5CVsCnxyXg/default.jpg")
        );
        assert_eq!(items[1].title, "Guns N' Roses & friends");
        assert_eq!(items[1].badge, None);
    }

    #[test]
    fn video_search_lists_channels_and_playlists_on_youtube() {
        let list: SearchList = serde_json::from_str(
            r#"{"items":[
                {"id":{"kind":"youtube#channel","channelId":"UCq19-LqvG35A-30oyAiPiqA"},
                 "snippet":{"title":"Radiohead","channelTitle":"Radiohead"}},
                {"id":{"kind":"youtube#playlist","playlistId":"PL1"},
                 "snippet":{"title":"OK Computer","channelTitle":"Radiohead"}}
            ]}"#,
        )
        .unwrap();
        let items = items(search_results(list), &HashMap::new(), false);
        assert_eq!(
            items[0].url,
            "https://www.youtube.com/channel/UCq19-LqvG35A-30oyAiPiqA"
        );
        assert_eq!(items[0].snippet.as_deref(), Some("Channel"));
        assert_eq!(items[1].url, "https://www.youtube.com/playlist?list=PL1");
        assert_eq!(items[1].snippet.as_deref(), Some("Playlist · Radiohead"));
    }

    #[test]
    fn channel_uploads_skip_private_videos_and_credit_the_uploader() {
        assert_eq!(
            uploads_playlist("UCq19-LqvG35A-30oyAiPiqA").as_deref(),
            Some("UUq19-LqvG35A-30oyAiPiqA")
        );
        assert_eq!(uploads_playlist("HCx"), None);
        let list: PlaylistItems = serde_json::from_str(
            r#"{"items":[
                {"snippet":{"title":"Private video","resourceId":{"kind":"youtube#video","videoId":"p"}}},
                {"snippet":{"title":"Daydreaming","channelTitle":"Radiohead",
                  "videoOwnerChannelTitle":"Radiohead Official",
                  "resourceId":{"kind":"youtube#video","videoId":"TTAU7lLDZYU"}}}
            ]}"#,
        )
        .unwrap();
        let found = playlist_results(list);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "TTAU7lLDZYU");
        assert_eq!(found[0].channel.as_deref(), Some("Radiohead Official"));
    }

    #[test]
    fn searches_without_a_keyword_are_music_when_about_music() {
        let mut query = Query {
            terms: "creep lyrics".into(),
            ..Query::default()
        };
        assert!(about_music(&query));
        query.terms = "dune trailer".into();
        assert!(!about_music(&query));
        query.terms = "radiohead".into();
        let mut about = plumb_plugin::About::default();
        about
            .ids
            .insert("musicbrainz-artist".into(), "a74b1b7f".into());
        query.about = Some(about);
        assert!(about_music(&query));
    }

    #[test]
    fn reads_lengths_counts_and_dates() {
        assert_eq!(length("PT3M5S").as_deref(), Some("3:05"));
        assert_eq!(length("PT1H2M3S").as_deref(), Some("1:02:03"));
        assert_eq!(length("PT45S").as_deref(), Some("0:45"));
        assert_eq!(length("P1DT1S").as_deref(), Some("24:00:01"));
        assert_eq!(length("P0D"), None);
        assert_eq!(length("3:05"), None);
        assert_eq!(short_count(950), "950");
        assert_eq!(short_count(12_345), "12K");
        assert_eq!(short_count(3_400_000), "3.4M");
        assert_eq!(short_count(2_000_000), "2M");
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(unix_seconds("soon"), None);
        assert_eq!(safe_search("off"), "none");
        assert_eq!(safe_search("moderate"), "moderate");
    }

    #[test]
    fn explains_quota_and_key_errors() {
        let quota: ApiError = serde_json::from_str(
            r#"{"error":{"code":403,"message":"The request cannot be completed because you have exceeded your quota.",
                "errors":[{"reason":"quotaExceeded"}]}}"#,
        )
        .unwrap();
        assert!(api_error(403, &quota.error).contains("daily quota"));
        let key: ApiError = serde_json::from_str(
            r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.",
                "errors":[{"reason":"badRequest"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            api_error(400, &key.error),
            "YouTube answered 400: API key not valid. Please pass a valid API key. badRequest"
        );
    }
}
