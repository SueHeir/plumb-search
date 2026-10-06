use super::*;

fn manifest(keywords: &[&str], hosts: &[&str]) -> Manifest {
    Manifest {
        name: "Test".into(),
        hosts: hosts.iter().map(|h| h.to_string()).collect(),
        keywords: keywords.iter().map(|k| k.to_string()).collect(),
        ..Manifest::default()
    }
}

fn plugin(manifest: Manifest, wat: &str) -> Plugin {
    Plugin::from_parts(
        "test".into(),
        manifest,
        serde_json::Value::Null,
        wat.as_bytes(),
    )
    .expect("a plugin")
}

#[test]
fn keywords_at_either_end_pick_a_plugin() {
    let m = manifest(&["hn", "hacker news"], &["hn.algolia.com"]);
    assert_eq!(
        m.picks("hn rust async", None),
        Some((Some("hn".into()), "rust async".into()))
    );
    assert_eq!(
        m.picks("Rust Async Hacker  News", None),
        Some((Some("hacker news".into()), "Rust Async".into()))
    );
    // Not in the middle, and not alone: "hn" alone is a search for the
    // site.
    assert_eq!(m.picks("rust hn async", None), None);
    assert_eq!(m.picks("hn", None), None);
    assert_eq!(m.picks("hacker news", None), None);
    let always = Manifest {
        always: true,
        ..manifest(&[], &["a.example"])
    };
    assert_eq!(
        always.picks("rust  async", None),
        Some((None, "rust async".into()))
    );
}

fn url(raw: &str) -> url::Url {
    url::Url::parse(raw).unwrap()
}

#[test]
fn plugins_reach_only_their_hosts() {
    let m = manifest(&["x"], &["api.example.org", "*.cdn.example"]);
    assert!(m.allows(&url("https://api.example.org/a")));
    assert!(m.allows(&url("http://API.Example.org./")));
    assert!(!m.allows(&url("https://example.org/")));
    assert!(!m.allows(&url("https://evilapi.example.org/")));
    assert!(m.allows(&url("https://img.cdn.example/")));
    assert!(m.allows(&url("https://a.b.cdn.example/")));
    assert!(!m.allows(&url("https://cdn.example/")));
    assert!(!m.allows(&url("https://evilcdn.example/")));
    // Without a port, only the web's own.
    assert!(!m.allows(&url("http://api.example.org:8080/")));
}

#[test]
fn hosts_can_name_a_port_a_home_network_name_or_ipv6() {
    let m = manifest(
        &["x"],
        &["127.0.0.1:7878", "nas:8989", "[::1]:9117", "media.lan"],
    );
    assert!(m.allows(&url("http://127.0.0.1:7878/api")));
    assert!(!m.allows(&url("http://127.0.0.1:8080/")));
    assert!(!m.allows(&url("http://127.0.0.1/")));
    assert!(m.allows(&url("http://nas:8989/")));
    assert!(!m.allows(&url("http://nas:22/")));
    assert!(m.allows(&url("http://[::1]:9117/")));
    assert!(!m.allows(&url("http://[::1]:9118/")));
    assert!(m.allows(&url("https://media.lan/")));
    assert!(!m.allows(&url("http://media.lan:3000/")));
}

#[test]
fn identifiers_pick_a_plugin_without_a_keyword() {
    let m = Manifest {
        ids: vec!["tmdb-movie".into()],
        ..manifest(&["films"], &["a.example"])
    };
    let mut about = About {
        title: "Paddington 2".into(),
        ..About::default()
    };
    assert_eq!(m.picks("paddington 2", Some(&about)), None);
    about.ids.insert("tmdb-movie".into(), "346648".into());
    assert_eq!(
        m.picks("paddington 2", Some(&about)),
        Some((None, "paddington 2".into()))
    );
    // A keyword still wins, and is taken off.
    assert_eq!(
        m.picks("films paddington 2", Some(&about)),
        Some((Some("films".into()), "paddington 2".into()))
    );
    let any_item = Manifest {
        ids: vec!["wikidata".into()],
        ..manifest(&[], &["a.example"])
    };
    about.wikidata = Some("Q25188".into());
    assert!(any_item.picks("paddington 2", Some(&about)).is_some());
}

#[test]
fn modules_asking_for_more_than_plumb_gives_are_refused() {
    let wat = r#"(module
        (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))
        (memory (export "memory") 1))"#;
    let error = Plugin::from_parts(
        "test".into(),
        manifest(&["x"], &["a.example"]),
        serde_json::Value::Null,
        wat.as_bytes(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("fd_write"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn results_are_checked_trimmed_and_cached() {
    let output = r#"{"results":[
        {"title":"  A   story ","url":"https://news.example.com/a","snippet":"one\ntwo","published":5},
        {"title":"Script","url":"javascript:alert(1)"},
        {"title":"","url":"https://untitled.example/"},
        {"title":"Hot singles","url":"https://b.example/","snippet":"xxx porn"}
    ]}"#;
    let plugins = Plugins::new(vec![plugin(
        manifest(&["hn"], &["a.example"]),
        &answering(output),
    )]);
    assert!(plugins
        .search("rust", SafeSearch::Moderate, None, None)
        .await
        .is_empty());
    let found = plugins
        .search("hn rust", SafeSearch::Moderate, None, None)
        .await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "Test");
    assert_eq!(
        found[0].results,
        vec![PluginItem {
            title: "A story".into(),
            url: "https://news.example.com/a".into(),
            site: "example.com".into(),
            snippet: Some("one two".into()),
            published: Some(5),
            ..PluginItem::default()
        }]
    );
    assert_eq!(plugins.inner.cache.lock().unwrap().len(), 1);
    let again = plugins
        .search("hn rust", SafeSearch::Moderate, None, None)
        .await;
    assert_eq!(again, found);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_that_never_stops_runs_out_of_fuel() {
    let wat = r#"(module
        (memory (export "memory") 1)
        (func (export "plumb_abi") (result i32) (i32.const 1))
        (func (export "plumb_search") (loop $forever (br $forever))))"#;
    let mut looping = plugin(manifest(&["x"], &["a.example"]), wat);
    looping.fuel = 1_000_000;
    let plugins = Plugins::new(vec![
        looping,
        plugin(
            manifest(&["x"], &["a.example"]),
            &answering(r#"{"results":[{"title":"Fine","url":"https://a.example/"}]}"#),
        ),
    ]);
    let found = plugins.search("x rust", SafeSearch::Off, None, None).await;
    assert_eq!(found.len(), 1, "only the plugin that finished: {found:?}");
    assert_eq!(found[0].results[0].title, "Fine");
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_and_other_interfaces_give_nothing() {
    let failing = plugin(
        manifest(&["x"], &["a.example"]),
        &answering(r#"{"results":[],"error":"no API key in config.json"}"#),
    );
    let failing = Arc::new(failing);
    let error = failing.try_query("x rust").await.unwrap_err();
    assert!(error.to_string().contains("no API key"), "{error}");
    let newer = r#"(module
        (memory (export "memory") 1)
        (func (export "plumb_abi") (result i32) (i32.const 99))
        (func (export "plumb_search")))"#;
    let newer = Arc::new(plugin(manifest(&["x"], &["a.example"]), newer));
    let error = newer.try_query("x rust").await.unwrap_err();
    assert!(error.to_string().contains("interface 99"), "{error}");
}

/// A plugin module that fetches the request JSON `request` and hands
/// back the response's body as its output.
fn fetching(request: &str) -> String {
    let escaped: String = request.bytes().map(|b| format!("\\{b:02x}")).collect();
    format!(
        r#"(module
            (import "plumb" "fetch" (func $fetch (param i32 i32) (result i32)))
            (import "plumb" "body_read" (func $body_read (param i32)))
            (import "plumb" "output" (func $output (param i32 i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "{escaped}")
            (func (export "plumb_abi") (result i32) (i32.const 1))
            (func (export "plumb_search") (local $len i32)
                (local.set $len (call $fetch (i32.const 0) (i32.const {len})))
                (if (i32.ge_s (local.get $len) (i32.const 0))
                    (then
                        (call $body_read (i32.const 4096))
                        (call $output (i32.const 4096) (local.get $len))))))"#,
        len = request.len()
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn plugins_fetch_from_their_hosts() {
    use axum::routing::get;
    let app = axum::Router::new().route(
        "/search",
        get(|| async { r#"{"results":[{"title":"From the API","url":"https://a.example/1"}]}"# }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let request = format!(r#"{{"method":"GET","url":"http://127.0.0.1:{port}/search"}}"#);

    let allowed = Arc::new(plugin(
        manifest(&["x"], &[&format!("127.0.0.1:{port}")]),
        &fetching(&request),
    ));
    let found = allowed.try_query("x rust").await.unwrap();
    assert_eq!(found[0].title, "From the API");

    // Without the host in plugin.json the fetch is refused, and the
    // plugin hands back nothing; so it is with the host and another port.
    for hosts in [["a.example"], ["127.0.0.1"]] {
        let refused = Arc::new(plugin(manifest(&["x"], &hosts), &fetching(&request)));
        let error = refused.try_query("x rust").await.unwrap_err();
        assert!(error.to_string().contains("nothing"), "{error}");
    }
}

#[test]
fn fetches_are_limited() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut host = Host {
        input: Vec::new(),
        output: None,
        body: Vec::new(),
        headers: Vec::new(),
        status: 0,
        fetches: 0,
        deadline: Instant::now() + SEARCH_TIME,
        limits: wasmi::StoreLimitsBuilder::new().build(),
        manifest: manifest(&["x"], &["a.example"]),
        client: reqwest::Client::new(),
        runtime: runtime.handle().clone(),
        id: "test".into(),
    };
    let fetch = |host: &mut Host, request: &str| host.fetch(request.as_bytes()).unwrap_err();
    assert_eq!(
        fetch(
            &mut host,
            r#"{"method":"GET","url":"https://other.example/"}"#
        ),
        plumb_plugin::FETCH_NOT_ALLOWED
    );
    assert_eq!(
        fetch(&mut host, r#"{"method":"GET","url":"file:///etc/passwd"}"#),
        plumb_plugin::FETCH_NOT_ALLOWED
    );
    assert_eq!(
        fetch(
            &mut host,
            r#"{"method":"TRACE","url":"https://a.example/"}"#
        ),
        plumb_plugin::FETCH_BAD_REQUEST
    );
    assert_eq!(
        fetch(&mut host, "not json"),
        plumb_plugin::FETCH_BAD_REQUEST
    );
    host.fetches = MAX_FETCHES;
    assert_eq!(
        fetch(&mut host, r#"{"method":"GET","url":"https://a.example/"}"#),
        plumb_plugin::FETCH_TOO_MANY
    );
    host.fetches = 0;
    host.deadline = Instant::now() - Duration::from_millis(1);
    assert_eq!(
        fetch(&mut host, r#"{"method":"GET","url":"https://a.example/"}"#),
        plumb_plugin::FETCH_TIME_UP
    );
}

#[test]
fn a_plugins_folder_loads_what_it_can() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good");
    std::fs::create_dir(&good).unwrap();
    std::fs::write(
        good.join("plugin.json"),
        r#"{"name":"Good","hosts":["a.example"],"keywords":["g"]}"#,
    )
    .unwrap();
    std::fs::write(good.join("plugin.wasm"), answering(r#"{"results":[]}"#)).unwrap();
    std::fs::write(good.join("config.json"), r#"{"key":"k"}"#).unwrap();
    let broken = dir.path().join("broken");
    std::fs::create_dir(&broken).unwrap();
    std::fs::write(broken.join("plugin.json"), "{").unwrap();
    let plugins = Plugins::load_dir(dir.path());
    let ids: Vec<&str> = plugins.list().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["good"]);
    assert_eq!(plugins.list().next().unwrap().config["key"], "k");
    assert!(Plugins::load_dir(&dir.path().join("missing")).is_empty());
}

#[test]
fn magnet_links_badges_and_buttons_are_kept_checked() {
    let long_data = format!(r#"{{"x":"{}"}}"#, "a".repeat(5000));
    let items: Vec<Item> = serde_json::from_str(&format!(
        r#"[
        {{"title":"Sintel","url":"magnet:?xt=urn:btih:abc&dn=Sintel","badge":"  77   seeders ",
          "image":"https://a.example/p.png",
          "actions":[{{"label":"Download","data":{{"magnet":"magnet:?xt=urn:btih:abc"}}}},
                     {{"label":"Too big","data":{long_data}}},
                     {{"label":" ","data":1}}]}},
        {{"title":"Not a magnet","url":"magnet:?dn=nothing"}},
        {{"title":"Pictured","url":"https://a.example/","image":"file:///etc/passwd"}}
    ]"#
    ))
    .unwrap();
    let kept = clean(items.clone(), SafeSearch::Moderate, true);
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].url, "magnet:?xt=urn:btih:abc&dn=Sintel");
    assert_eq!(kept[0].site, "");
    assert_eq!(kept[0].badge.as_deref(), Some("77 seeders"));
    assert_eq!(kept[0].image.as_deref(), Some("https://a.example/p.png"));
    assert_eq!(
        kept[0].actions,
        vec![PluginAction {
            label: "Download".into(),
            data: r#"{"magnet":"magnet:?xt=urn:btih:abc"}"#.into(),
        }]
    );
    assert_eq!(kept[1].image, None);
    // A plugin that cannot act shows no buttons.
    assert!(clean(items, SafeSearch::Moderate, false)[0]
        .actions
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn plugins_read_response_headers() {
    use axum::routing::get;
    let app = axum::Router::new().route(
        "/login",
        get(|| async { ([("set-cookie", "SID=abc; HttpOnly")], "Ok.") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let request = format!(r#"{{"method":"GET","url":"http://127.0.0.1:{port}/login"}}"#);
    let runtime = tokio::runtime::Handle::current();
    let (status, headers, body) = tokio::task::spawn_blocking(move || {
        let mut host = Host {
            input: Vec::new(),
            output: None,
            body: Vec::new(),
            headers: Vec::new(),
            status: 0,
            fetches: 0,
            deadline: Instant::now() + SEARCH_TIME,
            limits: wasmi::StoreLimitsBuilder::new().build(),
            manifest: manifest(&["x"], &[&format!("127.0.0.1:{port}")]),
            client: reqwest::Client::new(),
            runtime,
            id: "test".into(),
        };
        host.fetch(request.as_bytes()).unwrap()
    })
    .await
    .unwrap();
    assert_eq!((status, body.as_slice()), (200, &b"Ok."[..]));
    assert!(headers
        .iter()
        .any(|(name, value)| name == "set-cookie" && value == "SID=abc; HttpOnly"));
}

#[tokio::test(flavor = "multi_thread")]
async fn buttons_run_the_plugins_act_and_forget_its_results() {
    let plugins = answering_plugins(
        "Shelf",
        "shelf",
        r#"{"results":[{"title":"A","url":"https://a.example/","actions":[{"label":"Add","data":{"id":1}}]}],"message":"Added A"}"#,
    );
    let found = plugins
        .search("shelf a", SafeSearch::Moderate, None, None)
        .await;
    assert_eq!(found[0].results[0].actions[0].data, r#"{"id":1}"#);
    assert_eq!(plugins.inner.cache.lock().unwrap().len(), 1);
    let said = plugins.act("shelf", r#"{"id":1}"#).await.unwrap();
    assert_eq!(said, "Added A");
    assert!(plugins.inner.cache.lock().unwrap().is_empty());
    assert!(plugins.act("other", "{}").await.is_err());
    assert!(plugins.act("shelf", "not json").await.is_err());
    // The token is what the node made, and only that.
    let token = plugins.token().to_string();
    assert_eq!(token.len(), 32);
    assert!(plugins.accepts(&token));
    assert!(!plugins.accepts(""));
    assert!(!plugins.accepts(&token[1..]));
}

#[tokio::test(flavor = "multi_thread")]
async fn results_live_as_long_as_the_plugin_says() {
    let live = Manifest {
        cache_seconds: Some(0),
        ..manifest(&["x"], &["a.example"])
    };
    let plugins = Plugins::new(vec![plugin(
        live,
        &answering(r#"{"results":[{"title":"Now","url":"https://a.example/"}]}"#),
    )]);
    let found = plugins.search("x now", SafeSearch::Off, None, None).await;
    assert_eq!(found.len(), 1);
    assert!(plugins.inner.cache.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn page_lookups_ask_only_plugins_that_know_the_site() {
    let knows = Manifest {
        pages: vec!["*.films.example".into()],
        ..manifest(&[], &["a.example"])
    };
    let plugins = Plugins::new(vec![plugin(
        knows,
        &answering(r#"{"results":[{"title":"In your shelf","url":"https://a.example/"}]}"#),
    )]);
    let found = plugins
        .page("https://www.films.example/title/1", SafeSearch::Off, None)
        .await;
    assert_eq!(found[0].results[0].title, "In your shelf");
    assert!(plugins
        .page("https://elsewhere.example/", SafeSearch::Off, None)
        .await
        .is_empty());
    assert!(plugins
        .page("file:///etc/passwd", SafeSearch::Off, None)
        .await
        .is_empty());
}

#[test]
fn about_carries_a_pages_identifiers() {
    let page: plumb_index::pages::Page = serde_json::from_str(
        r#"{"set":"wikipedia-en","url":"https://en.wikipedia.org/wiki/Paddington_2",
            "title":"Paddington 2","description":"2017 film","views":1,"item":"Q25188",
            "profiles":[{"service":"imdb","id":"tt4468740"},{"service":"tmdb-movie","id":"346648"}]}"#,
    )
    .unwrap();
    let about = about_page(&page);
    assert_eq!(about.wikidata.as_deref(), Some("Q25188"));
    assert_eq!(about.id("imdb"), Some("tt4468740"));
    assert_eq!(about.id("tmdb-movie"), Some("346648"));
}

fn shown(id: u32, url: &str, about: Option<About>) -> ShownResult {
    ShownResult {
        id,
        url: url.into(),
        title: url.into(),
        site: "a.example".into(),
        about,
    }
}

#[test]
fn plugins_with_ids_are_shown_only_results_about_what_they_know() {
    let film = About {
        title: "Paddington 2".into(),
        wikidata: Some("Q25188".into()),
        ..About::default()
    };
    let results = [
        shown(0, "https://a.example/", None),
        shown(1, "https://en.wikipedia.org/wiki/Paddington_2", Some(film)),
    ];
    let knows = plugin(
        Manifest {
            ids: vec!["wikidata".into()],
            ..manifest(&[], &["a.example"])
        },
        &annotating("{}"),
    );
    let picked: Vec<u32> = knows.picks_shown(&results).iter().map(|r| r.id).collect();
    assert_eq!(picked, [1]);
    let all = plugin(manifest(&[], &["a.example"]), &annotating("{}"));
    assert_eq!(all.picks_shown(&results).len(), 2);
    // Without keywords, ids, pages or always, a plugin must mark up
    // results to load.
    assert!(Plugin::from_parts(
        "x".into(),
        manifest(&[], &["a.example"]),
        serde_json::Value::Null,
        answering("{}").as_bytes()
    )
    .is_err());
}

#[test]
fn notes_are_kept_only_for_shown_results_and_say_something() {
    let results = [
        shown(0, "https://a.example/", None),
        shown(1, "https://b.example/", None),
    ];
    let notes: Vec<Note> = serde_json::from_str(
        r#"[{"id":0,"badge":"  Not in   library ","actions":[{"label":"Get","data":1}]},
            {"id":1,"hide":true},
            {"id":7,"badge":"Nowhere"},
            {"id":0,"badge":"Twice"},
            {"id":1}]"#,
    )
    .unwrap();
    let marker = plugin(manifest(&[], &["a.example"]), &annotating("{}"));
    let kept = clean_notes(notes, &results, &marker);
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].0, "https://a.example/");
    assert_eq!(kept[0].1.badge.as_deref(), Some("Not in library"));
    // It has no plumb_act, so no buttons.
    assert!(kept[0].1.actions.is_empty());
    assert_eq!(kept[1].0, "https://b.example/");
    assert!(kept[1].1.hide);
}

#[tokio::test(flavor = "multi_thread")]
async fn annotations_come_back_by_address_and_are_cached() {
    let plugins = annotating_plugins("Shelf", r#"{"notes":[{"id":0,"badge":"On your shelf"}]}"#);
    let results = [shown(0, "https://a.example/", None)];
    let notes = plugins.annotate("a", &results).await;
    let on_a = &notes["https://a.example/"];
    assert_eq!(on_a[0].name, "Shelf");
    assert_eq!(on_a[0].badge.as_deref(), Some("On your shelf"));
    assert_eq!(plugins.inner.notes_cache.lock().unwrap().len(), 1);
    assert!(plugins.annotate("a", &[]).await.is_empty());
}
