//! Recent headlines from sites' own feeds (RSS or Atom), for the "Recent"
//! block of a results page.
//!
//! Plumb keeps no article text. A headline is what a site's feed says about
//! one of its new posts: the title, the link and when it was published.
//! Nodes keep only the last [`NEWS_WINDOW_SECS`] of them, at most
//! [`MAX_HEADLINES_PER_SITE`] per site, so the whole store stays a few MB.

use serde::{Deserialize, Serialize};

use crate::{collapse_whitespace, registrable_domain, truncate_chars};

/// How long a headline is kept: a week.
pub const NEWS_WINDOW_SECS: u64 = 7 * 24 * 60 * 60;

/// Most headlines kept for one site, newest first.
pub const MAX_HEADLINES_PER_SITE: usize = 10;

/// Longest headline title, in characters.
pub const MAX_HEADLINE_CHARS: usize = 200;

/// Longest headline link, in bytes.
pub const MAX_HEADLINE_URL_BYTES: usize = 500;

/// How a results page shows its "Recent" block of headlines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecentNews {
    /// Folded: a "Recent" line that opens to the headlines.
    #[default]
    Collapsed,
    /// The headlines shown open.
    Expanded,
    /// No block at all.
    Off,
}

impl RecentNews {
    /// Reads `collapsed`, `expanded` or `off` (any case); `None` otherwise.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "collapsed" => Some(RecentNews::Collapsed),
            "expanded" | "open" => Some(RecentNews::Expanded),
            "off" | "0" => Some(RecentNews::Off),
            _ => None,
        }
    }

    /// The name used in addresses and settings.
    pub fn as_str(self) -> &'static str {
        match self {
            RecentNews::Collapsed => "collapsed",
            RecentNews::Expanded => "expanded",
            RecentNews::Off => "off",
        }
    }
}

/// One post from a site's feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Headline {
    pub title: String,
    /// The post's address, on the site the feed belongs to.
    pub url: String,
    /// When the post was published (or last updated, when the feed gives
    /// no publication date), in Unix seconds.
    pub at: u64,
}

impl Headline {
    /// `title`, `url` and `at` made fit to keep for `domain`'s feed, read
    /// at `now`: the title whitespace-collapsed and cut to
    /// [`MAX_HEADLINE_CHARS`], a time in the future taken as `now`. `None`
    /// when the title is empty, the link is not an http(s) address on
    /// `domain` (feeds of aggregators point elsewhere, and a site may only
    /// speak for itself) or is too long, or the post is older than
    /// [`NEWS_WINDOW_SECS`].
    pub fn checked(domain: &str, title: &str, url: &str, at: u64, now: u64) -> Option<Headline> {
        let title = truncate_chars(&collapse_whitespace(title), MAX_HEADLINE_CHARS);
        if title.is_empty() {
            return None;
        }
        let url = url.trim();
        let lower = url.to_ascii_lowercase();
        if url.len() > MAX_HEADLINE_URL_BYTES
            || !(lower.starts_with("https://") || lower.starts_with("http://"))
            || registrable_domain(url).as_deref() != Some(domain)
        {
            return None;
        }
        let at = at.min(now);
        if at + NEWS_WINDOW_SECS < now {
            return None;
        }
        Some(Headline {
            title,
            url: url.to_string(),
            at,
        })
    }
}

/// Puts `new` headlines into a site's `kept` ones: each link once (the
/// newer telling wins), newest first, none older than [`NEWS_WINDOW_SECS`]
/// before `now`, at most [`MAX_HEADLINES_PER_SITE`]. Returns how many of
/// `new` were not kept before.
pub fn merge_headlines(kept: &mut Vec<Headline>, new: Vec<Headline>, now: u64) -> usize {
    let mut added = 0;
    for headline in new {
        match kept.iter_mut().find(|h| h.url == headline.url) {
            Some(had) => {
                if headline.at > had.at {
                    *had = headline;
                }
            }
            None => {
                kept.push(headline);
                added += 1;
            }
        }
    }
    kept.retain(|h| h.at + NEWS_WINDOW_SECS >= now);
    kept.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.url.cmp(&b.url)));
    kept.truncate(MAX_HEADLINES_PER_SITE);
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn keeps_only_recent_headlines_on_the_site() {
        let ok = Headline::checked(
            "example.com",
            "  Big\n news ",
            "https://www.example.com/a",
            NOW - 60,
            NOW,
        )
        .unwrap();
        assert_eq!(ok.title, "Big news");
        assert!(Headline::checked("example.com", "x", "https://other.com/a", NOW, NOW).is_none());
        assert!(Headline::checked("example.com", "x", "javascript:alert(1)", NOW, NOW).is_none());
        assert!(Headline::checked("example.com", " ", "https://example.com/a", NOW, NOW).is_none());
        let old = NOW - NEWS_WINDOW_SECS - 1;
        assert!(Headline::checked("example.com", "x", "https://example.com/a", old, NOW).is_none());
        let future = Headline::checked("example.com", "x", "https://example.com/a", NOW + 99, NOW);
        assert_eq!(future.unwrap().at, NOW);
    }

    #[test]
    fn merging_keeps_each_link_once_newest_first() {
        let h = |url: &str, at: u64| Headline {
            title: url.into(),
            url: url.into(),
            at,
        };
        let mut kept = vec![h("https://a.com/1", NOW - 100)];
        let added = merge_headlines(
            &mut kept,
            vec![
                h("https://a.com/1", NOW - 50),
                h("https://a.com/2", NOW - 10),
            ],
            NOW,
        );
        assert_eq!(added, 1);
        assert_eq!(
            kept,
            vec![
                h("https://a.com/2", NOW - 10),
                h("https://a.com/1", NOW - 50)
            ]
        );

        let many = (0..30)
            .map(|i| h(&format!("https://a.com/n{i}"), NOW - i))
            .collect();
        merge_headlines(&mut kept, many, NOW);
        assert_eq!(kept.len(), MAX_HEADLINES_PER_SITE);
        assert_eq!(kept[0].url, "https://a.com/n0");
    }
}
