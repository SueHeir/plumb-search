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
//! A node run with `--drop-dead-sites` cuts each dead site's record down
//! to its ranks and crawl marks ([`plumb_core::SiteRecord::make_gone`]),
//! which leaves it out of the index and out of what the node shares, and
//! keeps it from being found again as new. It is tried again every three
//! recrawl windows, and a crawl that reaches it brings it back.

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
/// gives every site, as many times as asked.
pub fn find_dead<'a, S: CrawlSite + 'a, I: Iterator<Item = &'a S>>(
    sites: impl Fn() -> I,
    now: u64,
    keeps: impl Fn(&S) -> bool,
) -> Vec<&'a S> {
    let mut dead: Vec<(f32, &S)> = sites()
        .filter(|record| looks_dead(*record, now))
        .map(|record| (record.link_score(), record))
        .collect();
    if dead.is_empty() {
        return Vec::new();
    }
    // The link score of the last of the best: a site scoring as well is
    // kept, ties included.
    let mut scores: Vec<f32> = sites().map(|site| site.link_score()).collect();
    if scores.len() > NEVER_DEAD_BEST {
        let (_, nth, _) = scores.select_nth_unstable_by(NEVER_DEAD_BEST - 1, |a, b| b.total_cmp(a));
        let bar = *nth;
        dead.retain(|(score, _)| *score < bar);
    } else {
        dead.clear();
    }
    dead.retain(|(_, record)| !keeps(record));
    dead.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.domain().cmp(b.1.domain()))
    });
    dead.into_iter().map(|(_, record)| record).collect()
}

#[cfg(test)]
mod tests {
    use plumb_core::sites::Sites;
    use plumb_core::{site, RecordSet, SiteRecord};
    use plumb_format::Item;

    use super::*;

    const NOW: u64 = 1_800_000_000;
    const DAY: u64 = SECONDS_PER_DAY;

    fn site(domain: &str, failures: u32, reached_days_ago: Option<u64>) -> SiteRecord {
        let mut r = SiteRecord::new(domain);
        r.crawl_failures = failures;
        r.crawled_at = reached_days_ago.map(|days| NOW - days * DAY);
        r.crawl_attempted_at = Some(NOW - DAY);
        r
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
        gone.make_gone(NOW);
        assert!(!looks_dead(&gone, NOW), "already taken out");
    }

    #[test]
    fn the_best_sites_and_kept_ones_never_look_dead() {
        let mut set: RecordSet = (0..NEVER_DEAD_BEST + 3)
            .map(|i| {
                let mut r = site(&format!("s{i}.com"), 0, Some(1));
                r.signals.linking_domains = (NEVER_DEAD_BEST + 3 - i) as u32 * 10;
                r
            })
            .collect();
        let mut best = site("best.com", 7, None);
        best.signals.linking_domains = 1_000_000;
        let worst = site("worst.com", 7, None);
        let mut official = site("official.com", 7, None);
        official.signals.official_site = true;
        set.extend([best, worst, official]);
        let dead: Vec<&str> =
            find_dead(|| set.iter(), NOW, |r: &SiteRecord| r.signals.official_site)
                .iter()
                .map(|r| r.domain.as_str())
                .collect();
        assert_eq!(dead, ["worst.com"]);
        // A small node keeps all of them.
        let small: RecordSet = [site("worst.com", 7, None)].into_iter().collect();
        assert!(find_dead(|| small.iter(), NOW, |_: &SiteRecord| false).is_empty());
    }

    #[test]
    fn sites_items_look_dead_as_their_records_do() {
        let mut set: RecordSet = (0..NEVER_DEAD_BEST + 3)
            .map(|i| {
                let mut r = site(&format!("s{i}.com"), 0, Some(1));
                r.signals.linking_domains = (NEVER_DEAD_BEST + 3 - i) as u32 * 10;
                r
            })
            .collect();
        let mut gone = site("gone.com", 9, None);
        gone.make_gone(NOW - DAY);
        let mut kept = site("kept.com", 7, None);
        kept.signals.official_site = true;
        set.extend([
            site("worst.com", 7, None),
            site("quiet.com", 6, Some(90)),
            site("lately.com", 8, Some(30)),
            gone,
            kept,
        ]);
        let sites: Sites = set.iter().map(SiteRecord::to_stored_item).collect();
        let records: Vec<&str> =
            find_dead(|| set.iter(), NOW, |r: &SiteRecord| r.signals.official_site)
                .iter()
                .map(|r| r.domain.as_str())
                .collect();
        let items: Vec<&str> = find_dead(
            || sites.iter(),
            NOW,
            |site: &Item| site::signals(site).official_site,
        )
        .iter()
        .map(|site| site.id.as_str())
        .collect();
        assert_eq!(items, records);
        assert_eq!(items, ["quiet.com", "worst.com"]);
    }
}
