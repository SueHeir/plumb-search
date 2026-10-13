//! A node's status: setting up, ready, sharing popularity, and the notes
//! it shows.

use super::*;

#[tokio::test]
async fn search_navigation_only_shows_controls_for_a_direct_local_node() {
    let app = || {
        node_router(
            backend(bank_hits()),
            node(node_status(Phase::Ready, Step::Idle)),
        )
    };
    for path in ["/", "/search?q=us+bank"] {
        let (_, _, local) = send_with_headers(app(), path, &[("host", "127.0.0.1:7586")]).await;
        assert!(local.contains("href=\"/app\">Settings</a>"));
        assert!(!local.contains(">Status</a>"));
        for headers in [
            vec![("host", "plumbsearch.org")],
            vec![
                ("host", "127.0.0.1:7586"),
                ("x-forwarded-for", "203.0.113.2"),
            ],
            vec![("host", "127.0.0.1:7586"), ("forwarded", "for=203.0.113.2")],
        ] {
            let (_, _, public) = send_with_headers(app(), path, &headers).await;
            assert!(!public.contains("href=\"/app\""));
            assert!(!public.contains("href=\"/app?section=search\""));
        }
    }
    let (_, _, standalone) = send_with_headers(
        router(backend(bank_hits())),
        "/",
        &[("host", "127.0.0.1:7586")],
    )
    .await;
    assert!(!standalone.contains("href=\"/app\""));
}

#[tokio::test]
async fn a_node_reports_its_status_as_json() {
    let mut status = node_status(Phase::SettingUp, Step::Downloading);
    status.progress = Some(Progress {
        done: 1,
        total: 3,
        unit: "files".into(),
    });
    status.last_error = Some(LastError {
        message: "no network".into(),
        at: 1_700_000_000,
        retry_at: Some(1_700_000_600),
    });
    let app = node_router(backend(Vec::new()), node(status.clone()));
    let (code, headers, body) = send(app, "/api/status").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(serde_json::from_str::<Status>(&body).unwrap(), status);
    // The names are an interface: the desktop app and scripts read them.
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json["build"],
        serde_json::to_value(crate::build_info::current()).unwrap()
    );
    assert_eq!(json["phase"], "setting_up");
    assert_eq!(json["step"], "downloading");
    assert_eq!(
        json["progress"],
        serde_json::json!({"done": 1, "total": 3, "unit": "files"})
    );
    assert_eq!(
        json["last_error"],
        serde_json::json!({"message": "no network", "at": 1_700_000_000u64,
                           "retry_at": 1_700_000_600u64})
    );
    assert_eq!(json["wikidata_missing"], false);
    for null in ["index", "last_refresh", "next_refresh", "wikidata_error"] {
        assert!(json[null].is_null(), "{null}");
    }
    assert_eq!(json["sites"], 0);
    // plumbsearch.org's homepage shows these two.
    assert_eq!(json["disk_used"], 0);
    assert_eq!(json["storage_limit"], 0);

    // `plumb serve` has no status to report.
    let (code, _, _) = get(backend(Vec::new()), "/api/status").await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_node_setting_up_shows_its_progress_instead_of_searching() {
    let now = now_unix();
    let mut status = node_status(Phase::SettingUp, Step::Retrying);
    status.detail = "Waiting <to> try again".to_string();
    status.progress = Some(Progress {
        done: 1_500,
        total: 10_000,
        unit: "homepages".into(),
    });
    status.last_error = Some(LastError {
        message: "HTTP 503: <script>alert('x')</script> & more".into(),
        at: now - 150,
        retry_at: Some(now + 630),
    });
    let fake = backend(bank_hits());
    let node = node(status);
    for uri in ["/", "/search?q=us+bank", "/search"] {
        let app = node_router(fake.clone(), node.clone());
        let (code, headers, body) = send(app, uri).await;
        assert_eq!(code, StatusCode::OK, "{uri}");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert!(headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("default-src 'none'"));
        for expected in [
            "<meta http-equiv=\"refresh\" content=\"5\">",
            "<title>Setting up - Plumb Search</title>",
            "<p class=\"step\">Waiting &lt;to&gt; try again</p>",
            "<progress aria-label=\"Setup progress\" value=\"1500\" max=\"10000\"></progress>",
            "1,500 of 10,000 homepages",
            "Something went wrong</strong> 2 minutes ago:",
            "HTTP 503: &lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; more",
            "Plumb will try again in 10 minutes.",
        ] {
            assert!(body.contains(expected), "{uri}: no {expected:?} in {body}");
        }
        assert!(!body.contains("<script"), "{body}");
        assert!(!body.contains("<form"), "{body}");
    }
    let app = node_router(fake.clone(), node.clone());
    let (code, headers, body) = send(app, "/api/search?q=us+bank").await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers[header::RETRY_AFTER], "5");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"error": "the search index is not ready yet",
                           "phase": "setting_up", "step": "retrying"})
    );
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn the_setup_page_shows_only_what_it_knows() {
    let status = node_status(Phase::SettingUp, Step::Starting);
    let body = render_setup(&status, 1_700_000_000);
    assert!(body.contains("Downloading the Tranco list"));
    assert!(!body.contains("<progress"));
    assert!(!body.contains("Something went wrong"));
    let mut status = status;
    status.progress = Some(Progress {
        done: 0,
        total: 0,
        unit: "files".into(),
    });
    status.last_error = Some(LastError {
        message: "boom".into(),
        at: 1_700_000_000,
        retry_at: None,
    });
    let body = render_setup(&status, 1_700_000_000);
    assert!(
        body.contains("<progress aria-label=\"Setup progress\" value=\"0\" max=\"1\"></progress>")
    );
    assert!(body.contains("Something went wrong</strong> just now:"));
    assert!(!body.contains("will try again"));
}

#[tokio::test]
async fn a_ready_node_searches_and_says_what_it_is_doing() {
    let fake = backend(bank_hits());
    let mut status = node_status(Phase::Ready, Step::Crawling);
    status.progress = Some(Progress {
        done: 1_500,
        total: 10_000,
        unit: "homepages".into(),
    });
    status.sites = 12_345;
    status.places = 655;
    status.page_coverage = Some(crate::node::PageCoverage {
        indexed_pages: 100_000,
        index_generation: None,
        sets: Vec::new(),
    });
    let node = node(status);
    let app = || node_router(fake.clone(), node.clone());
    let (code, _, body) = send(app(), "/").await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("<form action=\"/search\""), "{body}");
    assert!(
        body.contains("113,000 items indexed &middot; crawling homepages, 1,500 of 10,000</p>"),
        "{body}"
    );
    assert!(!body.contains("http-equiv"), "{body}");
    let (code, _, body) = send(app(), "/search?q=us+bank").await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains("href=\"https://www.usbank.com/\""), "{body}");
    let (code, _, body) = send(app(), "/api/search?q=us+bank&limit=1").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Vec<Hit>>(&body).unwrap(),
        bank_hits()[..1].to_vec()
    );
    assert_eq!(fake.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_node_sharing_popularity_notes_the_result_opened_and_says_so() {
    let fake = backend(bank_hits());
    let node = Arc::new(SharingNode {
        status: node_status(Phase::Ready, Step::Idle),
        picks: Mutex::new(Vec::new()),
    });
    let app = || node_router(fake.clone(), node.clone());
    let (code, _, body) = send(app(), "/search?q=us+bank&country=any").await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        body.contains("href=\"/go?q=us+bank&amp;d=usbank.com&amp;country=any&amp;lang=en\""),
        "{body}"
    );
    // The address shown is still the site's own.
    assert!(
        body.contains("<span class=\"u\">www.usbank.com</span>"),
        "{body}"
    );
    assert!(
        body.contains("This node shares which result is opened"),
        "{body}"
    );

    let (code, headers, _) = send(app(), "/go?q=us+bank&d=usbank.com&country=any").await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "https://www.usbank.com/");
    assert_eq!(
        *node.picks.lock().unwrap(),
        vec![("us bank".to_string(), "usbank.com".to_string())]
    );

    // Only to a site the search returns: anything else goes back.
    for uri in [
        "/go?q=us+bank&d=evil.example",
        "/go?q=&d=usbank.com",
        "/go?d=usbank.com",
    ] {
        let (code, headers, _) = send(app(), uri).await;
        assert_eq!(code, StatusCode::SEE_OTHER, "{uri}");
        assert!(
            headers[header::LOCATION]
                .to_str()
                .unwrap()
                .starts_with("/search?q="),
            "{uri}"
        );
    }
    assert_eq!(node.picks.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_node_not_sharing_popularity_links_straight_to_sites() {
    let fake = backend(bank_hits());
    let node = node(node_status(Phase::Ready, Step::Idle));
    let (_, _, body) = send(node_router(fake, node), "/search?q=us+bank").await;
    assert!(body.contains("href=\"https://www.usbank.com/\""), "{body}");
    assert!(!body.contains("/go?"), "{body}");
    assert!(!body.contains("shares which result"), "{body}");
}

#[tokio::test]
async fn a_node_without_wikidata_says_so() {
    let now = now_unix();
    let mut status = node_status(Phase::Ready, Step::Idle);
    status.wikidata_missing = true;
    status.wikidata_error = Some(LastError {
        message: "Wikidata stopped the query <at> its time limit".into(),
        at: now - 60,
        retry_at: Some(now + 630),
    });
    let fake = backend(bank_hits());
    let (code, _, body) = send(node_router(fake.clone(), node(status.clone())), "/").await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        body.contains(
            "<p class=\"s\">Wikidata&#39;s list of official websites could not be \
             downloaded yet, so the index does without it for now: official sites get no \
             boost over look-alikes. Plumb will try again in 10 minutes.</p>"
        ),
        "{body}"
    );
    let (_, _, body) = send(
        node_router(fake.clone(), node(status.clone())),
        "/api/status",
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["wikidata_missing"], true);
    assert_eq!(
        json["wikidata_error"]["message"],
        "Wikidata stopped the query <at> its time limit"
    );

    // While the first index is built, the setup page says it too, with why.
    status.phase = Phase::SettingUp;
    status.step = Step::Indexing;
    let body = render_setup(&status, now);
    assert!(body.contains("could not be downloaded yet"), "{body}");
    assert!(
        body.contains("<p class=\"msg\">Wikidata stopped the query &lt;at&gt; its time limit</p>"),
        "{body}"
    );

    // Before Wikidata is first tried, it is still to come.
    status.wikidata_error = None;
    assert!(!render_setup(&status, now).contains("Wikidata"));
    status.phase = Phase::Ready;
    assert!(
        render_home(12, Some(&status), now, &no_settings()).contains(
            "Plumb is still downloading Wikidata&#39;s list of official websites and more \
             rankings. Search works now, and results get better once those are in."
        ),
        "{}",
        render_home(12, Some(&status), now, &no_settings())
    );

    // Once Wikidata is in, nothing is said.
    status.wikidata_missing = false;
    status.wikidata_error = None;
    assert!(!render_setup(&status, now).contains("Wikidata"));
    assert!(!render_home(12, Some(&status), now, &no_settings()).contains("Wikidata"));
}

#[test]
fn notes_on_the_home_page_of_a_ready_node() {
    let now = 1_700_000_000;
    let note = |step, progress: Option<Progress>, failed: bool, last_refresh| {
        let mut status = node_status(Phase::Ready, step);
        status.progress = progress;
        status.last_refresh = last_refresh;
        if failed {
            status.last_error = Some(LastError {
                message: "boom".into(),
                at: now,
                retry_at: None,
            });
        }
        node_note(&status, now)
    };
    let crawled = |done| {
        Some(Progress {
            done,
            total: 5_000,
            unit: "homepages".into(),
        })
    };
    let cases = [
        (
            note(Step::Crawling, crawled(0), false, None),
            "crawling homepages, 0 of 5,000",
        ),
        (
            note(Step::Crawling, crawled(2_500), true, None),
            "crawling homepages, 2,500 of 5,000",
        ),
        (
            note(Step::Indexing, None, false, Some(now)),
            "rebuilding the index",
        ),
        (
            note(Step::Retrying, None, true, Some(now)),
            "the last update failed and will be tried again (see /api/status)",
        ),
        (
            note(Step::Idle, None, false, Some(now - 2 * 3_600)),
            "updated 2 hours ago",
        ),
        (
            note(Step::Crawling, None, false, Some(now - 30)),
            "updated 30 seconds ago",
        ),
    ];
    for (got, expected) in cases {
        assert_eq!(got.as_deref(), Some(expected));
    }
    assert_eq!(note(Step::Idle, None, false, None), None);
}

#[test]
fn durations_in_words() {
    let words: Vec<String> = [
        0, 1, 59, 60, 119, 3_599, 3_600, 7_200, 86_399, 86_400, 259_200,
    ]
    .into_iter()
    .map(duration_words)
    .collect();
    assert_eq!(
        words,
        [
            "0 seconds",
            "1 second",
            "59 seconds",
            "1 minute",
            "1 minute",
            "59 minutes",
            "1 hour",
            "2 hours",
            "23 hours",
            "1 day",
            "3 days"
        ]
    );
    let now = 1_700_000_000;
    assert_eq!(time_ago(now, now), "just now");
    assert_eq!(time_ago(now - 9, now), "just now");
    assert_eq!(time_ago(now - 10, now), "10 seconds ago");
    assert_eq!(time_ago(now - 3_600, now), "1 hour ago");
    // A clock that went back a little.
    assert_eq!(time_ago(now + 60, now), "just now");
    assert_eq!(time_until(now + 600, now), "in 10 minutes");
    assert_eq!(time_until(now, now), "any moment now");
    assert_eq!(time_until(now - 60, now), "any moment now");
}
