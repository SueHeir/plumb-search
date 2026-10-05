//! Recent headlines a node keeps for the "Recent" block of its results
//! page (see [`plumb_core::news`]).
//!
//! A node watches the feeds of its best-ranked sites ([`NewsStore::watch`],
//! at each index build) and checks those that are due
//! ([`NewsStore::due`]); its own crawls note the feeds homepages name, and
//! trusted nodes' feed checks arrive with their crawl batches. What it
//! keeps, a week of headlines at most ten per site, is a few MB:
//!
//! ```text
//! DIR/news/
//!   feeds.json       the sites watched, best first, and each feed's state
//!   headlines.json   recent headlines by site
//! ```
//!
//! A search shows them in one of two ways ([`NewsStore::recent`]): the
//! latest posts of the site a query names, or the newest headlines that
//! have every word of the query in their title.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

use plumb_core::news::{merge_headlines, NEWS_WINDOW_SECS};
use plumb_core::Headline;
use plumb_crawl::{CrawlOutcome, CrawlResult, CrawlTarget, FeedCheck, FeedOutcome, FeedTarget};
use serde::{Deserialize, Serialize};
use tracing::warn;

const FEEDS_FILE: &str = "feeds.json";
const HEADLINES_FILE: &str = "headlines.json";

/// Shortest wait between two checks of one feed: an hour.
pub const MIN_CHECK_GAP_SECS: u64 = 60 * 60;
/// Longest wait between two checks of a feed that has not changed.
const MAX_CHECK_GAP_SECS: u64 = 12 * 60 * 60;
/// Wait before looking again for the feed of a site that had none.
const NO_FEED_RECHECK_SECS: u64 = 7 * 24 * 60 * 60;

/// Headlines a "Recent" block shows.
const MAX_SHOWN: usize = 5;
/// Headlines one site may have in a block about a topic.
const MAX_SHOWN_PER_SITE: usize = 2;
/// A topic block needs this many sites with a matching headline, unless
/// the query asks for news ("election news").
const MIN_TOPIC_SITES: usize = 2;
/// ... headlines from the last this many seconds.
const TOPIC_WINDOW_SECS: u64 = 3 * 24 * 60 * 60;
/// Queries longer than this many words get no topic block.
const MAX_TOPIC_WORDS: usize = 6;

/// Words that ask for news rather than name a topic.
const NEWS_WORDS: &[&str] = &[
    "news",
    "latest",
    "recent",
    "today",
    "headlines",
    "breaking",
    "update",
    "updates",
];
/// Words too common to match a headline by.
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "how", "in", "is", "it", "of",
    "on", "or", "the", "to", "was", "what", "when", "who", "why", "with",
];

/// One site whose feed is watched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watched {
    pub domain: String,
    /// Its homepage, where its feed is looked for.
    pub homepage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    /// When the feed is next checked (or looked for), Unix seconds.
    #[serde(default)]
    pub next_at: u64,
    /// The wait after the last check, which grows while the feed is quiet.
    #[serde(default)]
    pub gap: u64,
}

/// What a "Recent" block shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    /// The site whose latest posts these are, when the query names it;
    /// `None` for headlines about the query's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    pub headlines: Vec<RecentHeadline>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentHeadline {
    /// The site that published it.
    pub domain: String,
    pub title: String,
    pub url: String,
    /// Unix seconds.
    pub at: u64,
}

#[derive(Debug, Default)]
struct State {
    watched: Vec<Watched>,
    headlines: HashMap<String, Vec<Headline>>,
    /// Folded title word -> the sites with a headline holding it; made
    /// again on the first search after a change.
    words: Option<HashMap<String, Vec<String>>>,
    /// Changed since the last save.
    dirty: bool,
}

/// A node's headlines and watched feeds.
#[derive(Debug)]
pub struct NewsStore {
    dir: PathBuf,
    state: RwLock<State>,
}

impl NewsStore {
    /// The store in `dir`, with what was saved there.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let watched = read_json(&dir.join(FEEDS_FILE)).unwrap_or_default();
        let headlines = read_json(&dir.join(HEADLINES_FILE)).unwrap_or_default();
        NewsStore {
            dir,
            state: RwLock::new(State {
                watched,
                headlines,
                words: None,
                dirty: false,
            }),
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Watches the feeds of `sites` (domain and homepage), best first,
    /// keeping what is known of those already watched; the others are no
    /// longer checked, though their headlines stay until they age out.
    pub fn watch(&self, sites: Vec<(String, String)>) {
        let mut state = self.write();
        let mut known: HashMap<String, Watched> = std::mem::take(&mut state.watched)
            .into_iter()
            .map(|w| (w.domain.clone(), w))
            .collect();
        state.watched = sites
            .into_iter()
            .map(|(domain, homepage)| match known.remove(&domain) {
                Some(mut had) => {
                    had.homepage = homepage;
                    had
                }
                None => Watched {
                    domain,
                    homepage,
                    ..Watched::default()
                },
            })
            .collect();
        state.dirty = true;
    }

    /// How many sites are watched, and how many of those have a feed.
    pub fn watched(&self) -> (usize, usize) {
        let state = self.read();
        let feeds = state.watched.iter().filter(|w| w.feed.is_some()).count();
        (state.watched.len(), feeds)
    }

    /// Notes the feeds that crawled homepages of watched sites name.
    pub fn note_feeds(&self, results: &[CrawlResult]) {
        let found: HashMap<&str, &str> = results
            .iter()
            .filter_map(|r| match &r.outcome {
                CrawlOutcome::Fetched(page) => {
                    Some((r.domain.as_str(), page.meta.feed.as_deref()?))
                }
                _ => None,
            })
            .collect();
        if found.is_empty() {
            return;
        }
        let mut state = self.write();
        let mut changed = false;
        for watched in &mut state.watched {
            if let Some(&feed) = found.get(watched.domain.as_str()) {
                if watched.feed.as_deref() != Some(feed) {
                    watched.feed = Some(feed.to_string());
                    watched.etag = None;
                    watched.last_modified = None;
                    watched.next_at = 0;
                    changed = true;
                }
            }
        }
        state.dirty |= changed;
    }

    /// Up to `limit` watched sites whose feed is due for a check at `now`,
    /// best-ranked first.
    pub fn due(&self, now: u64, limit: usize) -> Vec<FeedTarget> {
        self.read()
            .watched
            .iter()
            .filter(|w| w.next_at <= now)
            .take(limit)
            .map(|w| FeedTarget {
                homepage: CrawlTarget {
                    domain: w.domain.clone(),
                    url: w.homepage.clone(),
                    known_url: None,
                },
                feed: w.feed.clone(),
                etag: w.etag.clone(),
                last_modified: w.last_modified.clone(),
            })
            .collect()
    }

    /// When the next watched feed is due; `None` when none is watched.
    pub fn next_due(&self) -> Option<u64> {
        self.read().watched.iter().map(|w| w.next_at).min()
    }

    /// Takes in what checks of watched feeds found at `now`, and returns
    /// the sites with new headlines, as records carrying all their recent
    /// headlines ([`plumb_core::SiteRecord::news`]) to share.
    pub fn apply(&self, checks: Vec<FeedCheck>, now: u64) -> Vec<plumb_core::SiteRecord> {
        let mut state = self.write();
        let mut changed = Vec::new();
        let mut new_items: HashMap<String, Vec<Headline>> = HashMap::new();
        for check in checks {
            let Some(watched) = state.watched.iter_mut().find(|w| w.domain == check.domain) else {
                continue;
            };
            let quieter = (watched.gap * 2).clamp(MIN_CHECK_GAP_SECS, MAX_CHECK_GAP_SECS);
            match check.outcome {
                FeedOutcome::Read {
                    feed,
                    headlines,
                    etag,
                    last_modified,
                } => {
                    watched.feed = Some(feed);
                    watched.etag = etag;
                    watched.last_modified = last_modified;
                    new_items.insert(check.domain, headlines);
                    // Set below, once it is known whether any was new.
                    watched.gap = quieter;
                }
                FeedOutcome::NotModified => watched.gap = quieter,
                FeedOutcome::NoFeed => {
                    watched.feed = None;
                    watched.etag = None;
                    watched.last_modified = None;
                    watched.gap = NO_FEED_RECHECK_SECS;
                }
                FeedOutcome::Failed(_) => watched.gap = quieter,
            }
            watched.next_at = now + watched.gap;
        }
        for (domain, headlines) in new_items {
            let kept = state.headlines.entry(domain.clone()).or_default();
            if merge_headlines(kept, headlines, now) > 0 {
                changed.push(domain.clone());
                // A feed with news is checked again soon.
                if let Some(w) = state.watched.iter_mut().find(|w| w.domain == domain) {
                    w.gap = MIN_CHECK_GAP_SECS;
                    w.next_at = now + w.gap;
                }
            }
        }
        state.headlines.retain(|_, kept| !kept.is_empty());
        state.words = None;
        state.dirty = true;
        changed
            .into_iter()
            .filter_map(|domain| {
                let news = state.headlines.get(&domain)?.clone();
                let mut record = plumb_core::SiteRecord::new(domain);
                record.news = news;
                Some(record)
            })
            .collect()
    }

    /// Takes in headlines trusted nodes shared ([`plumb_core::SiteRecord::news`]).
    pub fn put_shared(&self, records: Vec<plumb_core::SiteRecord>, now: u64) {
        let mut state = self.write();
        for record in records {
            if record.news.is_empty() {
                continue;
            }
            let kept = state.headlines.entry(record.domain).or_default();
            merge_headlines(kept, record.news, now);
        }
        state.headlines.retain(|_, kept| !kept.is_empty());
        state.words = None;
        state.dirty = true;
    }

    /// Drops headlines older than a week before `now`.
    pub fn prune(&self, now: u64) {
        let mut state = self.write();
        let before: usize = state.headlines.values().map(Vec::len).sum();
        for kept in state.headlines.values_mut() {
            kept.retain(|h| h.at + NEWS_WINDOW_SECS >= now);
        }
        state.headlines.retain(|_, kept| !kept.is_empty());
        let after: usize = state.headlines.values().map(Vec::len).sum();
        if after != before {
            state.words = None;
            state.dirty = true;
        }
    }

    /// Headlines kept, all sites together.
    pub fn headline_count(&self) -> usize {
        self.read().headlines.values().map(Vec::len).sum()
    }

    /// Writes the store to its folder, if it changed.
    pub fn save(&self) -> io::Result<()> {
        let mut state = self.write();
        if !state.dirty {
            return Ok(());
        }
        fs::create_dir_all(&self.dir)?;
        write_json(&self.dir.join(FEEDS_FILE), &state.watched)?;
        write_json(&self.dir.join(HEADLINES_FILE), &state.headlines)?;
        state.dirty = false;
        Ok(())
    }

    /// The "Recent" block for `query` at `now`, whose best result is
    /// `top` (its domain, and whether the query names it): the latest
    /// posts of that site when the query names it (or asks for news and
    /// no headline is about its words), else the newest headlines with
    /// every word of the query in their title, when they come from at
    /// least two sites in the last three days or the query asks for news.
    pub fn recent(&self, query: &str, top: Option<(&str, bool)>, now: u64) -> Option<Recent> {
        let words = fold_words(query);
        let wants_news = words.iter().any(|w| is_one_of(w, NEWS_WORDS));
        let topic: Vec<String> = words
            .into_iter()
            .filter(|w| !is_one_of(w, NEWS_WORDS) && !is_one_of(w, STOP_WORDS))
            .collect();
        let site_block = |state: &State, domain: &str| {
            let kept = state.headlines.get(domain)?;
            let headlines: Vec<RecentHeadline> = kept
                .iter()
                .filter(|h| h.at + NEWS_WINDOW_SECS >= now)
                .take(MAX_SHOWN - 1)
                .map(|h| shown(domain, h))
                .collect();
            (!headlines.is_empty()).then(|| Recent {
                site: Some(domain.to_string()),
                headlines,
            })
        };
        if let Some((domain, true)) = top {
            if let Some(block) = site_block(&self.read(), domain) {
                return Some(block);
            }
        }
        let topical = (!topic.is_empty() && topic.len() <= MAX_TOPIC_WORDS)
            .then(|| self.topic_block(&topic, wants_news, now))
            .flatten();
        if topical.is_some() || !wants_news {
            return topical;
        }
        let (domain, _) = top?;
        site_block(&self.read(), domain)
    }

    fn topic_block(&self, topic: &[String], wants_news: bool, now: u64) -> Option<Recent> {
        if self.read().words.is_none() {
            let mut state = self.write();
            if state.words.is_none() {
                state.words = Some(word_index(&state.headlines));
            }
        }
        let state = self.read();
        let words = state.words.as_ref()?;
        // Sites with every word in some headline, then their headlines
        // that hold every word.
        let mut sites: Option<HashSet<&str>> = None;
        for word in topic {
            let with: HashSet<&str> = words
                .get(word)
                .map(|sites| sites.iter().map(String::as_str).collect())
                .unwrap_or_default();
            sites = Some(match sites {
                None => with,
                Some(had) => had.intersection(&with).copied().collect(),
            });
        }
        let mut found: Vec<RecentHeadline> = Vec::new();
        for domain in sites.unwrap_or_default() {
            let Some(kept) = state.headlines.get(domain) else {
                continue;
            };
            let window = if wants_news {
                NEWS_WINDOW_SECS
            } else {
                TOPIC_WINDOW_SECS
            };
            found.extend(
                kept.iter()
                    .filter(|h| h.at + window >= now)
                    .filter(|h| {
                        let title = fold_words(&h.title);
                        topic.iter().all(|w| title.contains(w))
                    })
                    .take(MAX_SHOWN_PER_SITE)
                    .map(|h| shown(domain, h)),
            );
        }
        let distinct: HashSet<&str> = found.iter().map(|h| h.domain.as_str()).collect();
        let enough = if wants_news {
            !found.is_empty()
        } else {
            distinct.len() >= MIN_TOPIC_SITES
        };
        if !enough {
            return None;
        }
        found.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.url.cmp(&b.url)));
        found.truncate(MAX_SHOWN);
        Some(Recent {
            site: None,
            headlines: found,
        })
    }
}

fn shown(domain: &str, h: &Headline) -> RecentHeadline {
    RecentHeadline {
        domain: domain.to_string(),
        title: h.title.clone(),
        url: h.url.clone(),
        at: h.at,
    }
}

/// Folded word -> sites with a headline holding it.
fn word_index(headlines: &HashMap<String, Vec<Headline>>) -> HashMap<String, Vec<String>> {
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    for (domain, kept) in headlines {
        let words: HashSet<String> = kept.iter().flat_map(|h| fold_words(&h.title)).collect();
        for word in words {
            index.entry(word).or_default().push(domain.clone());
        }
    }
    index
}

/// `text`'s words ([`fold_word`]).
fn fold_words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(fold_word)
        .collect()
}

/// `word` lowercased, with a plural `s` dropped ("elections" ->
/// "election"), so a query and a title match whatever their number.
fn fold_word(word: &str) -> String {
    let word = word.to_lowercase();
    if word == "news" {
        return word;
    }
    match word.strip_suffix('s') {
        Some(stem) if stem.chars().count() >= 3 && !stem.ends_with('s') => stem.to_string(),
        _ => word,
    }
}

fn is_one_of(word: &str, list: &[&str]) -> bool {
    list.iter().any(|listed| fold_word(listed) == word)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = match fs::read(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!("cannot read {}: {err}", path.display());
            return None;
        }
    };
    match serde_json::from_slice(&text) {
        Ok(value) => Some(value),
        Err(err) => {
            warn!("cannot read {}: {err}", path.display());
            None
        }
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec(value)?)?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const HOUR: u64 = 3600;

    fn headline(site: &str, title: &str, ago: u64) -> Headline {
        let slug: String = title.split_whitespace().collect::<Vec<_>>().join("-");
        Headline {
            title: title.into(),
            url: format!("https://{site}/{slug}"),
            at: NOW - ago,
        }
    }

    fn shared(site: &str, headlines: Vec<Headline>) -> plumb_core::SiteRecord {
        let mut record = plumb_core::SiteRecord::new(site);
        record.news = headlines;
        record
    }

    fn store() -> (tempfile::TempDir, NewsStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = NewsStore::open(dir.path().join("news"));
        store.put_shared(
            vec![
                shared(
                    "news.com",
                    vec![
                        headline("news.com", "Elections in France tomorrow", HOUR),
                        headline("news.com", "Rust 2.0 released", 2 * HOUR),
                    ],
                ),
                shared(
                    "paper.org",
                    vec![
                        headline("paper.org", "French election: what to know", 3 * HOUR),
                        headline("paper.org", "Gardening tips", 5 * 24 * HOUR),
                    ],
                ),
                shared(
                    "blog.dev",
                    vec![headline("blog.dev", "Why I like Rust", 30 * HOUR)],
                ),
            ],
            NOW,
        );
        (dir, store)
    }

    fn titles(recent: &Recent) -> Vec<&str> {
        recent.headlines.iter().map(|h| h.title.as_str()).collect()
    }

    #[test]
    fn a_named_site_shows_its_latest_posts() {
        let (_dir, store) = store();
        let recent = store.recent("news", Some(("news.com", true)), NOW).unwrap();
        assert_eq!(recent.site.as_deref(), Some("news.com"));
        assert_eq!(
            titles(&recent),
            ["Elections in France tomorrow", "Rust 2.0 released"]
        );
        // A site the query does not name shows nothing of its own.
        assert_eq!(store.recent("paper", Some(("paper.org", false)), NOW), None);
    }

    #[test]
    fn a_topic_needs_two_sites_unless_news_is_asked_for() {
        let (_dir, store) = store();
        let rust = store.recent("rust", None, NOW).unwrap();
        assert_eq!(rust.site, None);
        assert_eq!(titles(&rust), ["Rust 2.0 released", "Why I like Rust"]);
        // "elections" and "election" are the same word.
        let election = store.recent("the election", None, NOW).unwrap();
        assert_eq!(
            titles(&election),
            [
                "Elections in France tomorrow",
                "French election: what to know"
            ]
        );
        assert_eq!(store.recent("france", None, NOW), None);
        let france = store.recent("france news", None, NOW).unwrap();
        assert_eq!(titles(&france), ["Elections in France tomorrow"]);
        // Every word must be in the title.
        assert_eq!(store.recent("rust gardening", None, NOW), None);
        // "new" is a word, not a request for news.
        assert_eq!(store.recent("new france", None, NOW), None);
        // Topics look back three days, unless news is asked for.
        assert_eq!(
            store
                .recent("gardening news", None, NOW)
                .unwrap()
                .headlines
                .len(),
            1
        );
        assert_eq!(store.recent("tips", None, NOW), None);
    }

    #[test]
    fn asking_for_a_sites_news_shows_its_posts() {
        let (_dir, store) = store();
        let recent = store
            .recent("paper news", Some(("paper.org", false)), NOW)
            .unwrap();
        assert_eq!(recent.site.as_deref(), Some("paper.org"));
    }

    #[test]
    fn feed_checks_set_when_each_feed_is_due() {
        let dir = tempfile::tempdir().unwrap();
        let store = NewsStore::open(dir.path());
        store.watch(vec![
            ("a.com".into(), "https://a.com/".into()),
            ("b.com".into(), "https://b.com/".into()),
            ("c.com".into(), "https://c.com/".into()),
        ]);
        assert_eq!(store.due(NOW, 2).len(), 2);
        let shared = store.apply(
            vec![
                FeedCheck {
                    domain: "a.com".into(),
                    outcome: FeedOutcome::Read {
                        feed: "https://a.com/rss".into(),
                        headlines: vec![headline("a.com", "New thing", 60)],
                        etag: Some("\"1\"".into()),
                        last_modified: None,
                    },
                },
                FeedCheck {
                    domain: "b.com".into(),
                    outcome: FeedOutcome::NoFeed,
                },
                FeedCheck {
                    domain: "c.com".into(),
                    outcome: FeedOutcome::NotModified,
                },
            ],
            NOW,
        );
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].domain, "a.com");
        assert_eq!(shared[0].news.len(), 1);
        assert!(store.due(NOW + HOUR - 1, 10).is_empty());
        let due: Vec<String> = store
            .due(NOW + HOUR, 10)
            .into_iter()
            .map(|t| t.homepage.domain)
            .collect();
        assert_eq!(due, ["a.com", "c.com"]);
        assert_eq!(store.due(NOW + HOUR, 10)[0].etag.as_deref(), Some("\"1\""));
        // Nothing new: a.com waits twice as long.
        store.apply(
            vec![FeedCheck {
                domain: "a.com".into(),
                outcome: FeedOutcome::NotModified,
            }],
            NOW + HOUR,
        );
        assert!(store
            .due(NOW + 3 * HOUR - 1, 10)
            .iter()
            .all(|t| t.homepage.domain != "a.com"));

        store.save().unwrap();
        let again = NewsStore::open(dir.path());
        assert_eq!(again.watched(), (3, 1));
        assert_eq!(again.headline_count(), 1);
        again.prune(NOW + 8 * 24 * HOUR);
        assert_eq!(again.headline_count(), 0);
    }
}
