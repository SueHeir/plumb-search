use plumb_core::keys::{pick_buckets, query_keys, record_keys};
use plumb_core::{LinkText, SafeSearch, Signals, SiteRecord};
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
    index_top_with_relevance(
        records,
        query,
        options,
        n,
        RankConfig::default().whole_query_relevance,
    )
}

fn experimental_index_top(
    records: &[SiteRecord],
    query: &str,
    options: &SearchOptions,
    n: usize,
) -> Vec<String> {
    index_top_with_relevance(records, query, options, n, true)
}

fn index_top_with_relevance(
    records: &[SiteRecord],
    query: &str,
    options: &SearchOptions,
    n: usize,
    whole_query_relevance: bool,
) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index");
    build_index(&path, records).unwrap();
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

fn private_top(records: &[SiteRecord], query: &str, options: &Options, n: usize) -> Vec<String> {
    rank(query, records, options, n)
        .into_iter()
        .map(|hit| hit.domain)
        .collect()
}

fn experimental_rank(
    query: &str,
    records: &[SiteRecord],
    options: &Options,
    n: usize,
) -> Vec<Ranked> {
    rank::rank_with_whole_query_relevance(query, records, options, n, true)
}

fn experimental_private_top(
    records: &[SiteRecord],
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
    let legacy = rank("aster refund", &records, &options, 2);
    assert_eq!(
        legacy,
        rank::rank_with_whole_query_relevance("aster refund", &records, &options, 2, false)
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
            "description" => popular.description = Some(text.into()),
            "intro" => popular.intro = Some(text.into()),
            "summary" => popular.summary = Some(text.into()),
            "about" => popular.about = Some(text.into()),
            "headings" => popular.headings = vec![text.into()],
            "terms" => popular.terms = vec![text.into()],
            "aliases" => popular.aliases = vec![text.into()],
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
        let records: Vec<_> = records
            .into_iter()
            .map(plumb_core::keys::slim_record)
            .collect();
        let hits = search(
            "aster refund",
            &query_keys("aster refund"),
            vec![records],
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
    popular.description = Some("Aster refunds".into());
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
    german.language = Some("de".into());
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
fn a_shared_named_subject_is_required_even_when_task_words_dominate() {
    let mut owner = site(
        "aster.example",
        "Aster research organization",
        Some(1),
        90_000,
    );
    owner.aliases = vec!["Aster".into()];
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
    owner.aliases = vec![subject.into()];
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
    owner.aliases = vec!["Aster Labs".into()];
    owner.description = Some("Banking plans qz 7 API".into());
    let mut decoy = site("aster-labs-online.example", "Aster Labs Online", None, 0);
    decoy.aliases = vec!["Aster Labs".into()];
    decoy.description = Some("Aster Labs online banking qz 7 API".into());
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
            first.aliases = vec!["Nimbus".into()];
            second.aliases = vec!["Nimbus".into()];
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
    owner.aliases = vec!["Nimbus Research".into()];
    owner.signals.official_site = true;
    owner.about = Some("Research organization".into());
    owner.url = Some("https://other.example/".into());
    let mut rival = site("nimbus.example", "Nimbus Research guidance", None, 0);
    rival.aliases = vec!["Nimbus".into()];
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
    owner.aliases = vec!["Aster Research".into()];
    let records = [owner, SiteRecord::new("asterresearch.fr")];
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
    owner.aliases = vec!["Aster".into()];
    let mut copier = SiteRecord::new("asterrefund.test");
    copier.signals.tranco_rank = Some(500_000);
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

#[test]
fn copies_keep_the_less_favorable_popularity() {
    let mut boosted = site("phish.example", "US Bank", Some(1), 90_000);
    boosted.signals.official_site = true;
    let honest = site("phish.example", "Other text", Some(800_000), 2);
    let merged = rank::merge_copies(vec![boosted, honest]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].title.as_deref(), Some("US Bank"));
    assert_eq!(merged[0].signals.tranco_rank, Some(800_000));
    assert_eq!(merged[0].signals.linking_domains, 2);
    assert!(!merged[0].signals.official_site);
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
    german.language = Some("de".into());
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
