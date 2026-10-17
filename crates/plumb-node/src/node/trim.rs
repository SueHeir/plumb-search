//! Keeping a node under its storage limit.
//!
//! Filling stops at [`FILL_UP_TO_PERCENT`] of the limit, but a node keeps
//! growing past that by itself: the records other nodes publish are folded
//! in as they come, and page sets, places and vectors are added next to
//! the sites. Two things hold it back:
//!
//! * Once the node is that full, a record another node shares only
//!   refreshes a site the node already holds; a new site is taken in only
//!   when it is about one of the node's topics or an official website
//!   ([`takes_new_sites`], [`keeps_new_site`]).
//! * When the data folder is over the limit anyway, the least useful sites
//!   go ([`pick_outline_drops`]), lowest link score first, until the node
//!   is back at [`TRIM_TO_PERCENT`] of it, and the index is built without
//!   them. When the rest of the folder takes that much by itself, no site
//!   goes: dropping them could not help, and the node says so instead.
//!   Never dropped: the best [`MIN_SITES_KEPT`] sites, sites about the
//!   node's topics (its focus and the About pages' interests), sites an
//!   About page always puts first or a searcher opened from the results,
//!   official websites (Wikidata), and the network's own site.
//!
//! Choosing the sites to drop reads the records file a site at a time
//! ([`crate::outline`]): what is held of each site is its outline (where
//! its line is, its link score, its domain), and the domains of those the
//! node keeps, judged on each site's stored item ([`Keep::keeps`]), not
//! their whole records. The file is then written again
//! without the sites dropped, the other lines copied as they are, and the
//! index built from it a site at a time. A file that holds a site twice,
//! which only a whole set can merge, is first read whole once and written
//! again with each site on one line ([`outline::outline_merged`]); the
//! sites to drop are then chosen from it in the same way.
//!
//! Dropping a site only takes it out of this node's index. The batches this
//! node signed and published stay in `net/batches/` for as long as they
//! always did, so the network loses none of its crawls. Other crawlers'
//! batches there are held to a share of the limit by themselves, oldest out
//! first (see super::network::start).

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;
use plumb_core::storage::StorageBudget;
use plumb_core::{now_unix, site};
use plumb_format::Item;
use tracing::info;

use super::fill::FILL_UP_TO_PERCENT;
use super::store;
use super::{Inner, Step, MB};
use crate::about::Topics;
use crate::outline::{self, Outline};
use crate::records::journal_path;
use crate::web::group_thousands;

/// Trimming starts once the data folder is over this share of the storage
/// limit.
pub(super) const TRIM_ABOVE_PERCENT: u64 = 100;
/// And drops sites until it is back at this share, below where filling
/// stops, so crawling has room again.
pub(super) const TRIM_TO_PERCENT: u64 = FILL_UP_TO_PERCENT - 5;
/// Trimming never leaves a node with fewer sites than this.
pub(super) const MIN_SITES_KEPT: usize = 100_000;
/// Time between two looks for sites to drop: the data folder is counted
/// again only once the index without them is in service.
const TRIM_AGAIN_AFTER: u64 = 3600;
/// Sites go only once the node has been over the limit this long, so the
/// page sets and places cut to what is kept go first.
const OVER_FOR: u64 = 1800;

/// Whether the node should look for sites to drop now: it has a storage
/// limit, is over it, and has not looked in the last [`TRIM_AGAIN_AFTER`].
pub(super) fn due(inner: &Inner) -> bool {
    let limit = inner.settings().storage_limit_mb.saturating_mul(MB);
    if limit == 0 || inner.current().is_none() {
        return false;
    }
    let now = now_unix();
    let last = inner.last_trim.load(Ordering::SeqCst);
    if last > 0 && now < last.saturating_add(TRIM_AGAIN_AFTER) {
        return false;
    }
    if inner.disk_used() <= share(limit, TRIM_ABOVE_PERCENT) {
        inner.over_since.store(0, Ordering::SeqCst);
        return false;
    }
    // Page set files and places past what is kept go first, by themselves
    // (see super::pages): sites go only if that was not enough.
    let since = match inner.over_since.load(Ordering::SeqCst) {
        0 => {
            inner.over_since.store(now, Ordering::SeqCst);
            now
        }
        since => since,
    };
    now >= since.saturating_add(OVER_FOR)
}

/// When [`due`] may next say yes, while the node is over its storage
/// limit, so a node paused by the limit does not wait for the next day
/// to trim. `None` when it has no limit or is not over it.
pub(super) fn next_due(inner: &Inner) -> Option<u64> {
    if inner.settings().storage_limit_mb == 0 {
        return None;
    }
    next_due_at(
        inner.over_since.load(Ordering::SeqCst),
        inner.last_trim.load(Ordering::SeqCst),
    )
}

/// [`OVER_FOR`] after going over at `over_since` (0 when not over), and
/// no sooner than [`TRIM_AGAIN_AFTER`] after the last look at `last_trim`.
fn next_due_at(over_since: u64, last_trim: u64) -> Option<u64> {
    if over_since == 0 {
        return None;
    }
    let again = match last_trim {
        0 => 0,
        last => last.saturating_add(TRIM_AGAIN_AFTER),
    };
    Some(over_since.saturating_add(OVER_FOR).max(again))
}

/// Whether records other nodes share may add sites the node does not hold
/// yet: not once the data folder is at [`FILL_UP_TO_PERCENT`] of the limit.
pub(super) fn takes_new_sites(inner: &Inner) -> bool {
    let limit = inner.settings().storage_limit_mb.saturating_mul(MB);
    limit == 0 || inner.disk_used() < share(limit, FILL_UP_TO_PERCENT)
}

/// Whether a full node still takes in `site`, the stored item of a site
/// it does not hold: one about its topics, or an official website.
pub(super) fn keeps_new_site(site: &Item, topics: &Topics) -> bool {
    site::signals(site).official_site || topics.matches_site(site)
}

fn share(limit: u64, percent: u64) -> u64 {
    limit / 100 * percent
}

/// What the node keeps whatever the room.
#[derive(Debug, Default)]
pub(super) struct Keep {
    pub topics: Topics,
    /// Sites an About page always puts first, a searcher opened, or the
    /// node crawls as local ([`super::places::local_sites`]).
    pub domains: HashSet<String>,
}

impl Keep {
    pub(super) fn of(inner: &Inner) -> Keep {
        let history = inner.paths.data.join("history");
        let mut domains: HashSet<String> = crate::about::all_pinned(&history)
            .into_iter()
            .chain(crate::history::all_opened(&history))
            .chain(super::places::local_sites(inner))
            .collect();
        domains.insert(plumb_core::HOME_SITE.to_owned());
        Keep {
            topics: inner.keep_topics(),
            domains,
        }
    }

    /// Whether the node keeps `site`, its stored item, whatever the room:
    /// an official website, one of [`Keep::domains`], or one about its
    /// topics.
    pub(super) fn keeps(&self, site: &Item) -> bool {
        site::signals(site).official_site
            || self.domains.contains(&site.id)
            || self.topics.matches_site(site)
    }
}

/// The domains to drop of the sites `sorted` outlines, best link score
/// first ([`outline::sort`]), to free `bytes` at `per_site` bytes a site:
/// the lowest link scores first, never the best [`MIN_SITES_KEPT`] nor one
/// in `kept`, the domains of those [`Keep::keeps`].
pub(super) fn pick_outline_drops(
    sorted: &[Outline],
    bytes: u64,
    per_site: u64,
    kept: &HashSet<Box<str>>,
) -> Vec<String> {
    let wanted = bytes.div_ceil(per_site.max(1));
    sorted
        .iter()
        .skip(MIN_SITES_KEPT)
        .rev()
        .filter(|outline| !kept.contains(&outline.domain))
        .take(wanted.try_into().unwrap_or(usize::MAX))
        .map(|outline| outline.domain.to_string())
        .collect()
}

/// Drops the least useful sites to get the node back under its storage
/// limit (see the module docs) and builds an index without them. `None`
/// when no site could go.
pub(super) fn trim(inner: &Inner) -> Result<Option<super::ServingIndex>> {
    inner.last_trim.store(now_unix(), Ordering::SeqCst);
    let limit = inner.settings().storage_limit_mb.saturating_mul(MB);
    let used = inner.disk_used();
    let target = share(limit, TRIM_TO_PERCENT);
    if limit == 0 || used <= target {
        return Ok(None);
    }
    let sites = site_usage(inner);
    let Some(free) = bytes_to_free(used, sites, target) else {
        info!(
            "over the storage limit ({} MB of {} MB), but sites take only {} MB: none dropped",
            used / MB,
            limit / MB,
            sites / MB
        );
        inner.journal.warning_daily(
            "Over the storage limit, but not because of the sites: page sets, places and \
             the rest take more than the limit leaves. Lower the page sets or raise the limit",
        );
        return Ok(None);
    };
    let _records = inner.hold_records();
    inner.set_step(
        Step::Indexing,
        "Over the storage limit: choosing sites to drop",
    );
    let drops = drop_sites(
        &inner.paths.records,
        sites,
        free,
        &Keep::of(inner),
        inner.storage.as_ref(),
        || inner.check_stop(),
    )?;
    if drops.is_empty() {
        inner.journal.warning_daily(
            "Over the storage limit, and every site left is one this node keeps: \
             lower the page sets or raise the limit",
        );
        return Ok(None);
    }
    info!(
        "over the storage limit ({} MB of {} MB): dropped {} sites",
        used / MB,
        limit / MB,
        drops.len()
    );
    inner.journal.info(format!(
        "Over the storage limit: dropped the {} least-known sites, none about your \
         interests, opened from results or official",
        group_thousands(drops.len() as u64)
    ));
    inner.rest_fill();
    let built = super::worker::build_from_file(inner)?;
    inner.recount_disk();
    Ok(Some(built))
}

/// Drops sites from the records file at `path`, which take `sites` bytes,
/// to free `free` bytes (see [`trim`]), never one `keep` keeps, and writes
/// the file again without them, reading it a record at a time
/// ([`drop_from_file`]). A file only a whole set can merge is first written
/// again with each site on one line ([`outline::outline_merged`]). `check`
/// is called once the file is read, before anything is dropped. Returns
/// the domains dropped, lowest link score first; the index is then built
/// from the file ([`super::worker::build_from_file`]).
fn drop_sites(
    path: &Path,
    sites: u64,
    free: u64,
    keep: &Keep,
    budget: Option<&Arc<StorageBudget>>,
    check: impl Fn() -> Result<()>,
) -> Result<Vec<String>> {
    let outlines = outline::outline_merged(path, budget.cloned())?;
    check()?;
    drop_from_file(path, outlines, sites, free, keep, budget)
}

/// [`drop_sites`] of the records file at `path`, whose sites `outlines`
/// gives ([`outline::outline_merged`]), read a site at a time: of each
/// site only its outline is held, and the domains of those `keep` keeps.
/// The lines of the sites left are copied as the file holds them
/// ([`outline::replace_lines`]).
fn drop_from_file(
    path: &Path,
    mut outlines: Vec<Outline>,
    sites: u64,
    free: u64,
    keep: &Keep,
    budget: Option<&Arc<StorageBudget>>,
) -> Result<Vec<String>> {
    let kept = outline::domains_where(path, |site| keep.keeps(site))?;
    let per_site = sites / (outlines.len() as u64).max(1);
    outline::sort(&mut outlines);
    let drops = pick_outline_drops(&outlines, free, per_site, &kept);
    if !drops.is_empty() {
        let drop: HashSet<&str> = drops.iter().map(String::as_str).collect();
        outlines.retain(|outline| !drop.contains(&*outline.domain));
        outline::replace_lines(path, &outlines, budget)?;
    }
    Ok(drops)
}

/// The bytes to free by dropping sites, with `used` bytes in the data
/// folder, `sites` of them the sites', to get back to `target`: no more
/// than the sites take. `None` when the rest of the folder (page sets,
/// places, the model, batches...) is at `target` by itself, so dropping
/// sites cannot help.
fn bytes_to_free(used: u64, sites: u64, target: u64) -> Option<u64> {
    let other = used.saturating_sub(sites);
    (other < target).then(|| used.saturating_sub(target).min(sites))
}

/// The bytes the sites take: their records, the index being served, and
/// their vectors.
fn site_usage(inner: &Inner) -> u64 {
    let paths = &inner.paths;
    let file = |path: &std::path::Path| std::fs::metadata(path).map_or(0, |m| m.len());
    let index = inner
        .current_summary()
        .map_or(0, |(id, _)| store::dir_size(&paths.index(id)));
    file(&paths.records)
        + file(&journal_path(&paths.records))
        + index
        + file(&paths.data.join(plumb_embed::VECTORS_FILE_NAME))
}

#[cfg(test)]
mod tests {
    use plumb_core::sites::Sites;

    use super::*;
    use crate::records::{load_sites, replace_sites};

    fn set_of(n: usize) -> Sites {
        let mut set = Sites::new();
        for i in 0..n {
            let mut site = site::new(format!("site{i}.com"));
            // Lower numbers rank higher.
            let mut signals = site::signals(&site);
            signals.linking_domains = (n - i) as u32;
            site::set_signals(&mut site, &signals);
            set.upsert(site);
        }
        set
    }

    /// A records file at `path` of the sites of [`set_of`]`(n)`, as a node
    /// writes it, once `change` has changed them.
    fn file_of(path: &Path, n: usize, change: impl FnOnce(&mut Sites)) {
        let mut set = set_of(n);
        change(&mut set);
        replace_sites(path, &set.into_sorted()).unwrap();
    }

    /// The records file a whole set writes at `path` of the sites `left`
    /// holds but `drops`: best link score first, each site on one line.
    fn expected_file(path: &Path, mut left: Sites, drops: &[String]) -> Vec<u8> {
        left.retain_domains(|domain| !drops.iter().any(|drop| drop == domain));
        replace_sites(path, &left.into_sorted()).unwrap();
        std::fs::read(path).unwrap()
    }

    /// Makes `site` an official website.
    fn official(site: &mut Item) {
        let mut signals = site::signals(site);
        signals.official_site = true;
        site::set_signals(site, &signals);
    }

    fn keep_nothing() -> Keep {
        Keep {
            topics: Topics::default(),
            domains: HashSet::new(),
        }
    }

    #[test]
    fn drops_the_lowest_ranked_sites_first_and_keeps_the_best() {
        let dir = tempfile::tempdir().unwrap();
        let n = MIN_SITES_KEPT + 10;
        let file = |name: &str| {
            let path = dir.path().join(name);
            file_of(&path, n, |_| {});
            path
        };
        let sites_in = |path: &Path| outline::outline(path).unwrap().unwrap().len();
        // 1,000 bytes a site, 3,000 to free.
        let path = file("records.jsonl");
        let sites = 1_000 * n as u64;
        let drops = drop_sites(&path, sites, 3_000, &keep_nothing(), None, || Ok(())).unwrap();
        assert_eq!(
            drops,
            [n - 1, n - 2, n - 3]
                .map(|i| format!("site{i}.com"))
                .to_vec()
        );
        assert_eq!(sites_in(&path), n - 3);
        // Never below the floor, however much is asked: a byte a site, and
        // every byte to free.
        let path = file("all.jsonl");
        let all = drop_sites(&path, n as u64, u64::MAX, &keep_nothing(), None, || Ok(())).unwrap();
        assert_eq!(all.len(), 10);
        assert_eq!(sites_in(&path), MIN_SITES_KEPT);
        let sites = MIN_SITES_KEPT as u64;
        let none = drop_sites(&path, sites, 10_000, &keep_nothing(), None, || Ok(())).unwrap();
        assert!(none.is_empty());
        assert_eq!(sites_in(&path), MIN_SITES_KEPT);
    }

    #[test]
    fn never_drops_interests_opened_or_official_sites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let n = MIN_SITES_KEPT + 4;
        let last = |k: usize| format!("site{}.com", n - 1 - k);
        file_of(&path, n, |set| {
            set.update(&last(0), |site| {
                site.title = Some("Woodworking tools".into());
            });
            set.update(&last(1), official);
        });
        let keep = Keep {
            topics: Topics::new(&["woodworking".to_owned()]),
            domains: HashSet::from([last(2)]),
        };
        // Only the one unprotected site of the four lowest goes first: a
        // byte a site, and a byte to free.
        let drops = drop_sites(&path, n as u64, 1, &keep, None, || Ok(())).unwrap();
        assert_eq!(drops, vec![last(3)]);
    }

    #[test]
    fn drops_no_more_than_the_sites_can_free() {
        // 1,000 used, 600 of it sites: 200 over a target of 800.
        assert_eq!(bytes_to_free(1_000, 600, 800), Some(200));
        // Most of it is not sites: still only what is over the target.
        assert_eq!(bytes_to_free(1_000, 300, 800), Some(200));
        // The rest is at the target by itself: dropping sites cannot help.
        assert_eq!(bytes_to_free(1_000, 100, 800), None);
        assert_eq!(bytes_to_free(1_000, 0, 800), None);
        // Counted at different times, the sites may seem bigger than the
        // folder: never more than is over.
        assert_eq!(bytes_to_free(1_000, 5_000, 800), Some(200));
    }

    #[test]
    fn a_node_over_the_limit_trims_once_it_has_been_over_long_enough() {
        assert_eq!(next_due_at(0, 0), None, "not over");
        assert_eq!(next_due_at(1_000, 0), Some(1_000 + OVER_FOR));
        // Not again within the hour after the last look.
        assert_eq!(
            next_due_at(1_000, 5_000),
            Some(5_000 + TRIM_AGAIN_AFTER),
            "after a look"
        );
        assert_eq!(next_due_at(10_000, 1), Some(10_000 + OVER_FOR));
    }

    #[test]
    fn a_full_node_takes_new_sites_only_about_its_topics_or_official() {
        let topics = Topics::new(&["chess".to_owned()]);
        let mut chess = site::new("chess.example");
        chess.title = Some("Play chess online".into());
        let mut bank = site::new("bank.example");
        official(&mut bank);
        assert!(keeps_new_site(&chess, &topics));
        assert!(keeps_new_site(&bank, &topics));
        assert!(!keeps_new_site(&site::new("other.example"), &topics));
    }

    /// A records file of a few sites more than [`MIN_SITES_KEPT`], as a
    /// node writes it, and a journal still to fold into it, with sites the
    /// node keeps among the lowest-ranked: one official, one opened, one
    /// about its topic in the file and one about it in the journal. Returns
    /// what keeps them.
    fn file_to_trim(path: &Path) -> Keep {
        let n = MIN_SITES_KEPT + 20;
        let last = |k: usize| format!("site{}.com", n - 1 - k);
        file_of(path, n, |set| {
            set.update(&last(0), official);
            set.update(&last(2), |site| {
                site.title = Some("Woodworking tools".into());
            });
        });
        let mut crawled = site::new(last(4));
        crawled.title = Some("Woodworking club".into());
        site::set_crawled(&mut crawled, Some(1_700_000_000));
        crate::records::RecordStore::open(path)
            .save(&[
                // A site below them all.
                crate::records::Change::Merge {
                    site: site::new("unlinked.example"),
                },
                crate::records::Change::Merge { site: crawled },
                crate::records::Change::Gone {
                    domain: last(6),
                    at: 1_700_000_000,
                },
            ])
            .unwrap();
        Keep {
            topics: Topics::new(&["woodworking".to_owned()]),
            domains: HashSet::from([last(1)]),
        }
    }

    #[test]
    fn dropping_sites_a_site_at_a_time_drops_what_the_whole_set_drops() {
        let dir = tempfile::tempdir().unwrap();
        let streamed = dir.path().join("streamed").join("records.jsonl");
        std::fs::create_dir_all(streamed.parent().unwrap()).unwrap();
        let keep = file_to_trim(&streamed);
        assert!(journal_path(&streamed).exists());
        // The sites as loading the file gives them, its journal replayed.
        let left = load_sites(&streamed).unwrap();
        // 1,000 bytes a site, 4,500 to free: five sites.
        let (sites, free) = (1_000 * (MIN_SITES_KEPT as u64 + 21), 4_500);

        let budget = StorageBudget::open(streamed.parent().unwrap(), u64::MAX).unwrap();
        let drops = drop_sites(&streamed, sites, free, &keep, Some(&budget), || Ok(())).unwrap();
        // The lowest-ranked, but those the node keeps.
        let n = MIN_SITES_KEPT + 20;
        let last = |k: usize| format!("site{}.com", n - 1 - k);
        assert_eq!(
            drops,
            [
                "unlinked.example".to_owned(),
                last(3),
                last(5),
                last(6),
                last(7)
            ]
        );
        // The file a whole set writes of the sites left, line for line,
        // and its journal folded in.
        let expected = expected_file(&dir.path().join("expected.jsonl"), left, &drops);
        assert_eq!(std::fs::read(&streamed).unwrap(), expected);
        assert!(!journal_path(&streamed).exists());
        // The new file was admitted and is charged as it is.
        assert_eq!(budget.status().reserved_bytes, 0);
        assert_eq!(
            budget.status().used_bytes,
            plumb_core::storage::directory_bytes(streamed.parent().unwrap()).unwrap()
        );
    }

    #[test]
    fn a_file_holding_a_site_twice_is_merged_then_trimmed() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let n = MIN_SITES_KEPT + 5;
        file_of(&path, n, |_| {});
        // The best site again, on a line of its own.
        let mut again = site::new("site0.com");
        again.title = Some("Site zero".into());
        // As versions up to 0.4 wrote it: the record's own JSON.
        let append = |site: &Item| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file, "{}", crate::test_lines::old_line(site)).unwrap();
        };
        append(&again);
        assert_eq!(outline::outline(&path).unwrap(), None);
        // Loading merges the two lines.
        let left = load_sites(&path).unwrap();
        assert_eq!(left.len(), n);
        assert_eq!(left.get("site0.com").unwrap().title, again.title);

        let (sites, free) = (1_000 * n as u64, 2_000);
        let drops = drop_sites(&path, sites, free, &keep_nothing(), None, || Ok(())).unwrap();
        assert_eq!(
            drops,
            [format!("site{}.com", n - 1), format!("site{}.com", n - 2)]
        );
        // Merged first, each site on one line, then trimmed a record at a
        // time: the file a whole set writes of the sites left, which the
        // index is built from a record at a time.
        let outlines = outline::outline(&path).unwrap().expect("each site once");
        assert_eq!(outlines.len(), n - 2);
        let expected = expected_file(&dir.path().join("expected.jsonl"), left, &drops);
        assert_eq!(std::fs::read(&path).unwrap(), expected);

        // Stopping once the file is read drops no site. The file may be
        // merged by then, written again with each site on one line, which
        // drops none either.
        append(&again);
        assert_eq!(outline::outline(&path).unwrap(), None);
        let stopped = drop_sites(&path, sites, free, &keep_nothing(), None, || {
            Err(anyhow::anyhow!("stopped"))
        });
        assert!(stopped.is_err());
        assert_eq!(load_sites(&path).unwrap().len(), n - 2);
    }
}
