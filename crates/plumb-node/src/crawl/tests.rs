//! Tests of picking homepages and of crawling them in batches, with fake
//! fetchers instead of the network.

use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use plumb_core::sites::{SiteList, Sites};
use plumb_core::{RecordSet, SiteRecord};
use plumb_crawl::{CrawledPage, OutLink, PageMeta};

use super::*;
use crate::records::reference;
use crate::records::{journal_path, load_sites, replace_sites};

const DAY: u64 = SECONDS_PER_DAY;
const WINDOW: u64 = 30 * DAY;

/// A site of `domain` with a Tranco rank, last fetched at `crawled_at` and
/// last tried at `attempted_at`.
fn site_with(
    domain: &str,
    tranco: u32,
    crawled_at: Option<u64>,
    attempted_at: Option<u64>,
) -> Item {
    let mut s = site::new(domain);
    let mut signals = site::signals(&s);
    signals.tranco_rank = Some(tranco);
    site::set_signals(&mut s, &signals);
    site::set_crawled(&mut s, crawled_at);
    set_marks(&mut s, attempted_at, 0);
    s
}

/// Sets when `s`'s homepage was last tried and how many tries in a row
/// failed to reach it.
fn set_marks(s: &mut Item, attempted_at: Option<u64>, failures: u32) {
    let marks = site::CrawlMarks {
        tried: attempted_at,
        failures,
        ..site::crawl_marks(s)
    };
    site::set_crawl_marks(s, marks);
}

/// Sites `site00.com`, `site01.com`... never tried, best first.
fn sites(n: u32) -> Vec<Item> {
    (0..n)
        .map(|i| site_with(&format!("site{i:02}.com"), i + 1, None, None))
        .collect()
}

fn domains(targets: &[CrawlTarget]) -> Vec<&str> {
    targets.iter().map(|t| t.domain.as_str()).collect()
}

/// A fetched homepage of `domain` linking to the front page of each of `links_to`.
fn fetched(domain: &str, fetched_at: u64, links_to: &[&str]) -> CrawlResult {
    let links = links_to
        .iter()
        .map(|target| OutLink {
            url: format!("https://{target}/"),
            target_domain: target.to_string(),
            text: "linked site".to_string(),
        })
        .collect();
    CrawlResult {
        domain: domain.to_string(),
        outcome: CrawlOutcome::Fetched(CrawledPage {
            domain: domain.to_string(),
            final_url: format!("https://{domain}/"),
            status: 200,
            fetched_at,
            meta: PageMeta {
                title: Some(format!("{domain} home")),
                links,
                ..PageMeta::default()
            },
            icon: None,
        }),
    }
}

fn failed(domain: &str) -> CrawlResult {
    CrawlResult {
        domain: domain.to_string(),
        outcome: CrawlOutcome::Failed {
            error: "connection refused".into(),
            network: true,
        },
    }
}

/// A failure that is the site's answer, not a lack of one.
fn failed_there(domain: &str) -> CrawlResult {
    CrawlResult {
        domain: domain.to_string(),
        outcome: CrawlOutcome::Failed {
            error: "robots.txt: HTTP 503".into(),
            network: false,
        },
    }
}

fn refused(domain: &str) -> CrawlResult {
    CrawlResult {
        domain: domain.to_string(),
        outcome: CrawlOutcome::RobotsDisallowed,
    }
}

/// Every target fetched, linking nowhere.
fn all_fetched(batch: Vec<CrawlTarget>) -> Option<Vec<CrawlResult>> {
    Some(batch.iter().map(|t| fetched(&t.domain, 1, &[])).collect())
}

/// A records file holding `sites`, the set of sites loaded from it, and a
/// store for it.
fn records_file(dir: &Path, sites: &[Item]) -> (PathBuf, Sites, RecordStore) {
    let path = dir.join("records.jsonl");
    let list: SiteList = sites.iter().cloned().collect();
    replace_sites(&path, &list).unwrap();
    let set = load_sites(&path).unwrap();
    let store = RecordStore::open(&path);
    (path, set, store)
}

/// `(crawl_attempted_at, crawl_failures)` of `domain` in the file, journal included.
fn marks(path: &Path, domain: &str) -> (Option<u64>, u32) {
    let s = load_sites(path).unwrap().get(domain).unwrap();
    (s.crawl_attempted_at(), s.crawl_failures())
}

/// The sites of the records file at `path` in file order, as its lines
/// hold them: its journal is not replayed.
fn file_sites(path: &Path) -> Vec<Item> {
    let mut sites = Vec::new();
    crate::outline::for_each_site(path, |s| sites.push(s)).unwrap();
    sites
}

/// The lines of `set`, best first, as compacting writes them.
fn lines_of(set: &Sites) -> Vec<Vec<u8>> {
    set.lines_by_link_score().map(<[u8]>::to_vec).collect()
}

#[test]
fn when_sites_are_due_again() {
    assert_eq!(due_at(&site_with("a.com", 1, None, None), WINDOW), None);
    // Fetched, or answered without a page (robots.txt, HTTP errors): the window.
    assert_eq!(
        due_at(&site_with("a.com", 1, Some(100), Some(90)), WINDOW),
        Some(100 + WINDOW)
    );
    assert_eq!(
        due_at(&site_with("a.com", 1, None, Some(90)), WINDOW),
        Some(90 + WINDOW)
    );
    // Not reached: 1 day, 2, 4, 8, 16, then the window.
    let unreachable = |failures| {
        let mut s = site_with("a.com", 1, Some(10), Some(1_000));
        set_marks(&mut s, Some(1_000), failures);
        due_at(&s, WINDOW).unwrap() - 1_000
    };
    let waits: Vec<u64> = (1..=7).map(|f| unreachable(f) / DAY).collect();
    assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);
    assert_eq!(unreachable(u32::MAX), WINDOW);
    assert_eq!(retry_after(1, 6 * 3600), 6 * 3600, "never past the window");
    assert_eq!(retry_after(3, 0), 0);
    // A site judged dead: three windows.
    let mut gone = site_with("a.com", 1, Some(10), Some(1_000));
    set_marks(&mut gone, Some(1_000), 6);
    site::make_gone(&mut gone, 1_000);
    assert_eq!(due_at(&gone, WINDOW), Some(1_000 + 3 * WINDOW));
}

#[test]
fn picks_the_best_sites_that_are_due() {
    let now = 1_000;
    let window = 500;
    let sites = [
        site_with("c.com", 30, None, None),          // never tried
        site_with("a.com", 10, Some(1_000), None),   // fetched within the window
        site_with("b.com", 20, Some(10), None),      // fetched long ago: due
        site_with("f.com", 5, None, Some(900)),      // answered within the window
        site_with("g.com", 15, Some(10), Some(800)), // tried again within the window
        site_with("h.com", 25, None, Some(50)),      // answered long ago: due
        site_with("d.com", 40, None, None),
        site_with("e.com", 40, None, None), // ties broken by domain
    ];
    // Two never tried, one due again, best first.
    let targets = select_targets(sites.iter(), 3, now, window);
    assert_eq!(domains(&targets), ["b.com", "c.com", "d.com"]);
    assert_eq!(targets[0], CrawlTarget::new("b.com"));

    let all = select_targets(sites.iter(), 100, now, window);
    assert_eq!(domains(&all), ["b.com", "h.com", "c.com", "d.com", "e.com"]);
    assert!(select_targets(sites.iter(), 0, now, window).is_empty());

    // A whole set picks the same, keeping only what picking reads of each.
    let set: Sites = sites.iter().cloned().collect();
    for budget in [0, 1, 3, 100] {
        assert_eq!(
            select_site_targets(&set, budget, now, window),
            select_targets(sites.iter(), budget, now, window),
            "{budget}"
        );
    }
}

#[test]
fn targets_fall_back_to_where_a_site_was_last_reached() {
    let mut moved = site_with("moved.com", 1, None, None);
    moved.url = Some("https://www.moved.com/en/".into());
    let sites = [moved, site_with("plain.com", 2, None, None)];
    let targets = select_targets(sites.iter(), 2, 1_000, WINDOW);
    assert_eq!(
        targets,
        [
            CrawlTarget {
                domain: "moved.com".into(),
                url: "https://moved.com/".into(),
                known_url: Some("https://www.moved.com/en/".into()),
            },
            CrawlTarget::new("plain.com"),
        ]
    );
}

#[test]
fn a_site_s_item_is_picked_as_its_record_is() {
    let now = 1_000;
    let window = 500;
    let record = |domain: &str, tranco, crawled_at, attempted_at| {
        let mut r = SiteRecord::new(domain);
        r.signals.tranco_rank = Some(tranco);
        r.crawled_at = crawled_at;
        r.crawl_attempted_at = attempted_at;
        r
    };
    let mut moved = record("moved.com", 1, Some(10), Some(800));
    moved.url = Some("https://www.moved.com/en/".into());
    moved.crawl_failures = 2;
    let mut gone = record("gone.com", 3, None, Some(100));
    gone.crawl_failures = 6;
    gone.make_gone(100);
    let records = [
        moved,
        gone,
        record("c.com", 30, None, None),
        record("a.com", 10, Some(1_000), None),
        record("b.com", 20, Some(10), None),
        record("h.com", 25, None, Some(50)),
        record("d.com", 40, None, None),
        record("e.com", 40, None, None),
    ];
    let items: Vec<Item> = records.iter().map(SiteRecord::to_stored_item).collect();
    for (record, item) in records.iter().zip(&items) {
        assert_eq!(CrawlSite::domain(item), record.domain);
        assert_eq!(CrawlSite::url(item), record.url.as_deref());
        assert_eq!(CrawlSite::link_score(item), record.link_score());
        assert_eq!(CrawlSite::crawled_at(item), record.crawled_at);
        assert_eq!(
            CrawlSite::crawl_attempted_at(item),
            record.crawl_attempted_at
        );
        assert_eq!(CrawlSite::crawl_failures(item), record.crawl_failures);
        assert_eq!(CrawlSite::gone_at(item), record.gone_at);
        assert_eq!(due_at(item, window), due_at(record, window));
        assert_eq!(target_for(item), target_for(record));
    }
    let set: Sites = items.iter().cloned().collect();
    for budget in [0, 1, 3, 100] {
        let picked = select_targets(records.iter(), budget, now, window);
        assert_eq!(select_targets(items.iter(), budget, now, window), picked);
        assert_eq!(select_site_targets(&set, budget, now, window), picked);
    }
}

#[test]
fn the_budget_is_split_between_new_sites_and_due_ones() {
    let now = 100 * DAY;
    // Due sites rank better than the new ones, which would take every
    // pick if the best simply came first.
    let due = |i: u32| site_with(&format!("due{i}.com"), i + 1, Some(DAY), Some(DAY));
    let new = |i: u32| site_with(&format!("new{i}.com"), i + 100, None, None);
    let picks = |sites: &[Item], budget| -> (usize, usize) {
        let targets = select_targets(sites.iter(), budget, now, WINDOW);
        let new = targets
            .iter()
            .filter(|t| t.domain.starts_with("new"))
            .count();
        (new, targets.len() - new)
    };
    let both: Vec<Item> = (0..10).map(due).chain((0..10).map(new)).collect();
    assert_eq!(picks(&both, 4), (2, 2));
    assert_eq!(picks(&both, 5), (3, 2), "the odd one goes to a new site");
    assert_eq!(picks(&both, 15), (8, 7));
    assert_eq!(picks(&both, 100), (10, 10));
    // A side with too few leaves its share to the other.
    let one_due: Vec<Item> = (0..1).map(due).chain((0..10).map(new)).collect();
    assert_eq!(picks(&one_due, 6), (5, 1));
    let two_new: Vec<Item> = (0..10).map(due).chain((0..2).map(new)).collect();
    assert_eq!(picks(&two_new, 6), (2, 4));
    // The best of each side, in order of link score overall.
    let targets = select_targets(both.iter(), 4, now, WINDOW);
    assert_eq!(
        domains(&targets),
        ["due0.com", "due1.com", "new0.com", "new1.com"]
    );
}

#[test]
fn a_node_reaches_every_site_and_keeps_the_best_fresh() {
    // 1,000 sites, 10 homepages a day, 30 days between refreshes: picking
    // only the best would re-crawl the best 300 forever.
    let mut set: Sites = (0..1_000)
        .map(|i| site_with(&format!("s{i:04}.com"), i + 1, None, None))
        .collect();
    let mut crawls_of_best = 0;
    let mut never_tried_after = Vec::new();
    for day in 1..=250u64 {
        let now = day * DAY;
        for target in select_site_targets(&set, 10, now, WINDOW) {
            crawls_of_best += usize::from(target.domain == "s0000.com");
            set.update(&target.domain, |s| {
                site::set_crawled(s, Some(now));
                set_marks(s, Some(now), 0);
            });
        }
        never_tried_after.push(set.iter().filter(|s| s.crawled_at().is_none()).count());
    }
    let reached_all = never_tried_after.iter().position(|&n| n == 0).unwrap() + 1;
    assert!(
        reached_all <= 200,
        "every site crawled by day {reached_all}"
    );
    // The best site is refreshed every 30 days all along.
    assert_eq!(crawls_of_best, 250 / 30 + 1);
}

#[test]
fn a_batch_marks_every_target_by_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut set, mut store) = records_file(
        dir.path(),
        &[
            site_with("a.com", 10, None, None),
            site_with("b.com", 20, Some(100), Some(100)),
            site_with("c.com", 30, None, None),
            site_with("d.com", 35, None, None),
            site_with("idle.com", 40, None, None),
        ],
    );
    let mut flaky = site_with("flaky.com", 50, Some(100), Some(200));
    set_marks(&mut flaky, Some(200), 2);
    commit(&mut set, &mut store, vec![Change::Merge { site: flaky }]).unwrap();

    let started = now_unix();
    let targets: Vec<CrawlTarget> = ["a.com", "b.com", "c.com", "d.com", "flaky.com"]
        .into_iter()
        .map(CrawlTarget::new)
        .collect();
    let totals = crawl_in_batches(
        &mut set,
        &targets,
        10,
        &mut store,
        |_| {
            Some(vec![
                fetched("a.com", started, &["new.com"]),
                failed("b.com"),
                refused("c.com"),
                failed_there("d.com"),
                fetched("flaky.com", started, &[]),
            ])
        },
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!((totals.attempted, totals.discovered), (5, 1));
    assert_eq!(totals.end, RunEnd::Finished);

    // What is in memory is what is saved.
    let saved = load_sites(&path).unwrap();
    assert_eq!(lines_of(&saved), lines_of(&set));
    let a = saved.get("a.com").unwrap();
    let tried_at = a.crawl_attempted_at().unwrap();
    assert!(tried_at >= started);
    assert_eq!((a.crawled_at(), a.crawl_failures()), (Some(started), 0));
    assert_eq!(a.title.as_deref(), Some("a.com home"));
    // Not reached: retried in a day. Refused by robots.txt: an answer.
    let b = saved.get("b.com").unwrap();
    assert_eq!(
        (b.crawled_at(), b.crawl_attempted_at(), b.crawl_failures()),
        (Some(100), Some(tried_at), 1)
    );
    assert_eq!(due_at(&b, WINDOW), Some(tried_at + DAY));
    let c = saved.get("c.com").unwrap();
    assert_eq!(
        (c.crawl_attempted_at(), c.crawl_failures()),
        (Some(tried_at), 0)
    );
    assert_eq!(due_at(&c, WINDOW), Some(tried_at + WINDOW));
    // So is a failure on the site's side, like a robots.txt server error.
    let d = saved.get("d.com").unwrap();
    assert_eq!(
        (d.crawled_at(), d.crawl_attempted_at(), d.crawl_failures()),
        (None, Some(tried_at), 0)
    );
    assert_eq!(due_at(&d, WINDOW), Some(tried_at + WINDOW));
    // An answer ends a run of failures.
    assert_eq!(saved.get("flaky.com").unwrap().crawl_failures(), 0);
    let idle = saved.get("idle.com").unwrap();
    assert_eq!(
        (idle.crawl_attempted_at(), idle.crawl_failures()),
        (None, 0)
    );
    let new = saved.get("new.com").unwrap();
    assert_eq!(
        (
            new.crawl_attempted_at(),
            site::signals(&new).linking_domains
        ),
        (None, 1)
    );
}

/// A record set as a crawl held it before it held sites: each change made
/// to the records as changes were made to them then
/// ([`reference::apply`]). The reference that a crawl of a whole set of
/// sites is checked against.
struct Records(RecordSet);

impl CrawlSet for Records {
    fn marks(&self, domain: &str) -> (Option<u64>, u32) {
        let record = self.0.get(domain);
        (
            record.and_then(|r| r.crawl_attempted_at),
            record.map_or(0, |r| r.crawl_failures),
        )
    }

    fn apply(&mut self, change: Change) {
        reference::apply(change, &mut self.0);
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn compact(&self, _store: &mut RecordStore) -> Result<usize> {
        unreachable!("the journal stays smaller than it is folded in at")
    }
}

#[test]
fn a_crawl_of_sites_saves_and_writes_what_a_crawl_of_records_did() {
    let started = now_unix();
    let mut flaky = site_with("flaky.com", 50, Some(100), Some(200));
    set_marks(&mut flaky, Some(200), 2);
    let mut moved = site_with("moved.com", 60, Some(100), Some(100));
    moved.url = Some("https://www.moved.com/en/".into());
    let held = [
        site_with("a.com", 10, None, None),
        site_with("b.com", 20, Some(100), Some(100)),
        site_with("c.com", 30, None, None),
        site_with("d.com", 35, None, None),
        site_with("idle.com", 40, None, None),
        flaky,
        moved,
    ];
    let results = |batch: Vec<CrawlTarget>| {
        let results = batch
            .iter()
            .map(|t| match t.domain.as_str() {
                "a.com" => fetched("a.com", started, &["new.com", "b.com", "c.com"]),
                "b.com" => failed("b.com"),
                "c.com" => refused("c.com"),
                "d.com" => failed_there("d.com"),
                domain => fetched(domain, started, &["new.com", "other.org"]),
            })
            .collect();
        Some(results)
    };
    let write = |dir: &Path| {
        let path = dir.join("records.jsonl");
        replace_sites(&path, &held.iter().cloned().collect()).unwrap();
        path
    };
    let targets: Vec<CrawlTarget> = held.iter().map(target_for).collect();
    // A crawl marks its sites tried when it runs, and the two may run a
    // second apart.
    let at_runtime = |bytes: Vec<u8>| {
        let text = String::from_utf8(bytes).unwrap();
        let mut out = String::new();
        let mut digits = String::new();
        for c in text.chars().chain(['\n']) {
            if c.is_ascii_digit() {
                digits.push(c);
                continue;
            }
            if !digits.is_empty() {
                if digits.parse::<u64>().is_ok_and(|n| n >= started) {
                    out += "NOW";
                } else {
                    out += &digits;
                }
                digits.clear();
            }
            out.push(c);
        }
        out
    };

    let sites_dir = tempfile::tempdir().unwrap();
    let path = write(sites_dir.path());
    let mut set = load_sites(&path).unwrap();
    let mut store = RecordStore::open(&path);
    crawl_in_batches(&mut set, &targets, 2, &mut store, results, |_| Ok(())).unwrap();
    let journal = at_runtime(fs::read(journal_path(&path)).unwrap());
    store.compact(&set).unwrap();
    let file = at_runtime(fs::read(&path).unwrap());

    let records_dir = tempfile::tempdir().unwrap();
    let records_path = write(records_dir.path());
    let mut records = Records(reference::load_records(&records_path).unwrap());
    let mut store = RecordStore::open(&records_path);
    crawl_in_batches(&mut records, &targets, 2, &mut store, results, |_| Ok(())).unwrap();
    assert_eq!(
        journal,
        at_runtime(fs::read(journal_path(&records_path)).unwrap())
    );
    drop(store);
    reference::compact(&records_path, &records.0).unwrap();
    assert_eq!(file, at_runtime(fs::read(&records_path).unwrap()));
    assert!(file.contains("new.com") && file.contains("NOW"), "{file}");
}

#[test]
fn linking_domains_count_over_the_whole_run() {
    let results = [
        fetched("a.com", 1, &["hub.com", "a.com"]),
        fetched("b.com", 1, &["hub.com"]),
        fetched("c.com", 1, &["hub.com", "other.com"]),
    ];
    let signals = |set: &Sites, domain: &str| site::signals(&set.get(domain).unwrap());
    let unbatched: Sites = to_items(&results).into_iter().collect();
    assert_eq!(signals(&unbatched, "hub.com").linking_domains, 3);

    let mut set = Sites::new();
    let mut merger = BatchMerger::default();
    for batch in results.chunks(2) {
        for change in merger.changes(batch) {
            change.apply(&mut set);
        }
    }
    for domain in ["hub.com", "other.com"] {
        assert_eq!(
            signals(&set, domain),
            signals(&unbatched, domain),
            "{domain}"
        );
    }

    // A larger count from another source (a WAT file, say) is kept.
    let mut known = site::new("hub.com");
    let mut ranks = site::signals(&known);
    ranks.linking_domains = 10;
    site::set_signals(&mut known, &ranks);
    let mut set: Sites = [known].into_iter().collect();
    for change in BatchMerger::default().changes(&results) {
        change.apply(&mut set);
    }
    assert_eq!(signals(&set, "hub.com").linking_domains, 10);
}

#[test]
fn every_batch_is_saved_before_and_after_it_is_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut set, mut store) = records_file(dir.path(), &sites(5));
    let targets = select_site_targets(&set, 10, now_unix(), WINDOW);
    let started = now_unix();

    let mut batch_sizes: Vec<usize> = Vec::new();
    let mut reports: Vec<usize> = Vec::new();
    let totals = crawl_in_batches(
        &mut set,
        &targets,
        2,
        &mut store,
        |batch| {
            // Earlier batches are saved with their results, and this one
            // as tried and failed until it is done.
            let saved = load_sites(&path).unwrap();
            let done: usize = batch_sizes.iter().sum();
            for (i, target) in targets.iter().enumerate() {
                let s = saved.get(&target.domain).unwrap();
                let expected = if i < done {
                    let failed = target.domain == "site01.com";
                    (!failed, u32::from(failed))
                } else if i < done + batch.len() {
                    (false, 1)
                } else {
                    (false, 0)
                };
                assert_eq!(
                    (s.crawled_at().is_some(), s.crawl_failures()),
                    expected,
                    "{i}"
                );
            }
            batch_sizes.push(batch.len());
            Some(
                batch
                    .iter()
                    .map(|t| match t.domain.as_str() {
                        "site01.com" => failed("site01.com"),
                        domain => fetched(domain, started, &["linked.com"]),
                    })
                    .collect(),
            )
        },
        |totals| {
            reports.push(totals.attempted);
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(batch_sizes, [2, 2, 1]);
    assert_eq!(reports, [2, 4, 5]);
    // Totals cover all three batches, and linked.com is new only once.
    let outcomes = CrawlSummary {
        fetched: 4,
        failed: 1,
        unreachable: 1,
        ..CrawlSummary::default()
    };
    assert_eq!(
        totals,
        RunTotals {
            attempted: 5,
            outcomes,
            discovered: 1,
            end: RunEnd::Finished,
        }
    );

    // The file itself waits for the journal to be folded in.
    assert_eq!(file_sites(&path), sites(5));
    store.compact(&set).unwrap();
    assert!(!journal_path(&path).exists());
    let file = file_sites(&path);
    let order: Vec<&str> = file.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        order,
        [
            "site00.com",
            "site01.com",
            "site02.com",
            "site03.com",
            "site04.com",
            "linked.com"
        ]
    );
    for s in &file[..5] {
        assert!(
            s.crawl_attempted_at().is_some_and(|t| t >= started),
            "{s:?}"
        );
    }
    assert_eq!((file[1].crawled_at(), file[1].crawl_failures()), (None, 1));
    assert_eq!(site::signals(&file[5]).linking_domains, 4);
    assert_eq!(file[5].crawl_attempted_at(), None);
}

#[test]
fn a_crash_while_fetching_does_not_hit_the_same_sites_next_time() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut set, mut store) = records_file(dir.path(), &sites(4));
    let targets = select_site_targets(&set, 4, now_unix(), WINDOW);
    // The second batch holds a page that takes the crawler down.
    let mut batches = 0;
    let crashed = catch_unwind(AssertUnwindSafe(|| {
        crawl_in_batches(
            &mut set,
            &targets,
            2,
            &mut store,
            |batch| {
                batches += 1;
                assert!(batches < 2, "out of memory");
                all_fetched(batch)
            },
            |_| Ok(()),
        )
    }));
    assert!(crashed.is_err());
    drop(store);

    // The next run starts from the file: the first batch is done, the
    // second counts as a failure.
    let now = now_unix();
    let set = load_sites(&path).unwrap();
    for domain in ["site02.com", "site03.com"] {
        let (attempted_at, failures) = marks(&path, domain);
        assert!(attempted_at.is_some_and(|t| t >= now - 5), "{domain}");
        assert_eq!(failures, 1, "{domain}");
    }
    assert!(select_site_targets(&set, 10, now, WINDOW).is_empty());
    // They come back after a day...
    let later = select_site_targets(&set, 10, now + DAY + 5, WINDOW);
    assert_eq!(domains(&later), ["site02.com", "site03.com"]);

    // ...and after two days when it happens again.
    let mut set = set;
    let mut store = RecordStore::open(&path);
    let crashed = catch_unwind(AssertUnwindSafe(|| {
        crawl_in_batches(
            &mut set,
            &later,
            2,
            &mut store,
            |_| panic!("out of memory again"),
            |_| Ok(()),
        )
    }));
    assert!(crashed.is_err());
    let set = load_sites(&path).unwrap();
    let site02 = set.get("site02.com").unwrap();
    assert_eq!(site02.crawl_failures(), 2);
    assert_eq!(
        due_at(&site02, WINDOW),
        Some(site02.crawl_attempted_at().unwrap() + 2 * DAY)
    );
}

#[test]
fn a_stopped_batch_gets_its_old_marks_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut before = sites(5);
    set_marks(&mut before[3], Some(77), 1);
    let (path, mut set, mut store) = records_file(dir.path(), &before);
    let targets: Vec<CrawlTarget> = before.iter().map(|s| CrawlTarget::new(&s.id)).collect();
    let mut batches = 0;
    let totals = crawl_in_batches(
        &mut set,
        &targets,
        2,
        &mut store,
        |batch| {
            batches += 1;
            // The second batch is cut off, as on shutdown.
            if batches < 2 {
                all_fetched(batch)
            } else {
                None
            }
        },
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(batches, 2);
    assert_eq!(totals.end, RunEnd::Stopped);
    assert_eq!((totals.attempted, totals.outcomes.fetched), (2, 2));
    for (domain, expected) in [
        ("site02.com", (None, 0)),
        ("site03.com", (Some(77), 1)),
        ("site04.com", (None, 0)),
    ] {
        assert_eq!(marks(&path, domain), expected, "{domain}");
        let s = set.get(domain).unwrap();
        assert_eq!(
            (s.crawl_attempted_at(), s.crawl_failures()),
            expected,
            "{domain}"
        );
    }
    assert!(marks(&path, "site01.com").0.is_some());

    // An error from the report ends the run after that batch was saved.
    let err = crawl_in_batches(
        &mut set,
        &targets[2..],
        2,
        &mut store,
        |batch| Some(batch.iter().map(|t| failed(&t.domain)).collect()),
        |_| anyhow::bail!("disk full"),
    )
    .unwrap_err();
    assert_eq!(err.to_string(), "disk full");
    assert_eq!(marks(&path, "site02.com").1, 1);
    assert_eq!(marks(&path, "site04.com"), (None, 0));
}

#[test]
fn an_offline_batch_is_not_saved() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut set, mut store) = records_file(dir.path(), &sites(60));
    let targets = select_site_targets(&set, 60, now_unix(), WINDOW);
    let mut batches = 0;
    let totals = crawl_in_batches(
        &mut set,
        &targets,
        30,
        &mut store,
        |batch| {
            batches += 1;
            if batches == 1 {
                return all_fetched(batch);
            }
            // The uplink is gone: only 2 of 30 answer.
            Some(
                batch
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        if i < 2 {
                            fetched(&t.domain, 1, &["new.com"])
                        } else {
                            failed(&t.domain)
                        }
                    })
                    .collect(),
            )
        },
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(
        totals.end,
        RunEnd::Offline(OfflineBatch {
            failed: 28,
            expected: 30
        })
    );
    assert_eq!((totals.attempted, totals.outcomes.fetched), (30, 30));
    let saved = load_sites(&path).unwrap();
    assert_eq!(saved.len(), 60, "nothing of the second batch was merged");
    for target in &targets[30..] {
        let s = saved.get(&target.domain).unwrap();
        assert_eq!(
            (s.crawled_at(), s.crawl_attempted_at(), s.crawl_failures()),
            (None, None, 0),
            "{}",
            target.domain
        );
    }
    assert!(targets[..30]
        .iter()
        .all(|t| saved.get(&t.domain).unwrap().crawled_at().is_some()));
    // So they are first in line next time.
    assert_eq!(
        domains(&select_site_targets(&saved, 30, now_unix(), WINDOW)),
        domains(&targets[30..])
    );
}

#[test]
fn an_offline_batch_is_fetched_again_after_each_wait() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut set, mut store) = records_file(dir.path(), &sites(60));
    let targets = select_site_targets(&set, 60, now_unix(), WINDOW);
    let waits = [Duration::from_millis(1); 2];
    let mut tries: Vec<String> = Vec::new();
    let mut fetcher = Batches {
        crawl: |batch: Vec<CrawlTarget>| {
            tries.push(batch[0].domain.clone());
            // The router is swamped for the first two tries of batch 1.
            if tries.len() <= 2 {
                Some(batch.iter().map(|t| failed(&t.domain)).collect())
            } else {
                all_fetched(batch)
            }
        },
        queued: Vec::new(),
    };
    let totals = crawl_rolling(
        &mut set,
        &targets,
        30,
        &mut store,
        &waits,
        &mut fetcher,
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(totals.end, RunEnd::Finished);
    assert_eq!(
        tries,
        [&targets[0], &targets[0], &targets[0], &targets[30]].map(|t| t.domain.clone())
    );
    assert_eq!((totals.attempted, totals.outcomes.fetched), (60, 60));
    let saved = load_sites(&path).unwrap();
    assert!(targets
        .iter()
        .all(|t| saved.get(&t.domain).unwrap().crawl_failures() == 0));

    // Each batch gets every wait anew, and the run ends when they run out.
    let dir = tempfile::tempdir().unwrap();
    let (path, mut set, mut store) = records_file(dir.path(), &sites(60));
    let mut tries = 0;
    let mut fetcher = Batches {
        crawl: |batch: Vec<CrawlTarget>| {
            tries += 1;
            Some(batch.iter().map(|t| failed(&t.domain)).collect())
        },
        queued: Vec::new(),
    };
    let totals = crawl_rolling(
        &mut set,
        &targets,
        30,
        &mut store,
        &waits,
        &mut fetcher,
        |_| Ok(()),
    )
    .unwrap();
    drop(fetcher);
    assert_eq!(tries, 3);
    assert!(matches!(totals.end, RunEnd::Offline(_)));
    let saved = load_sites(&path).unwrap();
    assert!(targets.iter().all(|t| {
        let s = saved.get(&t.domain).unwrap();
        (s.crawl_attempted_at(), s.crawl_failures()) == (None, 0)
    }));
}

#[test]
fn only_sites_that_should_answer_can_make_a_batch_look_offline() {
    let mark = |i: usize, failures: u32| Mark {
        domain: format!("site{i}.com"),
        attempted_at: None,
        failures,
    };
    let judge = |marks: &[Mark], results: &[CrawlResult]| {
        let outcomes = results
            .iter()
            .map(|r| (r.domain.as_str(), &r.outcome))
            .collect();
        offline_batch(marks, &outcomes)
    };
    let fresh: Vec<Mark> = (0..20).map(|i| mark(i, 0)).collect();
    let all_failed: Vec<CrawlResult> = fresh.iter().map(|m| failed(&m.domain)).collect();
    assert_eq!(
        judge(&fresh, &all_failed),
        Some(OfflineBatch {
            failed: 20,
            expected: 20
        })
    );
    // Too few to tell: a few dead sites are not an outage.
    assert_eq!(judge(&fresh[..19], &all_failed[..19]), None);
    // 90% is the bar.
    let mut results = all_failed.clone();
    results[0] = fetched("site0.com", 1, &[]);
    results[1] = refused("site1.com");
    assert!(judge(&fresh, &results).is_some());
    results[2] = CrawlResult {
        domain: "site2.com".into(),
        outcome: CrawlOutcome::HttpStatus { status: 503 },
    };
    assert_eq!(judge(&fresh, &results), None, "any answer counts");
    // A missing result counts as a failure, and so does a failure of any
    // kind: one that nearly every site gives, such as an HTTP client that
    // cannot be built, is this side's.
    assert!(judge(&fresh, &[]).is_some());
    let no_client: Vec<CrawlResult> = fresh
        .iter()
        .map(|m| CrawlResult {
            domain: m.domain.clone(),
            outcome: CrawlOutcome::Failed {
                error: "building the HTTP client: no TLS".into(),
                network: false,
            },
        })
        .collect();
    assert!(judge(&fresh, &no_client).is_some());
    // Sites that failed last time are left out of the count.
    let mixed: Vec<Mark> = (0..40).map(|i| mark(i, u32::from(i >= 15))).collect();
    let none_answered: Vec<CrawlResult> = mixed.iter().map(|m| failed(&m.domain)).collect();
    assert_eq!(judge(&mixed, &none_answered), None);
}

#[test]
fn the_journal_is_folded_in_as_it_grows() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut set, mut store) = records_file(dir.path(), &sites(6));
    store.set_min_compact_bytes(0);
    let targets = select_site_targets(&set, 6, now_unix(), WINDOW);
    let mut journal_seen = Vec::new();
    crawl_in_batches(
        &mut set,
        &targets,
        2,
        &mut store,
        |batch| {
            journal_seen.push(journal_path(&path).exists());
            all_fetched(batch)
        },
        |_| {
            assert!(!journal_path(&path).exists(), "folded in after the batch");
            Ok(())
        },
    )
    .unwrap();
    // Each batch's marks go to a fresh journal before it is fetched.
    assert_eq!(journal_seen, [true, true, true]);
    assert!(file_sites(&path).iter().all(|s| s.crawled_at().is_some()));
}

#[test]
fn only_getting_no_answer_is_a_connection_failure() {
    assert!(is_connection_failure(&failed("a.com").outcome));
    assert!(!is_connection_failure(&failed_there("a.com").outcome));
    assert!(!is_connection_failure(&refused("a.com").outcome));
    assert!(!is_connection_failure(&CrawlOutcome::HttpStatus {
        status: 503
    }));
}

#[test]
fn counts_outcomes() {
    let result = |domain: &str, outcome| CrawlResult {
        domain: domain.to_string(),
        outcome,
    };
    let page = CrawledPage {
        domain: "a.com".into(),
        final_url: "https://a.com/".into(),
        status: 200,
        fetched_at: 1,
        meta: PageMeta::default(),
        icon: None,
    };
    let results = [
        result("a.com", CrawlOutcome::Fetched(page)),
        result("b.com", CrawlOutcome::RobotsDisallowed),
        result("c.com", CrawlOutcome::HttpStatus { status: 503 }),
        result(
            "d.com",
            CrawlOutcome::NotHtml {
                content_type: "application/pdf".into(),
            },
        ),
        result(
            "e.com",
            CrawlOutcome::OffsiteRedirect {
                final_url: "https://f.com/".into(),
            },
        ),
        result(
            "g.com",
            CrawlOutcome::Failed {
                error: "dns".into(),
                network: true,
            },
        ),
        result(
            "h.com",
            CrawlOutcome::Failed {
                error: "timeout".into(),
                network: true,
            },
        ),
        failed_there("i.com"),
    ];
    let summary = CrawlSummary::of(&results);
    assert_eq!(
        summary,
        CrawlSummary {
            fetched: 1,
            robots_disallowed: 1,
            http_status: 1,
            not_html: 1,
            bot_check: 0,
            offsite_redirect: 1,
            failed: 3,
            unreachable: 2,
        }
    );
    assert_eq!(summary.errors(), 6);
    let mut twice = summary;
    twice.add(&summary);
    assert_eq!((twice.failed, twice.unreachable), (6, 4));
}

/// A [`Fetcher`] that finishes the sites it was given in its own order:
/// `slow` ones only once nothing else is left.
struct OutOfOrder {
    ahead: usize,
    slow: Vec<&'static str>,
    started: Vec<String>,
    /// Sites started when each `finished` was called.
    seen: Vec<usize>,
    stop_at: Option<usize>,
}

impl Fetcher for OutOfOrder {
    fn ahead(&self) -> usize {
        self.ahead
    }

    fn start(&mut self, targets: Vec<CrawlTarget>) {
        self.started.extend(targets.into_iter().map(|t| t.domain));
    }

    fn finished(&mut self, n: usize) -> Option<(Vec<String>, Vec<CrawlResult>)> {
        self.seen.push(self.started.len());
        if self.stop_at == Some(self.seen.len()) {
            return None;
        }
        let (fast, slow): (Vec<String>, Vec<String>) = std::mem::take(&mut self.started)
            .into_iter()
            .partition(|domain| !self.slow.contains(&domain.as_str()));
        let mut order: Vec<String> = fast.into_iter().chain(slow).collect();
        self.started = order.split_off(n.min(order.len()));
        let results = order.iter().map(|domain| fetched(domain, 1, &[])).collect();
        Some((order, results))
    }
}

#[test]
fn slow_sites_do_not_hold_up_the_batches_after_them() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut set, mut store) = records_file(dir.path(), &sites(7));
    let targets = select_site_targets(&set, 10, now_unix(), WINDOW);
    let mut fetcher = OutOfOrder {
        ahead: 2,
        slow: vec!["site00.com"],
        started: Vec::new(),
        seen: Vec::new(),
        stop_at: None,
    };
    let mut reports = Vec::new();
    let totals = crawl_rolling(
        &mut set,
        &targets,
        2,
        &mut store,
        &[],
        &mut fetcher,
        |totals| {
            reports.push(totals.attempted);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(totals.end, RunEnd::Finished);
    assert_eq!((totals.attempted, totals.outcomes.fetched), (7, 7));
    assert_eq!(reports, [2, 4, 6, 7]);
    // Two batches are started ahead of the two sites waited for, and the
    // slow site is waited for only at the end.
    assert_eq!(fetcher.seen, [4, 4, 3, 1]);
    let saved = load_sites(&path).unwrap();
    assert!(saved.iter().all(|s| s.crawled_at() == Some(1)));
}

#[test]
fn a_stopped_rolling_crawl_gives_every_unfinished_site_its_old_marks_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut before = sites(6);
    set_marks(&mut before[0], Some(77), 1);
    let (path, mut set, mut store) = records_file(dir.path(), &before);
    let targets: Vec<CrawlTarget> = before.iter().map(|s| CrawlTarget::new(&s.id)).collect();
    let mut fetcher = OutOfOrder {
        ahead: 2,
        slow: vec!["site00.com"],
        started: Vec::new(),
        seen: Vec::new(),
        stop_at: Some(2),
    };
    let totals = crawl_rolling(&mut set, &targets, 2, &mut store, &[], &mut fetcher, |_| {
        Ok(())
    })
    .unwrap();
    assert_eq!(totals.end, RunEnd::Stopped);
    assert_eq!(totals.attempted, 2);
    // site01 and site02 finished; the rest were started or never begun.
    assert!(marks(&path, "site01.com").0.is_some());
    assert!(marks(&path, "site02.com").0.is_some());
    for (domain, expected) in [
        ("site00.com", (Some(77), 1)),
        ("site03.com", (None, 0)),
        ("site04.com", (None, 0)),
        ("site05.com", (None, 0)),
    ] {
        assert_eq!(marks(&path, domain), expected, "{domain}");
    }
}

#[test]
fn sites_can_be_made_due_early() {
    let now = 10 * DAY;
    let sites = [
        site_with("fresh.com", 1, Some(9 * DAY), Some(9 * DAY)),
        site_with("noted.com", 2, Some(9 * DAY), Some(9 * DAY)),
    ];
    assert!(select_targets(sites.iter(), 10, now, WINDOW).is_empty());
    let early = select_targets_with(sites.iter(), 10, now, WINDOW, |s| s.id == "fresh.com");
    assert_eq!(domains(&early), ["fresh.com"]);
}

/// A set of sites that takes no site it does not hold, as a node's round
/// does with [`crate::node::NodeConfig::take_new_sites`] off.
struct Holding(Sites);

impl CrawlSet for Holding {
    fn marks(&self, domain: &str) -> (Option<u64>, u32) {
        self.0.marks(domain)
    }

    fn apply(&mut self, change: Change) {
        change.apply(&mut self.0);
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn takes(&self, domain: &str) -> bool {
        self.0.contains(domain)
    }

    fn compact(&self, store: &mut RecordStore) -> Result<usize> {
        store.compact(&self.0)
    }
}

#[test]
fn a_set_holding_back_new_sites_only_refreshes_the_ones_it_holds() {
    let dir = tempfile::tempdir().unwrap();
    let (path, set, mut store) = records_file(
        dir.path(),
        &[
            site_with("a.com", 10, None, None),
            site_with("b.com", 20, None, None),
        ],
    );
    let mut set = Holding(set);
    let started = now_unix();
    let targets: Vec<CrawlTarget> = ["a.com"].into_iter().map(CrawlTarget::new).collect();
    let mut fetcher = Batches {
        crawl: |_| Some(vec![fetched("a.com", started, &["new.com", "b.com"])]),
        queued: Vec::new(),
    };
    let totals = crawl_rolling(
        &mut set,
        &targets,
        10,
        &mut store,
        &[],
        &mut fetcher,
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!((totals.attempted, totals.discovered), (1, 0));
    let saved = load_sites(&path).unwrap();
    assert_eq!(saved.len(), 2);
    assert!(!saved.contains("new.com"));
    assert_eq!(saved.get("a.com").unwrap().crawled_at(), Some(started));
    // A site held still counts the links to it.
    let b = saved.get("b.com").unwrap();
    assert_eq!(site::signals(&b).linking_domains, 1);
}
