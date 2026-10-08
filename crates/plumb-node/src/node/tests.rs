//! Tests of a whole node: started on a loopback port with a temporary data
//! directory and driven over HTTP. Nothing here reaches the internet: seed
//! data comes from the fixtures, served by a stand-in host on a loopback
//! port, and the tests that run a node with records leave nothing to crawl.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plumb_core::{now_unix, read_jsonl, write_jsonl, SiteRecord};
use plumb_index::Hit;
use plumb_ingest::{
    kind_sites, load_cc_domain_ranks, load_tranco, load_wikidata_official_sites, parse_wat,
    Builder, WatExtract,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::store::{self, SavedState};
use super::*;
use crate::meaning::MeaningModel;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// Site records from every fixture, as `plumb ingest` makes them.
fn fixture_records() -> Vec<SiteRecord> {
    let mut builder = Builder::new();
    builder.add_tranco(&load_tranco(&fixture("tranco.csv"), None).unwrap());
    builder.add_cc_ranks(&load_cc_domain_ranks(&fixture("cc-domain-ranks.txt"), None).unwrap());
    let mut extract = WatExtract::new();
    parse_wat(&fixture("sample.wat"), &mut extract).unwrap();
    builder.add_wat(&extract);
    builder.add_official_sites(
        &load_wikidata_official_sites(&fixture("wikidata-official-sites.tsv")).unwrap(),
    );
    builder.finish(None)
}

/// A data directory holding the fixture records, as if put there by hand.
fn seeded_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_jsonl(&dir.path().join("records.jsonl"), &fixture_records()).unwrap();
    dir
}

/// `http://127.0.0.1:<port>` of a port that was free a moment ago, so
/// that nothing answers there.
fn closed_port() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", listener.local_addr().unwrap())
}

/// A node that crawls nothing and never refreshes, on a free loopback
/// port, whose seed sources lead nowhere (no setup should need them).
fn test_config(dir: &Path) -> NodeConfig {
    let mut config = NodeConfig::desktop(dir.to_path_buf());
    config.initial_crawl = 0;
    config.refresh_every = None;
    config.crawl_per_refresh = 0;
    config.crawl_home_site = false;
    // No feed of a test site is ever fetched.
    config.news_feeds = 0;
    // As if "Set up my node" was answered, so filling goes ahead.
    config.settings.setup_chosen = true;
    let nowhere = closed_port();
    config.sources = SeedSources {
        tranco_url: format!("{nowhere}/tranco.csv"),
        wikidata_sparql_url: format!("{nowhere}/sparql"),
        wikipedia_api_url: format!("{nowhere}/w/api.php"),
        wikidata_min_sitelinks: 25,
        wikidata_pacing: quick_wikidata(),
        cc_ranks_url: None,
        model_base_url: format!("{nowhere}/model/"),
        gemma_downloads: Vec::new(),
        adult_list_url: None,
    };
    config
}

/// No pauses between Wikidata queries, and a moment's wait before a retry.
fn quick_wikidata() -> download::WikidataPacing {
    download::WikidataPacing {
        pause: Duration::ZERO,
        retry_wait: Duration::from_millis(1),
    }
}

/// The names in `dir`, sorted; nothing when it does not exist.
fn names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A plain HTTP/1.1 GET: the status code, the head (lowercased) and the body.
async fn get(addr: SocketAddr, path: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let code = head.split(' ').nth(1).unwrap().parse().unwrap();
    (code, head.to_ascii_lowercase(), body.to_string())
}

async fn get_status(addr: SocketAddr) -> Status {
    let (code, head, body) = get(addr, "/api/status").await;
    assert_eq!(code, 200, "{body}");
    assert!(head.contains("content-type: application/json"), "{head}");
    assert!(head.contains("cache-control: no-store"), "{head}");
    serde_json::from_str(&body).unwrap()
}

/// Polls `/api/status` until `done` holds; fails after a minute.
async fn wait_for(addr: SocketAddr, what: &str, done: impl Fn(&Status) -> bool) -> Status {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = get_status(addr).await;
        if done(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; the node says {status:#?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn ready_and_idle(status: &Status) -> bool {
    status.phase == Phase::Ready && status.step == Step::Idle
}

async fn search(addr: SocketAddr, query: &str) -> Vec<Hit> {
    let (code, _, body) = get(addr, &format!("/api/search?q={query}")).await;
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// Puts an English Wikipedia page set in `data_dir`.
fn write_page_set(data_dir: &Path, articles: &[plumb_core::Article]) {
    let file = crate::pages::SetInfo::find("wikipedia-en")
        .unwrap()
        .file(data_dir);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let mut text = plumb_core::article::ARTICLES_HEADER.as_bytes().to_vec();
    for article in articles {
        plumb_core::article::write_article(&mut text, article).unwrap();
    }
    std::fs::write(file, text).unwrap();
}

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
    let results: SearchResults = serde_json::from_str(&json).unwrap();
    assert_eq!(results.pages[0].hit.page.title, "Marie Curie");
    assert!(results.pages[0].hit.named);

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
    assert_eq!(status.sites, fixture_records().len() as u64);
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
        body.contains("sites indexed &middot; updated just now"),
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
    let mut found = SiteRecord::new("plumbline-example.com");
    found.title = Some("Plumbline Example Widgets".into());
    found.crawled_at = Some(now_unix());
    let mut store = crate::records::RecordStore::open(&records);
    store
        .save(&[crate::records::Change::Merge { record: found }])
        .unwrap();
    drop(store);

    let node = start(test_config(dir.path())).await.unwrap();
    let status = wait_for(node.addr(), "the first index", ready_and_idle).await;
    assert_eq!(status.sites, fixture_records().len() as u64 + 1);
    let hits = search(node.addr(), "plumbline+example+widgets").await;
    assert_eq!(hits[0].domain, "plumbline-example.com");
    node.shutdown().await.unwrap();
    // Building the index folded it into the records file.
    assert!(!dir.path().join("records.jsonl.journal").exists());
    let set = crate::records::load_records(&records).unwrap();
    assert!(set.get("plumbline-example.com").is_some());
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
    let records = fixture_records();
    // Two indexes of earlier runs, a newer one that is broken, and what
    // interrupted work leaves: a staging directory and temporary files.
    plumb_index::build_index(&paths.index(2), &records[..5]).unwrap();
    plumb_index::build_index(&paths.index(4), &records[..10]).unwrap();
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

/// Records like the fixtures, all crawled a moment ago, so no homepage is
/// due for a crawl and a round of crawling has nothing to fetch.
fn recently_crawled_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let now = now_unix();
    let records: Vec<SiteRecord> = fixture_records()
        .into_iter()
        .map(|mut record| {
            record.crawl_attempted_at = Some(now);
            record
        })
        .collect();
    write_jsonl(&dir.path().join("records.jsonl"), &records).unwrap();
    dir
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
        fixture_records().len() as u64,
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
    assert_eq!(status.sites, fixture_records().len() as u64);
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

/// A stand-in for the seed data hosts on a loopback port: every request is
/// answered by `respond` for its `METHOD /path` and how many times that was
/// asked before (from 1), and remembered.
struct SeedHost {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

type Respond = dyn Fn(&str, usize) -> Vec<u8> + Send + Sync;

impl SeedHost {
    async fn start(respond: impl Fn(&str, usize) -> Vec<u8> + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let respond: Arc<Respond> = Arc::new(respond);
        let log = Arc::clone(&requests);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(answer(socket, Arc::clone(&respond), Arc::clone(&log)));
            }
        });
        SeedHost { base, requests }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn sources(&self) -> SeedSources {
        SeedSources {
            tranco_url: self.url("/tranco.csv"),
            wikidata_sparql_url: self.url("/sparql"),
            wikipedia_api_url: self.url("/w/api.php"),
            wikidata_min_sitelinks: 25,
            wikidata_pacing: quick_wikidata(),
            cc_ranks_url: None,
            model_base_url: self.url("/model/"),
            gemma_downloads: Vec::new(),
            adult_list_url: None,
        }
    }

    /// How many times each `METHOD /path` was asked for.
    fn counts(&self) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for request in self.requests.lock().unwrap().iter() {
            *counts.entry(request.clone()).or_default() += 1;
        }
        counts
    }
}

/// Reads one request (head and body) and writes the response.
async fn answer(mut socket: TcpStream, respond: Arc<Respond>, log: Arc<Mutex<Vec<String>>>) {
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        match socket.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).to_string();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    // Read the whole body: closing with unread data would reset the connection.
    while request.len() < head_end + length {
        match socket.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    }
    let mut words = head.split_whitespace();
    let key = format!(
        "{} {}",
        words.next().unwrap_or(""),
        words.next().unwrap_or("")
    );
    let nth = {
        let mut log = log.lock().unwrap();
        log.push(key.clone());
        log.iter().filter(|k| **k == key).count()
    };
    let response = respond(&key, nth);
    let _ = socket.write_all(&response).await;
    let _ = socket.shutdown().await;
}

fn http(status: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// The fixture's Wikidata sites as the SPARQL endpoint returns them.
fn sparql_json(tsv: &Path) -> Vec<u8> {
    let text = std::fs::read_to_string(tsv).unwrap();
    let bindings: Vec<serde_json::Value> = text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let (item, label, website) = (fields.next()?, fields.next()?, fields.next()?);
            Some(serde_json::json!({
                "item": {"type": "uri", "value": item},
                "itemLabel": {"type": "literal", "value": label},
                "website": {"type": "uri", "value": website},
            }))
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "head": {"vars": ["item", "itemLabel", "website"]},
        "results": {"bindings": bindings},
    }))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sets_up_from_the_seed_data_and_retries_after_a_failure() {
    let tranco = std::fs::read(fixture("tranco.csv")).unwrap();
    let sparql = sparql_json(&fixture("wikidata-official-sites.tsv"));
    // Served as plain text: the download reads plain or gzipped files (the
    // gzipped kind is tested in plumb-ingest).
    let ranks = std::fs::read(fixture("cc-domain-ranks.txt")).unwrap();
    let host = SeedHost::start(move |request, nth| match request {
        // The Tranco list fails once, which fails the setup.
        "GET /tranco.csv" if nth == 1 => http("503 Service Unavailable", b"busy"),
        "GET /tranco.csv" => http("200 OK", &tranco),
        // Wikidata's first query times out once; the download tries again.
        "POST /sparql" if nth == 1 => http("504 Gateway Timeout", b"Query timeout"),
        "POST /sparql" => http("200 OK", &sparql),
        "GET /graph/x-domain-ranks.txt.gz" => http("200 OK", &ranks),
        _ => http("404 Not Found", b"no such file"),
    })
    .await;

    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config(dir.path());
    config.sources = host.sources();
    config.sources.cc_ranks_url = Some(host.url("/graph/x-domain-ranks.txt.gz"));
    config.sites = 50;
    config.retry_wait = Duration::from_millis(100);
    config.max_retry_wait = Duration::from_secs(1);
    let node = start(config).await.unwrap();
    let addr = node.addr();

    let status = wait_for(addr, "setup", ready_and_idle).await;
    assert_eq!(status.sites, 50);
    assert_eq!(status.last_error, None, "cleared by the retry that worked");
    assert!(!status.wikidata_missing);
    assert_eq!(status.wikidata_error, None);
    let hits = search(addr, "us+bank").await;
    assert_eq!(hits[0].domain, "usbank.com");
    node.shutdown().await.unwrap();

    // What worked the first time was not downloaded again. Wikidata was
    // asked once for each band of sitelink counts, and once more after
    // the time out.
    let counts = host.counts();
    assert_eq!(counts["GET /tranco.csv"], 2, "{counts:?}");
    // Then once per kind of organization, and once for the facts of the
    // official websites.
    assert_eq!(
        counts["POST /sparql"],
        download::wikidata_sitelink_bands(25).len() + 1 + kind_sites::KIND_LABELS.len() + 1,
        "{counts:?}"
    );
    assert_eq!(counts["GET /graph/x-domain-ranks.txt.gz"], 1, "{counts:?}");
    assert_eq!(counts.len(), 3, "{counts:?}");

    let seed = dir.path().join("seed");
    assert_eq!(
        names(&seed),
        [
            "tranco-top-1m.csv.zip",
            "wikidata-kind-sites.tsv",
            "wikidata-official-sites.tsv",
            "wikidata-site-facts.tsv",
            // Empty: the facts here name no sitelinks, so no item is well
            // enough known to ask Wikipedia about.
            "wikipedia-intros.tsv",
            "x-domain-ranks-top50.txt"
        ]
    );
    let records: Vec<SiteRecord> = read_jsonl(&dir.path().join("records.jsonl")).unwrap();
    assert_eq!(records.len(), 50);
    let usbank = records.iter().find(|r| r.domain == "usbank.com").unwrap();
    assert!(usbank.signals.official_site);
    assert!(usbank.signals.tranco_rank.is_some());
    assert!(usbank.signals.harmonic_rank.is_some());
    // The quick index of the Tranco list, then the one of all the seed data.
    assert_eq!(names(&dir.path().join("indexes")), ["000002"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shows_setup_progress_and_errors_while_downloads_fail() {
    let host = SeedHost::start(|_, _| {
        http(
            "503 Service Unavailable",
            b"<script>alert('pwned')</script> & <b>down</b>",
        )
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config(dir.path());
    config.sources = host.sources();
    let node = start(config).await.unwrap();
    let addr = node.addr();

    let status = wait_for(addr, "a failed download", |s| s.last_error.is_some()).await;
    assert_eq!(status.phase, Phase::SettingUp);
    assert_eq!(status.step, Step::Retrying);
    assert_eq!((status.sites, status.index), (0, None));
    let err = status.last_error.unwrap();
    assert!(err
        .message
        .starts_with("could not download the seed data:\nthe Tranco list:"));
    assert!(
        err.retry_at.unwrap() >= err.at + 590,
        "the first retry is 10 minutes out"
    );

    for path in ["/", "/search?q=us+bank", "/search?q=%3Cscript%3E"] {
        let (code, head, body) = get(addr, path).await;
        assert_eq!(code, 200, "{path}");
        assert!(head.contains("content-security-policy: default-src 'none'"));
        assert!(head.contains("cache-control: no-store"));
        assert!(
            body.contains("<meta http-equiv=\"refresh\" content=\"5\">"),
            "{body}"
        );
        assert!(body.contains("Setting up your search engine"), "{body}");
        assert!(body.contains("Something went wrong"), "{body}");
        assert!(body.contains("Plumb will try again in "), "{body}");
        assert!(!body.contains("<script"), "{body}");
        assert!(!body.contains("<b>"), "{body}");
        assert!(!body.contains("<form"), "{body}");
    }
    let (code, head, body) = get(addr, "/api/search?q=us+bank").await;
    assert_eq!(code, 503);
    assert!(head.contains("retry-after: 5"), "{head}");
    assert!(body.contains("not ready"), "{body}");

    // Shutting down does not wait out the ten minutes before the retry.
    let stopping = Instant::now();
    node.shutdown().await.unwrap();
    assert!(stopping.elapsed() < Duration::from_secs(5));
    // Failed downloads leave nothing behind.
    assert!(names(&dir.path().join("seed")).is_empty());
    assert!(!dir.path().join("records.jsonl").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stops_promptly_in_the_middle_of_a_download() {
    // A seed host that takes connections and never answers.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let host = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config(dir.path());
    config.sources.tranco_url = format!("{base}/tranco.csv");
    let node = start(config).await.unwrap();
    let status = wait_for(node.addr(), "the download", |s| s.step == Step::Downloading).await;
    assert_eq!(status.phase, Phase::SettingUp);
    assert_eq!(
        status.progress,
        Some(Progress {
            done: 0,
            total: 1,
            unit: "files".into()
        })
    );
    let (code, _, body) = get(node.addr(), "/").await;
    assert_eq!(code, 200);
    assert!(body.contains("Downloading the Tranco list"), "{body}");
    assert!(
        body.contains("<progress value=\"0\" max=\"1\"></progress>"),
        "{body}"
    );
    assert!(body.contains("0 of 1 files"), "{body}");

    let stopping = Instant::now();
    tokio::time::timeout(Duration::from_secs(10), node.shutdown())
        .await
        .expect("shutdown does not wait for the download")
        .unwrap();
    assert!(stopping.elapsed() < Duration::from_secs(5));
    host.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stops_promptly_in_the_middle_of_the_model_download() {
    // A model host that sends the start of a file and then nothing.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let host = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\n{")
                .await;
            held.push(socket);
        }
    });
    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    config.search_by_meaning = true;
    config.sources.model_base_url = format!("{base}/model/");
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "the first index", ready_and_idle).await;
    let part = dir.path().join("model/config.json.part");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !part.is_file() {
        assert!(
            Instant::now() < deadline,
            "the model download did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let stopping = Instant::now();
    tokio::time::timeout(Duration::from_secs(20), node.shutdown())
        .await
        .expect("shutdown does not wait for the model download")
        .unwrap();
    assert!(stopping.elapsed() < Duration::from_secs(15));
    host.abort();

    // The next start clears the partial file away.
    let node = start(test_config(dir.path())).await.unwrap();
    wait_for(node.addr(), "a restart", ready_and_idle).await;
    assert!(!part.exists());
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sets_up_without_wikidata_and_adds_it_later() {
    let tranco = std::fs::read(fixture("tranco.csv")).unwrap();
    let sparql = sparql_json(&fixture("wikidata-official-sites.tsv"));
    let wikidata_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let up = Arc::clone(&wikidata_up);
    let host = SeedHost::start(move |request, _| match request {
        "GET /tranco.csv" => http("200 OK", &tranco),
        "POST /sparql" if up.load(std::sync::atomic::Ordering::SeqCst) => http("200 OK", &sparql),
        "POST /sparql" => http("403 Forbidden", b"blocked"),
        _ => http("404 Not Found", b"no such file"),
    })
    .await;

    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config(dir.path());
    config.sources = host.sources();
    config.sites = 50;
    config.retry_wait = Duration::from_millis(300);
    config.max_retry_wait = Duration::from_secs(1);
    let node = start(config).await.unwrap();
    let addr = node.addr();

    // Setup goes ahead with the Tranco list alone.
    let status = wait_for(addr, "setup without Wikidata", |s| {
        s.phase == Phase::Ready && s.wikidata_error.is_some()
    })
    .await;
    assert_eq!(status.sites, 50);
    assert!(status.wikidata_missing);
    assert_eq!(status.last_error, None, "{status:#?}");
    let err = status.wikidata_error.unwrap();
    assert!(err.message.contains("HTTP 403"), "{}", err.message);
    assert!(err.retry_at.is_some());
    let records: Vec<SiteRecord> = read_jsonl(&dir.path().join("records.jsonl")).unwrap();
    assert_eq!(records.len(), 50);
    assert!(records.iter().all(|r| !r.signals.official_site));
    let paths = store::Paths::new(dir.path());
    let saved = store::load_state(&paths).unwrap();
    assert!(saved.wikidata_missing);
    assert!(!saved.quick_start, "the other seed files are folded in");
    let (code, _, body) = get(addr, "/").await;
    assert_eq!(code, 200);
    assert!(body.contains("could not be downloaded yet"), "{body}");
    let (_, _, body) = get(addr, "/api/status").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["wikidata_missing"], true);
    assert!(json["wikidata_error"]["message"].is_string(), "{json}");

    // Wikidata comes back: its sites are folded in and the index rebuilt.
    wikidata_up.store(true, std::sync::atomic::Ordering::SeqCst);
    let status = wait_for(addr, "Wikidata's sites", |s| {
        ready_and_idle(s) && !s.wikidata_missing
    })
    .await;
    assert_eq!(status.wikidata_error, None);
    assert_ne!(status.index.as_deref(), Some("000001"));
    let (_, _, body) = get(addr, "/").await;
    assert!(!body.contains("Wikidata"), "{body}");
    node.shutdown().await.unwrap();

    assert!(!store::load_state(&paths).unwrap().wikidata_missing);
    let records: Vec<SiteRecord> = read_jsonl(&dir.path().join("records.jsonl")).unwrap();
    assert!(records.len() >= 50, "{}", records.len());
    let usbank = records.iter().find(|r| r.domain == "usbank.com").unwrap();
    assert!(usbank.signals.official_site);
    assert!(usbank.signals.tranco_rank.is_some());
    assert!(
        usbank.aliases.iter().any(|alias| alias == "U.S. Bancorp"),
        "{:?}",
        usbank.aliases
    );
    assert_eq!(names(&dir.path().join("indexes")).len(), 1);

    // Records made before a change to how seed data is read are folded
    // again on request, from the seed files on disk, without downloading.
    let mut records: Vec<SiteRecord> = read_jsonl(&dir.path().join("records.jsonl")).unwrap();
    for record in &mut records {
        record.signals.official_site = false;
        record.aliases.clear();
    }
    write_jsonl(&dir.path().join("records.jsonl"), &records).unwrap();
    assert!(request_reseed(dir.path()).unwrap());
    assert!(store::load_state(&paths).unwrap().wikidata_missing);
    let offline = SeedHost::start(|_, _| http("500 Internal Server Error", b"offline")).await;
    let mut config = test_config(dir.path());
    config.sources = offline.sources();
    config.sites = 50;
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "the seed folded again", |s| {
        ready_and_idle(s) && !s.wikidata_missing
    })
    .await;
    node.shutdown().await.unwrap();
    assert_eq!(offline.requests.lock().unwrap().len(), 0);
    let records: Vec<SiteRecord> = read_jsonl(&dir.path().join("records.jsonl")).unwrap();
    let usbank = records.iter().find(|r| r.domain == "usbank.com").unwrap();
    assert!(usbank.signals.official_site);
    assert!(usbank.aliases.iter().any(|alias| alias == "U.S. Bancorp"));

    // Records made before the lists of sites on subdomains are folded
    // again too, from the seed files however old: Google Scholar, folded
    // into google.com then, gets a record of its own, and a college whose
    // website was misread from its email address is taken off google.com.
    let wikidata = dir.path().join("seed").join(download::WIKIDATA_FILE_NAME);
    let mut tsv = std::fs::read_to_string(&wikidata).unwrap();
    tsv.push_str(
        "http://www.wikidata.org/entity/Q90000099\tGoogle Scholar\thttps://scholar.google.com/\n",
    );
    tsv.push_str(
        "http://www.wikidata.org/entity/Q90000098\tCOE, Moro\thttps://mailto:coe@google.com\n",
    );
    std::fs::write(&wikidata, tsv).unwrap();
    let mut records: Vec<SiteRecord> = read_jsonl(&dir.path().join("records.jsonl")).unwrap();
    let google = records
        .iter_mut()
        .find(|r| r.domain == "google.com")
        .unwrap();
    google.aliases.push("Google Scholar".into());
    google.aliases.insert(0, "COE, Moro".into());
    google.kinds.push("college".into());
    write_jsonl(&dir.path().join("records.jsonl"), &records).unwrap();
    let mut saved = store::load_state(&paths).unwrap();
    saved.sites_version = 0;
    store::save_state(&paths, &saved).unwrap();
    let index_before = names(&dir.path().join("indexes"));
    let mut config = test_config(dir.path());
    config.sources = offline.sources();
    config.sites = 50;
    let node = start(config).await.unwrap();
    wait_for(
        node.addr(),
        "the seed folded for the subdomain sites",
        |s| {
            ready_and_idle(s)
                && s.index
                    .as_ref()
                    .is_some_and(|index| !index_before.contains(index))
        },
    )
    .await;
    node.shutdown().await.unwrap();
    assert_eq!(offline.requests.lock().unwrap().len(), 0);
    assert_eq!(
        store::load_state(&paths).unwrap().sites_version,
        plumb_core::SITES_VERSION
    );
    // The changes went into the journal, which the index build folded into
    // the records a record at a time.
    assert!(!crate::records::journal_path(&dir.path().join("records.jsonl")).exists());
    let check = || {
        let set = crate::records::load_records(&dir.path().join("records.jsonl")).unwrap();
        let scholar = set.get("scholar.google.com").unwrap();
        assert_eq!(scholar.aliases, ["Google Scholar"]);
        assert_eq!(scholar.signals.tranco_rank, Some(1));
        let google = set.get("google.com").unwrap();
        assert_eq!(google.aliases, ["Google"]);
        assert!(!google.kinds.iter().any(|kind| kind == "college"));
        assert!(google.signals.official_site);
        // Hacker News needs a record for ycombinator.com, which this has not.
        assert!(set.get("news.ycombinator.com").is_none());
        set.len()
    };
    let sites = check();

    // A node stopped before it noted the version makes the same changes
    // again on its next start, which leaves the records as they were.
    let mut saved = store::load_state(&paths).unwrap();
    saved.sites_version = 0;
    store::save_state(&paths, &saved).unwrap();
    let index_before = names(&dir.path().join("indexes"));
    let mut config = test_config(dir.path());
    config.sources = offline.sources();
    config.sites = 50;
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "the records updated again", |s| {
        ready_and_idle(s)
            && s.index
                .as_ref()
                .is_some_and(|index| !index_before.contains(index))
    })
    .await;
    node.shutdown().await.unwrap();
    assert_eq!(check(), sites);

    // Nothing to fold in a directory with no node yet.
    assert!(!request_reseed(tempfile::tempdir().unwrap().path()).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_in_the_network_takes_in_other_nodes_crawls_and_searches_them() {
    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    config.network = Some(net);
    // Other nodes' crawls add sites only with new sites taken.
    config.take_new_sites = true;
    let node = start(config).await.unwrap();
    let addr = node.addr();
    let status = wait_for(addr, "the first index", ready_and_idle).await;
    let net_status = status.network.expect("the node joined the network");
    let node_addr: plumb_net::Multiaddr = net_status.listening[0].parse().unwrap();
    let node_addr = node_addr
        .with_p2p(net_status.peer_id.parse().unwrap())
        .unwrap();
    assert!(dir.path().join("net/node.key").is_file());

    // Two other nodes crawled a site this one has never heard of.
    let peer_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let second_id = plumb_net::load_or_create_key(&second_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let now = now_unix();
    let domain = (0..)
        .map(|i| format!("lighthouse-keepers-{i}.org"))
        .find(|d| {
            [peer_id, second_id].iter().all(|p| {
                plumb_net::assign::is_assigned(
                    plumb_net::assign::epoch_of(now),
                    p,
                    d,
                    plumb_net::assign::MAX_SHARE_PPM,
                )
            })
        })
        .unwrap();
    let mut crawled = SiteRecord::new(domain.as_str());
    crawled.url = Some(format!("https://{domain}/"));
    crawled.title = Some("Lighthouse Keepers Guild".to_string());
    crawled.crawled_at = Some(now);
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.search_scope = plumb_net::SearchScope::Anyone;
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    peer_config.bootstrap = vec![node_addr.clone()];
    // The peer trusts the node, so it may fill its space from it.
    peer_config.trusted_peers = vec![net_status.peer_id.parse().unwrap()];
    let table = plumb_net::BucketTable::build(&peer_dir.path().join("buckets"), &[crawled.clone()])
        .unwrap();
    let (peer, _records) = plumb_net::start(peer_config, Arc::new(table))
        .await
        .unwrap();
    wait_for(addr, "the peer to connect", |s| {
        s.network.as_ref().is_some_and(|n| n.connected_peers >= 1)
    })
    .await;

    // The node asks the peer.
    let mut found = None;
    for _ in 0..100 {
        let (code, _, body) = get(addr, "/api/network/search?q=lighthouse").await;
        assert_eq!(code, 200, "{body}");
        let result: serde_json::Value = serde_json::from_str(&body).unwrap();
        if !result["hits"].as_array().unwrap().is_empty() {
            found = Some(result);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let found = found.expect("the peer answers");
    assert_eq!(found["hits"][0]["domain"], domain.as_str());
    assert_eq!(found["buckets"], plumb_net::bucket::BUCKETS_PER_SEARCH);
    assert_eq!(
        found["hits"][0]["verified"], false,
        "the peer has not published yet"
    );
    let (code, _, body) = get(addr, "/network?q=lighthouse").await;
    assert_eq!(code, 200);
    assert!(body.contains("Lighthouse Keepers Guild"), "{body}");
    let (_, _, body) = get(addr, "/search?q=us+bank").await;
    assert!(body.contains("name=\"net\" value=\"1\">"), "{body}");
    assert!(body.contains("href=\"/search?q=us+bank"), "{body}");
    // With the network setting on, the peer's site joins this node's, tinted.
    let (code, _, body) = get(addr, "/search?q=lighthouse&net=1").await;
    assert_eq!(code, 200);
    assert!(body.contains("name=\"net\" value=\"1\" checked>"), "{body}");
    assert!(
        body.contains("From this site's index and saved Plumb results"),
        "{body}"
    );
    assert!(body.contains("<li class=\"net\">"), "{body}");
    assert!(body.contains("Lighthouse Keepers Guild"), "{body}");
    // The buckets were kept from the first search: the network was not
    // asked again.
    assert!(body.contains("saved Plumb results, read locally"), "{body}");

    // The node hands its sites, best-ranked first, to a node filling up.
    let page = loop {
        if let Some(page) = peer.fill(None, 0, 50, false).await.unwrap() {
            break page;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(page.total, status.sites, "{page:?}");
    assert!(page.done() && !page.busy, "{page:?}");

    // And the other way round: the node serves the buckets of its index.
    assert!(dir
        .path()
        .join("indexes/000001/buckets/buckets.idx")
        .is_file());
    let from_node = peer
        .search("us bank", Duration::from_secs(5))
        .await
        .unwrap();
    // With one node to ask, every bucket goes to it.
    assert_eq!(
        from_node.asked,
        plumb_net::BUCKETS_PER_SEARCH,
        "{from_node:?}"
    );
    assert_eq!(from_node.answered, from_node.asked, "{from_node:?}");
    assert!(
        from_node
            .found
            .iter()
            .any(|f| f.record.domain == "usbank.com"),
        "{from_node:?}"
    );

    // The peer publishes its crawl; the node holds it until a second
    // crawler agrees, then keeps it and, at its next refresh, searches it
    // from its own index.
    let mut published = None;
    for _ in 0..100 {
        published = peer.publish(vec![crawled.clone()]).await.unwrap();
        let (_, _, body) = get(addr, "/api/status").await;
        let status: serde_json::Value = serde_json::from_str(&body).unwrap();
        if status["network"]["agreement"]["pending_sites"] == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(published.is_some());
    assert!(
        !dir.path().join("net/inbox.jsonl").exists(),
        "one crawler is not enough"
    );
    let second_config = {
        let mut c = plumb_net::NetConfig::new(second_dir.path().to_path_buf());
        c.search_scope = plumb_net::SearchScope::Anyone;
        c.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
        c.upnp = false;
        c.local_discovery = false;
        c.round_every = None;
        c.bootstrap = vec![node_addr.clone()];
        c
    };
    let empty =
        plumb_net::BucketTable::build(&second_dir.path().join("buckets"), &[] as &[SiteRecord])
            .unwrap();
    let (second, _records) = plumb_net::start(second_config, Arc::new(empty))
        .await
        .unwrap();
    for _ in 0..100 {
        second.publish(vec![crawled.clone()]).await.unwrap();
        if dir.path().join("net/inbox.jsonl").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        dir.path().join("net/inbox.jsonl").exists(),
        "the second crawl arrived and agreed"
    );
    assert!(search(addr, "lighthouse").await.is_empty());
    node.refresh_now();
    wait_for(addr, "a new index", |s| {
        ready_and_idle(s) && s.index.as_deref() != Some("000001")
    })
    .await;
    let hits = search(addr, "lighthouse").await;
    assert_eq!(hits[0].domain, domain);
    assert_eq!(hits[0].title.as_deref(), Some("Lighthouse Keepers Guild"));
    assert!(!dir.path().join("net/inbox.jsonl").exists());

    peer.shutdown().await;
    second.shutdown().await;
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_network_search_fills_in_text_this_node_lacks() {
    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    config.network = Some(net);
    let node = start(config).await.unwrap();
    let addr = node.addr();
    let status = wait_for(addr, "the first index", ready_and_idle).await;
    let net_status = status.network.expect("the node joined the network");
    let node_addr: plumb_net::Multiaddr = net_status.listening[0].parse().unwrap();
    let node_addr = node_addr
        .with_p2p(net_status.peer_id.parse().unwrap())
        .unwrap();
    let held = search(addr, "reddit").await;
    assert_eq!(held[0].domain, "reddit.com");
    assert_eq!(held[0].description, None, "the fixture has no description");

    // Another node, assigned reddit.com today, crawled it.
    let now = now_unix();
    let (peer_dir, peer_id) = loop {
        let dir = tempfile::tempdir().unwrap();
        let id = plumb_net::load_or_create_key(&dir.path().join("node.key"))
            .unwrap()
            .public()
            .to_peer_id();
        if plumb_net::assign::is_assigned(
            plumb_net::assign::epoch_of(now),
            &id,
            "reddit.com",
            plumb_net::assign::MAX_SHARE_PPM,
        ) {
            break (dir, id);
        }
    };
    let _ = peer_id;
    let mut crawled = SiteRecord::new("reddit.com");
    crawled.url = Some("https://www.reddit.com/".to_string());
    crawled.title = Some("Reddit".to_string());
    crawled.description = Some("Communities for every interest, from news to hobbies".to_string());
    crawled.crawled_at = Some(now);
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.search_scope = plumb_net::SearchScope::Anyone;
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    peer_config.bootstrap = vec![node_addr];
    let table = plumb_net::BucketTable::build(&peer_dir.path().join("buckets"), &[crawled.clone()])
        .unwrap();
    let (peer, _records) = plumb_net::start(peer_config, Arc::new(table))
        .await
        .unwrap();
    wait_for(addr, "the peer to connect", |s| {
        s.network.as_ref().is_some_and(|n| n.connected_peers >= 1)
    })
    .await;
    // Signed, so its answers carry the proof. One crawler is not enough
    // for the node to take the crawl from gossip.
    assert!(peer.publish(vec![crawled.clone()]).await.unwrap().is_some());

    // The node's own result shows the network's signed text, untinted.
    let mut body = String::new();
    for _ in 0..100 {
        body = get(addr, "/search?q=reddit&net=1").await.2;
        if body.contains("Communities for every interest") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(body.contains("Communities for every interest"), "{body}");
    let results = body.split("<ol>").nth(1).expect("results");
    let first = results.split("<li").nth(1).expect("a result");
    assert!(first.starts_with('>'), "untinted: {first}");
    assert!(first.contains("reddit.com"), "{first}");
    assert!(first.contains("Communities for every"), "{first}");

    // But one crawler alone, neither trusted nor confirmed, is not kept:
    // anyone can make a key assigned the site.
    let inbox = || std::fs::read_to_string(dir.path().join("net/inbox.jsonl")).unwrap_or_default();
    assert!(!inbox().contains("Communities for every"), "{}", inbox());

    // A crawl the search may keep (trusted or confirmed, see
    // plumb_net::FoundSite::keeps) goes to the inbox once, and the node's
    // next index has the text.
    network::keep_found(&node.inner, vec![crawled.clone()]);
    let kept = inbox();
    assert_eq!(kept.lines().count(), 1, "{kept}");
    assert!(kept.contains("Communities for every"), "{kept}");
    network::keep_found(&node.inner, vec![crawled.clone()]);
    assert_eq!(inbox(), kept, "kept once");
    node.refresh_now();
    wait_for(addr, "a new index", |s| {
        ready_and_idle(s) && s.index.as_deref() != Some("000001")
    })
    .await;
    let hits = search(addr, "reddit").await;
    assert_eq!(hits[0].domain, "reddit.com");
    assert_eq!(
        hits[0].description.as_deref(),
        Some("Communities for every interest, from news to hobbies")
    );

    peer.shutdown().await;
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_sharing_popularity_reports_picks_and_ranks_with_the_networks() {
    use plumb_net::popularity::{report_epoch, MAX_POPULARITY_BONUS, REPORT_THRESHOLD};

    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    config.network = Some(net);
    config.share_popularity = true;
    let node = start(config).await.unwrap();
    let addr = node.addr();
    let status = wait_for(addr, "the first index", ready_and_idle).await;
    let net_status = status.network.expect("the node joined the network");
    let node_addr: plumb_net::Multiaddr = net_status.listening[0].parse().unwrap();
    let node_addr = node_addr
        .with_p2p(net_status.peer_id.parse().unwrap())
        .unwrap();

    // "bank" rather than "us bank": a search naming usbank.com leaves
    // out the sites far below it, so it has no runner-up to pick.
    let before = search(addr, "bank").await;
    assert!(before.len() >= 2, "{before:?}");
    let runner_up = before[1].clone();

    // A result opened from the page is noted.
    let (_, _, body) = get(addr, "/search?q=bank").await;
    let go = format!("/go?q=bank&amp;d={}", runner_up.domain);
    assert!(body.contains(&go), "{body}");
    let (code, head, _) = get(addr, &go.replace("&amp;", "&")).await;
    assert_eq!(code, 303);
    assert!(
        head.contains(&format!("location: {}", runner_up.url)),
        "{head}"
    );
    assert!(dir.path().join("net/picks.json").is_file());

    // Another node to hand the report to.
    let peer_dir = tempfile::tempdir().unwrap();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.search_scope = plumb_net::SearchScope::Anyone;
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    peer_config.bootstrap = vec![node_addr];
    let source =
        plumb_net::BucketTable::build(&peer_dir.path().join("buckets"), &fixture_records())
            .unwrap();
    let (peer, _records) = plumb_net::start(peer_config, Arc::new(source))
        .await
        .unwrap();
    wait_for(addr, "the peer to connect", |s| {
        s.network.as_ref().is_some_and(|n| n.connected_peers >= 1)
    })
    .await;

    let mut sent = false;
    for _ in 0..50 {
        if network::report_one(&node.inner).await.unwrap_or(false) {
            sent = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(sent, "the node sent its report");
    // Sent once: the pick is not due again this week.
    assert!(!network::report_one(&node.inner).await.unwrap());
    for _ in 0..100 {
        if peer.status().reports_held >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(peer.status().reports_held, 1);

    // Once enough others report the same pick, the node ranks with it.
    let epoch = report_epoch(now_unix());
    for _ in 1..REPORT_THRESHOLD {
        let report = plumb_net::Report::new(epoch, "bank", &runner_up.domain).unwrap();
        peer.send_report(&report, Duration::from_secs(10))
            .await
            .unwrap();
    }
    let net = node.inner.net.get().unwrap().clone();
    let mut table = net.recount().await.unwrap();
    for _ in 0..100 {
        if !table.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        table = net.recount().await.unwrap();
    }
    assert_eq!(table.picks.len(), 1, "{table:?}");
    let after = search(addr, "bank").await;
    let boosted = after.iter().find(|h| h.domain == runner_up.domain).unwrap();
    assert!(
        (boosted.score - (runner_up.score + MAX_POPULARITY_BONUS)).abs() < 1e-4,
        "{before:?} {after:?}"
    );

    peer.shutdown().await;
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_search_serves_buckets_a_browser_can_search() {
    let dir = seeded_dir();
    // Without private search: no buckets, and nothing private on offer.
    let node = start(test_config(dir.path())).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;
    assert_eq!(get(addr, "/api/buckets").await.0, 503);
    assert_eq!(get(addr, "/private").await.0, 503);
    assert!(!get(addr, "/").await.2.contains("href=\"/private\""));
    node.shutdown().await.unwrap();

    // Turned on, the index built before it is rebuilt with its buckets.
    let mut config = test_config(dir.path());
    config.private_search = true;
    let node = start(config).await.unwrap();
    let addr = node.addr();
    let status = wait_for(addr, "an index with buckets", |s| {
        ready_and_idle(s) && s.index.as_deref() == Some("000002")
    })
    .await;
    assert_eq!(status.last_error, None);
    let (code, head, body) = get(addr, "/api/buckets").await;
    assert_eq!(code, 200, "{body}");
    assert!(head.contains("cache-control: no-store"), "{head}");
    let info: serde_json::Value = serde_json::from_str(&body).unwrap();
    let table = info["table"].as_str().unwrap().to_string();
    assert!(table.starts_with("2-"), "{table}");
    assert_eq!(info["buckets"], plumb_core::keys::BUCKETS);

    // What the page's script does, with fixed padding.
    let query = "us bank";
    let mut n = 0u64;
    let (buckets, keys) = plumb_core::keys::pick_buckets(query, || {
        n += 1;
        n * 7_777
    });
    let mut answers = Vec::new();
    for bucket in &buckets {
        let (code, head, body) = get(addr, &format!("/api/buckets/{table}/{bucket}")).await;
        assert_eq!(code, 200, "{body}");
        assert!(head.contains("immutable"), "{head}");
        answers.push(plumb_private::read_bucket(&body).unwrap());
    }
    let hits = plumb_private::search(query, &keys, answers, &Default::default(), 10);
    assert_eq!(hits[0].domain, "usbank.com", "{hits:?}");

    // Another table's buckets, or no such bucket, are not served.
    assert_eq!(get(addr, "/api/buckets/1-0000/5").await.0, 404);
    let past_end = format!("/api/buckets/{table}/{}", plumb_core::keys::BUCKETS);
    assert_eq!(get(addr, &past_end).await.0, 404);

    let (code, head, body) = get(addr, "/private").await;
    if crate::web::private::in_build() {
        assert_eq!(code, 200, "{body}");
        assert!(
            head.contains("script-src 'self' 'wasm-unsafe-eval'"),
            "{head}"
        );
        assert!(get(addr, "/").await.2.contains("href=\"/private\""));
    } else {
        assert_eq!(code, 503, "{body}");
    }
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_full_seed_replaces_a_quick_start_but_keeps_what_crawls_added() {
    let tranco = std::fs::read(fixture("tranco.csv")).unwrap();
    let sparql = sparql_json(&fixture("wikidata-official-sites.tsv"));
    let host = SeedHost::start(move |request, _| match request {
        "GET /tranco.csv" => http("200 OK", &tranco),
        "POST /sparql" => http("200 OK", &sparql),
        _ => http("404 Not Found", b"no such file"),
    })
    .await;

    // Quick records: one only the Tranco list had, one a crawl reached, and
    // one a crawl found links to.
    let dir = tempfile::tempdir().unwrap();
    let quick_only = SiteRecord::new("quick-only.example");
    let mut crawled = SiteRecord::new("crawled.example");
    crawled.crawl_attempted_at = Some(now_unix());
    crawled.title = Some("Crawled".into());
    let mut linked = SiteRecord::new("linked.example");
    linked.add_link_text("Linked", "crawled.example");
    write_jsonl(
        &dir.path().join("records.jsonl"),
        &[quick_only, crawled, linked],
    )
    .unwrap();
    let paths = store::Paths::new(dir.path());
    store::save_state(
        &paths,
        &SavedState {
            crawl_left: 0,
            index_stale: true,
            last_refresh: Some(now_unix()),
            wikidata_missing: true,
            quick_start: true,
            ..SavedState::default()
        },
    )
    .unwrap();

    let mut config = test_config(dir.path());
    config.sources = host.sources();
    config.sites = 50;
    let node = start(config).await.unwrap();
    let addr = node.addr();
    let status = wait_for(addr, "the full seed", |s| {
        ready_and_idle(s) && !s.wikidata_missing
    })
    .await;
    assert_eq!(status.wikidata_error, None);
    assert_eq!(search(addr, "us+bank").await[0].domain, "usbank.com");
    node.shutdown().await.unwrap();

    let saved = store::load_state(&paths).unwrap();
    assert!(!saved.quick_start && !saved.wikidata_missing);
    let records: Vec<SiteRecord> = read_jsonl(&dir.path().join("records.jsonl")).unwrap();
    let domains: Vec<&str> = records.iter().map(|r| r.domain.as_str()).collect();
    assert_eq!(records.len(), 52, "{domains:?}");
    assert!(!domains.contains(&"quick-only.example"));
    assert!(domains.contains(&"crawled.example"));
    assert!(domains.contains(&"linked.example"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quick_start_crawls_before_asking_wikidata() {
    let dir = recently_crawled_dir();
    let paths = store::Paths::new(dir.path());
    store::save_state(
        &paths,
        &SavedState {
            crawl_left: 700,
            index_stale: true,
            last_refresh: None,
            wikidata_missing: true,
            quick_start: true,
            ..SavedState::default()
        },
    )
    .unwrap();
    // What was left of the first crawl when Wikidata was first asked.
    let left_when_asked = Arc::new(Mutex::new(None));
    let asked = Arc::clone(&left_when_asked);
    let state_paths = paths.clone();
    let tranco = std::fs::read(fixture("tranco.csv")).unwrap();
    let sparql = sparql_json(&fixture("wikidata-official-sites.tsv"));
    let host = SeedHost::start(move |request, _| match request {
        "GET /tranco.csv" => http("200 OK", &tranco),
        "POST /sparql" => {
            let mut asked = asked.lock().unwrap();
            if asked.is_none() {
                *asked = store::load_state(&state_paths).map(|s| s.crawl_left);
            }
            http("200 OK", &sparql)
        }
        _ => http("404 Not Found", b"no such file"),
    })
    .await;

    let mut config = test_config(dir.path());
    config.sources = host.sources();
    config.sites = 50;
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "the full seed", |s| {
        ready_and_idle(s) && !s.wikidata_missing
    })
    .await;
    node.shutdown().await.unwrap();
    assert_eq!(*left_when_asked.lock().unwrap(), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_updates_wait_while_off_and_resume_from_the_panel() {
    let dir = recently_crawled_dir();
    let paths = store::Paths::new(dir.path());
    store::save_state(
        &paths,
        &SavedState {
            crawl_left: 700,
            index_stale: true,
            last_refresh: None,
            wikidata_missing: false,
            quick_start: false,
            ..SavedState::default()
        },
    )
    .unwrap();
    std::fs::write(&paths.settings, "{\"background_updates\": false}").unwrap();
    let mut config = test_config(dir.path());
    config.refresh_every = Some(Duration::from_secs(3600));
    let node = start(config).await.unwrap();
    let addr = node.addr();

    let status = wait_for(addr, "the first index", ready_and_idle).await;
    assert_eq!(status.detail, "Background updates are off");
    assert!(!status.background_updates);
    assert_eq!((status.crawl_left, status.last_refresh), (700, None));
    let (code, _, body) = get(addr, "/app").await;
    assert_eq!(code, 200);
    assert!(
        body.contains("<p>Background updates are off.</p>"),
        "{body}"
    );
    assert_eq!(status.paused.as_deref(), Some("Background updates are off"));

    // Ticking the box on the panel, from this computer, starts the round.
    let form = "background_updates=1";
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "POST /app/settings HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{form}",
        form.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 303"), "{response}");

    let status = wait_for(addr, "the end of the round", |s| {
        ready_and_idle(s) && s.last_refresh.is_some()
    })
    .await;
    assert!(status.background_updates);
    assert_eq!(status.crawl_left, 0);
    node.shutdown().await.unwrap();
    assert_eq!(store::load_settings(&paths), Some(NodeSettings::default()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crawling_waits_for_the_next_day_once_the_download_limit_is_reached() {
    let dir = recently_crawled_dir();
    let paths = store::Paths::new(dir.path());
    let mut state = SavedState {
        crawl_left: 700,
        index_stale: true,
        ..SavedState::default()
    };
    state.add_downloaded(3 * MB, now_unix());
    store::save_state(&paths, &state).unwrap();
    let mut config = test_config(dir.path());
    config.settings.download_limit_mb_per_day = 2;
    let node = start(config).await.unwrap();

    let status = wait_for(node.addr(), "the first index", ready_and_idle).await;
    assert_eq!(
        status.paused.as_deref(),
        Some("Paused until tomorrow: today's download limit is reached")
    );
    assert_eq!(status.detail, status.paused.clone().unwrap());
    assert_eq!((status.crawl_left, status.downloaded_today), (700, 3 * MB));
    assert!(status.disk_used > 0);
    node.shutdown().await.unwrap();
}

#[test]
fn downloads_are_counted_per_day() {
    let day = 24 * 60 * 60;
    let mut state = SavedState::default();
    state.add_downloaded(5, 10 * day + 1);
    state.add_downloaded(7, 10 * day + 2);
    assert_eq!(state.downloaded_today(10 * day + 3), 12);
    assert_eq!(state.downloaded_today(11 * day), 0);
    state.add_downloaded(1, 11 * day + 5);
    assert_eq!(state.downloaded_today(11 * day + 6), 1);
    assert_eq!(state.downloaded_total, 13);
    assert_eq!(store::next_day(10 * day + 7), 11 * day);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_by_meaning_embeds_sites_in_the_background() {
    let dir = seeded_dir();
    // The model is in place, so nothing is downloaded.
    plumb_embed::write_test_model(&dir.path().join(MeaningModel::Small.dir_name())).unwrap();
    let mut config = test_config(dir.path());
    config.search_by_meaning = true;
    let node = start(config).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;

    let vectors_path = dir.path().join(plumb_embed::VECTORS_FILE_NAME);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while node.inner.meaning.get().is_none_or(|m| m.is_empty()) {
        assert!(std::time::Instant::now() < deadline, "no vectors made");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Names still win.
    assert_eq!(search(addr, "us+bank").await[0].domain, "usbank.com");
    // Every site with text gets a vector, saved for the next start.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let with_text = fixture_records()
        .iter()
        .filter(|r| !plumb_embed::site_text(r).is_empty())
        .count();
    while plumb_embed::Vectors::load(&vectors_path).map_or(0, |v| v.len()) < with_text {
        assert!(std::time::Instant::now() < deadline, "vectors not saved");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_by_meaning_downloads_and_runs_embedding_gemma_when_chosen() {
    let made = tempfile::tempdir().unwrap();
    plumb_embed::write_test_gemma(made.path()).unwrap();
    let model = std::fs::read(made.path().join(plumb_embed::GEMMA_FILE)).unwrap();
    let tokenizer = std::fs::read(made.path().join(plumb_embed::GEMMA_TOKENIZER_FILE)).unwrap();
    let host = SeedHost::start(move |request, _| match request {
        "GET /gemma/model.gguf" => http("200 OK", &model),
        "GET /gemma/tokenizer.json" => http("200 OK", &tokenizer),
        _ => http("404 Not Found", b"no such file"),
    })
    .await;
    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    config.search_by_meaning = true;
    config.meaning_model = MeaningModel::Gemma;
    config.sources.gemma_downloads = [plumb_embed::GEMMA_FILE, plumb_embed::GEMMA_TOKENIZER_FILE]
        .map(|name| (name.to_string(), host.url(&format!("/gemma/{name}"))))
        .to_vec();
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "the first index", ready_and_idle).await;

    let vectors_path = dir.path().join(plumb_embed::VECTORS_FILE_NAME);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while plumb_embed::Vectors::load(&vectors_path).map_or(0, |v| v.len()) == 0 {
        assert!(std::time::Instant::now() < deadline, "no vectors saved");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node.shutdown().await.unwrap();
    let gemma_dir = dir.path().join(MeaningModel::Gemma.dir_name());
    let (model, dim, _) = plumb_embed::Vectors::read_header(&vectors_path).unwrap();
    assert_eq!(model, plumb_embed::gemma_id(&gemma_dir).unwrap());
    assert_eq!(model, plumb_embed::gemma_id(made.path()).unwrap());
    assert_eq!(dim, 96);
    // The small model was never downloaded.
    assert!(!dir.path().join(MeaningModel::Small.dir_name()).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_for_an_hour_holds_the_crawl_and_resume_lifts_it() {
    let dir = recently_crawled_dir();
    let paths = store::Paths::new(dir.path());
    store::save_state(
        &paths,
        &SavedState {
            crawl_left: 700,
            index_stale: true,
            ..SavedState::default()
        },
    )
    .unwrap();
    let until = now_unix() + 3600;
    let mut config = test_config(dir.path());
    config.settings.paused_until = Some(until);
    let node = start(config).await.unwrap();
    let status = wait_for(node.addr(), "the first index", ready_and_idle).await;
    assert_eq!(status.paused.as_deref(), Some("Paused by you"));
    assert_eq!(status.paused_until, Some(until));
    assert_eq!(status.crawl_left, 700);
    // Nobody listens for a restart yet, so the panel offers none.
    assert!(!status.can_restart);
    let signal = node.restart_signal();
    assert!(node.status().can_restart);
    let restart = tokio::spawn(async move { signal.requested().await });
    StatusSource::restart(node.inner.as_ref()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), restart)
        .await
        .expect("the restart request arrives")
        .unwrap();

    node.inner
        .change_settings(NodeSettings {
            paused_until: None,
            ..node.inner.settings()
        })
        .unwrap();
    let status = wait_for(node.addr(), "the end of the round", |s| {
        ready_and_idle(s) && s.crawl_left == 0
    })
    .await;
    assert_eq!(status.paused, None);
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_activity_log_and_backups_cover_a_node_s_life() {
    let dir = recently_crawled_dir();
    let mut config = test_config(dir.path());
    config.refresh_every = None;
    let node = start(config.clone()).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;
    let inner = node.inner.as_ref();
    inner
        .change_settings(NodeSettings {
            workload: Workload::Light,
            ..inner.settings()
        })
        .unwrap();
    let log = StatusSource::activity_log(inner);
    assert!(
        log.last().unwrap().message.starts_with("Plumb Search "),
        "{log:?}"
    );
    assert_eq!(log[0].message, "Settings changed: workload light");

    // A backup, then a change, then the backup restored: the change is undone.
    std::fs::create_dir_all(dir.path().join("net")).unwrap();
    std::fs::write(dir.path().join("net/node.key"), b"key one").unwrap();
    let made = StatusSource::make_backup(inner).unwrap();
    inner
        .change_settings(NodeSettings {
            workload: Workload::Full,
            ..inner.settings()
        })
        .unwrap();
    std::fs::write(dir.path().join("net/node.key"), b"key two").unwrap();
    let saved = backup::path_of(dir.path(), &made.name).unwrap();
    let restored = backup::Backup::parse(&std::fs::read(saved).unwrap()).unwrap();
    StatusSource::restore_backup(inner, &restored).unwrap();
    assert_eq!(inner.settings().workload, Workload::Light);
    assert_eq!(
        std::fs::read(dir.path().join("net/node.key")).unwrap(),
        b"key one"
    );
    // What it replaced was backed up first.
    let backups = backup::list(dir.path());
    assert_eq!(backups.len(), 2);
    assert!(backups.iter().any(|b| b.name.contains("before-restore")));
    let (code, _, body) = get(addr, "/app?section=backup").await;
    assert_eq!(code, 200);
    assert!(body.contains(&made.name), "{body}");
    let (code, head, body) = get(addr, &format!("/app/backups/{}", made.name)).await;
    assert_eq!(code, 200);
    assert!(head.contains("content-disposition: attachment"), "{head}");
    assert!(body.contains("plumb-backup/1"));
    let (code, _, _) = get(addr, "/app/backups/..%2Fsettings.json").await;
    assert_eq!(code, 404);
    let (code, _, body) = get(addr, "/app?section=activity").await;
    assert_eq!(code, 200);
    assert!(body.contains("restored"), "{body}");

    node.shutdown().await.unwrap();
    // The log outlives the node.
    let node = start(config).await.unwrap();
    let log = StatusSource::activity_log(node.inner.as_ref());
    assert!(log.iter().any(|e| e.message == "Stopped"));
    assert!(log.iter().any(|e| e.message.starts_with("Backup made")));
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_fills_its_free_space_with_a_trusted_node_s_crawls() {
    // A trusted node has crawled sites this one has never heard of.
    let peer_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let now = now_unix();
    let sites: Vec<SiteRecord> = ["harbourmasters.org", "tidetables.net", "uncrawled.org"]
        .iter()
        .enumerate()
        .map(|(i, domain)| {
            let mut record = SiteRecord::new(*domain);
            record.signals.tranco_rank = Some(10 + i as u32);
            if !domain.starts_with("uncrawled") {
                record.url = Some(format!("https://{domain}/"));
                record.title = Some(format!("Guild of {domain}"));
                record.crawled_at = Some(now - 600);
            }
            record
        })
        .collect();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.search_scope = plumb_net::SearchScope::Anyone;
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    let table = plumb_net::BucketTable::build(&peer_dir.path().join("buckets"), &sites).unwrap();
    let (peer, _records) = plumb_net::start(peer_config, Arc::new(table))
        .await
        .unwrap();
    let peer_addr: plumb_net::Multiaddr = loop {
        if let Some(addr) = peer.status().listening.first() {
            break addr.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.trusted_peers = vec![peer_id];
    net.bootstrap = vec![peer_addr.with_p2p(peer_id).unwrap()];
    config.network = Some(net);
    let node = start(config).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;
    assert!(search(addr, "harbourmasters").await.is_empty());

    // It asks the trusted node for its crawled sites and keeps them.
    let status = wait_for(addr, "the trusted node's crawls", |s| {
        s.fill
            .as_ref()
            .is_some_and(|f| f.filled == 2 && f.detail.starts_with("Done"))
    })
    .await;
    assert_eq!(status.fill.unwrap().total, 3);
    node.refresh_now();
    wait_for(addr, "a new index", |s| {
        ready_and_idle(s) && s.index.as_deref() != Some("000001")
    })
    .await;
    let hits = search(addr, "harbourmasters").await;
    assert_eq!(hits[0].domain, "harbourmasters.org");
    assert_eq!(
        hits[0].title.as_deref(),
        Some("Guild of harbourmasters.org")
    );
    assert!(search(addr, "uncrawled").await.is_empty());
    assert!(peer.status().fill_records_served >= 2);
    let kept: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("net/fill.json")).unwrap()).unwrap();
    assert_eq!(kept["filled"], 2);

    peer.shutdown().await;
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shows_the_recent_headlines_a_trusted_node_shares() {
    let (peer_id, peer_addr, peer, _peer_dir) = crawled_peer(&["harbourmasters.org"]).await;
    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.fill = false;
    net.trusted_peers = vec![peer_id];
    net.bootstrap = vec![peer_addr];
    config.network = Some(net);
    let node = start(config).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;

    let now = now_unix();
    let checked = |domain: &str, title: &str| {
        let mut record = SiteRecord::new(domain);
        record.news = vec![plumb_core::Headline {
            title: title.into(),
            url: format!("https://www.{domain}/story"),
            at: now - 600,
        }];
        record
    };
    let shared = vec![
        checked("tidetables.net", "Spring tides flood the harbour"),
        checked("harbourmasters.org", "Harbour closed for spring tides"),
    ];
    let mut body = String::new();
    for _ in 0..100 {
        peer.publish(shared.clone()).await.unwrap();
        body = get(addr, "/api/recent?q=spring+tides").await.2;
        if body.contains("tidetables.net") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let recent: crate::news::Recent = serde_json::from_str(&body).unwrap();
    assert_eq!(recent.site, None);
    assert_eq!(recent.headlines.len(), 2, "{recent:?}");
    // Headlines are not site records.
    assert!(!dir.path().join("net/inbox.jsonl").exists());
    let (_, _, page) = get(addr, "/search?q=spring+tides").await;
    assert!(page.contains("<span class=\"nh\">Recent</span>"), "{page}");
    let (_, _, page) = get(addr, "/search?q=spring+tides&news=off").await;
    assert!(!page.contains("class=\"nh\""), "{page}");
    assert!(!page.contains("Harbour closed for spring tides"), "{page}");
    // A topic only one site's headline is about gets no block.
    assert_eq!(
        get(addr, "/api/recent?q=closed").await.2,
        "{\"headlines\":[]}"
    );

    peer.shutdown().await;
    node.shutdown().await.unwrap();
    assert!(dir.path().join("news/headlines.json").is_file());
}

/// A node in the network holding `domains`, crawled, for others to fill
/// from: its id, address and handle, and the folder to keep alive.
async fn crawled_peer(
    domains: &[&str],
) -> (
    plumb_net::PeerId,
    plumb_net::Multiaddr,
    plumb_net::NetHandle,
    tempfile::TempDir,
) {
    let peer_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let now = now_unix();
    let sites: Vec<SiteRecord> = domains
        .iter()
        .enumerate()
        .map(|(i, domain)| {
            let mut record = SiteRecord::new(*domain);
            record.signals.tranco_rank = Some(10 + i as u32);
            record.url = Some(format!("https://{domain}/"));
            record.title = Some(format!("Guild of {domain}"));
            record.crawled_at = Some(now - 600);
            record
        })
        .collect();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    let table = plumb_net::BucketTable::build(&peer_dir.path().join("buckets"), &sites).unwrap();
    let (peer, _records) = plumb_net::start(peer_config, Arc::new(table))
        .await
        .unwrap();
    let peer_addr: plumb_net::Multiaddr = loop {
        if let Some(addr) = peer.status().listening.first() {
            break addr.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    (
        peer_id,
        peer_addr.with_p2p(peer_id).unwrap(),
        peer,
        peer_dir,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blackhole_node_fills_from_every_trusted_node() {
    // Two trusted nodes, each with crawled sites the other lacks.
    let (a_id, a_addr, a, _a_dir) = crawled_peer(&["harbourmasters.org", "tidetables.net"]).await;
    let (b_id, b_addr, b, _b_dir) = crawled_peer(&["lighthousekeepers.org"]).await;

    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.trusted_peers = vec![a_id, b_id];
    net.bootstrap = vec![a_addr, b_addr];
    config.network = Some(net);
    config.blackhole = true;
    let node = start(config).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;

    // It goes through both trusted nodes' lists, not just one.
    let status = wait_for(addr, "both trusted nodes' crawls", |s| {
        s.fill
            .as_ref()
            .is_some_and(|f| f.filled == 3 && f.lists_done == 2)
    })
    .await;
    let fill = status.fill.unwrap();
    assert!(fill.blackhole);
    node.refresh_now();
    wait_for(addr, "a new index", |s| {
        ready_and_idle(s) && s.index.as_deref() != Some("000001")
    })
    .await;
    assert_eq!(
        search(addr, "harbourmasters").await[0].domain,
        "harbourmasters.org"
    );
    assert_eq!(
        search(addr, "lighthousekeepers").await[0].domain,
        "lighthousekeepers.org"
    );
    let (_, _, panel) = get(addr, "/app").await;
    assert!(panel.contains("Blackhole"), "{panel}");

    a.shutdown().await;
    b.shutdown().await;
    node.shutdown().await.unwrap();
}

/// A trusted node's buckets plus its page set file.
struct WithPages(plumb_net::BucketTable, PathBuf);

impl plumb_net::BucketSource for WithPages {
    fn bucket(&self, bucket: u32) -> Option<Vec<String>> {
        self.0.bucket(bucket)
    }

    fn page_set_file(&self, set: &str) -> Option<PathBuf> {
        (set == "wikipedia-en").then(|| self.1.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_takes_wikipedia_articles_from_a_trusted_node() {
    let peer_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    // The trusted node's set file, gzipped as fetch-pages writes it.
    let articles: Vec<plumb_core::Article> = [
        ("Marie Curie", 900u64),
        ("Pierre Curie", 500),
        ("Curie (unit)", 10),
    ]
    .iter()
    .map(|(title, views)| plumb_core::Article {
        title: title.to_string(),
        views: *views,
        ..Default::default()
    })
    .collect();
    let set_file = peer_dir.path().join("wikipedia-en.tsv.gz");
    plumb_ingest::articles::write_articles_file(&set_file, &articles).unwrap();
    let table = plumb_net::BucketTable::build(
        &peer_dir.path().join("buckets"),
        &[SiteRecord::new("lighthouses.org")],
    )
    .unwrap();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.search_scope = plumb_net::SearchScope::Anyone;
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    let (peer, _records) = plumb_net::start(peer_config, Arc::new(WithPages(table, set_file)))
        .await
        .unwrap();
    let peer_addr: plumb_net::Multiaddr = loop {
        if let Some(addr) = peer.status().listening.first() {
            break addr.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // This node keeps the two most read.
    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    config.settings.page_sets = crate::pages::PageSets::parse("wikipedia-en=2").unwrap();
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.fill = false;
    net.trusted_peers = vec![peer_id];
    net.bootstrap = vec![peer_addr.with_p2p(peer_id).unwrap()];
    config.network = Some(net);
    let node = start(config).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, _, body) = get(addr, "/search?q=pierre+curie").await;
        if body.contains("Pierre_Curie") {
            break;
        }
        assert!(Instant::now() < deadline, "no articles came: {body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let set = crate::pages::SetInfo::find("wikipedia-en").unwrap();
    let notes = set.file_notes(dir.path()).unwrap();
    assert_eq!((notes.lines, notes.complete), (2, false));
    // Only whole files are passed on.
    assert!(set.servable_file(dir.path()).is_none());
    let (_, _, body) = get(addr, "/search?q=curie+unit").await;
    assert!(!body.contains("Curie_(unit)"), "{body}");

    peer.shutdown().await;
    node.shutdown().await.unwrap();
}

/// A trusted node's buckets plus its adult blocklist.
struct WithAdultList(plumb_net::BucketTable, PathBuf);

impl plumb_net::BucketSource for WithAdultList {
    fn bucket(&self, bucket: u32) -> Option<Vec<String>> {
        self.0.bucket(bucket)
    }

    fn page_set_file(&self, set: &str) -> Option<PathBuf> {
        (set == super::adult::SHARED_NAME).then(|| self.1.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_takes_the_adult_blocklist_from_a_trusted_node_before_its_source() {
    let peer_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let list_file = peer_dir.path().join("adult-domains.txt");
    std::fs::write(&list_file, "adult.example\nother.example\n").unwrap();
    let table = plumb_net::BucketTable::build(
        &peer_dir.path().join("buckets"),
        &[SiteRecord::new("lighthouses.org")],
    )
    .unwrap();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.search_scope = plumb_net::SearchScope::Anyone;
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    let (peer, _records) = plumb_net::start(
        peer_config,
        Arc::new(WithAdultList(table, list_file.clone())),
    )
    .await
    .unwrap();
    let peer_addr: plumb_net::Multiaddr = loop {
        if let Some(addr) = peer.status().listening.first() {
            break addr.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    // The source cannot be reached: the list can only come from the peer.
    config.sources.adult_list_url = Some("http://127.0.0.1:9/porn-nl.txt".into());
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.fill = false;
    net.trusted_peers = vec![peer_id];
    net.bootstrap = vec![peer_addr.with_p2p(peer_id).unwrap()];
    config.network = Some(net);
    let node = start(config).await.unwrap();

    let kept = dir.path().join("safe").join("adult-domains.txt");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !kept.is_file() {
        assert!(Instant::now() < deadline, "no list came");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        std::fs::read_to_string(&kept).unwrap(),
        "adult.example\nother.example\n"
    );
    // It keeps the time of the peer's copy, so it ages as that one does.
    let modified = |path: &Path| std::fs::metadata(path).unwrap().modified().unwrap();
    let (theirs, ours) = (modified(&list_file), modified(&kept));
    let apart = theirs
        .duration_since(ours)
        .or_else(|_| ours.duration_since(theirs))
        .unwrap();
    assert!(apart < Duration::from_secs(2), "{apart:?}");

    peer.shutdown().await;
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_set_file_being_downloaded_is_not_cut_at_the_same_time() {
    let dir = seeded_dir();
    let set = crate::pages::SetInfo::find("wikipedia-en").unwrap();
    let file = set.file(dir.path());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let articles: Vec<plumb_core::Article> = ["Marie Curie", "Pierre Curie", "Curie (unit)"]
        .iter()
        .map(|title| plumb_core::Article {
            title: title.to_string(),
            views: 10,
            ..Default::default()
        })
        .collect();
    plumb_ingest::articles::write_articles_file(&file, &articles).unwrap();
    let notes = crate::pages::SetFileNotes {
        lines: 3,
        complete: true,
        source_modified: 1,
        fetched_at: now_unix(),
        near: 0,
    };
    std::fs::write(
        crate::pages::notes_path(&file),
        serde_json::to_vec(&notes).unwrap(),
    )
    .unwrap();
    let node = start(test_config(dir.path())).await.unwrap();
    let addr = node.addr();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !get(addr, "/search?q=pierre+curie")
        .await
        .2
        .contains("Pierre_Curie")
    {
        assert!(Instant::now() < deadline, "no pages");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // A download holds the file; a lower limit meanwhile leaves it alone.
    node.inner.set_files_busy.lock().unwrap().insert(set.id);
    let mut settings = node.inner.settings();
    settings.page_sets = crate::pages::PageSets::parse("wikipedia-en=2").unwrap();
    settings.storage_limit_mb = 500;
    node.inner.change_settings(settings).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(set.file_notes(dir.path()).unwrap().lines, 3);

    // Once the download is done, the file is cut.
    node.inner.set_files_busy.lock().unwrap().remove(set.id);
    let deadline = Instant::now() + Duration::from_secs(30);
    while set.file_notes(dir.path()).unwrap().lines != 2 {
        assert!(Instant::now() < deadline, "never cut");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_set_file_over_the_storage_limit_is_cut_before_it_is_indexed() {
    // A file taken from a trusted node, with more pages than the node now
    // keeps.
    let dir = seeded_dir();
    let set = crate::pages::SetInfo::find("wikipedia-en").unwrap();
    let file = set.file(dir.path());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let articles: Vec<plumb_core::Article> = [
        ("Marie Curie", 900u64),
        ("Pierre Curie", 500),
        ("Curie (unit)", 10),
    ]
    .iter()
    .map(|(title, views)| plumb_core::Article {
        title: title.to_string(),
        views: *views,
        ..Default::default()
    })
    .collect();
    plumb_ingest::articles::write_articles_file(&file, &articles).unwrap();
    let notes = crate::pages::SetFileNotes {
        lines: 3,
        complete: true,
        source_modified: 1,
        fetched_at: now_unix(),
        near: 0,
    };
    std::fs::write(
        crate::pages::notes_path(&file),
        serde_json::to_vec(&notes).unwrap(),
    )
    .unwrap();
    let mut config = test_config(dir.path());
    config.settings.page_sets = crate::pages::PageSets::parse("wikipedia-en=2").unwrap();
    config.settings.storage_limit_mb = 500;
    let node = start(config).await.unwrap();
    let addr = node.addr();

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, _, body) = get(addr, "/search?q=pierre+curie").await;
        if body.contains("Pierre_Curie") {
            break;
        }
        assert!(Instant::now() < deadline, "no pages: {body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(set.file_notes(dir.path()).unwrap().lines, 2);
    // One index, of the pages kept: none of the whole file first.
    let built: Vec<String> = node
        .inner
        .journal
        .entries()
        .into_iter()
        .map(|entry| entry.message)
        .filter(|message| message.starts_with("Page sets ready"))
        .collect();
    assert_eq!(
        built,
        ["Page sets ready: 2 pages searched next to the sites"],
        "{built:?}"
    );

    node.shutdown().await.unwrap();
}

/// A trusted node that says when it is asked for a page set file, then
/// answers only once let go (or the asker gives up).
struct SlowPages {
    table: plumb_net::BucketTable,
    asked: Mutex<std::sync::mpsc::Sender<()>>,
    gate: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl plumb_net::BucketSource for SlowPages {
    fn bucket(&self, bucket: u32) -> Option<Vec<String>> {
        self.table.bucket(bucket)
    }

    fn page_set_file(&self, _set: &str) -> Option<PathBuf> {
        let _ = self.asked.lock().unwrap().send(());
        let _ = self
            .gate
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(60));
        None
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_searches_the_pages_it_has_while_a_download_waits() {
    let peer_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let table = plumb_net::BucketTable::build(
        &peer_dir.path().join("buckets"),
        &[SiteRecord::new("lighthouses.org")],
    )
    .unwrap();
    let (asked_tx, asked) = std::sync::mpsc::channel();
    let (let_go, gate) = std::sync::mpsc::channel();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    let source = SlowPages {
        table,
        asked: Mutex::new(asked_tx),
        gate: Mutex::new(gate),
    };
    let (peer, _records) = plumb_net::start(peer_config, Arc::new(source))
        .await
        .unwrap();
    let peer_addr: plumb_net::Multiaddr = loop {
        if let Some(addr) = peer.status().listening.first() {
            break addr.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // The node keeps a set it has no file of, so it asks the trusted node,
    // which takes its time.
    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    config.settings.page_sets = crate::pages::PageSets::parse("wikipedia-en=2").unwrap();
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.fill = false;
    net.trusted_peers = vec![peer_id];
    net.bootstrap = vec![peer_addr.with_p2p(peer_id).unwrap()];
    config.network = Some(net);
    let node = start(config).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;
    tokio::task::spawn_blocking(move || asked.recv_timeout(Duration::from_secs(30)))
        .await
        .unwrap()
        .expect("the node asks the trusted node for the set");

    // Meanwhile a file of the set turns up here (from fetch-pages, say):
    // it is searched without waiting on the download.
    let set = crate::pages::SetInfo::find("wikipedia-en").unwrap();
    let file = set.file(dir.path());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let articles = [plumb_core::Article {
        title: "Pierre Curie".into(),
        views: 500,
        ..Default::default()
    }];
    plumb_ingest::articles::write_articles_file(&file, &articles).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, _, body) = get(addr, "/search?q=pierre+curie").await;
        if body.contains("Pierre_Curie") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the pages wait on the download: {body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let _ = let_go.send(());
    peer.shutdown().await;
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_node_sets_up_from_a_trusted_node_without_the_seed_downloads() {
    // A trusted node holds five sites, two never crawled, with what the
    // seed downloads gave it: ranks, names, Wikidata facts.
    let now = now_unix();
    let peer_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let sites: Vec<SiteRecord> = [
        "lighthouses.org",
        "uncrawledlanterns.org",
        "harbourmasters.org",
        "tidetables.org",
        "uncrawledbuoys.org",
    ]
    .iter()
    .enumerate()
    .map(|(i, domain)| {
        let mut record = SiteRecord::new(*domain);
        record.signals.tranco_rank = Some(1 + i as u32);
        record.about = Some(format!("keepers of {domain}"));
        if !domain.starts_with("uncrawled") {
            record.url = Some(format!("https://{domain}/"));
            record.title = Some(format!("Guild of {domain}"));
            record.crawled_at = Some(now - 600);
        }
        record
    })
    .collect();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.search_scope = plumb_net::SearchScope::Anyone;
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    let table = plumb_net::BucketTable::build(&peer_dir.path().join("buckets"), &sites).unwrap();
    let (peer, _records) = plumb_net::start(peer_config, Arc::new(table))
        .await
        .unwrap();
    let peer_addr: plumb_net::Multiaddr = loop {
        if let Some(addr) = peer.status().listening.first() {
            break addr.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // A new node, whose seed sources lead nowhere: setup can only work
    // from the network.
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config(dir.path());
    config.sites = 10;
    config.settings.setup_chosen = false;
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.trusted_peers = vec![peer_id];
    net.bootstrap = vec![peer_addr.with_p2p(peer_id).unwrap()];
    config.network = Some(net);
    let node = start(config).await.unwrap();
    let addr = node.addr();
    let status = wait_for(addr, "the first index", ready_and_idle).await;
    // The first index holds the best three, crawled or not.
    assert_eq!(status.sites, 3, "{status:?}");
    assert!(!status.wikidata_missing);
    assert!(status.last_error.is_none(), "{status:?}");
    assert_eq!(
        search(addr, "lighthouses").await[0].domain,
        "lighthouses.org"
    );

    // The rest waits for "Set up my node".
    wait_for(addr, "the setup question", |s| {
        s.fill
            .as_ref()
            .is_some_and(|f| f.detail.contains("Set up my node"))
    })
    .await;
    node.inner
        .change_settings(NodeSettings {
            storage_limit_mb: 500,
            setup_chosen: true,
            ..node.inner.settings()
        })
        .unwrap();

    // Filling takes in the rest, uncrawled sites too.
    let status = wait_for(addr, "the rest of the trusted node's sites", |s| {
        s.fill
            .as_ref()
            .is_some_and(|f| f.filled == 5 && f.detail.starts_with("Done"))
    })
    .await;
    assert_eq!(status.fill.unwrap().total, 5);
    node.refresh_now();
    wait_for(addr, "a new index", |s| {
        ready_and_idle(s) && s.index.as_deref() != Some("000001")
    })
    .await;
    let hits = search(addr, "uncrawledbuoys").await;
    assert_eq!(hits[0].domain, "uncrawledbuoys.org");
    let records = crate::records::load_records(&dir.path().join("records.jsonl")).unwrap();
    assert_eq!(records.len(), 5);
    let buoys = records.get("uncrawledbuoys.org").unwrap();
    assert_eq!(buoys.signals.tranco_rank, Some(5));
    assert_eq!(
        buoys.about.as_deref(),
        Some("keepers of uncrawledbuoys.org")
    );
    assert!(buoys.crawled_at.is_none());
    // Nothing came from outside the network.
    assert!(names(&dir.path().join("seed")).is_empty());

    peer.shutdown().await;
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_node_that_trusts_no_one_still_sets_up_from_the_seed_downloads() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config(dir.path());
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.trusted_peers.clear();
    config.network = Some(net);
    let node = start(config).await.unwrap();
    // The seed sources lead nowhere, so setup goes to them and fails.
    let status = wait_for(node.addr(), "a try at the seed downloads", |s| {
        s.last_error.is_some()
    })
    .await;
    let error = status.last_error.unwrap().message;
    assert!(error.contains("Tranco"), "{error}");
    node.shutdown().await.unwrap();
}

/// An HTTP/1.1 request with a cookie and, for a POST, a form: the status
/// code, the head (lowercased) and the body.
async fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    cookie: &str,
    form: &str,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nCookie: {cookie}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{form}",
        form.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let code = head.split(' ').nth(1).unwrap().parse().unwrap();
    (code, head.to_ascii_lowercase(), body.to_string())
}

/// The profile cookie a response sets, if it sets one.
fn profile_cookie(head: &str) -> Option<String> {
    let start = head.find("plumb_profile=")? + "plumb_profile=".len();
    Some(head[start..start + 32].to_string())
}

/// A searcher's other node, keeping its profiles in a folder.
struct ProfileFolder(PathBuf);

impl plumb_net::BucketSource for ProfileFolder {
    fn bucket(&self, _bucket: u32) -> Option<Vec<String>> {
        None
    }

    fn profile(
        &self,
        peer: plumb_net::PeerId,
        request: plumb_net::proto::ProfileRequest,
    ) -> plumb_net::proto::ProfileResponse {
        crate::sync::answer(&self.0, peer, request)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_browser_takes_a_profile_from_another_node_with_a_link_code() {
    // The other node's browser searched github.
    let peer_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let profiles = peer_dir.path().join("history");
    let theirs = crate::history::HistoryStore::new(&profiles);
    let p = crate::history::new_profile().unwrap();
    theirs.update(&p, |h| h.add_search("github", 1)).unwrap();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    let (peer, _records) = plumb_net::start(peer_config, Arc::new(ProfileFolder(profiles)))
        .await
        .unwrap();
    let peer_addr: plumb_net::Multiaddr = loop {
        if let Some(addr) = peer.status().listening.first() {
            break addr.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    config.search_history = true;
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.fill = false;
    net.bootstrap = vec![peer_addr.with_p2p(peer_id).unwrap()];
    config.network = Some(net);
    let node = start(config).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;

    // This node's browser searched chase, then pastes the other's code.
    let (_, head, _) = send(addr, "GET", "/search?q=chase", "", "").await;
    let q = profile_cookie(&head).expect("a profile for the search");
    let code = crate::sync::format_code(&crate::sync::make_code(&p).unwrap(), Some(&peer_id));
    let form = format!("code={}", code.replace('@', "%40"));
    let (status, head, body) = send(
        addr,
        "POST",
        "/link/join",
        &format!("plumb_profile={q}"),
        &form,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Done."), "{body}");
    assert_eq!(profile_cookie(&head).as_deref(), Some(p.as_str()));

    // Both searches are on both nodes now.
    let (_, _, history) = send(addr, "GET", "/history", &format!("plumb_profile={p}"), "").await;
    assert!(
        history.contains("github") && history.contains("chase"),
        "{history}"
    );
    let searches: Vec<String> = theirs
        .load(&p)
        .searches
        .into_iter()
        .map(|s| s.query)
        .collect();
    assert_eq!(searches, ["chase", "github"]);
    let (_, _, page) = send(addr, "GET", "/link", &format!("plumb_profile={p}"), "").await;
    assert!(page.contains("synced"), "{page}");

    peer.shutdown().await;
    node.shutdown().await.unwrap();
}

/// A trusted node's buckets plus its vectors file, served as the set of
/// vectors of its model.
struct WithVectors(plumb_net::BucketTable, PathBuf, String);

impl plumb_net::BucketSource for WithVectors {
    fn bucket(&self, bucket: u32) -> Option<Vec<String>> {
        self.0.bucket(bucket)
    }

    fn page_set_file(&self, set: &str) -> Option<PathBuf> {
        (set == self.2).then(|| self.1.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_takes_site_vectors_made_from_its_own_text_from_a_trusted_node() {
    let dir = seeded_dir();
    let model_dir = dir.path().join(MeaningModel::Small.dir_name());
    plumb_embed::write_test_model(&model_dir).unwrap();
    let model = plumb_embed::model_id(&model_dir).unwrap();

    // The trusted node's vectors: one for a site's very text, one for
    // other text, one for a site this node does not know.
    let records = fixture_records();
    let mut with_text = records
        .iter()
        .filter(|r| !plumb_embed::site_text(r).is_empty());
    let same = with_text.next().unwrap();
    let other = with_text.next().unwrap();
    let marker = [7i8; 32];
    let mut theirs = plumb_embed::Vectors::new(model, 32);
    let hash = plumb_embed::text_hash(&plumb_embed::site_text(same));
    theirs.insert(&same.domain, hash, &marker).unwrap();
    theirs.insert(&other.domain, [9; 32], &marker).unwrap();
    theirs.insert("unknown-to-it.org", hash, &marker).unwrap();
    let peer_dir = tempfile::tempdir().unwrap();
    let file = peer_dir.path().join(plumb_embed::VECTORS_FILE_NAME);
    theirs.save(&file).unwrap();
    assert_eq!(
        shared_vectors::servable(peer_dir.path(), &shared_vectors::set_name(&model)),
        Some(file.clone())
    );
    assert_eq!(
        shared_vectors::servable(peer_dir.path(), &shared_vectors::set_name(&[0; 32])),
        None
    );
    assert_eq!(
        shared_vectors::servable(peer_dir.path(), "wikipedia-en"),
        None
    );

    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let table = plumb_net::BucketTable::build(
        &peer_dir.path().join("buckets"),
        &[SiteRecord::new("lighthouses.org")],
    )
    .unwrap();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.search_scope = plumb_net::SearchScope::Anyone;
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    let source = WithVectors(table, file, shared_vectors::set_name(&model));
    let (peer, _records) = plumb_net::start(peer_config, Arc::new(source))
        .await
        .unwrap();
    let peer_addr: plumb_net::Multiaddr = loop {
        if let Some(addr) = peer.status().listening.first() {
            break addr.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let mut config = test_config(dir.path());
    config.search_by_meaning = true;
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.fill = false;
    net.trusted_peers = vec![peer_id];
    net.bootstrap = vec![peer_addr.with_p2p(peer_id).unwrap()];
    config.network = Some(net);
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "the first index", ready_and_idle).await;

    // Every site with text ends with a vector: the one sent for its text
    // kept as sent, the others made here.
    let vectors_path = dir.path().join(plumb_embed::VECTORS_FILE_NAME);
    let wanted = records
        .iter()
        .filter(|r| !plumb_embed::site_text(r).is_empty())
        .count();
    let deadline = Instant::now() + Duration::from_secs(60);
    let vectors = loop {
        if let Ok(vectors) = plumb_embed::Vectors::load(&vectors_path) {
            if vectors.len() == wanted && vectors.get(&same.domain).is_some() {
                break vectors;
            }
        }
        assert!(Instant::now() < deadline, "vectors not saved");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(vectors.get(&same.domain), Some((&hash, &marker[..])));
    assert_ne!(vectors.get(&other.domain).unwrap().1, &marker[..]);
    assert!(vectors.get("unknown-to-it.org").is_none());

    peer.shutdown().await;
    node.shutdown().await.unwrap();
}
