//! What each browser keeps: its history, edits, tuning, clicks and the
//! about page.

use super::*;

/// A node that keeps search history in a folder.
struct HistoryNode(std::path::PathBuf);

impl StatusSource for HistoryNode {
    fn status(&self) -> Status {
        node_status(Phase::Ready, Step::Idle)
    }
    fn search_history(&self) -> Option<crate::history::HistoryStore> {
        Some(crate::history::HistoryStore::new(&self.0))
    }
}

/// The `name=value` of the cookie `name` that `headers` set.
fn set_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(&format!("{name}=")))
        .map(|v| v.split(';').next().unwrap().to_string())
}

#[tokio::test]
async fn each_browser_keeps_its_own_history_and_opened_sites_come_first() {
    let dir = tempfile::tempdir().unwrap();
    let node = Arc::new(HistoryNode(dir.path().join("history")));
    let fake = backend(bank_hits());
    let app = || node_router(fake.clone(), node.clone());

    // The first search gives the browser a profile and links through /go.
    let (code, headers, body) = send(app(), "/search?q=us+bank&country=any").await;
    assert_eq!(code, StatusCode::OK);
    let profile = set_cookie(&headers, "plumb_profile").expect("a profile cookie");
    assert!(
        body.contains(
            "href=\"/go?q=us+bank&amp;d=usbank-login-help.com&amp;country=any&amp;lang=en\""
        ),
        "{body}"
    );
    assert!(!body.contains("You opened this before"), "{body}");
    assert!(body.contains("name=\"hist\""), "{body}");
    let first = |body: &str| {
        body.find("usbank.com</span>").unwrap() < body.find("usbank-login-help.com</span>").unwrap()
    };
    assert!(first(&body));

    // Opening the second result puts it first next time, labelled.
    let me = [("cookie", profile.as_str())];
    let (code, _, _) = send_with_headers(
        app(),
        "/go?q=us+bank&d=usbank-login-help.com&country=any",
        &me,
    )
    .await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    let (_, _, body) = send_with_headers(app(), "/search?q=us+bank&country=any", &me).await;
    assert!(!first(&body), "{body}");
    assert!(body.contains("You opened this before"), "{body}");

    // The home page lists past searches; the history page lists both.
    let (_, _, home) = send_with_headers(app(), "/", &me).await;
    assert!(home.contains("class=\"recent\""), "{home}");
    assert!(home.contains(">us bank</a>"), "{home}");
    let (code, _, page) = send_with_headers(app(), "/history", &me).await;
    assert_eq!(code, StatusCode::OK);
    assert!(page.contains("usbank-login-help.com"), "{page}");

    // Another browser sees none of it.
    let (_, headers, other) = send(app(), "/search?q=us+bank&country=any").await;
    assert!(set_cookie(&headers, "plumb_profile").is_some_and(|c| c != profile));
    assert!(!other.contains("You opened this before"), "{other}");
    assert!(first(&other));

    // Turning both choices off: no labels, no reordering, no list.
    let (_, headers, body) =
        send_with_headers(app(), "/search?q=us+bank&country=any&hist=1", &me).await;
    let prefs = set_cookie(&headers, "plumb_history").unwrap();
    assert_eq!(prefs, "plumb_history=s0r0l0t0");
    assert!(first(&body), "{body}");
    assert!(!body.contains("You opened this before"), "{body}");
    let both = format!("{profile}; {prefs}");
    let (_, _, home) = send_with_headers(app(), "/", &[("cookie", both.as_str())]).await;
    assert!(!home.contains("class=\"recent\""), "{home}");

    // Clearing empties it.
    let response = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/history/clear")
                .header("cookie", profile.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let (_, _, page) = send_with_headers(app(), "/history", &me).await;
    assert!(page.contains("No searches yet."), "{page}");
}

#[tokio::test]
async fn a_map_the_searcher_never_opens_is_folded_until_they_do() {
    let dir = tempfile::tempdir().unwrap();
    let node = Arc::new(HistoryNode(dir.path().join("history")));
    let app = || node_router(Arc::new(BankPlaces), node.clone());

    // "us bank" is the bank's name: no banks in a town called Us.
    let (_, headers, body) = send(app(), "/search?q=us+bank&country=any").await;
    assert!(!body.contains("class=\"pl\""), "{body}");
    let profile = set_cookie(&headers, "plumb_profile").unwrap();
    let me = [("cookie", profile.as_str())];

    // Shown, and passed by, three times: folded the fourth.
    for _ in 0..3 {
        let (_, _, body) = send_with_headers(app(), "/search?q=denver+bank&country=any", &me).await;
        assert!(body.contains("<section class=\"pl\""), "{body}");
        assert!(!body.contains("class=\"plf\""), "{body}");
    }
    let (_, _, body) = send_with_headers(app(), "/search?q=denver+bank&country=any", &me).await;
    assert!(
        body.contains("<details class=\"plf\"><summary>Bank near"),
        "{body}"
    );
    let link = "/go?q=denver+bank&amp;b=places&amp;u=https%3A%2F%2Fbranch.example%2Fdenver\
                &amp;country=any";
    assert!(body.contains(link), "{body}");
    let (_, _, history) = send_with_headers(app(), "/history", &me).await;
    assert!(history.contains("Learned from your clicks"), "{history}");
    assert!(
        history.contains("<strong>Places and map</strong> folded"),
        "{history}"
    );

    // Only the box's own links are followed.
    let (code, headers, _) = send_with_headers(
        app(),
        "/go?q=denver+bank&b=places&u=https%3A%2F%2Fevil.example%2F&country=any",
        &me,
    )
    .await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/search?q=denver+bank");
    // Opening a place brings the box back.
    let (code, headers, _) = send_with_headers(app(), &link.replace("&amp;", "&"), &me).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "https://branch.example/denver");
    let (_, _, body) = send_with_headers(app(), "/search?q=denver+bank&country=any", &me).await;
    assert!(!body.contains("class=\"plf\""), "{body}");

    // Forgetting, or turning learning off, keeps it unfolded.
    let response = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/history/forget-clicks")
                .header("cookie", profile.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let (_, _, history) = send_with_headers(app(), "/history", &me).await;
    assert!(!history.contains("Learned from your clicks"), "{history}");
    let off = format!("{profile}; plumb_history=s1r1l0");
    let off = [("cookie", off.as_str())];
    for _ in 0..4 {
        let (_, _, body) =
            send_with_headers(app(), "/search?q=denver+bank&country=any", &off).await;
        assert!(!body.contains("class=\"plf\""), "{body}");
        assert!(!body.contains("b=places"), "{body}");
    }
}

#[tokio::test]
async fn edit_mode_moves_and_hides_results_and_folds_boxes() {
    let dir = tempfile::tempdir().unwrap();
    let node = Arc::new(HistoryNode(dir.path().join("history")));
    let app = || node_router(Arc::new(BankPlaces), node.clone());
    let post = |cookie: String, form: String, site: Option<&'static str>| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/feedback")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("cookie", cookie);
        if let Some(site) = site {
            request = request.header("sec-fetch-site", site);
        }
        app().oneshot(request.body(Body::from(form)).unwrap())
    };

    let (_, headers, body) = send(app(), "/search?q=denver+bank&country=any").await;
    let profile = set_cookie(&headers, "plumb_profile").unwrap();
    let me = [("cookie", profile.as_str())];
    assert!(
        body.contains("&amp;edit=1\">Edit these results</a>"),
        "{body}"
    );
    assert!(!body.contains("action=\"/feedback\""), "{body}");

    let (_, _, body) =
        send_with_headers(app(), "/search?q=denver+bank&country=any&edit=1", &me).await;
    assert!(body.contains("Edit mode:"), "{body}");
    assert!(
        body.contains("name=\"d\" value=\"usbank-login-help.com\""),
        "{body}"
    );
    assert!(body.contains("name=\"b\" value=\"places\""), "{body}");

    // Not for this search: gone from it, listed last while editing.
    let back = "%2Fsearch%3Fq%3Ddenver%2Bbank%26country%3Dany%26edit%3D1";
    let response = post(
        profile.clone(),
        format!("q=denver+bank&d=usbank.com&v=hide&back={back}"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[header::LOCATION],
        "/search?q=denver+bank&country=any&edit=1"
    );
    let (_, _, body) = send_with_headers(app(), "/search?q=denver+bank&country=any", &me).await;
    assert!(!body.contains("usbank.com</span>"), "{body}");
    assert!(body.contains("usbank-login-help.com</span>"), "{body}");
    let (_, _, other) = send_with_headers(app(), "/search?q=us+bank&country=any", &me).await;
    assert!(other.contains("usbank.com</span>"), "{other}");
    let (_, _, body) =
        send_with_headers(app(), "/search?q=denver+bank&country=any&edit=1", &me).await;
    assert!(
        body.find("usbank-login-help.com</span>").unwrap()
            < body.find("usbank.com</span>").unwrap(),
        "{body}"
    );
    assert!(body.contains("Hidden from this search"), "{body}");
    assert!(body.contains("value=\"none\" title=\"Undo\""), "{body}");

    // Undo, then put the second one higher.
    post(
        profile.clone(),
        "q=denver+bank&d=usbank.com&v=none&back=/search?q=x".into(),
        None,
    )
    .await
    .unwrap();
    post(
        profile.clone(),
        "q=denver+bank&d=usbank-login-help.com&v=up&back=https://evil.example/".into(),
        None,
    )
    .await
    .unwrap();
    let (_, _, body) = send_with_headers(app(), "/search?q=denver+bank&country=any", &me).await;
    assert!(
        body.find("usbank-login-help.com</span>").unwrap()
            < body.find("usbank.com</span>").unwrap(),
        "{body}"
    );
    assert!(body.contains("You put this higher"), "{body}");

    // Fold the places for every search like this.
    post(
        profile.clone(),
        "q=denver+bank&b=places&c=guessed&v=fold-all&back=/search?q=x".into(),
        None,
    )
    .await
    .unwrap();
    let (_, _, body) = send_with_headers(app(), "/search?q=boulder+bank&country=any", &me).await;
    assert!(body.contains("folded, as you asked"), "{body}");
    let (_, _, page) = send_with_headers(app(), "/history", &me).await;
    assert!(
        page.contains("<strong>Places and map</strong> folded for all searches"),
        "{page}"
    );
    assert!(
        page.contains("usbank-login-help.com</strong> higher"),
        "{page}"
    );

    // Other sites' pages cannot press the buttons.
    let response = post(
        profile.clone(),
        "q=denver+bank&d=usbank-login-help.com&v=hide".into(),
        Some("cross-site"),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn tuning_goes_through_real_results_pages_in_edit_mode() {
    let dir = tempfile::tempdir().unwrap();
    let node = Arc::new(HistoryNode(dir.path().join("history")));
    let app = || node_router(Arc::new(BankPlaces), node.clone());

    let (code, headers, _) = send(app(), "/tune").await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    let first = headers[header::LOCATION].to_str().unwrap().to_owned();
    assert!(first.starts_with("/search?q="), "{first}");
    assert!(first.contains("&edit=1&tune="), "{first}");

    let (_, headers, body) = send(app(), &first).await;
    let profile = set_cookie(&headers, "plumb_profile").expect("a profile");
    let me = [("cookie", profile.as_str())];
    assert!(body.contains("Tuning your search"), "{body}");
    assert!(body.contains("1 of 5"), "{body}");
    assert!(body.contains("Next search"), "{body}");
    assert!(body.contains("name=\"t\" value=\"popular\""), "{body}");
    let tune = first.split("tune=").nth(1).unwrap().to_owned();
    assert!(
        body.contains(&format!("&amp;edit=1&amp;tune={tune}\"")),
        "the buttons come back to the round: {body}"
    );

    // Putting a well-known site up once teaches nothing about well-known
    // sites yet.
    let back: String = url::form_urlencoded::byte_serialize(first.as_bytes()).collect();
    let response = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/feedback")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header("cookie", profile.as_str())
                // What browsers send with a form of a no-referrer page.
                .header(header::ORIGIN, "null")
                .header("sec-fetch-site", "same-origin")
                .body(Body::from(format!(
                    "q=us+bank&d=usbank.com&t=popular&v=up&back={back}"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()[header::LOCATION], first.as_str());
    let (_, _, done) = send_with_headers(app(), "/tune?done=1", &me).await;
    assert!(done.contains("Nothing learned yet"), "{done}");
    let (_, _, body) = send_with_headers(app(), "/search?q=denver+bank&country=any", &me).await;
    assert!(!body.contains("You like well-known sites"), "{body}");
    assert!(body.contains("href=\"/tune\""), "{body}");
    let id = profile.trim_start_matches("plumb_profile=");
    let learned = crate::history::HistoryStore::new(&node.0).load(id).learned;
    let judged = learned.judged.expect("results seen in edit mode");
    assert_eq!(judged.liked, 1.0, "{judged:?}");
    assert_eq!(judged.pages.len(), 1, "{judged:?}");
    // The round's searches are not kept as past searches.
    let (_, _, history) = send_with_headers(app(), "/history", &me).await;
    assert!(
        !history.contains("<h2>Searches</h2>\n<ul>\n<li><a href=\"/search?q=us"),
        "{history}"
    );
}

#[tokio::test]
async fn clicks_count_by_how_far_down_the_page_they_were() {
    let dir = tempfile::tempdir().unwrap();
    let node = Arc::new(HistoryNode(dir.path().join("history")));
    let fake = backend(bank_hits());
    let app = || node_router(fake.clone(), node.clone());
    let (_, headers, _) = send(app(), "/search?q=us+bank&country=any").await;
    let profile = set_cookie(&headers, "plumb_profile").expect("a profile");
    let me = [("cookie", profile.as_str())];
    // The second result, opened.
    let (code, _, _) = send_with_headers(
        app(),
        "/go?q=us+bank&d=usbank-login-help.com&country=any",
        &me,
    )
    .await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    let id = profile.trim_start_matches("plumb_profile=");
    let history = crate::history::HistoryStore::new(&node.0).load(id);
    // Counted twice over: half the people look at the second place.
    assert_eq!(history.opened[0].weighted, 200, "{:?}", history.opened);
    let counts = std::fs::read_to_string(node.0.join("positions.json")).unwrap();
    assert!(!counts.contains("bank"), "{counts}");
    // Opened again from the same page, it is not counted again.
    send_with_headers(
        app(),
        "/go?q=us+bank&d=usbank-login-help.com&country=any",
        &me,
    )
    .await;
    let positions = crate::clicks::PositionStore::in_dir(&node.0).load();
    assert_eq!(positions.bias(), crate::clicks::PositionBias::prior());
    assert_eq!(
        std::fs::read_to_string(node.0.join("positions.json")).unwrap(),
        counts
    );
}

#[tokio::test]
async fn searches_are_kept_for_training_only_when_the_browser_chose_it() {
    let dir = tempfile::tempdir().unwrap();
    let node = Arc::new(HistoryNode(dir.path().join("history")));
    let fake = backend(bank_hits());
    let app = || node_router(fake.clone(), node.clone());
    let labels = node.0.join("click-labels.json");
    for (prefs, kept) in [("s1r1l1", false), ("s1r1l1t1", true)] {
        let (_, headers, _) = send(app(), "/search?q=us+bank&country=any").await;
        let profile = set_cookie(&headers, "plumb_profile").expect("a profile");
        let cookies = format!("{profile}; plumb_history={prefs}");
        let me = [("cookie", cookies.as_str())];
        send_with_headers(app(), "/search?q=us+bank&country=any", &me).await;
        send_with_headers(
            app(),
            "/go?q=us+bank&d=usbank-login-help.com&country=any",
            &me,
        )
        .await;
        assert_eq!(labels.exists(), kept, "{prefs}");
    }
    let text = std::fs::read_to_string(&labels).unwrap();
    assert!(!text.contains("plumb_profile"), "{text}");
    let out = dir.path().join("labels.jsonl");
    let queries = dir.path().join("labels.tsv");
    crate::clicks::run(&crate::cli::ClickLabelsArgs {
        data: dir.path().to_path_buf(),
        out: out.clone(),
        queries: Some(queries.clone()),
        min_opened: 1,
    })
    .unwrap();
    let first: crate::clicks::Label = serde_json::from_str(
        std::fs::read_to_string(&out)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first.query, "us bank");
    assert_eq!(first.domain, "usbank-login-help.com");
    assert_eq!((first.shown, first.opened), (1, 1));
    let tsv = std::fs::read_to_string(&queries).unwrap();
    assert!(tsv.ends_with("us bank\tusbank-login-help.com\n"), "{tsv}");
}

/// A ready node with a data folder, where experiments are set up.
struct LabNode(std::path::PathBuf);

impl StatusSource for LabNode {
    fn status(&self) -> Status {
        node_status(Phase::Ready, Step::Idle)
    }
    fn data_dir(&self) -> Option<std::path::PathBuf> {
        Some(self.0.clone())
    }
}

/// Bank hits, noting the `alpha` of each search ranked for an
/// experiment.
#[derive(Default)]
struct RankedBackend {
    alphas: Mutex<Vec<f32>>,
}

impl SearchBackend for RankedBackend {
    fn search(&self, _query: &str, limit: usize) -> Result<Vec<Hit>> {
        Ok(bank_hits().into_iter().take(limit).collect())
    }
    fn search_ranked(
        &self,
        query: &str,
        limit: usize,
        _options: &SearchOptions,
        rank: &RankConfig,
    ) -> Result<SearchResults> {
        self.alphas.lock().unwrap().push(rank.alpha);
        Ok(SearchResults {
            hits: self.search(query, limit)?,
            ..SearchResults::default()
        })
    }
    fn num_docs(&self) -> u64 {
        2
    }
}

#[tokio::test]
async fn searches_in_an_experiment_are_ranked_its_way_and_their_clicks_counted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(crate::experiments::CONFIG_FILE),
        r#"{"layers": [{"name": "ranking", "experiments": [
            {"name": "more-popularity", "percent": 100, "rank": {"alpha": 0.9}}
        ]}]}"#,
    )
    .unwrap();
    let fake = Arc::new(RankedBackend::default());
    // One app, as a node serves: the pages shown are kept in memory.
    let app = node_router(fake.clone(), Arc::new(LabNode(dir.path().to_path_buf())));
    let (code, _, body) = send(app.clone(), "/search?q=us+bank&country=any").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(*fake.alphas.lock().unwrap(), [0.9]);
    let start = body
        .find("/go?q=us+bank&amp;d=usbank-login-help.com&amp;x=")
        .expect("results link through /go");
    let link = &body[start..start + body[start..].find('"').unwrap()];
    assert!(link.contains("&amp;p=1&amp;"), "{link}");
    let link = link.replace("&amp;", "&");
    let (code, headers, _) = send(app.clone(), &link).await;
    assert_eq!(code, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "https://usbank-login-help.com/");
    // A page never shown counts nothing.
    let forged = link.replace("&x=", "&x=1");
    send(app.clone(), &forged).await;
    let results: std::collections::BTreeMap<String, crate::experiments::Counts> =
        serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(crate::experiments::RESULTS_FILE)).unwrap(),
        )
        .unwrap();
    let counts = results.values().next().unwrap();
    assert_eq!((counts.searches, counts.clicked, counts.clicks), (1, 1, 1));
    assert_eq!(counts.at[1], 1);
    // The JSON API is not in experiments.
    send(app.clone(), "/api/search?q=us+bank").await;
    assert_eq!(fake.alphas.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_about_page_moves_and_hides_results_for_this_browser_only() {
    let dir = tempfile::tempdir().unwrap();
    let node = Arc::new(HistoryNode(dir.path().join("history")));
    let fake = backend(bank_hits());
    let app = || node_router(fake.clone(), node.clone());
    let post = |cookie: Option<&str>, form: &'static str| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/about")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        app().oneshot(request.body(Body::from(form)).unwrap())
    };

    let (code, _, page) = send(app(), "/about").await;
    assert_eq!(code, StatusCode::OK);
    assert!(page.contains("name=\"more\""), "{page}");

    // Saving gives the browser a profile.
    let response = post(None, "pinned=usbank-login-help.com&interests=")
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let profile = set_cookie(response.headers(), "plumb_profile").expect("a profile");
    let me = [("cookie", profile.as_str())];
    let (_, _, body) = send_with_headers(app(), "/search?q=us+bank&country=any", &me).await;
    let first = |body: &str| {
        body.find("usbank.com</span>").unwrap() < body.find("usbank-login-help.com</span>").unwrap()
    };
    assert!(!first(&body), "{body}");
    assert!(body.contains("One of your sites"), "{body}");

    // Another browser sees none of it.
    let (_, _, other) = send(app(), "/search?q=us+bank&country=any").await;
    assert!(first(&other));
    assert!(!other.contains("One of your sites"), "{other}");

    // Hidden sites are left out; forgetting brings them back.
    post(Some(&profile), "hidden=www.usbank-login-help.com")
        .await
        .unwrap();
    let (_, _, body) = send_with_headers(app(), "/search?q=us+bank&country=any", &me).await;
    assert!(!body.contains("usbank-login-help.com</span>"), "{body}");
    let response = post(Some(&profile), "clear=1").await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let (_, _, body) = send_with_headers(app(), "/search?q=us+bank&country=any", &me).await;
    assert!(first(&body), "{body}");
}

#[tokio::test]
async fn nodes_without_history_set_no_cookies() {
    let app = node_router(
        backend(bank_hits()),
        node(node_status(Phase::Ready, Step::Idle)),
    );
    let (_, headers, body) = send(app.clone(), "/search?q=us+bank").await;
    assert!(headers.get(header::SET_COOKIE).is_none());
    assert!(!body.contains("name=\"hist\""), "{body}");
    let (code, _, _) = send(app.clone(), "/history").await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    // The About page is still there: the browser keeps it.
    let (code, headers, page) = send(app, "/about").await;
    assert_eq!(code, StatusCode::OK);
    assert!(headers.get(header::SET_COOKIE).is_none());
    assert!(page.contains("keeps no copy"), "{page}");
}

#[tokio::test]
async fn a_node_without_history_uses_the_about_profile_the_browser_keeps() {
    let app = || {
        node_router(
            backend(bank_hits()),
            node(node_status(Phase::Ready, Step::Idle)),
        )
    };
    let post = |cookie: Option<&str>, form: &'static str| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/about")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        app().oneshot(request.body(Body::from(form)).unwrap())
    };
    let response = post(None, "pinned=usbank-login-help.com&town=Denver")
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(set_cookie(response.headers(), "plumb_profile").is_none());
    let about = set_cookie(response.headers(), "plumb_about").expect("kept in the browser");
    let me = [("cookie", about.as_str())];
    let (_, _, body) = send_with_headers(app(), "/search?q=us+bank&country=any", &me).await;
    assert!(
        body.find("usbank-login-help.com</span>").unwrap()
            < body.find("usbank.com</span>").unwrap(),
        "{body}"
    );
    assert!(body.contains("One of your sites"), "{body}");
    let (_, _, page) = send_with_headers(app(), "/about", &me).await;
    assert!(page.contains("value=\"Denver\""), "{page}");

    // Hiding, and forgetting, change only the cookie.
    let response = post(Some(&about), "hidden=usbank-login-help.com")
        .await
        .unwrap();
    let about = set_cookie(response.headers(), "plumb_about").unwrap();
    let me = [("cookie", about.as_str())];
    let (_, _, body) = send_with_headers(app(), "/search?q=us+bank&country=any", &me).await;
    assert!(!body.contains("usbank-login-help.com</span>"), "{body}");
    let response = post(Some(&about), "clear=1").await.unwrap();
    let cleared = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("plumb_about="))
        .unwrap()
        .to_owned();
    assert!(cleared.contains("Max-Age=0"), "{cleared}");

    // A cookie the browser changed is cleaned, and junk is no profile.
    let (code, _, _) =
        send_with_headers(app(), "/search?q=us+bank", &[("cookie", "plumb_about=!!")]).await;
    assert_eq!(code, StatusCode::OK);
}

#[tokio::test]
async fn kinds_of_results_are_kept_in_the_browser_and_shown_on_the_about_page() {
    let app = || {
        node_router(
            backend(bank_hits()),
            node(node_status(Phase::Ready, Step::Idle)),
        )
    };
    let request = Request::builder()
        .method("POST")
        .uri("/about")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            "interests=&kind-podcasts=off&kind-music=more&kind-papers=normal&kind-junk=off",
        ))
        .unwrap();
    let response = app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let about = set_cookie(response.headers(), "plumb_about").expect("kept in the browser");
    let me = [("cookie", about.as_str())];
    let (_, _, page) = send_with_headers(app(), "/about", &me).await;
    assert!(
        page.contains("name=\"kind-podcasts\" value=\"off\" checked"),
        "{page}"
    );
    assert!(page.contains("name=\"kind-music\" value=\"more\" checked"));
    assert!(page.contains("name=\"kind-papers\" value=\"normal\" checked"));
    assert!(!page.contains("kind-junk"));
    // The gear links to them.
    let (_, _, body) = send_with_headers(app(), "/search?q=us+bank", &me).await;
    assert!(body.contains("href=\"/about#kinds\""), "{body}");
}

#[tokio::test]
async fn new_searchers_are_invited_to_the_welcome_page_until_they_answer() {
    let dir = tempfile::tempdir().unwrap();
    let node = Arc::new(HistoryNode(dir.path().join("history")));
    let fake = backend(bank_hits());
    let app = || node_router(fake.clone(), node.clone());
    let post = |uri: &'static str, cookie: Option<&str>, form: &'static str| {
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        app().oneshot(request.body(Body::from(form)).unwrap())
    };
    let (_, _, home) = send(app(), "/").await;
    assert!(home.contains("href=\"/welcome\""), "{home}");
    let (code, _, page) = send(app(), "/welcome").await;
    assert_eq!(code, StatusCode::OK);
    assert!(page.contains("name=\"town\""), "{page}");
    assert!(page.contains("value=\"cooking\""), "{page}");

    // No thanks: the invitation goes.
    let response = post("/welcome/skip", None, "").await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let welcomed = set_cookie(response.headers(), "plumb_welcome").unwrap();
    let (_, _, home) = send_with_headers(app(), "/", &[("cookie", welcomed.as_str())]).await;
    assert!(!home.contains("href=\"/welcome\""), "{home}");

    // Answering saves the About profile, ticked topics and all, and
    // goes on to search; topics not offered are not taken from `t`.
    let response = post(
        "/welcome",
        None,
        "welcome=1&town=Denver%2C+CO&t=cooking&t=music&t=evil&more=my+team&pinned=usbank-login-help.com",
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let profile = set_cookie(response.headers(), "plumb_profile").unwrap();
    assert!(set_cookie(response.headers(), "plumb_welcome").is_some());
    let (_, _, page) = send_with_headers(app(), "/about", &[("cookie", profile.as_str())]).await;
    assert!(page.contains("value=\"cooking\" checked"), "{page}");
    assert!(page.contains("value=\"music\" checked"), "{page}");
    assert!(page.contains(">my team</textarea>"), "{page}");
    assert!(page.contains("value=\"Denver, CO\""), "{page}");
    let (_, _, home) = send_with_headers(app(), "/", &[("cookie", profile.as_str())]).await;
    assert!(!home.contains("href=\"/welcome\""), "{home}");
}

struct IconNode;

impl StatusSource for IconNode {
    fn status(&self) -> Status {
        node_status(Phase::Ready, Step::Idle)
    }
    fn icon(&self, domain: &str) -> Option<Vec<u8>> {
        (domain == "usbank.com").then(|| b"\x89PNG".to_vec())
    }
}

#[tokio::test]
async fn a_node_puts_the_icons_it_has_into_the_page() {
    let app = node_router(backend(bank_hits()), Arc::new(IconNode));
    let (code, _, body) = send(app, "/search?q=us+bank").await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        body.contains("<img src=\"data:image/png;base64,iVBORw==\""),
        "{body}"
    );
    assert_eq!(body.matches("<img ").count(), 2, "logo and the site's icon");
}
