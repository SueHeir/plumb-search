//! Setting up from the seed downloads, retrying and stopping during them.

use super::*;

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
    let records: Vec<SiteRecord> =
        plumb_core::stored::read_file(&dir.path().join("records.jsonl")).unwrap();
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
        body.contains("<progress aria-label=\"Setup progress\" value=\"0\" max=\"1\"></progress>"),
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
    let records: Vec<SiteRecord> =
        plumb_core::stored::read_file(&dir.path().join("records.jsonl")).unwrap();
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
    let records: Vec<SiteRecord> =
        plumb_core::stored::read_file(&dir.path().join("records.jsonl")).unwrap();
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
    // again on request, from the seed files on disk, without downloading,
    // and a disambiguation page's claim is taken back off them.
    let mut records: Vec<SiteRecord> =
        plumb_core::stored::read_file(&dir.path().join("records.jsonl")).unwrap();
    for record in &mut records {
        record.signals.official_site = false;
        record.aliases.clear();
    }
    let usbank = records
        .iter_mut()
        .find(|r| r.domain == "usbank.com")
        .unwrap();
    usbank.aliases.push("USB".into());
    write_jsonl(&dir.path().join("records.jsonl"), &records).unwrap();
    let seed = dir.path().join("seed");
    let mut tsv = std::fs::read_to_string(seed.join(download::WIKIDATA_FILE_NAME)).unwrap();
    tsv.push_str("http://www.wikidata.org/entity/Q90000097\tUSB\thttps://www.usbank.com/\n");
    std::fs::write(seed.join(download::WIKIDATA_FILE_NAME), tsv).unwrap();
    let facts = format!(
        "{}Q90000097\t\tWikimedia disambiguation page\t\t\t3\n",
        plumb_ingest::facts::FACTS_HEADER
    );
    std::fs::write(seed.join(plumb_ingest::facts::FACTS_FILE_NAME), facts).unwrap();
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
    let records: Vec<SiteRecord> =
        plumb_core::stored::read_file(&dir.path().join("records.jsonl")).unwrap();
    let usbank = records.iter().find(|r| r.domain == "usbank.com").unwrap();
    assert!(usbank.signals.official_site);
    assert!(usbank.aliases.iter().any(|alias| alias == "U.S. Bancorp"));
    assert!(
        !usbank.aliases.iter().any(|alias| alias == "USB"),
        "{:?}",
        usbank.aliases
    );

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
    let mut records: Vec<SiteRecord> =
        plumb_core::stored::read_file(&dir.path().join("records.jsonl")).unwrap();
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
        let sites = crate::records::load_sites(&dir.path().join("records.jsonl")).unwrap();
        let scholar = sites.get("scholar.google.com").unwrap();
        assert_eq!(scholar.names, ["Google Scholar"]);
        assert_eq!(plumb_core::site::signals(&scholar).tranco_rank, Some(1));
        let google = sites.get("google.com").unwrap();
        assert_eq!(google.names, ["Google"]);
        assert!(!plumb_core::site::kinds(&google)
            .iter()
            .any(|kind| kind == "college"));
        assert!(plumb_core::site::signals(&google).official_site);
        // Hacker News needs a record for ycombinator.com, which this has not.
        assert!(!sites.contains("news.ycombinator.com"));
        sites.len()
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

    // Records made with a site rules set the node no longer uses are
    // brought up to date the same way, and the lists in use noted.
    let mut saved = store::load_state(&paths).unwrap();
    saved.site_rules = Some("ab".repeat(32));
    store::save_state(&paths, &saved).unwrap();
    assert!(saved.sites_changed());
    let index_before = names(&dir.path().join("indexes"));
    let mut config = test_config(dir.path());
    config.sources = offline.sources();
    config.sites = 50;
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "the records updated for the rules", |s| {
        ready_and_idle(s)
            && s.index
                .as_ref()
                .is_some_and(|index| !index_before.contains(index))
    })
    .await;
    node.shutdown().await.unwrap();
    let saved = store::load_state(&paths).unwrap();
    assert_eq!(saved.site_rules, None);
    assert!(!saved.sites_changed());
    assert_eq!(check(), sites);

    // Nothing to fold in a directory with no node yet.
    assert!(!request_reseed(tempfile::tempdir().unwrap().path()).unwrap());
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
    let records: Vec<SiteRecord> =
        plumb_core::stored::read_file(&dir.path().join("records.jsonl")).unwrap();
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
    let table =
        plumb_net::BucketTable::build(&peer_dir.path().join("buckets"), &stored_items(&sites))
            .unwrap();
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
    let sites = crate::records::load_sites(&dir.path().join("records.jsonl")).unwrap();
    assert_eq!(sites.len(), 5);
    let buoys = sites.get("uncrawledbuoys.org").unwrap();
    assert_eq!(plumb_core::site::signals(&buoys).tranco_rank, Some(5));
    assert_eq!(
        plumb_core::site::wikidata_about(&buoys),
        Some("keepers of uncrawledbuoys.org")
    );
    assert!(plumb_core::site::crawled(&buoys).is_none());
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

/// Crawling only, a node sets up from the Tranco list without building an
/// index, and counts the sites it saved, as filling counts an index's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_only_crawls_sets_up_without_an_index() {
    let tranco = std::fs::read(fixture("tranco.csv")).unwrap();
    let host = SeedHost::start(move |request, _| match request {
        "GET /tranco.csv" => http("200 OK", &tranco),
        _ => http("404 Not Found", b"no such file"),
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config(dir.path());
    config.sources = host.sources();
    config.sites = 50;
    config.crawl_only = true;
    let node = start(config).await.unwrap();
    let status = wait_for(node.addr(), "setup", |s| {
        s.step == Step::Idle && s.last_refresh.is_some()
    })
    .await;
    assert_eq!(status.last_error, None, "{status:#?}");
    assert_eq!(status.index, None);
    assert_eq!(node.inner.held_sites(), 50);
    node.shutdown().await.unwrap();
    let records: Vec<SiteRecord> =
        plumb_core::stored::read_file(&dir.path().join("records.jsonl")).unwrap();
    assert_eq!(records.len(), 50);
    assert!(names(&dir.path().join("indexes")).is_empty());
}
