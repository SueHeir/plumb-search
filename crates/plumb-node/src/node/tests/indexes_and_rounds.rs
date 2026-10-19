//! Starting a node: its settings, the records it serves, its indexes and
//! its crawl rounds.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_wikipedia_articles_with_the_sites() {
    let dir = seeded_dir();
    write_page_set(
        dir.path(),
        &[
            plumb_core::Article {
                title: "Marie Curie".into(),
                description: Some("Polish-French physicist and chemist".into()),
                views: 80_000,
                ..Default::default()
            },
            plumb_core::Article {
                title: "Chase Bank".into(),
                description: Some("American bank".into()),
                site: Some("chase.com".into()),
                views: 9_000,
                ..Default::default()
            },
        ],
    );
    let node = start(test_config(dir.path())).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;
    // The node asks its index which words many sites' titles have, so a
    // health search that names a site by a rare word lists it first.
    assert!(!node.inner.common_word("chase"));
    let deadline = Instant::now() + Duration::from_secs(60);
    let body = loop {
        let (code, _, body) = get(addr, "/search?q=marie+curie").await;
        assert_eq!(code, 200);
        if body.contains("Marie_Curie") || Instant::now() > deadline {
            break body;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        body.contains(
            "<li class=\"pg\"><a class=\"r\" href=\"https://en.wikipedia.org/wiki/Marie_Curie\""
        ),
        "{body}"
    );
    assert!(
        body.contains("Polish-French physicist and chemist"),
        "{body}"
    );

    // An article about a listed site goes under it, not in a place of its own.
    let (_, _, body) = get(addr, "/search?q=chase").await;
    assert!(!body.contains("class=\"pg\""), "{body}");
    assert!(
        body.contains(
            "<p class=\"sub\">Wikipedia: <a href=\"https://en.wikipedia.org/wiki/Chase_Bank\""
        ),
        "{body}"
    );
    let (_, _, json) = get(addr, "/api/search?q=marie+curie&full=1").await;
    let results: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(results["pages"][0]["item"]["title"], "Marie Curie");
    assert_eq!(results["pages"][0]["set"], "wikipedia-en");
    assert_eq!(results["pages"][0]["named"], true);

    // Turned off, the articles go.
    let mut settings = node.inner.settings();
    settings.page_sets = crate::pages::PageSets::parse("wikipedia-en=off").unwrap();
    node.inner.change_settings(settings).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, _, body) = get(addr, "/search?q=marie+curie").await;
        if !body.contains("Marie_Curie") {
            break;
        }
        assert!(Instant::now() < deadline, "the articles stayed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node.shutdown().await.unwrap();
}

#[test]
fn profiles() {
    let server = NodeConfig::server(PathBuf::from("/data"));
    assert_eq!(server.data_dir, PathBuf::from("/data"));
    assert_eq!(server.bind, "127.0.0.1:8080".parse().unwrap());
    assert_eq!(
        (server.sites, server.initial_crawl, server.crawl_per_refresh),
        (1_000_000, 10_000, 5_000)
    );
    assert_eq!(server.refresh_every, Some(Duration::from_secs(3600)));
    assert_eq!((server.cc_release.as_deref(), server.alpha), (None, None));
    assert!(!server.use_system_proxy);
    assert_eq!(server.sources, SeedSources::default());
    assert_eq!(server.sources.tranco_url, download::TRANCO_LATEST_URL);
    assert_eq!(server.retry_wait, Duration::from_secs(600));
    assert_eq!(server.max_retry_wait, Duration::from_secs(6 * 3600));
    server.check().unwrap();

    let desktop = NodeConfig::desktop(PathBuf::from("data"));
    assert_eq!(desktop.bind, "127.0.0.1:0".parse().unwrap());
    assert_eq!(
        (
            desktop.sites,
            desktop.initial_crawl,
            desktop.crawl_per_refresh
        ),
        (250_000, 2_000, 1_000)
    );
    assert_eq!(desktop.refresh_every, Some(Duration::from_secs(12 * 3600)));
    assert!(!desktop.use_system_proxy);
    assert!(desktop.search_by_meaning && !server.search_by_meaning);
    desktop.check().unwrap();
}

#[test]
fn common_crawl_urls() {
    let mut config = NodeConfig::server(PathBuf::from("d"));
    assert_eq!(config.cc_ranks_url(), None);
    config.cc_release = Some("cc-main-2025-26-nov-dec-jan".into());
    assert_eq!(
        config.cc_ranks_url(),
        Some(download::cc_domain_ranks_url("cc-main-2025-26-nov-dec-jan"))
    );
    config.sources.cc_ranks_url = Some("https://mirror.example/ranks.txt.gz".into());
    assert_eq!(
        config.cc_ranks_url().as_deref(),
        Some("https://mirror.example/ranks.txt.gz")
    );
}

/// Spoils one setting.
type Spoil = fn(&mut NodeConfig);

#[tokio::test]
async fn refuses_settings_that_cannot_work() {
    let dir = tempfile::tempdir().unwrap();
    let base = test_config(dir.path());
    let broken: [(&str, Spoil); 6] = [
        ("sites", |c| c.sites = 0),
        ("alpha", |c| c.alpha = Some(1.5)),
        ("release", |c| c.cc_release = Some("../../etc".into())),
        ("retry_wait", |c| c.retry_wait = Duration::ZERO),
        ("retry_wait", |c| c.max_retry_wait = Duration::from_secs(1)),
        ("refresh_every", |c| c.refresh_every = Some(Duration::ZERO)),
    ];
    for (what, spoil) in broken {
        let mut config = base.clone();
        spoil(&mut config);
        let err = start(config).await.unwrap_err();
        assert!(format!("{err:#}").contains(what), "{what}: {err:#}");
    }
    // Nothing was created for them.
    assert!(names(dir.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serves_a_records_file_put_there_by_hand() {
    let dir = seeded_dir();
    let node = start(test_config(dir.path())).await.unwrap();
    let addr = node.addr();
    assert_eq!(node.url(), format!("http://{addr}/"));
    assert_ne!(addr.port(), 0);

    let status = wait_for(addr, "the first index", ready_and_idle).await;
    assert_eq!(status.index.as_deref(), Some("000001"));
    assert_eq!(status.sites, fixture_sites().len() as u64);
    assert_eq!(status.last_error, None);
    assert_eq!(status.progress, None);
    assert!(status.last_refresh.is_some_and(|t| t <= now_unix()));
    assert_eq!(status.next_refresh, None, "refreshing is off");
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(node.status().index, status.index);

    let hits = search(addr, "us+bank").await;
    assert_eq!(hits[0].domain, "usbank.com");
    assert_eq!(search(addr, "chase").await[0].domain, "chase.com");

    let (code, head, body) = get(addr, "/").await;
    assert_eq!(code, 200);
    assert!(head.contains("content-security-policy: default-src 'none'"));
    assert!(body.contains("<form action=\"/search\""), "{body}");
    assert!(
        body.contains("items indexed &middot; updated just now"),
        "{body}"
    );
    assert!(!body.contains("http-equiv=\"refresh\""));
    let (code, _, body) = get(addr, "/search?q=us+bank").await;
    assert_eq!(code, 200);
    let first = body.split("<li>").nth(1).expect("a result");
    // A desktop node keeps search history, so results link through /go.
    assert!(
        first.contains("href=\"/go?q=us+bank&amp;d=usbank.com"),
        "{first}"
    );

    node.shutdown().await.unwrap();
    assert!(TcpStream::connect(addr).await.is_err(), "still listening");
    assert_eq!(
        names(dir.path()),
        [
            "activity.jsonl",
            // The search above, in its browser's history.
            "history",
            "indexes",
            "node.lock",
            "records.jsonl",
            "state.json"
        ]
    );
    assert_eq!(names(&dir.path().join("indexes")), ["000001"]);
    let saved = store::load_state(&store::Paths::new(dir.path())).unwrap();
    assert!(!saved.index_stale);
    assert_eq!(saved.crawl_left, 0);

    // A restart searches the same index at once, without building another.
    let node = start(test_config(dir.path())).await.unwrap();
    let status = node.status();
    assert_eq!(status.phase, Phase::Ready);
    assert_eq!(status.index.as_deref(), Some("000001"));
    let status = wait_for(node.addr(), "idle", |s| s.step == Step::Idle).await;
    assert_eq!(status.index.as_deref(), Some("000001"));
    assert_eq!(search(node.addr(), "us+bank").await[0].domain, "usbank.com");
    node.shutdown().await.unwrap();
    assert_eq!(names(&dir.path().join("indexes")), ["000001"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexes_what_an_interrupted_crawl_saved() {
    let dir = seeded_dir();
    let records = dir.path().join("records.jsonl");
    // A crawl that stopped before folding its journal into the records.
    let mut found = site::new("plumbline-example.com");
    found.title = Some("Plumbline Example Widgets".into());
    site::set_crawled(&mut found, Some(now_unix()));
    let mut store = crate::records::RecordStore::open(&records);
    store
        .save(&[crate::records::Change::Merge { site: found }])
        .unwrap();
    drop(store);

    let node = start(test_config(dir.path())).await.unwrap();
    let status = wait_for(node.addr(), "the first index", ready_and_idle).await;
    assert_eq!(status.sites, fixture_sites().len() as u64 + 1);
    let hits = search(node.addr(), "plumbline+example+widgets").await;
    assert_eq!(hits[0].domain, "plumbline-example.com");
    node.shutdown().await.unwrap();
    // Building the index folded it into the records file.
    assert!(!dir.path().join("records.jsonl.journal").exists());
    let sites = crate::records::load_sites(&records).unwrap();
    assert!(sites.contains("plumbline-example.com"));
}

/// A records file that holds a site on two lines, as one put together by
/// hand can, is merged once and written again with each site on one line;
/// the index is then built from it a record at a time, of the sites merged.
#[test]
fn an_index_build_merges_a_site_the_records_file_holds_twice() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.jsonl");
    let ranked = |domain: &str, tranco: u32, title: &str| {
        let mut record = site::new(domain);
        let mut signals = site::signals(&record);
        signals.tranco_rank = Some(tranco);
        site::set_signals(&mut record, &signals);
        record.title = Some(title.into());
        record
    };
    let mut again = site::new("chase.com");
    again.about = Some("Credit cards, mortgages and checking accounts".into());
    write_old_lines(
        &path,
        &[
            ranked("usbank.com", 1, "U.S. Bank"),
            ranked("chase.com", 2, "Chase Bank"),
            ranked("wellsfargo.com", 3, "Wells Fargo"),
            again,
        ],
    );
    // And a journal still to fold in.
    crate::records::RecordStore::open(&path)
        .save(&[crate::records::Change::Merge {
            site: ranked("pnc.com", 4, "PNC Bank"),
        }])
        .unwrap();
    assert!(
        crate::outline::outline_copy(&path, &dir.path().join("copy"))
            .unwrap()
            .is_none()
    );
    let config = test_config(dir.path());
    let rank = RankConfig::default();
    let opened = open_data_dir(&config, rank).unwrap();
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let inner = Inner::new(config, rank, opened, stopped, None);

    let built = {
        let _records = inner.hold_records();
        crate::node::worker::build_from_file(&inner).unwrap()
    };
    assert_eq!(built.docs, 4);
    let searcher = plumb_index::Searcher::open(&built.dir).unwrap();
    let chase = searcher.site("chase.com").unwrap().unwrap();
    assert_eq!(chase.title.as_deref(), Some("Chase Bank"));
    assert_eq!(
        chase.description.as_deref(),
        Some("Credit cards, mortgages and checking accounts")
    );
    assert!(searcher.has_domain("pnc.com"));
    // Each site once in the file now, the journal folded in.
    let outlines = crate::outline::outline(&path)
        .unwrap()
        .expect("each site once");
    let mut domains: Vec<&str> = outlines.iter().map(|o| &*o.domain).collect();
    domains.sort_unstable();
    assert_eq!(
        domains,
        ["chase.com", "pnc.com", "usbank.com", "wellsfargo.com"]
    );
    assert!(!crate::records::journal_path(&path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swaps_in_a_new_index_and_deletes_the_old_one_once_unused() {
    let dir = seeded_dir();
    let indexes = dir.path().join("indexes");
    let node = start(test_config(dir.path())).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;
    assert_eq!(names(&indexes), ["000001"]);

    // A search still running on the first index keeps it open.
    let in_flight = node.inner.current().unwrap();
    node.refresh_now();
    let status = wait_for(addr, "the second index", |s| {
        s.index.as_deref() == Some("000002") && s.step == Step::Idle
    })
    .await;
    assert_eq!(status.sites, in_flight.docs);
    assert_eq!(search(addr, "us+bank").await[0].domain, "usbank.com");
    assert_eq!(names(&indexes), ["000001", "000002"]);
    let hits = in_flight.backend().search("us bank", 1).unwrap();
    assert_eq!(hits[0].domain, "usbank.com");

    // Once the search is over, the old index goes at the next sweep, which
    // the node makes every minute while it waits.
    drop(in_flight);
    node.inner.sweep();
    assert_eq!(names(&indexes), ["000002"]);
    assert!(!node.inner.has_retired());

    // Without a search holding it, the replaced index goes right away.
    node.refresh_now();
    wait_for(addr, "the third index", |s| {
        s.index.as_deref() == Some("000003") && s.step == Step::Idle
    })
    .await;
    assert_eq!(names(&indexes), ["000003"]);
    node.shutdown().await.unwrap();

    let node = start(test_config(dir.path())).await.unwrap();
    assert_eq!(node.status().index.as_deref(), Some("000003"));
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falls_back_to_the_newest_index_that_opens_and_clears_leftovers() {
    let dir = seeded_dir();
    let paths = store::Paths::new(dir.path());
    let records = fixture_sites();
    // Two indexes of earlier runs, a newer one that is broken, and what
    // interrupted work leaves: a staging directory and temporary files.
    let sites = stored_items(&records);
    plumb_index::build_index(&paths.index(2), &sites[..5]).unwrap();
    plumb_index::build_index(&paths.index(4), &sites[..10]).unwrap();
    std::fs::create_dir_all(paths.index(7)).unwrap();
    std::fs::write(paths.index(7).join("meta.json"), "not an index").unwrap();
    std::fs::create_dir_all(paths.indexes.join(".000008.new-1-0")).unwrap();
    std::fs::create_dir_all(&paths.seed).unwrap();
    for leftover in [
        ".records.jsonl.1.tmp",
        ".state.json.1.tmp",
        "seed/x.txt.part",
    ] {
        std::fs::write(dir.path().join(leftover), "partial").unwrap();
    }
    let up_to_date = SavedState {
        crawl_left: 0,
        index_stale: false,
        last_refresh: Some(now_unix()),
        wikidata_missing: false,
        network_pending: 0,
        quick_start: false,
        ..SavedState::default()
    };
    store::save_state(&paths, &up_to_date).unwrap();

    let config = test_config(dir.path());
    let opened = open_data_dir(&config, crate::rank_config(None)).unwrap();
    let index = opened.index.as_ref().expect("an index");
    assert_eq!((index.id, index.docs), (4, 10));
    assert!(opened.leftover.is_empty());
    assert_eq!(names(&paths.indexes), ["000004"]);
    assert_eq!(names(&paths.seed), Vec::<String>::new());
    assert_eq!(
        names(dir.path()),
        [
            "indexes",
            "node.lock",
            "records.jsonl",
            "seed",
            "state.json"
        ]
    );
    // The records may hold changes that only the broken index had: the
    // next index is built from them, even if the node stops first.
    assert!(opened.saved.index_stale);
    assert!(store::load_state(&paths).unwrap().index_stale);
    drop(opened);

    let node = start(config).await.unwrap();
    let status = wait_for(node.addr(), "a rebuild", ready_and_idle).await;
    assert_eq!(status.index.as_deref(), Some("000005"));
    assert_eq!(status.sites, records.len() as u64);
    node.shutdown().await.unwrap();
    assert_eq!(names(&paths.indexes), ["000005"]);
    assert!(!store::load_state(&paths).unwrap().index_stale);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_node_per_data_directory() {
    let dir = seeded_dir();
    let first = start(test_config(dir.path())).await.unwrap();
    let err = start(test_config(dir.path())).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("in use by another Plumb node"),
        "{err:#}"
    );
    first.shutdown().await.unwrap();
    let second = start(test_config(dir.path())).await.unwrap();
    second.shutdown().await.unwrap();

    // A port that is taken fails the start, and leaves the directory free.
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = test_config(dir.path());
    config.bind = taken.local_addr().unwrap();
    let err = start(config).await.unwrap_err();
    assert!(format!("{err:#}").contains("listening on"), "{err:#}");
    start(test_config(dir.path()))
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn picks_up_a_round_left_unfinished() {
    let dir = recently_crawled_dir();
    let paths = store::Paths::new(dir.path());
    // Stopped in the middle of the initial crawl, before the index caught up.
    store::save_state(
        &paths,
        &SavedState {
            crawl_left: 700,
            index_stale: true,
            last_refresh: None,
            wikidata_missing: false,
            network_pending: 0,
            quick_start: false,
            ..SavedState::default()
        },
    )
    .unwrap();
    let mut config = test_config(dir.path());
    config.refresh_every = Some(Duration::from_secs(3600));
    let node = start(config).await.unwrap();
    let status = wait_for(node.addr(), "the end of the round", |s| {
        ready_and_idle(s) && s.last_refresh.is_some()
    })
    .await;
    let last = status.last_refresh.unwrap();
    assert_eq!(status.next_refresh, Some(last + 3600));
    node.shutdown().await.unwrap();
    assert_eq!(
        store::load_state(&paths).unwrap(),
        SavedState {
            crawl_left: 0,
            index_stale: false,
            last_refresh: Some(last),
            wikidata_missing: false,
            network_pending: 0,
            quick_start: false,
            ..SavedState::default()
        }
    );
    assert_eq!(names(&paths.indexes), ["000001"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_only_crawls_builds_no_index_until_it_searches_again() {
    let dir = recently_crawled_dir();
    let paths = store::Paths::new(dir.path());
    store::save_state(
        &paths,
        // Records newer than any index, as after a crawl.
        &SavedState {
            crawl_left: 700,
            index_stale: true,
            ..SavedState::default()
        },
    )
    .unwrap();
    let mut config = test_config(dir.path());
    config.crawl_only = true;
    config.search_by_meaning = true;
    config.refresh_every = Some(Duration::from_secs(3600));
    let node = start(config).await.unwrap();
    assert!(!node.inner.config.search_by_meaning, "turned off");
    let status = wait_for(node.addr(), "the end of the round", |s| {
        s.step == Step::Idle && s.last_refresh.is_some()
    })
    .await;
    assert_eq!(status.phase, Phase::SettingUp, "nothing to search");
    assert_eq!(status.index, None);
    assert_eq!(status.last_error, None);
    assert_eq!(
        node.inner.held_sites(),
        fixture_sites().len() as u64,
        "counted for filling"
    );
    node.shutdown().await.unwrap();
    assert!(names(&paths.indexes).is_empty(), "no index built");
    let saved = store::load_state(&paths).unwrap();
    assert_eq!(saved.crawl_left, 0);
    assert!(saved.index_stale, "still newer than any index");

    // Without the flag again, the node builds its index.
    let node = start(test_config(dir.path())).await.unwrap();
    let status = wait_for(node.addr(), "the index", ready_and_idle).await;
    assert_eq!(status.sites, fixture_sites().len() as u64);
    assert_eq!(search(node.addr(), "chase").await[0].domain, "chase.com");
    node.shutdown().await.unwrap();
}

#[test]
fn crawling_only_turns_off_what_only_searching_needs() {
    let mut config = NodeConfig::server(PathBuf::from("d"));
    config.crawl_only = true;
    config.search_by_meaning = true;
    config.private_search = true;
    config.network = Some(plumb_net::NetConfig::new(PathBuf::from("d/net")));
    config.limit_to_crawling();
    assert!(!config.search_by_meaning && !config.private_search);
    assert_eq!(config.news_feeds, 0);
    let net = config.network.unwrap();
    assert!(!net.answer_searches && !net.follow_crawls);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refreshes_when_due() {
    let dir = recently_crawled_dir();
    let paths = store::Paths::new(dir.path());
    let mut config = test_config(dir.path());
    config.refresh_every = Some(Duration::from_secs(3600));
    config.crawl_per_refresh = 100;
    // The first start builds an index and starts the refresh clock.
    let node = start(config.clone()).await.unwrap();
    let first = wait_for(node.addr(), "the first index", ready_and_idle).await;
    node.shutdown().await.unwrap();
    let started = first.last_refresh.unwrap();
    assert_eq!(first.next_refresh, Some(started + 3600));

    // Back after a long break: the refresh is overdue and runs at once.
    // Every homepage was crawled a moment ago, so there is nothing to fetch
    // and nothing to rebuild.
    let long_ago = started - 2 * 3600;
    store::save_state(
        &paths,
        &SavedState {
            crawl_left: 0,
            index_stale: false,
            last_refresh: Some(long_ago),
            wikidata_missing: false,
            network_pending: 0,
            quick_start: false,
            ..SavedState::default()
        },
    )
    .unwrap();
    let node = start(config).await.unwrap();
    let status = wait_for(node.addr(), "the refresh", |s| {
        ready_and_idle(s) && s.last_refresh > Some(long_ago)
    })
    .await;
    assert_eq!(status.index.as_deref(), Some("000001"));
    node.shutdown().await.unwrap();
    let saved = store::load_state(&paths).unwrap();
    assert_eq!((saved.crawl_left, saved.index_stale), (0, false));
}
