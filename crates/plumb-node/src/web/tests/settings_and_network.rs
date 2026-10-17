//! Country, language and safe search settings, site search links, and
//! the network's results merged with this node's.

use super::*;

/// Answers with one hit and a site search link, and remembers the options.
#[derive(Default)]
struct OptionsBackend {
    link: String,
    options: Mutex<Vec<SearchOptions>>,
}

impl SearchBackend for OptionsBackend {
    fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Hit>> {
        unreachable!("pages search with options")
    }

    fn search_full(
        &self,
        query: &str,
        _limit: usize,
        options: &SearchOptions,
    ) -> Result<SearchResults> {
        self.options.lock().unwrap().push(options.clone());
        let mut github = hit("github.com", "https://github.com/", Some("GitHub"), None);
        github.country = Some("US".into());
        Ok(SearchResults {
            pages: Vec::new(),
            hits: vec![github],
            site_search: Some(SiteSearch {
                domain: "github.com".into(),
                terms: query.trim_start_matches("github ").into(),
                url: self.link.clone(),
            }),
            spelling: None,
            meant: None,
            sites_ordered: false,
        })
    }

    fn num_docs(&self) -> u64 {
        1
    }
}

fn options_backend(link: &str) -> Arc<OptionsBackend> {
    Arc::new(OptionsBackend {
        link: link.into(),
        ..OptionsBackend::default()
    })
}

#[tokio::test]
async fn pages_pick_the_country_and_link_to_site_search() {
    let fake = options_backend("https://github.com/search?q=plumb%20%3Cb%3E");
    let app = || router_with(fake.clone(), HomeCountry::Fixed("US".into()));
    let (code, _, body) = send(app(), "/search?q=github+plumb+%3Cb%3E&country=de&only=on").await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        body.contains(
            "<p class=\"ss\"><a href=\"https://github.com/search?q=plumb%20%3Cb%3E\" \
         rel=\"noreferrer\">Search github.com for <strong>plumb &lt;b&gt;</strong></a></p>"
        ),
        "{body}"
    );
    assert!(body.contains("<option value=\"DE\" selected>Germany</option>"));
    assert!(body.contains("name=\"only\" value=\"1\" checked"));
    assert!(body.contains(
        "<div class=\"m\">United States<span class=\"rank-meta\"> &middot; <span title=\"text"
    ));
    assert!(body.contains("name=\"only\" value=\"1\" checked"), "{body}");

    // No country asked for: the server's setting, then the browser's.
    send(app(), "/search?q=github").await;
    let auto = router_with(fake.clone(), HomeCountry::Auto);
    send_with_headers(
        auto,
        "/search?q=github",
        &[("accept-language", "en-GB,en;q=0.8")],
    )
    .await;
    // `any` turns the home country off, and `only` needs one.
    send(app(), "/search?q=github&country=any&only=1").await;
    // A bad code falls back to the setting.
    send(app(), "/search?q=github&country=zz").await;
    let seen: Vec<(Option<String>, bool)> = fake
        .options
        .lock()
        .unwrap()
        .iter()
        .map(|o| (o.country.clone(), o.only_country))
        .collect();
    let code = |c: &str| Some(c.to_string());
    assert_eq!(
        seen,
        [
            (code("DE"), true),
            (code("US"), false),
            (code("GB"), false),
            (None, false),
            (code("US"), false),
        ]
    );
}

#[tokio::test]
async fn a_server_can_start_its_pages_with_only_this_country() {
    let fake = options_backend("https://github.com/search?q=x");
    let app = || {
        router_with(
            fake.clone(),
            WebSettings {
                home: HomeCountry::Fixed("US".into()),
                only_country: true,
                language: Some("en".into()),
                ..WebSettings::default()
            },
        )
    };
    let german = [("accept-language", "de-DE,de;q=0.9")];
    // A new search, from a link or another site's search box: the
    // server's choices, shown in the gear.
    let (_, _, body) = send_with_headers(app(), "/search?q=github", &german).await;
    assert!(
        body.contains("<option value=\"auto\" selected>Automatic"),
        "{body}"
    );
    assert!(body.contains("name=\"only\" value=\"1\" checked"), "{body}");
    assert!(
        body.contains("<option value=\"en\" lang=\"en\" selected>"),
        "{body}"
    );
    assert!(body.contains("name=\"only\" value=\"1\" checked"), "{body}");
    let (_, _, home) = send(app(), "/").await;
    assert!(home.contains("name=\"only\" value=\"1\" checked"), "{home}");
    // The gear's form with the box unchecked names the country, and so
    // turns it off; so does `only=0`.
    send(app(), "/search?q=github&country=US&lang=en").await;
    send(app(), "/search?q=github&only=0").await;
    // A search for another country keeps that country's sites only
    // when asked.
    send(app(), "/search?q=github&country=de").await;
    // The JSON API is not a page: AI apps get every country's sites.
    send(app(), "/api/search?q=github").await;
    let seen: Vec<(Option<String>, bool, Option<String>)> = fake
        .options
        .lock()
        .unwrap()
        .iter()
        .map(|o| (o.country.clone(), o.only_country, o.language.clone()))
        .collect();
    let code = |c: &str| Some(c.to_string());
    assert_eq!(
        seen,
        [
            (code("US"), true, code("en")),
            (code("US"), false, code("en")),
            (code("US"), false, code("en")),
            (code("DE"), false, code("en")),
            (code("US"), false, code("en")),
        ]
    );
}

#[tokio::test]
async fn site_search_links_must_be_web_addresses() {
    let fake = options_backend("javascript:alert(1)");
    let (_, _, body) = send(router(fake), "/search?q=github+x").await;
    assert!(!body.contains("class=\"ss\""), "{body}");
    assert!(!body.contains("javascript:"));
}

#[tokio::test]
async fn full_api_answers_include_the_site_search() {
    let fake = options_backend("https://github.com/search?q=x");
    let (_, _, body) = send(router(fake.clone()), "/api/search?q=github+x&full=1").await;
    let results: SearchResults = serde_json::from_str(&body).unwrap();
    assert_eq!(results.hits[0].domain, "github.com");
    assert_eq!(
        results.site_search.unwrap().url,
        "https://github.com/search?q=x"
    );
    // Without `full`, the plain list as before.
    let (_, _, body) = send(router(fake), "/api/search?q=github+x").await;
    let hits: Vec<Hit> = serde_json::from_str(&body).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn long_queries_are_cut() {
    let params = SearchParams {
        q: format!("  us \n bank {}", "x".repeat(500)),
        ..SearchParams::default()
    };
    let query = params.query();
    assert!(query.starts_with("us bank x"));
    assert_eq!(query.chars().count(), MAX_QUERY_CHARS);
    assert_eq!(params.limit(), DEFAULT_LIMIT);
}

#[tokio::test]
async fn settings_sit_behind_a_gear() {
    for uri in ["/", "/search?q=us+bank&net=1"] {
        let (code, _, body) = get(backend(bank_hits()), uri).await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains("<details class=\"gear\">"), "{uri}: {body}");
        assert!(body.contains("name=\"country\""), "{uri}");
        assert!(body.contains("name=\"only\""), "{uri}");
        assert!(body.contains("<select name=\"lang\">"), "{uri}");
        assert!(
            body.contains("<option value=\"moderate\" selected>Moderate</option>"),
            "{uri}"
        );
        // `plumb serve` is in no network: the setting is off, and says why.
        assert!(
            body.contains("<input type=\"checkbox\" disabled> Use the Plumb network"),
            "{uri}"
        );
        assert!(body.contains("has not joined the Plumb network"), "{uri}");
        assert!(!body.contains("name=\"net\""), "{uri}");
    }
    let (_, _, body) = get(backend(bank_hits()), "/search?q=us+bank&net=1").await;
    assert!(body.contains("<p class=\"src\">From this site's own index.</p>"));
    assert!(!body.contains("class=\"net\""));
}

#[test]
fn english_is_the_language_unless_chosen() {
    let language_with = |server: Option<&str>, lang: Option<&str>, accept: Option<&str>| {
        let params = SearchParams {
            lang: lang.map(str::to_string),
            ..SearchParams::default()
        };
        let mut headers = HeaderMap::new();
        if let Some(accept) = accept {
            headers.insert(header::ACCEPT_LANGUAGE, accept.parse().unwrap());
        }
        let settings = WebSettings {
            home: HomeCountry::Off,
            language: server.map(str::to_string),
            ..WebSettings::default()
        };
        params.options(&settings, &headers).language
    };
    let language = |lang: Option<&str>, accept: Option<&str>| language_with(None, lang, accept);
    assert_eq!(language(None, None).as_deref(), Some("en"));
    assert_eq!(language(None, Some("*")).as_deref(), Some("en"));
    assert_eq!(
        language(None, Some("de-DE,de;q=0.9")).as_deref(),
        Some("de")
    );
    // A language the gear does not offer falls back to English.
    assert_eq!(language(None, Some("eu-ES")).as_deref(), Some("en"));
    assert_eq!(language(Some("any"), Some("de-DE")), None);
    assert_eq!(language(Some(""), None), None);
    assert_eq!(language(Some("fr"), None).as_deref(), Some("fr"));
    // A server's own language comes before the browser's, and a
    // search's own before both.
    assert_eq!(
        language_with(Some("en"), None, Some("de-DE")).as_deref(),
        Some("en")
    );
    assert_eq!(
        language_with(Some("en"), Some("fr"), Some("de-DE")).as_deref(),
        Some("fr")
    );
    assert_eq!(language_with(Some("en"), Some("any"), None), None);
}

#[tokio::test]
async fn safe_search_and_language_stay_with_the_search() {
    let (_, _, body) = get(
        backend(bank_hits()),
        "/search?q=us+bank&safe=strict&lang=de",
    )
    .await;
    assert!(body.contains("<option value=\"strict\" selected>Strict</option>"));
    assert!(body.contains("<option value=\"de\" lang=\"de\" selected>Deutsch</option>"));
    let options = SearchOptions {
        safe: SafeSearch::Off,
        language: Some("de".into()),
        ..SearchOptions::default()
    };
    assert_eq!(
        search_link("/search", "x", &options, false),
        "/search?q=x&country=any&safe=off&lang=de"
    );
    assert!(go_link("x", &options, "a.com").ends_with("&safe=off&lang=de"));
    // No country and no language filter say so, as the defaults
    // are the United States and English.
    assert_eq!(
        search_link("/search", "x", &SearchOptions::default(), false),
        "/search?q=x&country=any&lang=any"
    );
}

#[test]
fn the_network_setting_shows_its_state() {
    let mut settings = no_settings();
    settings.network = NetSetting::Off;
    let off = settings_form("x", false, &settings);
    assert!(off.contains("name=\"net\" value=\"1\"> Use the Plumb network"));
    settings.network = NetSetting::On;
    let on = settings_form("x", false, &settings);
    assert!(on.contains("name=\"net\" value=\"1\" checked> Use the Plumb network"));
    assert!(on.contains("without sending query text"));
    assert!(on.contains("Missing data may wait for a background download"));
}

#[test]
fn the_private_switch_shows_only_where_private_search_runs() {
    let mut settings = no_settings();
    let off = settings_form("x", false, &settings);
    assert!(!off.contains("Private search"), "{off}");
    settings.private = true;
    let on = settings_form("x", false, &settings);
    // A link, so turning it on never submits what was typed.
    assert!(
        on.contains("href=\"/private?country=any")
            && on.contains("role=\"switch\" aria-checked=\"false\""),
        "{on}"
    );
    assert!(!on.contains("name=\"private\""));
    assert!(private_toggle(true).contains("href=\"/\" role=\"switch\" aria-checked=\"true\""));
}

#[test]
fn a_signed_network_crawl_fills_in_this_nodes_copy() {
    // This node holds b.com without text; the network has a signed
    // crawl of it whose text matches the search better.
    let local = vec![scored("a.com", 0.9), scored("b.com", 0.3)];
    let mut signed = plumb_core::site::new("b.com");
    signed.title = Some("B Shoes".into());
    signed.about = Some("Handmade shoes".into());
    let mut network = from_network(scored("b.com", 0.95));
    network.shared = Some(signed.into());
    let unsigned = from_network(scored("a.com", 1.0));
    let network = NetOutcome::Answered(NetworkResults {
        hits: vec![network, unsigned],
        ..NetworkResults::default()
    });
    let shown = merge_results(&local, &network, 10);
    let order: Vec<(&str, bool)> = shown
        .iter()
        .map(|s| (s.hit.domain.as_str(), s.network.is_some()))
        .collect();
    // Still this node's, untinted, but with the network's text and
    // score; a.com has no signed crawl, so it keeps its own score.
    assert_eq!(order, [("b.com", false), ("a.com", false)]);
    assert_eq!(shown[0].hit.title.as_deref(), Some("B Shoes"));
    assert_eq!(shown[0].hit.description.as_deref(), Some("Handmade shoes"));
    assert!((shown[1].hit.score - 0.9).abs() < f32::EPSILON);

    // Text this node has stays.
    let mut own = scored("b.com", 0.3);
    own.description = Some("Our own words".into());
    let shown = merge_results(std::slice::from_ref(&own), &network, 10);
    let b = shown.iter().find(|s| s.hit.domain == "b.com").unwrap();
    assert_eq!(b.hit.description.as_deref(), Some("Our own words"));
    assert_eq!(b.hit.title.as_deref(), Some("B Shoes"));
}

#[test]
fn network_only_sites_lacking_main_words_come_after_this_nodes() {
    // "weather in denver": the network's only bucket at hand was that
    // of "in", whose best match scored as if it had every word.
    let partial = |domain: &str, score: f32| {
        let mut hit = scored(domain, score);
        hit.missing_words = true;
        hit
    };
    let local = vec![scored("denverpost.com", 0.65), partial("krain88.com", 0.55)];
    let network = answered(vec![
        partial("seg-social.es", 0.66),
        scored("denverweather.example", 0.6),
        partial("heybakeries.com", 0.62),
    ]);
    let order =
        |shown: &[Shown]| -> Vec<String> { shown.iter().map(|s| s.hit.domain.clone()).collect() };
    assert_eq!(
        order(&merge_results(&local, &network, 10)),
        [
            "denverpost.com",
            "denverweather.example",
            "krain88.com",
            "seg-social.es",
            "heybakeries.com"
        ]
    );
    assert_eq!(
        order(&merge_results(&local, &network, 3)),
        ["denverpost.com", "denverweather.example", "krain88.com"]
    );
    // With nothing of this node's own, they still show.
    assert_eq!(
        order(&merge_results(&[], &network, 10)),
        ["denverweather.example", "seg-social.es", "heybakeries.com"]
    );

    // Nor does a partial match's score lift this node's copy.
    let mut signed = plumb_core::site::new("krain88.com");
    signed.title = Some("Weather".into());
    let mut crawl = from_network(partial("krain88.com", 0.9));
    crawl.shared = Some(signed.into());
    let network = NetOutcome::Answered(NetworkResults {
        hits: vec![crawl],
        ..NetworkResults::default()
    });
    let shown = merge_results(&local, &network, 10);
    assert_eq!(order(&shown), ["denverpost.com", "krain88.com"]);
    assert_eq!(shown[1].hit.title.as_deref(), Some("Weather"));
    assert!((shown[1].hit.score - 0.55).abs() < f32::EPSILON);
}

#[test]
fn network_only_sites_are_merged_by_score_and_tinted() {
    let local = vec![scored("a.com", 0.9), scored("b.com", 0.5)];
    let network = answered(vec![scored("b.com", 0.8), scored("c.com", 0.7)]);
    let shown = merge_results(&local, &network, 10);
    let order: Vec<(&str, bool)> = shown
        .iter()
        .map(|s| (s.hit.domain.as_str(), s.network.is_some()))
        .collect();
    // b.com is this node's own, so it keeps its own score and no tint.
    assert_eq!(order, [("a.com", false), ("c.com", true), ("b.com", false)]);
    assert_eq!(merge_results(&local, &network, 2).len(), 2);

    let mut settings = no_settings();
    settings.network = NetSetting::On;
    let results = SearchResults {
        pages: Vec::new(),
        hits: local.clone(),
        site_search: None,
        spelling: None,
        meant: None,
        sites_ordered: false,
    };
    let page = render_results(
        "q",
        &results,
        None,
        &network,
        &settings,
        None,
        10,
        false,
        &Icons::default(),
    );
    assert!(page.contains("<li class=\"net\"><a class=\"r\" href=\"https://c.com/\""));
    assert_eq!(page.matches("<li class=\"net\">").count(), 1);
    assert!(page.contains("6 of 8 requests to other nodes answered"));
    assert!(page.contains("Tinted results came only from the network."));
    assert!(page.contains("from the Plumb network (signed crawl, checked, in 2 answers)"));
    assert!(!page.contains("As JSON:"));
}
