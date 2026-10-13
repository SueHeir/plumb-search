//! What the owner decides: background updates, download limits, pauses,
//! the activity log and backups.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_updates_wait_while_off_and_resume_from_the_panel() {
    let dir = recently_crawled_dir();
    let paths = store::Paths::new(dir.path());
    store::save_state(
        &paths,
        &SavedState {
            crawl_left: 700,
            index_stale: true,
            last_refresh: None,
            wikidata_missing: false,
            quick_start: false,
            ..SavedState::default()
        },
    )
    .unwrap();
    std::fs::write(&paths.settings, "{\"background_updates\": false}").unwrap();
    let mut config = test_config(dir.path());
    config.refresh_every = Some(Duration::from_secs(3600));
    let node = start(config).await.unwrap();
    let addr = node.addr();

    let status = wait_for(addr, "the first index", ready_and_idle).await;
    assert_eq!(status.detail, "Background updates are off");
    assert!(!status.background_updates);
    assert_eq!((status.crawl_left, status.last_refresh), (700, None));
    let (code, _, body) = get(addr, "/app").await;
    assert_eq!(code, 200);
    assert!(
        body.contains("<p>Background updates are off.</p>"),
        "{body}"
    );
    assert_eq!(status.paused.as_deref(), Some("Background updates are off"));

    // Ticking the box on the panel, from this computer, starts the round.
    let form = "background_updates=1";
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "POST /app/settings HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{form}",
        form.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 303"), "{response}");

    let status = wait_for(addr, "the end of the round", |s| {
        ready_and_idle(s) && s.last_refresh.is_some()
    })
    .await;
    assert!(status.background_updates);
    assert_eq!(status.crawl_left, 0);
    node.shutdown().await.unwrap();
    assert_eq!(store::load_settings(&paths), Some(NodeSettings::default()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crawling_waits_for_the_next_day_once_the_download_limit_is_reached() {
    let dir = recently_crawled_dir();
    let paths = store::Paths::new(dir.path());
    let mut state = SavedState {
        crawl_left: 700,
        index_stale: true,
        ..SavedState::default()
    };
    state.add_downloaded(3 * MB, now_unix());
    store::save_state(&paths, &state).unwrap();
    let mut config = test_config(dir.path());
    config.settings.download_limit_mb_per_day = 2;
    let node = start(config).await.unwrap();

    let status = wait_for(node.addr(), "the first index", ready_and_idle).await;
    assert_eq!(
        status.paused.as_deref(),
        Some("Paused until tomorrow: today's download limit is reached")
    );
    assert_eq!(status.detail, status.paused.clone().unwrap());
    assert_eq!((status.crawl_left, status.downloaded_today), (700, 3 * MB));
    assert!(status.disk_used > 0);
    // Page set, vector and model downloads wait too.
    assert!(node.inner.download_pause().is_some());
    // The storage limit pauses crawls, not those downloads: they keep to it
    // by what they take.
    let mut settings = node.inner.settings();
    settings.download_limit_mb_per_day = 0;
    settings.storage_limit_mb = 1;
    node.inner.change_settings(settings).unwrap();
    std::fs::write(dir.path().join("filler"), vec![0u8; 2 * MB as usize]).unwrap();
    // Direct owner writes are reconciled explicitly at blocking checkpoints.
    node.inner
        .storage
        .as_ref()
        .unwrap()
        .recount(dir.path())
        .unwrap();
    node.inner.recount_disk();
    assert!(node.inner.pause().is_some());
    assert!(node.inner.download_pause().is_none());
    // The status says so from that count, without counting again.
    assert_eq!(
        node.inner.status().paused.as_deref(),
        Some("Paused: the storage limit is reached")
    );
    node.shutdown().await.unwrap();
}

#[test]
fn downloads_are_counted_per_day() {
    let day = 24 * 60 * 60;
    let mut state = SavedState::default();
    state.add_downloaded(5, 10 * day + 1);
    state.add_downloaded(7, 10 * day + 2);
    assert_eq!(state.downloaded_today(10 * day + 3), 12);
    assert_eq!(state.downloaded_today(11 * day), 0);
    state.add_downloaded(1, 11 * day + 5);
    assert_eq!(state.downloaded_today(11 * day + 6), 1);
    assert_eq!(state.downloaded_total, 13);
    assert_eq!(store::next_day(10 * day + 7), 11 * day);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_for_an_hour_holds_the_crawl_and_resume_lifts_it() {
    let dir = recently_crawled_dir();
    let paths = store::Paths::new(dir.path());
    store::save_state(
        &paths,
        &SavedState {
            crawl_left: 700,
            index_stale: true,
            ..SavedState::default()
        },
    )
    .unwrap();
    let until = now_unix() + 3600;
    let mut config = test_config(dir.path());
    config.settings.paused_until = Some(until);
    let node = start(config).await.unwrap();
    let status = wait_for(node.addr(), "the first index", ready_and_idle).await;
    assert_eq!(status.paused.as_deref(), Some("Paused by you"));
    assert_eq!(status.paused_until, Some(until));
    assert_eq!(status.crawl_left, 700);
    // Nobody listens for a restart yet, so the panel offers none.
    assert!(!status.can_restart);
    let signal = node.restart_signal();
    assert!(node.status().can_restart);
    let restart = tokio::spawn(async move { signal.requested().await });
    StatusSource::restart(node.inner.as_ref()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), restart)
        .await
        .expect("the restart request arrives")
        .unwrap();

    node.inner
        .change_settings(NodeSettings {
            paused_until: None,
            ..node.inner.settings()
        })
        .unwrap();
    let status = wait_for(node.addr(), "the end of the round", |s| {
        ready_and_idle(s) && s.crawl_left == 0
    })
    .await;
    assert_eq!(status.paused, None);
    node.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_activity_log_and_backups_cover_a_node_s_life() {
    let dir = recently_crawled_dir();
    let mut config = test_config(dir.path());
    config.refresh_every = None;
    let node = start(config.clone()).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;
    let inner = node.inner.as_ref();
    inner
        .change_settings(NodeSettings {
            workload: Workload::Light,
            ..inner.settings()
        })
        .unwrap();
    let log = StatusSource::activity_log(inner);
    assert!(
        log.last().unwrap().message.starts_with("Plumb Search "),
        "{log:?}"
    );
    assert_eq!(log[0].message, "Settings changed: workload light");

    // A backup, then a change, then the backup restored: the change is undone.
    std::fs::create_dir_all(dir.path().join("net")).unwrap();
    std::fs::write(dir.path().join("net/node.key"), b"key one").unwrap();
    let made = StatusSource::make_backup(inner).unwrap();
    inner
        .change_settings(NodeSettings {
            workload: Workload::Full,
            ..inner.settings()
        })
        .unwrap();
    std::fs::write(dir.path().join("net/node.key"), b"key two").unwrap();
    let saved = backup::path_of(dir.path(), &made.name).unwrap();
    let restored = backup::Backup::parse(&std::fs::read(saved).unwrap()).unwrap();
    StatusSource::restore_backup(inner, &restored).unwrap();
    assert_eq!(inner.settings().workload, Workload::Light);
    assert_eq!(
        std::fs::read(dir.path().join("net/node.key")).unwrap(),
        b"key one"
    );
    // What it replaced was backed up first.
    let backups = backup::list(dir.path());
    assert_eq!(backups.len(), 2);
    assert!(backups.iter().any(|b| b.name.contains("before-restore")));
    let (code, _, body) = get(addr, "/app?section=backup").await;
    assert_eq!(code, 200);
    assert!(body.contains(&made.name), "{body}");
    let (code, head, body) = get(addr, &format!("/app/backups/{}", made.name)).await;
    assert_eq!(code, 200);
    assert!(head.contains("content-disposition: attachment"), "{head}");
    assert!(body.contains("plumb-backup/1"));
    let (code, _, _) = get(addr, "/app/backups/..%2Fsettings.json").await;
    assert_eq!(code, 404);
    let (code, _, body) = get(addr, "/app?section=activity").await;
    assert_eq!(code, 200);
    assert!(body.contains("restored"), "{body}");

    node.shutdown().await.unwrap();
    // The log outlives the node.
    let node = start(config).await.unwrap();
    let log = StatusSource::activity_log(node.inner.as_ref());
    assert!(log.iter().any(|e| e.message == "Stopped"));
    assert!(log.iter().any(|e| e.message.starts_with("Backup made")));
    node.shutdown().await.unwrap();
}
