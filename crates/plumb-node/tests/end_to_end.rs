//! End-to-end tests: run the `plumb` binary over the synthetic fixtures in
//! `fixtures/` (ingest, index, search, eval), and drive the web app's router
//! directly with `tower::ServiceExt::oneshot`, without a network listener.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use plumb_core::{read_jsonl, SiteRecord};
use plumb_index::{build_index, Hit, RankConfig, Searcher};
use plumb_node::web::{escape_html, router, IndexBackend};
use tower::ServiceExt;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

fn args(items: &[&dyn AsRef<OsStr>]) -> Vec<OsString> {
    items.iter().map(|a| a.as_ref().to_os_string()).collect()
}

fn plumb(args: &[OsString]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_plumb"))
        .args(args)
        .env("RUST_LOG", "warn")
        .env("NO_COLOR", "1")
        .output()
        .expect("running the plumb binary")
}

/// Runs `plumb`, failing the test with its output unless it succeeds, and
/// returns its stdout.
fn plumb_ok(args: &[OsString]) -> String {
    let output = plumb(args);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "plumb {args:?} failed with {}\n--- stdout\n{stdout}\n--- stderr\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// Ingests every fixture into `dir/records.jsonl`, indexes it into
/// `dir/index`, and returns the index path and the ingest output.
fn build_fixture_index(dir: &Path) -> (PathBuf, String) {
    let records = dir.join("records.jsonl");
    let index = dir.join("index");
    let ingest = plumb_ok(&args(&[
        &"ingest",
        &"--tranco",
        &fixture("tranco.csv"),
        &"--cc-ranks",
        &fixture("cc-domain-ranks.txt"),
        &"--wat",
        &fixture("sample.wat"),
        &"--wikidata",
        &fixture("wikidata-official-sites.tsv"),
        &"--out",
        &records,
    ]));
    plumb_ok(&args(&[
        &"index",
        &"--records",
        &records,
        &"--index",
        &index,
    ]));
    (index, ingest)
}

#[test]
fn ingest_index_search_and_eval_the_fixtures() {
    let tmp = tempfile::tempdir().unwrap();
    let (index, ingest) = build_fixture_index(tmp.path());

    for source in ["tranco", "cc-ranks", "wat", "wikidata", "wrote"] {
        assert!(
            ingest.lines().any(|l| l.starts_with(source)),
            "no {source} line in:\n{ingest}"
        );
    }

    // Every source reached the merged record.
    let records: Vec<SiteRecord> = read_jsonl(&tmp.path().join("records.jsonl")).unwrap();
    let usbank = records
        .iter()
        .find(|r| r.domain == "usbank.com")
        .expect("usbank.com record");
    assert_eq!(usbank.url.as_deref(), Some("https://www.usbank.com/"));
    assert!(usbank.title.as_deref().unwrap().contains("U.S. Bank"));
    assert!(usbank.signals.tranco_rank.is_some());
    assert!(usbank.signals.harmonic_rank.is_some());
    assert!(usbank.signals.official_site);
    assert!(usbank.signals.linking_domains >= 2);
    assert!(usbank.link_texts.iter().any(|lt| lt.text == "us bank"));
    // A shared host claimed by many Wikidata items is nobody's official site.
    let facebook = records.iter().find(|r| r.domain == "facebook.com").unwrap();
    assert!(!facebook.signals.official_site);
    // A domain only seen as a link target is discovered.
    assert!(records.iter().any(|r| r.domain == "github.com"));

    let json = plumb_ok(&args(&[
        &"search", &"--index", &index, &"--json", &"us", &"bank",
    ]));
    let hits: Vec<Hit> = serde_json::from_str(&json).expect("search --json output");
    assert_eq!(hits.first().map(|h| h.domain.as_str()), Some("usbank.com"));
    assert!(hits.len() <= 10);
    assert!(hits.iter().any(|h| h.domain == "usbank-login-help.com"));

    let text = plumb_ok(&args(&[
        &"search", &"--index", &index, &"--limit", &"3", &"chase",
    ]));
    assert!(text.starts_with(" 1. chase.com "), "{text}");

    let eval = plumb_ok(&args(&[
        &"eval",
        &"--index",
        &index,
        &"--queries",
        &fixture("brand_queries.tsv"),
        &"--min-top1",
        &"0.9",
    ]));
    assert!(eval.contains("top-1"), "{eval}");
    assert!(eval.contains("MRR@10"), "{eval}");
}

#[test]
fn eval_fails_below_min_top1_and_reports_misses() {
    let tmp = tempfile::tempdir().unwrap();
    let (index, _) = build_fixture_index(tmp.path());
    let queries = tmp.path().join("queries.tsv");
    std::fs::write(
        &queries,
        "# one right, one wrong on purpose\nus bank\tusbank.com\nus bank\tchase.com\n",
    )
    .unwrap();
    let eval_args = |min: &str| {
        args(&[
            &"eval",
            &"--index",
            &index,
            &"--queries",
            &queries,
            &"--min-top1",
            &min,
        ])
    };

    let output = plumb(&eval_args("0.9"));
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("miss: \"us bank\" (line 3) expected chase.com"),
        "{stdout}"
    );
    assert!(stdout.contains("first was usbank.com"), "{stdout}");
    assert!(stdout.contains("top-1    50.0%  (1/2)"), "{stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("below --min-top1"), "{stderr}");

    plumb_ok(&eval_args("0.5"));
}

async fn get(app: &Router, uri: &str) -> (StatusCode, String, String) {
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        content_type,
        String::from_utf8(body.to_vec()).unwrap(),
    )
}

fn app_for(index: &Path) -> Router {
    let searcher = Searcher::open(index).unwrap();
    router(Arc::new(IndexBackend::new(searcher, RankConfig::default())))
}

#[tokio::test]
async fn web_app_serves_the_fixture_index() {
    let tmp = tempfile::tempdir().unwrap();
    let (index, _) = build_fixture_index(tmp.path());
    let app = app_for(&index);

    let (status, content_type, body) = get(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"));
    assert!(body.contains("<form action=\"/search\""));
    assert!(body.contains("sites indexed"));

    let (status, _, body) = get(&app, "/search?q=us+bank").await;
    assert_eq!(status, StatusCode::OK);
    let first = body.split("<li>").nth(1).expect("at least one result");
    assert!(
        first.contains("href=\"https://www.usbank.com/\""),
        "first result: {first}"
    );

    let (status, content_type, body) = get(&app, "/api/search?q=us%20bank&limit=3").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "application/json");
    let hits: Vec<Hit> = serde_json::from_str(&body).unwrap();
    assert!(!hits.is_empty() && hits.len() <= 3, "{hits:?}");
    assert_eq!(hits[0].domain, "usbank.com");
}

#[tokio::test]
async fn web_app_escapes_text_from_the_web() {
    let tmp = tempfile::tempdir().unwrap();
    let index = tmp.path().join("index");

    let mut evil = SiteRecord::new("evil-example.com");
    evil.url = Some("javascript:alert(document.cookie)".into());
    evil.title = Some("<script>alert('pwned')</script> Evil Example & Co".into());
    evil.description = Some("<img src=x onerror=alert(1)> a \"quoted\" description".into());
    evil.add_alias("Evil Example");
    evil.signals.tranco_rank = Some(1_000);
    let mut plain = SiteRecord::new("example.org");
    plain.url = Some("https://example.org/?a=1&b=2".into());
    plain.title = Some("Example Org".into());
    build_index(&index, &[evil, plain]).unwrap();

    // The index hands the raw text back, so escaping is up to the web app.
    let searcher = Searcher::open(&index).unwrap();
    let hits = searcher.search("evil example", 10).unwrap();
    let evil_hit = hits
        .iter()
        .find(|h| h.domain == "evil-example.com")
        .expect("evil-example.com is found");
    assert_eq!(evil_hit.url, "javascript:alert(document.cookie)");
    let raw_title = evil_hit.title.clone().unwrap();
    assert!(raw_title.contains("<script>"), "{raw_title}");

    let app = app_for(&index);
    let (status, _, body) = get(&app, "/search?q=evil+example").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("<script"), "{body}");
    assert!(!body.contains("<img"), "{body}");
    assert!(!body.contains("javascript:"), "{body}");
    assert!(body.contains(&escape_html(&raw_title)), "{body}");
    assert!(body.contains("&lt;script&gt;alert(&#39;pwned&#39;)&lt;/script&gt;"));
    assert!(body.contains("&lt;img src=x onerror=alert(1)&gt; a &quot;quoted&quot; description"));
    // A non-http URL is replaced by the site's homepage.
    assert!(
        body.contains("href=\"https://evil-example.com/\""),
        "{body}"
    );

    let (_, _, body) = get(&app, "/search?q=example+org").await;
    assert!(
        body.contains("href=\"https://example.org/?a=1&amp;b=2\""),
        "{body}"
    );

    // The query is echoed back escaped too.
    let (_, _, body) = get(&app, "/search?q=%22%3E%3Cscript%3Ealert(1)%3C/script%3E").await;
    assert!(!body.contains("<script"), "{body}");
    assert!(body.contains("value=\"&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;\""));
}
