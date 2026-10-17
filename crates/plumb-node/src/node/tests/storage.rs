//! The storage limit: what is charged, and deliveries, folds and the
//! inbox past it.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enabling_and_changing_quota_preserves_inflight_reservations_and_search() {
    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    config.settings.storage_limit_mb = 0;
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "initial index", ready_and_idle).await;
    let budget = node.inner.storage.clone().unwrap();
    assert_eq!(budget.status().limit_bytes, u64::MAX);
    // A direct owner edit is reconciled at the blocking checkpoint, never
    // by an ingest/status invalidation. This distinguishes a cached ledger
    // from a hidden full tree walk on every call.
    let probe = dir.path().join("owner-recount-probe");
    let counted = budget.status().used_bytes;
    std::fs::write(&probe, vec![0; 64 * 1024]).unwrap();
    for _ in 0..100 {
        node.inner.recount_disk();
        assert_eq!(node.inner.disk_used(), counted);
        assert_eq!(node.inner.disk_used_shown(), counted);
    }
    budget.recount(dir.path()).unwrap();
    assert!(node.inner.disk_used() > counted);
    plumb_core::storage::remove_file(&probe, Some(&budget)).unwrap();
    let pending = budget.reserve(1_500_000, false).unwrap();
    let mut settings = node.inner.settings();
    settings.storage_limit_mb = 1;
    node.inner.change_settings(settings.clone()).unwrap();
    assert_eq!(budget.status().limit_bytes, MB);
    assert_eq!(budget.status().reserved_bytes, 1_500_000);
    assert!(budget.status().backpressure);
    // Past the limit writes go on, as in 0.2.0; crawls pause.
    drop(budget.reserve(8192, false).unwrap());
    assert!(node.inner.pause().is_some());
    assert!(!search(node.addr(), "chase").await.is_empty());
    settings.storage_limit_mb = 5;
    node.inner.change_settings(settings.clone()).unwrap();
    assert!(!budget.status().backpressure);
    drop(budget.reserve(8192, false).unwrap());
    settings.storage_limit_mb = 0;
    node.inner.change_settings(settings).unwrap();
    assert_eq!(budget.status().limit_bytes, u64::MAX);
    drop(budget.reserve(8_000_000, false).unwrap());
    drop(pending);
    assert_eq!(budget.status().reserved_bytes, 0);
    assert!(Arc::ptr_eq(&budget, node.inner.storage.as_ref().unwrap()));
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_inbox_past_the_storage_limit_still_takes_records() {
    let dir = seeded_dir();
    let node = start(test_config(dir.path())).await.unwrap();
    wait_for(node.addr(), "initial index", ready_and_idle).await;
    let raw = std::fs::read(&node.inner.paths.records).unwrap();
    std::fs::create_dir_all(&node.inner.paths.net).unwrap();
    let prefix = b"{crash cut inbox";
    std::fs::write(&node.inner.paths.inbox, prefix).unwrap();
    let credits = node.inner.paths.net.join("credits-proof.json");
    std::fs::write(&credits, b"preserve saved credits").unwrap();
    let budget = node.inner.storage.as_ref().unwrap();
    budget.recount(dir.path()).unwrap();
    budget.set_limit(budget.status().used_bytes);
    // The limit is a target, as in 0.2.0: records peers sent are kept.
    let record = SiteRecord::new("waiting.example");
    crate::node::network::append_inbox(&node.inner, &listed(&[record])).unwrap();
    let inbox = std::fs::read(&node.inner.paths.inbox).unwrap();
    assert!(inbox.starts_with(prefix));
    assert!(String::from_utf8_lossy(&inbox).contains("waiting.example"));
    assert_eq!(std::fs::read(&node.inner.paths.records).unwrap(), raw);
    assert_eq!(std::fs::read(credits).unwrap(), b"preserve saved credits");
    assert_eq!(budget.status().reserved_bytes, 0);
    assert_eq!(budget.status().rejected_writes, 0);
    assert!(budget.status().backpressure);
    assert!(!search(node.addr(), "chase").await.is_empty());
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaced_page_index_stays_charged_and_searchable_until_held_reader_closes() {
    let dir = seeded_dir();
    let set = crate::pages::SetInfo::find("wikipedia-en").unwrap();
    let file = set.file(dir.path());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let article = |title: &str| plumb_core::Article {
        title: title.into(),
        views: 10,
        ..Default::default()
    };
    write_articles_file(&file, &[article("First Quota Page")]).unwrap();
    crate::node::newer::set_time(&file, 1_700_000_000).unwrap();
    let mut config = test_config(dir.path());
    config.settings.page_sets = crate::pages::PageSets::parse("wikipedia-en=all").unwrap();
    let node = start(config).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while node.inner.page_key().is_none() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait_for(node.addr(), "initial index", ready_and_idle).await;
    let (old_key, old_reader) = node.inner.pages.read().unwrap().as_ref().unwrap().clone();
    let old_dir = crate::pages::index_dir(dir.path(), &old_key);
    let old_bytes = plumb_core::storage::directory_bytes(&old_dir).unwrap();
    write_articles_file(&file, &[article("Second Quota Page")]).unwrap();
    crate::node::newer::set_time(&file, 1_700_000_001).unwrap();
    while node.inner.page_key().as_deref() == Some(&old_key) {
        assert!(Instant::now() < deadline, "replacement never became ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(old_dir.exists(), "held generation was deleted");
    assert_eq!(old_reader.num_pages(), 1);
    assert_eq!(old_reader.search("First", 10).unwrap().len(), 1);
    let before = node.inner.storage.as_ref().unwrap().status().used_bytes;
    drop(old_reader);
    while old_dir.exists() {
        assert!(
            Instant::now() < deadline,
            "retired index was never collected"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let after = node.inner.storage.as_ref().unwrap().status().used_bytes;
    assert!(
        before.saturating_sub(after) >= old_bytes,
        "old allocation was not released"
    );
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_replacements_and_implicit_folds_run_past_the_storage_limit() {
    let dir = seeded_dir();
    let node = start(test_config(dir.path())).await.unwrap();
    wait_for(node.addr(), "initial index", ready_and_idle).await;
    let budget = node.inner.storage.clone().unwrap();
    let mut large = SiteRecord::new("quota-fold.example");
    large.description = Some("large protected journal ".repeat(50_000));
    crate::records::RecordStore::open(&node.inner.paths.records)
        .save(&[crate::records::Change::Merge {
            site: large.clone().stored_item_as_is(),
        }])
        .unwrap();
    plumb_core::storage::create_directory(&node.inner.paths.net, Some(&budget)).unwrap();
    crate::node::network::append_inbox(
        &node.inner,
        &listed(&[SiteRecord::new("protected-inbox.example")]),
    )
    .unwrap();
    let inbox = std::fs::read(&node.inner.paths.inbox).unwrap();
    budget.recount(dir.path()).unwrap();
    let used = budget.status().used_bytes;
    budget.set_limit(used + 8192);
    // The limit is a target, as in 0.2.0: the rewrites and folds a node
    // makes, those that trim it back under the limit among them, run past
    // it rather than leave the node stuck over it.
    let seed = [large.clone().stored_item_as_is()].into_iter().collect();
    crate::node::worker::save_seed_sites(&node.inner, &seed).unwrap();
    crate::node::round::RoundSites::load_with_budget(
        &node.inner.paths.records,
        crate::about::Topics::default(),
        crate::node::trim::Keep::default(),
        Some(budget.clone()),
    )
    .unwrap();
    crate::outline::outline_with_budget(&node.inner.paths.records, Some(budget.clone())).unwrap();
    let vectors = std::sync::RwLock::new(plumb_embed::Vectors::new([0; 32], 4));
    crate::meaning::sites_to_embed_from_file(
        &vectors,
        &node.inner.paths.records,
        1,
        32,
        Some(budget.clone()),
    )
    .unwrap();
    crate::meaning::wanted_texts(
        &vectors,
        &node.inner.paths.records,
        32,
        Some(budget.clone()),
    )
    .unwrap();
    assert_eq!(std::fs::read(&node.inner.paths.inbox).unwrap(), inbox);
    assert_eq!(budget.status().reserved_bytes, 0);
    assert_eq!(budget.status().rejected_writes, 0);
    assert!(!search(node.addr(), "chase").await.is_empty());
    node.shutdown().await.unwrap();
}

#[test]
fn absorbing_the_inbox_waits_for_whoever_holds_the_records() {
    let dir = tempfile::tempdir().unwrap();
    write_jsonl(
        &dir.path().join("records.jsonl"),
        &[SiteRecord::new("held.example")],
    )
    .unwrap();
    let config = test_config(dir.path());
    let rank = RankConfig::default();
    let opened = open_data_dir(&config, rank).unwrap();
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let inner = Arc::new(Inner::new(config, rank, opened, stopped, None));
    std::fs::create_dir_all(&inner.paths.net).unwrap();
    crate::node::network::append_inbox(&inner, &listed(&[SiteRecord::new("sent.example")]))
        .unwrap();
    // The embedding job holds them while it folds the journal away.
    let held = inner.hold_records();
    let absorbing = {
        let inner = inner.clone();
        std::thread::spawn(move || crate::node::network::absorb_inbox(&inner).unwrap())
    };
    std::thread::sleep(Duration::from_millis(300));
    let journal = crate::records::journal_path(&inner.paths.records);
    assert!(!journal.exists(), "nothing is appended meanwhile");
    drop(held);
    assert_eq!(absorbing.join().unwrap(), 1);
    let sites = crate::records::load_sites(&inner.paths.records).unwrap();
    assert!(sites.contains("sent.example"));
}

#[test]
fn two_inbox_absorptions_cross_the_real_fold_threshold_under_receiving_contention() {
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    const RECORDS: usize = 16_384;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.jsonl");
    let record = |i: usize, at: u64| {
        let mut record = SiteRecord::new(format!("site-{i}.example"));
        record.description = Some("x".repeat(4 * 1024));
        record.title = Some(format!("Received site {i}"));
        record.crawled_at = Some(at);
        record
    };
    // Written as the node writes it, so that the fold's output is as long.
    let mut raw = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    for i in 0..RECORDS {
        plumb_core::stored::write(&mut raw, &record(i, 1)).unwrap();
        raw.write_all(b"\n").unwrap();
    }
    raw.into_inner().unwrap().sync_all().unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() > crate::records::MIN_COMPACT_BYTES);
    let mut config = test_config(dir.path());
    config.take_new_sites = true;
    let rank = RankConfig::default();
    let opened = open_data_dir(&config, rank).unwrap();
    let budget = plumb_core::storage::StorageBudget::open(dir.path(), u64::MAX).unwrap();
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let inner = Inner::new(config, rank, opened, stopped, Some(budget.clone()));
    plumb_core::storage::create_directory(&inner.paths.net, Some(&budget)).unwrap();
    let old = {
        let _mutation = budget.mutation();
        (
            std::fs::File::open(&path).unwrap(),
            budget.read_lease(&path),
        )
    };
    let mut old = Some(old);
    let old_bytes = plumb_core::storage::file_bytes(&path).unwrap();
    let append = |at| {
        for first in (0..RECORDS).step_by(512) {
            let records: Vec<_> = (first..first + 512).map(|i| record(i, at)).collect();
            crate::node::network::append_inbox(&inner, &listed(&records)).unwrap();
        }
    };
    append(2);
    let limit = budget.status().used_bytes + 192 * 1024 * 1024;
    budget.set_limit(limit);
    let queued = budget.reserve(16 * 1024 * 1024, false).unwrap();
    let sampling_done = Arc::new(AtomicBool::new(false));
    struct StopSampling(Arc<AtomicBool>);
    impl Drop for StopSampling {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let stop_sampling = StopSampling(sampling_done.clone());
    let ledger_peak = Arc::new(AtomicU64::new(0));
    let disk_peak = Arc::new(AtomicU64::new(0));
    let sampler = {
        let budget = budget.clone();
        let done = sampling_done.clone();
        let ledger_peak = ledger_peak.clone();
        let disk_peak = disk_peak.clone();
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let status = budget.status();
                ledger_peak.fetch_max(status.used_bytes + status.reserved_bytes, Ordering::SeqCst);
                if let Ok(linked) = plumb_core::storage::directory_bytes(budget.root()) {
                    disk_peak.fetch_max(linked + status.reader_held_bytes, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    for at in [2, 3] {
        if at == 3 {
            append(at);
        }
        assert_eq!(
            crate::node::network::absorb_inbox(&inner).unwrap(),
            RECORDS as u64
        );
        assert_eq!(budget.status().rejected_writes, 0);
        assert!(!crate::records::journal_path(&path).exists());
        assert!(!inner.paths.inbox.exists());
        assert!(!inner.paths.absorbing.exists());
        let mut count = 0;
        crate::outline::for_each_record(&path, |record| {
            assert_eq!(record.crawled_at, Some(at));
            count += 1;
        })
        .unwrap();
        assert_eq!(count, RECORDS);
        let status = budget.status();
        assert_eq!(status.reserved_bytes, 16 * 1024 * 1024);
        assert_eq!(
            status.used_bytes,
            plumb_core::storage::directory_bytes(dir.path()).unwrap() + status.reader_held_bytes
        );
        assert!(status.used_bytes + status.reserved_bytes <= limit);
        if at == 2 {
            assert_eq!(status.reader_held_bytes, old_bytes);
            assert_eq!(
                old.as_ref().unwrap().0.metadata().unwrap().len(),
                std::fs::metadata(&path).unwrap().len()
            );
            // Let the retired generation go before the next absorption.
            // Keeping it would legitimately consume the next output's room.
            drop(old.take());
        } else {
            assert_eq!(status.reader_held_bytes, 0);
        }
    }
    drop(stop_sampling);
    sampler.join().unwrap();
    // The limit is a target: the peaks are reported, not bounded by it.
    eprintln!(
        "two absorption peaks: ledger={}, linked+reader-held={}, cap={limit}",
        ledger_peak.load(Ordering::SeqCst),
        disk_peak.load(Ordering::SeqCst)
    );
    drop(queued);
    assert_eq!(budget.status().reserved_bytes, 0);
    assert_eq!(budget.status().reader_held_bytes, 0);
    assert_eq!(
        budget.status().used_bytes,
        plumb_core::storage::directory_bytes(dir.path()).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_set_cut_past_the_storage_limit_still_runs() {
    let dir = seeded_dir();
    let set = crate::pages::SetInfo::find("wikipedia-en").unwrap();
    let file = set.file(dir.path());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let mut random = 31u64;
    let articles: Vec<_> = (0..3)
        .map(|i| plumb_core::Article {
            title: format!("Retry Cut Page {i}"),
            views: 3 - i,
            description: Some(
                (0..65_536)
                    .map(|_| {
                        random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                        (b'a' + ((random >> 32) % 26) as u8) as char
                    })
                    .collect(),
            ),
            ..Default::default()
        })
        .collect();
    write_articles_file(&file, &articles).unwrap();
    let notes = crate::pages::SetFileNotes {
        lines: 3,
        complete: true,
        source_modified: 1_700_000_000,
        fetched_at: now_unix(),
        near: 0,
    };
    std::fs::write(
        crate::pages::notes_path(&file),
        serde_json::to_vec(&notes).unwrap(),
    )
    .unwrap();
    let original = std::fs::read(&file).unwrap();
    let mut config = test_config(dir.path());
    config.settings.page_sets = crate::pages::PageSets::parse("wikipedia-en=all").unwrap();
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "initial index", ready_and_idle).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while node.inner.page_key().is_none() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let budget = node.inner.storage.clone().unwrap();
    budget.set_limit(budget.status().used_bytes + 8192);
    // The limit is a target, as in 0.2.0: a cut, which makes room, runs
    // past it.
    let mut kept = std::collections::HashSet::new();
    crate::node::pages::cut_if_longer(&node.inner, set, 2, &[], &mut kept).unwrap();
    let part = PathBuf::from(format!("{}.part", file.display()));
    assert!(!part.exists());
    assert_eq!(budget.status().reserved_bytes, 0);
    assert_eq!(budget.status().rejected_writes, 0);
    assert!(!crate::node::newer::prev_path(&file).exists());
    assert_eq!(set.file_notes(dir.path()).unwrap().lines, 2);
    assert!(std::fs::read(&file).unwrap().len() < original.len());
    assert!(!search(node.addr(), "chase").await.is_empty());
    node.shutdown().await.unwrap();
}

#[test]
fn a_fold_past_the_limit_installs_its_output_and_keeps_the_old_reader_charged() {
    use sha2::Digest;
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.jsonl");
    let record = |i: usize, at: u64| {
        let mut r = SiteRecord::new(format!("site-{i}.example"));
        r.url = Some(format!("https://site-{i}.example/"));
        r.description = Some("x".repeat(4 * 1024));
        r.crawled_at = Some(at);
        r
    };
    // Written as the node writes it, so that the fold's output is as long.
    let mut raw = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    for i in 0..32_768 {
        plumb_core::stored::write(&mut raw, &record(i, 1)).unwrap();
        raw.write_all(b"\n").unwrap();
    }
    raw.into_inner().unwrap().sync_all().unwrap();
    let journal = crate::records::journal_path(&path);
    write_jsonl(
        &journal,
        &[crate::records::Change::MergeShared {
            site: record(0, 2).stored_item_as_is(),
        }],
    )
    .unwrap();
    let absorbing = dir.path().join("inbox.absorbing");
    std::fs::write(&absorbing, [1; 384 * 1024]).unwrap();
    let logical = std::fs::metadata(&path).unwrap().len();
    let raw_before = sha2::Sha256::digest(std::fs::read(&path).unwrap());
    let budget = plumb_core::storage::StorageBudget::open(dir.path(), u64::MAX).unwrap();
    let other = budget.reserve(64 * 1024, false).unwrap();
    // Less room than the fold's output: the limit is a target, as in 0.2.0,
    // and the fold that keeps a node near it is not refused for it.
    budget.set_limit(budget.status().used_bytes + logical / 2);
    // An actual descriptor plus its registered lease keeps the previous raw
    // generation charged once the rewrite is installed.
    let (reader, lease) = {
        let _mutation = budget.mutation();
        (
            std::fs::File::open(&path).unwrap(),
            budget.read_lease(&path),
        )
    };
    let mut store = crate::records::RecordStore::open(&path).with_budget(Some(budget.clone()));
    assert_eq!(
        store.fold().unwrap(),
        crate::outline::Folded::Records(32_768)
    );
    assert_eq!(std::fs::read(&absorbing).unwrap(), [1; 384 * 1024]);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), logical);
    assert_ne!(
        sha2::Sha256::digest(std::fs::read(&path).unwrap()),
        raw_before
    );
    assert!(!journal.exists());
    assert!(
        std::fs::read_dir(dir.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".records.jsonl.")),
        "the fold left a staging file"
    );
    let status = budget.status();
    assert_eq!(status.reserved_bytes, other.bytes());
    assert_eq!(status.reader_held_bytes, logical);
    assert_eq!(status.rejected_writes, 0);
    assert!(status.backpressure);
    assert_eq!(
        status.used_bytes,
        plumb_core::storage::directory_bytes(dir.path()).unwrap() + logical
    );
    {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::BufReader::new(reader.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let old = plumb_core::stored::read(line.as_bytes()).unwrap();
        assert_eq!(old.crawled_at, Some(1));
        line.clear();
        std::io::BufReader::new(std::fs::File::open(&path).unwrap())
            .read_line(&mut line)
            .unwrap();
        let new = plumb_core::stored::read(line.as_bytes()).unwrap();
        assert_eq!(new.crawled_at, Some(2));
    }
    drop(reader);
    drop(lease);
    drop(other);
    assert_eq!(budget.status().reserved_bytes, 0);
    assert_eq!(budget.status().reader_held_bytes, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_retained_replay_queue_leaves_room_for_raw_progress_and_normal_acknowledgment() {
    use plumb_net::assign::{epoch_of, MAX_SHARE_PPM};
    use plumb_net::batch::Batch;
    use plumb_net::store::BatchStore;
    use sha2::Digest;
    use std::io::Write;

    const RECORDS: usize = 32_768;
    const PER_BATCH: usize = 512;
    const BATCHES: usize = RECORDS / PER_BATCH;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.jsonl");
    let journal = crate::records::journal_path(&path);
    let record = |i: usize, at: u64| {
        let mut record = SiteRecord::new(format!("site-{i}.example"));
        record.url = Some(format!("https://site-{i}.example/"));
        record.title = Some(format!("Retained crawl {i}"));
        record.description = Some("x".repeat(4 * 1024));
        record.crawled_at = Some(at);
        record
    };
    let mut raw = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    let mut pending = std::io::BufWriter::new(std::fs::File::create(&journal).unwrap());
    for i in 0..RECORDS {
        serde_json::to_writer(&mut raw, &record(i, 1)).unwrap();
        raw.write_all(b"\n").unwrap();
        serde_json::to_writer(
            &mut pending,
            &crate::records::Change::MergeShared {
                site: record(i, 2).stored_item_as_is(),
            },
        )
        .unwrap();
        pending.write_all(b"\n").unwrap();
    }
    raw.into_inner().unwrap().sync_all().unwrap();
    pending.into_inner().unwrap().sync_all().unwrap();
    assert!(std::fs::metadata(&journal).unwrap().len() > crate::records::MIN_COMPACT_BYTES);
    let raw_digest = sha2::Sha256::digest(std::fs::read(&path).unwrap());
    let key_dir = tempfile::tempdir().unwrap();
    let key = plumb_net::load_or_create_key(&key_dir.path().join("source.key")).unwrap();
    let crawler = key.public().to_peer_id();
    let batches = dir.path().join("net/batches");
    std::fs::create_dir(dir.path().join("net")).unwrap();
    let mut legacy = BatchStore::open(&batches).unwrap();
    let now = now_unix();
    let mut signed = Vec::new();
    let mut initial_delivery_bytes = Vec::new();
    for n in 0..BATCHES {
        let made = now - (BATCHES - n) as u64;
        let records: Vec<_> = (n * PER_BATCH..(n + 1) * PER_BATCH)
            .map(|i| record(i, made))
            .collect();
        let batch = Batch::sign(&key, &listed(&records), epoch_of(made), MAX_SHARE_PPM, made)
            .unwrap()
            .unwrap();
        assert_eq!(batch.records.len(), PER_BATCH);
        if n < 6 {
            // These trusted crawls carry only homepage facts, which replay
            // preserves. Check each of the six held batches' records as
            // the receiver writes them.
            let bytes = records
                .iter()
                .map(|record| plumb_core::stored::line(record).unwrap().len() as u64 + 1)
                .sum::<u64>();
            initial_delivery_bytes.push(plumb_core::storage::allocation_for(bytes + 1));
        }
        legacy.insert(&batch).unwrap();
        let file = batches.join(format!("{}.json", batch.id()));
        signed.push((
            batch.id(),
            sha2::Sha256::digest(std::fs::read(file).unwrap()),
        ));
    }
    drop(legacy);
    // None marked delivered: all are sent again.
    let mut config = test_config(dir.path());
    config.take_new_sites = true;
    let rank = RankConfig::default();
    let opened = open_data_dir(&config, rank).unwrap();
    let budget = plumb_core::storage::StorageBudget::open(dir.path(), u64::MAX).unwrap();
    fn allocation_snapshot(
        root: &Path,
    ) -> std::collections::BTreeMap<PathBuf, (u64, u64, Option<std::time::SystemTime>)> {
        fn walk(
            path: &Path,
            root: &Path,
            snapshot: &mut std::collections::BTreeMap<
                PathBuf,
                (u64, u64, Option<std::time::SystemTime>),
            >,
        ) {
            let m = std::fs::symlink_metadata(path).unwrap();
            snapshot.insert(
                path.strip_prefix(root).unwrap().to_path_buf(),
                (
                    m.len(),
                    plumb_core::storage::file_bytes(path).unwrap(),
                    m.modified().ok(),
                ),
            );
            if m.is_dir() {
                for e in std::fs::read_dir(path).unwrap() {
                    walk(&e.unwrap().path(), root, snapshot);
                }
            }
        }
        let mut snapshot = std::collections::BTreeMap::new();
        walk(root, root, &mut snapshot);
        snapshot
    }
    let allocation_history = Mutex::new(allocation_snapshot(dir.path()));
    let ledger_checkpoint = |label: &str| {
        let _mutation = budget.mutation();
        let status = budget.status();
        let disk = plumb_core::storage::directory_bytes(dir.path()).unwrap();
        let current = allocation_snapshot(dir.path());
        let mut previous = allocation_history.lock().unwrap();
        for (path, now) in &current {
            if let Some(before) = previous.get(path) {
                if before != now {
                    eprintln!(
                        "allocation {label} {}: before={before:?} after={now:?}",
                        path.display()
                    );
                }
            } else {
                eprintln!("allocation {label} {}: added={now:?}", path.display());
            }
        }
        for (path, before) in previous.iter() {
            if !current.contains_key(path) {
                eprintln!("allocation {label} {}: removed={before:?}", path.display());
            }
        }
        *previous = current;
        eprintln!(
            "replay ledger {label}: used={}, linked={disk}, held={}, reserved={}, delta={}",
            status.used_bytes,
            status.reader_held_bytes,
            status.reserved_bytes,
            i128::from(status.used_bytes) - i128::from(disk + status.reader_held_bytes)
        );
    };
    ledger_checkpoint("initial-closed-fixture");
    let initial_inputs = [
        crate::records::FoldInputAllocation::read(&path),
        crate::records::FoldInputAllocation::read(&journal),
    ];
    crate::records::start_fold_allocation_evidence(&path);
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let inner = Arc::new(Inner::new(
        config,
        rank,
        opened,
        stopped,
        Some(budget.clone()),
    ));
    crate::node::network::append_inbox(&inner, &listed(&[record(RECORDS - 1, 3)])).unwrap();
    std::fs::rename(&inner.paths.inbox, &inner.paths.absorbing).unwrap();
    let absorbing = std::fs::read(&inner.paths.absorbing).unwrap();
    let source = Arc::new(
        plumb_net::BucketTable::build::<plumb_core::site::Item>(
            &dir.path().join("net/buckets"),
            &[],
        )
        .unwrap(),
    );
    let mut net_config = plumb_net::NetConfig::new(dir.path().join("net"));
    net_config.listen.clear();
    net_config.bootstrap.clear();
    net_config.upnp = false;
    net_config.local_discovery = false;
    net_config.round_every = None;
    net_config.fill = false;
    net_config.trusted_peers = vec![crawler];
    net_config.storage_budget = Some(budget.clone());
    ledger_checkpoint("before-network-start");
    let (net, mut records) = plumb_net::start(net_config.clone(), source.clone())
        .await
        .unwrap();
    let credits = plumb_net::credits::Ledger::open(&dir.path().join("net/credits"))
        .unwrap()
        .account(&crawler);
    // The network's first tick saves popularity.json (33 bytes) off its
    // swarm task, reserving them in this budget while it writes. Wait it
    // out, so the strict reservation checks below see only replay.
    let popularity = dir.path().join("net/popularity.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !popularity.exists() {
        assert!(Instant::now() < deadline, "popularity.json was never saved");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // write_atomic holds the mutation lock until it charges what it wrote.
    drop(budget.mutation());
    // Fixed across the failing old-allowance run and the repaired run.
    let limit = 663_711_744;
    budget.set_limit(limit);
    ledger_checkpoint("startup");
    let deadline = Instant::now() + Duration::from_secs(10);
    while records.len() != 4 {
        assert!(
            Instant::now() < deadline,
            "replay did not fill the real four-slot channel"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let first = records.recv().await.unwrap();
    assert_eq!(first.sites.len(), PER_BATCH);
    let first_bytes = first
        .reservation
        .as_ref()
        .expect("replay delivery admission")
        .bytes();
    assert_eq!(first_bytes, initial_delivery_bytes[0]);
    // Each of the six held admissions is its own batch's encoded length.
    let full_queue_bytes: u64 = initial_delivery_bytes.iter().sum();
    // Hold actual receiver I/O, while its owned reservation and the producer's
    // fifth pending batch remain live. The raw worker resumes an absorbing file.
    let (locked, ready) = tokio::sync::oneshot::channel();
    let (release_lock, released) = std::sync::mpsc::channel();
    let locked_inner = inner.clone();
    let inbox_holder = std::thread::spawn(move || {
        let _guard = locked_inner.inbox_lock.lock().unwrap();
        locked.send(()).unwrap();
        let _ = released.recv();
    });
    ready.await.unwrap();
    let first_writer = inner.clone();
    let first_consumer = tokio::spawn(async move {
        crate::node::network::persist_delivery(
            first_writer,
            first.sites,
            SiteList::new(),
            first.reservation,
            Some(first.acknowledgment),
        )
        .await;
    });
    // Four queued deliveries, the blocked receiver, and one pending producer
    // must all own their admission before the fold's pressure snapshot.
    loop {
        let status = budget.status();
        assert!(
            status.reserved_bytes <= full_queue_bytes,
            "replay exceeded six held admissions: expected {full_queue_bytes}, status {status:?}"
        );
        if records.len() == 4 && status.reserved_bytes == full_queue_bytes {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "replay did not fill and charge all six owners: queued={}, expected={full_queue_bytes}, status={status:?}",
            records.len()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (old_reader, old_lease) = {
        let _mutation = budget.mutation();
        (
            std::fs::File::open(&path).unwrap(),
            budget.read_lease(&path),
        )
    };
    let worker = inner.clone();
    let first_fold =
        tokio::task::spawn_blocking(move || crate::node::network::absorb_inbox(&worker))
            .await
            .unwrap();
    if let Err(error) = first_fold {
        assert_eq!(
            sha2::Sha256::digest(std::fs::read(&path).unwrap()),
            raw_digest
        );
        assert_eq!(std::fs::read(&inner.paths.absorbing).unwrap(), absorbing);
        assert!(journal.exists());
        release_lock.send(()).unwrap();
        inbox_holder.join().unwrap();
        stop.send(true).unwrap();
        net.shutdown().await;
        first_consumer.await.unwrap();
        drop(records);
        panic!("full replay queue prevents raw progress at fixed cap {limit}, held {full_queue_bytes}: {error:#}");
    }
    assert_eq!(first_fold.unwrap(), 1);
    assert_eq!(
        budget.status().reserved_bytes,
        full_queue_bytes,
        "the full receive queue must remain charged during the fold"
    );
    assert!(!journal.exists());
    assert!(!inner.paths.absorbing.exists());
    ledger_checkpoint("first-fold");
    // Use the real bounded maintenance path while the receive queue and an
    // unlinked old raw reader still own their room. Preserve prior peaks and
    // residual evidence; reconciliation cannot excuse a staging overshoot.
    let before_checkpoint = budget.status();
    assert!(before_checkpoint.reader_held_bytes > 0);
    assert_eq!(before_checkpoint.reserved_bytes, full_queue_bytes);
    let at = Instant::now();
    crate::node::worker::sweep_at(&inner, at + Duration::from_secs(301)).await;
    let after_checkpoint = budget.status();
    assert_eq!(
        after_checkpoint.reader_held_bytes,
        before_checkpoint.reader_held_bytes
    );
    assert_eq!(after_checkpoint.reserved_bytes, full_queue_bytes);
    assert_eq!(after_checkpoint.limit_bytes, limit);
    assert_eq!(
        after_checkpoint.used_bytes,
        plumb_core::storage::directory_bytes(dir.path()).unwrap()
            + after_checkpoint.reader_held_bytes
    );
    eprintln!(
        "bounded maintenance preserved queue={} reader={} corrected conservative used={} to={}",
        full_queue_bytes,
        after_checkpoint.reader_held_bytes,
        before_checkpoint.used_bytes,
        after_checkpoint.used_bytes
    );
    {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::BufReader::new(old_reader.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(
            plumb_core::stored::read(line.as_bytes())
                .unwrap()
                .crawled_at,
            Some(1)
        );
    }
    drop(old_reader);
    drop(old_lease);
    assert_eq!(budget.status().reader_held_bytes, 0);
    assert_eq!(budget.status().reserved_bytes, full_queue_bytes);
    ledger_checkpoint("after-bounded-maintenance-and-reader-release");
    release_lock.send(()).unwrap();
    inbox_holder.join().unwrap();
    first_consumer.await.unwrap();
    let delivered = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let count = delivered.clone();
    let receiver = inner.clone();
    let consumer = tokio::spawn(async move {
        for _ in 1..BATCHES {
            let delivery = records.recv().await.unwrap();
            assert_eq!(delivery.sites.len(), PER_BATCH);
            crate::node::network::persist_delivery(
                receiver.clone(),
                delivery.sites,
                SiteList::new(),
                delivery.reservation,
                Some(delivery.acknowledgment),
            )
            .await;
            count.fetch_add(1, Ordering::SeqCst);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(180);
    let outcome = async {
        let mut absorbed = 0;
        let mut during_backlog = false;
        let mut peak = budget.status().used_bytes + budget.status().reserved_bytes;
        while delivered.load(Ordering::SeqCst) < BATCHES as u64 || inner.paths.inbox.exists() || inner.paths.absorbing.exists() {
            anyhow::ensure!(Instant::now() < deadline, "receive/raw progress stalled: delivered {}, budget {:?}", delivered.load(Ordering::SeqCst), budget.status());
            if inner.paths.absorbing.exists() || inner.inbox_records.load(Ordering::SeqCst) >= crate::node::network::REBUILD_AFTER_RECORDS || delivered.load(Ordering::SeqCst) == BATCHES as u64 {
                let worker = inner.clone();
                let result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), tokio::task::spawn_blocking(move || crate::node::network::absorb_inbox(&worker))).await??;
                if let Ok(n) = result {
                    absorbed += n;
                    during_backlog |= n > 0 && delivered.load(Ordering::SeqCst) < BATCHES as u64;
                }
            }
            let status = budget.status();
            peak = peak.max(status.used_bytes + status.reserved_bytes);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        consumer.await?;
        ledger_checkpoint("backlog-drained");
        anyhow::ensure!(absorbed == RECORDS as u64 && during_backlog, "raw worker did not absorb the live backlog");
        let worker = inner.clone();
        tokio::task::spawn_blocking(move || crate::records::RecordStore::open(&worker.paths.records).with_budget(worker.storage.clone()).fold()).await??;
        ledger_checkpoint("final-fold");
        let mut sites = 0;
        crate::outline::for_each_record(&path, |record| {
            assert!(record.crawled_at.unwrap() >= now - BATCHES as u64);
            sites += 1;
        })?;
        anyhow::ensure!(sites == RECORDS, "raw records were lost");
        for (id, digest) in &signed {
            anyhow::ensure!(sha2::Sha256::digest(std::fs::read(batches.join(format!("{id}.json")))?) == *digest, "signed bytes changed");
            anyhow::ensure!(batches.join(format!("{id}.delivered-trusted")).is_file(), "delivery was not durably acknowledged");
        }
        anyhow::ensure!(plumb_net::credits::Ledger::open(&dir.path().join("net/credits"))?.account(&crawler) == credits, "replay awarded credits");
        // The limit is a target, as in 0.2.0: the peak is reported, not bounded by it.
        eprintln!("full replay progress: deliveries={BATCHES}, records={RECORDS}, queued={full_queue_bytes}, observed ledger peak={peak}, limit={limit}");
        Ok::<_, anyhow::Error>(())
    }.await;
    stop.send(true).unwrap();
    ledger_checkpoint("before-shutdown");
    net.shutdown().await;
    ledger_checkpoint("after-shutdown");
    outcome.unwrap();
    assert_eq!(budget.status().reserved_bytes, 0);
    let evidence = crate::records::take_fold_allocation_evidence(&path);
    let mut before = None;
    let mut residual_bound: u64 = initial_inputs.iter().map(|s| s.dense_slack()).sum();
    for phase in &evidence.phases {
        eprintln!("fold allocation phase {}: {:?}", phase.phase, phase.inputs);
        if phase.phase == "before-close" {
            before = Some(&phase.inputs);
        }
        if phase.phase == "before-write" {
            if let Some(before) = before.take() {
                for i in 0..2 {
                    assert!(
                        before[i].same_generation(&phase.inputs[i]),
                        "allocation comparison changed source generation"
                    );
                    assert!(
                        phase.inputs[i].allocated <= before[i].allocated,
                        "a closed input grew outside the measured filesystem envelope"
                    );
                    if !before[i].same_generation(&initial_inputs[i]) {
                        residual_bound += before[i].allocated - phase.inputs[i].allocated;
                    }
                }
            }
        }
    }
    let status = budget.status();
    let actual =
        plumb_core::storage::directory_bytes(dir.path()).unwrap() + status.reader_held_bytes;
    eprintln!("bounded replay accounting residual={} bound={residual_bound} physical+queued peak={} charged+reserved peak={} uncovered peak={}", status.used_bytes.saturating_sub(actual), evidence.physical_plus_queued_peak, evidence.charged_plus_reserved_peak, evidence.uncovered_peak);
    // Keep demonstrated conservative shrinkage visible. Reject undercharge or
    // an unexplained residual; do not force equality with a final recount.
    assert!(status.used_bytes >= actual);
    assert!(status.used_bytes - actual <= residual_bound);
    assert_eq!(evidence.uncovered_peak, 0);
    let (restarted, mut replayed) = plumb_net::start(net_config, source).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), replayed.recv())
            .await
            .is_err()
    );
    restarted.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_maintenance_reconciles_owner_bytes_only_at_the_bounded_checkpoint() {
    let dir = seeded_dir();
    let node = start(test_config(dir.path())).await.unwrap();
    wait_for(node.addr(), "initial index", ready_and_idle).await;
    let budget = node.inner.storage.clone().unwrap();
    let raw = std::fs::read(&node.inner.paths.records).unwrap();
    let used = budget.status().used_bytes;
    budget.set_limit(MB);
    std::fs::write(
        dir.path().join("owner-idle-probe"),
        vec![1; 2 * MB as usize],
    )
    .unwrap();
    let at = Instant::now();
    crate::node::worker::sweep_at(&node.inner, at).await;
    assert_eq!(budget.status().used_bytes, used);
    crate::node::worker::sweep_at(&node.inner, at + Duration::from_secs(301)).await;
    assert!(budget.status().used_bytes >= used + 2 * MB);
    assert!(budget.status().backpressure);
    let counted = budget.status().used_bytes;
    std::fs::write(dir.path().join("owner-second-probe"), vec![1; 64 * 1024]).unwrap();
    for _ in 0..10 {
        node.inner.recount_disk();
        crate::node::worker::sweep_at(&node.inner, at + Duration::from_secs(302)).await;
        assert_eq!(budget.status().used_bytes, counted);
    }
    assert_eq!(std::fs::read(&node.inner.paths.records).unwrap(), raw);
    assert!(!search(node.addr(), "chase").await.is_empty());
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn genuine_oversized_replay_reaches_the_receiver_and_does_not_block_later_batches() {
    use plumb_net::assign::{epoch_of, is_assigned, MAX_SHARE_PPM};
    use plumb_net::batch::Batch;
    use plumb_net::store::BatchStore;
    use sha2::{Digest, Sha256};

    const SITES: usize = plumb_net::batch::MAX_BATCH_RECORDS;
    let dir = seeded_dir();
    let now = now_unix();
    let epoch = epoch_of(now);
    let batches = dir.path().join("net/batches");
    std::fs::create_dir_all(&batches).unwrap();
    let mut store = BatchStore::open(&batches).unwrap();
    let mut signed = Vec::new();
    let mut made = now;
    let own_key = plumb_net::node::load_or_create_key(&dir.path().join("net/node.key")).unwrap();
    let good_key = plumb_net::node::load_or_create_key(&dir.path().join("following.key")).unwrap();
    let trigger_key = plumb_net::node::load_or_create_key(&dir.path().join("trigger.key")).unwrap();
    let trigger_peer = trigger_key.public().to_peer_id();
    let domains: Vec<_> = (0u64..)
        .map(|n| format!("oversized-facts-{n:08}.example"))
        .filter(|domain| is_assigned(epoch, &trigger_peer, domain, MAX_SHARE_PPM))
        .take(SITES)
        .collect();
    // Each earlier crawl is an item of exactly the protocol's 16 KiB line
    // limit, spread over eight legal 16 MiB batches, and its site is linked
    // to, as in the later thin crawls. Signs that are not letters or digits
    // do not change agreement's normalized description, and other names are
    // not compared. The thin crawls agree with the earlier, newer own and
    // trusted facts and release them, each a little under 16 KiB as the
    // receiver writes it, and with each a mention of the site: over 64 MiB.
    let earlier = {
        let symbols = |chars: usize| "\u{1d11e}".repeat(chars);
        let mut record = SiteRecord::new(&domains[0]);
        record.crawled_at = Some(now + 1);
        record.description = Some("same".into());
        record.signals.linking_domains = 1;
        for n in 0..plumb_core::MAX_ALIASES {
            record.add_alias(&format!("{n} {}", symbols(200)));
        }
        let room = plumb_net::batch::MAX_RECORD_BYTES - record.to_line().unwrap().len();
        let pad = format!("{}{}", symbols(room / 4), ".".repeat(room % 4));
        record.description = Some(format!("same{pad}"));
        assert_eq!(
            record.to_line().unwrap().len(),
            plumb_net::batch::MAX_RECORD_BYTES
        );
        record
    };
    for (key, own) in [(&own_key, true), (&good_key, false)] {
        let peer = key.public().to_peer_id();
        for chunk in domains.chunks(1024) {
            let records: Vec<_> = chunk
                .iter()
                .map(|domain| SiteRecord {
                    domain: domain.clone(),
                    ..earlier.clone()
                })
                .collect();
            let batch = Batch::sign(key, &listed(&records), epoch, MAX_SHARE_PPM, made)
                .unwrap()
                .unwrap();
            assert_eq!(batch.records.len(), records.len());
            assert_eq!(batch.check(made).unwrap(), peer);
            assert!(batch
                .records
                .iter()
                .all(|line| line.len() <= plumb_net::batch::MAX_RECORD_BYTES));
            store.insert(&batch).unwrap();
            if !own {
                std::fs::write(
                    batches.join(format!("{}.delivered-trusted", batch.id())),
                    b"persisted\n",
                )
                .unwrap();
            }
            signed.push((
                batch.id(),
                Sha256::digest(
                    std::fs::read(batches.join(format!("{}.json", batch.id()))).unwrap(),
                ),
            ));
            made += 1;
        }
    }
    let thin: Vec<_> = domains
        .iter()
        .map(|domain| {
            let mut record = SiteRecord::new(domain);
            record.crawled_at = Some(now);
            record.description = Some("same".into());
            record.signals.linking_domains = 1;
            record
        })
        .collect();
    let trigger = Batch::sign(&trigger_key, &listed(&thin), epoch, MAX_SHARE_PPM, made)
        .unwrap()
        .unwrap();
    assert_eq!(trigger.records.len(), thin.len());
    trigger.check(made).unwrap();
    let trigger_id = trigger.id();
    store.insert(&trigger).unwrap();
    let trigger_signed = std::fs::read(batches.join(format!("{trigger_id}.json"))).unwrap();

    let good_peer = good_key.public().to_peer_id();
    let mut good = SiteRecord::new("following-replay.example");
    good.title = Some("Smaller batch after unsupported output".into());
    good.crawled_at = Some(now);
    good.news = vec![plumb_core::Headline::checked(
        &good.domain,
        "Following headline",
        "https://following-replay.example/news",
        now,
        now,
    )
    .unwrap()];
    let following = Batch::sign(&good_key, &listed(&[good]), epoch, MAX_SHARE_PPM, made + 1)
        .unwrap()
        .unwrap();
    let following_id = following.id();
    store.insert(&following).unwrap();
    let following_signed = std::fs::read(batches.join(format!("{following_id}.json"))).unwrap();
    drop(store);

    let credits = dir.path().join("net/credits");
    let mut ledger = plumb_net::credits::Ledger::open(&credits).unwrap();
    ledger.record(&[plumb_net::agree::Verdict {
        crawler: good_peer,
        crawled_at: now,
        agreed: true,
        witnesses: 0,
        trusted: true,
    }]);
    ledger.save().unwrap();
    let credit_bytes = std::fs::read(credits.join("ledger.json")).unwrap();
    let mut config = test_config(dir.path());
    config.take_new_sites = true;
    let rank = RankConfig::default();
    let opened = open_data_dir(&config, rank).unwrap();
    let budget = plumb_core::storage::StorageBudget::open(dir.path(), u64::MAX).unwrap();
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let inner = Arc::new(Inner::new(
        config,
        rank,
        opened,
        stopped,
        Some(budget.clone()),
    ));
    let raw = std::fs::read(&inner.paths.records).unwrap();
    let table = Arc::new(
        plumb_net::BucketTable::build::<plumb_core::site::Item>(
            &inner.paths.net.join("buckets"),
            &[],
        )
        .unwrap(),
    );
    let mut net_config = plumb_net::NetConfig::new(inner.paths.net.clone());
    net_config.listen.clear();
    net_config.bootstrap.clear();
    net_config.upnp = false;
    net_config.local_discovery = false;
    net_config.round_every = None;
    net_config.fill = false;
    net_config.trusted_peers = vec![good_peer];
    net_config.storage_budget = Some(budget.clone());
    let mut cap = None;
    for restart in [false, true] {
        eprintln!("starting genuine signed replay restart={restart}");
        let (net, mut received) = tokio::time::timeout(
            Duration::from_secs(300),
            plumb_net::start(net_config.clone(), table.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        if cap.is_none() {
            let limit = budget.status().used_bytes + 128 * 1024;
            budget.set_limit(limit);
            cap = Some(limit);
        }
        let oversized = tokio::time::timeout(Duration::from_secs(900), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            oversized.reservation.is_none(),
            "oversized replay waited for an impossible token"
        );
        assert!(!batches
            .join(format!("{trigger_id}.delivered-agreed"))
            .exists());
        let (news, records) = oversized
            .sites
            .partition(|site| !plumb_core::site::news(site).is_empty());
        let mut count = plumb_core::storage::ByteCount {
            bytes: 0,
            limit: u64::MAX,
        };
        for line in records.lines() {
            std::io::Write::write_all(&mut count, line).unwrap();
            std::io::Write::write_all(&mut count, b"\n").unwrap();
        }
        assert_eq!(records.len(), 2 * SITES);
        assert!(
            count.bytes > plumb_net::node::MAX_RECORD_DELIVERY_BYTES,
            "{}",
            count.bytes
        );
        eprintln!("genuine signed replay restart={restart}: input={} final={} receiver_limit={} quota_room=131072", trigger.records.iter().map(String::len).sum::<usize>(), count.bytes, plumb_net::node::MAX_RECORD_DELIVERY_BYTES);
        tokio::time::timeout(
            Duration::from_secs(30),
            crate::node::network::persist_delivery(
                inner.clone(),
                records,
                news,
                oversized.reservation,
                Some(oversized.acknowledgment),
            ),
        )
        .await
        .unwrap();
        assert!(!batches
            .join(format!("{trigger_id}.delivered-agreed"))
            .exists());
        if !restart {
            let delivery = tokio::time::timeout(Duration::from_secs(10), received.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(delivery.reservation.is_some());
            let (news, records) = delivery
                .sites
                .partition(|site| !plumb_core::site::news(site).is_empty());
            assert_eq!(news.len(), 1);
            assert_eq!(records.len(), 1);
            tokio::time::timeout(
                Duration::from_secs(10),
                crate::node::network::persist_delivery(
                    inner.clone(),
                    records,
                    news,
                    delivery.reservation,
                    Some(delivery.acknowledgment),
                ),
            )
            .await
            .unwrap();
            assert!(batches
                .join(format!("{following_id}.delivered-trusted"))
                .exists());
            assert_eq!(
                plumb_core::stored::read_file(&inner.paths.inbox)
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(inner.news.headline_count(), 1);
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(300), received.recv())
                    .await
                    .is_err(),
                "acknowledged following delivery replayed twice"
            );
        }
        assert_eq!(budget.status().limit_bytes, cap.unwrap());
        assert_eq!(std::fs::read(&inner.paths.records).unwrap(), raw);
        assert_eq!(
            std::fs::read(batches.join(format!("{trigger_id}.json"))).unwrap(),
            trigger_signed
        );
        assert_eq!(
            std::fs::read(batches.join(format!("{following_id}.json"))).unwrap(),
            following_signed
        );
        for (id, hash) in &signed {
            assert_eq!(
                Sha256::digest(std::fs::read(batches.join(format!("{id}.json"))).unwrap()),
                *hash
            );
        }
        net.shutdown().await;
        drop(received);
        assert_eq!(
            std::fs::read(credits.join("ledger.json")).unwrap(),
            credit_bytes
        );
    }
    assert_eq!(budget.status().reserved_bytes, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_owned_deliveries_retry_growth_offline_and_leave_unsupported_payloads_pending() {
    for unsupported in [false, true] {
        let source_dir = tempfile::tempdir().unwrap();
        let mut source_config = plumb_net::NetConfig::new(source_dir.path().to_owned());
        source_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
        source_config.bootstrap.clear();
        source_config.upnp = false;
        source_config.local_discovery = false;
        source_config.round_every = None;
        source_config.fill = false;
        let source_table = Arc::new(
            plumb_net::BucketTable::build::<plumb_core::site::Item>(
                &source_dir.path().join("buckets"),
                &[],
            )
            .unwrap(),
        );
        let (source, _source_records) =
            plumb_net::start(source_config, source_table).await.unwrap();
        let source_id = source.peer_id();
        let deadline = Instant::now() + Duration::from_secs(15);
        let source_addr: plumb_net::Multiaddr = loop {
            if let Some(addr) = source.status().listening.first() {
                break addr.parse().unwrap();
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let dir = seeded_dir();
        let expendable = dir.path().join("expendable-cache");
        std::fs::write(&expendable, [1; 64 * 1024]).unwrap();
        let mut config = test_config(dir.path());
        config.take_new_sites = true;
        let rank = RankConfig::default();
        let opened = open_data_dir(&config, rank).unwrap();
        let budget = plumb_core::storage::StorageBudget::open(dir.path(), u64::MAX).unwrap();
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let inner = Arc::new(Inner::new(
            config,
            rank,
            opened,
            stopped,
            Some(budget.clone()),
        ));
        let raw = std::fs::read(&inner.paths.records).unwrap();
        let table = Arc::new(
            plumb_net::BucketTable::build::<plumb_core::site::Item>(
                &inner.paths.net.join("buckets"),
                &[],
            )
            .unwrap(),
        );
        let mut net_config = plumb_net::NetConfig::new(inner.paths.net.clone());
        net_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
        net_config.bootstrap = vec![source_addr.with_p2p(source_id).unwrap()];
        net_config.upnp = false;
        net_config.local_discovery = false;
        net_config.round_every = None;
        net_config.fill = false;
        net_config.trusted_peers = vec![source_id];
        net_config.storage_budget = Some(budget.clone());
        let (net, mut received) = plumb_net::start(net_config.clone(), table.clone())
            .await
            .unwrap();
        let mut record = SiteRecord::new("owned-growth.example");
        record.url = Some("https://owned-growth.example/".into());
        record.title = Some("Confirmed output survives admission failure".into());
        record.description = Some("x".repeat(12 * 1024));
        record.crawled_at = Some(now_unix());
        let id = source.publish(listed(&[record])).await.unwrap().unwrap();
        let mut delivery = tokio::time::timeout(Duration::from_secs(20), received.recv())
            .await
            .unwrap()
            .unwrap();
        let credit_dir = inner.paths.net.join("credits");
        // Stop both transports after live confirmation, flushing the credit
        // ledger. The same process retains this receiver's final owned payload;
        // quota recovery below never restarts or reobserves agreement.
        source.shutdown().await;
        net.shutdown().await;
        let credited = plumb_net::credits::Ledger::open(&credit_dir)
            .unwrap()
            .account(&source_id);
        assert_eq!(credited.confirmed, 1);
        assert!(credited.earned > 0);
        let batches = inner.paths.net.join("batches");
        let signed_path = batches.join(format!("{id}.json"));
        let signed = std::fs::read(&signed_path).unwrap();
        let marker = batches.join(format!("{id}.delivered-trusted"));
        // Fault injection models a final output larger than its owned token.
        // No agreement, batch storage or credit operation is repeated.
        let owned = delivery.reservation.as_mut().unwrap();
        assert!(owned.bytes() > 8192);
        owned.commit(owned.bytes() - 8192, 0, 0);
        let cap = budget.status().used_bytes + budget.status().reserved_bytes;
        budget.set_limit(cap);
        if unsupported {
            // The delivery's one site.
            delivery.sites.retain_mut(|site| {
                site.about = Some("x".repeat(65 * 1024 * 1024));
                true
            });
        }
        let rejected = budget.status().rejected_writes;
        let pending = tokio::spawn(crate::node::network::persist_delivery(
            inner.clone(),
            delivery.sites,
            SiteList::new(),
            delivery.reservation,
            Some(delivery.acknowledgment),
        ));
        if unsupported {
            // Permanent encoding failures return promptly, allowing the next
            // delivery to proceed, while the signed source remains pending.
            tokio::time::timeout(Duration::from_secs(10), pending)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(budget.status().reserved_bytes, 0);
        } else {
            // Past the limit the receiver's admission grows rather than
            // waits, as in 0.2.0, whose limit was a target.
            tokio::time::timeout(Duration::from_secs(10), pending)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(budget.status().rejected_writes, rejected);
            assert_eq!(
                plumb_core::stored::read_file(&inner.paths.inbox)
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(inner.inbox_records.load(Ordering::SeqCst), 1);
            assert!(marker.is_file());
        }
        assert_eq!(budget.status().limit_bytes, cap);
        assert_eq!(std::fs::read(&inner.paths.records).unwrap(), raw);
        assert_eq!(std::fs::read(&signed_path).unwrap(), signed);
        assert_eq!(
            plumb_net::credits::Ledger::open(&credit_dir)
                .unwrap()
                .account(&source_id),
            credited
        );
        net.shutdown().await;
        drop(received);
        if unsupported {
            assert!(!marker.exists());
            assert!(!inner.paths.inbox.exists());
            plumb_core::storage::remove_file(&expendable, Some(&budget)).unwrap();
        }
        // With the source offline, restart recovers an unacknowledged signed
        // payload; an acknowledged retry never enqueues a duplicate.
        net_config.bootstrap.clear();
        let (restarted, mut replayed) = plumb_net::start(net_config, table).await.unwrap();
        if unsupported {
            let delivery = tokio::time::timeout(Duration::from_secs(10), replayed.recv())
                .await
                .unwrap()
                .unwrap();
            crate::node::network::persist_delivery(
                inner.clone(),
                delivery.sites,
                SiteList::new(),
                delivery.reservation,
                Some(delivery.acknowledgment),
            )
            .await;
            assert!(marker.is_file());
            assert_eq!(
                plumb_core::stored::read_file(&inner.paths.inbox)
                    .unwrap()
                    .len(),
                1
            );
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(300), replayed.recv())
                    .await
                    .is_err()
            );
        }
        restarted.shutdown().await;
        drop(replayed);
        // The replay runs on a blocking thread that shutting down does not
        // wait for; it lets its reservation go once it sees the stop.
        let deadline = Instant::now() + Duration::from_secs(5);
        while budget.status().reserved_bytes != 0 {
            assert!(Instant::now() < deadline, "the replay kept its reservation");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(std::fs::read(&signed_path).unwrap(), signed);
        assert_eq!(
            plumb_net::credits::Ledger::open(&credit_dir)
                .unwrap()
                .account(&source_id),
            credited
        );
        stop.send(true).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_network_delivery_survives_inbox_io_failure_without_restart() {
    let dir = seeded_dir();
    let node = start(test_config(dir.path())).await.unwrap();
    wait_for(node.addr(), "initial index", ready_and_idle).await;
    let budget = node.inner.storage.clone().unwrap();
    let raw = std::fs::read(&node.inner.paths.records).unwrap();
    plumb_core::storage::create_directory(&node.inner.paths.inbox, Some(&budget)).unwrap();
    let delivery = tokio::spawn(crate::node::network::persist_delivery(
        node.inner.clone(),
        listed(&[SiteRecord::new("retry-inbox.example")]),
        SiteList::new(),
        None,
        None,
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    while budget.status().reserved_bytes == 0 {
        assert!(
            Instant::now() < deadline,
            "delivery was not retained for retry"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!delivery.is_finished());
    assert_eq!(node.inner.inbox_records.load(Ordering::SeqCst), 0);
    {
        let _mutation = budget.mutation();
        let bytes = plumb_core::storage::file_bytes(&node.inner.paths.inbox).unwrap();
        std::fs::remove_dir(&node.inner.paths.inbox).unwrap();
        budget.removed(bytes);
    }
    tokio::time::timeout(Duration::from_secs(10), delivery)
        .await
        .unwrap()
        .unwrap();
    let inbox: Vec<SiteRecord> = plumb_core::stored::read_file(&node.inner.paths.inbox).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].domain, "retry-inbox.example");
    assert_eq!(node.inner.inbox_records.load(Ordering::SeqCst), 1);
    assert_eq!(budget.status().reserved_bytes, 0);
    assert_eq!(std::fs::read(&node.inner.paths.records).unwrap(), raw);
    assert!(!search(node.addr(), "chase").await.is_empty());
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publishing_an_external_corpus_fold_cannot_free_node_storage_room() {
    let dir = seeded_dir();
    let external = tempfile::tempdir().unwrap();
    let path = external.path().join("records.jsonl");
    write_jsonl(&path, &[SiteRecord::new("external-publish.example")]).unwrap();
    let mut store = crate::records::RecordStore::open(&path);
    for n in 0..100 {
        store
            .save(&[crate::records::Change::Mark {
                domain: "external-publish.example".into(),
                attempted_at: Some(n),
                failures: 1,
            }])
            .unwrap();
    }
    drop(store);
    let journal = crate::records::journal_path(&path);
    let mut config = test_config(dir.path());
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.fill = false;
    net.trusted_peers.clear();
    config.network = Some(net);
    let node = start(config).await.unwrap();
    wait_for(node.addr(), "initial index", ready_and_idle).await;
    let budget = node.inner.storage.as_ref().unwrap();
    let raw = std::fs::read(&node.inner.paths.records).unwrap();
    let before = budget.status();
    assert_eq!(
        crate::node::network::publish_new_records(&node.inner, &path)
            .await
            .unwrap(),
        0
    );
    assert!(!journal.exists());
    assert_eq!(budget.status(), before);
    assert_eq!(std::fs::read(&node.inner.paths.records).unwrap(), raw);
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_receiver_restart_recovers_an_unacknowledged_inbox_without_crediting_twice() {
    let source_dir = tempfile::tempdir().unwrap();
    let mut source_config = plumb_net::NetConfig::new(source_dir.path().to_owned());
    source_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    source_config.upnp = false;
    source_config.local_discovery = false;
    source_config.round_every = None;
    let table = plumb_net::BucketTable::build::<plumb_core::site::Item>(
        &source_dir.path().join("buckets"),
        &[],
    )
    .unwrap();
    let (source, _source_records) = plumb_net::start(source_config, Arc::new(table))
        .await
        .unwrap();
    let source_id = source.peer_id();
    let source_addr: plumb_net::Multiaddr = loop {
        if let Some(addr) = source.status().listening.first() {
            break addr.parse().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let dir = seeded_dir();
    let mut config = test_config(dir.path());
    config.take_new_sites = true;
    let mut net = plumb_net::NetConfig::new(PathBuf::new());
    net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    net.upnp = false;
    net.local_discovery = false;
    net.round_every = None;
    net.fill = false;
    net.trusted_peers = vec![source_id];
    net.bootstrap = vec![source_addr.with_p2p(source_id).unwrap()];
    config.network = Some(net);
    let receiver = start(config.clone()).await.unwrap();
    wait_for(receiver.addr(), "initial index", ready_and_idle).await;
    let budget = receiver.inner.storage.as_ref().unwrap();
    let raw = std::fs::read(&receiver.inner.paths.records).unwrap();
    let inbox_path = receiver.inner.paths.inbox.clone();
    plumb_core::storage::create_directory(&inbox_path, Some(budget)).unwrap();
    let mut record = SiteRecord::new("restart-inbox.example");
    record.url = Some("https://restart-inbox.example/".into());
    record.title = Some("Durable receiver restart".into());
    record.crawled_at = Some(now_unix());
    record.news = vec![plumb_core::Headline {
        title: "A headline that survives receiver restart".into(),
        url: "https://restart-inbox.example/receiver-restart".into(),
        at: now_unix(),
    }];
    let id = source.publish(listed(&[record])).await.unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while receiver
        .inner
        .net
        .get()
        .is_none_or(|n| n.status().batches_received != 1)
        || budget.status().reserved_bytes == 0
        || crate::news::NewsStore::open(&receiver.inner.paths.news).headline_count() != 1
    {
        assert!(
            Instant::now() < deadline,
            "receiver did not retain the failed inbox delivery: {:?}, {:?}",
            receiver.inner.net.get().map(|n| n.status()),
            budget.status()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(receiver.inner.inbox_records.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&receiver.inner.paths.records).unwrap(), raw);
    let batches = dir.path().join("net/batches");
    let signed = std::fs::read(batches.join(format!("{id}.json"))).unwrap();
    let marker = batches.join(format!("{id}.delivered-trusted"));
    assert!(!marker.exists());
    receiver.shutdown().await.unwrap();
    source.shutdown().await;
    let credit_dir = dir.path().join("net/credits");
    let credited = plumb_net::credits::Ledger::open(&credit_dir)
        .unwrap()
        .account(&source_id);
    assert_eq!(credited.confirmed, 1);
    assert!(credited.earned > 0);
    std::fs::remove_dir(&inbox_path).unwrap();
    std::fs::create_dir(&marker).unwrap(); // Inbox can persist; marker I/O still fails.
    config.network.as_mut().unwrap().bootstrap.clear(); // Source is offline: replay is local.
    let receiver = start(config.clone()).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while receiver.inner.inbox_records.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < deadline,
            "restart did not persist its pending inbox"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(marker.is_dir());
    {
        let budget = receiver.inner.storage.as_ref().unwrap();
        let _mutation = budget.mutation();
        let bytes = plumb_core::storage::file_bytes(&marker).unwrap();
        std::fs::remove_dir(&marker).unwrap();
        budget.removed(bytes);
    }
    while !marker.is_file() {
        assert!(
            Instant::now() < deadline,
            "restart did not persist and acknowledge the held batch"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let inbox: Vec<SiteRecord> = plumb_core::stored::read_file(&inbox_path).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].domain, "restart-inbox.example");
    assert_eq!(
        crate::news::NewsStore::open(&receiver.inner.paths.news).headline_count(),
        1
    );
    assert_eq!(receiver.inner.inbox_records.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read(batches.join(format!("{id}.json"))).unwrap(),
        signed
    );
    receiver.shutdown().await.unwrap();
    let after = plumb_net::credits::Ledger::open(&credit_dir)
        .unwrap()
        .account(&source_id);
    assert_eq!(
        (after.confirmed, after.earned),
        (credited.confirmed, credited.earned)
    );
    // A second restart respects the durable acknowledgment; old-format signed
    // bytes remain readable and the inbox is not delivered a second time.
    let receiver = start(config).await.unwrap();
    wait_for(receiver.addr(), "restarted index", ready_and_idle).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(receiver.inner.inbox_records.load(Ordering::SeqCst), 0);
    assert_eq!(receiver.inner.news.headline_count(), 1);
    assert_eq!(
        crate::news::NewsStore::open(&receiver.inner.paths.news).headline_count(),
        1
    );
    assert_eq!(
        std::fs::read(batches.join(format!("{id}.json"))).unwrap(),
        signed
    );
    receiver.shutdown().await.unwrap();
    let after = plumb_net::credits::Ledger::open(&credit_dir)
        .unwrap()
        .account(&source_id);
    assert_eq!(
        (after.confirmed, after.earned),
        (credited.confirmed, credited.earned)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headline_write_failure_keeps_news_only_and_mixed_deliveries_for_restart() {
    for with_raw in [false, true] {
        let source_dir = tempfile::tempdir().unwrap();
        let mut source_config = plumb_net::NetConfig::new(source_dir.path().to_owned());
        source_config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
        source_config.upnp = false;
        source_config.local_discovery = false;
        source_config.round_every = None;
        let table = plumb_net::BucketTable::build::<plumb_core::site::Item>(
            &source_dir.path().join("buckets"),
            &[],
        )
        .unwrap();
        let (source, _source_records) = plumb_net::start(source_config, Arc::new(table))
            .await
            .unwrap();
        let source_id = source.peer_id();
        let source_addr: plumb_net::Multiaddr = loop {
            if let Some(addr) = source.status().listening.first() {
                break addr.parse().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let dir = seeded_dir();
        let mut config = test_config(dir.path());
        config.take_new_sites = true;
        let mut net = plumb_net::NetConfig::new(PathBuf::new());
        net.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
        net.upnp = false;
        net.local_discovery = false;
        net.round_every = None;
        net.fill = false;
        net.trusted_peers = vec![source_id];
        net.bootstrap = vec![source_addr.with_p2p(source_id).unwrap()];
        config.network = Some(net);
        let receiver = start(config.clone()).await.unwrap();
        wait_for(receiver.addr(), "initial index", ready_and_idle).await;
        let raw = std::fs::read(&receiver.inner.paths.records).unwrap();
        let news_dir = receiver.inner.paths.news.clone();
        let budget = receiver.inner.storage.as_ref().unwrap();
        plumb_core::storage::create_directory(&news_dir, Some(budget)).unwrap();
        let blocked = news_dir.join("headlines.json");
        plumb_core::storage::create_directory(&blocked, Some(budget)).unwrap();
        let mut record = SiteRecord::new("headline-persist.example");
        record.news = vec![plumb_core::Headline {
            title: "News survives a failed durable write".into(),
            url: "https://headline-persist.example/story".into(),
            at: now_unix(),
        }];
        if with_raw {
            record.url = Some("https://headline-persist.example/".into());
            record.title = Some("A raw crawl with its headline".into());
            record.crawled_at = Some(now_unix());
        }
        let id = source.publish(listed(&[record])).await.unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while receiver
            .inner
            .net
            .get()
            .is_none_or(|n| n.status().batches_received != 1)
            || receiver.inner.news.headline_count() != 1
        {
            assert!(
                Instant::now() < deadline,
                "headline delivery did not reach the receiver"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(receiver.inner.news.save().is_err());
        tokio::time::sleep(Duration::from_millis(300)).await;
        let batches = dir.path().join("net/batches");
        let marker = batches.join(format!("{id}.delivered-trusted"));
        let signed = std::fs::read(batches.join(format!("{id}.json"))).unwrap();
        assert!(!marker.exists(), "failed headline save was acknowledged");
        assert_eq!(crate::news::NewsStore::open(&news_dir).headline_count(), 0);
        assert_eq!(receiver.inner.inbox_records.load(Ordering::SeqCst), 0);
        assert!(!receiver.inner.paths.inbox.exists());
        assert_eq!(std::fs::read(&receiver.inner.paths.records).unwrap(), raw);
        receiver.shutdown().await.unwrap();
        source.shutdown().await;
        let credits = dir.path().join("net/credits");
        let before = plumb_net::credits::Ledger::open(&credits)
            .unwrap()
            .account(&source_id);
        assert_eq!(before.confirmed, u64::from(with_raw));
        assert_eq!(crate::news::NewsStore::open(&news_dir).headline_count(), 0);
        std::fs::remove_dir(&blocked).unwrap();
        config.network.as_mut().unwrap().bootstrap.clear();
        let receiver = start(config.clone()).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !marker.is_file() {
            assert!(
                Instant::now() < deadline,
                "restart did not persist both destinations before acknowledging"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(crate::news::NewsStore::open(&news_dir).headline_count(), 1);
        let inbox: Vec<SiteRecord> =
            plumb_core::stored::read_file(&receiver.inner.paths.inbox).unwrap_or_default();
        let held = crate::records::load_sites(&receiver.inner.paths.records).unwrap();
        let durable_raw = inbox
            .iter()
            .filter(|r| r.domain == "headline-persist.example")
            .count()
            + usize::from(held.contains("headline-persist.example"));
        assert_eq!(durable_raw, usize::from(with_raw));
        assert_eq!(
            std::fs::read(batches.join(format!("{id}.json"))).unwrap(),
            signed
        );
        receiver.shutdown().await.unwrap();
        let after = plumb_net::credits::Ledger::open(&credits)
            .unwrap()
            .account(&source_id);
        assert_eq!(
            (after.confirmed, after.earned),
            (before.confirmed, before.earned)
        );
        let receiver = start(config).await.unwrap();
        wait_for(receiver.addr(), "acknowledged restart", ready_and_idle).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(receiver.inner.news.headline_count(), 1);
        assert_eq!(crate::news::NewsStore::open(&news_dir).headline_count(), 1);
        assert_eq!(receiver.inner.inbox_records.load(Ordering::SeqCst), 0);
        receiver.shutdown().await.unwrap();
        let after = plumb_net::credits::Ledger::open(&credits)
            .unwrap()
            .account(&source_id);
        assert_eq!(
            (after.confirmed, after.earned),
            (before.confirmed, before.earned)
        );
    }
}

/// New topics under a storage limit drop the sites kept for the old ones
/// from the records, read a record at a time, and the index is built
/// without them; a file that holds a site twice is read whole instead,
/// and written again for the index to be built from it the same way.
#[test]
fn new_topics_make_room_in_the_records_file() {
    let site = |domain: &str, tranco: u32, title: &str| {
        let mut r = SiteRecord::new(domain);
        r.signals.tranco_rank = Some(tranco);
        r.title = Some(title.into());
        r
    };
    for twice in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        let mut records = vec![
            site("google.com", 1, "Google"),
            site("youtube.com", 2, "YouTube"),
            site("chess.com", 5_000, "Play Chess Games"),
            site("allrecipes.com", 6_000, "Recipes for cooking"),
        ];
        if twice {
            records.push(site("chess.com", 5_000, "Chess"));
        }
        write_jsonl(&path, &records).unwrap();
        crate::records::RecordStore::open(&path)
            .save(&[crate::records::Change::Merge {
                site: site("seriouseats.com", 7_000, "Serious Eats cooking").stored_item_as_is(),
            }])
            .unwrap();
        assert_eq!(
            crate::outline::outline_copy(&path, &dir.path().join("copy"))
                .unwrap()
                .is_none(),
            twice
        );
        let net = dir.path().join("net");
        std::fs::create_dir_all(&net).unwrap();
        std::fs::write(net.join("fill.json"), r#"{"focus_base":2,"prune":true}"#).unwrap();
        let mut config = test_config(dir.path());
        config.settings.storage_limit_mb = 500;
        config.focus_topics = vec!["cooking".to_owned()];
        let rank = RankConfig::default();
        let opened = open_data_dir(&config, rank).unwrap();
        let (_stop, stopped) = tokio::sync::watch::channel(false);
        let inner = Inner::new(config, rank, opened, stopped, None);

        let built = crate::node::worker::make_room_for_new_topics(&inner).unwrap();
        assert_eq!(built.docs, 4, "twice: {twice}");
        let mut left: Vec<String> = plumb_core::stored::read_file(&path)
            .unwrap()
            .into_iter()
            .map(|r: SiteRecord| r.domain)
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "allrecipes.com",
                "google.com",
                "seriouseats.com",
                "youtube.com"
            ]
        );
        assert!(!crate::records::journal_path(&path).exists());
        let state = inner.fill_state();
        assert!(!state.prune);
        assert_eq!(
            state.detail,
            "Made room for the new topics: dropped 1 sites"
        );
        assert!(!crate::node::fill::FillState::load(&net).prune);
    }
}
