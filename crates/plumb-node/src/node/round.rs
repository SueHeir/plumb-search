//! What a node keeps of each site during a crawl round: the little that
//! choosing homepages and saving crawl marks need, about a tenth of a whole
//! record. A round used to hold every record ([`plumb_core::RecordSet`]),
//! well over a gigabyte for two million sites, next to everything else a
//! node holds; the crawl results go to the records' journal as before, and
//! the index is built from the file a record at a time
//! ([`super::worker::build_from_file`]).
//!
//! The sites are read from the records file once its journal is folded in
//! ([`crate::outline`]). Changes made during the round keep each site's
//! crawl marks up to date and add the sites a crawl finds; the rest of a
//! site (its score, its name) is as it was when the round read it, which
//! is all a round reads: it picks its homepages when it starts, and the
//! next part of the round reads the file again.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use plumb_core::{canonical_domain, now_unix, SiteRecord};
use tracing::info;

use crate::about::Topics;
use crate::crawl::{CrawlSet, CrawlSite};
use crate::outline::Folded;
use crate::records::{load_records, Change, RecordStore};

use super::trim::Keep;

/// What a round keeps of one site.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RoundSite {
    domain: Box<str>,
    url: Option<Box<str>>,
    score: f32,
    /// Unix seconds, 0 for none (as for the rest).
    crawled_at: u32,
    crawl_attempted_at: u32,
    gone_at: u32,
    crawl_failures: u32,
    crawl_version: u32,
    flags: u8,
}

/// The site has key pages ([`SiteRecord::key_pages`]).
const HAS_KEY_PAGES: u8 = 1;
/// The site is about one of the node's focus topics.
const FOCUSED: u8 = 2;
/// The node keeps the site whatever its crawls say ([`Keep::keeps`]); only
/// noted for sites that looked dead ([`crate::dead::looks_dead`]) when the
/// round read them.
const KEPT: u8 = 4;
/// The site has a homepage title.
const TITLED: u8 = 8;

fn seconds(at: Option<u64>) -> u32 {
    at.map_or(0, |at| u32::try_from(at).unwrap_or(u32::MAX).max(1))
}

fn at(seconds: u32) -> Option<u64> {
    (seconds > 0).then_some(u64::from(seconds))
}

impl RoundSite {
    pub(super) fn of(record: &SiteRecord, focus: &Topics, keep: &Keep) -> RoundSite {
        let mut flags = 0;
        for (flag, set) in [
            (HAS_KEY_PAGES, !record.key_pages.is_empty()),
            (FOCUSED, focus.matches(record)),
            // Only ever asked of sites that look dead; matching topics
            // against every site would take a while.
            (
                KEPT,
                crate::dead::looks_dead(record, now_unix()) && keep.keeps(record),
            ),
            (TITLED, record.title.is_some()),
        ] {
            if set {
                flags |= flag;
            }
        }
        RoundSite {
            domain: record.domain.as_str().into(),
            url: record.url.as_deref().map(Into::into),
            score: record.link_score(),
            crawled_at: seconds(record.crawled_at),
            crawl_attempted_at: seconds(record.crawl_attempted_at),
            gone_at: seconds(record.gone_at),
            crawl_failures: record.crawl_failures,
            crawl_version: record.crawl_version,
            flags,
        }
    }

    fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    /// About one of the node's focus topics, as they were when the round
    /// read the sites.
    pub(super) fn focused(&self) -> bool {
        self.has(FOCUSED)
    }

    /// Kept whatever its crawls say ([`Keep::keeps`]), for a site that
    /// looked dead when the round read it.
    pub(super) fn kept(&self) -> bool {
        self.has(KEPT)
    }

    pub(super) fn has_key_pages(&self) -> bool {
        self.has(HAS_KEY_PAGES)
    }

    pub(super) fn titled(&self) -> bool {
        self.has(TITLED)
    }

    pub(super) fn crawl_version(&self) -> u32 {
        self.crawl_version
    }

    /// Takes in a crawl of the site merged into its record: its new crawl
    /// marks, and whether it has key pages and a title now.
    fn merge(&mut self, record: &SiteRecord) {
        if record
            .crawled_at
            .is_some_and(|at| at > u64::from(self.crawled_at))
        {
            self.crawled_at = seconds(record.crawled_at);
            self.crawl_version = record.crawl_version;
            self.gone_at = 0;
            if record.url.is_some() {
                self.url = record.url.as_deref().map(Into::into);
            }
        }
        if !record.key_pages.is_empty() {
            self.flags |= HAS_KEY_PAGES;
        }
        if record.title.is_some() {
            self.flags |= TITLED;
        }
    }
}

impl CrawlSite for RoundSite {
    fn domain(&self) -> &str {
        &self.domain
    }

    fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    fn link_score(&self) -> f32 {
        self.score
    }

    fn crawled_at(&self) -> Option<u64> {
        at(self.crawled_at)
    }

    fn crawl_attempted_at(&self) -> Option<u64> {
        at(self.crawl_attempted_at)
    }

    fn crawl_failures(&self) -> u32 {
        self.crawl_failures
    }

    fn gone_at(&self) -> Option<u64> {
        at(self.gone_at)
    }
}

/// Every site of the records, as a round keeps them, findable by domain.
#[derive(Debug, Default)]
pub(super) struct RoundSites {
    sites: Vec<RoundSite>,
    /// Positions in `sites` by a hash of the domain...
    by_hash: HashMap<u64, u32>,
    /// ...and by the domain itself for the few whose hash another domain
    /// took first.
    others: HashMap<Box<str>, u32>,
    /// The topics and kept sites the sites were read with, for sites a
    /// crawl adds.
    focus: Topics,
    keep: Keep,
}

fn hash(domain: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    domain.hash(&mut hasher);
    hasher.finish()
}

impl RoundSites {
    /// Reads the sites of the records file at `path`, folding its journal
    /// in first. `focus` marks the sites about the node's focus topics and
    /// `keep` the ones it keeps whatever their crawls say. A file only a
    /// whole set can fold in (a site on two lines) is loaded and rewritten
    /// once first.
    pub(super) fn load(path: &Path, focus: Topics, keep: Keep) -> Result<RoundSites> {
        let mut store = RecordStore::open(path);
        if store.fold()? == Folded::NeedsSet {
            info!(
                "{} holds a site more than once: reading it whole to merge them",
                path.display()
            );
            let set = load_records(path)?;
            store.compact(&set)?;
        }
        let mut sites = RoundSites {
            focus,
            keep,
            ..RoundSites::default()
        };
        let mut merged = false;
        crate::outline::for_each_record(path, |record| {
            merged |= canonical_domain(&record.domain).as_deref() != Some(record.domain.as_str())
                || sites.position(&record.domain).is_some();
            sites.add(&record);
        })?;
        if merged {
            // Only loading merges a site the file holds twice, or under
            // another form of its domain. Rewritten, it holds each once.
            info!(
                "{} holds a site more than once: reading it whole to merge them",
                path.display()
            );
            let set = load_records(path)?;
            store.compact(&set)?;
            drop(set);
            let RoundSites { focus, keep, .. } = sites;
            return Self::load(path, focus, keep);
        }
        sites.sites.shrink_to_fit();
        Ok(sites)
    }

    /// The sites of `records`, for tests.
    #[cfg(test)]
    pub(super) fn of(records: impl IntoIterator<Item = SiteRecord>) -> RoundSites {
        let mut sites = RoundSites::default();
        for record in records {
            sites.add(&record);
        }
        sites
    }

    /// Adds `record`, or takes it into the site already held.
    fn add(&mut self, record: &SiteRecord) {
        if let Some(i) = self.position(&record.domain) {
            self.sites[i].merge(record);
            return;
        }
        let site = RoundSite::of(record, &self.focus, &self.keep);
        let i = u32::try_from(self.sites.len()).expect("fewer than 4 billion sites");
        match self.by_hash.entry(hash(&site.domain)) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(i);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                self.others.insert(site.domain.clone(), i);
            }
        }
        self.sites.push(site);
    }

    fn position(&self, domain: &str) -> Option<usize> {
        if let Some(&i) = self.by_hash.get(&hash(domain)) {
            if &*self.sites[i as usize].domain == domain {
                return Some(i as usize);
            }
        }
        self.others.get(domain).map(|&i| i as usize)
    }

    pub(super) fn get(&self, domain: &str) -> Option<&RoundSite> {
        self.position(domain).map(|i| &self.sites[i])
    }

    fn get_mut(&mut self, domain: &str) -> Option<&mut RoundSite> {
        self.position(domain).map(|i| &mut self.sites[i])
    }

    pub(super) fn iter(&self) -> std::slice::Iter<'_, RoundSite> {
        self.sites.iter()
    }

    /// Every site, best link score first, ties by domain.
    pub(super) fn sorted_by_link_score(&self) -> Vec<&RoundSite> {
        let mut sorted: Vec<&RoundSite> = self.sites.iter().collect();
        sorted.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.domain.cmp(&b.domain))
        });
        sorted
    }
}

impl CrawlSet for RoundSites {
    fn marks(&self, domain: &str) -> (Option<u64>, u32) {
        self.get(domain).map_or((None, 0), |site| {
            (site.crawl_attempted_at(), site.crawl_failures)
        })
    }

    fn apply(&mut self, change: Change) {
        match change {
            Change::Merge { record }
            | Change::MergeShared { record }
            | Change::SubdomainSite { record } => {
                if let Some(domain) = canonical_domain(&record.domain) {
                    let mut record = record;
                    record.domain = domain;
                    self.add(&record);
                }
            }
            Change::RefreshShared { mut record } => {
                if let Some(domain) = canonical_domain(&record.domain) {
                    record.domain = domain;
                    if let Some(site) = self.get_mut(&record.domain) {
                        site.merge(&record);
                    }
                }
            }
            Change::Mark {
                domain,
                attempted_at,
                failures,
            } => {
                if let Some(site) = self.get_mut(&domain) {
                    site.crawl_attempted_at = seconds(attempted_at);
                    site.crawl_failures = failures;
                }
            }
            Change::Gone { domain, at } => {
                if let Some(site) = self.get_mut(&domain) {
                    site.gone_at = seconds(Some(at));
                }
            }
            Change::TakeBack { .. } => {}
        }
    }

    fn len(&self) -> usize {
        self.sites.len()
    }

    fn compact(&self, store: &mut RecordStore) -> Result<usize> {
        // The journal stays when only a whole set can fold it in; loading
        // the file replays it.
        Ok(match store.fold()? {
            Folded::Records(records) => records,
            Folded::Nothing | Folded::NeedsSet => self.sites.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use plumb_core::write_jsonl;

    use super::*;
    use crate::records::journal_path;

    fn record(domain: &str, tranco: u32) -> SiteRecord {
        let mut r = SiteRecord::new(domain);
        r.signals.tranco_rank = Some(tranco);
        r
    }

    #[test]
    fn a_round_reads_the_file_and_its_journal_and_keeps_marks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let mut bank = record("usbank.com", 5);
        bank.title = Some("U.S. Bank".into());
        bank.description = Some("A bank in Minneapolis".into());
        bank.url = Some("https://www.usbank.com/".into());
        bank.crawled_at = Some(1_000);
        bank.signals.official_site = true;
        write_jsonl(&path, &[bank, record("quiet.net", 900)]).unwrap();
        let mut found = record("Found.ORG", 50);
        found.crawled_at = Some(2_000);
        RecordStore::open(&path)
            .save(&[
                Change::Merge { record: found },
                Change::Mark {
                    domain: "quiet.net".into(),
                    attempted_at: Some(1_500),
                    failures: 3,
                },
            ])
            .unwrap();

        let topics = Topics::new(&["bank".to_string()]);
        let mut sites = RoundSites::load(&path, topics, Keep::default()).unwrap();
        assert!(!journal_path(&path).exists(), "folded in");
        assert_eq!(sites.len(), 3);
        let bank = sites.get("usbank.com").unwrap();
        assert!(bank.focused() && bank.titled());
        // Asked only of sites that look dead.
        assert!(!bank.kept());
        assert_eq!(bank.url(), Some("https://www.usbank.com/"));
        assert_eq!(bank.crawled_at(), Some(1_000));
        let mut official = record("usbank.com", 5);
        official.signals.official_site = true;
        assert_eq!(bank.link_score(), official.link_score());
        assert_eq!(sites.marks("quiet.net"), (Some(1_500), 3));
        assert_eq!(sites.get("found.org").unwrap().crawled_at(), Some(2_000));
        assert!(!sites.get("quiet.net").unwrap().kept());

        // Changes during the round.
        sites.apply(Change::Mark {
            domain: "quiet.net".into(),
            attempted_at: Some(3_000),
            failures: 4,
        });
        assert_eq!(sites.marks("quiet.net"), (Some(3_000), 4));
        let mut fresh = SiteRecord::new("new.com");
        fresh.crawled_at = Some(3_100);
        sites.apply(Change::Merge { record: fresh });
        sites.apply(Change::RefreshShared {
            record: SiteRecord::new("unheld.com"),
        });
        assert_eq!(sites.len(), 4);
        sites.apply(Change::Gone {
            domain: "quiet.net".into(),
            at: 3_200,
        });
        assert_eq!(sites.get("quiet.net").unwrap().gone_at(), Some(3_200));
        let order: Vec<&str> = sites
            .sorted_by_link_score()
            .iter()
            .map(|site| site.domain())
            .collect();
        assert_eq!(order[0], "usbank.com");
    }

    #[test]
    fn sites_that_look_dead_say_whether_the_node_keeps_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let dead = |domain: &str, official: bool| {
            let mut r = record(domain, 1_000);
            r.crawl_failures = crate::dead::DEAD_AFTER_FAILURES;
            r.crawl_attempted_at = Some(now_unix());
            r.signals.official_site = official;
            r
        };
        write_jsonl(
            &path,
            &[dead("official.org", true), dead("gone.com", false)],
        )
        .unwrap();
        let sites = RoundSites::load(&path, Topics::default(), Keep::default()).unwrap();
        assert!(sites.get("official.org").unwrap().kept());
        assert!(!sites.get("gone.com").unwrap().kept());
    }

    #[test]
    fn a_file_with_a_site_twice_is_merged_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let mut titled = record("WWW.Example.com", 9);
        titled.title = Some("Example".into());
        write_jsonl(&path, &[record("example.com", 3), titled]).unwrap();
        let sites = RoundSites::load(&path, Topics::default(), Keep::default()).unwrap();
        assert_eq!(sites.len(), 1);
        assert!(sites.get("example.com").unwrap().titled());
        assert_eq!(load_records(&path).unwrap().len(), 1);
        let lines = std::fs::read_to_string(&path).unwrap();
        assert_eq!(lines.lines().count(), 1, "rewritten once");
    }
}
