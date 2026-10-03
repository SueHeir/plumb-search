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
//! disagreement. A crawl close in time to one of the node's own crawls is
//! scored against it straight away, confirmed or not. A crawler judged at
//! least [`MIN_JUDGED`] times that agrees less than [`MIN_AGREEMENT`] of the
//! time is distrusted: its crawls are still held and still scored, but no
//! longer count towards a quorum. The node's own crawls are never
//! distrusted.
//!
//! # One person, many keys
//!
//! Making a node key costs nothing, so counting keys alone would let one
//! person agree with themselves. Two rules make that hard:
//!
//! * **A key earns its vote by agreeing with this node.** Once the node
//!   has crawled anything itself, another crawler counts towards a quorum
//!   only after its crawls have matched the node's own crawls of
//!   [`VOUCHES_NEEDED`] sites (and it is not distrusted). Each node checks
//!   for itself, against fetches it made itself, so a crowd of fresh keys
//!   counts for nothing, and a key only gets a vote by crawling honestly
//!   first. A node that has not crawled anything cannot check anyone, and
//!   counts every crawler that is not distrusted.
//! * **Disputes are settled by fetching the site.** A quorum that would
//!   overturn what the node already has is held instead of released when
//!   another counting crawler saw something else around the same time, when
//!   the node's own crawl saw something else, or when it contradicts the
//!   last record confirmed for the site. The site goes on the node's
//!   [`rechecks`](Agreement::rechecks) list, the node fetches it itself
//!   (outside its daily assignment), and its own crawl then decides: it
//!   releases the side it matches and counts against the other side's
//!   crawlers. So a group of keys that turns on one site after earning
//!   their votes loses them, and the site is not changed in the meantime.
//!
//! The state is rebuilt from the batches held on disk at start (see
//! [`crate::store`]), so it needs no file of its own, and observations older
//! than [`WINDOW_EPOCHS`] are dropped, which also lets a distrusted crawler
//! earn its way back.

use std::collections::{BTreeSet, HashMap, HashSet};

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

/// Sites on which a crawler's crawl must have matched this node's own
/// before it counts towards a quorum.
pub const VOUCHES_NEEDED: u32 = 3;

/// Sites vouching for a crawler are remembered up to this many.
pub const MAX_VOUCHES: u32 = 16;

/// The most sites waiting to be fetched again to settle a dispute. Past
/// that, a disputed site just stays unconfirmed until it is pruned.
pub const MAX_RECHECKS: usize = 1_000;

/// How often a crawler agreed with the others.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Score {
    pub agreed: u32,
    pub disagreed: u32,
    /// Distinct sites on which a crawl of it matched one this node made
    /// itself, counted up to [`MAX_VOUCHES`].
    #[serde(default)]
    pub vouched: u32,
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
    /// Other crawlers whose crawls matched this node's own often enough
    /// to count.
    #[serde(default)]
    pub vouched_crawlers: usize,
    /// Sites held back by a dispute, waiting for this node to fetch them.
    #[serde(default)]
    pub disputed_sites: usize,
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
    /// The record last released for each confirmed site.
    confirmed: HashMap<String, SiteRecord>,
    mentions: HashMap<String, Vec<Mention>>,
    scores: HashMap<PeerId, Score>,
    /// The sites behind each crawler's [`Score::vouched`].
    vouches: HashMap<PeerId, Vec<String>>,
    /// Sites this node holds a crawl of its own for.
    own: usize,
    /// Disputed sites this node should fetch itself.
    rechecks: BTreeSet<String>,
}

impl Agreement {
    /// An empty agreement step for the node `me`.
    pub fn new(me: PeerId) -> Agreement {
        Agreement {
            me,
            homepages: HashMap::new(),
            confirmed: HashMap::new(),
            mentions: HashMap::new(),
            scores: HashMap::new(),
            vouches: HashMap::new(),
            own: 0,
            rechecks: BTreeSet::new(),
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
        let me = self.me;
        let mut own = 0;
        self.homepages.retain(|domain, held| {
            held.retain(|o| o.crawled_at >= oldest);
            if held.is_empty() {
                self.confirmed.remove(domain);
                self.rechecks.remove(domain);
            }
            own += usize::from(held.iter().any(|o| o.crawler == me));
            !held.is_empty()
        });
        self.own = own;
        self.mentions.retain(|_, held| {
            held.retain(|m| m.seen_at >= oldest);
            !held.is_empty()
        });
    }

    /// How often `crawler` agreed with the others.
    pub fn score(&self, crawler: &PeerId) -> Score {
        self.scores.get(crawler).copied().unwrap_or_default()
    }

    /// Whether `crawler`'s crawls count towards a quorum here: this node's
    /// own always do; another crawler's when it is not distrusted and, once
    /// this node has crawled anything itself, has been vouched for by
    /// matching this node's own crawls of [`VOUCHES_NEEDED`] sites.
    pub fn counts(&self, crawler: &PeerId) -> bool {
        if *crawler == self.me {
            return true;
        }
        let score = self.score(crawler);
        !score.distrusted() && (self.own == 0 || score.vouched >= VOUCHES_NEEDED)
    }

    /// Up to `limit` disputed sites this node should fetch itself to settle
    /// them. A site stays on the list until this node's crawl of it comes
    /// in (see [`Agreement::observe`]) or its crawls are pruned.
    pub fn rechecks(&self, limit: usize) -> Vec<String> {
        self.rechecks.iter().take(limit).cloned().collect()
    }

    pub fn status(&self) -> AgreementStatus {
        let others = || self.scores.iter().filter(|(peer, _)| **peer != self.me);
        AgreementStatus {
            pending_sites: self.homepages.len() - self.confirmed.len(),
            confirmed_sites: self.confirmed.len(),
            distrusted_crawlers: others().filter(|(_, score)| score.distrusted()).count(),
            vouched_crawlers: others()
                .filter(|(_, score)| !score.distrusted() && score.vouched >= VOUCHES_NEEDED)
                .count(),
            disputed_sites: self.rechecks.len(),
        }
    }

    fn observe_homepage(&mut self, crawler: PeerId, record: SiteRecord) -> Option<SiteRecord> {
        let crawled_at = record.crawled_at?;
        let domain = record.domain.clone();
        let held = self.homepages.entry(domain.clone()).or_default();
        let mut observation = Observation {
            crawler,
            record,
            crawled_at,
            judged: false,
        };
        match held.iter_mut().find(|o| o.crawler == crawler) {
            Some(old) if old.crawled_at > crawled_at => return None,
            Some(old) => {
                // Sending the same crawl again does not earn a second
                // verdict.
                observation.judged = old.judged && agree(&old.record, &observation.record);
                *old = observation;
            }
            None => {
                held.push(observation);
                if crawler == self.me {
                    self.own += 1;
                }
            }
        }
        if crawler == self.me {
            self.rechecks.remove(&domain);
        }
        self.judge_against_own(&domain);
        let held = &self.homepages[&domain];
        let new = held.iter().position(|o| o.crawler == crawler)?;
        // The crawls that match this one, from crawlers that count.
        let group: Vec<usize> = (0..held.len())
            .filter(|&i| agree(&held[i].record, &held[new].record))
            .collect();
        // A crawl that matches this node's own needs no vouching: that
        // match is the check.
        let ours = group.iter().any(|&i| held[i].crawler == self.me);
        let counting = group
            .iter()
            .filter(|&&i| {
                let crawler = &held[i].crawler;
                self.counts(crawler) || (ours && !self.score(crawler).distrusted())
            })
            .count();
        if counting < QUORUM {
            return None;
        }
        let newest = *group
            .iter()
            .max_by_key(|&&i| held[i].crawled_at)
            .expect("the group holds the new crawl");
        if let Some(settled) = self.disputed(&domain, &group, newest) {
            if !settled && self.rechecks.len() < MAX_RECHECKS {
                self.rechecks.insert(domain);
            }
            return None;
        }
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
        for (i, agreed) in verdicts {
            self.judge(&domain, i, agreed, ours);
        }
        self.confirmed.insert(domain, confirmed.clone());
        Some(confirmed)
    }

    /// Whether the quorum `group` (indexes into the crawls of `domain`,
    /// `newest` the newest of them) is disputed: `None` when it is not and
    /// may be released, `Some(false)` when the node should fetch the site
    /// to settle it, and `Some(true)` when there is nothing to fetch: the
    /// node's own crawl, newer than the group's or at most
    /// [`JUDGE_WINDOW_SECS`] older, already says otherwise, or the node does not crawl and the other side is at
    /// least as large.
    fn disputed(&self, domain: &str, group: &[usize], newest: usize) -> Option<bool> {
        let held = &self.homepages[domain];
        if group.iter().any(|&i| held[i].crawler == self.me) {
            return None;
        }
        let near = |o: &Observation| {
            group
                .iter()
                .any(|&i| held[i].crawled_at.abs_diff(o.crawled_at) <= JUDGE_WINDOW_SECS)
        };
        let rivals = held
            .iter()
            .enumerate()
            .filter(|(i, o)| {
                !group.contains(i) && (o.crawler == self.me || (self.counts(&o.crawler) && near(o)))
            })
            .count();
        if self.own == 0 {
            // A node that does not crawl cannot settle a dispute itself,
            // so the larger side wins.
            let counting = group
                .iter()
                .filter(|&&i| self.counts(&held[i].crawler))
                .count();
            return (rivals >= counting).then_some(true);
        }
        let overturns = self
            .confirmed
            .get(domain)
            .is_some_and(|last| !agree(last, &held[newest].record));
        if rivals == 0 && !overturns {
            return None;
        }
        let own = held.iter().find(|o| o.crawler == self.me);
        Some(own.is_some_and(|o| o.crawled_at + JUDGE_WINDOW_SECS >= held[newest].crawled_at))
    }

    /// Scores every crawl of `domain` not judged yet that was made close in
    /// time to this node's own crawl of it, against that crawl.
    fn judge_against_own(&mut self, domain: &str) {
        let held = &self.homepages[domain];
        let Some(own) = held.iter().find(|o| o.crawler == self.me) else {
            return;
        };
        let verdicts: Vec<(usize, bool)> = held
            .iter()
            .enumerate()
            .filter(|(_, o)| {
                o.crawler != self.me
                    && !o.judged
                    && o.crawled_at.abs_diff(own.crawled_at) <= JUDGE_WINDOW_SECS
            })
            .map(|(i, o)| (i, agree(&o.record, &own.record)))
            .collect();
        for (i, agreed) in verdicts {
            self.judge(domain, i, agreed, true);
        }
    }

    /// Scores crawl `i` of `domain` for or against its crawler; `ours` when
    /// the crawl it was compared with includes this node's own.
    fn judge(&mut self, domain: &str, i: usize, agreed: bool, ours: bool) {
        let o = &mut self.homepages.get_mut(domain).expect("held")[i];
        o.judged = true;
        let crawler = o.crawler;
        let score = self.scores.entry(crawler).or_default();
        if agreed {
            score.agreed += 1;
            if ours && crawler != self.me {
                let sites = self.vouches.entry(crawler).or_default();
                if sites.len() < MAX_VOUCHES as usize && !sites.iter().any(|d| d == domain) {
                    sites.push(domain.to_string());
                    score.vouched = sites.len() as u32;
                }
            }
        } else {
            score.disagreed += 1;
        }
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
                disagreed: 1,
                vouched: 0,
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
        // Our own crawl always counts, and so does a crawl matching it.
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

    /// Makes `peers` match this node's own crawls of enough sites to count.
    fn vouch_for(agreement: &mut Agreement, me: PeerId, peers: &[PeerId], at: u64) {
        for i in 0..VOUCHES_NEEDED {
            let d = format!("vouch{i}.com");
            agreement.observe(me, vec![crawl(&d, "Vouch", at)], at);
            for &peer in peers {
                agreement.observe(peer, vec![crawl(&d, "Vouch", at)], at);
            }
        }
    }

    #[test]
    fn fresh_keys_do_not_count_once_this_node_crawls_until_they_match_its_crawls() {
        let (x, y, me) = (PeerId::random(), PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me);
        agreement.observe(me, vec![crawl("mine.com", "Mine", NOW)], NOW);
        // Two keys of one person agreeing with each other are not enough.
        agreement.observe(x, vec![crawl("usbank.com", "Free crypto", NOW)], NOW);
        let out = agreement.observe(y, vec![crawl("usbank.com", "Free crypto", NOW)], NOW);
        assert!(out.is_empty());
        assert!(!agreement.counts(&x) && !agreement.counts(&y));
        // Once their crawls matched ours on enough sites, they count.
        vouch_for(&mut agreement, me, &[x, y], NOW);
        assert!(agreement.counts(&x) && agreement.counts(&y));
        assert_eq!(agreement.status().vouched_crawlers, 2);
        let out = agreement.observe(x, vec![crawl("shop.com", "Shop", NOW)], NOW);
        assert!(out.is_empty());
        let out = agreement.observe(y, vec![crawl("shop.com", "Shop", NOW)], NOW);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn the_same_crawl_sent_again_vouches_once() {
        let (x, me) = (PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me);
        agreement.observe(me, vec![crawl("mine.com", "Mine", NOW)], NOW);
        for i in 0..5 {
            agreement.observe(x, vec![crawl("mine.com", "Mine", NOW + i)], NOW + i);
        }
        assert_eq!(
            agreement.score(&x),
            Score {
                agreed: 1,
                disagreed: 0,
                vouched: 1
            }
        );
        assert!(!agreement.counts(&x));
    }

    #[test]
    fn a_dispute_is_held_until_this_node_fetches_the_site_itself() {
        let (a, b, x, y, me) = (
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
        );
        let mut agreement = Agreement::new(me);
        vouch_for(&mut agreement, me, &[a, b, x, y], NOW);
        agreement.observe(a, vec![crawl("usbank.com", "U.S. Bank", NOW)], NOW);
        let out = agreement.observe(b, vec![crawl("usbank.com", "U.S. Bank", NOW)], NOW);
        assert_eq!(out[0].title.as_deref(), Some("U.S. Bank"));

        // Two keys that earned their votes turn on one site a week later.
        let later = NOW + 7 * EPOCH_SECS;
        agreement.observe(x, vec![crawl("usbank.com", "Free crypto", later)], later);
        let out = agreement.observe(y, vec![crawl("usbank.com", "Free crypto", later)], later);
        assert!(out.is_empty(), "overturning a confirmed site is held");
        assert_eq!(agreement.rechecks(10), vec!["usbank.com".to_string()]);
        assert_eq!(agreement.status().disputed_sites, 1);

        // This node fetches it: its crawl decides and counts against them.
        let out = agreement.observe(
            me,
            vec![crawl("usbank.com", "U.S. Bank", later + 60)],
            later,
        );
        // It matches A's and B's crawls, so theirs stays the record.
        assert_eq!(out[0].title.as_deref(), Some("U.S. Bank"));
        assert!(agreement.rechecks(10).is_empty());
        assert_eq!(agreement.score(&x).disagreed, 1);
        assert_eq!(agreement.score(&y).disagreed, 1);
        // Their lie is not released later either.
        let out = agreement.observe(
            y,
            vec![crawl("usbank.com", "Free crypto", later + 90)],
            later,
        );
        assert!(out.is_empty());
        assert!(
            agreement.rechecks(10).is_empty(),
            "our recent crawl settles it"
        );
        // An honest crawler matching our crawl releases it again.
        let out = agreement.observe(
            a,
            vec![crawl("usbank.com", "U.S. Bank", later + 120)],
            later,
        );
        assert_eq!(out[0].title.as_deref(), Some("U.S. Bank"));
    }

    #[test]
    fn a_rival_crawl_near_in_time_holds_a_quorum_for_a_recheck() {
        let (a, x, y, me) = (
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
        );
        let mut agreement = Agreement::new(me);
        vouch_for(&mut agreement, me, &[a, x, y], NOW);
        agreement.observe(a, vec![crawl("new.com", "New Bank", NOW)], NOW);
        agreement.observe(x, vec![crawl("new.com", "Free crypto", NOW)], NOW);
        let out = agreement.observe(y, vec![crawl("new.com", "Free crypto", NOW)], NOW);
        assert!(out.is_empty());
        assert_eq!(agreement.rechecks(10), vec!["new.com".to_string()]);
        // Our crawl sides with A and releases its crawl.
        let out = agreement.observe(me, vec![crawl("new.com", "New Bank", NOW + 60)], NOW);
        assert_eq!(out[0].title.as_deref(), Some("New Bank"));
        assert_eq!(agreement.score(&x).disagreed, 1);
    }

    #[test]
    fn a_node_that_does_not_crawl_takes_the_larger_side() {
        let (a, b, x, me) = (
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
        );
        let mut agreement = Agreement::new(me);
        agreement.observe(x, vec![crawl("new.com", "Free crypto", NOW)], NOW);
        agreement.observe(a, vec![crawl("new.com", "New Bank", NOW)], NOW);
        let out = agreement.observe(b, vec![crawl("new.com", "New Bank", NOW)], NOW);
        assert_eq!(out[0].title.as_deref(), Some("New Bank"));
        assert!(agreement.rechecks(10).is_empty());
    }
}
