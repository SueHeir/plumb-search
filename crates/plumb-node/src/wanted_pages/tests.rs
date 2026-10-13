use plumb_index::pages::PlacedPage;
use plumb_index::Hit;

use super::*;

const NOW: u64 = 1_800_000_000;

fn hit(domain: &str, title: &str) -> Hit {
    Hit {
        demand: None,
        missing_words: false,
        query_evidence: None,
        placing_text_score: None,
        domain: domain.into(),
        url: format!("https://{domain}/"),
        title: Some(title.into()),
        description: None,
        score: 1.0,
        text_score: 1.0,
        link_score: 0.0,
        country: None,
        named: false,
        official: false,
        key_pages: Vec::new(),
    }
}

fn results(hits: &[(&str, &str)], pages: &[&str]) -> SearchResults {
    SearchResults {
        hits: hits
            .iter()
            .map(|(domain, title)| hit(domain, title))
            .collect(),
        pages: pages
            .iter()
            .map(|url| -> PlacedPage {
                serde_json::from_value(serde_json::json!({
                    "page": {"set": "subpages", "url": url, "title": "A page", "views": 1},
                    "score": 1.0,
                    "named": false,
                    "at": 0,
                }))
                .unwrap()
            })
            .collect(),
        ..SearchResults::default()
    }
}

fn wants(store: &WantedPages, now: u64) -> Vec<(String, Vec<Vec<String>>)> {
    store
        .due(now, 10, 5, |_| true)
        .into_iter()
        .map(|target| (target.domain, target.wants))
        .collect()
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|word| word.to_string()).collect()
}

const IRS: &[(&str, &str)] = &[
    ("irs.gov", "Internal Revenue Service"),
    ("fidelity.com", "Fidelity Investments"),
    ("vanguard.com", "Vanguard"),
    ("schwab.com", "Charles Schwab"),
];

#[test]
fn a_search_wants_the_rest_of_its_words_of_its_best_sites() {
    let dir = tempfile::tempdir().unwrap();
    let store = WantedPages::open(dir.path());
    let shown = ["https://www.vanguard.com/ira/contribution-limits"];
    store.note(
        "irs contribution limits",
        &results(IRS, &shown),
        NOW,
        |url| !url.contains("fidelity"),
    );
    // Vanguard shows a page for it, Fidelity is not allowed, and Schwab is
    // not among the best three.
    assert_eq!(
        wants(&store, NOW + LOOK_AGAIN),
        [("irs.gov".into(), vec![words(&["contribution", "limit"])])]
    );
    // A search for a site alone, or with operators, wants nothing more.
    store.note("internal revenue service", &results(IRS, &[]), NOW, |_| {
        true
    });
    store.note("limits site:irs.gov", &results(IRS, &[]), NOW, |_| true);
    assert_eq!(wants(&store, NOW + LOOK_AGAIN).len(), 1);
}

#[test]
fn pages_found_answer_their_search_and_branch_out_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = WantedPages::open(dir.path());
    store.note(
        "irs contribution limits",
        &results(&IRS[..1], &[]),
        NOW,
        |_| true,
    );
    let targets = store.due(NOW + LOOK_AGAIN, 10, 5, |_| true);
    let lead = |url: &str| Lead {
        url: url.into(),
        label: "Contribution limits".into(),
        depth: 1,
    };
    let page = WantedPage {
        url: "https://www.irs.gov/retirement-plans/contribution-limits".into(),
        title: "Contribution limits".into(),
        depth: 2,
        ..WantedPage::default()
    };
    let followed = Followed {
        domain: "irs.gov".into(),
        pages: vec![
            (page.clone(), 0),
            (
                WantedPage {
                    url: "http://www.irs.gov/plain".into(),
                    ..page.clone()
                },
                0,
            ),
            (
                WantedPage {
                    url: "https://www.irs.gov/xxx".into(),
                    title: "Contribution limits xxx".into(),
                    ..page.clone()
                },
                0,
            ),
        ],
        leads: vec![lead("https://www.irs.gov/more-limits")],
        elsewhere: vec![(lead("https://www.ssa.gov/limits"), 0)],
        fetched: 5,
    };
    let then = NOW + LOOK_AGAIN;
    // Only the https page not plainly adult is kept.
    assert_eq!(store.apply(&targets, vec![followed], then, |_| true), 1);
    // The IRS search is answered; the page found links to ssa.gov, which
    // is wanted for it in turn, with that link to follow.
    let due = store.due(then + LOOK_AGAIN, 10, 5, |_| true);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].domain, "ssa.gov");
    assert_eq!(due[0].homepage, "https://ssa.gov/");
    assert_eq!(due[0].wants, [words(&["contribution", "limit"])]);
    assert_eq!(due[0].leads, [lead("https://www.ssa.gov/limits")]);
    // A page found on ssa.gov does not branch out again.
    let followed = Followed {
        domain: "ssa.gov".into(),
        elsewhere: vec![(lead("https://www.example.gov/limits"), 0)],
        ..Followed::default()
    };
    store.apply(&due, vec![followed], then + LOOK_AGAIN, |_| true);
    assert!(store
        .due(then + 2 * FOLLOW_LEADS_AGAIN, 10, 5, |_| true)
        .is_empty());
    assert!(!store.sites.read_locked().sites.contains_key("example.gov"));
}

#[test]
fn a_site_is_looked_at_again_when_wanted_again_or_a_day_on_for_its_leads() {
    let dir = tempfile::tempdir().unwrap();
    let store = WantedPages::open(dir.path());
    store.note(
        "irs contribution limits",
        &results(&IRS[..1], &[]),
        NOW,
        |_| true,
    );
    let targets = store.due(NOW, 10, 5, |_| true);
    let followed = Followed {
        domain: "irs.gov".into(),
        leads: vec![Lead::default()],
        ..Followed::default()
    };
    store.apply(&targets, vec![followed], NOW, |_| true);
    assert!(store.due(NOW + LOOK_AGAIN, 10, 5, |_| true).is_empty());
    assert_eq!(
        store.due(NOW + FOLLOW_LEADS_AGAIN, 10, 5, |_| true).len(),
        1
    );
    store.note(
        "irs contribution limits",
        &results(&IRS[..1], &[]),
        NOW + 1,
        |_| true,
    );
    assert!(store.due(NOW + LOOK_AGAIN - 1, 10, 5, |_| true).is_empty());
    assert_eq!(store.due(NOW + LOOK_AGAIN, 10, 5, |_| true).len(), 1);
    // A month on, the search is forgotten.
    store.prune(NOW + WANT_KEPT + 1, 10);
    assert!(store.due(NOW + WANT_KEPT + 1, 10, 5, |_| true).is_empty());
}

#[test]
fn pages_kept_are_searched_and_saved() {
    let dir = tempfile::tempdir().unwrap();
    let store = WantedPages::open(dir.path());
    let targets = vec![FollowTarget {
        domain: "irs.gov".into(),
        wants: vec![words(&["contribution", "limit"])],
        ..FollowTarget::default()
    }];
    store.note(
        "irs contribution limits",
        &results(&IRS[..1], &[]),
        NOW,
        |_| true,
    );
    let found = |path: &str, title: &str| {
        (
            WantedPage {
                url: format!("https://www.irs.gov/{path}"),
                title: title.into(),
                depth: 2,
                ..WantedPage::default()
            },
            0,
        )
    };
    let followed = Followed {
        domain: "irs.gov".into(),
        pages: vec![
            found("old", "Retirement topics"),
            found("limits", "Retirement plan contribution limits"),
        ],
        ..Followed::default()
    };
    store.apply(&targets, vec![followed.clone()], NOW, |_| true);
    store.save().unwrap();
    store.build_index().unwrap();
    let hits = store
        .searcher()
        .unwrap()
        .search("contribution limits", 5)
        .unwrap();
    assert_eq!(hits[0].page.url, "https://www.irs.gov/limits");
    assert_eq!(hits[0].page.set, WANTED_SET);

    // Past the most kept, the page found longest ago goes, and the index
    // is built anew in place of the old one.
    let newer = Followed {
        pages: vec![found("limits", "Retirement plan contribution limits")],
        ..followed
    };
    store.apply(&targets, vec![newer], NOW + 1, |_| true);
    store.prune(NOW + 1, 1);
    store.build_index().unwrap();
    let indexes = std::fs::read_dir(dir.path())
        .unwrap()
        .filter(|entry| {
            let entry = entry.as_ref().unwrap();
            index_number(&entry.file_name().to_string_lossy()).is_some()
        })
        .count();
    assert_eq!(indexes, 1);
    store.save().unwrap();

    let reopened = WantedPages::open(dir.path());
    let hits = reopened
        .searcher()
        .unwrap()
        .search("contribution limits", 5)
        .unwrap();
    let urls: Vec<&str> = hits.iter().map(|hit| hit.page.url.as_str()).collect();
    assert_eq!(urls, ["https://www.irs.gov/limits"]);
    // An index older than the pages saved is not searched.
    {
        let mut pages = reopened.pages.write_locked();
        pages.index = 0;
        pages.dirty = true;
    }
    reopened.save().unwrap();
    assert!(WantedPages::open(dir.path()).searcher().is_none());
    #[cfg(unix)]
    for file in ["", SITES_FILE, PAGES_FILE] {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join(file))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{file} is private");
    }
}

#[test]
fn sites_searches_named_come_before_sites_one_hop_out() {
    let dir = tempfile::tempdir().unwrap();
    let store = WantedPages::open(dir.path());
    store.note(
        "irs contribution limits",
        &results(&IRS[..1], &[]),
        NOW,
        |_| true,
    );
    let targets = store.due(NOW, 10, 5, |_| true);
    let lead = Lead {
        url: "https://www.ssa.gov/limits".into(),
        ..Lead::default()
    };
    let followed = Followed {
        domain: "irs.gov".into(),
        elsewhere: vec![(lead, 0)],
        ..Followed::default()
    };
    store.apply(&targets, vec![followed], NOW, |_| true);
    // Wanted by a search before the one that led to ssa.gov, Vanguard
    // still comes first.
    store.note(
        "vanguard roth conversion",
        &results(&IRS[2..3], &[]),
        NOW - 10,
        |_| true,
    );
    let due = store.due(NOW + LOOK_AGAIN, 1, 5, |_| true);
    assert_eq!(due[0].domain, "vanguard.com");
    let due = store.due(NOW + LOOK_AGAIN, 2, 5, |_| true);
    assert_eq!(due[1].domain, "ssa.gov");
}

#[test]
fn searches_and_pages_are_forgotten_on_time() {
    let dir = tempfile::tempdir().unwrap();
    let store = WantedPages::open(dir.path());
    let irs = results(&IRS[..1], &[]);
    store.note("irs contribution limits", &irs, NOW, |_| true);
    store.note("irs tax brackets", &irs, NOW + WANT_KEPT / 2, |_| true);
    let targets = store.due(NOW, 10, 5, |_| true);
    let page = |path: &str| WantedPage {
        url: format!("https://www.irs.gov/{path}"),
        title: "Tax".into(),
        depth: 1,
        ..WantedPage::default()
    };
    let followed = |path: &str| Followed {
        domain: "irs.gov".into(),
        pages: vec![(page(path), 0)],
        ..Followed::default()
    };
    store.apply(&targets, vec![followed("limits")], NOW, |_| true);
    store.apply(&targets, vec![followed("brackets")], NOW + 1, |_| true);
    store.save().unwrap();
    store.prune(NOW + WANT_KEPT, 10);
    store.save().unwrap();
    let saved = std::fs::read_to_string(dir.path().join(SITES_FILE)).unwrap();
    assert!(saved.contains("bracket"));
    assert!(!saved.contains("contribution"));
    let saved = std::fs::read_to_string(dir.path().join(PAGES_FILE)).unwrap();
    assert!(saved.contains("/brackets"));
    assert!(!saved.contains("/limits"));
}
