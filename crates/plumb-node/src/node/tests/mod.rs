//! Tests of a whole node: started on a loopback port with a temporary data
//! directory and driven over HTTP. Nothing here reaches the internet: seed
//! data comes from the fixtures, served by a stand-in host on a loopback
//! port, and the tests that run a node with records leave nothing to crawl.
//! `crawl_golden` checks a crawl round, run on its own, against a golden
//! file, and `seed_golden` setup and the seed, `plumb ingest` and the
//! sites a node embeds.

mod agents;
mod crawl_golden;
mod indexes_and_rounds;
mod meaning;
mod owner;
mod seed_golden;
mod sets;
mod setup;
mod storage;
mod trusted;
mod wanted_pages;
mod with_peers;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plumb_core::{now_unix, write_jsonl, SiteRecord};
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

/// Writes `articles` as a gzipped articles file, as `plumb-tools fetch-pages`
/// makes them.
fn write_articles_file(dest: &Path, articles: &[plumb_core::Article]) -> anyhow::Result<()> {
    use std::io::Write;
    let file = std::fs::File::create(dest)?;
    let mut out = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    out.write_all(plumb_core::article::ARTICLES_HEADER.as_bytes())?;
    for article in articles {
        plumb_core::article::write_article(&mut out, article)?;
    }
    out.finish()?;
    Ok(())
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// Site records from every fixture, as `plumb ingest` makes them, each
/// the record its stored item holds.
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
    let sites = builder.finish(None);
    sites
        .iter()
        .map(|site| SiteRecord::from_stored_item(&site).unwrap())
        .collect()
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
    // No feed of a test site is ever fetched, nor its pages for searches.
    config.news_feeds = 0;
    config.wanted_pages = false;
    // As if "Set up my node" was answered, so filling goes ahead.
    config.settings.setup_chosen = true;
    // No model is downloaded unless a test asks for one.
    config.search_by_meaning = false;
    let nowhere = closed_port();
    config.sources = SeedSources {
        tranco_url: format!("{nowhere}/tranco.csv"),
        wikidata_sparql_url: format!("{nowhere}/sparql"),
        wikidata_mirror_url: None,
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
            wikidata_mirror_url: None,
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

/// Writes a denylist of `entries` into `data`, signed by `publisher`.
fn hold_denylist(
    data: &Path,
    publisher: &ed25519_dalek::SigningKey,
    entries: &[plumb_tools::set::DenyEntryArgs],
) {
    let work = tempfile::tempdir().unwrap();
    let mut text = String::new();
    for entry in entries {
        text += &serde_json::to_string(&plumb_tools::set::deny_entry(entry).unwrap()).unwrap();
        text.push('\n');
    }
    let file = work.path().join("entries.jsonl");
    std::fs::write(&file, text).unwrap();
    let dir = crate::node::signed_sets::set_dir(data, "plumb/denylist").unwrap();
    plumb_tools::set::denylist(&plumb_tools::set::DenylistArgs {
        entries: file,
        name: "plumb/denylist".into(),
        out: dir.clone(),
        title: None,
        license: Some("CC0-1.0".into()),
        level: 3,
    })
    .unwrap();
    plumb_tools::set::sign_set(&dir, publisher, None, None).unwrap();
}

/// A node's buckets plus the signed sets in a data folder.
struct WithSignedSets(plumb_net::BucketTable, PathBuf);

impl plumb_net::BucketSource for WithSignedSets {
    fn bucket(&self, bucket: u32) -> Option<Vec<String>> {
        self.0.bucket(bucket)
    }

    fn set_manifest(&self, set: &str) -> Option<String> {
        super::signed_sets::held_manifest(&self.1, set)
    }

    fn set_part_file(&self, sha256: &str) -> Option<PathBuf> {
        super::signed_sets::held_part(&self.1, sha256)
    }
}
