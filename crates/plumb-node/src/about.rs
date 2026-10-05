//! "About you": what a searcher tells the node about themselves, kept on
//! the node for their browser only, next to their search history (see
//! [`crate::history`]).
//!
//! - Interests ("cooking", "rust programming"): results that match one get a
//!   small boost and say which interest they match, so an ambiguous name
//!   like "rust" or "jaguar" leans towards the meaning the searcher cares
//!   about.
//! - Sites always first: these come first whenever a search finds them.
//! - Sites never shown: these are left out of every result list.
//! - Your town: where "coffee near me" looks for places (see
//!   [`crate::places`]). Plumb never works it out from the searcher's
//!   address.
//!
//! The profile is used only after results are found, on the node itself.
//! It is never part of a search sent to other nodes, so nothing of it
//! leaves the node.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result};
use plumb_core::collapse_whitespace;
use plumb_core::SiteRecord;
use plumb_index::Hit;
use serde::{Deserialize, Serialize};

use crate::history::valid_profile;

/// Most interests kept.
pub const MAX_INTERESTS: usize = 40;
/// Most sites kept in each of the always-first and never-shown lists.
pub const MAX_SITES: usize = 100;
/// Longest interest kept, in characters.
const MAX_INTEREST_CHARS: usize = 60;
/// Longest domain kept.
const MAX_DOMAIN_CHARS: usize = 253;
/// Longest town kept, in characters.
pub const MAX_TOWN_CHARS: usize = 80;

/// Score added to a site the searcher put first: more than a site opened
/// before for the same search, so their own choice wins.
pub const PINNED_BONUS: f32 = 0.5;
/// Score added to a site that matches one of the searcher's interests:
/// enough to settle a close call between two meanings of a name, not
/// enough to beat a much better match.
pub const INTEREST_BONUS: f32 = 0.1;

/// One write at a time: profiles are small.
static WRITING: Mutex<()> = Mutex::new(());

/// What a searcher told the node about themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct About {
    /// As typed, once each, in the order given.
    pub interests: Vec<String>,
    /// Domains always put first.
    pub pinned: Vec<String>,
    /// Domains never shown.
    pub hidden: Vec<String>,
    /// The searcher's town as they typed it ("Denver, CO"), for places
    /// "near me"; empty when not given.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub town: String,
}

/// Why a result was moved, for its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason<'a> {
    /// The site is one the searcher always wants first.
    Pinned,
    /// The site matches this interest.
    Interest(&'a str),
}

impl About {
    /// A profile from the About page's form: one item per line or comma.
    pub fn from_form(interests: &str, pinned: &str, hidden: &str) -> About {
        let mut about = About {
            interests: items(interests)
                .filter_map(|item| clean_interest(&item))
                .collect(),
            pinned: items(pinned)
                .filter_map(|item| clean_domain(&item))
                .collect(),
            hidden: items(hidden)
                .filter_map(|item| clean_domain(&item))
                .collect(),
            town: String::new(),
        };
        dedup_by_key(&mut about.interests, |i| i.to_lowercase());
        dedup_by_key(&mut about.pinned, Clone::clone);
        dedup_by_key(&mut about.hidden, Clone::clone);
        // A site cannot be both: hiding it wins.
        about.pinned.retain(|d| !about.hidden.contains(d));
        about.interests.truncate(MAX_INTERESTS);
        about.pinned.truncate(MAX_SITES);
        about.hidden.truncate(MAX_SITES);
        about
    }

    /// This profile with the town `town` as typed, cleaned.
    pub fn with_town(mut self, town: &str) -> About {
        let town: String = town
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(MAX_TOWN_CHARS)
            .collect();
        self.town = collapse_whitespace(&town);
        self
    }

    /// The searcher's town, when they gave one.
    pub fn town(&self) -> Option<&str> {
        (!self.town.is_empty()).then_some(self.town.as_str())
    }

    /// Topics as typed in a form, one per line or comma, cleaned, once
    /// each, at most [`MAX_INTERESTS`].
    pub fn topics_from_text(text: &str) -> Vec<String> {
        About::from_form(text, "", "").interests
    }

    /// Whether nothing is set.
    pub fn is_empty(&self) -> bool {
        self.interests.is_empty()
            && self.pinned.is_empty()
            && self.hidden.is_empty()
            && self.town.is_empty()
    }

    /// Whether `domain`, or a site it belongs to, is never shown.
    pub fn hides(&self, domain: &str) -> bool {
        self.hidden.iter().any(|h| covers(h, domain))
    }

    /// Why `hit` is moved up, if it is: being put first wins over an
    /// interest.
    pub fn reason(&self, hit: &Hit) -> Option<Reason<'_>> {
        if self.pinned.iter().any(|p| covers(p, &hit.domain)) {
            return Some(Reason::Pinned);
        }
        if self.interests.is_empty() {
            return None;
        }
        let words = hit_words(hit);
        self.interests
            .iter()
            .find(|interest| matches_words(interest, &words))
            .map(|interest| Reason::Interest(interest))
    }

    /// Leaves out the hidden sites of `hits` and moves the pinned ones and
    /// those matching an interest up.
    pub fn apply(&self, hits: &mut Vec<Hit>) {
        if self.is_empty() {
            return;
        }
        hits.retain(|hit| !self.hides(&hit.domain));
        let mut changed = false;
        for hit in hits.iter_mut() {
            let bonus = match self.reason(hit) {
                Some(Reason::Pinned) => PINNED_BONUS,
                Some(Reason::Interest(_)) => INTEREST_BONUS,
                None => continue,
            };
            hit.score += bonus;
            changed = true;
        }
        if changed {
            // Stable, so equal scores keep their order.
            hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        }
    }
}

/// The items of a form field: split on lines and commas, trimmed.
fn items(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(['\n', ',', ';'])
        .map(|item| collapse_whitespace(item).trim().to_owned())
        .filter(|item| !item.is_empty())
}

fn dedup_by_key(items: &mut Vec<String>, key: impl Fn(&String) -> String) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(key(item)));
}

fn clean_interest(item: &str) -> Option<String> {
    let item: String = item
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_INTEREST_CHARS)
        .collect();
    let item = item.trim();
    let has_words = words_of(item).next().is_some();
    has_words.then(|| item.to_owned())
}

/// A domain as typed ("https://www.Example.com/path") as kept
/// ("example.com"); `None` when it is not one.
pub fn clean_domain(item: &str) -> Option<String> {
    let item = item.trim().to_ascii_lowercase();
    let item = item
        .strip_prefix("https://")
        .or_else(|| item.strip_prefix("http://"))
        .unwrap_or(&item);
    let host = item.split(['/', '?', '#']).next().unwrap_or_default();
    // No port, and no user name.
    let host = host.rsplit('@').next().unwrap_or_default();
    let host = host.split(':').next().unwrap_or_default();
    let host = host.strip_prefix("www.").unwrap_or(host).trim_matches('.');
    let valid = !host.is_empty()
        && host.len() <= MAX_DOMAIN_CHARS
        && host.contains('.')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !host.split('.').any(str::is_empty);
    valid.then(|| host.to_owned())
}

/// Whether the site `listed` covers `domain`: the same site or one under
/// it (`example.com` covers `blog.example.com`).
fn covers(listed: &str, domain: &str) -> bool {
    domain == listed
        || domain
            .strip_suffix(listed)
            .is_some_and(|rest| rest.ends_with('.'))
}

/// Whether every word of `interest` is among `words`.
fn matches_words(interest: &str, words: &HashSet<String>) -> bool {
    let mut wanted = words_of(interest).peekable();
    wanted.peek().is_some() && wanted.all(|w| words.contains(&w))
}

/// Topics to match sites against when choosing which sites a node keeps
/// or crawls first: a node's focus topics, and the interests of the About
/// profiles kept on it.
#[derive(Debug, Clone, Default)]
pub struct Topics {
    /// Each topic's words, stemmed.
    topics: Vec<Vec<String>>,
}

impl Topics {
    pub fn new<'a>(topics: impl IntoIterator<Item = &'a String>) -> Topics {
        let mut seen = HashSet::new();
        let topics = topics
            .into_iter()
            .map(|topic| words_of(topic).collect::<Vec<_>>())
            .filter(|words| !words.is_empty() && seen.insert(words.clone()))
            .collect();
        Topics { topics }
    }

    pub fn is_empty(&self) -> bool {
        self.topics.is_empty()
    }

    /// A key that changes when the topics do.
    pub fn key(&self) -> String {
        let mut topics: Vec<String> = self.topics.iter().map(|t| t.join(" ")).collect();
        topics.sort();
        topics.join(",")
    }

    /// Whether `record` is about one of the topics: every word of a topic
    /// is in its name, description, Wikidata facts or headings.
    pub fn matches(&self, record: &SiteRecord) -> bool {
        if self.topics.is_empty() {
            return false;
        }
        let words = record_words(record);
        self.topics
            .iter()
            .any(|topic| topic.iter().all(|w| words.contains(w)))
    }
}

/// The words a site record is matched on.
fn record_words(record: &SiteRecord) -> HashSet<String> {
    let mut words = HashSet::new();
    let mut add = |text: &str| words.extend(words_of(text));
    add(&record.domain.replace(['.', '-'], " "));
    for text in [&record.title, &record.description, &record.about]
        .into_iter()
        .flatten()
    {
        add(text);
    }
    for text in record
        .kinds
        .iter()
        .chain(&record.aliases)
        .chain(&record.headings)
    {
        add(text);
    }
    words
}

/// Every interest of the About profiles in `dir`, a node's history folder.
pub fn all_interests(dir: &Path) -> Vec<String> {
    all_profiles(dir)
        .into_iter()
        .flat_map(|about| about.interests)
        .collect()
}

/// Every site the About profiles in `dir` always put first.
pub fn all_pinned(dir: &Path) -> Vec<String> {
    all_profiles(dir)
        .into_iter()
        .flat_map(|about| about.pinned)
        .collect()
}

/// Every town the About profiles in `dir` give, once each.
pub fn all_towns(dir: &Path) -> Vec<String> {
    let mut towns: Vec<String> = all_profiles(dir)
        .iter()
        .filter_map(|about| about.town().map(str::to_owned))
        .collect();
    towns.sort();
    towns.dedup();
    towns
}

/// The About profiles in `dir`, a node's history folder.
fn all_profiles(dir: &Path) -> Vec<About> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut profiles = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(profile) = name.to_str().and_then(|n| n.strip_suffix(".about.json")) else {
            continue;
        };
        if !valid_profile(profile) {
            continue;
        }
        if let Some(about) = read(&entry.path()) {
            profiles.push(about);
        }
    }
    profiles
}

/// The words a result is matched on: its name, description and domain.
fn hit_words(hit: &Hit) -> HashSet<String> {
    let text = [
        hit.title.as_deref().unwrap_or_default(),
        hit.description.as_deref().unwrap_or_default(),
        &hit.domain.replace(['.', '-'], " "),
    ]
    .join(" ");
    words_of(&text).collect()
}

/// The words of `text`, lowercased and cut to a rough stem, so that
/// "games", "gaming" and "game" are one word.
fn words_of(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| stem(&word.to_lowercase()))
}

fn stem(word: &str) -> String {
    let mut word = word.to_owned();
    for suffix in ["ing", "ers", "er", "es", "s", "e"] {
        if word.chars().count() > suffix.len() + 2 {
            if let Some(rest) = word.strip_suffix(suffix) {
                word = rest.to_owned();
                break;
            }
        }
    }
    // "programm" (from "programming") is "program".
    let chars: Vec<char> = word.chars().collect();
    if let [.., a, b] = chars[..] {
        if a == b && chars.len() > 3 && !matches!(a, 'a' | 'e' | 'i' | 'o' | 'u' | 'l' | 's' | 'z')
        {
            word.pop();
        }
    }
    word
}

/// The About profiles of a node, one file per browser profile next to its
/// history: `DIR/history/<id>.about.json`.
#[derive(Debug, Clone)]
pub struct AboutStore {
    dir: PathBuf,
}

impl AboutStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        AboutStore { dir: dir.into() }
    }

    fn path(&self, profile: &str) -> Option<PathBuf> {
        valid_profile(profile).then(|| self.dir.join(format!("{profile}.about.json")))
    }

    /// The profile of `profile`; empty when it has none or cannot be read.
    pub fn load(&self, profile: &str) -> About {
        self.path(profile)
            .and_then(|path| read(&path))
            .unwrap_or_default()
    }

    /// Saves `about` as the profile of `profile`; an empty one is deleted.
    pub fn save(&self, profile: &str, about: &About) -> Result<()> {
        let Some(path) = self.path(profile) else {
            anyhow::bail!("not a profile id: {profile:?}");
        };
        let _writing = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
        if about.is_empty() {
            return remove(&path);
        }
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        crate::node::store::write_atomically(&path, &serde_json::to_vec(about)?)
    }
}

fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("deleting {}", path.display()))
        }
        _ => Ok(()),
    }
}

fn read(path: &Path) -> Option<About> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::new_profile;

    fn hit(domain: &str, title: &str, description: &str, score: f32) -> Hit {
        Hit {
            domain: domain.into(),
            url: format!("https://{domain}/"),
            title: Some(title.into()),
            description: Some(description.into()),
            score,
            text_score: score,
            link_score: 0.0,
            country: None,
            named: true,
            official: false,
            key_pages: Vec::new(),
        }
    }

    #[test]
    fn the_form_is_read_once_each_and_cleaned() {
        let about = About::from_form(
            "Cooking, rust programming\ncooking\n  \n<b>",
            "https://www.Example.com/path\nnot a site, github.com:443",
            "pinterest.com\nexample.com\n../etc",
        );
        assert_eq!(about.interests, ["Cooking", "rust programming", "<b>"]);
        assert_eq!(about.pinned, ["github.com"]);
        assert_eq!(about.hidden, ["pinterest.com", "example.com"]);
    }

    #[test]
    fn interests_settle_which_meaning_of_a_name_comes_first() {
        let mut hits = vec![
            hit(
                "rust.facepunch.com",
                "Rust",
                "A multiplayer survival game",
                0.9,
            ),
            hit(
                "rust-lang.org",
                "Rust Programming Language",
                "A language empowering everyone",
                0.85,
            ),
        ];
        let about = About::from_form("programming", "", "");
        about.apply(&mut hits);
        assert_eq!(hits[0].domain, "rust-lang.org");
        assert_eq!(
            about.reason(&hits[0]),
            Some(Reason::Interest("programming"))
        );

        let gamer = About::from_form("gaming", "", "");
        gamer.apply(&mut hits);
        assert_eq!(hits[0].domain, "rust.facepunch.com");
    }

    #[test]
    fn an_interest_does_not_beat_a_much_better_match() {
        let mut hits = vec![
            hit("chase.com", "Chase Bank", "Banking", 1.5),
            hit("chasecooking.com", "Chase cooking", "Recipes", 0.4),
        ];
        About::from_form("cooking", "", "").apply(&mut hits);
        assert_eq!(hits[0].domain, "chase.com");
    }

    #[test]
    fn pinned_sites_come_first_and_hidden_ones_go() {
        let mut hits = vec![
            hit("pinterest.com", "Pinterest", "Ideas", 1.0),
            hit("allrecipes.com", "Allrecipes", "Recipes", 0.8),
            hit("seriouseats.com", "Serious Eats", "Recipes", 0.5),
            hit("uk.pinterest.com", "Pinterest UK", "Ideas", 0.4),
        ];
        let about = About::from_form("", "seriouseats.com", "pinterest.com");
        about.apply(&mut hits);
        let domains: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
        assert_eq!(domains, ["seriouseats.com", "allrecipes.com"]);
        assert_eq!(about.reason(&hits[0]), Some(Reason::Pinned));
        assert!(!about.hides("notpinterest.com"));
    }

    #[test]
    fn topics_match_site_records_and_profiles_add_theirs() {
        let mut steam = SiteRecord::new("steampowered.com");
        steam.title = Some("Welcome to Steam".into());
        steam.kinds = vec!["video game".into()];
        let mut bank = SiteRecord::new("chase.com");
        bank.title = Some("Chase Bank".into());
        let topics = Topics::new(&["Games".to_owned(), "games".to_owned()]);
        assert!(topics.matches(&steam));
        assert!(!topics.matches(&bank));
        assert!(!Topics::default().matches(&steam));
        assert_eq!(topics.key(), Topics::new(&["game".to_owned()]).key());

        let dir = tempfile::tempdir().unwrap();
        let store = AboutStore::new(dir.path());
        let profile = new_profile().unwrap();
        store
            .save(&profile, &About::from_form("cooking", "", ""))
            .unwrap();
        std::fs::write(dir.path().join("x.about.json"), "{}").unwrap();
        assert_eq!(all_interests(dir.path()), ["cooking"]);
        assert!(all_interests(&dir.path().join("none")).is_empty());
    }

    #[test]
    fn words_match_across_endings() {
        let words: Vec<String> = words_of("Games gaming game programmers programming").collect();
        assert_eq!(words[0], words[1]);
        assert_eq!(words[1], words[2]);
        assert_eq!(words[3], words[4]);
        assert_eq!(stem("program"), words[4]);
    }

    #[test]
    fn profiles_are_saved_per_browser_and_deleted_when_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let store = AboutStore::new(dir.path().join("history"));
        let (a, b) = (new_profile().unwrap(), new_profile().unwrap());
        let about = About::from_form("cooking", "seriouseats.com", "");
        store.save(&a, &about).unwrap();
        assert_eq!(store.load(&a), about);
        assert!(store.load(&b).is_empty());
        store.save(&a, &About::default()).unwrap();
        assert!(store.load(&a).is_empty());
        assert!(!dir
            .path()
            .join("history")
            .join(format!("{a}.about.json"))
            .exists());
        assert!(store.save("../x", &about).is_err());
    }
}
