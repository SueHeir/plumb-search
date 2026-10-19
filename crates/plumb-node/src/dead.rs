//! Sites that look dead, which `plumb-tools dead-sites` counts.
//!
//! A crawler retries a homepage it could not reach at all (no connection,
//! no answer; see [`crate::crawl`]) after 1 day, then 2, 4, 8, 16... So
//! [`DEAD_AFTER_FAILURES`] such tries in a row span a month or more, and a
//! dropped uplink never counts: a batch that looks offline is not saved.
//! A site looks dead when it got that many and no crawl has reached it for
//! [`DEAD_AFTER_DAYS`]. Crawls trusted nodes share count too: a site
//! another node reached in that time is not dead, only out of this node's
//! reach. Never dead whatever its tries: the best [`NEVER_DEAD_BEST`]
//! sites by link score, and whatever else the caller keeps (a node keeps
//! official websites, sites about its topics, and ones its searchers chose,
//! as when trimming to a storage limit).
//!
//! A node run with `--drop-dead-sites` cuts each dead site down to its
//! ranks and crawl marks ([`plumb_core::site::make_gone`]),
//! which leaves it out of the index and out of what the node shares, and
//! keeps it from being found again as new. It is tried again every three
//! recrawl windows, and a crawl that reaches it brings it back.

use std::borrow::Borrow;

use crate::crawl::{CrawlSite, SECONDS_PER_DAY};

/// Tries in a row that could not reach a site before it can look dead.
pub(crate) const DEAD_AFTER_FAILURES: u32 = 6;

/// Days without a crawl reaching a site before it can look dead.
pub(crate) const DEAD_AFTER_DAYS: u64 = 60;

/// The best sites by link score never look dead: for them a failure is far
/// more likely this side's (a firewall, a block on this node's address).
pub(crate) const NEVER_DEAD_BEST: usize = 10_000;

/// Whether `record`, by its own crawl marks alone, looks dead at `now`.
pub(crate) fn looks_dead(record: &impl CrawlSite, now: u64) -> bool {
    let quiet_since = now.saturating_sub(DEAD_AFTER_DAYS * SECONDS_PER_DAY);
    record.gone_at().is_none()
        && record.crawl_failures() >= DEAD_AFTER_FAILURES
        && record.crawled_at().is_none_or(|at| at <= quiet_since)
}

/// The sites of `sites` that look dead at `now`, best link score first:
/// none of the best [`NEVER_DEAD_BEST`], and none that `keeps`. `sites`
/// gives every site once, by reference or owned (a site read from its
/// line, say), and only the dead ones are kept.
pub fn find_dead<S: CrawlSite, T: Borrow<S>>(
    sites: impl IntoIterator<Item = T>,
    now: u64,
    keeps: impl Fn(&S) -> bool,
) -> Vec<T> {
    let mut scores: Vec<f32> = Vec::new();
    let mut dead: Vec<(f32, T)> = Vec::new();
    for site in sites {
        let score = site.borrow().link_score();
        scores.push(score);
        if looks_dead(site.borrow(), now) {
            dead.push((score, site));
        }
    }
    if dead.is_empty() {
        return Vec::new();
    }
    // The link score of the last of the best: a site scoring as well is
    // kept, ties included.
    if scores.len() > NEVER_DEAD_BEST {
        let (_, nth, _) = scores.select_nth_unstable_by(NEVER_DEAD_BEST - 1, |a, b| b.total_cmp(a));
        let bar = *nth;
        dead.retain(|(score, _)| *score < bar);
    } else {
        dead.clear();
    }
    dead.retain(|(_, record)| !keeps(record.borrow()));
    dead.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.borrow().domain().cmp(b.1.borrow().domain()))
    });
    dead.into_iter().map(|(_, record)| record).collect()
}

#[cfg(test)]
mod tests {
    use plumb_core::site::{self, CrawlMarks};
    use plumb_core::sites::Sites;
    use plumb_format::Item;

    use super::*;

    const NOW: u64 = 1_800_000_000;
    const DAY: u64 = SECONDS_PER_DAY;

    fn site(domain: &str, failures: u32, reached_days_ago: Option<u64>) -> Item {
        let mut site = site::new(domain);
        site::set_crawled(&mut site, reached_days_ago.map(|days| NOW - days * DAY));
        let marks = CrawlMarks {
            tried: Some(NOW - DAY),
            failures,
            gone: None,
        };
        site::set_crawl_marks(&mut site, marks);
        site
    }

    /// `site` linked to by `linking` sites.
    fn linked(mut site: Item, linking: u32) -> Item {
        let mut signals = site::signals(&site);
        signals.linking_domains = linking;
        site::set_signals(&mut site, &signals);
        site
    }

    /// `site` made an official website.
    fn official(mut site: Item) -> Item {
        let mut signals = site::signals(&site);
        signals.official_site = true;
        site::set_signals(&mut site, &signals);
        site
    }

    /// The domains of the sites of `sites` that look dead at [`NOW`],
    /// keeping official websites.
    fn dead_of(sites: &Sites) -> Vec<String> {
        find_dead(sites.iter(), NOW, |site: &Item| {
            site::signals(site).official_site
        })
        .into_iter()
        .map(|site| site.id)
        .collect()
    }

    /// Sites enough that the worst of them can look dead, all reached
    /// lately.
    fn many() -> Sites {
        (0..NEVER_DEAD_BEST + 3)
            .map(|i| {
                let site = site(&format!("s{i}.com"), 0, Some(1));
                linked(site, (NEVER_DEAD_BEST + 3 - i) as u32 * 10)
            })
            .collect()
    }

    #[test]
    fn only_long_unreachable_sites_look_dead() {
        assert!(looks_dead(&site("a.com", 6, None), NOW));
        assert!(looks_dead(&site("a.com", 9, Some(61)), NOW));
        // Too few tries, or reached lately (by this node or a trusted one).
        assert!(!looks_dead(&site("a.com", 5, None), NOW));
        assert!(!looks_dead(&site("a.com", 8, Some(30)), NOW));
        assert!(!looks_dead(&site("a.com", 0, Some(400)), NOW));
        let mut gone = site("a.com", 6, None);
        site::make_gone(&mut gone, NOW);
        assert!(!looks_dead(&gone, NOW), "already taken out");
    }

    #[test]
    fn the_best_sites_and_kept_ones_never_look_dead() {
        let mut set = many();
        set.extend([
            linked(site("best.com", 7, None), 1_000_000),
            site("worst.com", 7, None),
            official(site("official.com", 7, None)),
        ]);
        assert_eq!(dead_of(&set), ["worst.com"]);
        // A small node keeps all of them.
        let small: Sites = [site("worst.com", 7, None)].into_iter().collect();
        assert!(find_dead(small.iter(), NOW, |_: &Item| false).is_empty());
    }

    #[test]
    fn sites_look_dead_by_their_crawl_marks_best_first() {
        let mut gone = site("gone.com", 9, None);
        site::make_gone(&mut gone, NOW - DAY);
        let mut set = many();
        set.extend([
            site("worst.com", 7, None),
            site("quiet.com", 6, Some(90)),
            site("lately.com", 8, Some(30)),
            gone,
            official(site("kept.com", 7, None)),
        ]);
        assert_eq!(dead_of(&set), ["quiet.com", "worst.com"]);
    }
}
