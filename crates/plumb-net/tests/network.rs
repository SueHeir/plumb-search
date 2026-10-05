//! Several nodes on this machine: crawl batches spread from one to all and
//! count once two crawlers agree,
//! a network search is answered with proofs, a node behind a relay is
//! reachable through it, and a late node catches up.

use std::sync::Arc;
use std::time::Duration;

use plumb_core::{now_unix, SiteRecord};
use plumb_net::assign::{epoch_of, is_assigned, MAX_SHARE_PPM};
use plumb_net::{BucketSource, BucketTable, Multiaddr, NetConfig, NetHandle, PeerId, SearchScope};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;

/// Serves the buckets of the records it was given, as a node with an
/// index of them would.
fn table(dir: &std::path::Path, records: &[SiteRecord]) -> Arc<dyn BucketSource> {
    Arc::new(BucketTable::build(&dir.join("buckets"), records).unwrap())
}

struct Node {
    handle: NetHandle,
    records: UnboundedReceiver<Vec<SiteRecord>>,
    _dir: TempDir,
}

impl Node {
    async fn start(relay: bool, bootstrap: Vec<Multiaddr>, local: Vec<SiteRecord>) -> Node {
        Self::start_with(relay, bootstrap, local, true).await
    }

    /// `listen: false` makes a node only other nodes' relays can reach,
    /// like one behind a home router.
    async fn start_with(
        relay: bool,
        bootstrap: Vec<Multiaddr>,
        local: Vec<SiteRecord>,
        listen: bool,
    ) -> Node {
        let dir = tempfile::tempdir().unwrap();
        Self::start_config(dir, relay, bootstrap, local, listen, |_| {}).await
    }

    /// [`Node::start_with`] in `dir`, with `tweak` applied to the config
    /// last.
    async fn start_config(
        dir: TempDir,
        relay: bool,
        bootstrap: Vec<Multiaddr>,
        local: Vec<SiteRecord>,
        listen: bool,
        tweak: impl FnOnce(&mut NetConfig),
    ) -> Node {
        let mut config = NetConfig::new(dir.path().to_path_buf());
        config.listen = if listen {
            vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()]
        } else {
            vec![]
        };
        config.upnp = false;
        config.local_discovery = false;
        config.relay_server = relay;
        config.bootstrap = bootstrap;
        // Background rounds would make the counts below drift; the test
        // of them turns them on.
        config.round_every = None;
        // Most tests are about nodes that do not trust each other.
        config.search_scope = SearchScope::Anyone;
        tweak(&mut config);
        let source = table(dir.path(), &local);
        let (handle, records) = plumb_net::start(config, source).await.unwrap();
        Node {
            handle,
            records,
            _dir: dir,
        }
    }

    /// Its TCP address with `/p2p/<id>`.
    async fn addr(&self) -> Multiaddr {
        let status = wait_for(|| {
            let status = self.handle.status();
            (!status.listening.is_empty()).then_some(status)
        })
        .await;
        let addr: Multiaddr = status.listening[0].parse().unwrap();
        addr.with_p2p(self.handle.peer_id()).unwrap()
    }

    async fn next_records(&mut self) -> Vec<SiteRecord> {
        tokio::time::timeout(Duration::from_secs(30), self.records.recv())
            .await
            .expect("records within 30 s")
            .expect("the node is running")
    }
}

async fn wait_for<T>(mut check: impl FnMut() -> Option<T>) -> T {
    for _ in 0..300 {
        if let Some(value) = check() {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("gave up waiting");
}

/// A crawled homepage all of `peers` are assigned today, whose title
/// contains `word`.
fn crawled_for(peers: &[PeerId], word: &str) -> SiteRecord {
    let now = now_unix();
    let domain = (0..)
        .map(|i| format!("{word}{i}.com"))
        .find(|d| {
            peers
                .iter()
                .all(|peer| is_assigned(epoch_of(now), peer, d, MAX_SHARE_PPM))
        })
        .unwrap();
    let mut record = SiteRecord::new(domain.as_str());
    record.url = Some(format!("https://{domain}/"));
    record.title = Some(format!("The {word} site"));
    record.crawled_at = Some(now);
    record
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nodes_share_batches_search_each_other_and_reach_through_a_relay() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("plumb_net=debug")
        .with_test_writer()
        .try_init();

    let mut relay = Node::start(true, vec![], vec![]).await;
    let relay_addr = relay.addr().await;

    // A crawls and publishes; B hears of it but holds it until a second
    // crawler, the relay, publishes the same. (Another node would do, but
    // every extra node spends the relay's per-address circuit budget, which
    // all of these share on 127.0.0.1.)
    let a = Node::start(false, vec![relay_addr.clone()], vec![]).await;
    let mut b = Node::start(false, vec![relay_addr.clone()], vec![]).await;
    for node in [&a, &b] {
        wait_for(|| (node.handle.status().connected_peers >= 1).then_some(())).await;
    }
    // Gossip needs the mesh to form before a publish reaches anyone; a
    // header published too early is announced again once a node subscribes.
    let published = vec![crawled_for(
        &[a.handle.peer_id(), relay.handle.peer_id()],
        "harbor",
    )];
    let id = a.handle.publish(published.clone()).await.unwrap().unwrap();
    wait_for(|| (b.handle.status().agreement.pending_sites == 1).then_some(())).await;
    assert!(b.records.try_recv().is_err(), "one crawler is not enough");
    let mut again = published.clone();
    again[0].title = Some(format!("{}!", again[0].title.as_deref().unwrap()));
    wait_for(|| (relay.handle.status().agreement.pending_sites == 1).then_some(())).await;
    assert!(
        relay.records.try_recv().is_err(),
        "one crawler is not enough"
    );
    relay.handle.publish(again).await.unwrap().unwrap();
    let got = b.next_records().await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].domain, published[0].domain);
    assert_eq!(got[0].title.as_deref(), Some("The harbor site!"));
    assert_eq!(b.handle.status().agreement.confirmed_sites, 1);
    // The relay's own crawl confirmed A's; what that confirms it already has.
    assert_eq!(relay.handle.status().agreement.confirmed_sites, 1);
    let relay_got = relay;
    assert!(b.handle.status().batches_held >= 1, "{id}");

    // C answers searches from an index holding A's crawl, with a proof
    // from the batch it received.
    let mut c = Node::start(false, vec![relay_addr.clone()], published.clone()).await;
    let caught_up = c.next_records().await;
    assert_eq!(
        caught_up[0].domain, published[0].domain,
        "catches up on join"
    );
    wait_for(|| (b.handle.status().connected_peers >= 2).then_some(())).await;

    let mut result = None;
    for _ in 0..50 {
        b.handle.clear_search_cache();
        let found = b
            .handle
            .search("harbor", Duration::from_secs(5))
            .await
            .unwrap();
        if found.found.iter().any(|h| h.verified) {
            result = Some(found);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let result = result.expect("a verified hit from C");
    let hit = &result.found[0];
    assert_eq!(hit.record.domain, published[0].domain);
    assert_eq!(result.buckets, plumb_net::bucket::BUCKETS_PER_SEARCH);
    let crawlers = [
        a.handle.peer_id().to_string(),
        relay_got.handle.peer_id().to_string(),
    ];
    assert!(crawlers.iter().any(|c| hit.crawler.as_deref() == Some(c)));
    // C holds both crawls and answers with both proofs.
    assert!(hit.confirmed, "{hit:?}");
    assert_eq!(hit.crawlers.len(), 2);
    assert_eq!(result.rejected, 0);
    // With other nodes to relay, every request went through one.
    assert_eq!(result.direct, 0, "{result:?}");
    assert_eq!(result.relayed, result.answered, "{result:?}");

    // B kept the buckets it fetched: the same search again is answered on
    // B, without asking any node.
    let again = b
        .handle
        .search("harbor", Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(again.asked, 0, "{again:?}");
    assert_eq!(again.buckets, 0, "{again:?}");
    assert!(again.cached > 0, "{again:?}");
    assert_eq!(again.found, result.found);

    // C is behind the relay: it holds a reservation, and a node that only
    // knows the relay's address reaches C through it. That includes a
    // throwaway identity asking for a bucket.
    wait_for(|| (!c.handle.status().relays.is_empty()).then_some(())).await;
    let circuit = relay_addr
        .clone()
        .with(plumb_net::Protocol::P2pCircuit)
        .with_p2p(c.handle.peer_id())
        .unwrap();
    let peer = plumb_net::search::BucketPeer {
        peer: c.handle.peer_id(),
        addrs: vec![circuit.clone()],
        oblivious: true,
    };
    let bucket = plumb_net::bucket::bucket_of("harbor");
    let served_before = c.handle.status().buckets_served;
    let answer = plumb_net::search::fetch_bucket(&peer, bucket, Duration::from_secs(10))
        .await
        .expect("a bucket fetched over the relay");
    let records = answer.records.expect("C serves buckets");
    assert!(records.iter().any(|r| r.proof.is_some()));
    assert_eq!(c.handle.status().buckets_served, served_before + 1);

    let d = Node::start(false, vec![], vec![]).await;
    d.handle.dial(circuit).unwrap();
    let mut through_relay = None;
    for _ in 0..50 {
        // Peers discovered before C may give a valid negative answer. This
        // connectivity test deliberately refreshes while discovery completes;
        // ordinary repeated searches now retain that negative cache answer.
        d.handle.clear_search_cache();
        let found = d
            .handle
            .search("harbor", Duration::from_secs(5))
            .await
            .unwrap();
        if !found.found.is_empty() {
            through_relay = Some(found);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let through_relay = through_relay.expect("D searches C after meeting it over the relay");
    assert!(through_relay.found[0].verified);

    // E cannot be dialed at all, only reached through the relay, and the
    // relay itself searches it. The relay learns of E's relayed address
    // from the reservation, after E first introduced itself.
    let e = Node::start_with(false, vec![relay_addr.clone()], published.clone(), false).await;
    wait_for(|| (!e.handle.status().relays.is_empty()).then_some(())).await;
    let mut reached_e = false;
    for _ in 0..50 {
        relay_got.handle.clear_search_cache();
        let _ = relay_got
            .handle
            .search("harbor", Duration::from_secs(5))
            .await
            .unwrap();
        if e.handle.status().buckets_served > 0 {
            reached_e = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(reached_e, "the relay's search reaches E through itself");

    // F is behind NAT too. Two such nodes find each other through the
    // network (they only know the relay) and meet over a relayed
    // connection, so F's searches reach E.
    let f = Node::start_with(false, vec![relay_addr.clone()], vec![], false).await;
    let e_id = e.handle.peer_id();
    wait_for(|| (!f.handle.status().relays.is_empty()).then_some(())).await;
    let served_before = e.handle.status().buckets_served;
    let mut f_reached_e = false;
    let mut last = None;
    for _ in 0..100 {
        f.handle.clear_search_cache();
        last = Some(
            f.handle
                .search("harbor", Duration::from_secs(5))
                .await
                .unwrap(),
        );
        if e.handle.status().buckets_served > served_before {
            f_reached_e = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(
        f_reached_e,
        "F finds E ({e_id}) and searches it: {:?} {last:?}",
        f.handle.status()
    );

    for node in [a, b, c, d, e, f, relay_got] {
        node.handle.shutdown().await;
    }
}

/// A search goes through a relay: the node answering gets a sealed request
/// from the relay, the relay hands everyone the same key for it, and
/// cannot pass off a key of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bucket_requests_go_sealed_through_a_relay() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("plumb_net=debug")
        .with_test_writer()
        .try_init();

    let relay = Node::start(true, vec![], vec![]).await;
    let relay_addr = relay.addr().await;
    // The crawler publishes a signed crawl; the holder catches up on it
    // and answers from an index holding it, as C does above.
    let crawler = Node::start(false, vec![relay_addr.clone()], vec![]).await;
    let records = vec![crawled_for(&[crawler.handle.peer_id()], "lantern")];
    crawler
        .handle
        .publish(records.clone())
        .await
        .unwrap()
        .unwrap();
    let holder = Node::start(false, vec![relay_addr.clone()], records.clone()).await;
    // Its proof comes from the batch, held whether or not a second
    // crawler has agreed yet.
    wait_for(|| (holder.handle.status().batches_held >= 1).then_some(())).await;
    let asker = Node::start(false, vec![relay_addr.clone()], vec![]).await;
    wait_for(|| (asker.handle.status().connected_peers >= 3).then_some(())).await;

    // The relay hands out the holder's own key, the same one each time.
    let target = holder.handle.peer_id();
    let own = holder.handle.oblivious_keys(target).await.unwrap().unwrap();
    let mut via_relay = None;
    for _ in 0..50 {
        via_relay = relay.handle.oblivious_keys(target).await.unwrap();
        if via_relay.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let via_relay = via_relay.expect("the relay fetches the key");
    assert_eq!(via_relay, own);
    assert_eq!(
        relay.handle.oblivious_keys(target).await.unwrap().unwrap(),
        own
    );

    let served_before = holder.handle.status().buckets_served;
    let mut result = None;
    for _ in 0..50 {
        asker.handle.clear_search_cache();
        let found = asker
            .handle
            .search("lantern", Duration::from_secs(5))
            .await
            .unwrap();
        if found.found.iter().any(|f| f.verified) {
            result = Some(found);
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let result = result.expect("a verified hit through a relay");
    assert_eq!(result.found[0].record.domain, records[0].domain);
    assert_eq!(result.direct, 0, "{result:?}");
    assert_eq!(result.relayed, result.answered, "{result:?}");
    assert!(holder.handle.status().buckets_served > served_before);
    let relayed: u64 = [&relay, &crawler, &holder]
        .iter()
        .map(|n| n.handle.status().requests_relayed)
        .sum();
    assert!(relayed >= result.answered as u64, "{relayed} {result:?}");

    for node in [relay, crawler, holder, asker] {
        node.handle.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn popularity_reports_spread_and_are_read_once_enough_are_sent() {
    use plumb_net::popularity::{report_epoch, REPORT_THRESHOLD};
    use plumb_net::Report;

    let _ = tracing_subscriber::fmt()
        .with_env_filter("plumb_net=debug")
        .with_test_writer()
        .try_init();

    let relay = Node::start(true, vec![], vec![]).await;
    let relay_addr = relay.addr().await;
    let a = Node::start(false, vec![relay_addr.clone()], vec![]).await;
    let b = Node::start(false, vec![relay_addr.clone()], vec![]).await;
    for node in [&a, &b] {
        wait_for(|| (node.handle.status().connected_peers >= 1).then_some(())).await;
    }
    // A needs to know both others can relay to send through one to the
    // other.
    wait_for(|| (a.handle.status().relaying_peers >= 2).then_some(())).await;

    // A hands in reports of one pick, each under a throwaway identity
    // and through a relay.
    let epoch = report_epoch(now_unix());
    let wait = Duration::from_secs(10);
    let send = |n: u32| {
        let a = &a;
        async move {
            for _ in 0..n {
                let report = Report::new(epoch, "us bank", "usbank.com").unwrap();
                a.handle.send_report(&report, wait).await.unwrap();
            }
        }
    };
    send(REPORT_THRESHOLD - 1).await;
    let held = REPORT_THRESHOLD as usize - 1;
    wait_for(|| (b.handle.status().reports_held >= held).then_some(())).await;
    assert!(b.handle.recount().await.unwrap().is_empty());

    send(1).await;
    wait_for(|| (b.handle.status().reports_held > held).then_some(())).await;
    let table = b.handle.recount().await.unwrap();
    assert_eq!(table.picks.len(), 1, "{table:?}");
    assert_eq!(table.picks[0].domain, "usbank.com");
    assert!(table.bonus("US Bank", "usbank.com") > 0.0);
    assert_eq!(a.handle.status().reports_sent, u64::from(REPORT_THRESHOLD));
    // Each went sealed through the other node, so the one it was handed to
    // never saw A's address.
    let relayed = relay.handle.status().requests_relayed + b.handle.status().requests_relayed;
    assert_eq!(relayed, u64::from(REPORT_THRESHOLD));

    // A node that joins later catches up on the week's reports.
    let c = Node::start(false, vec![relay_addr], vec![]).await;
    wait_for(|| (c.handle.status().reports_held > held).then_some(())).await;
    assert_eq!(c.handle.recount().await.unwrap().picks, table.picks);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_says_who_it_is_connected_to_and_why_it_is_not() {
    // A port nothing listens on: the bootstrap node is down.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let gone: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}/p2p/{}", PeerId::random())
        .parse()
        .unwrap();
    let alone = Node::start(false, vec![gone], vec![]).await;
    let problem = wait_for(|| alone.handle.status().problem).await;
    assert!(
        problem.message.contains("turned the connection away"),
        "{problem:?}"
    );
    assert!(alone.handle.status().alone_since.is_some());
    alone.handle.reconnect().unwrap();

    let relay = Node::start(true, vec![], vec![]).await;
    let joined = Node::start(false, vec![relay.addr().await], vec![]).await;
    let status = wait_for(|| {
        let status = joined.handle.status();
        (!status.peers.is_empty()).then_some(status)
    })
    .await;
    let peer = &status.peers[0];
    assert_eq!(peer.peer_id, relay.handle.peer_id().to_string());
    assert!(peer.bootstrap);
    // Over loopback, which counts as this network.
    assert_eq!(peer.route, plumb_net::Route::Nearby);
    assert_eq!(status.nearby_peers, 1);
    assert_eq!(status.problem, None);
    assert_eq!(status.alone_since, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmed_crawls_earn_credits_that_buy_tokens() {
    use plumb_net::agree::MIN_JUDGED;
    use plumb_net::credits::credits_for;

    let _ = tracing_subscriber::fmt()
        .with_env_filter("plumb_net=debug")
        .with_test_writer()
        .try_init();

    let r = Node::start(true, vec![], vec![]).await;
    let r_addr = r.addr().await;
    let a = Node::start(false, vec![r_addr.clone()], vec![]).await;
    let b = Node::start(false, vec![r_addr], vec![]).await;
    for node in [&a, &b] {
        wait_for(|| (node.handle.status().connected_peers >= 1).then_some(())).await;
    }
    let peers = [a.handle.peer_id(), r.handle.peer_id()];

    // Before crawling anything, A's crawls do not count at R: no tokens.
    let at_r = a.handle.credits_at(r.handle.peer_id()).await.unwrap();
    assert_eq!((at_r.credits, at_r.counts), (0, false));
    assert!(a
        .handle
        .collect_tokens(r.handle.peer_id(), 4)
        .await
        .is_err());

    // A and R crawl the same sites and agree on every one.
    let sites: Vec<SiteRecord> = (0..MIN_JUDGED)
        .map(|i| crawled_for(&peers, &format!("credit{i}x")))
        .collect();
    a.handle.publish(sites.clone()).await.unwrap().unwrap();
    let n = sites.len();
    wait_for(|| (r.handle.status().agreement.pending_sites == n).then_some(())).await;
    r.handle.publish(sites.clone()).await.unwrap().unwrap();
    let earned = i64::from(MIN_JUDGED) * credits_for(now_unix());
    // R counts every one of A's crawls: each matched R's own. R vouches for
    // A only after A matched 3 of its crawls, so R's own crawls of those
    // first sites had no witness yet; A's ledger is much the same.
    let mut at_r = a.handle.credits_at(r.handle.peer_id()).await.unwrap();
    for _ in 0..100 {
        if at_r.credits == earned {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        at_r = a.handle.credits_at(r.handle.peer_id()).await.unwrap();
    }
    assert_eq!((at_r.credits, at_r.counts), (earned, true));
    wait_for(|| (a.handle.status().credits.balance > 0).then_some(())).await;
    let own = a.handle.status().credits;
    assert!(own.balance < earned, "{own:?}");
    assert!(r.handle.status().credits.balance < earned);

    // R sells A tokens for them.
    let got = a
        .handle
        .collect_tokens(r.handle.peer_id(), 8)
        .await
        .unwrap();
    assert_eq!(got, 8);
    assert_eq!(a.handle.tokens_held(&r.handle.peer_id()), 8);
    let at_r = a.handle.credits_at(r.handle.peer_id()).await.unwrap();
    assert_eq!(at_r.credits, earned - 8);
    wait_for(|| (r.handle.status().credits.tokens_issued == 8).then_some(())).await;
    wait_for(|| (a.handle.status().credits.tokens_held == 8).then_some(())).await;
    // Asking for more than is left gets what is left.
    let rest = usize::try_from(earned - 8).unwrap();
    let got = a
        .handle
        .collect_tokens(r.handle.peer_id(), 64)
        .await
        .unwrap();
    assert_eq!(got, rest);

    // B never crawled: R has nothing for it, and A's credits are A's.
    let refused = b.handle.collect_tokens(r.handle.peer_id(), 1).await;
    assert!(refused.is_err(), "{refused:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_node_answers_searches_that_spend_its_tokens() {
    use plumb_net::agree::MIN_JUDGED;

    let _ = tracing_subscriber::fmt()
        .with_env_filter("plumb_net=debug")
        .with_test_writer()
        .try_init();

    // Keys first, to pick sites both are assigned.
    let (a_dir, r_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let key = |dir: &TempDir| {
        plumb_net::load_or_create_key(&dir.path().join("node.key"))
            .unwrap()
            .public()
            .to_peer_id()
    };
    let peers = [key(&a_dir), key(&r_dir)];
    let sites: Vec<SiteRecord> = (0..MIN_JUDGED)
        .map(|i| crawled_for(&peers, &format!("busy{i}x")))
        .collect();

    // R holds those sites and is always busy: it answers nothing for free.
    let r = Node::start_config(r_dir, true, vec![], sites.clone(), true, |c| {
        c.max_answering = 0;
    })
    .await;
    let r_addr = r.addr().await;
    let a = Node::start_config(a_dir, false, vec![r_addr], vec![], true, |_| {}).await;
    wait_for(|| (a.handle.status().connected_peers >= 1).then_some(())).await;
    let r = &r.handle;

    // A and R crawl the same sites, so R counts credits for A.
    a.handle.publish(sites.clone()).await.unwrap().unwrap();
    let n = sites.len();
    wait_for(|| (r.status().agreement.pending_sites == n).then_some(())).await;
    r.publish(sites.clone()).await.unwrap().unwrap();
    let mut credits = 0;
    for _ in 0..100 {
        credits = a.handle.credits_at(r.peer_id()).await.unwrap().credits;
        if credits > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(credits > 0);

    // Without tokens, R turns every request away.
    let wait = Duration::from_secs(10);
    let free = a.handle.search("busy0x", wait).await.unwrap();
    assert!(free.busy > 0, "{free:?}");
    assert_eq!(free.priority, 0);
    assert_eq!(free.answered, 0, "{free:?}");

    // With tokens, R answers them, spending one each.
    let got = a.handle.collect_tokens(r.peer_id(), 8).await.unwrap();
    assert_eq!(got, 8);
    let paid = a.handle.search("busy0x", wait).await.unwrap();
    assert!(paid.priority > 0, "{paid:?}");
    assert_eq!(paid.priority, paid.answered, "{paid:?}");
    assert!(paid
        .found
        .iter()
        .any(|s| s.record.domain == sites[0].domain));
    let spent = paid.priority;
    assert_eq!(a.handle.tokens_held(&r.peer_id()), 8 - spent);
    wait_for(|| (a.handle.status().credits.tokens_spent == spent as u64).then_some(())).await;
    wait_for(|| (r.status().credits.priority_answered == spent as u64).then_some(())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nodes_send_rounds_of_bucket_requests_without_searching() {
    // Three nodes that answer searches, so every request can go through a
    // third one; A also sends background rounds, often.
    let r = Node::start(true, vec![], vec![crawled_for(&[], "quay")]).await;
    let r_addr = r.addr().await;
    let h = Node::start(false, vec![r_addr.clone()], vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let a = Node::start_config(dir, false, vec![r_addr], vec![], false, |c| {
        c.round_every = Some(Duration::from_millis(300));
    })
    .await;
    assert_eq!(a.handle.status().rounds.every_secs, Some(0));
    assert_eq!(h.handle.status().rounds.every_secs, None);
    wait_for(|| (a.handle.status().relaying_peers >= 2).then_some(())).await;

    // Nobody searched, yet A asks the others for buckets, a round at a
    // time, sealed through a relay, as a search would.
    wait_for(|| {
        let status = a.handle.status();
        (status.rounds.sent >= 3 && status.rounds.answers >= 6).then_some(())
    })
    .await;
    assert_eq!(h.handle.status().rounds.sent, 0);
    wait_for(|| {
        let served = r.handle.status().buckets_served + h.handle.status().buckets_served;
        let relayed = r.handle.status().requests_relayed + h.handle.status().requests_relayed;
        (served >= 6 && relayed >= 6).then_some(())
    })
    .await;

    // A cache miss queues real buckets for the next independently due round.
    // The search sends no requests itself, even while waiting for that round.
    let found = a
        .handle
        .search("quay", Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(found.asked, 0, "{found:?}");
    assert_eq!(found.buckets, 0, "{found:?}");
    assert_eq!(found.pending, 0, "{found:?}");
    assert!(found
        .found
        .iter()
        .any(|s| s.record.domain.starts_with("quay")));

    a.handle.shutdown().await;
    h.handle.shutdown().await;
    r.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_cache_misses_send_nothing_before_the_scheduled_deadline() {
    let server = Node::start(false, vec![], vec![]).await;
    let addr = server.addr().await;
    let asker = Node::start_config(
        tempfile::tempdir().unwrap(),
        false,
        vec![addr],
        vec![],
        true,
        |config| config.round_every = Some(Duration::from_secs(60)),
    )
    .await;
    wait_for(|| (asker.handle.status().connected_peers >= 1).then_some(())).await;
    for query in (0..24).map(|i| format!("uncached{i}")) {
        let found = asker
            .handle
            .search(&query, Duration::from_millis(3))
            .await
            .unwrap();
        assert_eq!(found.asked, 0, "{found:?}");
        assert_eq!(found.buckets, 0, "{found:?}");
        assert!(found.pending > 0, "{found:?}");
    }
    assert_eq!(asker.handle.status().rounds.sent, 0);
    assert_eq!(server.handle.status().buckets_served, 0);
    asker.handle.shutdown().await;
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduled_empty_answers_are_reused_without_search_triggered_fetches() {
    let server = Node::start(false, vec![], vec![]).await;
    let addr = server.addr().await;
    let asker = Node::start_config(
        tempfile::tempdir().unwrap(),
        false,
        vec![],
        vec![],
        true,
        |config| config.round_every = Some(Duration::from_millis(500)),
    )
    .await;
    // A due slot with no peers must leave queued IDs in place.
    let missing = asker
        .handle
        .search("emptyneedle", Duration::from_millis(600))
        .await
        .unwrap();
    assert!(missing.pending > 0);
    assert_eq!(asker.handle.status().rounds.sent, 0);
    asker.handle.dial(addr).unwrap();
    wait_for(|| (asker.handle.status().connected_peers >= 1).then_some(())).await;
    let answer = asker
        .handle
        .search("emptyneedle", Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(answer.pending, 0, "{answer:?}");
    assert!(answer.cached > 0, "{answer:?}");
    assert!(answer.found.is_empty(), "{answer:?}");
    assert_eq!(answer.asked, 0);
    let before = server.handle.status().buckets_served;
    for _ in 0..16 {
        let again = tokio::time::timeout(
            Duration::from_millis(100),
            asker.handle.search("emptyneedle", Duration::from_secs(5)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(again.pending, 0);
        assert_eq!(again.asked, 0);
        assert!(again.cached > 0);
    }
    assert_eq!(server.handle.status().buckets_served, before);
    asker.handle.shutdown().await;
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_fills_its_space_from_a_node_it_trusts_and_no_other() {
    // S holds crawls of many sites; F trusts S, U trusts nobody.
    let now = now_unix();
    let mut local: Vec<SiteRecord> = (0..30u32)
        .map(|i| {
            let mut record = SiteRecord::new(format!("filler{i}.com"));
            record.signals.tranco_rank = Some(i + 1);
            if i % 3 != 1 {
                record.title = Some(format!("Filler {i}"));
                record.crawled_at = Some(now - 3_600);
            }
            record
        })
        .collect();
    local.reverse();
    let s = Node::start(true, vec![], local).await;
    let s_id = s.handle.peer_id();
    let s_addr = s.addr().await;
    let f = Node::start_config(
        tempfile::tempdir().unwrap(),
        false,
        vec![s_addr.clone()],
        vec![],
        true,
        |c| c.trusted_peers = vec![s_id],
    )
    .await;
    let u = Node::start_config(
        tempfile::tempdir().unwrap(),
        false,
        vec![s_addr],
        vec![],
        true,
        |c| c.trusted_peers = vec![],
    )
    .await;

    // Best-ranked first, crawled sites only, a stretch at a time.
    let first = loop {
        if let Some(page) = f.handle.fill(None, 0, 5, false).await.unwrap() {
            break page;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(first.peer, s_id);
    assert_eq!(first.total, 30);
    let domains: Vec<&str> = first.records.iter().map(|r| r.domain.as_str()).collect();
    assert_eq!(
        domains,
        [
            "filler0.com",
            "filler2.com",
            "filler3.com",
            "filler5.com",
            "filler6.com"
        ]
    );
    assert!(first.records.iter().all(|r| r.crawled_at.is_some()));
    // A node setting up asks for every site, crawled or not.
    let all = f
        .handle
        .fill(Some(s_id), 0, 5, true)
        .await
        .unwrap()
        .unwrap();
    let domains: Vec<&str> = all.records.iter().map(|r| r.domain.as_str()).collect();
    assert_eq!(
        domains,
        [
            "filler0.com",
            "filler1.com",
            "filler2.com",
            "filler3.com",
            "filler4.com"
        ]
    );
    assert_eq!(all.records[1].signals.tranco_rank, Some(2));
    assert_eq!(all.next, 5);
    let mut filled = first.records.len();
    let mut from = first.next;
    loop {
        let page = f
            .handle
            .fill(Some(s_id), from, 5, false)
            .await
            .unwrap()
            .unwrap();
        if page.busy {
            // Asked too often this minute: S turns F away for now.
            break;
        }
        filled += page.records.len();
        from = page.next;
        if page.done() {
            break;
        }
    }
    assert!(filled >= 10, "filled {filled}");
    wait_for(|| (s.handle.status().fill_records_served >= 10).then_some(())).await;

    // U is connected to S too, but takes nothing from a node it does not
    // trust.
    wait_for(|| (u.handle.status().connected_peers >= 1).then_some(())).await;
    assert!(u.handle.fill(None, 0, 5, false).await.unwrap().is_none());

    f.handle.shutdown().await;
    u.handle.shutdown().await;
    s.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn searches_ask_trusted_nodes_friends_of_friends_or_anyone() {
    // A trusts S, S trusts F; nobody trusts X. All meet through S.
    let f = Node::start(false, vec![], vec![]).await;
    let f_id = f.handle.peer_id();
    let s = Node::start_config(
        tempfile::tempdir().unwrap(),
        true,
        vec![],
        vec![],
        true,
        |c| c.trusted_peers = vec![f_id],
    )
    .await;
    let s_id = s.handle.peer_id();
    let s_addr = s.addr().await;
    let f_addr = f.addr().await;
    let x = Node::start(false, vec![s_addr.clone()], vec![]).await;
    let x_id = x.handle.peer_id();
    let x_addr = x.addr().await;
    let start = |scope: SearchScope| {
        let bootstrap = vec![s_addr.clone(), f_addr.clone(), x_addr.clone()];
        async move {
            Node::start_config(
                tempfile::tempdir().unwrap(),
                false,
                bootstrap,
                vec![],
                true,
                |c| {
                    c.trusted_peers = vec![s_id];
                    c.search_scope = scope;
                },
            )
            .await
        }
    };
    let sorted = |mut ids: Vec<PeerId>| {
        ids.sort();
        ids
    };

    let fof = start(SearchScope::FriendsOfFriends).await;
    assert_eq!(asked(&fof.handle, 2).await, sorted(vec![s_id, f_id]));
    let status = wait_for(|| {
        let status = fof.handle.status();
        (status.friends_of_friends == 1 && status.search_peers == 2).then_some(status)
    })
    .await;
    assert_eq!(status.search_scope, SearchScope::FriendsOfFriends);
    // X is connected too, but never asked.
    wait_for(|| (fof.handle.status().connected_peers >= 3).then_some(())).await;
    assert!(!fof.handle.bucket_peers().await.unwrap().contains(&x_id));

    let trusted = start(SearchScope::Trusted).await;
    wait_for(|| (trusted.handle.status().connected_peers >= 3).then_some(())).await;
    assert_eq!(asked(&trusted.handle, 1).await, vec![s_id]);

    let anyone = start(SearchScope::Anyone).await;
    assert_eq!(
        asked(&anyone.handle, 3).await,
        sorted(vec![s_id, f_id, x_id])
    );
}

/// The nodes `handle`'s searches ask, sorted, once there are `want`.
async fn asked(handle: &NetHandle, want: usize) -> Vec<PeerId> {
    for _ in 0..300 {
        let mut peers = handle.bucket_peers().await.unwrap();
        if peers.len() >= want {
            peers.sort();
            return peers;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("gave up waiting for {want} nodes to ask");
}
