//! Inner-page caches are observations, not proof of useful source coverage.
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use plumb_crawl::{CrawlConfig, SitePageOutcome, SitePagesTarget};
use plumb_ingest::docs::FetchedDoc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

const SCHEMA: u32 = 1;
/// Bump when extraction, passage/symbol limits or title policies change.
/// Kept here so richer FetchedDoc fields can be added independently.
pub(super) const EXTRACTOR_VERSION: &str = "inner-pages-3-rich-language";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Envelope {
    pub schema: u32,
    pub key: String,
    pub fingerprint: String,
    pub extractor: String,
    pub fetched_at: u64,
    /// Actual upstream observation time when known; never copied from fetched_at.
    pub source_observed_at: Option<u64>,
    pub complete: bool,
    pub found: usize,
    pub skipped: usize,
    pub capped: bool,
    pub outcomes: Vec<SitePageOutcome>,
    pub docs: Vec<FetchedDoc>,
}

#[derive(Clone, Copy)]
pub(super) struct Policy {
    pub max_age: u64,
    pub force: bool,
    pub extraction: plumb_crawl::InnerPageExtraction,
}

pub(super) struct Cached {
    pub envelope: Envelope,
    pub reused: bool,
}

pub(super) struct SiteFetch<S> {
    pub site: S,
    pub target: SitePagesTarget,
    pub cached: Cached,
}

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub(super) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn fingerprint(
    target: &SitePagesTarget,
    profile: &str,
    cfg: &CrawlConfig,
    extraction: plumb_crawl::InnerPageExtraction,
) -> String {
    // Debug of a static profile is deterministic and includes roots, title
    // cleaning names, aliases, weights and caps. Avoid environment/proxy secrets.
    digest(
        &serde_json::to_vec(&(
            target,
            profile,
            EXTRACTOR_VERSION,
            plumb_crawl::DOCS_EXTRACTOR_VERSION,
            extraction,
            &cfg.user_agent,
            cfg.max_bytes,
            cfg.max_redirects,
            cfg.timeout.as_secs(),
            cfg.allow_private_addresses,
        ))
        .expect("serializable configuration"),
    )
}

/// Legacy vectors are readable but explicitly stale. A damaged/unknown
/// envelope is never silently reinterpreted as fresh data.
fn read(path: &Path) -> Option<Envelope> {
    let bytes = std::fs::read(path).ok()?;
    if let Ok(envelope) = serde_json::from_slice::<Envelope>(&bytes) {
        return Some(envelope);
    }
    let docs: Vec<FetchedDoc> = serde_json::from_slice(&bytes).ok()?;
    Some(Envelope {
        schema: 0,
        key: String::new(),
        fingerprint: String::new(),
        extractor: "legacy".into(),
        fetched_at: 0,
        source_observed_at: None,
        complete: false,
        found: docs.len(),
        skipped: 0,
        capped: false,
        outcomes: Vec::new(),
        docs,
    })
}

fn fresh(envelope: &Envelope, key: &str, fingerprint: &str, policy: Policy, now: u64) -> bool {
    !policy.force
        && envelope.schema == SCHEMA
        && envelope.key == key
        && envelope.fingerprint == fingerprint
        && envelope.extractor == EXTRACTOR_VERSION
        && envelope.complete
        && !envelope.docs.is_empty()
        && envelope.fetched_at <= now
        && now - envelope.fetched_at < policy.max_age
}

fn write(path: &Path, envelope: &Envelope) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut part = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(part.as_file_mut(), envelope)?;
    part.as_file().sync_all()?;
    part.persist(path)?;
    Ok(())
}

pub(super) async fn fetch(
    key: &str,
    target: &SitePagesTarget,
    profile: &str,
    cfg: &CrawlConfig,
    path: Option<&Path>,
    policy: Policy,
) -> Cached {
    let fingerprint = fingerprint(target, profile, cfg, policy.extraction);
    if let Some(envelope) = path
        .and_then(read)
        .filter(|e| fresh(e, key, &fingerprint, policy, now()))
    {
        info!(
            "{key}: {} fresh compatible pages reused",
            envelope.docs.len()
        );
        return Cached {
            envelope,
            reused: true,
        };
    }
    let result =
        plumb_crawl::fetch_site_pages_with_extraction(target, cfg, policy.extraction).await;
    // Allowed skips (robots, non-HTML and duplicates) are recorded but do not
    // mean an interrupted run. Network/bot/HTTP failures cannot replace a host.
    let failed = result.outcomes.iter().any(|o| {
        o.status == "failed"
            || o.status == "discovery-failed"
            || o.status == "a bot check"
            || o.status == "robots.txt could not be read"
            || o.status.starts_with("HTTP 5")
            || o.status == "HTTP 429"
    });
    let envelope = Envelope {
        schema: SCHEMA,
        key: key.into(),
        fingerprint,
        extractor: EXTRACTOR_VERSION.into(),
        fetched_at: now(),
        source_observed_at: None,
        complete: result.finished && !failed && !result.pages.is_empty(),
        found: result.found,
        skipped: result.skipped,
        capped: result.found > target.max_pages,
        outcomes: result.outcomes,
        docs: result
            .pages
            .into_iter()
            .map(|page| FetchedDoc {
                url: page.url,
                title: page.meta.title,
                description: page.meta.description,
                text: page.meta.body_text,
                sections: page.meta.sections,
                search: page.meta.search,
                language: page.meta.language,
            })
            .collect(),
    };
    if let Some(path) = path {
        // Keep the last successful cache. Failed attempt diagnostics are separate,
        // so a later retry can still inspect the last good observation.
        let attempt = path.with_extension("attempt.json");
        if let Err(err) = write(&attempt, &envelope) {
            warn!("{key}: recording fetch attempt: {err:#}");
        }
        if envelope.complete {
            if let Err(err) = write(path, &envelope) {
                warn!("{key}: keeping fetched pages: {err:#}");
            }
        }
    }
    Cached {
        envelope,
        reused: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completed_cache_skips_requests_force_refetches_and_failure_keeps_last_good_cache() {
        use axum::{response::Html, routing::get, Router};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let sitemap = format!("<urlset><url><loc>{origin}/docs/guide</loc></url></urlset>");
        let requests = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let count = requests.clone();
        let failing = fail.clone();
        let app = Router::new().route("/robots.txt", get(|| async { "User-agent: *\nAllow: /" }))
            .route("/sitemap.xml", get(move || { let sitemap = sitemap.clone(); async move { sitemap } }))
            .route("/docs/guide", get(move || {
                let count = count.clone(); let fail = failing.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    let status = if fail.load(Ordering::SeqCst) { axum::http::StatusCode::SERVICE_UNAVAILABLE } else { axum::http::StatusCode::OK };
                    (status, Html("<html lang=\"en\"><head><title>Testing guide</title></head><body><main><h2 id=\"test-fixtures\">set_multiplayer_authority</h2><p>How to test a package with fixtures and verify multiplayer authority.</p></main></body></html>"))
                }
            }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cfg = CrawlConfig {
            allow_private_addresses: true,
            per_host_delay: std::time::Duration::ZERO,
            timeout: std::time::Duration::from_secs(2),
            ..Default::default()
        };
        let target = SitePagesTarget {
            domain: "example.test".into(),
            roots: vec![format!("{origin}/docs/")],
            max_pages: 4,
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docs-test.json");
        let policy = Policy {
            max_age: 100,
            force: false,
            extraction: plumb_crawl::InnerPageExtraction::Docs,
        };
        let first = fetch("test", &target, "policy-v1", &cfg, Some(&path), policy).await;
        assert!(first.envelope.complete && !first.reused);
        assert_eq!(first.envelope.docs[0].language.as_deref(), Some("en"));
        assert!(first.envelope.docs[0].search.as_ref().is_some_and(|s| s
            .symbols
            .iter()
            .any(|s| s.identifier == "set_multiplayer_authority")));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        let second = fetch("test", &target, "policy-v1", &cfg, Some(&path), policy).await;
        assert!(second.reused);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "fresh cache skips upstream requests"
        );
        let forced = Policy {
            force: true,
            ..policy
        };
        fetch("test", &target, "policy-v1", &cfg, Some(&path), forced).await;
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        let good = std::fs::read(&path).unwrap();
        fail.store(true, Ordering::SeqCst);
        let failed = fetch("test", &target, "policy-v1", &cfg, Some(&path), forced).await;
        assert!(!failed.envelope.complete);
        assert_eq!(std::fs::read(&path).unwrap(), good);
        assert!(path.with_extension("attempt.json").exists());
        server.abort();
    }

    #[test]
    fn legacy_cache_is_readable_but_stale_and_compatibility_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docs-python.json");
        std::fs::write(&path, br#"[{"url":"https://docs.python.org/3/","title":"Python","description":null,"text":null}]"#).unwrap();
        let mut e = read(&path).unwrap();
        let policy = Policy {
            max_age: 100,
            force: false,
            extraction: plumb_crawl::InnerPageExtraction::Docs,
        };
        assert!(!fresh(&e, "python", "config", policy, 1000));
        e.schema = SCHEMA;
        e.key = "python".into();
        e.fingerprint = "config".into();
        e.extractor = EXTRACTOR_VERSION.into();
        e.fetched_at = 950;
        e.complete = true;
        assert!(fresh(&e, "python", "config", policy, 1000));
        assert!(!fresh(&e, "python", "changed-roots", policy, 1000));
        assert!(!fresh(&e, "python", "config", policy, 1050));
        assert!(!fresh(&e, "python", "config", policy, 900));
        assert!(!fresh(
            &e,
            "python",
            "config",
            Policy {
                force: true,
                ..policy
            },
            1000
        ));
        e.extractor = "older".into();
        assert!(!fresh(&e, "python", "config", policy, 1000));
        e.extractor = EXTRACTOR_VERSION.into();
        e.complete = false;
        assert!(!fresh(&e, "python", "config", policy, 1000));
    }

    #[test]
    fn source_configuration_and_body_limits_change_the_fingerprint() {
        let mut target = SitePagesTarget {
            domain: "example.com".into(),
            roots: vec!["https://example.com/docs/".into()],
            max_pages: 10,
            ..Default::default()
        };
        let mut cfg = CrawlConfig::default();
        assert_ne!(
            fingerprint(
                &target,
                "title-cleaning-v1",
                &cfg,
                plumb_crawl::InnerPageExtraction::Compact
            ),
            fingerprint(
                &target,
                "title-cleaning-v1",
                &cfg,
                plumb_crawl::InnerPageExtraction::Docs
            )
        );
        let first = fingerprint(
            &target,
            "title-cleaning-v1",
            &cfg,
            plumb_crawl::InnerPageExtraction::Docs,
        );
        target.max_pages = 11;
        assert_ne!(
            first,
            fingerprint(
                &target,
                "title-cleaning-v1",
                &cfg,
                plumb_crawl::InnerPageExtraction::Docs
            )
        );
        target.max_pages = 10;
        cfg.max_bytes += 1;
        assert_ne!(
            first,
            fingerprint(
                &target,
                "title-cleaning-v1",
                &cfg,
                plumb_crawl::InnerPageExtraction::Docs
            )
        );
        cfg.max_bytes -= 1;
        assert_ne!(
            first,
            fingerprint(
                &target,
                "title-cleaning-v2",
                &cfg,
                plumb_crawl::InnerPageExtraction::Docs
            )
        );
    }
}
