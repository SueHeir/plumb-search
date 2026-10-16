//! `plumb-tools dead-sites`: how many of a node's sites look dead
//! ([`plumb_node::dead`]), without changing anything.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use plumb_core::sites::Sites;
use plumb_core::{now_unix, site};
use plumb_format::Item;
use plumb_node::crawl::SECONDS_PER_DAY;
use plumb_node::dead::find_dead;
use plumb_node::records::load_sites;
use plumb_node::web::group_thousands;

#[derive(Debug, Args)]
pub struct DeadSitesArgs {
    /// The node's data directory, as given to `plumb run --data`.
    #[arg(long, value_name = "DIR")]
    pub data: PathBuf,
    /// How many of the best-known dead sites to name.
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub show: usize,
}

/// Counts the sites in a node's records that look dead,
/// as `plumb run --drop-dead-sites` would judge them, and names the best
/// known, changing nothing. It keeps what a node keeps, except the node's
/// focus topics given on its command line or in its settings.
pub fn run(args: &DeadSitesArgs) -> Result<()> {
    let set = load_sites(&args.data.join("records.jsonl"))?;
    let history = args.data.join("history");
    let mut kept: std::collections::HashSet<String> = plumb_node::about::all_pinned(&history)
        .into_iter()
        .chain(plumb_node::history::all_opened(&history))
        .collect();
    kept.insert(plumb_core::HOME_SITE.to_owned());
    let interests = plumb_node::about::all_interests(&history);
    let topics = plumb_node::about::Topics::new(&interests);
    print!(
        "{}",
        report(&set, now_unix(), args.show, |site| {
            site::signals(site).official_site
                || kept.contains(&site.id)
                || topics.matches_site(site)
        })
    );
    Ok(())
}

/// What [`run`] prints of the sites of `set` at `now`, naming the best
/// `show` of the dead ones, none that `keeps`.
fn report(set: &Sites, now: u64, show: usize, keeps: impl Fn(&Item) -> bool) -> String {
    let dead = find_dead(|| set.iter(), now, keeps);
    let failing = set
        .iter()
        .map(site::crawl_marks)
        .filter(|marks| marks.gone.is_none() && marks.failures > 0)
        .count();
    let gone = set
        .iter()
        .filter(|site| site::crawl_marks(site).gone.is_some())
        .count();
    let never_reached = dead
        .iter()
        .filter(|site| site::crawled(site).is_none())
        .count();
    let mut out = format!(
        "{} sites in the records; {} could not be reached on their last try\n",
        group_thousands(set.len() as u64),
        group_thousands(failing as u64)
    );
    out += &format!(
        "{} look dead ({} never reached by any crawl); --drop-dead-sites would take them \
         out of the index\n",
        group_thousands(dead.len() as u64),
        group_thousands(never_reached as u64)
    );
    if gone > 0 {
        out += &format!("{} were taken out already\n", group_thousands(gone as u64));
    }
    for site in dead.iter().take(show) {
        let last = site::crawled(site).map_or_else(
            || "never reached".to_owned(),
            |at| {
                format!(
                    "last reached {} days ago",
                    now.saturating_sub(at) / SECONDS_PER_DAY
                )
            },
        );
        out += &format!(
            "  {} ({} failed tries, {last})\n",
            site.id,
            site::crawl_marks(site).failures
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use plumb_core::{now_unix, RecordSet, SiteRecord};
    use plumb_node::records::load_records;

    use super::*;

    const DAY: u64 = SECONDS_PER_DAY;

    /// [`report`] as it was made of records.
    fn record_report(
        set: &RecordSet,
        now: u64,
        show: usize,
        keeps: impl Fn(&SiteRecord) -> bool,
    ) -> String {
        let dead = find_dead(|| set.iter(), now, keeps);
        let failing = set
            .iter()
            .filter(|record| record.gone_at.is_none() && record.crawl_failures > 0)
            .count();
        let gone = set.iter().filter(|record| record.gone_at.is_some()).count();
        let never_reached = dead.iter().filter(|r| r.crawled_at.is_none()).count();
        let mut out = format!(
            "{} sites in the records; {} could not be reached on their last try\n",
            group_thousands(set.len() as u64),
            group_thousands(failing as u64)
        );
        out += &format!(
            "{} look dead ({} never reached by any crawl); --drop-dead-sites would take them \
             out of the index\n",
            group_thousands(dead.len() as u64),
            group_thousands(never_reached as u64)
        );
        if gone > 0 {
            out += &format!("{} were taken out already\n", group_thousands(gone as u64));
        }
        for record in dead.iter().take(show) {
            let last = record.crawled_at.map_or_else(
                || "never reached".to_owned(),
                |at| {
                    format!(
                        "last reached {} days ago",
                        now.saturating_sub(at) / SECONDS_PER_DAY
                    )
                },
            );
            out += &format!(
                "  {} ({} failed tries, {last})\n",
                record.domain, record.crawl_failures
            );
        }
        out
    }

    #[test]
    fn dead_sites_are_counted_and_named_as_they_were_of_records() {
        let now = now_unix();
        let failing = |domain: &str, failures: u32, reached_days_ago: Option<u64>| {
            let mut record = SiteRecord::new(domain);
            record.crawl_failures = failures;
            record.crawled_at = reached_days_ago.map(|days| now - days * DAY);
            record.crawl_attempted_at = Some(now - DAY);
            record
        };
        let mut records = crate::test_sites::records();
        // Enough better sites that the worst can look dead.
        for i in 0..10_000 {
            let mut record = SiteRecord::new(format!("s{i}.com"));
            record.signals.linking_domains = 50;
            records.push(record);
        }
        let mut official = failing("official.org", 9, None);
        official.signals.official_site = true;
        let mut chess = failing("chessclub.org", 7, Some(200));
        chess.title = Some("The Chess Club".into());
        let mut kept = failing("kept.org", 8, None);
        kept.signals.linking_domains = 1;
        records.extend([
            failing("dead.org", 7, None),
            failing("quiet.org", 6, Some(90)),
            failing("lately.org", 8, Some(30)),
            failing("few.org", 5, None),
            failing("WWW.Shouting.ORG", 6, Some(61)),
            official,
            chess,
            kept,
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        crate::test_sites::write(&path, &records);
        let sites = load_sites(&path).unwrap();
        let set = load_records(&path).unwrap();
        let topics = plumb_node::about::Topics::new(&["chess".to_owned()]);
        for show in [0, 2, 20] {
            let report = report(&sites, now, show, |site| {
                site::signals(site).official_site
                    || site.id == "kept.org"
                    || topics.matches_site(site)
            });
            let expected = record_report(&set, now, show, |record| {
                record.signals.official_site
                    || record.domain == "kept.org"
                    || topics.matches(record)
            });
            assert_eq!(report, expected, "{show}");
        }
        let report = report(&sites, now, 20, |site| site::signals(site).official_site);
        assert!(
            report.ends_with(
                "  kept.org (8 failed tries, never reached)\n  \
                 chessclub.org (7 failed tries, last reached 200 days ago)\n  \
                 dead.org (7 failed tries, never reached)\n  \
                 quiet.org (6 failed tries, last reached 90 days ago)\n  \
                 shouting.org (6 failed tries, last reached 61 days ago)\n"
            ),
            "{report}"
        );
    }
}
