use plumb_core::keys::{pick_buckets, query_keys, site_keys, slim_site};
use plumb_core::site::{self, Item};
use plumb_core::{LinkText, SafeSearch, Signals, SiteRecord};
use plumb_index::{build_index, RankConfig, SearchOptions, Searcher};

use super::*;

fn site(domain: &str, title: &str, tranco: Option<u32>, linking: u32) -> Item {
    let mut r = site::new(domain);
    r.title = Some(title.to_string());
    site::set_signals(
        &mut r,
        &Signals {
            tranco_rank: tranco,
            linking_domains: linking,
            ..Signals::default()
        },
    );
    r
}

/// `sites` as they are, not cleaned: the sites ranked here.
fn items(sites: &[Item]) -> Vec<Item> {
    sites.to_vec()
}

fn corpus() -> Vec<Item> {
    let mut usbank = site(
        "usbank.com",
        "U.S. Bank | Checking & Savings",
        Some(900),
        4000,
    );
    site::set_link_texts(&mut usbank, &[LinkText::with_count("US Bank", 40)]);
    usbank.names = vec!["U.S. Bancorp".into()];
    let mut signals = site::signals(&usbank);
    signals.official_site = true;
    site::set_signals(&mut usbank, &signals);
    site::set_country(&mut usbank, Some("US".into()));
    site::set_kinds(&mut usbank, vec!["bank".into()]);
    let mut chase = site("chase.com", "Chase Bank", Some(200), 9000);
    site::set_country(&mut chase, Some("US".into()));
    site::set_kinds(&mut chase, vec!["bank".into()]);
    let mut dkb = site("dkb.de", "DKB - Das Kundenkonto", Some(5000), 800);
    site::set_kinds(&mut dkb, vec!["bank".into()]);
    let mut irs = site("irs.gov", "Internal Revenue Service", Some(300), 7000);
    irs.about = Some("Get a tax refund and pay taxes".into());
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
fn index_top(records: &[Item], query: &str, options: &SearchOptions, n: usize) -> Vec<String> {
    index_top_with_relevance(
        records,
        query,
        options,
        n,
        RankConfig::default().whole_query_relevance,
    )
}

fn experimental_index_top(
    records: &[Item],
    query: &str,
    options: &SearchOptions,
    n: usize,
) -> Vec<String> {
    index_top_with_relevance(records, query, options, n, true)
}

fn index_top_with_relevance(
    records: &[Item],
    query: &str,
    options: &SearchOptions,
    n: usize,
    whole_query_relevance: bool,
) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index");
    // The index is built from sites cleaned, as a node holds them.
    let mut sites = records.to_vec();
    sites.iter_mut().for_each(site::clean);
    build_index(&path, &sites).unwrap();
    // The browser does not correct typos.
    let options = SearchOptions {
        exact: true,
        ..options.clone()
    };
    Searcher::open(&path)
        .unwrap()
        .search_full(
            query,
            n,
            &RankConfig {
                whole_query_relevance,
                ..RankConfig::default()
            },
            &options,
        )
        .unwrap()
        .hits
        .into_iter()
        .map(|hit| hit.domain)
        .collect()
}

fn private_top(records: &[Item], query: &str, options: &Options, n: usize) -> Vec<String> {
    rank(query, &items(records), options, n)
        .into_iter()
        .map(|hit| hit.domain)
        .collect()
}

fn experimental_rank(query: &str, records: &[Item], options: &Options, n: usize) -> Vec<Ranked> {
    rank::rank_with_whole_query_relevance(query, &items(records), options, n, true)
}

fn experimental_private_top(
    records: &[Item],
    query: &str,
    options: &Options,
    n: usize,
) -> Vec<String> {
    experimental_rank(query, records, options, n)
        .into_iter()
        .map(|hit| hit.domain)
        .collect()
}

#[test]
fn constants_match_the_index_defaults() {
    let cfg = RankConfig::default();
    assert!(!cfg.whole_query_relevance);
    assert_eq!(
        rank::DEFAULT_WHOLE_QUERY_RELEVANCE,
        cfg.whole_query_relevance
    );
    assert_eq!(rank::ALPHA, cfg.alpha);
    assert_eq!(Some(rank::DESCRIBED_ALPHA), cfg.described_alpha);
    assert_eq!(Some(rank::DESCRIBED_RELEVANCE), cfg.described_relevance);
    assert_eq!(
        Some(rank::NAVIGATIONAL_RELEVANCE),
        cfg.navigational_relevance
    );
    assert_eq!(Some(rank::QUESTION_RELEVANCE), cfg.question_relevance);
    assert_eq!(rank::EXACT_LABEL_BONUS, cfg.exact_label_bonus);
    assert_eq!(rank::EXACT_ALIAS_BONUS, cfg.exact_alias_bonus);
    assert_eq!(rank::TRUSTED_LINK_SCORE, cfg.trusted_link_score);
    assert_eq!(rank::UNTRUSTED_SHARE, cfg.untrusted_share);
    assert_eq!(rank::KIND_BONUS, cfg.kind_bonus);
    assert_eq!(Some(rank::NAMED_SHARE), cfg.named_share);
    assert_eq!(
        rank::WELL_KNOWN_LINK_SCORE,
        plumb_index::WELL_KNOWN_LINK_SCORE
    );
    assert_eq!(rank::COUNTRY_BOOST, cfg.country_boost);
}

#[test]
fn default_off_keeps_native_partial_name_blend_and_explicit_opt_in() {
    let records = [
        site("aster.example", "Aster Observatory", Some(1), 90_000),
        site("nimbus.example", "Aster refund information", None, 0),
    ];
    let options = Options::default();
    let legacy = rank("aster refund", &items(&records), &options, 2);
    assert_eq!(
        legacy,
        rank::rank_with_whole_query_relevance("aster refund", &items(&records), &options, 2, false)
    );
    assert_eq!(
        private_top(&records, "aster refund", &options, 1),
        index_top(&records, "aster refund", &SearchOptions::default(), 1)
    );
    let owner = legacy
        .iter()
        .find(|hit| hit.domain == "aster.example")
        .unwrap();
    let expected = rank::ALPHA * owner.link_score
        + (1.0 - rank::ALPHA) * owner.text_score
        + rank::EXACT_LABEL_BONUS / 2.0;
    assert!((owner.score - expected).abs() < 1e-6, "{owner:?}");
    assert_eq!(
        experimental_private_top(&records, "aster refund", &options, 1),
        ["nimbus.example"]
    );
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
fn unsupported_qualifiers_do_not_inherit_a_partial_names_popularity() {
    for qualifier in [
        "refund",
        "qz",
        "7",
        "api",
        "manual",
        "government",
        "agency",
        "project",
        "benefits",
        "baggage policy",
        "website address",
        "student login",
    ] {
        let query = format!("aster {qualifier}");
        let popular = site("aster.example", "Aster Observatory", Some(1), 90_000);
        let relevant = site("nimbus.example", &format!("{query} information"), None, 0);
        let records = [popular, relevant];
        let expected = ["nimbus.example"];
        assert_eq!(
            experimental_index_top(&records, &query, &SearchOptions::default(), 1),
            expected,
            "native {query}"
        );
        assert_eq!(
            experimental_private_top(&records, &query, &Options::default(), 1),
            expected,
            "private {query}"
        );
        let ranked = experimental_rank(&query, &records, &Options::default(), 2);
        assert!(ranked.windows(2).all(|pair| pair[0].score > pair[1].score));
    }
}

#[test]
fn every_substantive_field_can_support_a_partial_names_task() {
    for field in [
        "title",
        "description",
        "intro",
        "summary",
        "about",
        "headings",
        "terms",
        "aliases",
    ] {
        let mut popular = site("aster.example", "Aster Observatory", Some(1), 90_000);
        let text = "Aster Observatory refund information";
        match field {
            "title" => popular.title = Some(text.into()),
            "description" => popular.about = Some(text.into()),
            "intro" => site::set_intro(&mut popular, Some(text.into())),
            "summary" => site::set_summary(&mut popular, Some(text.into())),
            "about" => site::set_wikidata_about(&mut popular, Some(text.into())),
            "headings" => site::set_headings(&mut popular, vec![text.into()]),
            "terms" => site::set_terms(&mut popular, vec![text.into()]),
            "aliases" => popular.names = vec![text.into()],
            _ => unreachable!(),
        }
        let records = [
            popular,
            site("nimbus.example", "Aster refund information", None, 0),
        ];
        let expected = ["aster.example"];
        assert_eq!(
            experimental_index_top(&records, "aster refund", &SearchOptions::default(), 1),
            expected,
            "native {field}"
        );
        assert_eq!(
            experimental_private_top(&records, "aster refund", &Options::default(), 1),
            expected,
            "private {field}"
        );
        // Bucket trimming preserves this evidence for browser searches.
        let sites: Vec<_> = items(&records).into_iter().map(slim_site).collect();
        let hits = search(
            "aster refund",
            &query_keys("aster refund"),
            vec![sites],
            &Options::default(),
            1,
        );
        assert_eq!(hits[0].domain, "aster.example", "bucket {field}");
    }
}

#[test]
fn navigation_preserves_the_entire_subject() {
    for (query, title, expected) in [
        ("aster login", "Aster login information", "aster.example"),
        (
            "where can i find aster login",
            "Aster login information",
            "aster.example",
        ),
        (
            "aster qz login",
            "Aster qz login information",
            "nimbus.example",
        ),
        (
            "aster government website",
            "Aster government website",
            "nimbus.example",
        ),
        ("aster agency docs", "Aster agency docs", "nimbus.example"),
    ] {
        let records = [
            site("aster.example", "Aster Observatory", Some(1), 90_000),
            site("nimbus.example", title, None, 0),
        ];
        assert_eq!(
            experimental_index_top(&records, query, &SearchOptions::default(), 1),
            [expected],
            "native {query}"
        );
        assert_eq!(
            experimental_private_top(&records, query, &Options::default(), 1),
            [expected],
            "private {query}"
        );
    }
}

#[test]
fn a_redirected_homepages_words_do_not_establish_the_original_sites_task() {
    let mut popular = site("aster.example", "Aster refund information", Some(1), 90_000);
    popular.about = Some("Aster refunds".into());
    popular.url = Some("https://nimbus.example/".into());
    let records = [
        popular,
        site("nimbus.example", "Aster refund information", None, 0),
    ];
    assert_eq!(
        experimental_private_top(&records, "aster refund", &Options::default(), 1),
        ["nimbus.example"]
    );
    assert_eq!(
        experimental_private_top(&records, "aster refund", &Options::default(), 1),
        experimental_index_top(&records, "aster refund", &SearchOptions::default(), 1)
    );
}

#[test]
fn exact_domain_names_keep_the_native_language_exception() {
    let mut german = site("aster.de", "Aster Observatory", Some(1), 90_000);
    german.lang = Some("de".into());
    let records = [german, site("nimbus.example", "Aster lab", None, 0)];
    let private_options = Options {
        language: Some("en".into()),
        ..Options::default()
    };
    let index_options = SearchOptions {
        language: Some("en".into()),
        ..SearchOptions::default()
    };
    for query in ["aster", "aster.de", "https://aster.de/"] {
        assert_eq!(
            private_top(&records, query, &private_options, 1),
            ["aster.de"]
        );
        assert_eq!(
            private_top(&records, query, &private_options, 1),
            index_top(&records, query, &index_options, 1),
            "{query}"
        );
    }
}

#[test]
fn the_language_filter_keeps_the_nodes_exceptions() {
    let mut rakuten = site("rakuten.example", "楽天市場", Some(300), 50_000);
    rakuten.lang = Some("ja".into());
    let mut madrid = site("realmadrid.example", "Real Madrid CF", Some(300), 50_000);
    madrid.lang = Some("es".into());
    let records = [rakuten, madrid];
    let private_options = Options {
        language: Some("en".into()),
        ..Options::default()
    };
    let index_options = SearchOptions {
        language: Some("en".into()),
        ..SearchOptions::default()
    };
    // Written in the site's script, and named by two words of a well-known
    // site.
    for (query, domain) in [
        ("楽天市場", "rakuten.example"),
        ("real madrid tickets", "realmadrid.example"),
    ] {
        assert_eq!(
            private_top(&records, query, &private_options, 1),
            [domain],
            "{query}"
        );
        assert_eq!(
            private_top(&records, query, &private_options, 1),
            index_top(&records, query, &index_options, 1),
            "{query}"
        );
    }
}

#[test]
fn a_site_an_alias_names_in_full_keeps_its_language_too() {
    // Written in Vietnamese, which the index tells by its letters.
    let mut railways = site(
        "vr.example",
        "Tổng Công Ty Đường Sắt Việt Nam",
        Some(1),
        90_000,
    );
    railways.names = vec!["Vietnam Railways".into()];
    let records = [
        railways,
        site("nimbus.example", "Vietnam travel guide", None, 0),
    ];
    let private_options = Options {
        language: Some("en".into()),
        ..Options::default()
    };
    let index_options = SearchOptions {
        language: Some("en".into()),
        ..SearchOptions::default()
    };
    assert_eq!(
        private_top(&records, "vietnam railways", &private_options, 1),
        ["vr.example"]
    );
    assert_eq!(
        private_top(&records, "vietnam railways", &private_options, 1),
        index_top(&records, "vietnam railways", &index_options, 1)
    );
}

#[test]
fn a_shared_named_subject_is_required_even_when_task_words_dominate() {
    let mut owner = site(
        "aster.example",
        "Aster research organization",
        Some(1),
        90_000,
    );
    owner.names = vec!["Aster".into()];
    let records = [
        owner,
        site(
            "nimbus.example",
            "Refund claim policy guidance status",
            Some(2),
            100_000,
        ),
    ];
    let query = "aster refund claim policy guidance status";
    let ranked = experimental_rank(query, &records, &Options::default(), 2);
    assert_eq!(ranked[0].domain, "aster.example");
    assert_eq!(ranked[1].text_score, 0.0);
    assert_eq!(ranked[1].score, 0.0);
    assert_eq!(
        experimental_private_top(&records, query, &Options::default(), 1),
        experimental_index_top(&records, query, &SearchOptions::default(), 1)
    );
    let subject = "Aster Nimbus Zephyr Lunar Observatory Institute Lab";
    let mut owner = site("aster.example", subject, Some(1), 90_000);
    owner.names = vec![subject.into()];
    let records = [
        owner,
        site("nimbus.example", "Refund guidance", Some(2), 100_000),
    ];
    let query = format!("{subject} refund");
    assert_eq!(
        experimental_private_top(&records, &query, &Options::default(), 1),
        ["aster.example"]
    );
    assert_eq!(
        experimental_private_top(&records, &query, &Options::default(), 1),
        experimental_index_top(&records, &query, &SearchOptions::default(), 1)
    );
}

#[test]
fn alias_subjects_precede_longer_domains_and_preserve_supported_qualifiers() {
    let mut owner = site("aster.example", "Aster Labs", Some(1), 90_000);
    owner.names = vec!["Aster Labs".into()];
    owner.about = Some("Banking plans qz 7 API".into());
    let mut decoy = site("aster-labs-online.example", "Aster Labs Online", None, 0);
    decoy.names = vec!["Aster Labs".into()];
    decoy.about = Some("Aster Labs online banking qz 7 API".into());
    let records = [owner, decoy];
    for query in [
        "aster labs online banking",
        "aster labs qz api",
        "aster labs 7 api",
    ] {
        assert_eq!(
            experimental_private_top(&records, query, &Options::default(), 1),
            ["aster.example"]
        );
        assert_eq!(
            experimental_private_top(&records, query, &Options::default(), 1),
            experimental_index_top(&records, query, &SearchOptions::default(), 1),
            "{query}"
        );
    }
}

#[test]
fn descriptive_queries_survive_self_labels_and_competing_unsupported_aliases() {
    for (aliases, first_domain, second_domain) in [
        (false, "nimbus.example", "nimbus-town.example"),
        (true, "nimbus.example", "nimbus-town.example"),
        (true, "first.github.io", "second.github.io"),
    ] {
        let mut first = site(first_domain, "Nimbus village", Some(1), 90_000);
        let mut second = site(second_domain, "Nimbus magazine", Some(2), 80_000);
        if aliases {
            first.names = vec!["Nimbus".into()];
            second.names = vec!["Nimbus".into()];
        }
        let records = [
            first,
            second,
            site("aster.example", "Research project guidance status", None, 0),
        ];
        let query = "nimbus research project guidance status";
        let ranked = experimental_rank(query, &records, &Options::default(), 3);
        assert_eq!(ranked[0].domain, "aster.example");
        assert_eq!(
            experimental_private_top(&records, query, &Options::default(), 1),
            experimental_index_top(&records, query, &SearchOptions::default(), 1)
        );
    }
}

#[test]
fn complete_alias_identity_survives_borrowed_homepage_and_partial_rivals() {
    let mut owner = site("aster.example", "Borrowed portal", Some(1), 90_000);
    owner.names = vec!["Nimbus Research".into()];
    let mut signals = site::signals(&owner);
    signals.official_site = true;
    site::set_signals(&mut owner, &signals);
    site::set_wikidata_about(&mut owner, Some("Research organization".into()));
    owner.url = Some("https://other.example/".into());
    let mut rival = site("nimbus.example", "Nimbus Research guidance", None, 0);
    rival.names = vec!["Nimbus".into()];
    let records = [owner, rival];
    let query = "nimbus research";
    assert_eq!(
        experimental_private_top(&records, query, &Options::default(), 1),
        ["aster.example"]
    );
    assert_eq!(
        experimental_private_top(&records, query, &Options::default(), 1),
        experimental_index_top(&records, query, &SearchOptions::default(), 1)
    );
}

#[test]
fn a_complete_hostname_written_as_words_keeps_address_lookup() {
    let mut owner = site("aa.example", "Aster Research", Some(1), 90_000);
    owner.names = vec!["Aster Research".into()];
    let records = [owner, site::new("asterresearch.fr")];
    let query = "asterresearch fr";
    assert_eq!(
        experimental_private_top(&records, query, &Options::default(), 1),
        ["asterresearch.fr"]
    );
    assert_eq!(
        experimental_private_top(&records, query, &Options::default(), 1),
        experimental_index_top(&records, query, &SearchOptions::default(), 1)
    );
}

#[test]
fn a_bare_query_domain_needs_corroboration_except_when_typed() {
    let mut owner = site(
        "aster.example",
        "Aster research organization",
        Some(1),
        90_000,
    );
    owner.names = vec!["Aster".into()];
    let mut copier = site::new("asterrefund.test");
    let mut signals = site::signals(&copier);
    signals.tranco_rank = Some(500_000);
    site::set_signals(&mut copier, &signals);
    let records = [owner, copier];
    let ranked = experimental_rank("aster refund", &records, &Options::default(), 2);
    assert_eq!(ranked[0].domain, "aster.example");
    assert_eq!(ranked[1].score, 0.0);
    for query in ["aster refund", "asterrefund.test"] {
        assert_eq!(
            experimental_private_top(&records, query, &Options::default(), 1),
            experimental_index_top(&records, query, &SearchOptions::default(), 1),
            "{query}"
        );
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
                exact: false,
                ..SearchOptions::default()
            },
            3,
        );
        let private = private_top(
            &records,
            "banks",
            &Options {
                country: Some(country.into()),
                only_country: true,
                ..Options::default()
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
            ..Options::default()
        },
        3,
    );
    // Sites of no country that say "bank" come after it.
    assert_eq!(de[0], "dkb.de");
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
    let answers = vec![items(&records), items(&records[..2])];
    let hits = search("us bank", &keys, answers, &Options::default(), 10);
    assert_eq!(hits[0].domain, "usbank.com");
    let domains: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
    assert!(!domains.contains(&"irs.gov"), "{domains:?}");
    let mut unique = domains.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), domains.len());
    let sites = items(&records);
    for hit in &hits {
        let site = sites.iter().find(|s| s.id == hit.domain).unwrap();
        assert!(query_keys("us bank")
            .iter()
            .any(|k| site_keys(site).contains(k)));
    }
}

/// The record a node of an earlier version sent of `site`.
fn record(site: &Item) -> SiteRecord {
    SiteRecord::from_stored_item(site).unwrap()
}

/// A bucket holds trimmed items, or, as a browser may still have cached
/// it from a node of an earlier version, trimmed records' JSON; either
/// ranks the same.
#[test]
fn buckets_read_back_in_either_form_and_skip_bad_sites() {
    let records = corpus();
    let slim: Vec<Item> = items(&records).into_iter().map(slim_site).collect();
    let page = r#"{"id": "page.example", "kind": "page"}"#;
    let json = format!(
        "[{}, {{\"domain\": 5}}, {{\"id\": 5}}, {page}, {}]",
        serde_json::to_string(&slim[0]).unwrap(),
        serde_json::to_string(&record(&records[1])).unwrap()
    );
    let sites = read_bucket(&json).unwrap();
    let domains: Vec<&str> = sites.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(domains, ["usbank.com", "chase.com"]);
    assert_eq!(sites[1], records[1]);
    assert!(read_bucket("not json").is_err());
    assert!(read_bucket(&" ".repeat(MAX_BUCKET_BYTES + 1)).is_err());

    let as_items = serde_json::to_string(&slim).unwrap();
    let as_records: Vec<SiteRecord> = records.iter().map(record).collect();
    let as_records = serde_json::to_string(&as_records).unwrap();
    let (_, keys) = pick_buckets("us bank", || 3);
    let found = |json: &str| {
        let answers = vec![read_bucket(json).unwrap()];
        search("us bank", &keys, answers, &Options::default(), 10)
    };
    let (of_items, of_records) = (found(&as_items), found(&as_records));
    assert_eq!(of_items.len(), of_records.len());
    assert_eq!(of_items[0].domain, "usbank.com");
    for (a, b) in of_items.iter().zip(&of_records) {
        // Scores may differ in their last bit from one ranking to the next.
        assert!((a.score - b.score).abs() < 1e-5, "{a:?} {b:?}");
        let rounded = |r: &Ranked| Ranked {
            score: 0.0,
            text_score: 0.0,
            link_score: 0.0,
            ..r.clone()
        };
        assert_eq!(rounded(a), rounded(b));
    }
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

#[test]
fn copies_keep_the_less_favorable_popularity() {
    let mut boosted = site("phish.example", "US Bank", Some(1), 90_000);
    let mut signals = site::signals(&boosted);
    signals.official_site = true;
    site::set_signals(&mut boosted, &signals);
    let honest = site("phish.example", "Other text", Some(800_000), 2);
    let merged = rank::merge_copies(items(&[boosted, honest]));
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].title.as_deref(), Some("US Bank"));
    let signals = site::signals(&merged[0]);
    assert_eq!(signals.tranco_rank, Some(800_000));
    assert_eq!(signals.linking_domains, 2);
    assert!(!signals.official_site);
}

#[test]
fn safe_search_and_language_filter_as_in_the_index() {
    let mut records = corpus();
    records.push(site(
        "freeporn.example",
        "Free bank videos",
        Some(3000),
        300,
    ));
    let mut german = site("bankde.example", "Bank Deutschland", Some(3500), 300);
    german.lang = Some("de".into());
    records.push(german);
    let cases = [
        (SafeSearch::Off, None),
        (SafeSearch::Moderate, None),
        (SafeSearch::Strict, Some("en")),
        (SafeSearch::Moderate, Some("de")),
    ];
    for (safe, language) in cases {
        let language = language.map(str::to_string);
        let index = index_top(
            &records,
            "bank",
            &SearchOptions {
                safe,
                language: language.clone(),
                ..SearchOptions::default()
            },
            10,
        );
        let private = private_top(
            &records,
            "bank",
            &Options {
                safe,
                language,
                ..Options::default()
            },
            10,
        );
        // The simpler text match may order the tail differently.
        let (mut private, mut index) = (private, index);
        private.sort();
        index.sort();
        assert_eq!(private, index, "{safe:?}");
    }
    let o = Options::default();
    assert!(!private_top(&records, "bank", &o, 10).contains(&"freeporn.example".to_string()));
}

#[test]
fn operators_narrow_as_in_the_index() {
    let records = corpus();
    let o = Options::default();
    for query in [
        "bank site:usbank.com",
        "bank -chase",
        "\"us bank\"",
        "tax site:gov",
    ] {
        assert_eq!(
            private_top(&records, query, &o, 5),
            index_top(&records, query, &SearchOptions::default(), 5),
            "{query}"
        );
    }
    assert_eq!(
        private_top(&records, "bank site:usbank.com", &o, 5),
        ["usbank.com"]
    );
    assert!(!private_top(&records, "bank -chase", &o, 5).contains(&"chase.com".to_string()));
}

#[test]
fn script_language_filters_keep_multilingual_controls_and_native_private_parity() {
    // Frozen synthetic excerpts exercise filtering, not production domain rules.
    // Every document contains the same generic query word; no candidate is a
    // domain-name match, so language filtering alone changes the returned set.
    let cases = [
        (
            None,
            "Lab — научные исследования технологии и материалы",
            false,
        ),
        (None, "Lab — 中國科學研究技術資料文獻庫", false),
        (None, "Lab — 日本の科学研究資料と技術情報", false),
        (None, "Lab — 한국의 과학 연구 자료와 기술 정보", false),
        (None, "Lab — البحوث العلمية والتقنية والمعلومات", false),
        (None, "Lab in the English language", true),
        (None, "Lab und wissenschaftliche Veröffentlichungen", true),
        (
            None,
            "Lab 中文社区 English language research community",
            true,
        ),
        (None, "Lab — Олена Коваль and English publications", true),
        (None, "Lab 研究所", true),
        (None, "Lab ١٢٣٤٥٦٧٨٩٠،؛؟", true),
        (
            Some("en"),
            "Lab — научные исследования технологии и материалы",
            true,
        ),
        (Some("en-US"), "Lab — 中國科學研究技術資料文獻庫", true),
        (
            Some("de"),
            "Lab und wissenschaftliche Veröffentlichungen",
            false,
        ),
        (Some("az"), "Lab — Азәрбајҹан елми арашдырмалар", false),
        (Some("uz"), "Lab — Ўзбекистон илмий тадқиқотлар", false),
        (Some("kk"), "Lab — قازاقستان غىلىمى زەرتتەۋ", false),
        (Some("ko"), "Lab — 韓國學術研究資料", false),
        (Some("ja"), "Lab — 日本學術研究資料", false),
    ];
    let records: Vec<_> = cases
        .iter()
        .enumerate()
        .map(|(i, (language, title, _))| {
            let mut record = site(&format!("sample{i}.example"), title, Some(5000), 300);
            record.lang = language.map(str::to_string);
            record
        })
        .collect();
    let limit = records.len();
    let compare = |query: &str, language: Option<&str>| {
        let language = language.map(str::to_string);
        let mut native = index_top(
            &records,
            query,
            &SearchOptions {
                language: language.clone(),
                ..SearchOptions::default()
            },
            limit,
        );
        let mut private = private_top(
            &records,
            query,
            &Options {
                language,
                ..Options::default()
            },
            limit,
        );
        native.sort();
        private.sort();
        assert_eq!(native, private, "{query}");
        native
    };
    assert_eq!(compare("lab", None).len(), limit);
    let english = compare("lab", Some("en"));
    for (i, (_, _, keep)) in cases.iter().enumerate() {
        assert_eq!(english.contains(&records[i].id), *keep, "{}", cases[i].1);
    }
    // Shared scripts keep candidates without asserting a particular language.
    for (language, expected) in [
        ("ru", 0),
        ("uk", 0),
        ("zh", 1),
        ("ja", 2),
        ("ko", 3),
        ("ar", 4),
    ] {
        assert!(
            compare("lab", Some(language)).contains(&records[expected].id),
            "{language}"
        );
    }
    // Secondary scripts and unknown language mappings must not lose candidates.
    for language in ["az", "uz", "kk", "pa", "bo"] {
        let found = compare("lab", Some(language));
        let expected = if language == "kk" || language == "pa" {
            4
        } else {
            0
        };
        assert!(found.contains(&records[expected].id), "{language}");
    }
    assert!(
        compare("lab", Some("ko")).contains(&records[1].id),
        "Han alone can also be Korean"
    );
    // Valid declarations override the fallback, including Han-heavy Korean.
    for (language, index) in [("az", 14), ("uz", 15), ("kk", 16), ("ko", 17), ("ja", 18)] {
        assert!(compare("lab", Some(language)).contains(&records[index].id));
    }
    // Full typed names retain the current native/private exception.
    for i in [0, 1, 13, 17] {
        assert_eq!(
            compare(&records[i].id, Some("en")),
            vec![records[i].id.clone()]
        );
    }
    // Native-script queries use the same filter semantics, rather than an
    // English-only query special case.
    for (query, language, index) in [("научные", "ru", 0), ("البحوث", "ar", 4)] {
        assert!(compare(query, Some(language)).contains(&records[index].id));
    }
}
