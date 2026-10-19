//! Nodes in the network: crawls taken in, network searches, popularity,
//! private search and profiles.

use super::*;

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
    let mut crawled = site::new(domain.as_str());
    crawled.url = Some(format!("https://{domain}/"));
    crawled.title = Some("Lighthouse Keepers Guild".to_string());
    site::set_crawled(&mut crawled, Some(now));
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.search_scope = plumb_net::SearchScope::Anyone;
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    peer_config.bootstrap = vec![node_addr.clone()];
    // The peer trusts the node, so it may fill its space from it.
    peer_config.trusted_peers = vec![net_status.peer_id.parse().unwrap()];
    let table = plumb_net::BucketTable::build(
        &peer_dir.path().join("buckets"),
        &stored_items(&[crawled.clone()]),
    )
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
            .any(|f| f.site.domain() == "usbank.com"),
        "{from_node:?}"
    );

    // The peer publishes its crawl; the node holds it until a second
    // crawler agrees, then keeps it and, at its next refresh, searches it
    // from its own index.
    let mut published = None;
    for _ in 0..100 {
        published = peer
            .publish(listed(std::slice::from_ref(&crawled)))
            .await
            .unwrap();
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
    let empty = plumb_net::BucketTable::build(
        &second_dir.path().join("buckets"),
        &[] as &[plumb_core::site::Item],
    )
    .unwrap();
    let (second, _records) = plumb_net::start(second_config, Arc::new(empty))
        .await
        .unwrap();
    for _ in 0..100 {
        second
            .publish(listed(std::slice::from_ref(&crawled)))
            .await
            .unwrap();
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
    let mut crawled = site::new("reddit.com");
    crawled.url = Some("https://www.reddit.com/".to_string());
    crawled.title = Some("Reddit".to_string());
    crawled.about = Some("Communities for every interest, from news to hobbies".to_string());
    site::set_crawled(&mut crawled, Some(now));
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.search_scope = plumb_net::SearchScope::Anyone;
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    peer_config.bootstrap = vec![node_addr];
    let table = plumb_net::BucketTable::build(
        &peer_dir.path().join("buckets"),
        &stored_items(&[crawled.clone()]),
    )
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
    assert!(peer
        .publish(listed(std::slice::from_ref(&crawled)))
        .await
        .unwrap()
        .is_some());

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
    network::keep_found(&node.inner, listed(std::slice::from_ref(&crawled)));
    let kept = inbox();
    assert_eq!(kept.lines().count(), 1, "{kept}");
    assert!(kept.contains("Communities for every"), "{kept}");
    network::keep_found(&node.inner, listed(std::slice::from_ref(&crawled)));
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
async fn network_results_go_through_the_denylist() {
    let dir = seeded_dir();
    let publisher = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
    let deny = |target: &str| plumb_tools::set::DenyEntryArgs {
        category: "copyright".into(),
        target: Some(target.into()),
        hash_of: None,
        any_query: false,
        places: Vec::new(),
        id: None,
    };
    hold_denylist(
        dir.path(),
        &publisher,
        &[
            deny("site:lighthouse-wreckers.org"),
            deny("url:https://lighthouse-keepers.org/wreck-maps"),
            deny("url:https://www.facebook.com/marketplace/"),
        ],
    );
    let mut config = test_config(dir.path());
    config.set_publishers = vec![plumb_format::PublicKey::of(&publisher)];
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

    // Another node holds a site this node's denylist takes off, one with
    // a key page it takes off, and one whose key pages the index fills in
    // (plumb_core::key_pages_or_known), one of them denied.
    let titled = |domain: &str, title: &str| {
        let mut record = site::new(domain);
        record.url = Some(format!("https://{domain}/"));
        record.title = Some(title.to_string());
        record
    };
    let mut keepers = titled("lighthouse-keepers.org", "Lighthouse Keepers Guild");
    let page = |label: &str, path: &str| plumb_core::KeyPage {
        label: label.into(),
        url: format!("https://lighthouse-keepers.org/{path}"),
    };
    site::set_key_pages(
        &mut keepers,
        vec![
            page("Visiting hours", "hours"),
            page("Wreck maps", "wreck-maps"),
        ],
    );
    let wreckers = titled("lighthouse-wreckers.org", "Lighthouse Wreckers");
    let mut facebook = titled("facebook.com", "Lighthouse Fans on Facebook");
    facebook.url = Some("https://www.facebook.com/".into());
    let peer_dir = tempfile::tempdir().unwrap();
    let mut peer_config = plumb_net::NetConfig::new(peer_dir.path().to_path_buf());
    peer_config.search_scope = plumb_net::SearchScope::Anyone;
    peer_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    peer_config.upnp = false;
    peer_config.local_discovery = false;
    peer_config.round_every = None;
    peer_config.bootstrap = vec![node_addr];
    let records = [keepers, wreckers, facebook];
    let table =
        plumb_net::BucketTable::build(&peer_dir.path().join("buckets"), &stored_items(&records))
            .unwrap();
    let (peer, _records) = plumb_net::start(peer_config, Arc::new(table))
        .await
        .unwrap();
    wait_for(addr, "the peer to connect", |s| {
        s.network.as_ref().is_some_and(|n| n.connected_peers >= 1)
    })
    .await;

    let mut body = String::new();
    for _ in 0..100 {
        let (code, _, text) = get(addr, "/api/network/search?q=lighthouse").await;
        assert_eq!(code, 200, "{text}");
        body = text;
        if body.contains("Lighthouse Keepers Guild") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(body.contains("Lighthouse Keepers Guild"), "{body}");
    assert!(body.contains("/hours"), "{body}");
    assert!(!body.contains("lighthouse-wreckers.org"), "{body}");
    assert!(!body.contains("wreck-maps"), "{body}");
    assert!(body.contains("facebook.com/help/"), "{body}");
    assert!(!body.contains("marketplace"), "{body}");
    for path in ["/network?q=lighthouse", "/search?q=lighthouse&net=1"] {
        let (_, _, page) = get(addr, path).await;
        assert!(page.contains("Lighthouse Keepers Guild"), "{path}: {page}");
        assert!(!page.contains("lighthouse-wreckers.org"), "{path}: {page}");
        assert!(!page.contains("wreck-maps"), "{path}: {page}");
        assert!(!page.contains("facebook.com/marketplace"), "{path}: {page}");
    }

    // A site graded spam loses 90% of its score, as in the node's own
    // results.
    let score = |body: &str| {
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        let hits = body["hits"].as_array().unwrap().clone();
        let hit = hits
            .into_iter()
            .find(|hit| hit["domain"] == "lighthouse-keepers.org");
        hit.unwrap()["score"].as_f64().unwrap()
    };
    let ungraded = score(&body);
    hold_grades(
        dir.path(),
        &publisher,
        &[("lighthouse-keepers.org", "spam")],
    );
    node.inner.grades_from_set();
    let (_, _, body) = get(addr, "/api/network/search?q=lighthouse").await;
    assert!((score(&body) / ungraded - 0.1).abs() < 1e-5, "{body}");

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
    let source = plumb_net::BucketTable::build(
        &peer_dir.path().join("buckets"),
        &stored_items(&fixture_sites()),
    )
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
    assert!(!get(addr, "/").await.2.contains("href=\"/private?"));
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
        assert!(get(addr, "/").await.2.contains("href=\"/private?"));
    } else {
        assert_eq!(code, 503, "{body}");
    }
    node.shutdown().await.unwrap();
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
