//! The panel's tests.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::Router;
use tower::ServiceExt;

use super::view::bytes_words;
use super::*;
use crate::node::backup::Backup;
use crate::node::features::FeatureSettings;
use crate::node::{
    CrawlHours, LastError, LogEntry, LogLevel, NodeSettings, Phase, Progress, Retry, Step, Workload,
};
use crate::web::{node_router, StatusSource};

struct FakeNode {
    status: Status,
    settings: Mutex<NodeSettings>,
    refreshes: Mutex<usize>,
    features: Mutex<FeatureSettings>,
}

impl StatusSource for FakeNode {
    fn status(&self) -> Status {
        self.status.clone()
    }
    fn settings(&self) -> Option<NodeSettings> {
        Some(self.settings.lock().unwrap().clone())
    }
    fn change_settings(&self, settings: NodeSettings) -> anyhow::Result<()> {
        *self.settings.lock().unwrap() = settings;
        Ok(())
    }
    fn refresh_now(&self) {
        *self.refreshes.lock().unwrap() += 1;
    }
    fn saved_features(&self) -> anyhow::Result<FeatureSettings> {
        Ok(self.features.lock().unwrap().clone())
    }
    fn change_features(&self, features: FeatureSettings) -> anyhow::Result<()> {
        *self.features.lock().unwrap() = features;
        Ok(())
    }
    fn data_dir(&self) -> Option<std::path::PathBuf> {
        Some("/home/me/plumb <data>".into())
    }
    fn activity_log(&self) -> Vec<LogEntry> {
        vec![LogEntry {
            at: now_unix() - 120,
            level: LogLevel::Error,
            message: "Could not reach <tranco-list.eu>".into(),
        }]
    }
    fn retry(&self, what: Retry) -> anyhow::Result<()> {
        match what {
            Retry::Work => Ok(()),
            _ => anyhow::bail!("Search by meaning is off on this node."),
        }
    }
}

fn status(phase: Phase, step: Step) -> Status {
    Status {
        phase,
        step,
        detail: "Downloading the Tranco list of popular sites".into(),
        progress: None,
        last_error: None,
        wikidata_missing: false,
        wikidata_error: None,
        sites: 0,
        places: 0,
        index: None,
        last_refresh: None,
        next_refresh: None,
        version: "0.1.0".into(),
        crawl_left: 0,
        background_updates: true,
        paused: None,
        disk_used: 0,
        storage_limit: 0,
        downloaded_today: 0,
        downloaded_total: 0,
        homepages_visited: 0,
        meaning_sites: None,
        meaning_work: None,
        can_restart: false,
        page_coverage: None,
        paused_until: None,
        network: None,
        fill: None,
    }
}

struct NoSearch;
impl crate::web::SearchBackend for NoSearch {
    fn search(&self, _: &str, _: usize) -> anyhow::Result<Vec<plumb_index::Hit>> {
        Ok(Vec::new())
    }
    fn num_docs(&self) -> u64 {
        0
    }
}

fn app(status: Status) -> (Router, Arc<FakeNode>) {
    let node = Arc::new(FakeNode {
        status,
        settings: Mutex::new(NodeSettings::default()),
        refreshes: Mutex::new(0),
        features: Mutex::new(FeatureSettings::default()),
    });
    (node_router(Arc::new(NoSearch), node.clone()), node)
}

async fn get_panel(app: Router) -> String {
    let mut body = String::new();
    for section in ["overview", "search", "resources", "network", "about"] {
        body.push_str(&get_section(app.clone(), section).await);
    }
    body
}

async fn get_section(app: Router, section: &str) -> String {
    let mut request = Request::get(format!("/app?section={section}"))
        .header(header::HOST, "127.0.0.1:7586")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:50000".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // So that its forms send their origin: no-referrer sends "null".
    assert_eq!(response.headers()[header::REFERRER_POLICY], "same-origin");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    let no_referrer = body.find("content=\"no-referrer\"").unwrap();
    let same_origin = body
        .find("<meta name=\"referrer\" content=\"same-origin\">")
        .unwrap();
    assert!(same_origin > no_referrer, "the later one wins: {body}");
    body
}

async fn body_text(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// A form post to `path` from `peer`, with an `Origin` header if given.
async fn post(app: Router, path: &str, body: &str, peer: &str, origin: Option<&str>) -> Response {
    post_to_host(app, path, body, peer, "127.0.0.1:7586", origin).await
}

/// [`post`], with the given `Host` header.
async fn post_to_host(
    app: Router,
    path: &str,
    body: &str,
    peer: &str,
    host: &str,
    origin: Option<&str>,
) -> Response {
    let mut request = Request::post(path)
        .header(header::HOST, host)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    let mut request = request.body(Body::from(body.to_string())).unwrap();
    let peer: SocketAddr = peer.parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    app.oneshot(request).await.unwrap()
}

#[tokio::test]
async fn says_what_works_while_setting_up() {
    let mut setting_up = status(Phase::SettingUp, Step::Idle);
    setting_up.progress = Some(Progress {
        done: 0,
        total: 1,
        unit: "files".into(),
    });
    let body = get_panel(app(setting_up).0).await;
    assert!(
        body.contains("<p class=\"big\">Setting up search</p>"),
        "{body}"
    );
    assert!(body.contains("Downloading the Tranco list"), "{body}");
    assert!(
        body.contains("<progress aria-label=\"Setup progress\" value=\"0\" max=\"1\">"),
        "{body}"
    );
    assert!(body.contains("<h2>Setup</h2>"), "{body}");
    assert!(
        !body.contains("http-equiv=\"refresh\""),
        "refresh is opt-in: {body}"
    );
    assert!(!body.contains("Search in your browser"), "{body}");
    assert!(!body.contains("Update now"), "{body}");
    assert!(body.contains("/home/me/plumb &lt;data&gt;"), "{body}");
}

#[tokio::test]
async fn shows_the_node_at_a_glance() {
    let mut limited = status(Phase::Ready, Step::Idle);
    limited.sites = 250_000;
    limited.wikidata_missing = true;
    limited.detail = "Asking Wikidata for official websites".into();
    limited.disk_used = 312_400_000;
    limited.downloaded_today = 48_000_000;
    limited.downloaded_total = 1_400_000_000;
    limited.homepages_visited = 3_456;
    limited.crawl_left = 2_000;
    limited.fill = Some(crate::node::FillStatus {
        filled: 41_000,
        position: 60_000,
        total: 1_190_000,
        peer: None,
        detail: "Taking in crawled sites from a trusted node".into(),
        ..Default::default()
    });
    let (router, node) = app(limited.clone());
    *node.settings.lock().unwrap() = NodeSettings::desktop();
    let body = get_panel(router).await;
    assert!(
        body.contains("<p class=\"big\">Limited search is ready</p>"),
        "{body}"
    );
    assert!(body.contains("the 250,000 most popular sites"), "{body}");
    assert!(
        body.contains(
            "Filled with the network's crawls: 41,000 sites. Taking in crawled sites from a trusted node."
        ),
        "{body}"
    );
    assert!(
        body.contains("href=\"http://127.0.0.1:7586/\">Open search"),
        "{body}"
    );
    assert!(
        body.contains("<code>http://127.0.0.1:7586/search?q=%s</code>"),
        "{body}"
    );
    assert!(
        body.contains("<small>Asking Wikidata for official websites</small>"),
        "{body}"
    );
    // Storage and downloads, against the desktop's limits.
    assert!(body.contains("<p class=\"big\">312 MB</p>"), "{body}");
    assert!(
        body.contains(
            "<progress aria-label=\"Storage used\" value=\"312400000\" max=\"2000000000\">"
        ),
        "{body}"
    );
    assert!(body.contains("<p>of 2.0 GB allowed.</p>"), "{body}");
    assert!(body.contains("<p class=\"big\">48 MB</p>"), "{body}");
    assert!(body.contains("<p>of 500 MB a day.</p>"), "{body}");
    assert!(body.contains("1.4 GB since setup."), "{body}");
    assert!(
        body.contains("2,000 homepages to visit once setup is done."),
        "{body}"
    );
    assert!(
        body.contains("3,456 homepages visited since setup."),
        "{body}"
    );
    assert!(body.contains("<p class=\"big\">Off</p>"), "{body}");
    assert!(
        body.contains("name=\"download_limit_mb_per_day\" min=\"0\" step=\"1\" value=\"500\""),
        "{body}"
    );
    assert!(
        body.contains("name=\"storage_limit_mb\" min=\"0\" step=\"1\" value=\"2000\""),
        "{body}"
    );

    limited.step = Step::Idle;
    limited.wikidata_error = Some(LastError {
        message: "HTTP 429".into(),
        at: now_unix(),
        // A little past 10 minutes, so a second ticking over before the
        // page renders still reads "10 minutes".
        retry_at: Some(now_unix() + 630),
    });
    let body = get_panel(app(limited).0).await;
    assert!(body.contains("could not download Wikidata"), "{body}");
    assert!(
        body.contains("Plumb will try again in 10 minutes"),
        "{body}"
    );
    assert!(body.contains("No storage limit."), "{body}");

    let mut ready = status(Phase::Ready, Step::Idle);
    ready.sites = 260_123;
    ready.page_coverage = Some(crate::node::PageCoverage {
        indexed_pages: 1_000_000,
        index_generation: None,
        sets: Vec::new(),
    });
    ready.last_refresh = Some(now_unix() - 3600);
    // Past 2 hours by a margin, so a tick before rendering can't make it "1 hour".
    ready.next_refresh = Some(now_unix() + 7230);
    let body = get_panel(app(ready.clone()).0).await;
    assert!(
        body.contains("<p class=\"big\">Search is ready</p>"),
        "{body}"
    );
    assert!(body.contains("1,260,123 items indexed"), "{body}");
    assert!(body.contains("Next round in 2 hours"), "{body}");
    assert!(!body.contains("<h2>Setup</h2>"), "setup is done: {body}");
    assert!(body.contains("Update now"), "{body}");
    assert!(
        !body.contains("http-equiv=\"refresh\""),
        "refresh is opt-in: {body}"
    );

    // A round under way leaves nothing to start.
    let mut crawling = ready.clone();
    crawling.step = Step::Crawling;
    let body = get_panel(app(crawling).0).await;
    assert!(body.contains("Visiting homepages"), "{body}");
    assert!(!body.contains("Update now"), "{body}");

    ready.paused = Some("Paused until tomorrow: today's download limit is reached".into());
    let body = get_panel(app(ready).0).await;
    assert!(body.contains("<p class=\"big\">Paused</p>"), "{body}");
    assert!(
        body.contains("today&#39;s download limit is reached."),
        "{body}"
    );
    assert!(!body.contains("Update now"), "{body}");
}

#[test]
fn counts_items_and_says_what_they_are() {
    let mut ready = status(Phase::Ready, Step::Idle);
    assert_eq!(items_in_words(&ready), "0");
    ready.sites = 260_123;
    assert_eq!(items_in_words(&ready), "260,123");
    ready.page_coverage = Some(crate::node::PageCoverage {
        indexed_pages: 1_000_000,
        index_generation: None,
        sets: Vec::new(),
    });
    assert_eq!(
        items_in_words(&ready),
        "1,260,123: 260,123 sites and 1,000,000 pages"
    );
    ready.places = 5_000;
    assert_eq!(
        items_in_words(&ready),
        "1,265,123: 260,123 sites, 1,000,000 pages and 5,000 places"
    );
}

#[test]
fn reads_limits_and_words_bytes() {
    assert_eq!(parse_limit(""), Some(0));
    assert_eq!(parse_limit(" 1,500 "), Some(1_500));
    assert_eq!(parse_limit("0"), Some(0));
    assert_eq!(parse_limit("-5"), None);
    assert_eq!(parse_limit("2.5"), None);
    assert_eq!(bytes_words(0), "0 KB");
    assert_eq!(bytes_words(1_200), "2 KB");
    assert_eq!(bytes_words(312_400_000), "312 MB");
    assert_eq!(bytes_words(1_450_000_000), "1.4 GB");
    assert_eq!(bytes_words(150_000_000_000), "150 GB");
}

#[tokio::test]
async fn saves_settings_from_this_computer_only() {
    let (router, node) = app(status(Phase::Ready, Step::Idle));
    let body = get_panel(router.clone()).await;
    assert!(
        body.contains("name=\"background_updates\" value=\"1\" checked"),
        "{body}"
    );

    // An unticked box is left out of the form; empty limits are none.
    let form = "download_limit_mb_per_day=250&storage_limit_mb=";
    let response = post(
        router.clone(),
        "/app/settings",
        form,
        "127.0.0.1:50000",
        Some("http://127.0.0.1:7586"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        "/app?section=resources&saved=settings"
    );
    assert_eq!(
        *node.settings.lock().unwrap(),
        NodeSettings {
            background_updates: false,
            download_limit_mb_per_day: 250,
            storage_limit_mb: 0,
            ..NodeSettings::default()
        }
    );

    // Focus topics are kept as typed, once each; forms without them
    // keep them.
    post(
        router.clone(),
        "/app/settings",
        "focus_shown=1&focus_topics=games%0D%0ARust+programming,games",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(
        node.settings.lock().unwrap().focus_topics,
        ["games", "Rust programming"]
    );
    post(
        router.clone(),
        "/app/settings",
        "storage_limit_mb=",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(node.settings.lock().unwrap().focus_topics.len(), 2);
    node.settings.lock().unwrap().focus_topics.clear();

    // A limit that is not a number changes nothing.
    let response = post(
        router.clone(),
        "/app/settings",
        "background_updates=1&storage_limit_mb=lots",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // The window has no back button: every error page leads back.
    let body = body_text(response).await;
    assert!(body.contains("Nothing was changed."), "{body}");
    assert!(
        body.contains("href=\"/app?section=resources\">Back to the panel</a>"),
        "{body}"
    );
    assert!(!node.settings.lock().unwrap().background_updates);

    // Without an Origin header (some browsers leave it out) is fine too.
    let response = post(
        router.clone(),
        "/app/settings",
        "background_updates=1",
        "[::1]:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(node.settings.lock().unwrap().background_updates);
    // A form without the fill box keeps the fill setting.
    assert!(node.settings.lock().unwrap().fill_from_network);
    let panel = get_panel(router.clone()).await;
    assert!(
        panel.contains("name=\"fill_from_network\" value=\"1\" checked"),
        "{panel}"
    );
    // The panel's form shows it; unticked turns filling off.
    let response = post(
        router.clone(),
        "/app/settings",
        "background_updates=1&fill_shown=1",
        "[::1]:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(!node.settings.lock().unwrap().fill_from_network);
    let response = post(
        router.clone(),
        "/app/settings",
        "background_updates=1&fill_shown=1&fill_from_network=1",
        "[::1]:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(node.settings.lock().unwrap().fill_from_network);

    // Another machine, or a page of another site, may not.
    for (peer, origin) in [
        ("192.168.1.20:50000", Some("http://127.0.0.1:7586")),
        ("127.0.0.1:50000", Some("https://evil.example")),
        ("127.0.0.1:50000", Some("null")),
    ] {
        let response = post(router.clone(), "/app/settings", "", peer, origin).await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{peer} {origin:?}"
        );
        let response = post(router.clone(), "/app/refresh", "", peer, origin).await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{peer} {origin:?}"
        );
    }
    assert!(node.settings.lock().unwrap().background_updates);
    assert_eq!(*node.refreshes.lock().unwrap(), 0);

    let response = post(
        router,
        "/app/refresh",
        "",
        "127.0.0.1:50000",
        Some("http://127.0.0.1:7586"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(*node.refreshes.lock().unwrap(), 1);
}

#[tokio::test]
async fn refuses_changes_from_a_rebound_dns_name() {
    let (router, node) = app(status(Phase::Ready, Step::Idle));
    // attacker.example now resolves to 127.0.0.1: the browser connects
    // from loopback and calls the request same-origin.
    for path in ["/app/settings", "/app/refresh"] {
        let response = post_to_host(
            router.clone(),
            path,
            "storage_limit_mb=",
            "127.0.0.1:50000",
            "attacker.example:7586",
            Some("http://attacker.example:7586"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    assert!(node.settings.lock().unwrap().background_updates);
    assert_eq!(*node.refreshes.lock().unwrap(), 0);

    // Local names still work.
    for (host, origin) in [
        ("localhost:7586", "http://localhost:7586"),
        ("[::1]:7586", "http://[::1]:7586"),
    ] {
        let response = post_to_host(
            router.clone(),
            "/app/refresh",
            "",
            "127.0.0.1:50000",
            host,
            Some(origin),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{host}");
    }
}

#[tokio::test]
async fn accepts_ipv4_peers_on_a_dual_stack_listener() {
    let (router, node) = app(status(Phase::Ready, Step::Idle));
    let response = post(
        router,
        "/app/refresh",
        "",
        "[::ffff:127.0.0.1]:50000",
        Some("http://127.0.0.1:7586"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(*node.refreshes.lock().unwrap(), 1);
}

#[tokio::test]
async fn shows_how_to_add_plumb_to_firefox() {
    let body = get_panel(app(status(Phase::Ready, Step::Idle)).0).await;
    assert!(
        body.contains(
            "href=\"http://127.0.0.1:7586/add-to-firefox\" target=\"_blank\">Add to Firefox"
        ),
        "{body}"
    );
    let request = Request::get("/add-to-firefox")
        .header(header::HOST, "127.0.0.1:7586")
        .body(Body::empty())
        .unwrap();
    let response = app(status(Phase::SettingUp, Step::Idle))
        .0
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    // Firefox offers "Add" for pages that link to an OpenSearch description.
    assert!(
        body.contains("<link rel=\"search\" type=\"application/opensearchdescription+xml\""),
        "{body}"
    );
    assert!(
        body.contains("Choose <strong>Add \"Plumb Search\"</strong>"),
        "{body}"
    );
    assert!(
        body.contains("<code>http://127.0.0.1:7586/search?q=%s</code>"),
        "{body}"
    );
}

#[tokio::test]
async fn editing_pages_keep_unsaved_input_and_show_real_network_state() {
    let mut status = status(Phase::Ready, Step::Idle);
    status.network = Some(plumb_net::NetStatus {
        connected_peers: 3,
        relaying_peers: 2,
        batches_received: 8,
        peer_id: "<untrusted-peer>".into(),
        ..Default::default()
    });
    let (router, _) = app(status);
    for section in ["resources", "network", "search", "about"] {
        let body = get_section(router.clone(), section).await;
        assert!(!body.contains("http-equiv=\"refresh\""));
        assert!(body.contains(&format!(
            "href=\"/app?section={section}\" aria-current=\"page\""
        )));
    }
    let search = get_section(router.clone(), "search").await;
    assert!(search.contains("name=\"search_by_meaning\""));
    assert!(!search.contains("name=\"private_search\""));
    assert!(!search.contains("name=\"network\""));
    assert!(search.contains("class=\"wrap node-panel\""));
    let body = get_section(router, "network").await;
    assert!(body.contains("Connected: 3 over the internet."), "{body}");
    assert!(body.contains("&lt;untrusted-peer&gt;"));
    assert!(!body.contains("<untrusted-peer>"));
}

#[tokio::test]
async fn feature_changes_require_local_access_validate_and_show_restart() {
    let (router, node) = app(status(Phase::Ready, Step::Idle));
    for form in [
        "share_popularity=1",
        "network=1&bootstrap=not-an-address",
        "network=1&trust_shown=1&trusted=not-a-node",
        "network=1&credits_shown=1&answer_per_day=lots",
    ] {
        let response = post(
            router.clone(),
            "/app/features",
            form,
            "127.0.0.1:50000",
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(*node.features.lock().unwrap(), FeatureSettings::default());
    }
    for (peer, origin) in [
        ("192.168.1.20:50000", None),
        ("127.0.0.1:50000", Some("https://evil.example")),
    ] {
        assert_eq!(
            post(router.clone(), "/app/features", "network=1", peer, origin)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let response = post(
        router.clone(),
        "/app/features",
        "network=1&search_by_meaning=1&share_popularity=1",
        "127.0.0.1:50000",
        Some("http://127.0.0.1:7586"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(node.features.lock().unwrap().network);
    assert!(!node.features.lock().unwrap().no_default_trust);
    let page = get_section(router.clone(), "network").await;
    assert!(page.contains("name=\"default_trust\" value=\"1\" checked"));
    assert!(page.contains("name=\"trusted\""));
    let node_id = "12D3KooWEwYB7PYxRNgvSWiwkLXvwYajSmYn4yoPqmkN7NbNqJjg";
    let response = post(
        router.clone(),
        "/app/features",
        &format!("network=1&trust_shown=1&trusted={node_id}"),
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(node.features.lock().unwrap().no_default_trust);
    assert_eq!(node.features.lock().unwrap().trusted, [node_id]);
    assert!(page.contains("name=\"answer_per_day\""));
    assert!(page.contains("name=\"spend_credits\" value=\"1\" checked"));
    let response = post(
        router.clone(),
        "/app/features",
        "network=1&credits_shown=1&answer_per_day=20%2C000",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let saved = node.features.lock().unwrap().clone();
    assert_eq!(saved.answer_limit.unwrap().per_day, Some(20_000));
    assert_eq!(saved.spend_credits, Some(false));
    let page = get_section(router.clone(), "network").await;
    assert!(page.contains("value=\"20000\""), "{page}");
    let response = post(
        router.clone(),
        "/app/features",
        "network=1&credits_shown=1&answer_per_day=&spend_credits=1",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let saved = node.features.lock().unwrap().clone();
    assert_eq!(saved.answer_limit.unwrap().per_day, None, "blank: no limit");
    assert_eq!(saved.spend_credits, Some(true));
    // A form without the credit choices leaves them as they were.
    post(
        router.clone(),
        "/app/features",
        "network=1",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(node.features.lock().unwrap().spend_credits, Some(true));
    let response = post(
        router.clone(),
        "/app/features",
        "section=search&search_history=1",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(node.features.lock().unwrap().network);
    assert!(!node.features.lock().unwrap().search_by_meaning);
    assert!(get_section(router.clone(), "network")
        .await
        .contains("Feature changes saved."));
    let mut request = Request::get("/app?section=resources")
        .header(header::HOST, "server:8080")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "192.168.1.20:50000".parse::<SocketAddr>().unwrap(),
    ));
    let body = body_text(router.oneshot(request).await.unwrap()).await;
    assert!(body.contains("Settings are read-only"));
    assert!(body.contains("<fieldset disabled>"));
}

#[tokio::test]
async fn the_credits_card_says_what_was_earned_and_given() {
    let mut status = status(Phase::Ready, Step::Idle);
    status.network = Some(plumb_net::NetStatus {
        peer_id: "me".into(),
        credits: plumb_net::credits::CreditStatus {
            at_peers: vec![
                plumb_net::credits::CreditsAtPeer {
                    peer_id: "12D3KooWfirst-node-AAAAAAAA".into(),
                    credits: 1_200,
                    counts: true,
                },
                plumb_net::credits::CreditsAtPeer {
                    peer_id: "12D3KooWsecond-node-<BBBBBB>".into(),
                    credits: 34,
                    counts: false,
                },
            ],
            free_answers_today: 4_000,
            answer_per_day: Some(10_000),
            turned_away: 7,
            tokens_held: 16,
            ..Default::default()
        },
        ..Default::default()
    });
    let (router, _) = app(status);
    let page = get_section(router, "network").await;
    assert!(page.contains("1,234 credits at other nodes"), "{page}");
    assert!(page.contains("AAAAAAAA</code></dt><dd>1,200 credits</dd>"));
    assert!(page.contains("not spendable there yet"));
    assert!(!page.contains("<BBBBBB>"), "ids are escaped");
    assert!(page.contains("4,000 searches answered for other nodes today of 10,000 allowed"));
}

#[tokio::test]
async fn the_network_card_explains_why_a_node_is_not_connected() {
    let mut status = status(Phase::Ready, Step::Idle);
    status.network = Some(plumb_net::NetStatus {
        alone_since: Some(now_unix() - 300),
        problem: Some(plumb_net::JoinProblem::new(
            "The bootstrap node at plumbsearch.org did not answer. <A firewall>",
            "timed out",
            now_unix() - 20,
        )),
        ..Default::default()
    });
    let (router, _) = app(status);
    let overview = get_section(router.clone(), "overview").await;
    assert!(overview.contains("Can\u{2019}t connect"), "{overview}");
    assert!(overview.contains("did not answer. &lt;A firewall&gt;"));
    assert!(overview.contains("action=\"/app/network/retry\""));
    // No bootstrap nodes saved: only nearby nodes can be found.
    assert!(overview.contains("only Plumb nodes on this network"));
    // The fake node has no network to retry.
    let response = post(
        router.clone(),
        "/app/network/retry",
        "",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        post(router, "/app/network/retry", "", "192.168.1.20:50000", None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn the_network_section_lists_connected_nodes_and_one_bootstrap_switch() {
    let mut status = status(Phase::Ready, Step::Idle);
    let peer = |id: &str, route, relay, bootstrap| plumb_net::PeerView {
        peer_id: id.into(),
        route,
        relay,
        bootstrap,
    };
    status.network = Some(plumb_net::NetStatus {
        connected_peers: 4,
        nearby_peers: 1,
        nat: "private".into(),
        relays: vec!["relay".into()],
        peers: vec![
            peer(
                "12D3KooWrelayAAAA11111111",
                plumb_net::Route::Direct,
                true,
                true,
            ),
            peer(
                "12D3KooWhomeBBBB22222222",
                plumb_net::Route::Nearby,
                false,
                false,
            ),
            peer(
                "12D3KooWfarCCCC<33333333>",
                plumb_net::Route::Relayed,
                false,
                false,
            ),
        ],
        ..Default::default()
    });
    let (router, node) = app(status);
    let body = get_section(router.clone(), "network").await;
    assert!(
        body.contains("Connected: 1 on this network, 2 over the internet, 1 through a relay."),
        "{body}"
    );
    assert!(body.contains("reach this one through a relay"));
    assert!(body.contains(
        "11111111</code></dt><dd>Over the internet · bootstrap node · relays for this node</dd>"
    ));
    assert!(body.contains("<dd>On this network</dd>"));
    assert!(body.contains("&lt;33333333&gt;"));
    assert!(body.contains("And 1 more."));
    // A node not yet in the network finds others through plumbsearch.org
    // unless its owner says otherwise.
    assert!(body.contains("name=\"plumb_bootstrap\" value=\"1\" checked"));
    assert!(!body.contains("<details open>"));

    let response = post(
        router.clone(),
        "/app/features",
        "network=1&plumb_bootstrap=1&bootstrap=%2Fip4%2F10.0.0.2%2Ftcp%2F4001",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let saved = node.features.lock().unwrap().clone();
    assert!(saved.uses_default_bootstrap());
    assert_eq!(
        saved.extra_bootstrap().collect::<Vec<_>>(),
        ["/ip4/10.0.0.2/tcp/4001"]
    );
    let body = get_section(router.clone(), "network").await;
    assert!(body.contains("<details open>"));
    assert!(body.contains(">/ip4/10.0.0.2/tcp/4001</textarea>"));

    post(
        router.clone(),
        "/app/features",
        "network=1",
        "127.0.0.1:50000",
        None,
    )
    .await;
    let saved = node.features.lock().unwrap().clone();
    assert!(saved.bootstrap.is_empty());
    let body = get_section(router, "network").await;
    assert!(!body.contains("name=\"plumb_bootstrap\" value=\"1\" checked"));
}

#[tokio::test]
async fn presets_crawl_hours_and_pauses_are_one_click() {
    let (router, node) = app(status(Phase::Ready, Step::Idle));
    let form = "background_updates=1&workload=light&download_limit_mb_per_day=999\
                &crawl_hours=1&crawl_from=22&crawl_to=7";
    let response = post(
        router.clone(),
        "/app/settings",
        form,
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let saved = node.settings.lock().unwrap().clone();
    assert_eq!(saved.workload, Workload::Light);
    // The preset's limits win over what was typed.
    assert_eq!(
        (saved.download_limit_mb_per_day, saved.storage_limit_mb),
        (100, 1_000)
    );
    assert_eq!(saved.crawl_hours, Some(CrawlHours { from: 22, to: 7 }));
    let body = get_section(router.clone(), "resources").await;
    assert!(body.contains("value=\"light\" checked"), "{body}");
    assert!(body.contains("name=\"crawl_hours\" value=\"1\" checked"));
    assert!(body.contains("<option value=\"22\" selected>22:00</option>"));

    let response = post(
        router.clone(),
        "/app/settings",
        "crawl_hours=1&crawl_from=25&crawl_to=7",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let before = now_unix();
    let response = post(
        router.clone(),
        "/app/pause",
        "until=hour",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let until = node.settings.lock().unwrap().paused_until.unwrap();
    assert!((before + 3600..=now_unix() + 3600).contains(&until));
    // Saving the settings form keeps the pause.
    post(
        router.clone(),
        "/app/settings",
        "workload=full",
        "127.0.0.1:50000",
        None,
    )
    .await;
    let saved = node.settings.lock().unwrap().clone();
    assert_eq!(saved.paused_until, Some(until));
    assert_eq!(saved.workload, Workload::Full);
    assert_eq!(saved.crawl_hours, None);
    post(
        router.clone(),
        "/app/pause",
        "until=resume",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(node.settings.lock().unwrap().paused_until, None);
    for (form, peer, code) in [
        ("until=forever", "127.0.0.1:50000", StatusCode::BAD_REQUEST),
        ("until=hour", "192.168.1.20:50000", StatusCode::FORBIDDEN),
    ] {
        assert_eq!(
            post(router.clone(), "/app/pause", form, peer, None)
                .await
                .status(),
            code
        );
    }
    // This node cannot restart itself.
    assert_eq!(
        post(router, "/app/restart", "", "127.0.0.1:50000", None)
            .await
            .status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn the_panel_shows_pauses_progress_and_restart_to_apply() {
    let mut paused = status(Phase::Ready, Step::Idle);
    paused.background_updates = true;
    paused.paused = Some("Paused by you".into());
    paused.paused_until = Some(now_unix() + 1800);
    paused.can_restart = true;
    paused.meaning_work = Some(crate::node::BackgroundWork {
        detail: "Downloading the search-by-meaning model".into(),
        progress: Some(Progress {
            done: 48,
            total: 130,
            unit: "MB".into(),
        }),
        error: None,
    });
    let (router, node) = app(paused);
    node.settings.lock().unwrap().paused_until = Some(now_unix() + 1800);
    let body = get_section(router.clone(), "overview").await;
    assert!(body.contains("<p>Resumes in "), "{body}");
    assert!(body.contains("value=\"resume\">Resume now</button>"));
    assert!(body.contains("Downloading the search-by-meaning model: 48 of about 130 MB."));
    assert!(!body.contains("Restart to apply"));
    // Saved feature changes that differ from the running ones.
    node.features.lock().unwrap().search_by_meaning = true;
    let body = get_section(router, "overview").await;
    assert!(body.contains("action=\"/app/restart\""), "{body}");
    assert!(body.contains("Restart to apply"));

    let mut running = status(Phase::Ready, Step::Idle);
    running.background_updates = true;
    let (router, _) = app(running);
    let body = get_section(router, "overview").await;
    assert!(body.contains("value=\"hour\" class=\"alt\">Pause for an hour"));
}

#[tokio::test]
async fn failures_offer_a_retry_and_the_activity_log_reads_plainly() {
    let mut failing = status(Phase::Ready, Step::Retrying);
    failing.last_error = Some(LastError {
        message: "downloading the Tranco list failed".into(),
        at: now_unix() - 60,
        // A little past 10 minutes, so a second ticking over before the
        // page renders still reads "10 minutes".
        retry_at: Some(now_unix() + 630),
    });
    let (router, _) = app(failing);
    let body = get_section(router.clone(), "overview").await;
    assert!(
        body.contains("value=\"work\"><button type=\"submit\" class=\"alt\">Try again now"),
        "{body}"
    );
    assert!(
        body.contains("Plumb tries again by itself in 10 minutes."),
        "{body}"
    );
    assert!(body.contains("All activity"));
    assert!(!body.contains("<strong>Error:</strong> Could not reach &lt;tranco-list.eu&gt;"));
    let body = get_section(router.clone(), "activity").await;
    assert!(body.contains("2 minutes ago"), "{body}");
    // The fake node keeps no backups, but its own panel offers them.
    let body = get_section(router.clone(), "backup").await;
    assert!(body.contains("Make a backup now"), "{body}");
    assert!(body.contains("enctype=\"multipart/form-data\""));

    for (form, code) in [
        ("what=work", StatusCode::SEE_OTHER),
        ("what=meaning", StatusCode::CONFLICT),
        ("what=everything", StatusCode::BAD_REQUEST),
    ] {
        let response = post(router.clone(), "/app/retry", form, "127.0.0.1:50000", None).await;
        assert_eq!(response.status(), code, "{form}");
    }
    let response = post(
        router.clone(),
        "/app/retry",
        "what=work",
        "192.168.1.20:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = post(
        router,
        "/app/backups/restore",
        "name=..%2F..%2Fetc%2Fpasswd",
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plumb_serve_has_no_panel() {
    let router = crate::web::router(Arc::new(NoSearch));
    let request = Request::get("/app").body(Body::empty()).unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn refresh_is_opt_in_and_can_be_paused() {
    let (router, _) = app(status(Phase::SettingUp, Step::Idle));
    let normal = get_section(router.clone(), "overview").await;
    assert!(!normal.contains("http-equiv=\"refresh\""));
    let enabled = get_section(router.clone(), "overview&refresh=on").await;
    assert!(enabled.contains("content=\"30\""));
    assert!(enabled.contains("Pause automatic refresh"));
    let paused = get_section(router, "overview&refresh=off").await;
    assert!(!paused.contains("http-equiv=\"refresh\""));
}

#[tokio::test]
async fn saved_restore_requires_confirmation_before_changing_anything() {
    struct BackupNode {
        dir: std::path::PathBuf,
        restored: Mutex<usize>,
    }
    impl StatusSource for BackupNode {
        fn status(&self) -> Status {
            status(Phase::Ready, Step::Idle)
        }
        fn data_dir(&self) -> Option<std::path::PathBuf> {
            Some(self.dir.clone())
        }
        fn restore_backup(&self, _: &Backup) -> anyhow::Result<()> {
            *self.restored.lock().unwrap() += 1;
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let backup = backup::save(dir.path(), None).unwrap();
    let node = Arc::new(BackupNode {
        dir: dir.path().into(),
        restored: Mutex::new(0),
    });
    let app = node_router(Arc::new(NoSearch), node.clone());
    let form = format!("name={}", backup.name);
    let response = post(
        app.clone(),
        "/app/backups/restore",
        &form,
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(body.contains("Restore this backup?"));
    assert!(body.contains("name=\"confirm\" value=\"yes\""));
    assert!(body.contains("content=\"same-origin\""));
    assert_eq!(*node.restored.lock().unwrap(), 0);
    // Leaving the confirmation page through Cancel/Back is a GET only.
    let mut cancel = Request::get("/app?section=backup")
        .header(header::HOST, "127.0.0.1:7586")
        .body(Body::empty())
        .unwrap();
    cancel.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:50000".parse::<SocketAddr>().unwrap(),
    ));
    assert_eq!(
        app.clone().oneshot(cancel).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(*node.restored.lock().unwrap(), 0);
    // A file posted without confirmation, or an interrupted multipart body,
    // must never reach the restore callback.
    for data in [
        "--test\r\nContent-Disposition: form-data; name=\"backup\"; filename=\"backup.json\"\r\n\r\n{}\r\n--test--\r\n",
        "--test\r\nContent-Disposition: form-data; name=\"confirm\"\r\n\r\nyes\r\n--test\r\nContent-Disposition: form-data; name=\"backup\"; filename=\"backup.json\"\r\n\r\n{",
    ] {
        let mut upload = Request::post("/app/restore")
            .header(header::HOST, "127.0.0.1:7586")
            .header(header::CONTENT_TYPE, "multipart/form-data; boundary=test")
            .body(Body::from(data)).unwrap();
        upload.extensions_mut().insert(ConnectInfo("127.0.0.1:50000".parse::<SocketAddr>().unwrap()));
        assert_eq!(app.clone().oneshot(upload).await.unwrap().status(), StatusCode::BAD_REQUEST);
        assert_eq!(*node.restored.lock().unwrap(), 0);
    }
    let response = post(
        app,
        "/app/backups/restore",
        &format!("{form}&confirm=yes"),
        "127.0.0.1:50000",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(*node.restored.lock().unwrap(), 1);
}
#[test]
fn remote_panels_never_offer_an_unrouted_restart() {
    let mut status = status(Phase::Ready, Step::Idle);
    status.can_restart = true;
    let active = FeatureSettings::default();
    let saved = FeatureSettings {
        search_by_meaning: true,
        ..active.clone()
    };
    let settings = NodeSettings::default();
    let query = PanelQuery::default();
    let mut view = PanelView {
        status: &status,
        settings: &settings,
        origin: "http://localhost:7586",
        data_dir: None,
        now: now_unix(),
        query: &query,
        active: &active,
        saved: &saved,
        writable: true,
        private_ready: false,
        base: "/app/nodes/test",
        eyebrow: "Test",
        switcher: "",
        plugins: None,
        remote_control: None,
        activity: &[],
        backups: None,
    };
    assert!(!render_panel(&view).contains("/app/nodes/test/restart"));
    view.base = "/app";
    assert!(render_panel(&view).contains("action=\"/app/restart\""));
}
