//! The home page, results pages and the JSON API: what they list and how
//! they escape it.

use super::*;

#[tokio::test]
async fn answers_searxng_json_for_ai_apps() {
    let app = router_with(backend(bank_hits()), HomeCountry::Off);
    let (status, headers, body) = send(
        app.clone(),
        "/search?q=us+bank&format=json&pageno=1&safesearch=0",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("application/json"));
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["query"], "us bank");
    assert_eq!(body["number_of_results"], 2);
    let first = &body["results"][0];
    assert_eq!(first["url"], "https://www.usbank.com/");
    assert_eq!(first["title"], "U.S. Bank | Personal & Business Banking");
    assert_eq!(first["content"], "Checking, savings & loans.");
    assert_eq!(first["engine"], "plumb");
    assert_eq!(first["parsed_url"][1], "www.usbank.com");
    // A site without a title goes by its domain.
    assert_eq!(body["results"][1]["title"], "usbank-login-help.com");

    // The second page.
    let (_, _, body) = send(
        app.clone(),
        "/search?q=us+bank&format=json&pageno=2&limit=1",
    )
    .await;
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    assert_eq!(body["results"][0]["url"], "https://usbank-login-help.com/");

    // A page past the last one is empty, not the last one again, so a
    // client paging until empty stops.
    let (_, _, body) = send(app.clone(), "/search?q=us+bank&format=json&pageno=11").await;
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["results"].as_array().unwrap().len(), 0);

    // Instant answers go where SearXNG puts its own.
    let (_, _, body) = send(app.clone(), "/search?q=12*7&format=json").await;
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["answers"][0]["answer"], "12 × 7 = 84");
    // And leads the first snippet, for apps that read only snippets.
    assert!(body["results"][0]["content"]
        .as_str()
        .unwrap()
        .starts_with("Answer: 12 × 7 = 84. Checking"));

    // News only: this server keeps no headlines.
    let (_, _, body) = send(app.clone(), "/search?q=us+bank&format=json&categories=news").await;
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["results"], serde_json::json!([]));

    // Open WebUI's external search engine.
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/websearch")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"us bank","count":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body,
        serde_json::json!([{
            "link": "https://www.usbank.com/",
            "title": "U.S. Bank | Personal & Business Banking",
            "snippet": "Checking, savings & loans.",
        }])
    );

    // A bang stays a search: an AI app wants results, not a redirect.
    let (status, _, _) = send(app, "/search?q=!g+rust&format=json").await;
    assert_eq!(status, StatusCode::OK);
}

#[test]
fn escapes_html() {
    assert_eq!(
        escape_html(r#"<a href="x">Tom & 'Jerry'</a>"#),
        "&lt;a href=&quot;x&quot;&gt;Tom &amp; &#39;Jerry&#39;&lt;/a&gt;"
    );
    assert_eq!(escape_html("plain text"), "plain text");
}

#[test]
fn links_only_to_http_urls() {
    let h = |domain: &str, url: &str| hit(domain, url, None, None);
    assert_eq!(
        safe_href(&h("usbank.com", "https://www.usbank.com/?a=1&b=2")).as_deref(),
        Some("https://www.usbank.com/?a=1&b=2")
    );
    assert_eq!(
        safe_href(&h("example.com", "HTTP://Example.com")).as_deref(),
        Some("http://example.com/")
    );
    for bad in [
        "javascript:alert(1)",
        " JavaScript:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "file:///etc/passwd",
        "//evil.com/",
        "not a url",
    ] {
        assert_eq!(
            safe_href(&h("evil-example.com", bad)).as_deref(),
            Some("https://evil-example.com/"),
            "{bad}"
        );
    }
    assert_eq!(safe_href(&h("evil.com/\"><b>", "javascript:x")), None);
    assert_eq!(safe_href(&h("evil.com:8080", "javascript:x")), None);
    assert_eq!(safe_href(&h("", "javascript:x")), None);
}

#[test]
fn groups_thousands() {
    assert_eq!(group_thousands(0), "0");
    assert_eq!(group_thousands(999), "999");
    assert_eq!(group_thousands(1_000), "1,000");
    assert_eq!(group_thousands(12_345_678), "12,345,678");
}

#[tokio::test]
async fn bare_sites_go_last_for_near_me() {
    let mut hits = bank_hits();
    hits.reverse();
    let (_, _, body) = get(backend(hits), "/api/search?q=us+bank+near+me").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json[0]["domain"], "usbank.com", "{body}");
    assert_eq!(json[1]["domain"], "usbank-login-help.com", "{body}");
}

#[tokio::test]
async fn sites_are_searched_without_near_me() {
    let fake = backend(bank_hits());
    let _ = get(fake.clone(), "/search?q=us+bank+near+me").await;
    let _ = get(fake.clone(), "/api/search?q=us+bank+near+me&full=1").await;
    let calls = fake.calls.lock().unwrap();
    assert!(calls.iter().all(|(q, _)| q == "us bank"), "{calls:?}");
}

#[tokio::test]
async fn home_page_has_a_search_box() {
    let (status, headers, body) = get(backend(Vec::new()), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    assert!(headers[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .contains("default-src 'none'"));
    assert!(body.contains("<form action=\"/search\""), "{body}");
    assert!(body.contains("name=\"viewport\""));
    assert!(body.contains("12,345 items indexed"));
}

#[tokio::test]
async fn search_page_lists_hits() {
    let fake = backend(bank_hits());
    let (status, _, body) = send(
        router_with(fake.clone(), HomeCountry::Off),
        "/search?q=us+bank",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(
            "<a class=\"r\" href=\"https://www.usbank.com/\" rel=\"noreferrer\">\
             <span class=\"site\"><span class=\"ic l"
        ),
        "{body}"
    );
    assert!(body.contains(
        "<span class=\"dn\">usbank.com</span><span class=\"u\">www.usbank.com</span>\
         </span></span><span class=\"t\">U.S. Bank | Personal &amp; Business Banking</span></a>"
    ));
    assert!(body.contains("<p class=\"d\">Checking, savings &amp; loans.</p>"));
    // A hit without a title is shown by its domain.
    assert!(body.contains("<span class=\"t\">usbank-login-help.com</span></a>"));
    // Without an icon, a site gets its first letter.
    assert!(body.contains("aria-hidden=\"true\">U</span>"));
    assert!(body.contains("value=\"us bank\""));
    assert!(!body.contains("As JSON:"));
    let (_, _, debug) = get(fake.clone(), "/search?q=us+bank&debug=1").await;
    assert!(
        debug.contains("href=\"/api/search?q=us+bank&amp;country=auto&amp;lang=en&amp;debug=1\"")
    );
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec![("us bank".to_string(), DEFAULT_LIMIT); 2]
    );
}

#[tokio::test]
async fn the_site_searched_for_lists_its_key_pages() {
    let page = |label: &str, url: &str| KeyPage {
        label: label.into(),
        url: url.into(),
    };
    let mut paypal = hit(
        "paypal.com",
        "https://www.paypal.com/",
        Some("PayPal"),
        None,
    );
    paypal.named = true;
    paypal.key_pages = vec![
        page("Sign Up", "https://www.paypal.com/signup"),
        page("Log In", "https://www.paypal.com/signin"),
        page("Help", "https://www.paypal.com/help"),
        page("Phish", "https://paypal-login.example/"),
    ];
    let mut other = hit(
        "paypal-login.example",
        "https://paypal-login.example/",
        None,
        None,
    );
    other.key_pages = paypal.key_pages.clone();
    let fake = backend(vec![paypal.clone(), other]);
    let search = |q: &'static str| {
        let fake = fake.clone();
        async move { send(router_with(fake, HomeCountry::Off), q).await.2 }
    };

    let body = search("/search?q=paypal").await;
    assert!(
        body.contains(
            "<ul class=\"kp\"><li><a href=\"https://www.paypal.com/signup\" \
             rel=\"noreferrer\">Sign Up</a></li><li><a href=\"https://www.paypal.com/signin\" \
             rel=\"noreferrer\">Log In</a></li><li><a href=\"https://www.paypal.com/help\" \
             rel=\"noreferrer\">Help</a></li></ul>"
        ),
        "{body}"
    );
    assert_eq!(
        body.matches("class=\"kp\"").count(),
        1,
        "only the top result"
    );
    assert!(!body.contains("paypal-login.example/\" rel=\"noreferrer\">Phish"));

    // What the query asks for comes first.
    let body = search("/search?q=paypal+login").await;
    assert!(body.contains(
        "<ul class=\"kp\"><li><a href=\"https://www.paypal.com/signin\" \
         rel=\"noreferrer\"><strong>Log In</strong></a></li>"
    ));

    // Not for a top result the query does not name.
    paypal.named = false;
    let body = send(
        router_with(backend(vec![paypal]), HomeCountry::Off),
        "/search?q=pay",
    )
    .await
    .2;
    assert!(!body.contains("class=\"kp\""));
}

#[test]
fn papers_link_their_free_copy() {
    use plumb_core::article::Article;
    use plumb_index::pages::Page;
    let hit = |website: Option<&str>| PageHit {
        page: Page::from_paper(Article {
            title: "Superconductivity at 39 K in magnesium diboride".into(),
            description: Some("Paper by Jun Nagamatsu et al., 2001, Nature".into()),
            item: Some("10.1038/35065039".into()),
            website: website.map(str::to_string),
            views: 9000,
            ..Article::default()
        }),
        score: 1.0,
        named: true,
        popularity: 0.5,
        whole: false,
        learned: None,
    };
    let mut out = String::new();
    render_page(
        &mut out,
        &hit(Some("https://arxiv.org/abs/cond-mat/0101295")),
        None,
    );
    assert!(out.contains(
        "<a href=\"https://arxiv.org/abs/cond-mat/0101295\" rel=\"noreferrer\">Free to read</a> on arxiv.org"
    ));
    let mut out = String::new();
    render_page(&mut out, &hit(None), None);
    assert!(!out.contains("Free to read"));
}

#[test]
fn paper_dates_and_count_meanings_render_separately() {
    let metadata = plumb_core::papers::PaperMetadata {
        publication_date: Some("2024-07-01".into()),
        preprint_date: Some("2023-01-02".into()),
        preprint_version_date: Some("2025-03-04".into()),
        count: 12,
        count_kind: plumb_core::papers::PaperCountKind::MethodUses,
        ..Default::default()
    };
    let mut hit = PageHit {
        page: plumb_index::pages::Page::from_paper(plumb_core::article::Article {
            title: "A <paper>".into(),
            item: Some("10.1234/a".into()),
            paper: Some(metadata),
            ..Default::default()
        }),
        score: 1.0,
        popularity: 0.0,
        named: true,
        whole: true,
        learned: None,
    };
    let mut out = String::new();
    render_page(&mut out, &hit, None);
    assert!(out.contains("A &lt;paper&gt;"));
    assert!(out.contains("Published 2024-07-01"));
    assert!(out.contains("Preprint 2023-01-02"));
    assert!(out.contains("Preprint revised 2025-03-04"));
    assert!(out.contains("12 method uses"));
    assert!(!out.contains("citations"));
    hit.page.paper = None;
    out.clear();
    render_page(&mut out, &hit, None);
    assert!(out.contains("Published unknown"));
    assert!(out.contains("count type unknown"));
}

#[tokio::test]
async fn paper_limit_retry_submits_the_original_publication_constraints() {
    #[derive(Default)]
    struct Papers(Mutex<Vec<plumb_core::paper_query::PaperQuery>>);
    impl SearchBackend for Papers {
        fn search(&self, _: &str, _: usize) -> Result<Vec<Hit>> {
            panic!("a paper retry must not become ordinary site search");
        }
        fn papers(
            &self,
            query: &plumb_core::paper_query::PaperQuery,
            _: usize,
            _: &SearchOptions,
        ) -> Result<Vec<PageHit>> {
            self.0.lock().unwrap().push(query.clone());
            Ok(Vec::new())
        }
        fn num_docs(&self) -> u64 {
            1
        }
    }
    let backend = Arc::new(Papers::default());
    let app = node_router(backend.clone(), node(node_status(Phase::Ready, Step::Idle)));
    let (status, _, body) = send(app.clone(), "/search?q=transformer&kind=paper&after=2020-02-29&before=2024&order=newest&limit=invalid&country=DE&lang=de&safe=strict").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(backend.0.lock().unwrap().is_empty());
    // Read the actual retry form's hidden controls, as a browser would.
    let mut retry = url::form_urlencoded::Serializer::new(String::new());
    retry.append_pair("q", "transformer");
    let mut count = 0;
    for input in body.split("<input type=\"hidden\" name=\"").skip(1) {
        let (name, rest) = input.split_once('"').unwrap();
        if ["kind", "after", "before", "order"].contains(&name) {
            let value = rest
                .split_once("value=\"")
                .unwrap()
                .1
                .split('"')
                .next()
                .unwrap();
            retry.append_pair(name, value);
            count += 1;
        }
    }
    assert_eq!(count, 4);
    let (status, _, body) = send(app, &format!("/search?{}", retry.finish())).await;
    assert_eq!(status, StatusCode::OK);
    let called = backend.0.lock().unwrap();
    assert_eq!(called.len(), 1);
    assert_eq!(called[0].query, "transformer");
    assert_eq!(called[0].after.unwrap().day, Some(20200229));
    assert_eq!(called[0].before.unwrap().year, 2024);
    assert!(called[0].newest);
    assert!(body.contains("name=\"kind\" value=\"paper\""));
    assert!(body.contains("value=\"transformer\""));
    assert!(body.contains("Paper results for transformer"));
}

#[tokio::test]
async fn recovered_paper_fields_escape_untrusted_values() {
    let (_, _, body) = get(
        backend(Vec::new()),
        "/search?q=research&kind=%22%3E%3Cscript%3E&after=%22%3E%3Cscript%3E&limit=invalid",
    )
    .await;
    assert!(body.contains("name=\"kind\" value=\"&quot;&gt;&lt;script&gt;\""));
    assert!(body.contains("name=\"after\" value=\"&quot;&gt;&lt;script&gt;\""));
    assert!(!body.contains("<script>"));
}

#[tokio::test]
async fn paper_api_validates_dates_and_reports_missing_index_coverage() {
    let fake = backend(Vec::new());
    let app = router_with(fake, HomeCountry::Off);
    let (status, _, body) = send(
        app.clone(),
        "/api/search?q=transformer&kind=paper&after=2026-02-29",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("paper date"));
    let (status, _, body) = send(
        app.clone(),
        "/api/search?q=transformer&kind=paper&after=2026&order=newest",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(data["kind"], "paper");
    assert_eq!(data["pages"], serde_json::json!([]));
    assert!(data["message"].as_str().unwrap().contains("indexed corpus"));
    let (status, _, body) = send(app, "/search?q=transformer&kind=paper&after=2026").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("does not establish"));
}

#[test]
fn plugins_are_shown_what_articles_are_about_and_mark_them_up() {
    use plumb_index::pages::{Page, PlacedPage};
    let url = "https://en.wikipedia.org/wiki/But_I%27m_a_Cheerleader";
    let article = PageHit {
        page: Page {
            paper: None,
            set: "wikipedia-en".into(),
            url: url.into(),
            title: "But I'm a Cheerleader".into(),
            description: Some("1999 film by Jamie Babbit".into()),
            site: None,
            views: 1000,
            aliases: Vec::new(),
            item: Some("Q1257599".into()),
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            content_language: None,
            search: None,
        },
        score: 1.0,
        named: true,
        popularity: 0.5,
        whole: false,
        learned: None,
    };
    let results = SearchResults {
        pages: vec![PlacedPage {
            hit: article,
            under: None,
            at: 0,
        }],
        hits: vec![scored("cheerz.com", 0.5)],
        site_search: None,
        spelling: None,
    };
    let shown = shown_results(&results, 10);
    assert_eq!(shown.len(), 2);
    assert_eq!(shown[0].site, "cheerz.com");
    assert!(shown[0].about.is_none());
    assert_eq!(shown[1].url, url);
    let about = shown[1].about.as_ref().unwrap();
    assert_eq!(about.wikidata.as_deref(), Some("Q1257599"));
    assert_eq!(
        about.description.as_deref(),
        Some("1999 film by Jamie Babbit")
    );

    let mut extras = answers::Extras {
        plugin_token: Some("t0k".into()),
        ..answers::Extras::default()
    };
    extras.plugin_notes.insert(
        url.into(),
        vec![crate::plugins::ResultNote {
            plugin: "shelf".into(),
            name: "Shelf".into(),
            badge: Some("Not in library".into()),
            actions: vec![crate::plugins::PluginAction {
                label: "Add".into(),
                data: r#"{"id":1}"#.into(),
            }],
            hide: false,
        }],
    );
    let render = |extras: &answers::Extras| {
        render_results(
            "but i'm a cheerleader",
            &results,
            Some(extras),
            &NetOutcome::NotAsked,
            &no_settings(),
            None,
            10,
            false,
            &Icons::default(),
        )
    };
    let page = render(&extras);
    let article = page.find("<li class=\"pg\">").expect("the article");
    let marked = &page[article..];
    assert!(marked.contains("Shelf: Not in library"), "{page}");
    assert!(marked.contains("name=\"data\" value=\"{&quot;id&quot;:1}\""));
    assert!(marked.contains("<button type=\"submit\">Add</button>"));
    // A note's plugin did not run as a search: nothing to run again.
    assert!(!marked.contains("name=\"run\""));
    let actions = [crate::plugins::PluginAction {
        label: "Add".into(),
        data: "1".into(),
    }];
    assert!(action_forms("shelf", &actions, "t0k", "q", true)
        .contains("<input type=\"hidden\" name=\"run\" value=\"shelf\">"));
    // Hidden, it is gone.
    extras.plugin_notes.get_mut(url).unwrap()[0].hide = true;
    let page = render(&extras);
    assert!(!page.contains("<li class=\"pg\">"));
    assert!(page.contains("cheerz.com"));
}

#[test]
fn the_part_of_a_site_the_query_names_comes_first() {
    use plumb_index::pages::Page;
    let article = PageHit {
        page: Page {
            paper: None,
            set: "wikipedia-en".into(),
            url: "https://en.wikipedia.org/wiki/YouTube_Music".into(),
            title: "YouTube Music".into(),
            description: Some("Music streaming service".into()),
            site: Some("youtube.com".into()),
            views: 1000,
            aliases: vec!["YT Music".into()],
            item: Some("Q28404534".into()),
            profiles: Vec::new(),
            website: Some("https://music.youtube.com/".into()),
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            content_language: None,
            search: None,
        },
        score: 1.0,
        named: true,
        popularity: 0.5,
        whole: false,
        learned: None,
    };
    let youtube = hit(
        "youtube.com",
        "https://www.youtube.com/",
        Some("YouTube"),
        None,
    );
    for query in ["youtube music", "Music YouTube", "yt music"] {
        let product = product_of(&article, "youtube.com", query).unwrap();
        assert_eq!(product.url, "https://music.youtube.com/");
        // Under any result, named in full or not.
        assert_eq!(
            key_pages_line(&youtube, query, Some(&product)),
            "<ul class=\"kp\"><li><a href=\"https://music.youtube.com/\" \
             rel=\"noreferrer\"><strong>YouTube Music</strong></a></li></ul>"
        );
    }
    // Not for other words, nor on another site.
    assert!(product_of(&article, "youtube.com", "youtube").is_none());
    assert!(product_of(&article, "youtube.com", "youtube music charts").is_none());
    assert!(product_of(&article, "you-tubemusic.com", "youtube music").is_none());
    let mut elsewhere = article.clone();
    elsewhere.page.website = Some("https://youtubemusic.example/".into());
    assert!(product_of(&elsewhere, "youtube.com", "youtube music").is_none());
}

#[tokio::test]
async fn record_text_is_escaped() {
    let evil = hit(
        "evil-example.com",
        "javascript:alert(1)",
        Some("<script>alert('title')</script> Evil & Co"),
        Some("<img src=x onerror=alert(1)> \"quoted\""),
    );
    let (status, _, body) = get(backend(vec![evil]), "/search?q=%3Cb%3Eevil%3C%2Fb%3E").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("<script>"), "{body}");
    assert!(!body.contains("<img src=x"), "{body}");
    assert_eq!(body.matches("<img ").count(), 1, "only the bundled logo");
    assert!(!body.contains("<b>evil"), "{body}");
    assert!(!body.contains("javascript:"), "{body}");
    assert!(body.contains("&lt;script&gt;alert(&#39;title&#39;)&lt;/script&gt; Evil &amp; Co"));
    assert!(body.contains("&lt;img src=x onerror=alert(1)&gt; &quot;quoted&quot;"));
    assert!(body.contains("value=\"&lt;b&gt;evil&lt;/b&gt;\""));
    assert!(body.contains("href=\"https://evil-example.com/\""));
}

#[tokio::test]
async fn empty_query_shows_the_home_page() {
    let fake = backend(bank_hits());
    let (status, _, body) = get(Arc::clone(&fake), "/search?q=+++").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("items indexed"));
    assert!(fake.calls.lock().unwrap().is_empty());
    let (_, _, body) = get(Arc::clone(&fake), "/search").await;
    assert!(body.contains("items indexed"));
}

#[tokio::test]
async fn no_hits_message() {
    let (status, _, body) = get(backend(Vec::new()), "/search?q=zzz").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No sites match <strong>zzz</strong>."));
}

#[tokio::test]
async fn api_returns_hits_as_json() {
    let fake = backend(bank_hits());
    let (status, headers, body) = get(Arc::clone(&fake), "/api/search?q=us%20bank&limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    let hits: Vec<Hit> = serde_json::from_str(&body).unwrap();
    assert_eq!(hits, bank_hits()[..1].to_vec());

    let (_, _, body) = get(Arc::clone(&fake), "/api/search?q=x&limit=100000").await;
    assert_eq!(serde_json::from_str::<Vec<Hit>>(&body).unwrap().len(), 2);
    let (_, _, body) = get(Arc::clone(&fake), "/api/search?q=x&limit=0").await;
    assert_eq!(body, "[]");
    let (_, _, body) = get(Arc::clone(&fake), "/api/search").await;
    assert_eq!(body, "[]");
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec![("us bank".to_string(), 1), ("x".to_string(), MAX_LIMIT)]
    );
}

#[tokio::test]
async fn bad_limit_is_a_client_error() {
    let (status, _, _) = get(backend(Vec::new()), "/api/search?q=x&limit=lots").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn invalid_limit_keeps_search_preferences_and_does_not_search() {
    let fake = backend(Vec::new());
    for path in ["/search", "/network"] {
        let (status, _, body) = send(node_router(fake.clone(), node(node_status(Phase::Ready, Step::Idle))), &format!("{path}?q=a%3Cb%3E&limit=lots&country=DE&only=1&safe=strict&lang=de&news=off&exact=1&debug=1")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("value=\"a&lt;b&gt;\""));
        assert!(body.contains("<option value=\"DE\" selected>"));
        assert!(body.contains("name=\"only\" value=\"1\" checked"));
        assert!(body.contains("<option value=\"strict\" selected>"));
        assert!(body.contains("value=\"de\" lang=\"de\" selected>"));
        assert!(body.contains("name=\"exact\" value=\"1\" checked"));
        assert!(body.contains("name=\"debug\" value=\"1\" checked"));
        assert!(!body.contains("<script>"));
    }
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn backend_errors_become_500s() {
    let failing = Arc::new(FakeBackend {
        fail: true,
        ..FakeBackend::default()
    });
    let (status, _, body) = get(Arc::clone(&failing), "/search?q=x").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("Search could not finish"));
    assert!(!body.contains("index is broken"));
    let (status, _, body) = get(failing, "/api/search?q=x").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, r#"{"error":"search failed"}"#);

    let panicking = Arc::new(FakeBackend {
        panic: true,
        ..FakeBackend::default()
    });
    let (status, _, _) = get(panicking, "/search?q=x").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}
