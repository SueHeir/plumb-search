//! Sites that look dead, and `plumb dead-sites`, which counts them.
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

use anyhow::Result;
use plumb_core::{now_unix, RecordSet, SiteRecord};

use crate::cli::DeadSitesArgs;
use crate::crawl::SECONDS_PER_DAY;
use crate::records::load_records;
use crate::web::group_thousands;

/// Tries in a row that could not reach a site before it can look dead.
pub(crate) const DEAD_AFTER_FAILURES: u32 = 6;

/// Days without a crawl reaching a site before it can look dead.
pub(crate) const DEAD_AFTER_DAYS: u64 = 60;

/// The best sites by link score never look dead: for them a failure is far
/// more likely this side's (a firewall, a block on this node's address).
pub(crate) const NEVER_DEAD_BEST: usize = 10_000;

/// Whether `record`, by its own crawl marks alone, looks dead at `now`.
pub(crate) fn looks_dead(record: &SiteRecord, now: u64) -> bool {
    let quiet_since = now.saturating_sub(DEAD_AFTER_DAYS * SECONDS_PER_DAY);
    record.gone_at.is_none()
        && record.crawl_failures >= DEAD_AFTER_FAILURES
        && record.crawled_at.is_none_or(|at| at <= quiet_since)
}

/// The sites of `set` that look dead at `now`, best link score first: none
/// of the best [`NEVER_DEAD_BEST`], and none that `keeps`.
pub(crate) fn find_dead(
    set: &RecordSet,
    now: u64,
    keeps: impl Fn(&SiteRecord) -> bool,
) -> Vec<&SiteRecord> {
    let mut dead: Vec<(f32, &SiteRecord)> = set
        .iter()
        .filter(|record| looks_dead(record, now))
        .map(|record| (record.link_score(), record))
        .collect();
    if dead.is_empty() {
        return Vec::new();
    }
    // The link score of the last of the best: a site scoring as well is
    // kept, ties included.
    let mut scores: Vec<f32> = set.iter().map(SiteRecord::link_score).collect();
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
            .then_with(|| a.1.domain.cmp(&b.1.domain))
    });
    dead.into_iter().map(|(_, record)| record).collect()
}

/// `plumb dead-sites`: counts the sites in a node's records that look dead,
/// as `plumb run --drop-dead-sites` would judge them, and names the best
/// known, changing nothing. It keeps what a node keeps, except the node's
/// focus topics given on its command line or in its settings.
pub fn run(args: &DeadSitesArgs) -> Result<()> {
    let set = load_records(&args.data.join("records.jsonl"))?;
    let history = args.data.join("history");
    let mut kept: std::collections::HashSet<String> = crate::about::all_pinned(&history)
        .into_iter()
        .chain(crate::history::all_opened(&history))
        .collect();
    kept.insert(plumb_core::HOME_SITE.to_owned());
    let interests = crate::about::all_interests(&history);
    let topics = crate::about::Topics::new(&interests);
    let now = now_unix();
    let dead = find_dead(&set, now, |record| {
        record.signals.official_site || kept.contains(&record.domain) || topics.matches(record)
    });
    let failing = set
        .iter()
        .filter(|record| record.gone_at.is_none() && record.crawl_failures > 0)
        .count();
    let gone = set.iter().filter(|record| record.gone_at.is_some()).count();
    let never_reached = dead.iter().filter(|r| r.crawled_at.is_none()).count();
    println!(
        "{} sites in the records; {} could not be reached on their last try",
        group_thousands(set.len() as u64),
        group_thousands(failing as u64)
    );
    println!(
        "{} look dead ({} never reached by any crawl); --drop-dead-sites would take them \
         out of the index",
        group_thousands(dead.len() as u64),
        group_thousands(never_reached as u64)
    );
    if gone > 0 {
        println!("{} were taken out already", group_thousands(gone as u64));
    }
    for record in dead.iter().take(args.show) {
        let last = record.crawled_at.map_or_else(
            || "never reached".to_owned(),
            |at| {
                format!(
                    "last reached {} days ago",
                    now.saturating_sub(at) / SECONDS_PER_DAY
                )
            },
        );
        println!(
            "  {} ({} failed tries, {last})",
            record.domain, record.crawl_failures
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
        let dead: Vec<&str> = find_dead(&set, NOW, |r| r.signals.official_site)
            .iter()
            .map(|r| r.domain.as_str())
            .collect();
        assert_eq!(dead, ["worst.com"]);
        // A small node keeps all of them.
        let small: RecordSet = [site("worst.com", 7, None)].into_iter().collect();
        assert!(find_dead(&small, NOW, |_| false).is_empty());
    }
}
