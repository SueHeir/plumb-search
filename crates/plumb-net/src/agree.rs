//! Agreement between crawlers: a record from the network counts only once
//! two or more different nodes crawled it and saw the same thing, so one
//! bad node cannot poison the index on its own.
//!
//! A node feeds every record it accepts from a batch
//! ([`accept_batch`](crate::batch::accept_batch)), its own crawls included,
//! to an [`Agreement`] together with the crawler that signed it. Nothing is
//! passed on to the index until it is confirmed:
//!
//! * **Homepage facts** (URL, title, description, aliases) of a site are
//!   held as one observation per crawler. When at least [`QUORUM`] distinct
//!   crawlers' latest crawls [`agree`], the newest of them is released, with
//!   only the aliases that two of them saw. Homepages change, so "the same"
//!   is a tolerant comparison of normalized text, not byte equality.
//! * **Link text and new domains** another homepage pointed to are held per
//!   target site and crawler. A site's name is released once two crawlers
//!   named it, and a piece of link text once two crawlers reported it. The
//!   count of linking sites released is the one at least two crawlers
//!   reached.
//!
//! Until then the node keeps whatever it had (its own crawl or seed data).
//!
//! Every confirmation also scores the crawlers: each one in the agreeing
//! group gets an agreement, and each other crawler whose crawl of that site
//! was close in time ([`JUDGE_WINDOW_SECS`]) but did not match gets a
//! disagreement. A crawler judged at least [`MIN_JUDGED`] times that agrees
//! less than [`MIN_AGREEMENT`] of the time is distrusted: its crawls are
//! still held and still scored, but no longer count towards a quorum. The
//! node's own crawls are never distrusted.
//!
//! The state is rebuilt from the batches held on disk at start (see
//! [`crate::store`]), so it needs no file of its own, and observations older
//! than [`WINDOW_EPOCHS`] are dropped, which also lets a distrusted crawler
//! earn its way back.

use std::collections::{HashMap, HashSet};

use libp2p::PeerId;
use plumb_core::{normalize_text, registrable_domain, LinkText, SiteRecord};
use serde::{Deserialize, Serialize};

use crate::assign::EPOCH_SECS;

/// Distinct crawlers whose crawls must agree before a record counts.
pub const QUORUM: usize = 2;

/// Observations are kept for this many epochs. Each node is assigned a
/// given site about one day in eight, so over two weeks a site in a network
/// of a few nodes very likely has two crawlers.
pub const WINDOW_EPOCHS: u64 = 14;

/// A crawl that does not match a confirmed one counts against its crawler
/// only when the two were made at most this far apart; further apart, the
/// site may simply have changed.
pub const JUDGE_WINDOW_SECS: u64 = 2 * EPOCH_SECS;

/// Times a crawler must be judged before it can be distrusted.
pub const MIN_JUDGED: u32 = 10;

/// Share of judgements a crawler must win to keep counting.
pub const MIN_AGREEMENT: f64 = 0.5;

/// Two texts match when this share of their words is shared (Jaccard).
pub const TEXT_MATCH: f64 = 0.75;

/// How often a crawler agreed with the others.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Score {
    pub agreed: u32,
    pub disagreed: u32,
}

impl Score {
    /// Agreements out of judgements, counting one of each in advance so a
    /// new crawler starts at one half.
    pub fn weight(&self) -> f64 {
        (f64::from(self.agreed) + 1.0) / (f64::from(self.agreed + self.disagreed) + 2.0)
    }

    pub fn distrusted(&self) -> bool {
        self.agreed + self.disagreed >= MIN_JUDGED
            && f64::from(self.agreed) / f64::from(self.agreed + self.disagreed) < MIN_AGREEMENT
    }
}

/// What the agreement step holds, for the status page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgreementStatus {
    /// Sites with a crawl held but no quorum yet.
    pub pending_sites: usize,
    /// Sites whose homepage facts were confirmed.
    pub confirmed_sites: usize,
    /// Crawlers that no longer count towards a quorum.
    pub distrusted_crawlers: usize,
}

/// One crawler's latest crawl of a homepage.
#[derive(Debug, Clone)]
struct Observation {
    crawler: PeerId,
    record: SiteRecord,
    crawled_at: u64,
    /// Already scored for or against its crawler.
    judged: bool,
}

/// One crawler's latest report of links to a site.
#[derive(Debug, Clone)]
struct Mention {
    crawler: PeerId,
    seen_at: u64,
    linking_domains: u32,
    texts: Vec<LinkText>,
}

#[derive(Debug)]
pub struct Agreement {
    me: PeerId,
    homepages: HashMap<String, Vec<Observation>>,
    confirmed: HashSet<String>,
    mentions: HashMap<String, Vec<Mention>>,
    scores: HashMap<PeerId, Score>,
}

impl Agreement {
    /// An empty agreement step for the node `me`.
    pub fn new(me: PeerId) -> Agreement {
        Agreement {
            me,
            homepages: HashMap::new(),
            confirmed: HashSet::new(),
            mentions: HashMap::new(),
            scores: HashMap::new(),
        }
    }

    /// Takes in records `crawler` signed (as [`crate::batch::accept_batch`]
    /// kept them) and returns the records that are now confirmed.
    /// `seen_at` is when the batch was made.
    pub fn observe(
        &mut self,
        crawler: PeerId,
        records: Vec<SiteRecord>,
        seen_at: u64,
    ) -> Vec<SiteRecord> {
        let mut out = Vec::new();
        for mut record in records {
            let texts = std::mem::take(&mut record.link_texts);
            let linking = std::mem::take(&mut record.signals.linking_domains);
            let domain = record.domain.clone();
            let crawled = record.crawled_at.is_some();
            if crawled {
                if let Some(confirmed) = self.observe_homepage(crawler, record) {
                    out.push(confirmed);
                }
            }
            // A bare name is a link to it; a crawled homepage is a link
            // only when other homepages of the batch pointed to it.
            if !crawled || !texts.is_empty() || linking > 0 {
                if let Some(confirmed) =
                    self.observe_mention(crawler, &domain, texts, linking, seen_at)
                {
                    out.push(confirmed);
                }
            }
        }
        out
    }

    /// Drops observations older than [`WINDOW_EPOCHS`] before `now`.
    pub fn prune(&mut self, now: u64) {
        let oldest = now.saturating_sub(WINDOW_EPOCHS * EPOCH_SECS);
        self.homepages.retain(|domain, held| {
            held.retain(|o| o.crawled_at >= oldest);
            if held.is_empty() {
                self.confirmed.remove(domain);
            }
            !held.is_empty()
        });
        self.mentions.retain(|_, held| {
            held.retain(|m| m.seen_at >= oldest);
            !held.is_empty()
        });
    }

    /// How often `crawler` agreed with the others.
    pub fn score(&self, crawler: &PeerId) -> Score {
        self.scores.get(crawler).copied().unwrap_or_default()
    }

    pub fn status(&self) -> AgreementStatus {
        AgreementStatus {
            pending_sites: self.homepages.len() - self.confirmed.len(),
            confirmed_sites: self.confirmed.len(),
            distrusted_crawlers: self
                .scores
                .iter()
                .filter(|(peer, score)| **peer != self.me && score.distrusted())
                .count(),
        }
    }

    fn counts(&self, crawler: &PeerId) -> bool {
        *crawler == self.me || !self.score(crawler).distrusted()
    }

    fn observe_homepage(&mut self, crawler: PeerId, record: SiteRecord) -> Option<SiteRecord> {
        let crawled_at = record.crawled_at?;
        let domain = record.domain.clone();
        let held = self.homepages.entry(domain.clone()).or_default();
        match held.iter_mut().find(|o| o.crawler == crawler) {
            Some(old) if old.crawled_at > crawled_at => return None,
            Some(old) => {
                *old = Observation {
                    crawler,
                    record,
                    crawled_at,
                    judged: false,
                }
            }
            None => held.push(Observation {
                crawler,
                record,
                crawled_at,
                judged: false,
            }),
        }
        let held = &self.homepages[&domain];
        let new = held.iter().position(|o| o.crawler == crawler)?;
        // The crawls that match this one, from crawlers that count.
        let group: Vec<usize> = (0..held.len())
            .filter(|&i| agree(&held[i].record, &held[new].record))
            .collect();
        let counting = group
            .iter()
            .filter(|&&i| self.counts(&held[i].crawler))
            .count();
        if counting < QUORUM {
            return None;
        }
        let newest = *group
            .iter()
            .max_by_key(|&&i| held[i].crawled_at)
            .expect("the group holds the new crawl");
        let mut confirmed = held[newest].record.clone();
        confirmed.aliases.clear();
        for alias in &held[newest].record.aliases {
            let seen_twice = group.iter().any(|&i| {
                i != newest
                    && held[i]
                        .record
                        .aliases
                        .iter()
                        .any(|a| text_matches(a, alias))
            });
            if seen_twice {
                confirmed.add_alias(alias);
            }
        }
        // Score whoever has not been yet.
        let near = |o: &Observation| {
            group
                .iter()
                .any(|&i| held[i].crawled_at.abs_diff(o.crawled_at) <= JUDGE_WINDOW_SECS)
        };
        let mut verdicts = Vec::new();
        for (i, o) in held.iter().enumerate() {
            if o.judged {
                continue;
            }
            if group.contains(&i) {
                verdicts.push((i, true));
            } else if near(o) {
                verdicts.push((i, false));
            }
        }
        let held = self.homepages.get_mut(&domain).expect("held");
        for (i, agreed) in verdicts {
            held[i].judged = true;
            let score = self.scores.entry(held[i].crawler).or_default();
            if agreed {
                score.agreed += 1;
            } else {
                score.disagreed += 1;
            }
        }
        self.confirmed.insert(domain);
        Some(confirmed)
    }

    fn observe_mention(
        &mut self,
        crawler: PeerId,
        domain: &str,
        texts: Vec<LinkText>,
        linking_domains: u32,
        seen_at: u64,
    ) -> Option<SiteRecord> {
        let held = self.mentions.entry(domain.to_string()).or_default();
        let mention = Mention {
            crawler,
            seen_at,
            linking_domains,
            texts,
        };
        match held.iter_mut().find(|m| m.crawler == crawler) {
            Some(old) if old.seen_at > seen_at => return None,
            Some(old) => *old = mention,
            None => held.push(mention),
        }
        let held = &self.mentions[domain];
        let counting: Vec<&Mention> = held.iter().filter(|m| self.counts(&m.crawler)).collect();
        if counting.len() < QUORUM {
            return None;
        }
        let mut site = SiteRecord::new(domain);
        // The count at least two crawlers reached.
        let mut counts: Vec<u32> = counting.iter().map(|m| m.linking_domains).collect();
        counts.sort_unstable_by(|a, b| b.cmp(a));
        site.signals.linking_domains = counts[QUORUM - 1];
        let mut done: HashSet<String> = HashSet::new();
        for (i, m) in counting.iter().enumerate() {
            for lt in &m.texts {
                let key = normalize(&lt.text).join(" ");
                if key.is_empty() || done.contains(&key) {
                    continue;
                }
                let others: Vec<&LinkText> = counting
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .filter_map(|(_, o)| {
                        o.texts.iter().find(|t| normalize(&t.text).join(" ") == key)
                    })
                    .collect();
                if others.len() + 1 >= QUORUM {
                    let linkers = others.iter().fold(lt.linkers, |bits, t| bits | t.linkers);
                    site.add_link_text_linkers(&lt.text, linkers);
                    done.insert(key);
                }
            }
        }
        Some(site)
    }
}

/// Whether two crawls of a homepage saw the same thing: the same host in
/// the URL, and titles and descriptions whose words mostly match.
pub fn agree(a: &SiteRecord, b: &SiteRecord) -> bool {
    let host = |r: &SiteRecord| r.url.as_deref().and_then(registrable_domain);
    host(a) == host(b)
        && optional_text_matches(a.title.as_deref(), b.title.as_deref())
        && optional_text_matches(a.description.as_deref(), b.description.as_deref())
}

fn optional_text_matches(a: Option<&str>, b: Option<&str>) -> bool {
    text_matches(a.unwrap_or(""), b.unwrap_or(""))
}

/// Whether two texts share at least [`TEXT_MATCH`] of their words, ignoring
/// case and punctuation.
pub fn text_matches(a: &str, b: &str) -> bool {
    let a: HashSet<String> = normalize(a).into_iter().collect();
    let b: HashSet<String> = normalize(b).into_iter().collect();
    if a.is_empty() || b.is_empty() {
        return a.is_empty() && b.is_empty();
    }
    let shared = a.intersection(&b).count() as f64;
    let all = a.union(&b).count() as f64;
    shared / all >= TEXT_MATCH
}

/// The words of `text` as the index sees them ([`normalize_text`]):
/// lowercased, punctuation dropped, `U.S.` read as `us`.
fn normalize(text: &str) -> Vec<String> {
    normalize_text(text)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000;

    fn crawl(domain: &str, title: &str, at: u64) -> SiteRecord {
        let mut r = SiteRecord::new(domain);
        r.url = Some(format!("https://www.{domain}/"));
        r.title = Some(title.to_string());
        r.description = Some("Banking, loans and mortgages.".to_string());
        r.crawled_at = Some(at);
        r
    }

    #[test]
    fn texts_match_despite_case_punctuation_and_small_changes() {
        assert!(text_matches("U.S. Bank | Home", "u.s. bank — home"));
        assert!(text_matches(
            "Welcome to Example Bank, your local bank",
            "Welcome to Example Bank - your local bank!"
        ));
        assert!(!text_matches("Example Bank", "Buy cheap pills now"));
        assert!(text_matches("", "  "));
        assert!(!text_matches("", "Example"));
    }

    #[test]
    fn one_crawl_is_held_until_a_second_crawler_agrees() {
        let (a, b, me) = (PeerId::random(), PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me);
        let out = agreement.observe(a, vec![crawl("usbank.com", "U.S. Bank", NOW)], NOW);
        assert!(out.is_empty());
        assert_eq!(agreement.status().pending_sites, 1);
        // The same crawler again does not make two.
        let out = agreement.observe(a, vec![crawl("usbank.com", "U.S. Bank", NOW + 5)], NOW + 5);
        assert!(out.is_empty());
        let out = agreement.observe(
            b,
            vec![crawl("usbank.com", "U.S. Bank!", NOW + 60)],
            NOW + 60,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title.as_deref(), Some("U.S. Bank!"));
        assert_eq!(agreement.status().confirmed_sites, 1);
        assert_eq!(agreement.score(&a).agreed, 1);
        assert_eq!(agreement.score(&b).agreed, 1);
    }

    #[test]
    fn a_lone_bad_crawl_never_counts_and_is_held_against_its_crawler() {
        let (a, b, bad, me) = (
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
        );
        let mut agreement = Agreement::new(me);
        let poisoned = crawl("usbank.com", "Free crypto giveaway", NOW);
        assert!(agreement.observe(bad, vec![poisoned], NOW).is_empty());
        assert!(agreement
            .observe(a, vec![crawl("usbank.com", "U.S. Bank", NOW + 10)], NOW)
            .is_empty());
        let out = agreement.observe(b, vec![crawl("usbank.com", "U.S. Bank", NOW + 20)], NOW);
        assert_eq!(out[0].title.as_deref(), Some("U.S. Bank"));
        assert_eq!(
            agreement.score(&bad),
            Score {
                agreed: 0,
                disagreed: 1
            }
        );
    }

    #[test]
    fn a_crawler_that_keeps_disagreeing_stops_counting() {
        let (a, b, bad, me) = (
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
        );
        let mut agreement = Agreement::new(me);
        for i in 0..MIN_JUDGED {
            let d = format!("site{i}.com");
            agreement.observe(bad, vec![crawl(&d, "Spam spam spam", NOW)], NOW);
            agreement.observe(a, vec![crawl(&d, "A real site", NOW)], NOW);
            agreement.observe(b, vec![crawl(&d, "A real site", NOW)], NOW);
        }
        assert!(agreement.score(&bad).distrusted());
        assert_eq!(agreement.status().distrusted_crawlers, 1);
        // Two crawlers, one of them distrusted, are not a quorum.
        agreement.observe(bad, vec![crawl("new.com", "New", NOW)], NOW);
        assert!(agreement
            .observe(a, vec![crawl("new.com", "New", NOW)], NOW)
            .is_empty());
        // Our own crawl always counts.
        let out = agreement.observe(me, vec![crawl("new.com", "New", NOW)], NOW);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn a_changed_homepage_is_not_held_against_an_older_crawl() {
        let (a, b, c, me) = (
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
        );
        let mut agreement = Agreement::new(me);
        agreement.observe(a, vec![crawl("shop.com", "Summer sale", NOW)], NOW);
        let later = NOW + 5 * EPOCH_SECS;
        agreement.observe(b, vec![crawl("shop.com", "Winter sale", later)], later);
        let out = agreement.observe(c, vec![crawl("shop.com", "Winter sale", later)], later);
        assert_eq!(out[0].title.as_deref(), Some("Winter sale"));
        assert_eq!(agreement.score(&a), Score::default());
    }

    #[test]
    fn only_aliases_two_crawlers_saw_are_kept() {
        let (a, b, me) = (PeerId::random(), PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me);
        let mut one = crawl("usbank.com", "U.S. Bank", NOW);
        one.add_alias("U.S. Bank");
        one.add_alias("Cheap Pills");
        let mut two = crawl("usbank.com", "U.S. Bank", NOW);
        two.add_alias("US Bank");
        two.add_alias("US Bancorp");
        agreement.observe(a, vec![one], NOW);
        let out = agreement.observe(b, vec![two], NOW);
        assert_eq!(out[0].aliases, vec!["US Bank".to_string()]);
    }

    #[test]
    fn link_text_and_new_names_need_two_crawlers() {
        let (a, b, me) = (PeerId::random(), PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me);
        let mut named = SiteRecord::new("newbank.com");
        named.add_link_text_linkers("New Bank", 0b01);
        named.add_link_text_linkers("click here to win", 0b01);
        named.signals.linking_domains = 3;
        assert!(agreement.observe(a, vec![named], NOW).is_empty());
        let mut again = SiteRecord::new("newbank.com");
        again.add_link_text_linkers("new bank", 0b10);
        again.signals.linking_domains = 1;
        let out = agreement.observe(b, vec![again], NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].domain, "newbank.com");
        assert_eq!(out[0].link_texts.len(), 1);
        assert_eq!(out[0].link_texts[0].linkers, 0b11);
        assert_eq!(out[0].signals.linking_domains, 1);
        assert!(out[0].crawled_at.is_none());
    }

    #[test]
    fn old_observations_are_dropped() {
        let (a, b, me) = (PeerId::random(), PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me);
        agreement.observe(a, vec![crawl("usbank.com", "U.S. Bank", NOW)], NOW);
        agreement.prune(NOW + (WINDOW_EPOCHS + 1) * EPOCH_SECS);
        assert_eq!(agreement.status(), AgreementStatus::default());
        let later = NOW + (WINDOW_EPOCHS + 1) * EPOCH_SECS;
        assert!(agreement
            .observe(b, vec![crawl("usbank.com", "U.S. Bank", later)], later)
            .is_empty());
    }
}
