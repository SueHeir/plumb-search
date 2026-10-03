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
    load_cc_domain_ranks, load_tranco, load_wikidata_official_sites, parse_wat, Builder, WatExtract,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::store::{self, SavedState};
use super::*;

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
    let nowhere = closed_port();
    config.sources = SeedSources {
        tranco_url: format!("{nowhere}/tranco.csv"),
        wikidata_sparql_url: format!("{nowhere}/sparql"),
        wikidata_min_sitelinks: 25,
        wikidata_pacing: quick_wikidata(),
        cc_ranks_url: None,
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

#[test]
fn profiles() {
    let server = NodeConfig::server(PathBuf::from("/data"));
    assert_eq!(server.data_dir, PathBuf::from("/data"));
    assert_eq!(server.bind, "127.0.0.1:8080".parse().unwrap());
    assert_eq!(
        (server.sites, server.initial_crawl, server.crawl_per_refresh),
        (1_000_000, 10_000, 5_000)
    );
    assert_eq!(server.refresh_every, Some(Duration::from_secs(24 * 3600)));
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
    assert!(
        first.contains("href=\"https://www.usbank.com/\""),
        "{first}"
    );

    node.shutdown().await.unwrap();
    assert!(TcpStream::connect(addr).await.is_err(), "still listening");
    assert_eq!(
        names(dir.path()),
        ["indexes", "node.lock", "records.jsonl", "state.json"]
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
    // Every read replays it; the next crawl folds it in once it is big.
    assert!(dir.path().join("records.jsonl.journal").exists());
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
        }
    );
    assert_eq!(names(&paths.indexes), ["000001"]);
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
            wikidata_min_sitelinks: 25,
            wikidata_pacing: quick_wikidata(),
            cc_ranks_url: None,
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
    // Then twice for the countries and kinds of the official websites.
    assert_eq!(
        counts["POST /sparql"],
        download::wikidata_sitelink_bands(25).len() + 1 + 1,
        "{counts:?}"
    );
    assert_eq!(counts["GET /graph/x-domain-ranks.txt.gz"], 1, "{counts:?}");
    assert_eq!(counts.len(), 3, "{counts:?}");

    let seed = dir.path().join("seed");
    assert_eq!(
        names(&seed),
        [
            "tranco-top-1m.csv.zip",
            "wikidata-official-sites.tsv",
            "wikidata-site-facts.tsv",
            "x-domain-ranks-top50.txt"
        ]
    );
    let records: Vec<SiteRecord> = read_jsonl(&dir.path().join("records.jsonl")).unwrap();
    assert_eq!(records.len(), 50);
    let usbank = records.iter().find(|r| r.domain == "usbank.com").unwrap();
    assert!(usbank.signals.official_site);
    assert!(usbank.signals.tranco_rank.is_some());
    assert!(usbank.signals.harmonic_rank.is_some());
    assert_eq!(names(&dir.path().join("indexes")), ["000001"]);
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
    assert!(err.message.contains("Wikidata"), "{}", err.message);
    assert!(err.message.contains("<script>"), "{}", err.message);
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
        assert!(
            body.contains("&lt;script&gt;alert(&#39;pwned&#39;)&lt;/script&gt; &amp; &lt;b&gt;"),
            "{body}"
        );
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
            total: 2,
            unit: "files".into()
        })
    );
    let (code, _, body) = get(node.addr(), "/").await;
    assert_eq!(code, 200);
    assert!(body.contains("Downloading the Tranco list"), "{body}");
    assert!(
        body.contains("<progress value=\"0\" max=\"2\"></progress>"),
        "{body}"
    );
    assert!(body.contains("0 of 2 files"), "{body}");

    let stopping = Instant::now();
    tokio::time::timeout(Duration::from_secs(10), node.shutdown())
        .await
        .expect("shutdown does not wait for the download")
        .unwrap();
    assert!(stopping.elapsed() < Duration::from_secs(5));
    host.abort();
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
    assert!(store::load_state(&paths).unwrap().wikidata_missing);
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_in_the_network_takes_in_other_nodes_crawls_and_searches_them() {
    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    config.network = Some(net);
    let node = start(config).await.unwrap();
    let addr = node.addr();
    let status = wait_for(addr, "the first index", ready_and_idle).await;
    let net_status = status.network.expect("the node joined the network");
    let node_addr: plumb_net::Multiaddr = net_status.listening[0].parse().unwrap();
    let node_addr = node_addr
        .with_p2p(net_status.peer_id.parse().unwrap())
        .unwrap();
    assert!(dir.path().join("net/node.key").is_file());

    // Another node crawled a site this one has never heard of.
    let peer_dir = tempfile::tempdir().unwrap();
    let key = plumb_net::load_or_create_key(&peer_dir.path().join("node.key")).unwrap();
    let peer_id = key.public().to_peer_id();
    let now = now_unix();
    let domain = (0..)
        .map(|i| format!("lighthouse-keepers-{i}.org"))
        .find(|d| {
            plumb_net::assign::is_assigned(
                plumb_net::assign::epoch_of(now),
                &peer_id,
                d,
                plumb_net::assign::MAX_SHARE_PPM,
            )
        })
        .unwrap();
    let mut crawled = SiteRecord::new(domain.as_str());
    crawled.url = Some(format!("https://{domain}/"));
    crawled.title = Some("Lighthouse Keepers Guild".to_string());
    crawled.crawled_at = Some(now);
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
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
    assert!(body.contains("href=\"/network?q=us+bank\""), "{body}");

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

    // The peer publishes its crawl; the node keeps it and, at its next
    // refresh, searches it from its own index.
    let mut published = None;
    for _ in 0..100 {
        published = peer.publish(vec![crawled.clone()]).await.unwrap();
        if dir.path().join("net/inbox.jsonl").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(published.is_some());
    assert!(
        dir.path().join("net/inbox.jsonl").exists(),
        "the batch arrived"
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
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_sharing_popularity_reports_picks_and_ranks_with_the_networks() {
    use plumb_net::popularity::{report_epoch, MAX_POPULARITY_BONUS, REPORT_THRESHOLD};

    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
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

    let before = search(addr, "us+bank").await;
    assert!(before.len() >= 2, "{before:?}");
    let runner_up = before[1].clone();

    // A result opened from the page is noted.
    let (_, _, body) = get(addr, "/search?q=us+bank").await;
    let go = format!("/go?q=us+bank&amp;d={}", runner_up.domain);
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
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
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
        let report = plumb_net::Report::new(epoch, "us bank", &runner_up.domain).unwrap();
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
    let after = search(addr, "us+bank").await;
    let boosted = after.iter().find(|h| h.domain == runner_up.domain).unwrap();
    assert!(
        (boosted.score - (runner_up.score + MAX_POPULARITY_BONUS)).abs() < 1e-4,
        "{before:?} {after:?}"
    );

    peer.shutdown().await;
    node.shutdown().await.unwrap();
}
