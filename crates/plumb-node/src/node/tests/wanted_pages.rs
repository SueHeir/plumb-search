//! Pages a node goes looking for because its own searches wanted them.

use axum::response::Html;

use super::*;

/// A health site whose homepage's menu links only to its topics, which
/// list its fact sheets; the paths asked for are noted.
async fn health_site(requested: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let page = |html: &'static str| move || async move { Html(html) };
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::get(page(
                r#"<title>Health Example</title><nav><a href="/topics">Health topics</a>
                <a href="/about">About us</a></nav>"#,
            )),
        )
        .route(
            "/topics",
            axum::routing::get(page(r#"<a href="/sheets/dengue">Dengue</a>"#)),
        )
        .route(
            "/sheets/dengue",
            axum::routing::get(page("<title>Dengue fact sheet</title>")),
        )
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let requested = Arc::clone(&requested);
                async move {
                    requested
                        .lock()
                        .unwrap()
                        .push(request.uri().path().to_string());
                    next.run(request).await
                }
            },
        ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_search_sends_the_node_through_a_site_s_links_and_shows_what_it_found() {
    let requested = Arc::new(Mutex::new(Vec::new()));
    let site = health_site(Arc::clone(&requested)).await;
    let dir = tempfile::tempdir().unwrap();
    let mut record = site::new("healthexample.org");
    record.url = Some("http://healthexample.org/".into());
    record.title = Some("Health Example".into());
    write_old_lines(&dir.path().join("records.jsonl"), &[record]);
    // A page found before: the test site answers only plain http, and
    // only https pages are kept.
    let malaria = "https://healthexample.org/sheets/malaria";
    let found = serde_json::json!({
        "pages": {malaria: {"url": malaria, "title": "Malaria", "depth": 2, "at": now_unix()}},
        "index": 0,
    });
    std::fs::create_dir_all(dir.path().join("wanted")).unwrap();
    std::fs::write(dir.path().join("wanted/pages.json"), found.to_string()).unwrap();
    // Pages found are shown next to the page sets' only.
    let lighthouse = plumb_core::Article {
        title: "Lighthouse".into(),
        views: 900,
        ..Default::default()
    };
    super::agents::write_set_file(dir.path(), "wikipedia-en", &[lighthouse]);
    let mut config = test_config(dir.path());
    config.settings.page_sets = crate::pages::PageSets::parse("wikipedia-en=all").unwrap();
    config.wanted_pages = true;
    config
        .page_reader
        .resolve
        .push(("healthexample.org".into(), site));
    let node = start(config).await.unwrap();
    let addr = node.addr();
    wait_for(addr, "the first index", ready_and_idle).await;

    // The index of the pages found is built as the node starts.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, _, body) = get(addr, "/search?q=health+example+malaria").await;
        if body.contains("https://healthexample.org/sheets/malaria") {
            break;
        }
        assert!(Instant::now() < deadline, "{body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The site shows no page on dengue, so the node goes looking for one.
    get(addr, "/search?q=health+example+dengue").await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !requested
        .lock()
        .unwrap()
        .iter()
        .any(|path| path == "/sheets/dengue")
    {
        assert!(Instant::now() < deadline, "{:?}", requested.lock().unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!requested
        .lock()
        .unwrap()
        .iter()
        .any(|path| path == "/about"));
    node.shutdown().await.unwrap();

    // Off, the node neither shows the pages found nor looks for more.
    let mut config = test_config(dir.path());
    config.settings.page_sets = crate::pages::PageSets::parse("wikipedia-en=all").unwrap();
    config.wanted_pages = false;
    let node = start(config).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !get(node.addr(), "/search?q=lighthouse")
        .await
        .2
        .contains("wiki/Lighthouse")
    {
        assert!(Instant::now() < deadline, "no page sets");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (_, _, body) = get(node.addr(), "/search?q=health+example+malaria").await;
    assert!(!body.contains("sheets/malaria"), "{body}");
    node.shutdown().await.unwrap();
}
