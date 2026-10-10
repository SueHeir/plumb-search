use super::*;
use plumb_core::{article::Article, SiteRecord};
use plumb_index::pages::{build_page_index, Page, PageHit, PlacedPage};
use plumb_index::{build_index, Hit, SearchOptions, SearchResults};
use plumb_node::web::SearchBackend;

fn catalog(root: &Path) -> GenerationBinding {
    use std::os::unix::fs::MetadataExt;
    let files = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let metadata = path.metadata().unwrap();
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            let sha256 = matches!(name.as_str(), "meta.json" | ".managed.json" | "pages.json")
                .then(|| format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap())));
            (
                name,
                FileBinding {
                    bytes: metadata.len(),
                    modified_ns: metadata
                        .modified()
                        .unwrap()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    sha256,
                },
            )
        })
        .collect();
    let metadata = root.metadata().unwrap();
    GenerationBinding {
        generation: "synthetic-only".into(),
        retention_receipt: "owned test fixture".into(),
        corpus_manifest_sha256_attested: "0".repeat(64),
        directory_device: metadata.dev(),
        directory_inode: metadata.ino(),
        files,
    }
}

fn synthetic_backend(root: &Path) -> Arc<backend::FrozenBackend> {
    let site_path = root.join("sites");
    let page_path = root.join("pages");
    let mut site = SiteRecord::new("rust-lang.org");
    site.title = Some("Rust programming language".into());
    build_index(&site_path, &[site]).unwrap();
    build_page_index(
        &page_path,
        [Page::from_article(
            "en",
            Article {
                title: "Rust programming language".into(),
                views: 20,
                ..Default::default()
            },
        )],
    )
    .unwrap();
    let sites = RetainedGeneration::open(&site_path, catalog(&site_path)).unwrap();
    let pages = RetainedGeneration::open(&page_path, catalog(&page_path)).unwrap();
    Arc::new(backend::FrozenBackend {
        sites: IndexBackend::new(
            Searcher::open_retained(&sites).unwrap(),
            RankConfig::default(),
        ),
        pages: plumb_index::pages::PageSearcher::open_retained(&pages).unwrap(),
        rank: RankConfig::default(),
        places: None,
        raw: Default::default(),
        errors: Default::default(),
    })
}

#[tokio::test]
async fn frozen12_router_uses_real_retrieval_and_keeps_unknown_observations() {
    let temp = tempfile::tempdir().unwrap();
    let backend = synthetic_backend(temp.path());
    let options = SearchOptions {
        country: Some("US".into()),
        language: Some("en".into()),
        safe: plumb_index::SafeSearch::Off,
        ..Default::default()
    };
    let mut expected = backend.sites.search_full(QUERIES[0], 10, &options).unwrap();
    assert!(plumb_node::page_retrieval::add_pages(
        &backend.pages,
        Some(&backend.sites),
        &backend.rank,
        QUERIES[0],
        &options,
        &mut expected
    )
    .is_empty());
    let app = web::router_with(
        backend.clone(),
        plumb_node::country::HomeCountry::Fixed("US".into()),
    );
    let body = observe(app, QUERIES[0]).await.unwrap();
    assert_eq!(backend.raw.lock().unwrap().as_ref().unwrap(), &expected);
    assert!(!body["assembled"]["rows"].as_array().unwrap().is_empty());
    assert!(serde_json::to_string(&body)
        .unwrap()
        .contains("rust-lang.org"));
    assert!(body.get("grade").is_none());
    let mut report = json!({ "observations": [{ "body": body }], "judgments": "unknown" });
    finish_report(&mut report, &[]);
    assert_eq!(report["status"], "complete");
    assert_eq!(report["judgments"], "unknown");
    assert_eq!(report["observations"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn frozen12_negative_empty_retrieval_and_external_queries() {
    let temp = tempfile::tempdir().unwrap();
    let backend = synthetic_backend(temp.path());
    assert!(backend
        .search_full("zzunmatchedzz", 10, &SearchOptions::default())
        .unwrap()
        .hits
        .is_empty());
    let app = web::router(backend);
    assert!(observe(app.clone(), "weather in Denver").await.is_err());
    assert!(observe(app, "10 USD in EUR").await.is_err());
}

#[test]
fn frozen12_binding_mismatch_and_incomplete_baseline_are_rejected() {
    let binding = json!({ "sites": "synthetic-A", "pages": "synthetic-A", "options": OPTIONS });
    let mut baseline = json!({ "status": "complete", "comparison_binding": binding,
        "observations": vec![json!({}); 12] });
    validate_baseline(&baseline, &binding).unwrap();
    assert!(validate_baseline(&baseline, &json!({ "pages": "synthetic-B" })).is_err());
    baseline["status"] = json!("incomplete");
    assert!(validate_baseline(&baseline, &binding).is_err());
    baseline["status"] = json!("complete");
    baseline["observations"] = json!([]);
    assert!(validate_baseline(&baseline, &binding).is_err());
}

#[test]
fn frozen12_failure_clears_all_observations_and_retains_unknown_status() {
    let mut report = json!({ "status": "complete", "observations": [{"body": "partial"}], "judgments": "unknown" });
    finish_report(
        &mut report,
        &["generation changed after final query".into()],
    );
    assert_eq!(report["status"], "incomplete");
    assert_eq!(report["observations"], json!([]));
    assert_eq!(report["judgments"], "unknown");
}

struct RawGuardBackend(SearchResults);
impl SearchBackend for RawGuardBackend {
    fn search(&self, _: &str, _: usize) -> Result<Vec<Hit>> {
        Ok(self.0.hits.clone())
    }
    fn search_full(&self, _: &str, _: usize, _: &SearchOptions) -> Result<SearchResults> {
        Ok(self.0.clone())
    }
    fn num_docs(&self) -> u64 {
        self.0.hits.len() as u64
    }
}

#[tokio::test]
async fn frozen12_raw_stronger_page_guard_does_not_replace_homepage() {
    // The candidate selector's own tests exercise its raw-page veto. This
    // baseline fixture protects the router input/output boundary: raw evidence
    // is retained even when the assembled display drops that page.
    let site: Hit = serde_json::from_value(json!({ "domain": "station.example",
        "url": "https://station.example/", "title": "Union Station",
        "score": 0.9, "text_score": 0.8, "link_score": 0.7,
        "key_pages": [{ "url": "https://station.example/dine-shop", "label": "Dine & Shop" }] }))
    .unwrap();
    let mut page = Page::from_article(
        "en",
        Article {
            title: "Union Station dining guide".into(),
            ..Default::default()
        },
    );
    page.url = "https://station.example/dining-guide".into();
    page.site = Some("station.example".into());
    let hit: PageHit = serde_json::from_value(
        json!({ "page": page, "score": 0.95, "named": true, "whole": true, "popularity": 0.5 }),
    )
    .unwrap();
    let mut carried = hit.clone();
    carried.page.url = "https://station.example/".into();
    carried.page.title = "Union Station".into();
    carried.score = 0.5;
    let raw = SearchResults {
        hits: vec![site],
        pages: vec![
            PlacedPage {
                hit: carried,
                under: Some("station.example".into()),
                at: 0,
            },
            PlacedPage {
                hit,
                under: None,
                at: 20,
            },
        ],
        ..Default::default()
    };
    let backend = Arc::new(RawGuardBackend(raw.clone()));
    let body = observe(web::router(backend), "Union Station restaurants")
        .await
        .unwrap();
    assert_eq!(
        raw.pages[1].hit.page.url,
        "https://station.example/dining-guide"
    );
    let rows = body["assembled"]["rows"].as_array().unwrap();
    let site_row = rows.iter().find(|row| row["kind"] == "site").unwrap();
    assert_eq!(site_row["site"]["url"], "https://station.example/");
    assert!(!serde_json::to_string(rows)
        .unwrap()
        .contains("https://station.example/dining-guide"));
    assert_eq!(raw.pages.len(), 2);
}

#[test]
fn frozen12_request_options_and_cohort_are_fixed() {
    assert_eq!(QUERIES.len(), 12);
    for query in QUERIES {
        let uri = request_uri(query);
        assert!(uri.ends_with(OPTIONS));
        assert_eq!(
            url::form_urlencoded::parse(uri.split_once('?').unwrap().1.as_bytes())
                .find(|(key, _)| key == "q")
                .unwrap()
                .1,
            query
        );
        assert!(!plumb_answer::may_need_rates(query));
        assert!(plumb_answer::weather::asked(query).is_none());
    }
}
