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
//! Dropping a site only takes it out of this node's index. The batches this
//! node signed and published stay in `net/batches/` for as long as they
//! always did, so the network loses none of its crawls.

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use anyhow::Result;
use plumb_core::{now_unix, RecordSet, SiteRecord};
use tracing::info;

use super::fill::FILL_UP_TO_PERCENT;
use super::store;
use super::{Inner, Step, MB};
use crate::about::Topics;
use crate::records::{journal_path, load_records, replace_records, sorted_by_link_score};
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
    /// Sites an About page always puts first, or a searcher opened.
    pub domains: HashSet<String>,
}

impl Keep {
    pub(super) fn of(inner: &Inner) -> Keep {
        let history = inner.paths.data.join("history");
        let mut domains: HashSet<String> = crate::about::all_pinned(&history)
            .into_iter()
            .chain(crate::history::all_opened(&history))
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
    let wanted = bytes.div_ceil(per_site.max(1));
    sorted_by_link_score(set)
        .into_iter()
        .skip(MIN_SITES_KEPT)
        .rev()
        .filter(|record| !keep.keeps(record))
        .take(wanted.try_into().unwrap_or(usize::MAX))
        .map(|record| record.domain.clone())
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
        inner.journal.warning(
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
    let mut set = load_records(&inner.paths.records)?;
    inner.check_stop()?;
    let per_site = sites / (set.len() as u64).max(1);
    let drops = pick_drops(&set, free, per_site, &Keep::of(inner));
    if drops.is_empty() {
        inner.journal.warning(
            "Over the storage limit, and every site left is one this node keeps: \
             lower the page sets or raise the limit",
        );
        return Ok(None);
    }
    let drop: HashSet<&str> = drops.iter().map(String::as_str).collect();
    set.retain(|record| !drop.contains(record.domain.as_str()));
    let sorted = sorted_by_link_score(&set);
    replace_records(&inner.paths.records, sorted.iter().copied())?;
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
    let built = super::worker::build(inner, &sorted)?;
    inner.recount_disk();
    Ok(Some(built))
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
}
