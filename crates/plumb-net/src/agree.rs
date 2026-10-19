//! Agreement between crawlers: a site's crawl from the network counts only
//! once two or more different nodes crawled it and saw the same thing, so
//! one bad node cannot poison the index on its own.
//!
//! A node feeds every site it accepts from a batch
//! ([`accept_batch`](crate::batch::accept_batch)), its own crawls included,
//! to an [`Agreement`] together with the crawler that signed it, each as
//! its item ([`plumb_core::site`]). Nothing is passed on to the index until
//! it is confirmed:
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
//!   last crawl confirmed for the site. The site goes on the node's
//!   [`rechecks`](Agreement::rechecks) list, the node fetches it itself
//!   (outside its daily assignment), and its own crawl then decides: it
//!   releases the side it matches and counts against the other side's
//!   crawlers. So a group of keys that turns on one site after earning
//!   their votes loses them, and the site is not changed in the meantime.
//!
//! # Trusted nodes
//!
//! While the network is a handful of nodes, waiting for two crawlers to
//! agree mostly keeps good crawls out. So a node can name nodes it trusts
//! (`NetConfig::trusted_peers`, `plumb run --trust-peer`): a crawl signed
//! by one of them is taken in at once, like the node's own, and counts
//! towards any quorum. Everyone else goes through the rules above. Each
//! new crawl taken in from a trusted node earns it credits here (see
//! [`crate::credits`]); otherwise a trusted crawler is scored like any
//! other, and its crawls vouch for no one.
//!
//! The state is rebuilt from the batches held on disk at start (see
//! [`crate::store`]), so it needs no file of its own, and observations older
//! than [`WINDOW_EPOCHS`] are dropped, which also lets a distrusted crawler
//! earn its way back.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use libp2p::PeerId;
use plumb_core::site::{self, Item};
use plumb_core::sites::SiteList;
use plumb_core::{normalize_text, registrable_domain, KeyPage, LinkText};
use serde::{Deserialize, Serialize};

use crate::assign::EPOCH_SECS;

/// Distinct crawlers whose crawls must agree before a crawl counts.
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
    pub fn distrusted(&self) -> bool {
        self.agreed + self.disagreed >= MIN_JUDGED
            && f64::from(self.agreed) / f64::from(self.agreed + self.disagreed) < MIN_AGREEMENT
    }
}

/// One crawl scored for or against its crawler, for counting credits
/// (see [`crate::credits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub crawler: PeerId,
    /// When the crawl was made.
    pub crawled_at: u64,
    /// It agreed with the confirmed crawl; otherwise it was close in time
    /// and did not match.
    pub agreed: bool,
    /// Crawlers other than this one that this node trusts strictly (see
    /// [`Agreement::vouched`]) whose crawls make up the confirmed one,
    /// this node's own included. Credits go by this, not by what counts
    /// towards a quorum, so fresh keys agreeing with each other earn
    /// nothing however lenient agreement is.
    pub witnesses: usize,
    /// A crawl taken in at once because this node trusts its crawler:
    /// it needs no witness to earn.
    pub trusted: bool,
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
    /// Nodes whose crawls this node takes in without waiting for a second
    /// crawler.
    #[serde(default)]
    pub trusted_peers: usize,
}

/// What one crawl saw of a homepage: the homepage facts agreement compares
/// ([`agree`]) and releases, and nothing else. A node rebuilds its
/// agreement state from every batch it holds when it starts, so this is
/// kept small: a whole site's item for each of hundreds of thousands of
/// crawls would take gigabytes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Facts {
    url: Option<Box<str>>,
    title: Option<Box<str>>,
    description: Option<Box<str>>,
    aliases: Box<[Box<str>]>,
}

impl Facts {
    /// The facts of `site`, leaving the rest of it.
    fn of(site: Item) -> Facts {
        let boxed = |text: Option<String>| text.map(String::into_boxed_str);
        Facts {
            url: boxed(site.url),
            title: boxed(site.title),
            description: boxed(site.about),
            aliases: site.names.into_iter().map(String::into_boxed_str).collect(),
        }
    }

    /// [`agree`] for facts.
    fn agrees(&self, other: &Facts) -> bool {
        facts_agree(
            [
                self.url.as_deref(),
                self.title.as_deref(),
                self.description.as_deref(),
            ],
            [
                other.url.as_deref(),
                other.title.as_deref(),
                other.description.as_deref(),
            ],
        )
    }

    /// The site `domain` crawled at `crawled_at` that saw these facts, as
    /// [`crate::batch::accept_batch`] keeps a crawled homepage.
    fn site(&self, domain: &str, crawled_at: u64) -> Item {
        let text = |text: &Option<Box<str>>| text.as_deref().map(str::to_string);
        let mut site = site::new(domain);
        site.url = text(&self.url);
        site.title = text(&self.title);
        site.about = text(&self.description);
        site.names = self.aliases.iter().map(|a| a.to_string()).collect();
        site::set_crawled(&mut site, Some(crawled_at));
        site
    }
}

/// What a crawl of a homepage carries besides its [`Facts`]: the page
/// text and icon, passed on with a trusted or own crawl and not held.
struct PageText {
    headings: Vec<String>,
    text: Option<String>,
    terms: Vec<String>,
    search_url: Option<String>,
    key_pages: Vec<KeyPage>,
    links_to: Vec<String>,
    icon: Option<String>,
}

impl PageText {
    /// The page text of `site`, its text taken out of it.
    fn of(site: &mut Item) -> PageText {
        PageText {
            headings: site::headings(site),
            text: site.text.take(),
            terms: site::terms(site),
            search_url: site::search_url(site).map(str::to_string),
            key_pages: site::key_pages(site),
            links_to: site::links_to(site),
            icon: site::icon(site).map(str::to_string),
        }
    }

    /// Puts the page text back on `site`.
    fn put(self, site: &mut Item) {
        site::set_headings(site, self.headings);
        site.text = self.text;
        site::set_terms(site, self.terms);
        site::set_search_url(site, self.search_url);
        site::set_key_pages(site, self.key_pages);
        site::set_links_to(site, self.links_to);
        site::set_icon(site, self.icon);
    }
}

/// A crawler, by where it is in [`Agreement::crawlers`]. Millions of
/// crawls are held, so each names its crawler in 4 bytes, not the 80 of a
/// [`PeerId`].
type Crawler = u32;

/// This node, the first crawler.
const ME: Crawler = 0;

/// What this node knows of one crawler.
#[derive(Debug)]
struct CrawlerState {
    id: PeerId,
    score: Score,
    /// The sites behind [`Score::vouched`].
    vouches: Vec<String>,
    /// Its crawls are taken in at once (see "Trusted nodes").
    trusted: bool,
}

/// One crawler's latest crawl of a homepage.
#[derive(Debug, Clone)]
struct Observation {
    crawler: Crawler,
    /// Already scored for or against its crawler.
    judged: bool,
    crawled_at: u64,
    /// Shared with the other crawls of the site that saw the same, and
    /// with its confirmed facts when it is what was released.
    facts: Arc<Facts>,
}

/// The crawls held of one homepage.
#[derive(Debug, Default)]
struct Homepage {
    crawls: Vec<Observation>,
    /// The facts last released for the site.
    confirmed: Option<Arc<Facts>>,
}

/// One crawler's latest report of links to a site.
#[derive(Debug, Clone)]
struct Mention {
    crawler: Crawler,
    linking_domains: u32,
    seen_at: u64,
    texts: Vec<LinkText>,
}

#[derive(Debug)]
pub struct Agreement {
    /// Every crawler seen or trusted, this node first.
    crawlers: Vec<CrawlerState>,
    /// Where each crawler is in `crawlers`.
    ids: HashMap<PeerId, Crawler>,
    homepages: HashMap<Box<str>, Homepage>,
    /// Homepages with confirmed facts.
    confirmed: usize,
    mentions: HashMap<Box<str>, Vec<Mention>>,
    /// Sites this node holds a crawl of its own for.
    own: usize,
    /// Disputed sites this node should fetch itself.
    rechecks: BTreeSet<String>,
    /// Scored since the last [`Agreement::take_verdicts`].
    verdicts: Vec<Verdict>,
}

impl Agreement {
    /// An empty agreement step for the node `me`, taking crawls signed by
    /// `trusted` nodes in at once.
    pub fn new(me: PeerId, trusted: impl IntoIterator<Item = PeerId>) -> Agreement {
        let mut agreement = Agreement {
            crawlers: Vec::new(),
            ids: HashMap::new(),
            homepages: HashMap::new(),
            confirmed: 0,
            mentions: HashMap::new(),
            own: 0,
            rechecks: BTreeSet::new(),
            verdicts: Vec::new(),
        };
        agreement.crawler(me);
        for peer in trusted {
            if peer != me {
                let crawler = agreement.crawler(peer);
                agreement.crawlers[crawler as usize].trusted = true;
            }
        }
        agreement
    }

    /// Takes in sites `crawler` signed (as [`crate::batch::accept_batch`]
    /// kept them) and returns the sites that are now confirmed, each read
    /// from its line as it is reached. `seen_at` is when the batch was made.
    pub fn observe(&mut self, crawler: PeerId, sites: SiteList, seen_at: u64) -> SiteList {
        let crawler = self.crawler(crawler);
        let mut out = SiteList::new();
        for mut site in sites {
            let texts = site::link_texts(&site);
            site::set_link_texts(&mut site, &[]);
            let linking = std::mem::take(&mut site.weight).map_or(0, |weight| weight as u32);
            let domain = site.id.clone();
            let crawled = site::crawled(&site);
            if let Some(crawled_at) = crawled {
                if let Some(confirmed) = self.observe_homepage(crawler, site, crawled_at) {
                    out.push(confirmed);
                }
            }
            let crawled = crawled.is_some();
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
        let (confirmed, rechecks) = (&mut self.confirmed, &mut self.rechecks);
        let mut own = 0;
        self.homepages.retain(|domain, site| {
            site.crawls.retain(|o| o.crawled_at >= oldest);
            if site.crawls.is_empty() {
                *confirmed -= usize::from(site.confirmed.is_some());
                rechecks.remove(&**domain);
                return false;
            }
            own += usize::from(site.crawls.iter().any(|o| o.crawler == ME));
            true
        });
        self.own = own;
        self.mentions.retain(|_, held| {
            held.retain(|m| m.seen_at >= oldest);
            !held.is_empty()
        });
    }

    /// How often `crawler` agreed with the others.
    pub fn score(&self, crawler: &PeerId) -> Score {
        self.ids
            .get(crawler)
            .map_or_else(Score::default, |&c| self.crawlers[c as usize].score)
    }

    /// The crawls scored since the last call, oldest first.
    pub fn take_verdicts(&mut self) -> Vec<Verdict> {
        std::mem::take(&mut self.verdicts)
    }

    /// Whether `crawler`'s crawls count towards a quorum here: this node's
    /// own always do; another crawler's when it is not distrusted and, once
    /// this node has crawled anything itself, has been vouched for by
    /// matching this node's own crawls of [`VOUCHES_NEEDED`] sites.
    pub fn counts(&self, crawler: &PeerId) -> bool {
        self.ids
            .get(crawler)
            .map_or(self.own == 0, |&c| self.counts_id(c))
    }

    fn counts_id(&self, crawler: Crawler) -> bool {
        let state = &self.crawlers[crawler as usize];
        if crawler == ME || state.trusted {
            return true;
        }
        !state.score.distrusted() && (self.own == 0 || state.score.vouched >= VOUCHES_NEEDED)
    }

    /// Whether this node trusts `crawler` strictly: it is this node, or it
    /// is not distrusted and matched this node's own crawls of
    /// [`VOUCHES_NEEDED`] sites. Unlike [`Agreement::counts`], never true
    /// for a crawler merely because this node crawls nothing itself.
    pub fn vouched(&self, crawler: &PeerId) -> bool {
        self.ids.get(crawler).is_some_and(|&c| self.vouched_id(c))
    }

    fn vouched_id(&self, crawler: Crawler) -> bool {
        let score = self.crawlers[crawler as usize].score;
        crawler == ME || (!score.distrusted() && score.vouched >= VOUCHES_NEEDED)
    }

    /// Up to `limit` disputed sites this node should fetch itself to settle
    /// them. A site stays on the list until this node's crawl of it comes
    /// in (see [`Agreement::observe`]) or its crawls are pruned.
    pub fn rechecks(&self, limit: usize) -> Vec<String> {
        self.rechecks.iter().take(limit).cloned().collect()
    }

    pub fn status(&self) -> AgreementStatus {
        let others = || self.crawlers.iter().skip(1);
        AgreementStatus {
            pending_sites: self.homepages.len() - self.confirmed,
            confirmed_sites: self.confirmed,
            distrusted_crawlers: others().filter(|c| c.score.distrusted()).count(),
            vouched_crawlers: others()
                .filter(|c| !c.score.distrusted() && c.score.vouched >= VOUCHES_NEEDED)
                .count(),
            disputed_sites: self.rechecks.len(),
            trusted_peers: others().filter(|c| c.trusted).count(),
        }
    }

    /// Where `peer` is in [`Agreement::crawlers`], added if new.
    fn crawler(&mut self, peer: PeerId) -> Crawler {
        *self.ids.entry(peer).or_insert_with(|| {
            self.crawlers.push(CrawlerState {
                id: peer,
                score: Score::default(),
                vouches: Vec::new(),
                trusted: false,
            });
            (self.crawlers.len() - 1) as Crawler
        })
    }

    /// Notes `facts` as the last released for `domain`.
    fn confirm(&mut self, domain: &str, facts: Arc<Facts>) {
        if let Some(site) = self.homepages.get_mut(domain) {
            self.confirmed += usize::from(site.confirmed.replace(facts).is_none());
        }
    }

    fn observe_homepage(
        &mut self,
        crawler: Crawler,
        mut site: Item,
        crawled_at: u64,
    ) -> Option<Item> {
        let domain = site.id.clone();
        // Only the homepage facts are compared ([`agree`]); the page text
        // and icon a trusted or own crawl carries (a few KB a site) are
        // passed on with it, not held for two weeks.
        let page = PageText::of(&mut site);
        let facts = Facts::of(site);
        let site = self.homepages.entry(domain.as_str().into()).or_default();
        // Crawlers of a homepage mostly see the same: one copy for them all.
        let facts = (site.crawls.iter().map(|o| &o.facts))
            .chain(&site.confirmed)
            .find(|known| ***known == facts)
            .cloned()
            .unwrap_or_else(|| Arc::new(facts));
        let mut observation = Observation {
            crawler,
            judged: false,
            crawled_at,
            facts,
        };
        // A crawl not seen from this crawler before.
        let mut fresh = true;
        match site.crawls.iter_mut().find(|o| o.crawler == crawler) {
            Some(old) if old.crawled_at > crawled_at => return None,
            Some(old) => {
                // Sending the same crawl again does not earn a second
                // verdict.
                fresh = old.crawled_at < crawled_at;
                observation.judged = old.judged && old.facts.agrees(&observation.facts);
                *old = observation;
            }
            None => {
                // Most sites are held from one or two crawlers: no room
                // kept for more.
                site.crawls.reserve_exact(1);
                site.crawls.push(observation);
                if crawler == ME {
                    self.own += 1;
                }
            }
        }
        if crawler == ME {
            self.rechecks.remove(&domain);
        }
        self.judge_against_own(&domain);
        if self.crawlers[crawler as usize].trusted {
            if fresh {
                self.verdicts.push(Verdict {
                    crawler: self.crawlers[crawler as usize].id,
                    crawled_at,
                    agreed: true,
                    witnesses: 0,
                    trusted: true,
                });
            }
            let held = &self.homepages[domain.as_str()].crawls;
            let facts = held.iter().find(|o| o.crawler == crawler)?.facts.clone();
            let mut site = facts.site(&domain, crawled_at);
            self.confirm(&domain, facts);
            page.put(&mut site);
            return Some(site);
        }
        let held = &self.homepages[domain.as_str()].crawls;
        let new = held.iter().position(|o| o.crawler == crawler)?;
        // The crawls that match this one, from crawlers that count.
        let group: Vec<usize> = (0..held.len())
            .filter(|&i| held[i].facts.agrees(&held[new].facts))
            .collect();
        // A crawl that matches this node's own needs no vouching: that
        // match is the check.
        let ours = group.iter().any(|&i| held[i].crawler == ME);
        let counting = group
            .iter()
            .filter(|&&i| {
                let crawler = held[i].crawler;
                self.counts_id(crawler)
                    || (ours && !self.crawlers[crawler as usize].score.distrusted())
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
        let mut confirmed = held[newest].facts.site(&domain, held[newest].crawled_at);
        confirmed.names.clear();
        for alias in &held[newest].facts.aliases {
            let seen_twice = group.iter().any(|&i| {
                i != newest && held[i].facts.aliases.iter().any(|a| text_matches(a, alias))
            });
            if seen_twice {
                site::add_sent_alias(&mut confirmed, alias);
            }
        }
        // Only compared later ([`agree`] leaves aliases out), so the crawl's
        // own facts, shared rather than copied.
        let released = held[newest].facts.clone();
        // Score whoever has not been yet.
        let near = |o: &Observation| {
            group
                .iter()
                .any(|&i| held[i].crawled_at.abs_diff(o.crawled_at) <= JUDGE_WINDOW_SECS)
        };
        // Other crawlers in the group this node trusts strictly, for credits.
        let witnesses = |i: usize| {
            group
                .iter()
                .filter(|&&j| j != i && self.vouched_id(held[j].crawler))
                .count()
        };
        let mut verdicts = Vec::new();
        for (i, o) in held.iter().enumerate() {
            if o.judged {
                continue;
            }
            if group.contains(&i) {
                verdicts.push((i, true, witnesses(i)));
            } else if near(o) {
                verdicts.push((i, false, witnesses(i)));
            }
        }
        for (i, agreed, witnesses) in verdicts {
            self.judge(&domain, i, agreed, ours, witnesses);
        }
        self.confirm(&domain, released);
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
        let site = &self.homepages[domain];
        let held = &site.crawls;
        if group
            .iter()
            .any(|&i| held[i].crawler == ME || self.crawlers[held[i].crawler as usize].trusted)
        {
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
                !group.contains(i) && (o.crawler == ME || (self.counts_id(o.crawler) && near(o)))
            })
            .count();
        if self.own == 0 {
            // A node that does not crawl cannot settle a dispute itself,
            // so the larger side wins.
            let counting = group
                .iter()
                .filter(|&&i| self.counts_id(held[i].crawler))
                .count();
            return (rivals >= counting).then_some(true);
        }
        let overturns = site
            .confirmed
            .as_ref()
            .is_some_and(|last| !last.agrees(&held[newest].facts));
        if rivals == 0 && !overturns {
            return None;
        }
        let own = held.iter().find(|o| o.crawler == ME);
        Some(own.is_some_and(|o| o.crawled_at + JUDGE_WINDOW_SECS >= held[newest].crawled_at))
    }

    /// Scores every crawl of `domain` not judged yet that was made close in
    /// time to this node's own crawl of it, against that crawl.
    fn judge_against_own(&mut self, domain: &str) {
        let held = &self.homepages[domain].crawls;
        let Some(own) = held.iter().find(|o| o.crawler == ME) else {
            return;
        };
        let verdicts: Vec<(usize, bool)> = held
            .iter()
            .enumerate()
            .filter(|(_, o)| {
                o.crawler != ME
                    && !o.judged
                    && o.crawled_at.abs_diff(own.crawled_at) <= JUDGE_WINDOW_SECS
            })
            .map(|(i, o)| (i, o.facts.agrees(&own.facts)))
            .collect();
        for (i, agreed) in verdicts {
            // Compared with this node's own crawl: one witness, this node.
            self.judge(domain, i, agreed, true, 1);
        }
    }

    /// Scores crawl `i` of `domain` for or against its crawler; `ours` when
    /// the crawl it was compared with includes this node's own, which
    /// `witnesses` other strictly trusted crawlers made.
    fn judge(&mut self, domain: &str, i: usize, agreed: bool, ours: bool, witnesses: usize) {
        let o = &mut self.homepages.get_mut(domain).expect("held").crawls[i];
        o.judged = true;
        let (crawler, crawled_at) = (o.crawler, o.crawled_at);
        let state = &mut self.crawlers[crawler as usize];
        self.verdicts.push(Verdict {
            crawler: state.id,
            crawled_at,
            agreed,
            witnesses,
            trusted: false,
        });
        if agreed {
            state.score.agreed += 1;
            let sites = &mut state.vouches;
            if ours
                && crawler != ME
                && sites.len() < MAX_VOUCHES as usize
                && !sites.iter().any(|d| d == domain)
            {
                sites.push(domain.to_string());
                state.score.vouched = sites.len() as u32;
            }
        } else {
            state.score.disagreed += 1;
        }
    }

    fn observe_mention(
        &mut self,
        crawler: Crawler,
        domain: &str,
        texts: Vec<LinkText>,
        linking_domains: u32,
        seen_at: u64,
    ) -> Option<Item> {
        let held = self.mentions.entry(domain.into()).or_default();
        let mention = Mention {
            crawler,
            linking_domains,
            seen_at,
            texts,
        };
        match held.iter_mut().find(|m| m.crawler == crawler) {
            Some(old) if old.seen_at > seen_at => return None,
            Some(old) => *old = mention,
            None => {
                held.reserve_exact(1);
                held.push(mention);
            }
        }
        let held = &self.mentions[domain];
        if self.crawlers[crawler as usize].trusted {
            let m = held.iter().find(|m| m.crawler == crawler)?;
            let mut site = site::new(domain);
            site.weight = linked_from(m.linking_domains);
            for lt in &m.texts {
                site::add_link_text_linkers(&mut site, &lt.text, lt.linkers);
            }
            return Some(site);
        }
        let counting: Vec<&Mention> = held.iter().filter(|m| self.counts_id(m.crawler)).collect();
        if counting.len() < QUORUM {
            return None;
        }
        let mut site = site::new(domain);
        // The count at least two crawlers reached.
        let mut counts: Vec<u32> = counting.iter().map(|m| m.linking_domains).collect();
        counts.sort_unstable_by(|a, b| b.cmp(a));
        site.weight = linked_from(counts[QUORUM - 1]);
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
                    site::add_link_text_linkers(&mut site, &lt.text, linkers);
                    done.insert(key);
                }
            }
        }
        Some(site)
    }
}

/// A site's `weight`, the count of sites linking to it, as
/// [`site::set_signals`] sets it: none for none.
fn linked_from(linking_domains: u32) -> Option<f64> {
    (linking_domains > 0).then(|| f64::from(linking_domains))
}

/// Whether two crawls of a homepage, each a site's item, saw the same
/// thing: the same host in the URL, and titles and descriptions whose
/// words mostly match.
pub fn agree(a: &Item, b: &Item) -> bool {
    facts_agree(
        [a.url.as_deref(), a.title.as_deref(), a.about.as_deref()],
        [b.url.as_deref(), b.title.as_deref(), b.about.as_deref()],
    )
}

/// [`agree`] on (URL, title, description).
pub(crate) fn facts_agree(a: [Option<&str>; 3], b: [Option<&str>; 3]) -> bool {
    let [a_url, a_title, a_description] = a;
    let [b_url, b_title, b_description] = b;
    a_url.and_then(registrable_domain) == b_url.and_then(registrable_domain)
        && optional_text_matches(a_title, b_title)
        && optional_text_matches(a_description, b_description)
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

    fn crawl(domain: &str, title: &str, at: u64) -> Item {
        let mut site = site::new(domain);
        site.url = Some(format!("https://www.{domain}/"));
        site.title = Some(title.to_string());
        site.about = Some("Banking, loans and mortgages.".to_string());
        site::set_crawled(&mut site, Some(at));
        site
    }

    /// [`Agreement::observe`] of `sites`, with what it confirms read back.
    fn observe(
        agreement: &mut Agreement,
        crawler: PeerId,
        sites: Vec<Item>,
        seen_at: u64,
    ) -> Vec<Item> {
        let sites = sites.into_iter().collect();
        agreement.observe(crawler, sites, seen_at).iter().collect()
    }

    #[test]
    fn fresh_keys_agreeing_with_each_other_are_no_witnesses() {
        let (a, b, me) = (PeerId::random(), PeerId::random(), PeerId::random());
        // This node crawls nothing, so every crawler counts towards a
        // quorum, but none is vouched for.
        let mut agreement = Agreement::new(me, []);
        observe(
            &mut agreement,
            a,
            vec![crawl("usbank.com", "U.S. Bank", NOW)],
            NOW,
        );
        let out = observe(
            &mut agreement,
            b,
            vec![crawl("usbank.com", "U.S. Bank", NOW + 1)],
            NOW + 1,
        );
        assert_eq!(out.len(), 1, "a quorum of two counting crawlers");
        let verdicts = agreement.take_verdicts();
        assert_eq!(verdicts.len(), 2);
        assert!(verdicts.iter().all(|v| v.agreed && v.witnesses == 0));
        assert!(!agreement.vouched(&a) && agreement.counts(&a));
        assert!(agreement.vouched(&me));
    }

    #[test]
    fn a_trusted_nodes_lone_crawl_is_taken_in_and_earns_once() {
        let (t, me) = (PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me, [t]);
        let out = observe(
            &mut agreement,
            t,
            vec![crawl("usbank.com", "U.S. Bank", NOW)],
            NOW,
        );
        assert_eq!(out.len(), 1);
        let verdicts = agreement.take_verdicts();
        assert_eq!(verdicts.len(), 1);
        assert!(verdicts[0].trusted && verdicts[0].agreed && verdicts[0].crawler == t);
        assert!(!agreement.vouched(&t), "and vouches for no one");
        // The same crawl again earns nothing more; a newer one does.
        observe(
            &mut agreement,
            t,
            vec![crawl("usbank.com", "U.S. Bank", NOW)],
            NOW,
        );
        assert!(agreement.take_verdicts().is_empty());
        observe(
            &mut agreement,
            t,
            vec![crawl("usbank.com", "U.S. Bank", NOW + 9)],
            NOW + 9,
        );
        assert_eq!(agreement.take_verdicts().len(), 1);
    }

    #[test]
    fn a_crawl_matching_our_own_has_us_as_witness() {
        let (a, me) = (PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me, []);
        observe(
            &mut agreement,
            me,
            vec![crawl("usbank.com", "U.S. Bank", NOW)],
            NOW,
        );
        observe(
            &mut agreement,
            a,
            vec![crawl("usbank.com", "U.S. Bank", NOW + 1)],
            NOW + 1,
        );
        let verdicts = agreement.take_verdicts();
        let theirs = verdicts.iter().find(|v| v.crawler == a).unwrap();
        assert!(theirs.agreed);
        assert_eq!(theirs.witnesses, 1);
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
        let mut agreement = Agreement::new(me, []);
        let out = observe(
            &mut agreement,
            a,
            vec![crawl("usbank.com", "U.S. Bank", NOW)],
            NOW,
        );
        assert!(out.is_empty());
        assert_eq!(agreement.status().pending_sites, 1);
        // The same crawler again does not make two.
        let out = observe(
            &mut agreement,
            a,
            vec![crawl("usbank.com", "U.S. Bank", NOW + 5)],
            NOW + 5,
        );
        assert!(out.is_empty());
        let out = observe(
            &mut agreement,
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
        let mut agreement = Agreement::new(me, []);
        let poisoned = crawl("usbank.com", "Free crypto giveaway", NOW);
        assert!(observe(&mut agreement, bad, vec![poisoned], NOW).is_empty());
        assert!(observe(
            &mut agreement,
            a,
            vec![crawl("usbank.com", "U.S. Bank", NOW + 10)],
            NOW
        )
        .is_empty());
        let out = observe(
            &mut agreement,
            b,
            vec![crawl("usbank.com", "U.S. Bank", NOW + 20)],
            NOW,
        );
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
        let mut agreement = Agreement::new(me, []);
        for i in 0..MIN_JUDGED {
            let d = format!("site{i}.com");
            observe(
                &mut agreement,
                bad,
                vec![crawl(&d, "Spam spam spam", NOW)],
                NOW,
            );
            observe(&mut agreement, a, vec![crawl(&d, "A real site", NOW)], NOW);
            observe(&mut agreement, b, vec![crawl(&d, "A real site", NOW)], NOW);
        }
        assert!(agreement.score(&bad).distrusted());
        assert_eq!(agreement.status().distrusted_crawlers, 1);
        // Two crawlers, one of them distrusted, are not a quorum.
        observe(&mut agreement, bad, vec![crawl("new.com", "New", NOW)], NOW);
        assert!(observe(&mut agreement, a, vec![crawl("new.com", "New", NOW)], NOW).is_empty());
        // Our own crawl always counts, and so does a crawl matching it.
        let out = observe(&mut agreement, me, vec![crawl("new.com", "New", NOW)], NOW);
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
        let mut agreement = Agreement::new(me, []);
        observe(
            &mut agreement,
            a,
            vec![crawl("shop.com", "Summer sale", NOW)],
            NOW,
        );
        let later = NOW + 5 * EPOCH_SECS;
        observe(
            &mut agreement,
            b,
            vec![crawl("shop.com", "Winter sale", later)],
            later,
        );
        let out = observe(
            &mut agreement,
            c,
            vec![crawl("shop.com", "Winter sale", later)],
            later,
        );
        assert_eq!(out[0].title.as_deref(), Some("Winter sale"));
        assert_eq!(agreement.score(&a), Score::default());
    }

    #[test]
    fn only_aliases_two_crawlers_saw_are_kept() {
        let (a, b, me) = (PeerId::random(), PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me, []);
        let mut one = crawl("usbank.com", "U.S. Bank", NOW);
        site::add_alias(&mut one, "U.S. Bank");
        site::add_alias(&mut one, "Cheap Pills");
        let mut two = crawl("usbank.com", "U.S. Bank", NOW);
        site::add_alias(&mut two, "US Bank");
        site::add_alias(&mut two, "US Bancorp");
        observe(&mut agreement, a, vec![one], NOW);
        let out = observe(&mut agreement, b, vec![two], NOW);
        assert_eq!(out[0].names, vec!["US Bank".to_string()]);
    }

    #[test]
    fn link_text_and_new_names_need_two_crawlers() {
        let (a, b, me) = (PeerId::random(), PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me, []);
        let mut named = site::new("newbank.com");
        site::add_link_text_linkers(&mut named, "New Bank", 0b01);
        site::add_link_text_linkers(&mut named, "click here to win", 0b01);
        named.weight = Some(3.0);
        assert!(observe(&mut agreement, a, vec![named], NOW).is_empty());
        let mut again = site::new("newbank.com");
        site::add_link_text_linkers(&mut again, "new bank", 0b10);
        again.weight = Some(1.0);
        let out = observe(&mut agreement, b, vec![again], NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "newbank.com");
        let texts = site::link_texts(&out[0]);
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].linkers, 0b11);
        assert_eq!(site::signals(&out[0]).linking_domains, 1);
        assert!(site::crawled(&out[0]).is_none());
    }

    #[test]
    fn old_observations_are_dropped() {
        let (a, b, me) = (PeerId::random(), PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me, []);
        observe(
            &mut agreement,
            a,
            vec![crawl("usbank.com", "U.S. Bank", NOW)],
            NOW,
        );
        agreement.prune(NOW + (WINDOW_EPOCHS + 1) * EPOCH_SECS);
        assert_eq!(agreement.status(), AgreementStatus::default());
        let later = NOW + (WINDOW_EPOCHS + 1) * EPOCH_SECS;
        assert!(observe(
            &mut agreement,
            b,
            vec![crawl("usbank.com", "U.S. Bank", later)],
            later
        )
        .is_empty());
    }

    /// Makes `peers` match this node's own crawls of enough sites to count.
    fn vouch_for(agreement: &mut Agreement, me: PeerId, peers: &[PeerId], at: u64) {
        for i in 0..VOUCHES_NEEDED {
            let d = format!("vouch{i}.com");
            observe(agreement, me, vec![crawl(&d, "Vouch", at)], at);
            for &peer in peers {
                observe(agreement, peer, vec![crawl(&d, "Vouch", at)], at);
            }
        }
    }

    #[test]
    fn fresh_keys_do_not_count_once_this_node_crawls_until_they_match_its_crawls() {
        let (x, y, me) = (PeerId::random(), PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me, []);
        observe(
            &mut agreement,
            me,
            vec![crawl("mine.com", "Mine", NOW)],
            NOW,
        );
        // Two keys of one person agreeing with each other are not enough.
        observe(
            &mut agreement,
            x,
            vec![crawl("usbank.com", "Free crypto", NOW)],
            NOW,
        );
        let out = observe(
            &mut agreement,
            y,
            vec![crawl("usbank.com", "Free crypto", NOW)],
            NOW,
        );
        assert!(out.is_empty());
        assert!(!agreement.counts(&x) && !agreement.counts(&y));
        // Once their crawls matched ours on enough sites, they count.
        vouch_for(&mut agreement, me, &[x, y], NOW);
        assert!(agreement.counts(&x) && agreement.counts(&y));
        assert_eq!(agreement.status().vouched_crawlers, 2);
        let out = observe(&mut agreement, x, vec![crawl("shop.com", "Shop", NOW)], NOW);
        assert!(out.is_empty());
        let out = observe(&mut agreement, y, vec![crawl("shop.com", "Shop", NOW)], NOW);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn the_same_crawl_sent_again_vouches_once() {
        let (x, me) = (PeerId::random(), PeerId::random());
        let mut agreement = Agreement::new(me, []);
        observe(
            &mut agreement,
            me,
            vec![crawl("mine.com", "Mine", NOW)],
            NOW,
        );
        for i in 0..5 {
            observe(
                &mut agreement,
                x,
                vec![crawl("mine.com", "Mine", NOW + i)],
                NOW + i,
            );
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
        let mut agreement = Agreement::new(me, []);
        vouch_for(&mut agreement, me, &[a, b, x, y], NOW);
        observe(
            &mut agreement,
            a,
            vec![crawl("usbank.com", "U.S. Bank", NOW)],
            NOW,
        );
        let out = observe(
            &mut agreement,
            b,
            vec![crawl("usbank.com", "U.S. Bank", NOW)],
            NOW,
        );
        assert_eq!(out[0].title.as_deref(), Some("U.S. Bank"));

        // Two keys that earned their votes turn on one site a week later.
        let later = NOW + 7 * EPOCH_SECS;
        observe(
            &mut agreement,
            x,
            vec![crawl("usbank.com", "Free crypto", later)],
            later,
        );
        let out = observe(
            &mut agreement,
            y,
            vec![crawl("usbank.com", "Free crypto", later)],
            later,
        );
        assert!(out.is_empty(), "overturning a confirmed site is held");
        assert_eq!(agreement.rechecks(10), vec!["usbank.com".to_string()]);
        assert_eq!(agreement.status().disputed_sites, 1);

        // This node fetches it: its crawl decides and counts against them.
        let out = observe(
            &mut agreement,
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
        let out = observe(
            &mut agreement,
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
        let out = observe(
            &mut agreement,
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
        let mut agreement = Agreement::new(me, []);
        vouch_for(&mut agreement, me, &[a, x, y], NOW);
        observe(
            &mut agreement,
            a,
            vec![crawl("new.com", "New Bank", NOW)],
            NOW,
        );
        observe(
            &mut agreement,
            x,
            vec![crawl("new.com", "Free crypto", NOW)],
            NOW,
        );
        let out = observe(
            &mut agreement,
            y,
            vec![crawl("new.com", "Free crypto", NOW)],
            NOW,
        );
        assert!(out.is_empty());
        assert_eq!(agreement.rechecks(10), vec!["new.com".to_string()]);
        // Our crawl sides with A and releases its crawl.
        let out = observe(
            &mut agreement,
            me,
            vec![crawl("new.com", "New Bank", NOW + 60)],
            NOW,
        );
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
        let mut agreement = Agreement::new(me, []);
        observe(
            &mut agreement,
            x,
            vec![crawl("new.com", "Free crypto", NOW)],
            NOW,
        );
        observe(
            &mut agreement,
            a,
            vec![crawl("new.com", "New Bank", NOW)],
            NOW,
        );
        let out = observe(
            &mut agreement,
            b,
            vec![crawl("new.com", "New Bank", NOW)],
            NOW,
        );
        assert_eq!(out[0].title.as_deref(), Some("New Bank"));
        assert!(agreement.rechecks(10).is_empty());
    }

    #[test]
    fn a_trusted_node_crawl_is_taken_in_at_once() {
        let (trusted, x, y, me) = (
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
            PeerId::random(),
        );
        let mut agreement = Agreement::new(me, [trusted]);
        observe(
            &mut agreement,
            me,
            vec![crawl("mine.com", "Mine", NOW)],
            NOW,
        );
        let mut one = crawl("usbank.com", "U.S. Bank", NOW);
        site::add_alias(&mut one, "US Bank");
        let out = observe(&mut agreement, trusted, vec![one], NOW);
        assert_eq!(out[0].title.as_deref(), Some("U.S. Bank"));
        assert_eq!(out[0].names, vec!["US Bank".to_string()]);
        assert_eq!(agreement.status().trusted_peers, 1);
        // Page text is passed on, though only the facts are held.
        let mut texty = crawl("library.org", "Library", NOW);
        site::set_headings(&mut texty, vec!["Find a book".into()]);
        texty.text = Some("Borrow books and films".into());
        let out = observe(&mut agreement, trusted, vec![texty], NOW);
        assert_eq!(site::headings(&out[0]), ["Find a book"]);
        assert_eq!(out[0].text.as_deref(), Some("Borrow books and films"));
        let held = &agreement.homepages["library.org"].crawls[0].facts;
        assert_eq!(held.title.as_deref(), Some("Library"));
        let mut named = site::new("newbank.com");
        site::add_link_text_linkers(&mut named, "New Bank", 0b01);
        let out = observe(&mut agreement, trusted, vec![named], NOW);
        assert_eq!(site::link_texts(&out[0]).len(), 1);
        // Anyone else still needs the full rules: two fresh keys are not
        // enough, and matching a trusted crawl vouches for no one.
        observe(
            &mut agreement,
            x,
            vec![crawl("shop.com", "Free crypto", NOW)],
            NOW,
        );
        let out = observe(
            &mut agreement,
            y,
            vec![crawl("shop.com", "Free crypto", NOW)],
            NOW,
        );
        assert!(out.is_empty());
        for i in 0..VOUCHES_NEEDED {
            let d = format!("t{i}.com");
            observe(&mut agreement, trusted, vec![crawl(&d, "T", NOW)], NOW);
            observe(&mut agreement, x, vec![crawl(&d, "T", NOW)], NOW);
        }
        assert_eq!(agreement.score(&x).vouched, 0);
        assert!(!agreement.counts(&x));
        // A trusted crawl and one other agreeing make a quorum.
        observe(&mut agreement, x, vec![crawl("cafe.com", "Cafe", NOW)], NOW);
        let out = observe(
            &mut agreement,
            trusted,
            vec![crawl("cafe.com", "Cafe", NOW)],
            NOW,
        );
        assert_eq!(out.len(), 1);
    }
}
