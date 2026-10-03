//! Several nodes on this machine: crawl batches spread from one to all and
//! count once two crawlers agree,
//! a network search is answered with proofs, a node behind a relay is
//! reachable through it, and a late node catches up.

use std::sync::Arc;
use std::time::Duration;

use plumb_core::{now_unix, SiteRecord};
use plumb_net::assign::{epoch_of, is_assigned, MAX_SHARE_PPM};
use plumb_net::{BucketSource, BucketTable, Multiaddr, NetConfig, NetHandle, PeerId};
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
        let mut config = NetConfig::new(dir.path().to_path_buf());
        config.listen = if listen {
            vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()]
        } else {
            vec![]
        };
        config.upnp = false;
        config.local_discovery = false;
        // These tests check that one crawler is not enough.
        config.trusting = false;
        config.relay_server = relay;
        config.bootstrap = bootstrap;
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
