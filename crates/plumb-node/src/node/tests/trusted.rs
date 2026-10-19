//! What a node takes from trusted nodes: crawls, headlines, page sets and
//! the adult blocklist.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_fills_its_free_space_with_a_trusted_node_s_crawls() {
    // A trusted node has crawled sites this one has never heard of.
    let peer_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let now = now_unix();
    let sites: Vec<Item> = ["harbourmasters.org", "tidetables.net", "uncrawled.org"]
        .iter()
        .enumerate()
        .map(|(i, domain)| {
            let mut record = site::new(*domain);
            let mut signals = site::signals(&record);
            signals.tranco_rank = Some(10 + i as u32);
            site::set_signals(&mut record, &signals);
            if !domain.starts_with("uncrawled") {
                record.url = Some(format!("https://{domain}/"));
                record.title = Some(format!("Guild of {domain}"));
                site::set_crawled(&mut record, Some(now - 600));
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
        let mut record = site::new(domain);
        let news = vec![plumb_core::Headline {
            title: title.into(),
            url: format!("https://www.{domain}/story"),
            at: now - 600,
        }];
        site::set_news(&mut record, news);
        record
    };
    let shared = vec![
        checked("tidetables.net", "Spring tides flood the harbour"),
        checked("harbourmasters.org", "Harbour closed for spring tides"),
    ];
    let mut body = String::new();
    for _ in 0..100 {
        peer.publish(listed(&shared)).await.unwrap();
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
    let sites: Vec<Item> = domains
        .iter()
        .enumerate()
        .map(|(i, domain)| {
            let mut record = site::new(*domain);
            let mut signals = site::signals(&record);
            signals.tranco_rank = Some(10 + i as u32);
            site::set_signals(&mut record, &signals);
            record.url = Some(format!("https://{domain}/"));
            record.title = Some(format!("Guild of {domain}"));
            site::set_crawled(&mut record, Some(now - 600));
            record
        })
        .collect();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
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
    write_articles_file(&set_file, &articles).unwrap();
    let table = plumb_net::BucketTable::build(
        &peer_dir.path().join("buckets"),
        &[plumb_core::site::new("lighthouses.org")],
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

/// A trusted node's buckets plus files of several sets.
struct WithFiles(plumb_net::BucketTable, HashMap<String, PathBuf>);

impl plumb_net::BucketSource for WithFiles {
    fn bucket(&self, bucket: u32) -> Option<Vec<String>> {
        self.0.bucket(bucket)
    }

    fn page_set_file(&self, set: &str) -> Option<PathBuf> {
        self.1.get(set).cloned()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_takes_newer_set_files_that_hold_what_its_own_do() {
    let article = |title: &str, lead: Option<&str>| plumb_core::Article {
        title: title.to_string(),
        views: 100,
        item: Some("Q1".into()),
        lead: lead.map(str::to_string),
        ..Default::default()
    };
    let write = |path: &Path, articles: &[plumb_core::Article]| {
        write_articles_file(path, articles).unwrap();
    };
    let long_ago = now_unix() - 100_000;

    // This node made its own files a while ago: articles with leads, films
    // and a map.
    let dir = seeded_dir();
    let sets = crate::pages::sets_dir(dir.path());
    std::fs::create_dir_all(&sets).unwrap();
    let wikipedia = crate::pages::SetInfo::find("wikipedia-en").unwrap();
    let films = crate::pages::SetInfo::find("films").unwrap();
    let led = [article("Marie Curie", Some("Marie Curie was a physicist."))];
    write(&wikipedia.file(dir.path()), &led);
    write(&films.file(dir.path()), &[article("Old Film", None)]);
    let map = crate::map::file(dir.path());
    std::fs::write(&map, b"old map").unwrap();
    for file in [
        wikipedia.file(dir.path()),
        films.file(dir.path()),
        map.clone(),
    ] {
        crate::node::newer::set_time(&file, long_ago).unwrap();
    }

    // The trusted node has newer ones of all three, but its articles have
    // no leads.
    let peer_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let peer_articles = peer_dir.path().join("wikipedia-en.tsv.gz");
    write(&peer_articles, &[article("Marie Curie", None)]);
    let (modified, size) = crate::node::newer::stamp(&peer_articles).unwrap();
    let note = plumb_net::pages::Layers {
        version: plumb_net::pages::LAYERS_VERSION,
        modified,
        size,
        kinds: vec![],
    };
    std::fs::write(
        plumb_net::pages::layers_path(&peer_articles),
        serde_json::to_vec(&note).unwrap(),
    )
    .unwrap();
    let peer_films = peer_dir.path().join("films.tsv.gz");
    write(
        &peer_films,
        &[article("New Film", None), article("Old Film", None)],
    );
    // Books, which this node has no file of and doesn't name in
    // --set-updates, are never taken.
    let peer_books = peer_dir.path().join("books.tsv.gz");
    write(&peer_books, &[article("A Book", None)]);
    let peer_map = peer_dir.path().join("map.pmtiles");
    std::fs::write(&peer_map, b"new map, a bit longer").unwrap();
    let files = HashMap::from([
        ("wikipedia-en".to_string(), peer_articles),
        ("films".to_string(), peer_films.clone()),
        ("map".to_string(), peer_map.clone()),
        ("books".to_string(), peer_books),
    ]);
    let table = plumb_net::BucketTable::build(
        &peer_dir.path().join("buckets"),
        &[plumb_core::site::new("lighthouses.org")],
    )
    .unwrap();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    let (peer, _records) = plumb_net::start(peer_config, Arc::new(WithFiles(table, files)))
        .await
        .unwrap();
    let peer_addr: plumb_net::Multiaddr = loop {
        if let Some(addr) = peer.status().listening.first() {
            break addr.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let mut config = test_config(dir.path());
    config.settings.page_sets =
        crate::pages::PageSets::parse("wikipedia-en=all,films=all,books=all").unwrap();
    // Named, so the tiny files may grow past a quarter.
    config.set_updates = "wikipedia-en,films,map".parse().unwrap();
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
    wait_for(node.addr(), "the first index", ready_and_idle).await;

    // The newer films and map files are taken, the old ones kept as .prev,
    // and the new ones dated as their maker dated them.
    let deadline = Instant::now() + Duration::from_secs(60);
    let films_file = films.file(dir.path());
    while std::fs::read(&map).unwrap() != b"new map, a bit longer"
        || std::fs::read(&films_file).unwrap() != std::fs::read(&peer_films).unwrap()
    {
        assert!(Instant::now() < deadline, "the newer files never came");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        std::fs::read(crate::node::newer::prev_path(&map)).unwrap(),
        b"old map"
    );
    assert!(crate::node::newer::prev_path(&films_file).is_file());
    assert_eq!(
        crate::node::newer::stamp(&films_file).unwrap().0,
        crate::node::newer::stamp(&peer_films).unwrap().0
    );
    // The articles without leads are not taken, however new.
    let kept = wikipedia.file(dir.path());
    assert_eq!(crate::node::newer::stamp(&kept).unwrap().0, long_ago);
    assert_eq!(crate::node::newer::layers("wikipedia-en", &kept), ["lead"]);
    assert!(!crate::node::newer::prev_path(&kept).exists());
    let books = crate::pages::SetInfo::find("books").unwrap();
    assert!(!books.file(dir.path()).exists());

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
        (set == crate::node::adult::SHARED_NAME).then(|| self.1.clone())
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
        &[plumb_core::site::new("lighthouses.org")],
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
