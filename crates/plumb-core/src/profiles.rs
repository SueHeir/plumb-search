//! Official profiles: the accounts a person, group or company has on
//! well-known services (a YouTube channel, a Twitch channel, an X account,
//! an app in the App Store), as Wikidata's external identifiers record
//! them. Wikipedia articles carry their item's profiles
//! ([`crate::article::Article::profiles`]), so an info box can link them
//! and "mrbeast youtube" can lead straight to the channel.
//!
//! Only the identifier is kept; the address is built here from it, the way
//! Wikidata's formatter URL for the property builds it, and only for
//! identifiers that look right, so a strange value can never make a link
//! to somewhere else.

use serde::{Deserialize, Serialize};

/// A service people have profiles on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Service {
    /// Short name kept in files: `youtube`.
    pub key: &'static str,
    /// What people see: "YouTube".
    pub name: &'static str,
    /// The Wikidata property holding the identifier: `P2397`.
    pub property: &'static str,
    /// The host the property's formatter URL must link to, checked when
    /// fetching, so a wrong property number is noticed.
    pub host: &'static str,
    /// Words that, ending a query, ask for this service: "youtube".
    pub words: &'static [&'static str],
    /// Which characters an identifier may have, and how long it may be.
    id: IdShape,
    /// The address, with `{}` for the identifier.
    pattern: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdShape {
    /// Letters, digits and these other characters, at most this long.
    Word(&'static str, usize),
    /// Digits only.
    Number,
    /// `user@server` (Mastodon).
    Fediverse,
}

const fn service(
    key: &'static str,
    name: &'static str,
    property: &'static str,
    host: &'static str,
    words: &'static [&'static str],
    id: IdShape,
    pattern: &'static str,
) -> Service {
    Service {
        key,
        name,
        property,
        host,
        words,
        id,
        pattern,
    }
}

use IdShape::{Fediverse, Number, Word};

/// The services kept, in the order an info box lists them. Two YouTube
/// properties: a handle (`@MrBeast`), shown when there is one, and the
/// channel id it falls back to.
pub static SERVICES: &[Service] = &[
    service(
        "youtube-handle",
        "YouTube",
        "P11245",
        "youtube.com",
        &["youtube", "yt", "youtube channel"],
        Word("_-.", 30),
        "https://www.youtube.com/@{}",
    ),
    service(
        "youtube",
        "YouTube",
        "P2397",
        "youtube.com",
        &["youtube", "yt", "youtube channel"],
        Word("_-", 24),
        "https://www.youtube.com/channel/{}",
    ),
    service(
        "twitch",
        "Twitch",
        "P5797",
        "twitch.tv",
        &["twitch", "twitch channel", "stream"],
        Word("_", 25),
        "https://www.twitch.tv/{}",
    ),
    service(
        "tiktok",
        "TikTok",
        "P7085",
        "tiktok.com",
        &["tiktok", "tik tok"],
        Word("_.", 24),
        "https://www.tiktok.com/@{}",
    ),
    service(
        "instagram",
        "Instagram",
        "P2003",
        "instagram.com",
        &["instagram", "insta", "ig"],
        Word("_.", 30),
        "https://www.instagram.com/{}/",
    ),
    service(
        "x",
        "X",
        "P2002",
        "x.com",
        &["x", "twitter", "tweets"],
        Word("_", 15),
        "https://x.com/{}",
    ),
    service(
        "bluesky",
        "Bluesky",
        "P12361",
        "bsky.app",
        &["bluesky", "bsky"],
        Word("_.-", 253),
        "https://bsky.app/profile/{}",
    ),
    service(
        "mastodon",
        "Mastodon",
        "P4033",
        "",
        &["mastodon"],
        Fediverse,
        "",
    ),
    service(
        "threads",
        "Threads",
        "P11892",
        "threads.net",
        &["threads"],
        Word("_.", 30),
        "https://www.threads.net/@{}",
    ),
    service(
        "facebook",
        "Facebook",
        "P2013",
        "facebook.com",
        &["facebook", "fb"],
        Word(".-", 50),
        "https://www.facebook.com/{}",
    ),
    service(
        "linkedin-company",
        "LinkedIn",
        "P4264",
        "linkedin.com",
        &["linkedin"],
        Word("-_", 100),
        "https://www.linkedin.com/company/{}/",
    ),
    service(
        "linkedin",
        "LinkedIn",
        "P6634",
        "linkedin.com",
        &["linkedin"],
        Word("-_", 100),
        "https://www.linkedin.com/in/{}/",
    ),
    service(
        "github",
        "GitHub",
        "P2037",
        "github.com",
        &["github"],
        Word("-", 39),
        "https://github.com/{}",
    ),
    service(
        "reddit",
        "Reddit",
        "P3984",
        "reddit.com",
        &["reddit", "subreddit"],
        Word("_", 21),
        "https://www.reddit.com/r/{}/",
    ),
    service(
        "spotify",
        "Spotify",
        "P1902",
        "spotify.com",
        &["spotify"],
        Word("", 22),
        "https://open.spotify.com/artist/{}",
    ),
    service(
        "apple-music",
        "Apple Music",
        "P2850",
        "apple.com",
        &["apple music", "itunes"],
        Number,
        "https://music.apple.com/artist/{}",
    ),
    service(
        "soundcloud",
        "SoundCloud",
        "P3040",
        "soundcloud.com",
        &["soundcloud"],
        Word("_-", 50),
        "https://soundcloud.com/{}",
    ),
    service(
        "patreon",
        "Patreon",
        "P4175",
        "patreon.com",
        &["patreon"],
        Word("_-", 64),
        "https://www.patreon.com/{}",
    ),
    service(
        "steam",
        "Steam",
        "P1733",
        "steampowered.com",
        &["steam"],
        Number,
        "https://store.steampowered.com/app/{}/",
    ),
    service(
        "app-store",
        "App Store",
        "P3861",
        "apple.com",
        &["app store", "ios app", "iphone app"],
        Number,
        "https://apps.apple.com/app/id{}",
    ),
    service(
        "google-play",
        "Google Play",
        "P3418",
        "google.com",
        &["google play", "play store", "android app"],
        Word("_.", 150),
        "https://play.google.com/store/apps/details?id={}",
    ),
];

/// The service kept as `key`.
pub fn service_by_key(key: &str) -> Option<&'static Service> {
    SERVICES.iter().find(|s| s.key == key)
}

impl Service {
    /// Whether `id` has the shape of this service's identifiers.
    pub fn accepts(&self, id: &str) -> bool {
        if id.is_empty() || id.len() > 260 {
            return false;
        }
        match self.id {
            Word(others, longest) => {
                id.chars().count() <= longest
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || others.contains(c))
            }
            Number => id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit()),
            Fediverse => fediverse(id).is_some(),
        }
    }

    /// The address of the profile `id`, when `id` looks right.
    pub fn url(&self, id: &str) -> Option<String> {
        let id = id.trim().trim_start_matches('@');
        if !self.accepts(id) {
            return None;
        }
        match self.id {
            Fediverse => {
                let (user, server) = fediverse(id)?;
                Some(format!("https://{server}/@{user}"))
            }
            _ => Some(self.pattern.replace("{}", id)),
        }
    }
}

/// `user@server` of a Mastodon address (`Gargron@mastodon.social`, with or
/// without a leading `@`).
fn fediverse(id: &str) -> Option<(&str, &str)> {
    let (user, server) = id.trim_start_matches('@').split_once('@')?;
    let user_ok = !user.is_empty()
        && user.len() <= 64
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    let server_ok = server.len() <= 253
        && server.contains('.')
        && !server.starts_with(['.', '-'])
        && server
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    (user_ok && server_ok).then_some((user, server.trim_end_matches('.')))
}

/// One profile: the service's key and the identifier on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub service: String,
    pub id: String,
}

impl Profile {
    /// The service and the profile's address, when both are known and the
    /// identifier looks right.
    pub fn link(&self) -> Option<(&'static Service, String)> {
        let service = service_by_key(&self.service)?;
        Some((service, service.url(&self.id)?))
    }
}

/// The profiles to show, one per service people see (a YouTube handle
/// rather than the channel id), in [`SERVICES`] order.
pub fn shown_profiles(profiles: &[Profile]) -> Vec<(&'static Service, String)> {
    let mut shown: Vec<(&'static Service, String)> = Vec::new();
    for service in SERVICES {
        if shown.iter().any(|(s, _)| s.name == service.name) {
            continue;
        }
        if let Some(url) = profiles
            .iter()
            .filter(|p| p.service == service.key)
            .find_map(|p| service.url(&p.id))
        {
            shown.push((service, url));
        }
    }
    shown
}

/// The services named by the end of `query` ("mrbeast youtube", "valve
/// steam") and the words before them, which must be there.
pub fn services_asked(query: &str) -> Option<(Vec<&'static Service>, String)> {
    let words = crate::normalize_text(query);
    let words: Vec<&str> = words.split_whitespace().collect();
    let mut best: Option<(usize, &str)> = None;
    for service in SERVICES {
        for phrase in service.words {
            let phrase_words: Vec<&str> = phrase.split(' ').collect();
            let n = phrase_words.len();
            if n < words.len() && words.ends_with(&phrase_words) && best.is_none_or(|(m, _)| n > m)
            {
                best = Some((n, service.name));
            }
        }
    }
    let (n, name) = best?;
    let services = SERVICES.iter().filter(|s| s.name == name).collect();
    Some((services, words[..words.len() - n].join(" ")))
}

/// Writes `profiles` as kept in an articles file: `key=id|key=id`.
pub fn write_profiles(profiles: &[Profile]) -> String {
    profiles
        .iter()
        .filter(|p| service_by_key(&p.service).is_some_and(|s| s.accepts(&p.id)))
        .map(|p| format!("{}={}", p.service, p.id))
        .collect::<Vec<_>>()
        .join("|")
}

/// Reads [`write_profiles`]' text, leaving out services this build does
/// not know and identifiers that do not look right.
pub fn parse_profiles(text: &str) -> Vec<Profile> {
    text.split('|')
        .filter_map(|pair| {
            let (key, id) = pair.split_once('=')?;
            let service = service_by_key(key.trim())?;
            let id = id.trim();
            service.accepts(id).then(|| Profile {
                service: service.key.to_string(),
                id: id.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(service: &str, id: &str) -> Profile {
        Profile {
            service: service.into(),
            id: id.into(),
        }
    }

    #[test]
    fn builds_addresses() {
        let link = |s: &str, id: &str| profile(s, id).link().map(|(_, url)| url);
        assert_eq!(
            link("youtube", "UCX6OQ3DkcsbYNE6H8uQQuVA").unwrap(),
            "https://www.youtube.com/channel/UCX6OQ3DkcsbYNE6H8uQQuVA"
        );
        assert_eq!(
            link("youtube-handle", "MrBeast").unwrap(),
            "https://www.youtube.com/@MrBeast"
        );
        assert_eq!(
            link("youtube-handle", "@MrBeast").unwrap(),
            "https://www.youtube.com/@MrBeast"
        );
        assert_eq!(link("twitch", "xqc").unwrap(), "https://www.twitch.tv/xqc");
        assert_eq!(
            link("mastodon", "Gargron@mastodon.social").unwrap(),
            "https://mastodon.social/@Gargron"
        );
        assert_eq!(
            link("steam", "620").unwrap(),
            "https://store.steampowered.com/app/620/"
        );
        assert_eq!(
            link("google-play", "com.spotify.music").unwrap(),
            "https://play.google.com/store/apps/details?id=com.spotify.music"
        );
    }

    #[test]
    fn refuses_strange_identifiers() {
        let link = |s: &str, id: &str| profile(s, id).link();
        assert_eq!(link("x", "a/../../evil"), None);
        assert_eq!(link("x", "way_too_long_for_x_handles"), None);
        assert_eq!(link("steam", "620abc"), None);
        assert_eq!(link("mastodon", "user@evil.com/path"), None);
        assert_eq!(link("instagram", "a b"), None);
        assert_eq!(link("instagram", "x\"><script>"), None);
        assert_eq!(link("nope", "x"), None);
    }

    #[test]
    fn shows_one_profile_per_service() {
        let shown = shown_profiles(&[
            profile("youtube", "UCX6OQ3DkcsbYNE6H8uQQuVA"),
            profile("x", "MrBeast"),
            profile("youtube-handle", "MrBeast"),
        ]);
        let names: Vec<(&str, &str)> = shown.iter().map(|(s, u)| (s.name, u.as_str())).collect();
        assert_eq!(
            names,
            [
                ("YouTube", "https://www.youtube.com/@MrBeast"),
                ("X", "https://x.com/MrBeast")
            ]
        );
    }

    #[test]
    fn reads_what_it_writes() {
        let profiles = vec![
            profile("youtube-handle", "MrBeast"),
            profile("x", "MrBeast"),
        ];
        let text = write_profiles(&profiles);
        assert_eq!(text, "youtube-handle=MrBeast|x=MrBeast");
        assert_eq!(parse_profiles(&text), profiles);
        assert_eq!(
            parse_profiles("future=1|x=ok|x=not ok"),
            vec![profile("x", "ok")]
        );
    }

    #[test]
    fn finds_the_service_a_query_asks_for() {
        let (services, name) = services_asked("MrBeast YouTube").unwrap();
        assert_eq!(name, "mrbeast");
        assert_eq!(services.len(), 2);
        assert_eq!(services_asked("valve steam").unwrap().1, "valve");
        assert_eq!(
            services_asked("spotify android app").unwrap().0[0].name,
            "Google Play"
        );
        assert_eq!(services_asked("youtube"), None);
        assert_eq!(services_asked("python docs"), None);
    }

    #[test]
    fn every_service_is_whole() {
        for service in SERVICES {
            assert!(service.property.starts_with('P'), "{}", service.key);
            assert!(!service.words.is_empty(), "{}", service.key);
            assert!(service
                .key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '-'));
            if service.id != Fediverse {
                assert!(service.pattern.starts_with("https://") && service.pattern.contains("{}"));
                assert!(service.pattern.contains(service.host), "{}", service.key);
            }
        }
    }
}
