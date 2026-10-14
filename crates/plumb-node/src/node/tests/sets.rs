//! Signed sets: page sets, the adult list, the map and the denylist, who
//! may sign them, and their files under the storage limit.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_takes_the_project_page_sets_it_keeps_as_items_part_by_part() {
    use ed25519_dalek::SigningKey;
    use plumb_format::PublicKey;

    let peer_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let held = peer_dir.path().join("held");
    let publisher = SigningKey::from_bytes(&[7; 32]);
    // The books page set as the project's item set, a book a part.
    let pages: Vec<plumb_index::pages::Page> = ["Lighthouse keeping", "Fresnel lenses", "Foghorns"]
        .iter()
        .zip([30, 20, 10])
        .enumerate()
        .map(|(n, (title, views))| {
            serde_json::from_value(serde_json::json!({
                "set": "books", "url": format!("https://openlibrary.org/works/OL{n}W"),
                "title": title, "description": "A lighthouse keeper", "views": views,
            }))
            .unwrap()
        })
        .collect();
    let books = crate::node::signed_sets::set_dir(&held, "plumb/books").unwrap();
    let serde_json::Value::Object(manifest) = serde_json::json!({"format": plumb_format::FORMAT,
        "set": "plumb/books", "generation": "2026-10-12T04:00:00Z", "license": "CC0-1.0"})
    else {
        unreachable!()
    };
    let items = pages.iter().map(plumb_tools::page_items::page_item);
    let fields = plumb_tools::page_items::page_fields();
    plumb_tools::set::write_set(items, fields, manifest, &books, &[1, 2], 3).unwrap();
    plumb_tools::set::sign_set(&books, &publisher, None, None).unwrap();

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
    let (peer, _records) =
        plumb_net::start(peer_config, Arc::new(WithSignedSets(table, held.clone())))
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
    config.set_publishers = vec![PublicKey::of(&publisher)];
    config.settings.page_sets = crate::pages::PageSets::parse("books=2").unwrap();
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.fill = false;
    net.trusted_peers = Vec::new();
    net.bootstrap = vec![peer_addr.with_p2p(peer_id).unwrap()];
    config.network = Some(net);
    let node = start(config).await.unwrap();

    let taken = crate::node::signed_sets::set_dir(dir.path(), "plumb/books").unwrap();
    let indexed = |node: &NodeHandle| {
        node.inner
            .pages
            .read()
            .unwrap()
            .as_ref()
            .map_or(0, |(_, searcher)| searcher.num_pages())
    };
    // The first two parts hold the two pages kept.
    let deadline = Instant::now() + Duration::from_secs(60);
    while indexed(&node) != 2 {
        assert!(Instant::now() < deadline, "the books never came");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(taken.join("items-002.jsonl.zst").is_file());
    assert!(!taken.join("items-003.jsonl.zst").exists());
    let found = node
        .inner
        .pages
        .read()
        .unwrap()
        .as_ref()
        .unwrap()
        .1
        .search("fresnel lenses", 5)
        .unwrap();
    assert_eq!(found[0].page, pages[1], "the page its item gives back");
    assert!(
        !crate::pages::SetInfo::find("books")
            .unwrap()
            .file(dir.path())
            .exists(),
        "no articles file taken besides"
    );

    // Keeping more pages takes the rest of the generation held.
    let mut settings = node.inner.settings();
    settings.page_sets = crate::pages::PageSets::parse("books=all").unwrap();
    node.inner.change_settings(settings).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while indexed(&node) != 3 {
        assert!(Instant::now() < deadline, "the third part never came");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    crate::sets::check_set(&taken, Some(&PublicKey::of(&publisher)), &[]).unwrap();
    // The panel counts what the item set holds.
    let coverage = crate::node::pages::coverage(&node.inner);
    let shown = coverage.sets.iter().find(|set| set.set == "books").unwrap();
    assert_eq!(
        (shown.stored_records, shown.transfer_complete),
        (Some(3), Some(true))
    );

    // Under a storage limit, keeping fewer pages drops the parts past them.
    let mut settings = node.inner.settings();
    settings.storage_limit_mb = 1024;
    settings.page_sets = crate::pages::PageSets::parse("books=1").unwrap();
    node.inner.change_settings(settings).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while indexed(&node) != 1 {
        assert!(Instant::now() < deadline, "the parts past the first stayed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(taken.join("items-001.jsonl.zst").is_file());
    assert!(!taken.join("items-002.jsonl.zst").exists());
    assert!(!taken.join("items-003.jsonl.zst").exists());

    peer.shutdown().await;
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_keeps_the_adult_list_set_it_holds_as_its_list() {
    use ed25519_dalek::SigningKey;
    use plumb_format::PublicKey;

    let dir = seeded_dir();
    let list = dir.path().join("porn-nl.txt");
    std::fs::write(&list, "adult.example\nother.example\n").unwrap();
    let out =
        crate::node::signed_sets::set_dir(dir.path(), crate::node::adult::ADULT_SITES).unwrap();
    plumb_tools::set::adult_sites(&plumb_tools::set::AdultSitesArgs {
        list,
        name: crate::node::adult::ADULT_SITES.into(),
        out: out.clone(),
        license: "Unlicense".into(),
        attribution: "From the Block List Project".into(),
        level: 3,
    })
    .unwrap();
    let publisher = SigningKey::from_bytes(&[7; 32]);
    plumb_tools::set::sign_set(&out, &publisher, None, None).unwrap();
    let mut config = test_config(dir.path());
    config.set_publishers = vec![PublicKey::of(&publisher)];
    let node = start(config).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    while !node
        .inner
        .adult_list()
        .is_some_and(|list| list.contains("www.adult.example"))
    {
        assert!(Instant::now() < deadline, "the set never became the list");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Kept as the list's file, dated by the set's generation.
    let kept = dir.path().join("safe").join("adult-domains.txt");
    assert_eq!(
        std::fs::read_to_string(&kept).unwrap(),
        "adult.example\nother.example\n"
    );
    let (manifest, _) = crate::sets::read_items(&out).unwrap();
    let generation = plumb_format::value::unix_seconds(&manifest.generation).unwrap();
    assert_eq!(
        crate::node::newer::stamp(&kept).unwrap().0,
        u64::try_from(generation).unwrap()
    );
    node.shutdown().await.unwrap();
}

/// A node's buckets and signed sets, sending `served` for the map's part:
/// the map with its first half zeroed, so only a node that goes on from
/// the half it fetched before gets the map whole.
struct WithMapSet(plumb_net::BucketTable, PathBuf, PathBuf);

impl plumb_net::BucketSource for WithMapSet {
    fn bucket(&self, bucket: u32) -> Option<Vec<String>> {
        self.0.bucket(bucket)
    }

    fn set_manifest(&self, set: &str) -> Option<String> {
        crate::node::signed_sets::held_manifest(&self.1, set)
    }

    fn set_part_file(&self, sha256: &str) -> Option<PathBuf> {
        let part = crate::node::signed_sets::held_part(&self.1, sha256)?;
        Some(if part.ends_with(crate::map::MAP_FILE) {
            self.2.clone()
        } else {
            part
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_takes_the_map_as_a_set_going_on_from_what_it_fetched() {
    use ed25519_dalek::SigningKey;
    use plumb_format::PublicKey;

    let at = |path: &Path, seconds: u64| crate::node::newer::set_time(path, seconds).unwrap();
    let peer_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let held = peer_dir.path().join("held");
    let publisher = SigningKey::from_bytes(&[7; 32]);
    let made = tempfile::tempdir().unwrap();
    let source = crate::map::tests::sample_map(made.path());
    at(&source, 1_790_000_000);
    let out = crate::node::signed_sets::set_dir(&held, crate::node::signed_sets::MAP).unwrap();
    plumb_tools::set::map(&plumb_tools::set::MapArgs {
        pmtiles: source.clone(),
        name: crate::node::signed_sets::MAP.into(),
        out: out.clone(),
        license: "ODbL-1.0".into(),
        attribution: "© OpenStreetMap contributors".into(),
    })
    .unwrap();
    plumb_tools::set::sign_set(&out, &publisher, None, None).unwrap();
    let map = std::fs::read(&source).unwrap();
    let half = map.len() / 2;
    let mut zeroed = map.clone();
    zeroed[..half].fill(0);
    let served = peer_dir.path().join("served.pmtiles");
    std::fs::write(&served, &zeroed).unwrap();

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
        Arc::new(WithMapSet(table, held.clone(), served)),
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
    // An older map file, and the first half of the set's map from a take
    // that stopped.
    let file = crate::map::file(dir.path());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::copy(&source, &file).unwrap();
    at(&file, 1_780_000_000);
    let staging = dir
        .path()
        .join(crate::node::signed_sets::DIR)
        .join("plumb")
        .join("map.new");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join(crate::map::MAP_FILE), &map[..half]).unwrap();
    let mut config = test_config(dir.path());
    config.set_publishers = vec![PublicKey::of(&publisher)];
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.fill = false;
    net.trusted_peers = Vec::new();
    net.bootstrap = vec![peer_addr.with_p2p(peer_id).unwrap()];
    config.network = Some(net);
    let node = start(config).await.unwrap();

    // Taken, the set's map replaces the older file.
    let deadline = Instant::now() + Duration::from_secs(60);
    while file.exists() {
        assert!(Instant::now() < deadline, "the map never came");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let taken = crate::node::signed_sets::set_dir(dir.path(), crate::node::signed_sets::MAP)
        .unwrap()
        .join(crate::map::MAP_FILE);
    assert_eq!(std::fs::read(&taken).unwrap(), map);
    // Dated by its generation, as the file it was made from.
    assert_eq!(crate::node::newer::stamp(&taken).unwrap().0, 1_790_000_000);
    assert_eq!(node.inner.map.newest().unwrap().0, taken);
    assert!(node.inner.map.get().is_some());

    peer.shutdown().await;
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_takes_signed_sets_from_any_node_but_only_from_trusted_publishers() {
    use ed25519_dalek::SigningKey;
    use plumb_format::PublicKey;

    let peer_dir = tempfile::tempdir().unwrap();
    let peer_id = plumb_net::load_or_create_key(&peer_dir.path().join("node.key"))
        .unwrap()
        .public()
        .to_peer_id();
    let held = peer_dir.path().join("held");
    let publisher = SigningKey::from_bytes(&[7; 32]);
    let stranger = SigningKey::from_bytes(&[8; 32]);
    // Site rules signed by the publisher the node trusts.
    let psl = peer_dir.path().join("public_suffix_list.dat");
    std::fs::write(&psl, "com\norg\nuk\nco.uk\n").unwrap();
    let rules = crate::node::signed_sets::set_dir(&held, "plumb/site-rules").unwrap();
    plumb_tools::set::site_rules(&plumb_tools::set::SiteRulesArgs {
        psl,
        name: "plumb/site-rules".into(),
        out: rules.clone(),
        title: None,
        license: "MPL-2.0".into(),
        hosts: None,
        host_ranks: None,
        top_hosts: 0,
        level: 3,
    })
    .unwrap();
    plumb_tools::set::sign_set(&rules, &publisher, None, None).unwrap();
    // A denylist signed by a key the node does not trust.
    let entry = plumb_tools::set::deny_entry(&plumb_tools::set::DenyEntryArgs {
        category: "spam".into(),
        target: Some("site:spam.example".into()),
        hash_of: None,
        any_query: false,
        places: Vec::new(),
        id: None,
    })
    .unwrap();
    let entries = peer_dir.path().join("entries.jsonl");
    std::fs::write(&entries, serde_json::to_string(&entry).unwrap() + "\n").unwrap();
    let denylist = crate::node::signed_sets::set_dir(&held, "plumb/denylist").unwrap();
    plumb_tools::set::denylist(&plumb_tools::set::DenylistArgs {
        entries,
        name: "plumb/denylist".into(),
        out: denylist.clone(),
        title: None,
        license: Some("CC0-1.0".into()),
        level: 3,
    })
    .unwrap();
    plumb_tools::set::sign_set(&denylist, &stranger, None, None).unwrap();

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
    let (peer, _records) =
        plumb_net::start(peer_config, Arc::new(WithSignedSets(table, held.clone())))
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
    config.set_publishers = vec![PublicKey::of(&publisher)];
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.search_scope = plumb_net::SearchScope::Anyone;
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.fill = false;
    // Not a node it trusts: what it sends is checked instead.
    net.trusted_peers = Vec::new();
    net.bootstrap = vec![peer_addr.with_p2p(peer_id).unwrap()];
    config.network = Some(net);
    let node = start(config).await.unwrap();

    let taken = crate::node::signed_sets::set_dir(dir.path(), "plumb/site-rules").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !taken.join(crate::sets::MANIFEST).is_file() {
        assert!(Instant::now() < deadline, "the site rules never came");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let checked = crate::sets::check_set(&taken, Some(&PublicKey::of(&publisher)), &[]).unwrap();
    assert_eq!(checked.set, "plumb/site-rules");
    assert_eq!(
        std::fs::read(taken.join(crate::sets::MANIFEST)).unwrap(),
        std::fs::read(rules.join(crate::sets::MANIFEST)).unwrap()
    );
    // The denylist is looked at first in each look, and never taken.
    let refused = crate::node::signed_sets::set_dir(dir.path(), "plumb/denylist").unwrap();
    assert!(!refused.exists());
    assert!(!dir.path().join("sets/plumb/site-rules.new").exists());

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
    write_articles_file(&file, &articles).unwrap();
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
        description: (*views == 10).then(|| {
            let mut state = 17u64;
            (0..65_536)
                .map(|_| {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (b'a' + ((state >> 32) % 26) as u8) as char
                })
                .collect()
        }),
        ..Default::default()
    })
    .collect();
    write_articles_file(&file, &articles).unwrap();
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
    let original_bytes = plumb_core::storage::file_bytes(&file).unwrap();
    assert!(original_bytes > 8192);
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
    assert!(
        !crate::node::newer::prev_path(&file).exists(),
        "a quota cut kept the full source"
    );
    assert!(
        plumb_core::storage::file_bytes(&file).unwrap() < original_bytes,
        "a quota cut did not reduce allocation"
    );
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
    write_articles_file(&file, &articles).unwrap();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_complete_set_timestamp_precedes_the_first_key_and_builds_once() {
    let dir = seeded_dir();
    let set = crate::pages::SetInfo::find("wikipedia-en").unwrap();
    let file = set.file(dir.path());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    write_articles_file(
        &file,
        &[plumb_core::Article {
            title: "Legacy Curie".into(),
            views: 10,
            ..Default::default()
        }],
    )
    .unwrap();
    let bytes = std::fs::read(&file).unwrap();
    let source_modified = 1_700_000_000;
    let notes = crate::pages::SetFileNotes {
        lines: 1,
        complete: true,
        source_modified,
        fetched_at: 0,
        near: 0,
    };
    std::fs::write(
        crate::pages::notes_path(&file),
        serde_json::to_vec(&notes).unwrap(),
    )
    .unwrap();
    assert_ne!(crate::node::newer::stamp(&file).unwrap().0, source_modified);
    let mut config = test_config(dir.path());
    config.settings.page_sets = crate::pages::PageSets::parse("wikipedia-en=all").unwrap();
    let node = start(config).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while node.inner.page_key().is_none() {
        assert!(Instant::now() < deadline, "page index never became ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let first = node.inner.page_key();
    assert_eq!(crate::node::newer::stamp(&file).unwrap().0, source_modified);
    let wanted = crate::pages::Wanted::new(
        dir.path(),
        &node.inner.settings().page_sets,
        node.inner.settings().storage_limit_mb,
    );
    assert_eq!(first, wanted.key());
    // Several maintenance looks, with the downloader running, cannot produce
    // a metadata-only replacement of this first index.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(node.inner.page_key(), first);
    assert_eq!(
        node.inner
            .journal
            .entries()
            .iter()
            .filter(|entry| entry.message.starts_with("Page sets ready:"))
            .count(),
        1
    );
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    // The page index can be in use before the first site index is: until
    // then a search answers with the setup page.
    wait_for(node.addr(), "the first index", |status| {
        status.phase == Phase::Ready
    })
    .await;
    let (_, _, body) = get(node.addr(), "/search?q=legacy+curie").await;
    assert!(body.contains("Legacy_Curie"), "{body}");
    node.shutdown().await.unwrap();
}

#[test]
fn a_held_denylist_stays_in_use_when_a_new_one_would_be_refused() {
    use plumb_format::deny::Switch;
    use plumb_format::PublicKey;

    let dir = tempfile::tempdir().unwrap();
    let publisher = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
    let entry = plumb_tools::set::DenyEntryArgs {
        category: "csam".into(),
        target: None,
        hash_of: Some("https://chase.com/".into()),
        any_query: false,
        places: Vec::new(),
        id: None,
    };
    hold_denylist(dir.path(), &publisher, &[entry]);
    // Taken while the clock read later than it does now, as after it is
    // set back: a list dated this far ahead is refused when taken.
    let set = crate::node::signed_sets::set_dir(dir.path(), "plumb/denylist").unwrap();
    let path = set.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    manifest["generation"] = "2099-01-01T00:00:00Z".into();
    plumb_format::sign::sign(&mut manifest, &publisher).unwrap();
    std::fs::write(&path, manifest.to_string()).unwrap();
    let key = PublicKey::of(&publisher);
    assert!(crate::sets::check_set(&set, Some(&key), &[]).is_err());
    let (list, _) = crate::node::denylist::read_held(dir.path(), &[key], Switch::On)
        .unwrap()
        .unwrap();
    assert!(list.denies_url("https://chase.com/").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_denylist_takes_results_off_and_its_csam_entries_stay_when_off() {
    use plumb_format::PublicKey;

    let dir = seeded_dir();
    let publisher = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
    let entry = |category: &str, target: Option<&str>, hash_of: Option<&str>| {
        plumb_tools::set::DenyEntryArgs {
            category: category.into(),
            target: target.map(str::to_string),
            hash_of: hash_of.map(str::to_string),
            any_query: false,
            places: Vec::new(),
            id: None,
        }
    };
    hold_denylist(
        dir.path(),
        &publisher,
        &[
            entry("copyright", Some("site:usbank.com"), None),
            entry("csam", None, Some("https://chase.com/")),
        ],
    );
    let config = |on: bool| {
        let mut config = test_config(dir.path());
        config.set_publishers = vec![PublicKey::of(&publisher)];
        config.project_denylist = on;
        config
    };
    let domains = |hits: Vec<Hit>| -> Vec<String> { hits.into_iter().map(|h| h.domain).collect() };

    let node = start(config(true)).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;
    let found = domains(search(addr, "us+bank").await);
    assert!(!found.iter().any(|d| d == "usbank.com"), "{found:?}");
    let found = domains(search(addr, "chase").await);
    assert!(!found.iter().any(|d| d == "chase.com"), "{found:?}");
    // Only what an entry names goes.
    let found = domains(search(addr, "chase+center").await);
    assert_eq!(
        found.first().map(String::as_str),
        Some("chasecenter.com"),
        "{found:?}"
    );
    let (_, _, body) = get(addr, "/search?q=us+bank").await;
    assert!(!body.contains("usbank.com"), "{body}");
    node.shutdown().await.unwrap();

    // Turned off, only the csam entry applies.
    let node = start(config(false)).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the index", ready_and_idle).await;
    assert_eq!(search(addr, "us+bank").await[0].domain, "usbank.com");
    let found = domains(search(addr, "chase").await);
    assert!(!found.iter().any(|d| d == "chase.com"), "{found:?}");
    node.shutdown().await.unwrap();

    // A list no publisher the node trusts signed is not applied.
    let stranger = ed25519_dalek::SigningKey::from_bytes(&[10; 32]);
    std::fs::remove_dir_all(
        crate::node::signed_sets::set_dir(dir.path(), "plumb/denylist").unwrap(),
    )
    .unwrap();
    hold_denylist(
        dir.path(),
        &stranger,
        &[entry("copyright", Some("site:usbank.com"), None)],
    );
    let node = start(config(true)).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the index", ready_and_idle).await;
    assert_eq!(search(addr, "us+bank").await[0].domain, "usbank.com");
    node.shutdown().await.unwrap();
}
