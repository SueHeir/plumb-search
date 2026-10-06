//! Tests of picking homepages and of crawling them in batches, with fake
//! fetchers instead of the network.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use plumb_core::{read_jsonl, write_jsonl};
use plumb_crawl::{CrawledPage, OutLink, PageMeta};

use super::*;
use crate::records::journal_path;

const DAY: u64 = SECONDS_PER_DAY;
const WINDOW: u64 = 30 * DAY;

fn record(
    domain: &str,
    tranco: u32,
    crawled_at: Option<u64>,
    attempted_at: Option<u64>,
) -> SiteRecord {
    let mut r = SiteRecord::new(domain);
    r.signals.tranco_rank = Some(tranco);
    r.crawled_at = crawled_at;
    r.crawl_attempted_at = attempted_at;
    r
}

/// Sites `site00.com`, `site01.com`... never tried, best first.
fn sites(n: u32) -> Vec<SiteRecord> {
    (0..n)
        .map(|i| record(&format!("site{i:02}.com"), i + 1, None, None))
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

/// A records file holding `records`, and a store for it.
fn records_file(dir: &Path, records: &[SiteRecord]) -> (PathBuf, RecordSet, RecordStore) {
    let path = dir.join("records.jsonl");
    write_jsonl(&path, records).unwrap();
    let set = load_records(&path).unwrap();
    let store = RecordStore::open(&path);
    (path, set, store)
}

/// `(crawl_attempted_at, crawl_failures)` of `domain` in the file, journal included.
fn marks(path: &Path, domain: &str) -> (Option<u64>, u32) {
    let set = load_records(path).unwrap();
    let r = set.get(domain).unwrap();
    (r.crawl_attempted_at, r.crawl_failures)
}

#[test]
fn when_sites_are_due_again() {
    assert_eq!(due_at(&record("a.com", 1, None, None), WINDOW), None);
    // Fetched, or answered without a page (robots.txt, HTTP errors): the window.
    assert_eq!(
        due_at(&record("a.com", 1, Some(100), Some(90)), WINDOW),
        Some(100 + WINDOW)
    );
    assert_eq!(
        due_at(&record("a.com", 1, None, Some(90)), WINDOW),
        Some(90 + WINDOW)
    );
    // Not reached: 1 day, 2, 4, 8, 16, then the window.
    let unreachable = |failures| {
        let mut r = record("a.com", 1, Some(10), Some(1_000));
        r.crawl_failures = failures;
        due_at(&r, WINDOW).unwrap() - 1_000
    };
    let waits: Vec<u64> = (1..=7).map(|f| unreachable(f) / DAY).collect();
    assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);
    assert_eq!(unreachable(u32::MAX), WINDOW);
    assert_eq!(retry_after(1, 6 * 3600), 6 * 3600, "never past the window");
    assert_eq!(retry_after(3, 0), 0);
    // A site judged dead: three windows.
    let mut gone = record("a.com", 1, Some(10), Some(1_000));
    gone.crawl_failures = 6;
    gone.make_gone(1_000);
    assert_eq!(due_at(&gone, WINDOW), Some(1_000 + 3 * WINDOW));
}

#[test]
fn picks_the_best_sites_that_are_due() {
    let now = 1_000;
    let window = 500;
    let records = [
        record("c.com", 30, None, None),          // never tried
        record("a.com", 10, Some(1_000), None),   // fetched within the window
        record("b.com", 20, Some(10), None),      // fetched long ago: due
        record("f.com", 5, None, Some(900)),      // answered within the window
        record("g.com", 15, Some(10), Some(800)), // tried again within the window
        record("h.com", 25, None, Some(50)),      // answered long ago: due
        record("d.com", 40, None, None),
        record("e.com", 40, None, None), // ties broken by domain
    ];
    // Two never tried, one due again, best first.
    let targets = select_targets(records.iter(), 3, now, window);
    assert_eq!(domains(&targets), ["b.com", "c.com", "d.com"]);
    assert_eq!(targets[0], CrawlTarget::new("b.com"));

    let all = select_targets(records.iter(), 100, now, window);
    assert_eq!(domains(&all), ["b.com", "h.com", "c.com", "d.com", "e.com"]);
    assert!(select_targets(records.iter(), 0, now, window).is_empty());
}

#[test]
fn targets_fall_back_to_where_a_site_was_last_reached() {
    let mut moved = record("moved.com", 1, None, None);
    moved.url = Some("https://www.moved.com/en/".into());
    let records = [moved, record("plain.com", 2, None, None)];
    let targets = select_targets(records.iter(), 2, 1_000, WINDOW);
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
fn the_budget_is_split_between_new_sites_and_due_ones() {
    let now = 100 * DAY;
    // Due sites rank better than the new ones, which would take every
    // pick if the best simply came first.
    let due = |i: u32| record(&format!("due{i}.com"), i + 1, Some(DAY), Some(DAY));
    let new = |i: u32| record(&format!("new{i}.com"), i + 100, None, None);
    let picks = |records: &[SiteRecord], budget| -> (usize, usize) {
        let targets = select_targets(records.iter(), budget, now, WINDOW);
        let new = targets
            .iter()
            .filter(|t| t.domain.starts_with("new"))
            .count();
        (new, targets.len() - new)
    };
    let both: Vec<SiteRecord> = (0..10).map(due).chain((0..10).map(new)).collect();
    assert_eq!(picks(&both, 4), (2, 2));
    assert_eq!(picks(&both, 5), (3, 2), "the odd one goes to a new site");
    assert_eq!(picks(&both, 15), (8, 7));
    assert_eq!(picks(&both, 100), (10, 10));
    // A side with too few leaves its share to the other.
    let one_due: Vec<SiteRecord> = (0..1).map(due).chain((0..10).map(new)).collect();
    assert_eq!(picks(&one_due, 6), (5, 1));
    let two_new: Vec<SiteRecord> = (0..10).map(due).chain((0..2).map(new)).collect();
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
    let mut set: RecordSet = (0..1_000)
        .map(|i| record(&format!("s{i:04}.com"), i + 1, None, None))
        .collect();
    let mut crawls_of_best = 0;
    let mut never_tried_after = Vec::new();
    for day in 1..=250u64 {
        let now = day * DAY;
        for target in select_targets(set.iter(), 10, now, WINDOW) {
            crawls_of_best += usize::from(target.domain == "s0000.com");
            let r = set.entry(&target.domain);
            r.crawled_at = Some(now);
            r.crawl_attempted_at = Some(now);
        }
        never_tried_after.push(set.iter().filter(|r| r.crawled_at.is_none()).count());
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
            record("a.com", 10, None, None),
            record("b.com", 20, Some(100), Some(100)),
            record("c.com", 30, None, None),
            record("d.com", 35, None, None),
            record("idle.com", 40, None, None),
        ],
    );
    let mut flaky = record("flaky.com", 50, Some(100), Some(200));
    flaky.crawl_failures = 2;
    commit(&mut set, &mut store, vec![Change::Merge { record: flaky }]).unwrap();

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
    let saved = load_records(&path).unwrap();
    assert_eq!(
        sorted_by_link_score_owned(&saved),
        sorted_by_link_score_owned(&set)
    );
    let a = saved.get("a.com").unwrap();
    let tried_at = a.crawl_attempted_at.unwrap();
    assert!(tried_at >= started);
    assert_eq!((a.crawled_at, a.crawl_failures), (Some(started), 0));
    assert_eq!(a.title.as_deref(), Some("a.com home"));
    // Not reached: retried in a day. Refused by robots.txt: an answer.
    let b = saved.get("b.com").unwrap();
    assert_eq!(
        (b.crawled_at, b.crawl_attempted_at, b.crawl_failures),
        (Some(100), Some(tried_at), 1)
    );
    assert_eq!(due_at(b, WINDOW), Some(tried_at + DAY));
    let c = saved.get("c.com").unwrap();
    assert_eq!(
        (c.crawl_attempted_at, c.crawl_failures),
        (Some(tried_at), 0)
    );
    assert_eq!(due_at(c, WINDOW), Some(tried_at + WINDOW));
    // So is a failure on the site's side, like a robots.txt server error.
    let d = saved.get("d.com").unwrap();
    assert_eq!(
        (d.crawled_at, d.crawl_attempted_at, d.crawl_failures),
        (None, Some(tried_at), 0)
    );
    assert_eq!(due_at(d, WINDOW), Some(tried_at + WINDOW));
    // An answer ends a run of failures.
    assert_eq!(saved.get("flaky.com").unwrap().crawl_failures, 0);
    let idle = saved.get("idle.com").unwrap();
    assert_eq!((idle.crawl_attempted_at, idle.crawl_failures), (None, 0));
    let new = saved.get("new.com").unwrap();
    assert_eq!(
        (new.crawl_attempted_at, new.signals.linking_domains),
        (None, 1)
    );
}

fn sorted_by_link_score_owned(set: &RecordSet) -> Vec<SiteRecord> {
    set.clone().into_sorted_vec()
}

#[test]
fn linking_domains_count_over_the_whole_run() {
    let results = [
        fetched("a.com", 1, &["hub.com", "a.com"]),
        fetched("b.com", 1, &["hub.com"]),
        fetched("c.com", 1, &["hub.com", "other.com"]),
    ];
    let unbatched: RecordSet = to_records(&results).into_iter().collect();
    assert_eq!(unbatched.get("hub.com").unwrap().signals.linking_domains, 3);

    let mut set = RecordSet::new();
    let mut merger = BatchMerger::default();
    for batch in results.chunks(2) {
        for change in merger.changes(batch) {
            change.apply(&mut set);
        }
    }
    for domain in ["hub.com", "other.com"] {
        assert_eq!(
            set.get(domain).unwrap().signals,
            unbatched.get(domain).unwrap().signals,
            "{domain}"
        );
    }

    // A larger count from another source (a WAT file, say) is kept.
    let mut known = SiteRecord::new("hub.com");
    known.signals.linking_domains = 10;
    let mut set: RecordSet = [known].into_iter().collect();
    for change in BatchMerger::default().changes(&results) {
        change.apply(&mut set);
    }
    assert_eq!(set.get("hub.com").unwrap().signals.linking_domains, 10);
}

#[test]
fn every_batch_is_saved_before_and_after_it_is_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut set, mut store) = records_file(dir.path(), &sites(5));
    let targets = select_targets(set.iter(), 10, now_unix(), WINDOW);
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
            let saved = load_records(&path).unwrap();
            let done: usize = batch_sizes.iter().sum();
            for (i, target) in targets.iter().enumerate() {
                let r = saved.get(&target.domain).unwrap();
                let expected = if i < done {
                    let failed = target.domain == "site01.com";
                    (!failed, u32::from(failed))
                } else if i < done + batch.len() {
                    (false, 1)
                } else {
                    (false, 0)
                };
                assert_eq!((r.crawled_at.is_some(), r.crawl_failures), expected, "{i}");
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
    let file: Vec<SiteRecord> = read_jsonl(&path).unwrap();
    assert_eq!(file, sites(5));
    store.compact(&set).unwrap();
    assert!(!journal_path(&path).exists());
    let file: Vec<SiteRecord> = read_jsonl(&path).unwrap();
    let order: Vec<&str> = file.iter().map(|r| r.domain.as_str()).collect();
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
    for r in &file[..5] {
        assert!(r.crawl_attempted_at.is_some_and(|t| t >= started), "{r:?}");
    }
    assert_eq!((file[1].crawled_at, file[1].crawl_failures), (None, 1));
    assert_eq!(file[5].signals.linking_domains, 4);
    assert_eq!(file[5].crawl_attempted_at, None);
}

#[test]
fn a_crash_while_fetching_does_not_hit_the_same_sites_next_time() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut set, mut store) = records_file(dir.path(), &sites(4));
    let targets = select_targets(set.iter(), 4, now_unix(), WINDOW);
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
    let set = load_records(&path).unwrap();
    for domain in ["site02.com", "site03.com"] {
        let (attempted_at, failures) = marks(&path, domain);
        assert!(attempted_at.is_some_and(|t| t >= now - 5), "{domain}");
        assert_eq!(failures, 1, "{domain}");
    }
    assert!(select_targets(set.iter(), 10, now, WINDOW).is_empty());
    // They come back after a day...
    let later = select_targets(set.iter(), 10, now + DAY + 5, WINDOW);
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
    let set = load_records(&path).unwrap();
    let site02 = set.get("site02.com").unwrap();
    assert_eq!(site02.crawl_failures, 2);
    assert_eq!(
        due_at(site02, WINDOW),
        Some(site02.crawl_attempted_at.unwrap() + 2 * DAY)
    );
}

#[test]
fn a_stopped_batch_gets_its_old_marks_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut before = sites(5);
    before[3].crawl_attempted_at = Some(77);
    before[3].crawl_failures = 1;
    let (path, mut set, mut store) = records_file(dir.path(), &before);
    let targets: Vec<CrawlTarget> = before.iter().map(|r| CrawlTarget::new(&r.domain)).collect();
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
        let r = set.get(domain).unwrap();
        assert_eq!(
            (r.crawl_attempted_at, r.crawl_failures),
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
    let targets = select_targets(set.iter(), 60, now_unix(), WINDOW);
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
    let saved = load_records(&path).unwrap();
    assert_eq!(saved.len(), 60, "nothing of the second batch was merged");
    for target in &targets[30..] {
        let r = saved.get(&target.domain).unwrap();
        assert_eq!(
            (r.crawled_at, r.crawl_attempted_at, r.crawl_failures),
            (None, None, 0),
            "{}",
            target.domain
        );
    }
    assert!(targets[..30]
        .iter()
        .all(|t| saved.get(&t.domain).unwrap().crawled_at.is_some()));
    // So they are first in line next time.
    assert_eq!(
        domains(&select_targets(saved.iter(), 30, now_unix(), WINDOW)),
        domains(&targets[30..])
    );
}

#[test]
fn an_offline_batch_is_fetched_again_after_each_wait() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut set, mut store) = records_file(dir.path(), &sites(60));
    let targets = select_targets(set.iter(), 60, now_unix(), WINDOW);
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
    let saved = load_records(&path).unwrap();
    assert!(targets
        .iter()
        .all(|t| saved.get(&t.domain).unwrap().crawl_failures == 0));

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
    let saved = load_records(&path).unwrap();
    assert!(targets.iter().all(|t| {
        let r = saved.get(&t.domain).unwrap();
        (r.crawl_attempted_at, r.crawl_failures) == (None, 0)
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
    let targets = select_targets(set.iter(), 6, now_unix(), WINDOW);
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
    let file: Vec<SiteRecord> = read_jsonl(&path).unwrap();
    assert!(file.iter().all(|r| r.crawled_at.is_some()));
}

fn crawl_args(records: &Path, out: Option<&Path>, top: usize) -> CrawlArgs {
    CrawlArgs {
        records: records.to_path_buf(),
        top,
        skip_crawled_within_days: 30,
        concurrency: 16,
        dns_lookups: 32,
        out: out.map(Path::to_path_buf),
        use_system_proxy: false,
    }
}

#[test]
fn plumb_crawl_writes_the_whole_file_at_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.jsonl");
    write_jsonl(&path, &sites(30)).unwrap();
    crawl_file(&crawl_args(&path, None, 10), |batch| {
        batch.iter().map(|t| fetched(&t.domain, 1, &[])).collect()
    })
    .unwrap();
    assert!(!journal_path(&path).exists());
    let file: Vec<SiteRecord> = read_jsonl(&path).unwrap();
    assert_eq!(file.iter().filter(|r| r.crawled_at.is_some()).count(), 10);

    // To another file, which starts from the records read.
    let out = dir.path().join("out.jsonl");
    std::fs::write(journal_path(&out), "{\"op\":\"stale\"}\n").unwrap();
    crawl_file(&crawl_args(&path, Some(&out), 5), |batch| {
        batch.iter().map(|t| fetched(&t.domain, 2, &[])).collect()
    })
    .unwrap();
    assert!(!journal_path(&out).exists());
    let copy: Vec<SiteRecord> = read_jsonl(&out).unwrap();
    assert_eq!(copy.iter().filter(|r| r.crawled_at.is_some()).count(), 15);
    let input: Vec<SiteRecord> = read_jsonl(&path).unwrap();
    assert_eq!(input, file, "the input is left alone");

    // An interrupted crawl's journal is picked up and folded in.
    let mut store = RecordStore::open(&path);
    let mut late = SiteRecord::new("late.com");
    late.title = Some("Saved before a crash".into());
    store.save(&[Change::Merge { record: late }]).unwrap();
    drop(store);
    crawl_file(&crawl_args(&path, None, 0), |_| unreachable!()).unwrap();
    assert!(!journal_path(&path).exists());
    let file: Vec<SiteRecord> = read_jsonl(&path).unwrap();
    assert!(file.iter().any(|r| r.domain == "late.com"));
}

#[test]
fn plumb_crawl_stops_when_the_network_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.jsonl");
    write_jsonl(&path, &sites(30)).unwrap();
    let err = crawl_file(&crawl_args(&path, None, 30), |batch| {
        batch.iter().map(|t| failed(&t.domain)).collect()
    })
    .unwrap_err()
    .to_string();
    assert!(
        err.starts_with(
            "stopped crawling: 30 of 30 homepages that should have answered could not be \
             fetched"
        ),
        "{err}"
    );
    assert!(err.contains("--use-system-proxy"), "{err}");
    // Nothing was marked, and the file is whole.
    assert!(!journal_path(&path).exists());
    let file: Vec<SiteRecord> = read_jsonl(&path).unwrap();
    assert_eq!(file, sites(30));

    let mut args = crawl_args(&path, None, 30);
    args.use_system_proxy = true;
    let err = crawl_file(&args, |batch| {
        batch.iter().map(|t| failed(&t.domain)).collect()
    })
    .unwrap_err()
    .to_string();
    assert!(!err.contains("--use-system-proxy"), "{err}");
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
    let targets = select_targets(set.iter(), 10, now_unix(), WINDOW);
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
    let saved = load_records(&path).unwrap();
    assert!(saved.iter().all(|r| r.crawled_at == Some(1)));
}

#[test]
fn a_stopped_rolling_crawl_gives_every_unfinished_site_its_old_marks_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut before = sites(6);
    before[0].crawl_attempted_at = Some(77);
    before[0].crawl_failures = 1;
    let (path, mut set, mut store) = records_file(dir.path(), &before);
    let targets: Vec<CrawlTarget> = before.iter().map(|r| CrawlTarget::new(&r.domain)).collect();
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
    let records = [
        record("fresh.com", 1, Some(9 * DAY), Some(9 * DAY)),
        record("noted.com", 2, Some(9 * DAY), Some(9 * DAY)),
    ];
    assert!(select_targets(records.iter(), 10, now, WINDOW).is_empty());
    let early = select_targets_with(records.iter(), 10, now, WINDOW, |r| r.domain == "fresh.com");
    assert_eq!(domains(&early), ["fresh.com"]);
}
