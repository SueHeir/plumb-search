use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Request;
use axum::response::Html;
use axum::routing::get;
use axum::Router;

use super::*;

fn words(text: &str) -> Vec<String> {
    let mut words: Vec<String> = match_words(text).into_iter().collect();
    words.sort();
    words
}

#[test]
fn words_match_without_plurals_endings_or_function_words() {
    assert_eq!(words("Fact sheets of the WHO"), ["fact", "sheet", "who"]);
    assert_eq!(words("/news-room/fact-sheets/detail/dengue"), {
        let mut w = vec!["news", "room", "fact", "sheet", "detail", "dengue"];
        w.sort();
        w
    });
    assert_eq!(words("Contribution limits"), ["contribution", "limit"]);
    assert_eq!(words("Diseases and policies"), ["disease", "policy"]);
    assert_eq!(words("disease policy"), ["disease", "policy"]);
    // Short words, and "-ss", "-us" and "-is", keep their ending.
    assert_eq!(
        words("bus class status analysis"),
        ["analysis", "bus", "class", "status"]
    );
}

#[test]
fn a_page_needs_all_of_a_short_want_and_most_of_a_long_one() {
    let wants = |list: &[&str]| -> Vec<HashSet<String>> {
        list.iter().map(|want| match_words(want)).collect()
    };
    let found = match_words("Dengue and severe dengue | fact sheet");
    assert_eq!(has_enough(&wants(&["dengue"]), &found), Some(0));
    assert_eq!(has_enough(&wants(&["dengue vaccine"]), &found), None);
    // Three of four words is enough; the better of two wants is taken.
    assert_eq!(
        has_enough(&wants(&["malaria", "dengue fact sheet 2026"]), &found),
        Some(1)
    );
    assert_eq!(has_enough(&wants(&["dengue fact 2026 2027"]), &found), None);
}

#[test]
fn only_menu_links_to_lists_of_pages_are_hubs() {
    assert!(is_hub("Health topics"));
    assert!(is_hub("Diseases A-Z"));
    assert!(is_hub("Browse all guides"));
    assert!(!is_hub("About us"));
    assert!(!is_hub("Contact"));
}

/// A small health site: its homepage links to nothing about dengue but
/// has a menu link to its topics, which list the fact sheets.
fn health_site(requested: Arc<Mutex<Vec<String>>>) -> Router {
    let page = |html: &'static str| move || async move { Html(html) };
    Router::new()
        .route(
            "/",
            get(page(
                r#"<title>Health Org</title><nav><a href="/topics">Health topics</a>
                <a href="/about">About us</a></nav>"#,
            )),
        )
        .route(
            "/topics",
            get(page(
                r#"<title>Topics</title><a href="/sheets/dengue">Dengue</a>
                <a href="/sheets/malaria">Malaria</a><a href="/sheets/dengue-vaccines">Vaccines</a>"#,
            )),
        )
        .route(
            "/sheets/dengue",
            get(page(
                r#"<title>Dengue and severe dengue</title>
                <meta name="description" content="Key facts about dengue.">
                <a href="http://other.example/dengue">Dengue elsewhere</a>
                <a href="http://other.example/news">News</a>"#,
            )),
        )
        .route(
            "/sheets/dengue-vaccines",
            get(page("<title>Dengue vaccines</title>")),
        )
        .route("/sheets/malaria", get(page("<title>Malaria</title>")))
        .layer(axum::middleware::from_fn(
            move |request: Request, next: axum::middleware::Next| {
                let requested = Arc::clone(&requested);
                async move {
                    requested.lock().unwrap().push(request.uri().path().to_string());
                    next.run(request).await
                }
            },
        ))
}

async fn follow_on_test_site(target: FollowTarget) -> (Followed, Vec<String>) {
    let requested = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = health_site(Arc::clone(&requested));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg = CrawlConfig {
        per_host_delay: Duration::from_millis(1),
        timeout: Duration::from_secs(3),
        resolve: vec![("health.example".into(), ([127, 0, 0, 1], port).into())],
        ..CrawlConfig::default()
    };
    let mut followed = follow_links(vec![target], &cfg).await;
    let requested = requested.lock().unwrap().clone();
    (followed.pop().unwrap(), requested)
}

fn target(wants: &[&str], leads: Vec<Lead>, max_fetches: usize) -> FollowTarget {
    FollowTarget {
        domain: "health.example".into(),
        homepage: "http://health.example/".into(),
        leads,
        wants: wants
            .iter()
            .map(|want| match_words(want).into_iter().collect())
            .collect(),
        max_fetches,
    }
}

#[tokio::test]
async fn follows_a_hub_to_the_page_with_the_words() {
    let (followed, requested) = follow_on_test_site(target(&["dengue"], Vec::new(), 4)).await;
    let found: Vec<&str> = followed
        .pages
        .iter()
        .map(|(p, _)| p.title.as_str())
        .collect();
    assert_eq!(found, ["Dengue and severe dengue", "Dengue vaccines"]);
    let (page, want) = &followed.pages[0];
    assert_eq!(page.url, "http://health.example/sheets/dengue");
    assert_eq!(page.description.as_deref(), Some("Key facts about dengue."));
    assert_eq!((page.depth, *want), (2, 0));
    // Malaria has none of the words, and "About us" is no list of pages.
    assert!(!requested
        .iter()
        .any(|path| path == "/sheets/malaria" || path == "/about"));
    // The other site's page on dengue is a lead there; its news is not.
    let elsewhere: Vec<&str> = followed
        .elsewhere
        .iter()
        .map(|(l, _)| l.url.as_str())
        .collect();
    assert_eq!(elsewhere, ["http://other.example/dengue"]);
    assert_eq!(followed.fetched, 4);
}

#[tokio::test]
async fn stops_at_its_fetches_and_leaves_the_rest_as_leads() {
    let (followed, requested) = follow_on_test_site(target(&["dengue"], Vec::new(), 3)).await;
    assert_eq!(followed.fetched, 3);
    let pages: Vec<&str> = requested
        .iter()
        .filter(|path| *path != "/robots.txt")
        .map(String::as_str)
        .collect();
    assert_eq!(pages, ["/", "/topics", "/sheets/dengue"]);
    let leads: Vec<(&str, u8)> = followed
        .leads
        .iter()
        .map(|lead| (lead.url.as_str(), lead.depth))
        .collect();
    assert_eq!(leads, [("http://health.example/sheets/dengue-vaccines", 2)]);
}

#[tokio::test]
async fn a_lead_with_the_words_goes_before_the_homepage() {
    let lead = Lead {
        url: "http://health.example/sheets/malaria".into(),
        label: "Malaria".into(),
        depth: 2,
    };
    let (followed, requested) = follow_on_test_site(target(&["malaria"], vec![lead], 1)).await;
    assert_eq!(followed.pages.len(), 1);
    assert_eq!(followed.pages[0].0.title, "Malaria");
    assert!(!requested.iter().any(|path| path == "/"));
}

#[tokio::test]
async fn the_homepage_is_never_a_page_found() {
    let (followed, _) = follow_on_test_site(target(&["health org"], Vec::new(), 2)).await;
    assert!(followed.pages.is_empty());
}
