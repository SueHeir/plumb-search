use plumb_core::keys::{pick_buckets, query_keys, record_keys};
use plumb_core::{LinkText, Signals, SiteRecord};
use plumb_index::{build_index, RankConfig, SearchOptions, Searcher};

use super::*;

fn site(domain: &str, title: &str, tranco: Option<u32>, linking: u32) -> SiteRecord {
    let mut r = SiteRecord::new(domain);
    r.title = Some(title.to_string());
    r.signals = Signals {
        tranco_rank: tranco,
        linking_domains: linking,
        ..Signals::default()
    };
    r
}

fn corpus() -> Vec<SiteRecord> {
    let mut usbank = site(
        "usbank.com",
        "U.S. Bank | Checking & Savings",
        Some(900),
        4000,
    );
    usbank.link_texts = vec![LinkText::with_count("US Bank", 40)];
    usbank.aliases = vec!["U.S. Bancorp".into()];
    usbank.signals.official_site = true;
    usbank.country = Some("US".into());
    usbank.kinds = vec!["bank".into()];
    let mut chase = site("chase.com", "Chase Bank", Some(200), 9000);
    chase.country = Some("US".into());
    chase.kinds = vec!["bank".into()];
    let mut dkb = site("dkb.de", "DKB - Das Kundenkonto", Some(5000), 800);
    dkb.kinds = vec!["bank".into()];
    let mut irs = site("irs.gov", "Internal Revenue Service", Some(300), 7000);
    irs.description = Some("Get a tax refund and pay taxes".into());
    vec![
        usbank,
        chase,
        dkb,
        irs,
        site(
            "usbank-login-help.com",
            "US Bank Login Help | us bank login",
            None,
            0,
        ),
        site("bank.com", "Bank", Some(60_000), 50),
        site("refundtracker.net", "IRS refund tracker", Some(400_000), 3),
        site("example.org", "Example Domain", Some(2000), 500),
    ]
}

/// The top domains plumb-index gives `query` over `records`.
fn index_top(
    records: &[SiteRecord],
    query: &str,
    options: &SearchOptions,
    n: usize,
) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index");
    build_index(&path, records).unwrap();
    Searcher::open(&path)
        .unwrap()
        .search_full(query, n, &RankConfig::default(), options)
        .unwrap()
        .hits
        .into_iter()
        .map(|hit| hit.domain)
        .collect()
}

fn private_top(records: &[SiteRecord], query: &str, options: &Options, n: usize) -> Vec<String> {
    rank(query, records, options, n)
        .into_iter()
        .map(|hit| hit.domain)
        .collect()
}

#[test]
fn constants_match_the_index_defaults() {
    let cfg = RankConfig::default();
    assert_eq!(rank::ALPHA, cfg.alpha);
    assert_eq!(rank::EXACT_LABEL_BONUS, cfg.exact_label_bonus);
    assert_eq!(rank::EXACT_ALIAS_BONUS, cfg.exact_alias_bonus);
    assert_eq!(rank::TRUSTED_LINK_SCORE, cfg.trusted_link_score);
    assert_eq!(rank::UNTRUSTED_SHARE, cfg.untrusted_share);
    assert_eq!(rank::KIND_BONUS, cfg.kind_bonus);
    assert_eq!(rank::COUNTRY_BOOST, cfg.country_boost);
}

#[test]
fn first_results_agree_with_the_index() {
    let records = corpus();
    for query in [
        "us bank",
        "U.S. Bank",
        "us bank login",
        "chase",
        "irs refund",
        "usbank.com",
        "https://www.usbank.com/",
        "internal revenue service",
        "example",
    ] {
        let index = index_top(&records, query, &SearchOptions::default(), 1);
        let private = private_top(&records, query, &Options::default(), 1);
        assert_eq!(private, index, "{query:?}");
    }
}

#[test]
fn kinds_and_countries_rank_as_in_the_index() {
    let records = corpus();
    for country in ["US", "DE"] {
        let index = index_top(
            &records,
            "banks",
            &SearchOptions {
                country: Some(country.into()),
                only_country: true,
            },
            3,
        );
        let private = private_top(
            &records,
            "banks",
            &Options {
                country: Some(country.into()),
                only_country: true,
            },
            3,
        );
        assert_eq!(private, index, "{country}");
    }
    let de = private_top(
        &records,
        "banks",
        &Options {
            country: Some("DE".into()),
            only_country: true,
        },
        3,
    );
    assert_eq!(de, vec!["dkb.de"]);
}

#[test]
fn look_alikes_stay_below_the_brand() {
    let top = private_top(&corpus(), "us bank login", &Options::default(), 2);
    assert_eq!(top[0], "usbank.com");
}

#[test]
fn search_keeps_only_sites_with_the_query_keys_once_each() {
    let records = corpus();
    let (_, keys) = pick_buckets("us bank", || 3);
    // The same site in two buckets, and a site the keys do not find.
    let answers = vec![records.clone(), records[..2].to_vec()];
    let hits = search("us bank", &keys, answers, &Options::default(), 10);
    assert_eq!(hits[0].domain, "usbank.com");
    let domains: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
    assert!(!domains.contains(&"irs.gov"), "{domains:?}");
    let mut unique = domains.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), domains.len());
    for hit in &hits {
        let record = records.iter().find(|r| r.domain == hit.domain).unwrap();
        assert!(query_keys("us bank")
            .iter()
            .any(|k| record_keys(record).contains(k)));
    }
}

#[test]
fn buckets_read_back_and_skip_bad_records() {
    let json = format!(
        "[{}, {{\"domain\": 5}}, {}]",
        serde_json::to_string(&corpus()[0]).unwrap(),
        serde_json::to_string(&corpus()[1]).unwrap()
    );
    let sites = read_bucket(&json).unwrap();
    assert_eq!(sites.len(), 2);
    assert!(read_bucket("not json").is_err());
    assert!(read_bucket(&" ".repeat(MAX_BUCKET_BYTES + 1)).is_err());
}

#[test]
fn only_web_links_are_shown() {
    assert_eq!(
        safe_href(" https://www.usbank.com/ ").as_deref(),
        Some("https://www.usbank.com/")
    );
    assert_eq!(safe_href("javascript:alert(1)"), None);
    assert_eq!(safe_href("data:text/html,hi"), None);
    assert_eq!(safe_href("not a url"), None);
}

#[test]
fn countries_come_from_language_tags() {
    assert_eq!(language_country("en-US").as_deref(), Some("US"));
    assert_eq!(language_country("de_DE").as_deref(), Some("DE"));
    assert_eq!(language_country("en-GB").as_deref(), Some("GB"));
    assert_eq!(language_country("zh-Hant-TW").as_deref(), Some("TW"));
    assert_eq!(language_country("fr"), None);
}

#[test]
fn queries_come_from_the_fragment() {
    let decode = |s: &str| Some(s.replace("%20", " "));
    assert_eq!(query_from_fragment("#q=us%20bank", decode), "us bank");
    assert_eq!(query_from_fragment("#x=1&q=us+bank", decode), "us bank");
    assert_eq!(query_from_fragment("", decode), "");
    assert_eq!(query_from_fragment("#q=%E0%A4%A", |_| None), "");
}

#[test]
fn padding_repeats_for_a_query_in_one_browser_only() {
    let buckets = |secret: &[u8], query: &str| {
        let mut picked = pick_buckets(query, padding(secret, query)).0;
        picked.sort();
        picked
    };
    assert_eq!(buckets(b"one", "us bank"), buckets(b"one", "U.S.  Bank"));
    assert_ne!(buckets(b"one", "us bank"), buckets(b"two", "us bank"));
    assert_ne!(buckets(b"one", "chase"), buckets(b"one", "irs"));
    // Same order too: the shuffle draws from the same source.
    assert_eq!(
        pick_buckets("chase", padding(b"one", "chase")).0,
        pick_buckets("chase", padding(b"one", "chase")).0
    );
}
