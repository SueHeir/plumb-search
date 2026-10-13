//! The network node's tests.

use super::answering::holds_denied;
use super::batches::{
    batch_chunks, by_crawl_epoch, delivery_bytes, max_delivery_bytes, queue_confirmed_delivery,
    release_delivery_slack_with, replay_agreement, resend_pending, BatchListRetries, BatchRetries,
};
use super::swarm::{is_circuit_through, is_global, redial_bootstrap};
use super::*;
use crate::assign::{epoch_of, is_assigned};
use crate::batch::{accept_trusted_batch, Batch, MAX_BATCH_AGE_EPOCHS, MAX_BATCH_RECORDS};
use crate::popularity::Report;
use crate::storage::allocation_for;

#[test]
fn a_batch_naming_what_the_denylist_takes_off_is_not_relayed() {
    struct Denying(Option<&'static str>);
    impl BucketSource for Denying {
        fn bucket(&self, _bucket: u32) -> Option<Vec<String>> {
            None
        }
        fn denying(&self) -> bool {
            self.0.is_some()
        }
        fn denies(&self, url: &str) -> bool {
            self.0 == Some(url)
        }
    }
    let lines: Vec<String> = [
        r#"{"domain":"example.com","url":"https://www.example.com/","key_pages":[{"label":"Docs","url":"https://example.com/docs"}]}"#,
        r#"{"icon_of":"icons.example","png":"AAAA"}"#,
        r#"{"news_of":"news.example","items":[{"title":"A","url":"https://news.example/a","at":1}]}"#,
        "not a record",
    ]
    .map(str::to_string)
    .to_vec();
    assert!(!holds_denied(&Denying(None), &lines), "no list");
    assert!(!holds_denied(&Denying(Some("https://other.org/")), &lines));
    for denied in [
        "https://example.com/",
        "https://www.example.com/",
        "https://example.com/docs",
        "https://icons.example/",
        "https://news.example/a",
    ] {
        assert!(holds_denied(&Denying(Some(denied)), &lines), "{denied}");
    }
}

#[test]
fn empty_startup_lists_retry_with_bounded_peers_attempts_and_backoff() {
    let mut retries = BatchListRetries::default();
    let peer = PeerId::random();
    let mut now = tokio::time::Instant::now();
    for _ in 0..16 {
        assert!(retries.defer(peer, 10, now));
        let at = retries.next().unwrap();
        assert!(at > now);
        assert!(at - now <= Duration::from_secs(2));
        assert!(retries.due(now).is_empty());
        assert_eq!(retries.due(at), vec![(peer, 10)]);
        now = at;
    }
    assert!(!retries.defer(peer, 10, now));
    assert!(retries.next().is_none());
    for _ in 0..MAX_BATCH_LISTS {
        assert!(retries.defer(PeerId::random(), 20, now));
    }
    assert!(!retries.defer(PeerId::random(), 20, now));
    assert_eq!(retries.entries.len(), MAX_BATCH_LISTS);
}

#[test]
fn temporary_busy_replies_keep_sources_and_wake_with_bounded_backoff() {
    let mut retry = BatchRetries::default();
    let id = Hash::of(&[b"temporary busy"]);
    let source = PeerId::random();
    let other = PeerId::random();
    let mut now = tokio::time::Instant::now();
    for attempt in 0..16 {
        assert!(retry.defer(id, source, vec![other], now));
        let at = retry.next().unwrap();
        assert!(at > now, "retry could hot-loop");
        assert!(retry.due(now).is_empty());
        let due = retry.due(at);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, id);
        assert!(due[0].1.contains(&source));
        assert!(due[0].1.contains(&other));
        assert_eq!(retry.entries[&id].attempts, attempt + 1);
        now = at;
    }
    assert!(!retry.defer(id, source, Vec::new(), now));
    assert!(retry.entries.is_empty());
    // A later successful response ends the wait without a maintenance or
    // relist tick; the task does this when the admitted write completes.
    assert!(retry.defer(id, source, Vec::new(), now));
    retry.forget(&id);
    assert!(retry.next().is_none());
}

#[test]
fn retry_ids_cannot_exceed_the_wanted_id_bound() {
    let mut retry = BatchRetries::default();
    let source = PeerId::random();
    let now = tokio::time::Instant::now();
    for n in 0..MAX_WANTED {
        assert!(retry.defer(Hash::of(&[&n.to_le_bytes()]), source, Vec::new(), now));
    }
    assert!(!retry.defer(Hash::of(&[b"overflow"]), source, Vec::new(), now));
    assert_eq!(retry.entries.len(), MAX_WANTED);
}

#[tokio::test]
async fn startup_replay_shutdown_cancels_the_receiver_wait() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_unix();
    let key = Keypair::generate_ed25519();
    let crawler = key.public().to_peer_id();
    let batches = dir.path().join("batches");
    let mut legacy = BatchStore::open(&batches).unwrap();
    for n in 0..6 {
        let mut record = SiteRecord::new(format!("shutdown-replay-{n}.example"));
        record.title = Some("Pending legacy delivery".into());
        record.crawled_at = Some(now);
        legacy
            .insert(
                &Batch::sign(&key, &[record], epoch_of(now), MAX_SHARE_PPM, now)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
    }
    drop(legacy);
    let source = Arc::new(
        crate::BucketTable::build::<SiteRecord>(&dir.path().join("buckets"), &[]).unwrap(),
    );
    let budget = StorageBudget::open(dir.path(), u64::MAX).unwrap();
    let mut config = NetConfig::new(dir.path().to_owned());
    config.listen.clear();
    config.upnp = false;
    config.local_discovery = false;
    config.round_every = None;
    config.trusted_peers = vec![crawler];
    config.storage_budget = Some(budget.clone());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let (handle, records) = start(config, source).await.unwrap();
    while records.len() != RECORD_DELIVERIES {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), handle.shutdown())
        .await
        .unwrap();
    assert!(handle
        .replay_stopped
        .load(std::sync::atomic::Ordering::SeqCst));
    drop(records);
    while budget.status().reserved_bytes != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "cancelled replay retained its reservation"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(BatchStore::open(&batches).unwrap().len(), 6);
}

#[tokio::test]
async fn cold_network_startup_accounts_its_directories_before_replay() {
    for limit in [None, Some(u64::MAX), Some(1024 * 1024)] {
        let dir = tempfile::tempdir().unwrap();
        let net_dir = dir.path().join("net");
        std::fs::create_dir(&net_dir).unwrap();
        let source = Arc::new(
            crate::BucketTable::build::<SiteRecord>(&net_dir.join("buckets"), &[]).unwrap(),
        );
        let budget = limit.map(|limit| StorageBudget::open(dir.path(), limit).unwrap());
        let mut config = NetConfig::new(net_dir.clone());
        config.listen.clear();
        config.bootstrap.clear();
        config.upnp = false;
        config.local_discovery = false;
        config.round_every = None;
        config.fill = false;
        config.storage_budget = budget.clone();
        let (handle, records) = start(config.clone(), source.clone()).await.unwrap();
        handle.recount().await.unwrap(); // Wait for the actual async table save.
        assert_eq!(
            std::fs::read_to_string(net_dir.join(SCOPE_FILE)).unwrap(),
            config.search_scope.as_str()
        );
        if let Some(budget) = &budget {
            let _mutation = budget.mutation();
            let used = budget.status().used_bytes;
            let disk = crate::storage::directory_bytes(dir.path()).unwrap();
            eprintln!("cold startup ledger limit={limit:?} used={used} disk={disk}");
            assert_eq!(used, disk);
            assert_eq!(budget.status().reserved_bytes, 0);
        }
        handle.cache.put(123, vec![Vec::new()], now_unix());
        assert_eq!(handle.cache.len(), 1);
        handle.shutdown().await;
        drop(records);
        // Same scope retains its cache; a changed scope clears it and
        // atomically replaces the marker using the shared admission.
        let (handle, records) = start(config.clone(), source.clone()).await.unwrap();
        assert_eq!(handle.cache.len(), 1);
        handle.shutdown().await;
        drop(records);
        config.search_scope = SearchScope::Trusted;
        let (handle, records) = start(config, source).await.unwrap();
        assert_eq!(handle.cache.len(), 0);
        assert_eq!(
            std::fs::read_to_string(net_dir.join(SCOPE_FILE)).unwrap(),
            "trusted"
        );
        handle.shutdown().await;
        drop(records);
        if let Some(budget) = &budget {
            let _mutation = budget.mutation();
            assert_eq!(
                budget.status().used_bytes,
                crate::storage::directory_bytes(dir.path()).unwrap()
            );
            assert_eq!(budget.status().reserved_bytes, 0);
        }
    }
}

#[tokio::test]
async fn a_scope_change_past_the_storage_limit_is_saved() {
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(
        crate::BucketTable::build::<SiteRecord>(&dir.path().join("buckets"), &[]).unwrap(),
    );
    let budget = StorageBudget::open(dir.path(), u64::MAX).unwrap();
    let mut config = NetConfig::new(dir.path().to_owned());
    config.listen.clear();
    config.bootstrap.clear();
    config.upnp = false;
    config.local_discovery = false;
    config.round_every = None;
    config.fill = false;
    config.storage_budget = Some(budget.clone());
    let (handle, records) = start(config.clone(), source.clone()).await.unwrap();
    handle.recount().await.unwrap();
    handle.shutdown().await;
    drop(records);
    let table_path = dir.path().join(POPULARITY_FILE);
    let old_table =
        serde_json::to_vec_pretty(&PopularityTable::new(vec![123], Vec::new())).unwrap();
    {
        // Recount waits for its own save, but startup maintenance can have
        // another blocking save pending after shutdown. Close admission
        // under the mutation barrier before seeding the persisted fixture.
        let _mutation = budget.mutation();
        budget.set_limit(1);
        plumb_core::storage::write_atomic(
            &table_path,
            &table_path.with_extension("tmp"),
            &old_table,
            None,
            None,
        )
        .unwrap();
    }
    budget.recount(dir.path()).unwrap();
    let old = std::fs::read(dir.path().join(SCOPE_FILE)).unwrap();
    let used = budget.status().used_bytes;
    assert!(used > 0);
    budget.set_limit(used);
    config.search_scope = SearchScope::Trusted;
    // The limit is a target, as in 0.2.0: a node at it still saves a
    // changed scope rather than keep the old one.
    let (handle, records) = start(config, source).await.unwrap();
    handle.recount().await.unwrap();
    assert_ne!(std::fs::read(dir.path().join(SCOPE_FILE)).unwrap(), old);
    assert_eq!(
        std::fs::read_to_string(dir.path().join(SCOPE_FILE)).unwrap(),
        "trusted"
    );
    handle.shutdown().await;
    drop(records);
    // Saves are no longer refused, so one startup began can still be
    // finishing.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while table_path.with_extension("tmp").exists() || budget.status().reserved_bytes != 0 {
        assert!(std::time::Instant::now() < deadline, "a save never ended");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let _mutation = budget.mutation();
    assert_eq!(budget.status().rejected_writes, 0);
    assert_eq!(
        budget.status().used_bytes,
        crate::storage::directory_bytes(dir.path()).unwrap()
    );
}

#[tokio::test]
async fn a_count_failure_still_enqueues_the_final_owned_delivery_and_acknowledgment() {
    let dir = tempfile::tempdir().unwrap();
    let key = Keypair::generate_ed25519();
    let now = now_unix();
    let mut record = SiteRecord::new("confirmed-count-fault.example");
    record.title = Some("Final confirmed output".into());
    record.crawled_at = Some(now);
    let batch = Batch::sign(&key, &[record.clone()], epoch_of(now), MAX_SHARE_PPM, now)
        .unwrap()
        .unwrap();
    let mut store = BatchStore::open(dir.path()).unwrap();
    store.insert(&batch).unwrap();
    let signed_path = dir.path().join(format!("{}.json", batch.id()));
    let signed = std::fs::read(&signed_path).unwrap();
    let marker = dir.path().join(format!("{}.delivered-trusted", batch.id()));
    let budget = StorageBudget::open(dir.path(), u64::MAX).unwrap();
    let reservation = budget.reserve(max_delivery_bytes(1), false).unwrap();
    let allowance = reservation.bytes();
    let (tx, mut rx) = mpsc::channel(1);
    queue_confirmed_delivery(
        tx.reserve_owned().await.unwrap(),
        RecordDelivery {
            records: vec![record.clone()],
            reservation: Some(reservation),
            acknowledgment: store.delivery_ack(&batch.id(), true),
        },
        |_| anyhow::bail!("injected count failure after confirmation"),
    );
    let queued = rx.recv().await.unwrap();
    assert_eq!(queued.records, vec![record]);
    assert_eq!(queued.reservation.as_ref().unwrap().bytes(), allowance);
    assert_eq!(budget.status().reserved_bytes, allowance);
    assert!(!marker.exists());
    assert_eq!(std::fs::read(&signed_path).unwrap(), signed);
    drop(queued.reservation);
    queued.acknowledgment.acknowledge().unwrap();
    assert!(marker.is_file());
    assert_eq!(std::fs::read(&signed_path).unwrap(), signed);
    assert_eq!(budget.status().reserved_bytes, 0);
}

#[test]
fn confirmed_delivery_sizing_releases_only_its_owned_surplus() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("protected-raw"), [1; 32 * 1024]).unwrap();
    let budget = StorageBudget::open(dir.path(), u64::MAX).unwrap();
    let used = budget.status().used_bytes;
    let other = budget.reserve(64 * 1024, false).unwrap();
    let mut record = SiteRecord::new("counted.example");
    record.description = Some("quoted \" text\n".repeat(100));
    let records = vec![record];
    let mut owned = Some(budget.reserve(max_delivery_bytes(1), false).unwrap());
    let encoded = serde_json::to_vec(&records[0]).unwrap();
    // One line break plus a possible recovery line break on append.
    let expected = allocation_for(encoded.len() as u64 + 2);
    release_delivery_slack_with(&mut owned, || delivery_bytes(&records)).unwrap();
    assert_eq!(owned.as_ref().unwrap().bytes(), expected);
    assert_eq!(budget.status().reserved_bytes, expected + other.bytes());
    assert_eq!(budget.status().used_bytes, used);
    drop(owned);
    assert_eq!(budget.status().reserved_bytes, other.bytes());
    drop(other);
    assert_eq!(budget.status().reserved_bytes, 0);
}

#[test]
fn a_delivery_sizing_error_or_oversize_does_not_release_any_owned_admission() {
    let dir = tempfile::tempdir().unwrap();
    let budget = StorageBudget::open(dir.path(), 128 * 1024).unwrap();
    let other = budget.reserve(16 * 1024, false).unwrap();
    let mut owned = Some(budget.reserve(64 * 1024, false).unwrap());
    let before = budget.status();
    assert!(
        release_delivery_slack_with(&mut owned, || anyhow::bail!("injected count error")).is_err()
    );
    assert_eq!(budget.status(), before);
    assert!(release_delivery_slack_with(&mut owned, || Ok(64 * 1024 + 1)).is_err());
    assert_eq!(budget.status(), before);
    assert_eq!(owned.as_ref().unwrap().bytes(), 64 * 1024);
    let mut uncapped = None;
    release_delivery_slack_with(&mut uncapped, || panic!("uncapped delivery must not count"))
        .unwrap();
    drop(owned);
    assert_eq!(budget.status().reserved_bytes, other.bytes());
}

#[test]
fn replay_past_the_storage_limit_still_delivers() {
    let dir = tempfile::tempdir().unwrap();
    let key = Keypair::generate_ed25519();
    let crawler = key.public().to_peer_id();
    let me = PeerId::random();
    let now = now_unix();
    let batches = dir.path().join("batches");
    let mut record = SiteRecord::new("insufficient-replay.example");
    record.description = Some("x".repeat(8 * 1024));
    record.crawled_at = Some(now);
    let batch = Batch::sign(&key, &[record], epoch_of(now), MAX_SHARE_PPM, now)
        .unwrap()
        .unwrap();
    BatchStore::open(&batches).unwrap().insert(&batch).unwrap();
    let path = batches.join(format!("{}.json", batch.id()));
    let signed = std::fs::read(&path).unwrap();
    let budget = StorageBudget::open(dir.path(), u64::MAX).unwrap();
    let store = Mutex::new(
        BatchStore::open_retained(
            &batches,
            true,
            StoreLimits {
                own: Vec::new(),
                bytes: None,
                now,
                epochs: crate::store::RETAIN_EPOCHS,
                storage: Some(budget.clone()),
            },
        )
        .unwrap(),
    );
    let mut agreement = Agreement::new(me, [crawler]);
    let confirmed = agreement.observe(crawler, accept_trusted_batch(&batch, &crawler, now), now);
    let bytes = delivery_bytes(&confirmed).unwrap();
    budget.set_limit(budget.status().used_bytes + bytes - 1);
    let (tx, mut rx) = mpsc::channel(RECORD_DELIVERIES);
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancelled = stopped.clone();
    let id = batch.id();
    let producer = std::thread::spawn(move || {
        resend_pending(&store, &[id], &tx, me, &[crawler], &cancelled);
    });
    // The limit is a target, as in 0.2.0: the delivery is not held
    // back for it; crawls pause instead.
    let delivery = rx.blocking_recv().unwrap();
    assert_eq!(delivery.records, confirmed);
    assert!(budget.status().backpressure);
    assert_eq!(budget.status().rejected_writes, 0);
    producer.join().unwrap();
    drop((delivery, stopped));
    assert_eq!(budget.status().reserved_bytes, 0);
    assert_eq!(std::fs::read(path).unwrap(), signed);
    assert_eq!(
        budget.status().used_bytes,
        crate::storage::directory_bytes(dir.path()).unwrap()
    );
}

#[test]
fn a_start_replays_only_up_to_the_last_batch_not_delivered() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_unix();
    let key = Keypair::generate_ed25519();
    let crawler = key.public().to_peer_id();
    let me = PeerId::random();
    let mut store = BatchStore::open(&dir.path().join("batches")).unwrap();
    let mut ids = Vec::new();
    for n in 0..3 {
        let mut record = SiteRecord::new(format!("site-{n}.example"));
        record.title = Some(format!("Site {n}"));
        record.crawled_at = Some(now - 3 + n);
        let batch = Batch::sign(&key, &[record], epoch_of(now), MAX_SHARE_PPM, now - 3 + n)
            .unwrap()
            .unwrap();
        store.insert(&batch).unwrap();
        ids.push(batch.id());
    }
    let replay = |store: &BatchStore| replay_agreement(store, me, &[crawler]).1;
    assert_eq!(replay(&store), ids, "nothing delivered yet");
    for id in &ids[1..] {
        store.delivery_ack(id, true).acknowledge().unwrap();
    }
    assert_eq!(replay(&store), ids[..1]);
    store.delivery_ack(&ids[0], true).acknowledge().unwrap();
    assert!(replay(&store).is_empty(), "all delivered: no second replay");
}

#[test]
fn legacy_no_ack_replay_is_bounded_and_preserves_signed_batch_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_unix();
    let key = Keypair::generate_ed25519();
    let crawler = key.public().to_peer_id();
    let me = PeerId::random();
    let batches = dir.path().join("batches");
    let mut legacy = BatchStore::open(&batches).unwrap();
    let mut signed = Vec::new();
    for n in 0..12 {
        let mut record = SiteRecord::new(format!("legacy-{n}.example"));
        record.title = Some(format!("Legacy batch {n}"));
        record.body_text = Some("An old-format batch without a delivery marker".into());
        record.crawled_at = Some(now - 12 + n);
        let batch = Batch::sign(&key, &[record], epoch_of(now), MAX_SHARE_PPM, now - 12 + n)
            .unwrap()
            .unwrap();
        legacy.insert(&batch).unwrap();
        signed.push((
            batch.id(),
            std::fs::read(batches.join(format!("{}.json", batch.id()))).unwrap(),
        ));
    }
    drop(legacy);
    let budget = StorageBudget::open(dir.path(), u64::MAX).unwrap();
    let store = BatchStore::open_retained(
        &batches,
        true,
        StoreLimits {
            own: Vec::new(),
            bytes: None,
            now,
            epochs: crate::store::RETAIN_EPOCHS,
            storage: Some(budget.clone()),
        },
    )
    .unwrap();
    let (mut agreement, replay) = replay_agreement(&store, me, &[crawler]);
    assert!(agreement.take_verdicts().is_empty());
    let store = Arc::new(Mutex::new(store));
    let (tx, mut rx) = mpsc::channel(RECORD_DELIVERIES);
    let producer_store = store.clone();
    let producer = std::thread::spawn(move || {
        resend_pending(
            &producer_store,
            &replay,
            &tx,
            me,
            &[crawler],
            &std::sync::atomic::AtomicBool::new(false),
        )
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while rx.len() != RECORD_DELIVERIES {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!producer.is_finished());
    // Four channel slots plus the producer's single bounded batch.
    assert!(
        budget.status().reserved_bytes <= max_delivery_bytes(1) * (RECORD_DELIVERIES as u64 + 1)
    );
    let mut received = HashSet::new();
    while let Some(delivery) = rx.blocking_recv() {
        assert_eq!(delivery.records.len(), 1);
        received.insert(delivery.records[0].domain.clone());
        drop(delivery.reservation);
        delivery.acknowledgment.acknowledge().unwrap();
    }
    producer.join().unwrap();
    assert_eq!(received.len(), 12);
    assert_eq!(budget.status().reserved_bytes, 0);
    assert_eq!(
        budget.status().used_bytes,
        crate::storage::directory_bytes(dir.path()).unwrap()
    );
    for (id, bytes) in signed {
        assert_eq!(
            std::fs::read(batches.join(format!("{id}.json"))).unwrap(),
            bytes
        );
        assert!(store.lock().unwrap().get(&id).unwrap().is_some());
    }
}

#[test]
fn pending_trusted_delivery_is_replayed_until_durably_acknowledged() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_unix();
    let key = Keypair::generate_ed25519();
    let crawler = key.public().to_peer_id();
    let me = PeerId::random();
    let mut record = SiteRecord::new("shoes.example");
    record.title = Some("Shoes".into());
    record.body_text = Some("Handmade leather shoes".into());
    record.crawled_at = Some(now);
    let batch = Batch::sign(&key, &[record], epoch_of(now), MAX_SHARE_PPM, now)
        .unwrap()
        .unwrap();
    let mut store = BatchStore::open(&dir.path().join("batches")).unwrap();
    store.insert(&batch).unwrap();
    // Delivered while the crawler was not trusted: that marker does not
    // stand for the delivery trust asks for.
    store
        .delivery_ack(&batch.id(), false)
        .acknowledge()
        .unwrap();
    let (_, replay) = replay_agreement(&store, me, &[crawler]);
    let store = Mutex::new(store);
    let (tx, mut rx) = mpsc::channel(RECORD_DELIVERIES);
    resend_pending(
        &store,
        &replay,
        &tx,
        me,
        &[crawler],
        &std::sync::atomic::AtomicBool::new(false),
    );
    let sent = rx.try_recv().unwrap();
    assert_eq!(
        sent.records[0].body_text.as_deref(),
        Some("Handmade leather shoes")
    );
    drop(sent); // Sending is not an acknowledgment.
    resend_pending(
        &store,
        &replay,
        &tx,
        me,
        &[crawler],
        &std::sync::atomic::AtomicBool::new(false),
    );
    rx.try_recv().unwrap().acknowledgment.acknowledge().unwrap();
    resend_pending(
        &store,
        &replay,
        &tx,
        me,
        &[crawler],
        &std::sync::atomic::AtomicBool::new(false),
    );
    assert!(rx.try_recv().is_err());
    // A previous quorum-mode acknowledgment cannot suppress a trust upgrade.
    let store = store.lock().unwrap();
    assert!(store.delivery_ack(&batch.id(), true).acknowledged());
    assert!(store.delivery_ack(&batch.id(), false).acknowledged());
}

#[test]
fn a_search_keeps_the_text_of_crawls_only_from_trusted_nodes() {
    use crate::proto::BucketRecord;
    let now = 1_790_000_000;
    let epoch = epoch_of(now);
    let key = Keypair::generate_ed25519();
    let crawler = key.public().to_peer_id();
    let site = |assigned: bool| {
        (0..)
            .map(|i| format!("shop{i}.com"))
            .find(|d| is_assigned(epoch, &crawler, d, MAX_SHARE_PPM) == assigned)
            .unwrap()
    };
    let crawled = |domain: &str| {
        let mut record = SiteRecord::new(domain);
        record.title = Some("Shop".into());
        record.description = Some("Handmade shoes".into());
        record.body_text = Some("Handmade leather shoes, made to order".into());
        record.crawled_at = Some(now);
        record
    };
    let (assigned, unassigned) = (site(true), site(false));
    let batch = Batch::sign(
        &key,
        &[crawled(&assigned), crawled(&unassigned)],
        epoch,
        MAX_SHARE_PPM,
        now,
    )
    .unwrap()
    .unwrap();
    // The node answering holds no text for either site.
    let answer = || {
        (0..2)
            .map(|i| {
                let mut bare: SiteRecord = serde_json::from_str(&batch.records[i]).unwrap();
                bare.body_text = None;
                BucketRecord {
                    record: serde_json::to_string(&bare).unwrap(),
                    proof: Some(batch.proof(i)),
                    also: Vec::new(),
                }
            })
            .collect::<Vec<_>>()
    };
    let search = |trusted: &[PeerId]| {
        let mut found = NetSearch {
            found: crate::search::check_answer(answer(), now).unwrap(),
            ..NetSearch::default()
        };
        keep_trusted_crawls(&mut found, trusted, now);
        let by = |d: &str| found.found.iter().find(|s| s.record.domain == d).cloned();
        (by(&assigned).unwrap(), by(&unassigned).unwrap())
    };

    // Untrusted: an assigned crawl's homepage facts count, never its
    // text, and an unassigned crawl counts not at all.
    let (a, u) = search(&[]);
    // One untrusted crawler alone is not kept: anyone can make a key
    // assigned the site.
    assert!(a.keeps().is_none());
    let shared = a.shared.clone().expect("an assigned crawl counts");
    assert_eq!(shared.description.as_deref(), Some("Handmade shoes"));
    assert_eq!(shared.body_text, None);
    assert!(u.shared.is_none() && !u.verified);
    // Confirmed by crawlers this node counts, it is.
    let confirmed = crate::search::FoundSite {
        confirmed: true,
        ..a
    };
    assert_eq!(confirmed.keeps(), Some(&shared));

    // Trusted: both count whole, text included, and are ranked with it.
    let (a, u) = search(&[crawler]);
    for site in [a, u] {
        assert!(site.verified);
        assert!(site.keeps().is_some());
        let text = Some("Handmade leather shoes, made to order");
        assert_eq!(site.shared.unwrap().body_text.as_deref(), text);
        assert_eq!(site.record.body_text.as_deref(), text);
    }
}

#[test]
fn the_largest_honest_answers_fit_under_the_response_caps() {
    // A batch of the most records, as many bytes as a batch may hold.
    let key = Keypair::generate_ed25519();
    let now = now_unix();
    let mut batch = Batch::sign(
        &key,
        &[SiteRecord::new("a.com")],
        epoch_of(now),
        MAX_SHARE_PPM,
        now,
    )
    .unwrap()
    .unwrap();
    let line = "x".repeat(crate::batch::MAX_BATCH_BYTES / MAX_BATCH_RECORDS);
    batch.records = vec![line; MAX_BATCH_RECORDS];
    assert!(size(&BatchResponse::Batch(Some(batch.clone()))) < MAX_BATCH_RESPONSE);
    let headers = vec![batch.header; MAX_LISTED_BATCHES];
    assert!(size(&BatchResponse::Headers(headers)) < MAX_BATCH_RESPONSE);
    // The most reports listed, of the longest pick there is.
    let domain = format!("{}.com", "a".repeat(59));
    let query = crate::popularity::pick_query(&format!("{} {}", "b".repeat(30), "c".repeat(30)))
        .expect("a query that is reported");
    let report = Report::new(crate::popularity::report_epoch(now), &query, &domain).unwrap();
    let reports = vec![report; MAX_LISTED_REPORTS];
    assert!(size(&ReportResponse::Reports(reports)) < MAX_REPORT_RESPONSE);
}

/// The size of `value` in CBOR, as the request-response codec sends it.
fn size<T: Serialize>(value: &T) -> u64 {
    cbor4ii::serde::to_vec(Vec::new(), value).unwrap().len() as u64
}

#[test]
fn a_node_goes_back_to_its_bootstrap_nodes_after_losing_them() {
    // Alone: every minute.
    assert!(redial_bootstrap(0, false, 1));
    // Connected to a bootstrap node: never.
    assert!(!(0..=BOOTSTRAP_REDIAL_MINUTES).any(|t| redial_bootstrap(3, true, t)));
    // Other nodes only: every few minutes.
    let tries = (1..=3 * BOOTSTRAP_REDIAL_MINUTES)
        .filter(|&t| redial_bootstrap(3, false, t))
        .count();
    assert_eq!(tries, 3);
}

#[test]
fn a_record_with_an_icon_takes_two_lines_of_a_batch() {
    let record = |i: usize, icon: bool| {
        let mut r = SiteRecord::new(format!("s{i}.com").as_str());
        r.icon = icon.then(|| "x".to_string());
        r
    };
    let plain: Vec<SiteRecord> = (0..MAX_BATCH_RECORDS + 1)
        .map(|i| record(i, false))
        .collect();
    let sizes: Vec<usize> = batch_chunks(&plain).iter().map(|c| c.len()).collect();
    assert_eq!(sizes, [MAX_BATCH_RECORDS, 1]);
    let iconed: Vec<SiteRecord> = (0..MAX_BATCH_RECORDS).map(|i| record(i, true)).collect();
    let sizes: Vec<usize> = batch_chunks(&iconed).iter().map(|c| c.len()).collect();
    assert_eq!(sizes, [MAX_BATCH_RECORDS / 2, MAX_BATCH_RECORDS / 2]);
    assert!(batch_chunks(&[]).is_empty());
}

#[test]
fn crawls_are_published_in_the_epoch_they_were_made() {
    let now = 1_790_000_000;
    let day = crate::assign::EPOCH_SECS;
    let at = |domain: &str, crawled_at: Option<u64>| {
        let mut r = SiteRecord::new(domain);
        r.crawled_at = crawled_at;
        r
    };
    let records = vec![
        at("today.com", Some(now)),
        at("linked.com", None),
        at("yesterday.com", Some(now - day)),
        at("lastweek.com", Some(now - MAX_BATCH_AGE_EPOCHS * day)),
        at("tomorrow.com", Some(now + 2 * day)),
    ];
    let grouped = by_crawl_epoch(records, now, |_, _| true);
    let domains: Vec<(u64, Vec<&str>)> = grouped
        .iter()
        .map(|(epoch, rs)| (*epoch, rs.iter().map(|r| r.domain.as_str()).collect()))
        .collect();
    let today = epoch_of(now);
    assert_eq!(
        domains,
        vec![
            (today - 1, vec!["yesterday.com"]),
            (today, vec!["today.com", "linked.com"]),
        ]
    );
}

#[test]
fn a_crawl_past_midnight_goes_with_the_epoch_it_was_assigned_in() {
    let day = crate::assign::EPOCH_SECS;
    let midnight = 1_790_035_200;
    assert_eq!(midnight % day, 0);
    let today = epoch_of(midnight);
    let at = |domain: &str, crawled_at: u64| {
        let mut r = SiteRecord::new(domain);
        r.crawled_at = Some(crawled_at);
        r
    };
    let records = vec![
        at("before.com", midnight - 60),
        at("after.com", midnight + 60),
        at("both.com", midnight + 60),
        at("today.com", midnight + 60),
    ];
    // Each site is this node's in the epochs listed.
    let assigned = |epoch: u64, domain: &str| match domain {
        "before.com" | "after.com" => epoch == today - 1,
        "both.com" => true,
        _ => epoch == today,
    };
    let grouped = by_crawl_epoch(records, midnight + 120, assigned);
    let domains: Vec<(u64, Vec<&str>)> = grouped
        .iter()
        .map(|(epoch, rs)| (*epoch, rs.iter().map(|r| r.domain.as_str()).collect()))
        .collect();
    assert_eq!(
        domains,
        vec![
            (today - 1, vec!["before.com", "after.com"]),
            (today, vec!["both.com", "today.com"]),
        ]
    );
    // A batch of the day before takes the crawl after midnight.
    assert!(crate::batch::in_crawl_window(midnight + 60, today - 1));
}

#[test]
fn home_network_addresses_are_not_global() {
    for local in [
        "/ip4/127.0.0.1/tcp/4001",
        "/ip4/192.168.4.21/tcp/4101",
        "/ip4/10.0.0.2/udp/4001/quic-v1",
        "/ip4/100.64.1.1/tcp/1",
        "/ip6/fd2e:4ba7:777b:1::5/udp/4101/quic-v1",
        "/ip6/fe80::1/tcp/1",
        "/ip6/::1/tcp/1",
    ] {
        assert!(!is_global(&local.parse().unwrap()), "{local}");
    }
    for public in [
        "/ip4/198.211.114.63/tcp/4001",
        "/ip6/2604:a880:400:d1::5:1815:7001/tcp/4001",
        "/dns4/plumbsearch.org/tcp/4001",
        "/ip4/198.211.114.63/tcp/4001/p2p/12D3KooWJ2UWUBsxmPfXTfHa8cBBmzifa6kj5pFZKfJXYNQyJ69a/p2p-circuit",
    ] {
        assert!(is_global(&public.parse().unwrap()), "{public}");
    }
}

#[test]
fn circuits_are_matched_by_their_relay() {
    let relay = PeerId::random();
    let other = PeerId::random();
    let target = PeerId::random();
    let through = |r: PeerId| -> Multiaddr {
        format!("/ip4/198.211.114.63/tcp/4001/p2p/{r}/p2p-circuit/p2p/{target}")
            .parse()
            .unwrap()
    };
    assert!(is_circuit_through(&through(relay), &relay));
    assert!(!is_circuit_through(&through(other), &relay));
    let direct: Multiaddr = format!("/ip4/1.2.3.4/tcp/1/p2p/{relay}").parse().unwrap();
    assert!(!is_circuit_through(&direct, &relay));
}
