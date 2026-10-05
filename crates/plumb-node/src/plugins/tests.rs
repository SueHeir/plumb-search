use super::*;

fn manifest(keywords: &[&str], hosts: &[&str]) -> Manifest {
    Manifest {
        name: "Test".into(),
        about: String::new(),
        hosts: hosts.iter().map(|h| h.to_string()).collect(),
        keywords: keywords.iter().map(|k| k.to_string()).collect(),
        always: false,
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
        m.picks("hn rust async"),
        Some((Some("hn".into()), "rust async".into()))
    );
    assert_eq!(
        m.picks("Rust Async Hacker  News"),
        Some((Some("hacker news".into()), "Rust Async".into()))
    );
    // Not in the middle, and not alone: "hn" alone is a search for the
    // site.
    assert_eq!(m.picks("rust hn async"), None);
    assert_eq!(m.picks("hn"), None);
    assert_eq!(m.picks("hacker news"), None);
    let always = Manifest {
        always: true,
        ..manifest(&[], &["a.example"])
    };
    assert_eq!(
        always.picks("rust  async"),
        Some((None, "rust async".into()))
    );
}

#[test]
fn plugins_reach_only_their_hosts() {
    let m = manifest(&["x"], &["api.example.org", "*.cdn.example"]);
    assert!(m.allows("api.example.org"));
    assert!(m.allows("API.Example.org."));
    assert!(!m.allows("example.org"));
    assert!(!m.allows("evilapi.example.org"));
    assert!(m.allows("img.cdn.example"));
    assert!(m.allows("a.b.cdn.example"));
    assert!(!m.allows("cdn.example"));
    assert!(!m.allows("evilcdn.example"));
}

#[test]
fn manifests_need_a_name_hosts_and_a_way_to_run() {
    assert!(manifest(&["x"], &["a.example"]).check().is_ok());
    assert!(manifest(&[], &["a.example"]).check().is_err());
    assert!(manifest(&["x"], &["https://a.example/"]).check().is_err());
    assert!(manifest(&["x"], &["*"]).check().is_err());
    assert!(manifest(&["x"], &["a.example:8080"]).check().is_err());
    let unnamed = Manifest {
        name: " ".into(),
        ..manifest(&["x"], &["a.example"])
    };
    assert!(unnamed.check().is_err());
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
        .search("rust", SafeSearch::Moderate, None)
        .await
        .is_empty());
    let found = plugins.search("hn rust", SafeSearch::Moderate, None).await;
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
        }]
    );
    assert_eq!(plugins.inner.cache.lock().unwrap().len(), 1);
    let again = plugins.search("hn rust", SafeSearch::Moderate, None).await;
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
    let found = plugins.search("x rust", SafeSearch::Off, None).await;
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
        (func (export "plumb_abi") (result i32) (i32.const 2))
        (func (export "plumb_search")))"#;
    let newer = Arc::new(plugin(manifest(&["x"], &["a.example"]), newer));
    let error = newer.try_query("x rust").await.unwrap_err();
    assert!(error.to_string().contains("interface 2"), "{error}");
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
        manifest(&["x"], &["127.0.0.1"]),
        &fetching(&request),
    ));
    let found = allowed.try_query("x rust").await.unwrap();
    assert_eq!(found[0].title, "From the API");

    // Without the host in plugin.json the fetch is refused, and the
    // plugin hands back nothing.
    let refused = Arc::new(plugin(
        manifest(&["x"], &["a.example"]),
        &fetching(&request),
    ));
    let error = refused.try_query("x rust").await.unwrap_err();
    assert!(error.to_string().contains("nothing"), "{error}");
}

#[test]
fn fetches_are_limited() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut host = Host {
        input: Vec::new(),
        output: None,
        body: Vec::new(),
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
            r#"{"method":"DELETE","url":"https://a.example/"}"#
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
