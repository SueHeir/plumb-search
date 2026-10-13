//! `plumb-tools dead-sites`: how many of a node's sites look dead
//! ([`plumb_node::dead`]), without changing anything.

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use plumb_core::{now_unix, SiteRecord};
use plumb_node::crawl::SECONDS_PER_DAY;
use plumb_node::dead::find_dead;
use plumb_node::records::load_records;
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
    let set = load_records(&args.data.join("records.jsonl"))?;
    let history = args.data.join("history");
    let mut kept: std::collections::HashSet<String> = plumb_node::about::all_pinned(&history)
        .into_iter()
        .chain(plumb_node::history::all_opened(&history))
        .collect();
    kept.insert(plumb_core::HOME_SITE.to_owned());
    let interests = plumb_node::about::all_interests(&history);
    let topics = plumb_node::about::Topics::new(&interests);
    let now = now_unix();
    let dead = find_dead(
        || set.iter(),
        now,
        |record: &SiteRecord| {
            record.signals.official_site || kept.contains(&record.domain) || topics.matches(record)
        },
    );
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
