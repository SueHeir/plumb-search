//! Plugins on results pages: their results, buttons, offers and the
//! denylist over them.

use super::*;

#[tokio::test]
async fn plugins_mark_up_and_leave_out_the_nodes_own_results() {
    // Results are shown to plugins sites first: usbank.com is 0, the
    // look-alike 1.
    let plugins = crate::plugins::annotating_plugins(
        "Shelf",
        r#"{"notes":[{"id":0,"badge":"On <your> shelf"},{"id":1,"hide":true}]}"#,
    );
    let app = router_with(
        backend(bank_hits()),
        WebSettings {
            home: HomeCountry::Off,
            plugins,
            ..WebSettings::default()
        },
    );
    let (_, _, page) = send(app.clone(), "/search?q=us+bank").await;
    assert!(page.contains("Shelf: On &lt;your&gt; shelf"), "{page}");
    assert!(!page.contains("usbank-login-help.com"));
    // Not on this computer, so no buttons.
    assert!(!page.contains("/plugins/act"));
    let (_, _, body) = send(app, "/api/search?q=us+bank&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json["plugin_notes"]["https://www.usbank.com/"][0]["badge"],
        "On <your> shelf"
    );
    assert_eq!(
        json["plugin_notes"]["https://usbank-login-help.com/"][0]["hide"],
        true
    );
}

#[tokio::test]
async fn plugin_results_show_on_the_page_and_in_json_but_only_when_asked() {
    let plugins = crate::plugins::answering_plugins(
        "Test News",
        "tn",
        r#"{"results":[{"title":"Story <b>","url":"https://news.example.com/1","snippet":"Hot take"}]}"#,
    );
    let app = router_with(
        backend(bank_hits()),
        WebSettings {
            home: HomeCountry::Off,
            plugins,
            ..WebSettings::default()
        },
    );
    let (_, _, page) = send(app.clone(), "/search?q=tn+us+bank").await;
    let block = page
        .find("<li class=\"news plugin\">")
        .expect("the plugin's block");
    assert!(
        page[..block].contains("usbank.com"),
        "after the first result"
    );
    assert!(page[block..].contains("From Test News"));
    assert!(page[block..].contains("Story &lt;b&gt;"));
    assert!(page[block..].contains("href=\"https://news.example.com/1\""));
    // Its keyword picks it.
    let (_, _, page) = send(app.clone(), "/search?q=us+bank").await;
    assert!(!page.contains("From Test News"));

    let (_, _, body) = send(app.clone(), "/api/search?q=tn+us+bank&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["plugins"][0]["name"], "Test News");
    assert_eq!(json["plugins"][0]["results"][0]["site"], "example.com");

    let (_, _, body) = send(app, "/search?q=tn+us+bank&format=json").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let results = json["results"].as_array().unwrap();
    assert_eq!(results[1]["url"], "https://news.example.com/1");
    assert_eq!(results[1]["engine"], "test-news");
    assert_eq!(results[0]["engine"], "plumb");
}

/// A ready node whose denylist takes off the pages listed.
struct DenyingNode(&'static [&'static str]);

impl StatusSource for DenyingNode {
    fn status(&self) -> Status {
        node_status(Phase::Ready, Step::Idle)
    }
    fn denies(&self, url: &str) -> bool {
        self.0.contains(&url)
    }
}

#[tokio::test]
async fn the_denylist_takes_plugin_results_off_too() {
    let output = r#"{"results":[
        {"title":"Kept","url":"https://news.example.com/1","snippet":"One"},
        {"title":"Denied","url":"https://news.example.com/2","snippet":"Two"}]}"#;
    let app = |denied: &'static [&'static str]| {
        node_router_with(
            backend(bank_hits()),
            Arc::new(DenyingNode(denied)),
            WebSettings {
                home: HomeCountry::Off,
                plugins: crate::plugins::answering_plugins("Test News", "tn", output),
                ..WebSettings::default()
            },
        )
    };
    let one = app(&["https://news.example.com/2"]);
    let (_, _, page) = send(one.clone(), "/search?q=tn+us+bank").await;
    assert!(page.contains("href=\"https://news.example.com/1\""));
    assert!(!page.contains("news.example.com/2"));
    let (_, _, body) = send(one.clone(), "/api/search?q=tn+us+bank&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let results = json["plugins"][0]["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["url"], "https://news.example.com/1");
    let (_, _, body) = send(one, "/search?q=tn+us+bank&format=json").await;
    assert!(!body.contains("news.example.com/2"));

    // A plugin left with nothing to show is left out.
    let both = app(&["https://news.example.com/1", "https://news.example.com/2"]);
    let (_, _, page) = send(both.clone(), "/search?q=tn+us+bank").await;
    assert!(!page.contains("From Test News"));
    let (_, _, body) = send(both, "/api/search?q=tn+us+bank&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(json["plugins"].as_array().is_none_or(Vec::is_empty));
}

#[tokio::test]
async fn a_button_on_a_results_page_can_be_pressed_on_the_nodes_computer() {
    let plugins = crate::plugins::answering_plugins("Shelf", "sh", r#"{"message":"Saved."}"#);
    let token = plugins.token().to_string();
    let app = router_with(
        backend(bank_hits()),
        WebSettings {
            home: HomeCountry::Off,
            plugins,
            ..WebSettings::default()
        },
    );
    let press = |peer: &str, headers: &[(&str, &str)]| {
        let mut request = Request::post("/plugins/act")
            .header(header::HOST, "127.0.0.1:7586")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let body = format!("plugin=shelf&data=%7B%7D&token={token}&q=sh+us+bank");
        let mut request = request.body(Body::from(body)).unwrap();
        let peer: std::net::SocketAddr = peer.parse().unwrap();
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
        let app = app.clone();
        async move {
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8(body.to_vec()).unwrap())
        }
    };
    // The results page's `no-referrer` policy makes the browser send
    // `Origin: null` with its own form.
    let own_page = [("origin", "null"), ("sec-fetch-site", "same-origin")];
    let (status, body) = press("127.0.0.1:50000", &own_page).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Saved."), "{body}");
    // Not from another site, a page the browser can't place, or
    // another computer.
    for headers in [
        &[("origin", "null"), ("sec-fetch-site", "cross-site")][..],
        &[("origin", "null")][..],
        &[
            ("origin", "http://evil.example"),
            ("sec-fetch-site", "same-origin"),
        ][..],
    ] {
        let (status, _) = press("127.0.0.1:50000", headers).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}");
    }
    let (status, _) = press("203.0.113.9:50000", &own_page).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_plugin_a_search_fits_can_be_offered_behind_a_link() {
    let plugins = crate::plugins::offering_plugins(
        "Test News",
        "bank",
        r#"{"results":[{"title":"Story","url":"https://news.example.com/1"}]}"#,
    );
    let app = router_with(
        backend(bank_hits()),
        WebSettings {
            home: HomeCountry::Off,
            plugins,
            ..WebSettings::default()
        },
    );
    let (_, _, page) = send(app.clone(), "/search?q=us+bank").await;
    assert!(!page.contains("From Test News"), "not run");
    assert!(page.contains(
        "<a href=\"/search?q=us+bank&amp;country=any&amp;lang=en&amp;run=test-news\">Show results from Test News</a>"
    ));
    let (_, _, page) = send(app.clone(), "/search?q=us+bank&run=test-news").await;
    assert!(page.contains("From Test News"));
    assert!(!page.contains("Show results from"));
    let (_, _, body) = send(app, "/api/search?q=us+bank&full=1").await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["plugin_offers"][0]["plugin"], "test-news");
    assert!(json.get("plugins").is_none());
}

/// Finds `hits` and `pages`, and knows "creep" is Radiohead's song.
struct SongBackend {
    hits: Vec<Hit>,
    pages: Vec<plumb_index::pages::PlacedPage>,
}

impl SearchBackend for SongBackend {
    fn search(&self, _query: &str, limit: usize) -> Result<Vec<Hit>> {
        Ok(self.hits.iter().take(limit).cloned().collect())
    }

    fn search_full(
        &self,
        query: &str,
        limit: usize,
        _options: &SearchOptions,
    ) -> Result<SearchResults> {
        Ok(SearchResults {
            hits: self.search(query, limit)?,
            pages: self.pages.clone(),
            site_search: None,
            spelling: None,
            meant: Vec::new(),
            sites_ordered: false,
        })
    }

    fn num_docs(&self) -> u64 {
        1
    }

    fn known_song(
        &self,
        query: &str,
        _options: &SearchOptions,
    ) -> Option<plumb_index::pages::Page> {
        (query == "creep").then(|| {
            plumb_index::pages::Page::from_music(plumb_core::Article {
                title: "Creep".into(),
                description: Some("Song by Radiohead, 1992".into()),
                item: Some("recording/b1a9c0e9-d987-4042-ae91-78d6a3267d69".into()),
                views: 288_000,
                ..Default::default()
            })
            .unwrap()
        })
    }
}

#[tokio::test]
async fn a_songs_title_alone_runs_a_plugin_for_songs() {
    let manifest = crate::plugins::Manifest {
        name: "Songs".into(),
        hosts: vec!["a.example".into()],
        ids: vec!["musicbrainz-recording".into()],
        run_ids: vec!["musicbrainz-recording".into()],
        suggest: crate::plugins::Suggest::Button,
        ..crate::plugins::Manifest::default()
    };
    let plugins = || {
        crate::plugins::Plugins::new(vec![crate::plugins::Plugin::from_parts(
            "songs".into(),
            manifest.clone(),
            serde_json::Value::Null,
            crate::plugins::answering(
                r#"{"results":[{"title":"Radiohead - Creep","url":"https://a.example/creep"}]}"#,
            )
            .as_bytes(),
        )
        .unwrap()])
    };
    let app_with = |hit: Hit, pages: Vec<plumb_index::pages::PlacedPage>| {
        router_with(
            Arc::new(SongBackend {
                hits: vec![hit],
                pages,
            }),
            WebSettings {
                home: HomeCountry::Off,
                plugins: plugins(),
                ..WebSettings::default()
            },
        )
    };
    let app = |hit: Hit| app_with(hit, Vec::new());
    let article = |title: &str, description: &str| plumb_index::pages::PlacedPage {
        hit: PageHit {
            page: plumb_index::pages::Page::from_article(
                "en",
                plumb_core::Article {
                    title: title.into(),
                    description: Some(description.into()),
                    views: 10_000,
                    ..Default::default()
                },
            ),
            score: 1.0,
            named: true,
            popularity: 0.5,
            whole: false,
            learned: None,
            meant: false,
        },
        under: None,
        at: 0,
    };
    let (_, _, page) = send(app(scored("creed.com", 0.5)), "/search?q=creep").await;
    assert!(page.contains("Radiohead - Creep"), "{page}");
    // A site found for a near spelling, named by its own name, and an
    // article on a namesake do not stand in its way.
    let mut creed = hit(
        "creed.com",
        "https://creed.com/",
        Some("Creed - Official Website"),
        None,
    );
    (creed.named, creed.official) = (true, true);
    let film = article("Creep (2014 film)", "2014 film by Patrick Brice");
    let (_, _, page) = send(app_with(creed, vec![film]), "/search?q=creep").await;
    assert!(page.contains("Radiohead - Creep"), "{page}");
    // Not when the search is a site's name, or an article's.
    let mut named = scored("creep.example", 0.5);
    named.named = true;
    let (_, _, page) = send(app(named), "/search?q=creep").await;
    assert!(!page.contains("Radiohead - Creep"));
    let creep = article("Creep", "Person who makes others uneasy");
    let (_, _, page) = send(
        app_with(scored("creed.com", 0.5), vec![creep]),
        "/search?q=creep",
    )
    .await;
    assert!(!page.contains("Radiohead - Creep"));
    let (_, _, page) = send(app(scored("creed.com", 0.5)), "/search?q=weather").await;
    assert!(!page.contains("Radiohead - Creep"));
}
