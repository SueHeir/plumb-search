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
//! match the substantive topic terms in their title.

use plumb_core::locks::{Locked, RwLocked};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use plumb_core::news::{merge_headlines, NEWS_PUBLISHERS as PUBLISHERS, NEWS_WINDOW_SECS};
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
/// Headlines about a topic shown for a search that names a site and does
/// not ask for news ("amazon"): the site is what was searched for.
const MAX_SHOWN_BESIDE_NAME: usize = 2;
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
    "a", "an", "and", "about", "are", "as", "at", "be", "by", "for", "from", "how", "in", "is",
    "it", "of", "on", "or", "the", "to", "was", "what", "when", "who", "why", "with",
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
    /// Last local feed check, not the publication date of its articles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_status: Option<String>,
}

/// What a "Recent" block shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    /// The site whose latest posts these are, when the query names it;
    /// `None` for headlines about the query's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    pub headlines: Vec<RecentHeadline>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<NewsSource>,
}

/// Feed provenance is local observation metadata; peer headlines have no invented fetch date.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewsSource {
    pub domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    pub status: String,
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
    saving: Mutex<()>,
    budget: Option<Arc<plumb_core::storage::StorageBudget>>,
}

impl NewsStore {
    /// The store in `dir`, with what was saved there.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let watched = read_json(&dir.join(FEEDS_FILE)).unwrap_or_default();
        let headlines = read_json(&dir.join(HEADLINES_FILE)).unwrap_or_default();
        NewsStore {
            dir,
            budget: None,
            saving: Mutex::new(()),
            state: RwLock::new(State {
                watched,
                headlines,
                words: None,
                dirty: false,
            }),
        }
    }

    pub fn with_budget(mut self, budget: Option<Arc<plumb_core::storage::StorageBudget>>) -> Self {
        self.budget = budget;
        self
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read_locked()
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write_locked()
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
                    let feed = page.meta.feed.as_deref()?;
                    plumb_core::news::publisher_feed_allows(&r.domain, feed)
                        .then_some((r.domain.as_str(), feed))
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
            watched.checked_at = Some(now);
            watched.check_status = Some(
                match &check.outcome {
                    FeedOutcome::Read { .. } => "read",
                    FeedOutcome::NotModified => "not_modified",
                    FeedOutcome::NoFeed => "no_feed",
                    FeedOutcome::Failed(_) => "failed",
                }
                .into(),
            );
            let quieter = (watched.gap * 2).clamp(MIN_CHECK_GAP_SECS, MAX_CHECK_GAP_SECS);
            match check.outcome {
                FeedOutcome::Read {
                    feed,
                    headlines,
                    etag,
                    last_modified,
                } => {
                    if !plumb_core::news::publisher_feed_allows(&watched.domain, &feed) {
                        watched.check_status = Some("unpermitted_feed".into());
                        watched.gap = quieter;
                        watched.next_at = now + watched.gap;
                        continue;
                    }
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

    /// Writes the store to its folder, if it changed. The headlines are
    /// copied out first, so searches keep reading them while the write
    /// waits for the storage ledger, which a records fold can hold for
    /// minutes.
    pub fn save(&self) -> io::Result<()> {
        // One save at a time, so an older copy never lands after a newer one.
        let _saving = self.saving.locked();
        let (feeds, headlines) = {
            let mut state = self.write();
            if !state.dirty {
                return Ok(());
            }
            let copy = (
                serde_json::to_vec(&state.watched)?,
                serde_json::to_vec(&state.headlines)?,
            );
            state.dirty = false;
            copy
        };
        let written = self.write_files(&feeds, &headlines);
        if written.is_err() {
            self.write().dirty = true;
        }
        written
    }

    fn write_files(&self, feeds: &[u8], headlines: &[u8]) -> io::Result<()> {
        if self.budget.is_some() {
            plumb_core::storage::create_directory(&self.dir, self.budget.as_ref())
                .map_err(io::Error::other)?;
            for (name, bytes) in [(FEEDS_FILE, feeds), (HEADLINES_FILE, headlines)] {
                let path = self.dir.join(name);
                plumb_core::storage::write_atomic(
                    &path,
                    &path.with_extension("json.tmp"),
                    bytes,
                    self.budget.as_ref(),
                    None,
                )?;
            }
        } else {
            fs::create_dir_all(&self.dir)?;
            write_bytes(&self.dir.join(FEEDS_FILE), feeds)?;
            write_bytes(&self.dir.join(HEADLINES_FILE), headlines)?;
        }
        #[cfg(unix)]
        {
            fs::File::open(&self.dir)?.sync_all()?;
            if let Some(parent) = self.dir.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::File::open(parent)?.sync_all()?;
            }
        }
        Ok(())
    }

    /// Requests for a curated publisher, or for a site's news "from" it,
    /// stay publisher-bound even when its feeds are unavailable. Other
    /// queries match substantive topic terms within the existing time windows.
    pub fn recent(&self, query: &str, top: Option<(&str, bool)>, now: u64) -> Option<Recent> {
        let operators = plumb_core::Operators::parse(query);
        let words = fold_words(&operators.words);
        let wants_news = words.iter().any(|w| is_one_of(w, NEWS_WORDS));
        let topic: Vec<String> = words
            .into_iter()
            .filter(|w| !is_one_of(w, NEWS_WORDS) && !is_one_of(w, STOP_WORDS))
            .collect();
        let state = self.read();
        let named = top.and_then(|(domain, named)| named.then_some(domain));
        let source = requested_source(&operators.words, &topic, wants_news, &state);
        // A query can name a site whose name only starts with a watched
        // site's: "fox news" is foxnews.com and "abc news" is ABC News, not
        // fox.com's or abc.es's posts. That named site is then the request.
        let gives_way = source.as_ref().is_some_and(|source| {
            source.bare && named.is_some_and(|domain| !source.domains.iter().any(|d| d == domain))
        });
        let spelled = gives_way
            || !topic.is_empty()
                && named.is_some_and(|domain| spells_site(&operators.words, domain));
        if let Some(source) = source.filter(|_| !spelled) {
            let bare = source.bare;
            let block = source_block(&state, &operators, query, source, now);
            // A watched site the query only names, with nothing current,
            // leaves the block to the query's topic: "google news" is not
            // google.com's quiet feed. A curated publisher, or a site asked
            // for "from" or "by", stays the request.
            if !bare || !block.headlines.is_empty() {
                return Some(block);
            }
        }
        // Ordinary named navigation may show that site's posts, but never invent a source request.
        if let Some((domain, true)) = top.filter(|_| spelled || !wants_news || topic.is_empty()) {
            let headlines: Vec<_> = state
                .headlines
                .get(domain)
                .into_iter()
                .flatten()
                .filter(|h| fresh(h.at, NEWS_WINDOW_SECS, now) && allows_headline(&operators, h))
                .take(MAX_SHOWN - 1)
                .map(|h| shown(domain, h))
                .collect();
            if !headlines.is_empty() {
                return Some(Recent {
                    site: Some(domain.into()),
                    headlines,
                    status: "available".into(),
                    sources: vec![source_status(&state, domain)],
                });
            }
        }
        drop(state);
        let mut topical = (!topic.is_empty() && topic.len() <= MAX_TOPIC_WORDS)
            .then(|| self.topic_block(&topic, query, wants_news, now))
            .flatten();
        if !wants_news && top.is_some_and(|(_, named)| named) {
            if let Some(block) = &mut topical {
                block.headlines.truncate(MAX_SHOWN_BESIDE_NAME);
            }
        }
        if topical.is_some() || !wants_news {
            return topical;
        }
        topic
            .is_empty()
            .then(|| self.latest_block(query, now))
            .flatten()
    }

    fn latest_block(&self, query: &str, now: u64) -> Option<Recent> {
        let operators = plumb_core::Operators::parse(query);
        let state = self.read();
        let mut found: Vec<RecentHeadline> = state
            .headlines
            .iter()
            .filter_map(|(domain, kept)| {
                let newest = kept
                    .iter()
                    .filter(|h| {
                        fresh(h.at, TOPIC_WINDOW_SECS, now) && allows_headline(&operators, h)
                    })
                    .max_by_key(|h| h.at)?;
                fresh(newest.at, TOPIC_WINDOW_SECS, now).then(|| shown(domain, newest))
            })
            .collect();
        if found.is_empty() {
            return None;
        }
        found.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.url.cmp(&b.url)));
        drop_copies(&mut found);
        found.truncate(MAX_SHOWN);
        Some(Recent {
            site: None,
            sources: found
                .iter()
                .map(|h| source_status(&state, &h.domain))
                .collect(),
            headlines: found,
            status: "available".into(),
        })
    }

    fn topic_block(
        &self,
        topic: &[String],
        query: &str,
        wants_news: bool,
        now: u64,
    ) -> Option<Recent> {
        let operators = plumb_core::Operators::parse(query);
        if self.read().words.is_none() {
            let mut state = self.write();
            if state.words.is_none() {
                state.words = Some(word_index(&state.headlines));
            }
        }
        let state = self.read();
        let words = state.words.as_ref()?;
        // Union retrieval preserves candidates with optional task modifiers; subject/event constraints
        // are checked together on each title below, before diversity and output caps.
        let sites: HashSet<&str> = topic
            .iter()
            .filter_map(|word| words.get(word))
            .flatten()
            .map(String::as_str)
            .collect();
        let mut found: Vec<RecentHeadline> = Vec::new();
        for domain in sites {
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
                    .filter(|h| fresh(h.at, window, now) && allows_headline(&operators, h))
                    .filter(|h| topic_matches(topic, &h.title, query))
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
        drop_copies(&mut found);
        found.truncate(MAX_SHOWN);
        Some(Recent {
            site: None,
            sources: found
                .iter()
                .map(|h| source_status(&state, &h.domain))
                .collect(),
            headlines: found,
            status: "available".into(),
        })
    }
}

/// The publisher a news query asks for, the sites it publishes on and
/// the topic asked of it.
struct Source {
    publisher: String,
    domains: Vec<String>,
    topic: Vec<String>,
    /// A watched site the query only names ("fox news" for fox.com), not
    /// a curated publisher or one asked for "from" or "by".
    bare: bool,
}

fn requested_source(
    query: &str,
    topic: &[String],
    wants_news: bool,
    state: &State,
) -> Option<Source> {
    if !wants_news {
        return None;
    }
    let normalized = plumb_core::normalize_text(query);
    let about = normalized.contains("about ");
    let mut candidates: Vec<(String, Vec<String>, Vec<String>, bool)> = PUBLISHERS
        .iter()
        .map(|(domain, domains, aliases)| {
            (
                domain.to_string(),
                domains.iter().map(|d| d.to_string()).collect(),
                aliases.iter().map(|a| a.to_string()).collect(),
                true,
            )
        })
        .collect();
    let domains: Vec<&str> = state
        .watched
        .iter()
        .map(|w| w.domain.as_str())
        .chain(state.headlines.keys().map(String::as_str))
        .collect();
    let mut known: HashSet<String> = candidates
        .iter()
        .flat_map(|(_, domains, _, _)| domains.iter().cloned())
        .collect();
    for domain in domains {
        if !known.insert(domain.to_string()) {
            continue;
        }
        let label = domain.split('.').next().unwrap_or("");
        candidates.push((
            domain.into(),
            vec![domain.into()],
            vec![label.into(), domain.into()],
            false,
        ));
    }
    candidates
        .into_iter()
        .find_map(|(publisher, domains, aliases, curated)| {
            for alias in aliases {
                let folded = fold_words(&alias);
                if !about && topic == folded {
                    return Some(Source {
                        publisher,
                        domains,
                        topic: Vec::new(),
                        bare: !curated,
                    });
                }
                for marker in [" from ", " by "] {
                    let suffix = format!("{marker}{alias}");
                    if let Some(before) = normalized.strip_suffix(&suffix) {
                        let topic = fold_words(before)
                            .into_iter()
                            .filter(|w| !is_one_of(w, NEWS_WORDS) && !is_one_of(w, STOP_WORDS))
                            .collect();
                        return Some(Source {
                            publisher,
                            domains,
                            topic,
                            bare: false,
                        });
                    }
                }
            }
            None
        })
}

/// The latest posts of `source`, newest first, about its topic if any.
fn source_block(
    state: &State,
    operators: &plumb_core::Operators,
    query: &str,
    source: Source,
    now: u64,
) -> Recent {
    let Source {
        publisher,
        domains,
        topic,
        ..
    } = source;
    let publishes = |url: &str| {
        plumb_core::host_of(url).is_some_and(|host| {
            domains
                .iter()
                .any(|d| host == *d || host.ends_with(&format!(".{d}")))
        })
    };
    let had_current_source = domains.iter().any(|domain| {
        state.headlines.get(domain).is_some_and(|headlines| {
            headlines
                .iter()
                .any(|h| fresh(h.at, NEWS_WINDOW_SECS, now) && publishes(&h.url))
        })
    });
    let mut headlines: Vec<RecentHeadline> = domains
        .iter()
        .flat_map(|domain| {
            state
                .headlines
                .get(domain)
                .into_iter()
                .flatten()
                .filter(|h| fresh(h.at, NEWS_WINDOW_SECS, now) && allows_headline(operators, h))
                .filter(|h| topic.is_empty() || topic_matches(&topic, &h.title, query))
                // Feed discovery permits alternate feed hosts; article URLs must still belong to the publisher.
                .filter(|h| publishes(&h.url))
                .map(|h| shown(domain, h))
        })
        .collect();
    headlines.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.url.cmp(&b.url)));
    drop_copies(&mut headlines);
    headlines.truncate(MAX_SHOWN - 1);
    let status = match (headlines.is_empty(), had_current_source) {
        (false, _) => "available",
        (true, true) => "no_matching_headlines",
        (true, false) => "source_unavailable",
    };
    Recent {
        site: Some(publisher),
        status: status.into(),
        sources: domains
            .iter()
            .map(|domain| source_status(state, domain))
            .collect(),
        headlines,
    }
}

/// Whether `query` spells the name of the site `domain`, its words run
/// together: "fox news" for foxnews.com.
fn spells_site(query: &str, domain: &str) -> bool {
    let label = domain
        .trim_start_matches("www.")
        .split('.')
        .next()
        .unwrap_or("");
    let squashed: String = plumb_core::normalize_text(query)
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    !label.is_empty() && squashed == label.to_lowercase()
}

fn source_status(state: &State, domain: &str) -> NewsSource {
    let watched = state.watched.iter().find(|w| w.domain == domain);
    NewsSource {
        domain: domain.into(),
        feed: watched.and_then(|w| w.feed.clone()),
        checked_at: watched.and_then(|w| w.checked_at),
        status: watched
            .and_then(|w| w.check_status.clone())
            .unwrap_or_else(|| {
                if state.headlines.contains_key(domain) {
                    "shared_or_saved"
                } else {
                    "not_watched"
                }
                .into()
            }),
    }
}

fn allows_headline(operators: &plumb_core::Operators, h: &Headline) -> bool {
    plumb_core::host_of(&h.url).is_some_and(|host| operators.allows(&host, [h.title.as_str()]))
}

fn fresh(at: u64, window: u64, now: u64) -> bool {
    at <= now && at.saturating_add(window) >= now
}

fn topic_matches(topic: &[String], title: &str, query: &str) -> bool {
    let title_words = fold_words(title);
    // Remove only bounded presentation modifiers; the subject and event remain required.
    let substantive: Vec<_> = topic
        .iter()
        .filter(|word| {
            !["coverage", "report", "reports", "story", "stories"]
                .iter()
                .any(|modifier| is_one_of(word, std::slice::from_ref(modifier)))
        })
        .collect();
    if substantive.is_empty() || !substantive.iter().all(|w| title_words.contains(w)) {
        return false;
    }
    let title = plumb_core::normalize_text(title);
    query
        .split('"')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .all(|(_, phrase)| title.contains(&plumb_core::normalize_text(phrase)))
}

/// Keeps the first of headlines with the same words: one story that
/// papers of a group (smh.com.au, theage.com.au) all run.
fn drop_copies(found: &mut Vec<RecentHeadline>) {
    let mut seen = HashSet::new();
    let mut urls = HashSet::new();
    found.retain(|h| urls.insert(h.url.clone()) && seen.insert(fold_words(&h.title)));
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

fn write_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

/// Absolute UTC publication time alongside Unix seconds and relative display age.
pub(crate) fn published_date(at: u64) -> Option<String> {
    chrono::DateTime::from_timestamp(i64::try_from(at).ok()?, 0)
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
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
    fn a_named_site_keeps_topic_headlines_few_and_copies_go() {
        let dir = tempfile::tempdir().unwrap();
        let store = NewsStore::open(dir.path().join("news"));
        let story =
            |site: &str, title: &str, ago: u64| shared(site, vec![headline(site, title, ago)]);
        store.put_shared(
            vec![
                story("a.com", "Amazon Prime Day deals", HOUR),
                story("b.com", "Best Amazon deals now", 2 * HOUR),
                story("c.com", "Amazon earnings beat", 3 * HOUR),
                story(
                    "smh.com.au",
                    "World Cup: Amazon streams the final",
                    4 * HOUR,
                ),
                story(
                    "theage.com.au",
                    "World Cup: Amazon streams the final",
                    4 * HOUR,
                ),
            ],
            NOW,
        );
        let named = store
            .recent("amazon", Some(("amazon.com", true)), NOW)
            .unwrap();
        assert_eq!(named.headlines.len(), 2, "{:?}", titles(&named));
        let topic = store.recent("amazon", None, NOW).unwrap();
        assert_eq!(topic.headlines.len(), 4, "{:?}", titles(&topic));
        let asked = store
            .recent("amazon news", Some(("amazon.com", true)), NOW)
            .unwrap();
        assert_eq!(asked.headlines.len(), 4, "{:?}", titles(&asked));
    }

    #[test]
    fn news_alone_shows_the_newest_headline_of_each_site() {
        let (_dir, store) = store();
        for query in ["news", "latest news today"] {
            let recent = store.recent(query, Some(("cnn.com", false)), NOW).unwrap();
            assert_eq!(recent.site, None);
            assert_eq!(
                titles(&recent),
                [
                    "Elections in France tomorrow",
                    "French election: what to know",
                    "Why I like Rust"
                ],
                "{query}"
            );
        }
        assert_eq!(store.recent("news", None, NOW + 3 * 24 * HOUR), None);
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
    fn a_site_named_with_news_is_not_a_shorter_sites_news() {
        let (_dir, store) = store();
        // fox.com is watched and quiet; foxnews.com and others post.
        store.watch(vec![("fox.com".into(), "https://www.fox.com/".into())]);
        store.put_shared(
            vec![
                shared(
                    "foxnews.com",
                    vec![headline("foxnews.com", "Senate passes the budget", HOUR)],
                ),
                shared(
                    "paper.org",
                    vec![headline(
                        "paper.org",
                        "Fox network renews its hit show",
                        HOUR,
                    )],
                ),
                shared(
                    "news.com",
                    vec![headline("news.com", "Fox wins the ratings week", 2 * HOUR)],
                ),
            ],
            NOW,
        );
        // The query spells the site it names, foxnews.com: its posts.
        let named = store
            .recent("fox news", Some(("foxnews.com", true)), NOW)
            .unwrap();
        assert_eq!(named.site.as_deref(), Some("foxnews.com"));
        assert_eq!(titles(&named), ["Senate passes the budget"]);
        // Not naming a site, it is not fox.com's quiet feed but news about Fox.
        let topic = store.recent("fox news", None, NOW).unwrap();
        assert_eq!(topic.site, None);
        assert_eq!(
            titles(&topic),
            [
                "Fox network renews its hit show",
                "Fox wins the ratings week"
            ]
        );
        // Asked for "from" it, a quiet site stays the request.
        let asked = store.recent("budget news from fox", None, NOW).unwrap();
        assert_eq!(asked.site.as_deref(), Some("fox.com"));
        assert_eq!(asked.status, "source_unavailable");
    }

    #[test]
    fn a_named_site_outranks_a_watched_site_that_shares_its_first_word() {
        let (_dir, store) = store();
        store.watch(vec![("abc.es".into(), "https://www.abc.es/".into())]);
        store.put_shared(
            vec![
                shared(
                    "abc.es",
                    vec![headline("abc.es", "El Parlamento aprueba la ley", HOUR)],
                ),
                shared(
                    "go.com",
                    vec![headline("go.com", "Storm reaches the coast", HOUR)],
                ),
            ],
            NOW,
        );
        // "abc news" names ABC News (go.com), not the Spanish paper abc.es.
        let named = store
            .recent("abc news", Some(("go.com", true)), NOW)
            .unwrap();
        assert_eq!(named.site.as_deref(), Some("go.com"));
        assert_eq!(titles(&named), ["Storm reaches the coast"]);
        // Naming abc.es itself, or nothing, it is abc.es's posts.
        for top in [Some(("abc.es", true)), None] {
            let paper = store.recent("abc news", top, NOW).unwrap();
            assert_eq!(paper.site.as_deref(), Some("abc.es"));
        }
    }

    #[test]
    fn publisher_requests_do_not_fall_back_to_mentions_and_accept_bbc_article_hosts() {
        let (_dir, store) = store();
        store.put_shared(
            vec![shared(
                "other.com",
                vec![headline(
                    "other.com",
                    "BBC latest headlines controversy",
                    HOUR,
                )],
            )],
            NOW,
        );
        let unavailable = store
            .recent("BBC latest news", Some(("other.com", false)), NOW)
            .unwrap();
        assert_eq!(unavailable.site.as_deref(), Some("bbc.com"));
        assert_eq!(unavailable.status, "source_unavailable");
        assert!(unavailable.headlines.is_empty());
        store.put_shared(
            vec![shared(
                "bbc.co.uk",
                vec![
                    Headline {
                        title: "NVIDIA earnings beat forecasts".into(),
                        url: "https://www.bbc.com/news/business/1".into(),
                        at: NOW - HOUR,
                    },
                    Headline {
                        title: "Spoofed publisher article".into(),
                        url: "https://bbc.com.attacker.test/news/2".into(),
                        at: NOW - HOUR,
                    },
                ],
            )],
            NOW,
        );
        let bbc = store.recent("latest news from BBC", None, NOW).unwrap();
        assert_eq!(bbc.status, "available");
        assert_eq!(titles(&bbc), ["NVIDIA earnings beat forecasts"]);
        assert_eq!(bbc.headlines[0].domain, "bbc.co.uk");
        assert_eq!(bbc.sources[1].checked_at, None);
        let subject = store.recent("news about BBC", None, NOW).unwrap();
        assert_eq!(subject.site, None);
        assert_eq!(subject.headlines[0].domain, "other.com");
    }

    #[test]
    fn topics_keep_subject_event_phrases_dates_and_diversity() {
        let (_dir, store) = store();
        store.put_shared(
            vec![shared(
                "events.com",
                vec![
                    headline("events.com", "NVIDIA earnings beat forecasts", HOUR),
                    headline("events.com", "NVIDIA unveils new GPU", HOUR),
                    headline("events.com", "AMD earnings beat forecasts", HOUR),
                    headline("events.com", "Earnings predictions for NVIDIA", HOUR),
                ],
            )],
            NOW,
        );
        let report = store
            .recent("NVIDIA earnings news coverage", None, NOW)
            .unwrap();
        assert_eq!(report.headlines.len(), 2);
        assert!(report
            .headlines
            .iter()
            .all(|h| h.title.contains("NVIDIA") && h.title.to_lowercase().contains("earnings")));
        let phrase = store.recent("\"NVIDIA earnings\" news", None, NOW).unwrap();
        assert_eq!(titles(&phrase), ["NVIDIA earnings beat forecasts"]);
        assert_eq!(
            store.recent("NVIDIA earnings news", None, NOW + NEWS_WINDOW_SECS + HOUR),
            None
        );
        assert!(!fresh(NOW + 1, NEWS_WINDOW_SECS, NOW));
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
        let checked = store.recent("news from a", None, NOW).unwrap();
        assert_eq!(checked.sources[0].checked_at, Some(NOW));
        assert_eq!(checked.sources[0].status, "read");
        assert_eq!(
            checked.sources[0].feed.as_deref(),
            Some("https://a.com/rss")
        );
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

    #[test]
    fn headlines_stay_readable_while_a_save_waits_for_storage() {
        let (dir, store) = store();
        let budget =
            Arc::new(plumb_core::storage::StorageBudget::open(dir.path(), u64::MAX).unwrap());
        let store = Arc::new(store.with_budget(Some(Arc::clone(&budget))));
        let before = store.headline_count();
        // A records fold holds the storage ledger.
        let fold = budget.mutation();
        let saving = Arc::clone(&store);
        let save = std::thread::spawn(move || saving.save());
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!save.is_finished());
        // Searches still read the headlines, and new ones still arrive.
        let reading = Arc::clone(&store);
        let search = std::thread::spawn(move || {
            let found = reading.recent("election", None, NOW).is_some();
            reading.put_shared(
                vec![shared(
                    "late.com",
                    vec![headline("late.com", "Late news", HOUR)],
                )],
                NOW,
            );
            found
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !search.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(search.is_finished(), "a search waited for the save");
        assert!(search.join().unwrap());
        assert_eq!(store.headline_count(), before + 1);
        drop(fold);
        save.join().unwrap().unwrap();
        // What arrived during the save is written by the next one.
        let saved = || NewsStore::open(dir.path().join("news")).headline_count();
        assert_eq!(saved(), before);
        store.save().unwrap();
        assert_eq!(saved(), before + 1);
    }

    #[test]
    fn a_failed_save_is_tried_again() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "a file where the folder should be").unwrap();
        let store = NewsStore::open(blocked.join("news"));
        store.put_shared(
            vec![shared(
                "news.com",
                vec![headline("news.com", "Some news", HOUR)],
            )],
            NOW,
        );
        assert!(store.save().is_err());
        std::fs::remove_file(&blocked).unwrap();
        store.save().unwrap();
        assert_eq!(NewsStore::open(blocked.join("news")).headline_count(), 1);
    }
}
