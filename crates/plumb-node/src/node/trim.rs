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
//!   go ([`pick_drops`]), lowest link score first, until the node is back
//!   at [`TRIM_TO_PERCENT`] of it, and the index is built without them.
//!   When the rest of the folder takes that much by itself, no site goes:
//!   dropping them could not help, and the node says so instead.
//!   Never dropped: the best [`MIN_SITES_KEPT`] sites, sites about the
//!   node's topics (its focus and the About pages' interests), sites an
//!   About page always puts first or a searcher opened from the results,
//!   official websites (Wikidata), and the network's own site.
//!
//! Choosing the sites to drop reads the records file a record at a time
//! ([`crate::outline`]): what is held of each site is its outline (where
//! its line is, its link score, its domain), and the domains of those the
//! node keeps, not their whole records. The file is then written again
//! without the sites dropped, the other lines copied as they are, and the
//! index built from it a record at a time. Only a file that holds a site
//! twice, which a whole set has to merge, is read whole to choose them; it
//! is written again with each site on one line, and the index is built
//! from it a record at a time all the same.
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
use plumb_core::{now_unix, RecordSet, SiteRecord};
use tracing::info;

use super::fill::FILL_UP_TO_PERCENT;
use super::store;
use super::{Inner, Step, MB};
use crate::about::Topics;
use crate::outline::{self, Outline};
use crate::records::{
    journal_path, load_records, replace_records_with_budget, sorted_by_link_score,
};
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

/// Whether a full node still takes in `record`, a site it does not hold:
/// one about its topics, or an official website.
pub(super) fn keeps_new_site(record: &SiteRecord, topics: &Topics) -> bool {
    record.signals.official_site || topics.matches(record)
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

    pub(super) fn keeps(&self, record: &SiteRecord) -> bool {
        record.signals.official_site
            || self.domains.contains(&record.domain)
            || self.topics.matches(record)
    }
}

/// The domains to drop from `set` to free `bytes`, at `per_site` bytes a
/// site: the lowest link scores first, never the best [`MIN_SITES_KEPT`]
/// nor what `keep` keeps.
pub(super) fn pick_drops(set: &RecordSet, bytes: u64, per_site: u64, keep: &Keep) -> Vec<String> {
    lowest(&sorted_by_link_score(set), bytes, per_site, |record| {
        keep.keeps(record)
    })
    .map(|record| record.domain.clone())
    .collect()
}

/// [`pick_drops`] of the sites `sorted` outlines, best link score first
/// ([`outline::sort`]), with `kept` the domains of those [`Keep::keeps`].
pub(super) fn pick_outline_drops(
    sorted: &[Outline],
    bytes: u64,
    per_site: u64,
    kept: &HashSet<Box<str>>,
) -> Vec<String> {
    lowest(sorted, bytes, per_site, |outline| {
        kept.contains(&outline.domain)
    })
    .map(|outline| outline.domain.to_string())
    .collect()
}

/// The sites of `sorted`, best link score first, to drop to free `bytes`
/// at `per_site` bytes a site: the lowest first, never the best
/// [`MIN_SITES_KEPT`] nor one `keeps` says to keep.
fn lowest<T>(
    sorted: &[T],
    bytes: u64,
    per_site: u64,
    keeps: impl Fn(&T) -> bool,
) -> impl Iterator<Item = &T> {
    let wanted = bytes.div_ceil(per_site.max(1));
    sorted
        .iter()
        .skip(MIN_SITES_KEPT)
        .rev()
        .filter(move |site| !keeps(site))
        .take(wanted.try_into().unwrap_or(usize::MAX))
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
/// the file again without them. Reads the file a record at a time
/// ([`drop_from_file`]), but for one only a whole set can merge, which is
/// read whole ([`drop_from_set`]) and written again with each site on one
/// line. `check` is called once the file is read, before anything is
/// dropped. Returns the domains dropped, lowest link score first; the
/// index is then built from the file ([`super::worker::build_from_file`]).
fn drop_sites(
    path: &Path,
    sites: u64,
    free: u64,
    keep: &Keep,
    budget: Option<&Arc<StorageBudget>>,
    check: impl Fn() -> Result<()>,
) -> Result<Vec<String>> {
    let Some(outlines) = outline::outline_with_budget(path, budget.cloned())? else {
        info!(
            "{} holds a site more than once: reading it whole to merge them",
            path.display()
        );
        let set = load_records(path)?;
        check()?;
        return drop_from_set(set, path, sites, free, keep, budget);
    };
    check()?;
    drop_from_file(path, outlines, sites, free, keep, budget)
}

/// [`drop_sites`] of the records file at `path`, whose sites `outlines`
/// gives ([`outline::outline`]), read a record at a time: of each site only
/// its outline is held, and the domains of those `keep` keeps. The lines of
/// the sites left are copied as the file holds them
/// ([`outline::replace_lines`]).
fn drop_from_file(
    path: &Path,
    mut outlines: Vec<Outline>,
    sites: u64,
    free: u64,
    keep: &Keep,
    budget: Option<&Arc<StorageBudget>>,
) -> Result<Vec<String>> {
    let kept = outline::domains_where(path, |record| keep.keeps(record))?;
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

/// [`drop_sites`] of `set`, the records file at `path` read whole.
fn drop_from_set(
    mut set: RecordSet,
    path: &Path,
    sites: u64,
    free: u64,
    keep: &Keep,
    budget: Option<&Arc<StorageBudget>>,
) -> Result<Vec<String>> {
    let per_site = sites / (set.len() as u64).max(1);
    let drops = pick_drops(&set, free, per_site, keep);
    if !drops.is_empty() {
        let drop: HashSet<&str> = drops.iter().map(String::as_str).collect();
        set.retain(|record| !drop.contains(record.domain.as_str()));
        replace_records_with_budget(path, sorted_by_link_score(&set), budget)?;
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
    use super::*;

    fn set_of(n: usize) -> RecordSet {
        let mut set = RecordSet::new();
        for i in 0..n {
            let mut record = SiteRecord::new(format!("site{i}.com"));
            // Lower numbers rank higher.
            record.signals.linking_domains = (n - i) as u32;
            set.upsert(record);
        }
        set
    }

    fn keep_nothing() -> Keep {
        Keep {
            topics: Topics::default(),
            domains: HashSet::new(),
        }
    }

    #[test]
    fn drops_the_lowest_ranked_sites_first_and_keeps_the_best() {
        let n = MIN_SITES_KEPT + 10;
        let set = set_of(n);
        let drops = pick_drops(&set, 3_000, 1_000, &keep_nothing());
        assert_eq!(
            drops,
            [n - 1, n - 2, n - 3]
                .map(|i| format!("site{i}.com"))
                .to_vec()
        );
        // Never below the floor, however much is asked.
        let all = pick_drops(&set, u64::MAX, 1, &keep_nothing());
        assert_eq!(all.len(), 10);
        assert!(pick_drops(&set_of(MIN_SITES_KEPT), 10_000, 1, &keep_nothing()).is_empty());
    }

    #[test]
    fn never_drops_interests_opened_or_official_sites() {
        let n = MIN_SITES_KEPT + 4;
        let mut set = set_of(n);
        let last = |k: usize| format!("site{}.com", n - 1 - k);
        set.entry(&last(0)).title = Some("Woodworking tools".into());
        set.entry(&last(1)).signals.official_site = true;
        let keep = Keep {
            topics: Topics::new(&["woodworking".to_owned()]),
            domains: HashSet::from([last(2)]),
        };
        // Only the one unprotected site of the four lowest goes first.
        assert_eq!(pick_drops(&set, 1, 1, &keep), vec![last(3)]);
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
        let mut chess = SiteRecord::new("chess.example");
        chess.title = Some("Play chess online".into());
        let mut official = SiteRecord::new("bank.example");
        official.signals.official_site = true;
        assert!(keeps_new_site(&chess, &topics));
        assert!(keeps_new_site(&official, &topics));
        assert!(!keeps_new_site(&SiteRecord::new("other.example"), &topics));
    }

    /// A records file of a few sites more than [`MIN_SITES_KEPT`], as a
    /// node writes it, and a journal still to fold into it, with sites the
    /// node keeps among the lowest-ranked: one official, one opened, one
    /// about its topic in the file and one about it in the journal. Returns
    /// what keeps them.
    fn file_to_trim(path: &Path) -> Keep {
        let n = MIN_SITES_KEPT + 20;
        let last = |k: usize| format!("site{}.com", n - 1 - k);
        let mut set = set_of(n);
        set.entry(&last(0)).signals.official_site = true;
        set.entry(&last(2)).title = Some("Woodworking tools".into());
        crate::records::replace_records(path, sorted_by_link_score(&set)).unwrap();
        let mut crawled = SiteRecord::new(last(4));
        crawled.title = Some("Woodworking club".into());
        crawled.crawled_at = Some(1_700_000_000);
        crate::records::RecordStore::open(path)
            .save(&[
                // A site below them all.
                crate::records::Change::Merge {
                    record: SiteRecord::new("unlinked.example"),
                },
                crate::records::Change::Merge { record: crawled },
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
    fn dropping_sites_a_record_at_a_time_drops_what_the_whole_set_drops() {
        let dir = tempfile::tempdir().unwrap();
        let whole = dir.path().join("whole").join("records.jsonl");
        let streamed = dir.path().join("streamed").join("records.jsonl");
        std::fs::create_dir_all(whole.parent().unwrap()).unwrap();
        std::fs::create_dir_all(streamed.parent().unwrap()).unwrap();
        let keep = file_to_trim(&whole);
        file_to_trim(&streamed);
        assert!(journal_path(&streamed).exists());
        // 1,000 bytes a site, 4,500 to free: five sites.
        let (sites, free) = (1_000 * (MIN_SITES_KEPT as u64 + 21), 4_500);

        let set = load_records(&whole).unwrap();
        let expected = drop_from_set(set, &whole, sites, free, &keep, None).unwrap();
        let budget = StorageBudget::open(streamed.parent().unwrap(), u64::MAX).unwrap();
        let drops = drop_sites(&streamed, sites, free, &keep, Some(&budget), || Ok(())).unwrap();
        assert_eq!(drops, expected);
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
        // The same file, line for line, and its journal folded in.
        assert_eq!(
            std::fs::read(&streamed).unwrap(),
            std::fs::read(&whole).unwrap()
        );
        assert!(!journal_path(&streamed).exists());
        assert!(!journal_path(&whole).exists());
        // The new file was admitted and is charged as it is.
        assert_eq!(budget.status().reserved_bytes, 0);
        assert_eq!(
            budget.status().used_bytes,
            plumb_core::storage::directory_bytes(streamed.parent().unwrap()).unwrap()
        );
    }

    #[test]
    fn a_file_holding_a_site_twice_is_trimmed_whole() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let n = MIN_SITES_KEPT + 5;
        crate::records::replace_records(&path, sorted_by_link_score(&set_of(n))).unwrap();
        // The best site again, on a line of its own.
        let mut again = SiteRecord::new("site0.com");
        again.title = Some("Site zero".into());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&again).unwrap()).unwrap();
        drop(file);
        assert_eq!(outline::outline(&path).unwrap(), None);

        let (sites, free) = (1_000 * n as u64, 2_000);
        let drops = drop_sites(&path, sites, free, &keep_nothing(), None, || Ok(())).unwrap();
        assert_eq!(
            drops,
            [format!("site{}.com", n - 1), format!("site{}.com", n - 2)]
        );
        let set = load_records(&path).unwrap();
        assert_eq!(set.len(), n - 2);
        assert_eq!(set.get("site0.com").unwrap().title, again.title);
        // Written again with each site once, those dropped left out, for
        // the index to be built from a record at a time.
        let outlines = outline::outline(&path).unwrap().expect("each site once");
        assert_eq!(outlines.len(), n - 2);
        assert!(outlines
            .iter()
            .all(|o| !drops.contains(&o.domain.to_string())));
        // Stopping once the file is read leaves it as it was.
        let before = std::fs::read(&path).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&again).unwrap()).unwrap();
        drop(file);
        let twice = std::fs::read(&path).unwrap();
        assert!(twice.len() > before.len());
        let stopped = drop_sites(&path, sites, free, &keep_nothing(), None, || {
            Err(anyhow::anyhow!("stopped"))
        });
        assert!(stopped.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), twice);
    }
}
