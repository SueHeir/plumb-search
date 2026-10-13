//! The web server's tests: its pages, API and what each browser keeps.

mod browser;
mod instant_answers;
mod node_status;
mod opensearch;
mod pages;
mod places;
mod plugins;
mod results;
mod settings_and_network;

use std::sync::Mutex;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use plumb_core::{now_unix, site_initial, KeyPage, RecentNews, SafeSearch, SiteRecord};
use plumb_index::pages::PageHit;
use plumb_index::{Hit, RankConfig, SiteSearch, Spelling};
use tower::ServiceExt;

use super::hit::{go_link, key_pages_line, product_of, render_page};
use super::home::{node_note, render_home, render_opensearch, render_setup};
use super::html::{safe_href, settings_form};
use super::params::{NetSetting, Settings};
use super::plugin_results::action_forms;
use super::results::{
    merge_results, render_network, render_results, render_results_with, render_source, Shown,
};
use super::search::{music_about, shown_results, NetOutcome, MUSIC_ABOUT_AT};
use super::*;
use crate::node::{LastError, Progress, Step};

/// Returns canned hits and remembers what it was asked.
#[derive(Default)]
struct FakeBackend {
    hits: Vec<Hit>,
    fail: bool,
    panic: bool,
    calls: Mutex<Vec<(String, usize)>>,
}

impl SearchBackend for FakeBackend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.calls.lock().unwrap().push((query.to_string(), limit));
        if self.panic {
            panic!("backend exploded");
        }
        if self.fail {
            anyhow::bail!("index is broken");
        }
        Ok(self.hits.iter().take(limit).cloned().collect())
    }

    fn num_docs(&self) -> u64 {
        12_345
    }
}

fn hit(domain: &str, url: &str, title: Option<&str>, description: Option<&str>) -> Hit {
    Hit {
        demand: None,
        missing_words: false,
        query_evidence: None,
        placing_text_score: None,
        domain: domain.to_string(),
        url: url.to_string(),
        title: title.map(str::to_string),
        description: description.map(str::to_string),
        score: 0.9,
        text_score: 0.8,
        link_score: 0.7,
        country: None,
        named: false,
        official: false,
        key_pages: Vec::new(),
    }
}

fn bank_hits() -> Vec<Hit> {
    vec![
        hit(
            "usbank.com",
            "https://www.usbank.com/",
            Some("U.S. Bank | Personal & Business Banking"),
            Some("Checking, savings & loans."),
        ),
        hit(
            "usbank-login-help.com",
            "https://usbank-login-help.com/",
            None,
            None,
        ),
    ]
}

async fn get(backend: Arc<FakeBackend>, uri: &str) -> (StatusCode, HeaderMap, String) {
    send(router(backend), uri).await
}

async fn send(app: Router, uri: &str) -> (StatusCode, HeaderMap, String) {
    send_with_headers(app, uri, &[]).await
}

async fn send_with_headers(
    app: Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder().uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

fn backend(hits: Vec<Hit>) -> Arc<FakeBackend> {
    Arc::new(FakeBackend {
        hits,
        ..FakeBackend::default()
    })
}

fn node(status: Status) -> Arc<FakeNode> {
    Arc::new(FakeNode(status))
}

fn node_status(phase: Phase, step: Step) -> Status {
    Status {
        phase,
        step,
        detail: "Downloading the Tranco list of popular sites".to_string(),
        progress: None,
        last_error: None,
        wikidata_missing: false,
        wikidata_error: None,
        sites: 0,
        places: 0,
        index: None,
        last_refresh: None,
        next_refresh: None,
        version: "0.1.0".to_string(),
        network: None,
        fill: None,
        crawl_left: 0,
        background_updates: true,
        paused: None,
        disk_used: 0,
        storage_limit: 0,
        downloaded_today: 0,
        downloaded_total: 0,
        homepages_visited: 0,
        meaning_sites: None,
        meaning_work: None,
        can_restart: false,
        page_coverage: None,
        paused_until: None,
    }
}

/// A ready node that shares popularity and remembers the picks.
struct SharingNode {
    status: Status,
    picks: Mutex<Vec<(String, String)>>,
}

impl StatusSource for SharingNode {
    fn status(&self) -> Status {
        self.status.clone()
    }
    fn shares_popularity(&self) -> bool {
        true
    }
    fn record_pick(&self, query: &str, domain: &str) {
        self.picks
            .lock()
            .unwrap()
            .push((query.to_string(), domain.to_string()));
    }
}

fn no_settings() -> Settings {
    Settings {
        manage: false,
        history: None,
        browser_about: None,
        welcome: false,
        experiment: None,
        options: SearchOptions::default(),
        network: NetSetting::Unavailable,
        scope: plumb_net::SearchScope::default(),
        private: false,
        country_mode: "any".into(),
        debug: false,
        request_fields: Vec::new(),
    }
}

fn scored(domain: &str, score: f32) -> Hit {
    let mut hit = hit(domain, &format!("https://{domain}/"), None, None);
    hit.score = score;
    hit
}

fn answered(hits: Vec<Hit>) -> NetOutcome {
    NetOutcome::Answered(NetworkResults {
        buckets: 4,
        asked: 8,
        answered: 6,
        relayed: 6,
        direct: 0,
        rejected: 0,
        cached: 0,
        pending: 0,
        stale: 0,
        hits: hits.into_iter().map(from_network).collect(),
    })
}

/// Bank sites, with banks around a town guessed from any query ending
/// in "bank"; usbank.com is named by "us bank".
struct BankPlaces;

impl SearchBackend for BankPlaces {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let mut hits = bank_hits();
        hits[0].named = query == "us bank";
        Ok(hits.into_iter().take(limit).collect())
    }

    fn num_docs(&self) -> u64 {
        2
    }

    fn places(
        &self,
        query: &str,
        _home: Option<&str>,
        _country: Option<&str>,
    ) -> Option<plumb_index::places::PlaceResults> {
        use plumb_core::place::Place;
        let (town, _) = query.split_once(' ')?;
        let at = |name: &str, kind: &str, osm: &str| Place {
            name: name.into(),
            kind: kind.into(),
            osm: osm.into(),
            lat: 39.74,
            lon: -104.99,
            country: Some("US".into()),
            ..Place::default()
        };
        Some(plumb_index::places::PlaceResults {
            location: None,
            what: "bank".into(),
            center: Some(at(town, "place=city", "n1")),
            near_me: false,
            guessed: true,
            radius_km: 10.0,
            hits: vec![plumb_index::places::PlaceHit {
                place: Place {
                    website: Some("https://branch.example/denver".into()),
                    ..at("First Branch", "amenity=bank", "n2")
                },
                km: 1.0,
            }],
        })
    }
}

/// Denver sites that only have the town's name, a guide that says
/// "breweries", and breweries around Denver, one of them with a site
/// the index has.
struct BreweryPlaces;

impl SearchBackend for BreweryPlaces {
    fn search(&self, _query: &str, limit: usize) -> Result<Vec<Hit>> {
        let hits = vec![
            hit(
                "denvergov.org",
                "https://denvergov.org/",
                Some("City and County of Denver"),
                None,
            ),
            hit(
                "denverbroncos.com",
                "https://www.denverbroncos.com/",
                Some("Denver Broncos"),
                None,
            ),
            hit(
                "westword.com",
                "https://www.westword.com/",
                Some("Westword"),
                Some("Denver news, the best breweries and bars."),
            ),
        ];
        Ok(hits.into_iter().take(limit).collect())
    }

    fn num_docs(&self) -> u64 {
        4
    }

    fn places(
        &self,
        query: &str,
        _home: Option<&str>,
        _country: Option<&str>,
    ) -> Option<plumb_index::places::PlaceResults> {
        use plumb_core::place::Place;
        if !query.contains("brewery") {
            return None;
        }
        let at = |name: &str, kind: &str, osm: &str, website: Option<&str>| Place {
            name: name.into(),
            kind: kind.into(),
            osm: osm.into(),
            lat: 39.74,
            lon: -104.99,
            country: Some("US".into()),
            website: website.map(str::to_string),
            ..Place::default()
        };
        if query.ends_with("near me") {
            return Some(plumb_index::places::PlaceResults {
                location: None,
                what: "brewery".into(),
                center: None,
                near_me: true,
                guessed: false,
                radius_km: 0.0,
                hits: Vec::new(),
            });
        }
        // A town without any mapped.
        if query.contains("aurora") {
            return Some(plumb_index::places::PlaceResults {
                location: None,
                what: "brewery".into(),
                center: Some(at("Aurora", "place=city", "n5", None)),
                near_me: false,
                guessed: false,
                radius_km: 12.0,
                hits: Vec::new(),
            });
        }
        let brewery =
            |name: &str, osm: &str, website: Option<&str>| plumb_index::places::PlaceHit {
                place: at(name, "craft=brewery", osm, website),
                km: 1.0,
            };
        Some(plumb_index::places::PlaceResults {
            location: None,
            what: "brewery".into(),
            center: Some(at("Denver", "place=city", "n1", None)),
            near_me: false,
            guessed: !query.contains(" in "),
            radius_km: 12.0,
            hits: vec![
                brewery("Great Divide", "n2", Some("https://greatdivide.com/")),
                brewery("Unknown Ales", "n3", Some("https://unknown-ales.example/")),
                brewery("No Site Brewing", "n4", None),
            ],
        })
    }

    fn site(&self, domain: &str) -> Option<Hit> {
        (domain == "greatdivide.com").then(|| {
            hit(
                "greatdivide.com",
                "https://greatdivide.com/",
                Some("Great Divide Brewing Co."),
                None,
            )
        })
    }
}

/// A node that reports a set status.
struct FakeNode(Status);

impl StatusSource for FakeNode {
    fn status(&self) -> Status {
        self.0.clone()
    }
}

fn from_network(hit: Hit) -> NetworkResult {
    NetworkResult {
        hit,
        verified: true,
        crawler: Some("12D3KooWexample".into()),
        confirmed: false,
        answers: 2,
        shared: None,
    }
}
