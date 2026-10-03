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
    assert_eq!(result.rejected, 0);

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
