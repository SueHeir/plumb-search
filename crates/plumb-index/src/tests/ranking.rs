//! How scores blend text and popularity, intent words, and the fields hits
//! carry.

use super::*;

#[test]
fn brand_plus_intent_queries_find_the_official_site() {
    let (_dir, searcher) = build(&lookalike_corpus());
    for (query, official, lookalike) in [
        ("irs refund", "irs.gov", "irs-tax-refund-help.com"),
        ("irs refund status", "irs.gov", "irs-tax-refund-help.com"),
        (
            "internal revenue service refund",
            "irs.gov",
            "irs-tax-refund-help.com",
        ),
        ("us bank login", "usbank.com", "usbank-login-help.com"),
        ("U.S. Bank login", "usbank.com", "usbank-login-help.com"),
    ] {
        let hits = searcher.search(query, 10).unwrap();
        assert_eq!(hits[0].domain, official, "{query:?}: {hits:#?}");
        // The look-alike is still found, below the site it imitates.
        assert!(domains(&hits).contains(&lookalike), "{query:?}");
    }
    // Unless it is the query spelled as one name.
    let hits = searcher.search("us bank login help", 10).unwrap();
    assert_eq!(domains(&hits), ["usbank.com"]);
    // Typing its hostname still goes to the look-alike.
    assert_eq!(
        top(&searcher, "usbank-login-help.com"),
        "usbank-login-help.com"
    );
    assert_eq!(
        top(&searcher, "amazon-prime-refund.com"),
        "amazon-prime-refund.com"
    );

    let untrusting = RankConfig {
        trusted_link_score: 0.0,
        ..RankConfig::default()
    };
    let exact = SearchOptions {
        exact: true,
        ..SearchOptions::default()
    };
    // "login" names no site, so the look-alike's domain does not win
    // "us bank login" even without the trust rule, in an exact search.
    let hits = searcher
        .search_full("us bank login", 1, &untrusting, &exact)
        .unwrap()
        .hits;
    assert_eq!(hits[0].domain, "usbank.com");
}

#[test]
fn refund_queries_need_subject_evidence_beyond_the_brand_name() {
    let (_dir, searcher) = build(&lookalike_corpus());
    let meaning = FixedMeaning(vec![("amazon.com", 0.65)]);
    for query in ["amazon refund", "amazon prime refund"] {
        let pool = searcher
            .candidate_pool(
                query,
                &experimental_rank_config(),
                &SearchOptions::default(),
                None,
            )
            .unwrap();
        let unsupported = pool
            .evidence
            .iter()
            .find(|e| e.domain == "amazon.com")
            .unwrap();
        assert_eq!(unsupported.relevance_tier, 0, "{query}: {unsupported:?}");
        let hits = searcher
            .search_meaning(
                query,
                10,
                &experimental_rank_config(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap()
            .hits;
        let owner = hits.iter().find(|hit| hit.domain == "amazon.com").unwrap();
        assert_eq!(owner.query_evidence.as_ref().unwrap().relevance_tier, 1);
        assert_eq!(
            owner
                .query_evidence
                .as_ref()
                .unwrap()
                .subject_remaining_coverage,
            Some(0.0)
        );
    }
    let mut records = lookalike_corpus();
    let owner = records
        .iter_mut()
        .find(|site| site.domain == "amazon.com")
        .unwrap();
    owner.description = Some("Amazon Prime refunds and refund status".into());
    let (_dir, searcher) = build(&records);
    for query in ["amazon refund", "amazon prime refund"] {
        let hits = searcher
            .search_meaning(
                query,
                10,
                &experimental_rank_config(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap()
            .hits;
        assert_eq!(hits[0].domain, "amazon.com", "{query}: {hits:?}");
        assert_eq!(hits[0].query_evidence.as_ref().unwrap().relevance_tier, 1);
    }
}

#[test]
fn little_known_sites_win_when_no_named_site_competes() {
    let (_dir, searcher) = build(&lookalike_corpus());
    // No rank either way: github.com is named by "github", and the
    // look-alikes stuffing "login" have no more evidence than it has.
    assert_eq!(top(&searcher, "github login"), "github.com");
    // No site is named "maple" or "maple street", so the popular bakery
    // does not discount the unranked one.
    let hits = searcher
        .search_with("maple street bakery", 10, &keep_all())
        .unwrap();
    assert_eq!(
        domains(&hits)[..2],
        ["maplestreetbakery.com", "panerabread.com"]
    );
    assert_eq!(hits[0].link_score, 0.0);
}

#[test]
fn intent_words_rank_the_site_they_follow() {
    let records = vec![
        site(
            "paypal.com",
            Some("PayPal: Pay, Send and Save Money"),
            None,
            &["PayPal"],
            &[("PayPal", 3)],
            popular(20, 3),
        ),
        // Some links of its own, so the trust rule alone lets it win.
        site(
            "paypal-login.us",
            Some("PayPal Login | Sign in to your PayPal account"),
            Some("PayPal login: sign in to PayPal."),
            &["PayPal Login"],
            &[("PayPal login", 2)],
            obscure(1_000_000, 40),
        ),
        site(
            "postgresql.org",
            Some("PostgreSQL: The world's most advanced open source database"),
            None,
            &["PostgreSQL"],
            &[("Postgres", 2)],
            popular(9_000, 3),
        ),
        // Named "postgres", but not what "postgres docs" is after.
        site(
            "postgres.ai",
            Some("Postgres.AI"),
            None,
            &[],
            &[],
            obscure(5_000_000, 2),
        ),
        site(
            "github.com",
            Some("GitHub: Let's build from here"),
            None,
            &["GitHub"],
            &[("docs", 3), ("GitHub", 3)],
            popular(30, 4),
        ),
        site(
            "readthedocs.org",
            Some("Read the Docs"),
            None,
            &["Read the Docs"],
            &[("Read the Docs", 2)],
            popular(3_000, 3),
        ),
    ];
    let (_dir, searcher) = build(&records);
    assert_eq!(top(&searcher, "paypal login"), "paypal.com");
    assert_eq!(top(&searcher, "PayPal sign in"), "paypal.com");
    // "postgres" is searched as typed, with a suggestion that finds
    // postgresql.org above postgres.ai.
    let results = searcher
        .search_full(
            "postgres docs",
            10,
            &RankConfig::default(),
            &SearchOptions::default(),
        )
        .unwrap();
    let fixed = results.spelling.expect("a suggestion").query;
    let hits = searcher.search(&fixed, 10).unwrap();
    let rank = |domain: &str| domains(&hits).iter().position(|&d| d == domain);
    let official = rank("postgresql.org").expect("postgresql.org");
    assert!(
        rank("postgres.ai").is_none_or(|other| official < other),
        "{fixed}: {hits:#?}"
    );
    // A well-known site named by all of it keeps the query.
    assert_eq!(top(&searcher, "read the docs"), "readthedocs.org");
    // Typing its hostname still goes to the look-alike.
    assert_eq!(top(&searcher, "paypal-login.us"), "paypal-login.us");
}

#[test]
fn intent_words_are_cut_from_the_end() {
    for (query, name) in [
        ("paypal login", Some("paypal")),
        ("Bank of America sign in", Some("bank of america")),
        ("us bank login help", Some("us bank")),
        ("netflix help center", Some("netflix")),
        ("mdn web docs", Some("mdn")),
        ("login", None),
        ("help center", None),
        ("login help desk", None),
        ("chase center tickets", None),
        ("aster api", None),
        ("aster retirement benefits", None),
        ("aster baggage policy", None),
        ("aster student login", Some("aster student")),
        ("aster order tracking", Some("aster order")),
    ] {
        assert_eq!(without_intent_words(query).as_deref(), name, "{query:?}");
    }
}

#[test]
fn docs_searches_put_the_docs_site_first() {
    let hit = |domain: &str| Hit {
        domain: domain.into(),
        url: format!("https://{domain}/"),
        title: None,
        description: None,
        score: 1.0,
        text_score: 1.0,
        link_score: 1.0,
        country: None,
        named: false,
        official: false,
        key_pages: Vec::new(),
        demand: None,
        missing_words: false,
        query_evidence: None,
        placing_text_score: None,
    };
    let order = |query: &str, name: &str| {
        let mut hits = vec![hit("github.com"), hit("postgresql.org"), hit("mozilla.org")];
        docs_site_first(query, name, &mut hits);
        hits.into_iter().map(|h| h.domain).collect::<Vec<_>>()
    };
    assert_eq!(
        order("postgres docs", "postgres"),
        ["postgresql.org", "github.com", "mozilla.org"]
    );
    assert_eq!(
        order("mdn web docs", "mdn"),
        ["mozilla.org", "github.com", "postgresql.org"]
    );
    // Not asking for docs, or not naming a docs site: as it was.
    assert_eq!(
        order("postgres login", "postgres"),
        ["github.com", "postgresql.org", "mozilla.org"]
    );
    assert_eq!(
        order("acme docs", "acme"),
        ["github.com", "postgresql.org", "mozilla.org"]
    );
}

#[test]
fn small_joining_words_name_no_site_in_a_longer_query() {
    let records = vec![
        site(
            "in.gov",
            Some("IN.gov | The Official Website of the State of Indiana"),
            None,
            &["Indiana"],
            &[("IN.gov", 3)],
            popular(300, 5_000),
        ),
        site(
            "denverpizzaco.com",
            Some("Denver Pizza Company | Pizza in Denver"),
            Some("Wood-fired pizza in Denver, Colorado."),
            &[],
            &[("Denver Pizza", 1)],
            obscure(10_000_000, 0),
        ),
    ];
    let (_dir, searcher) = build(&records);
    assert_eq!(top(&searcher, "pizza in denver"), "denverpizzaco.com");
    // On its own, the word is still a name.
    assert_eq!(top(&searcher, "in"), "in.gov");
}

#[test]
fn comparison_words_match_nothing_between_the_things_compared() {
    let records = vec![
        site(
            "apple.com",
            Some("Apple"),
            Some("Compare iPhone 17 Pro vs iPhone Air and shop the latest models."),
            &["Apple"],
            &[("Apple", 50)],
            popular(1, 50),
        ),
        site(
            "biologycorner.com",
            Some("The Biology Corner"),
            Some("Lessons on mitosis and meiosis for biology teachers."),
            &[],
            &[],
            obscure(500_000, 2),
        ),
        site(
            "vscode.dev",
            Some("Visual Studio Code for the Web"),
            None,
            &["VS Code"],
            &[("VS Code", 40)],
            popular(900, 40),
        ),
    ];
    let (_dir, searcher) = build(&records);
    for query in [
        "mitosis vs meiosis",
        "mitosis vs. meiosis",
        "mitosis versus meiosis",
    ] {
        let hits = searcher.search(query, 10).unwrap();
        assert_eq!(domains(&hits), ["biologycorner.com"], "{query}: {hits:#?}");
    }
    // First, "vs" is still a word of a name.
    assert_eq!(top(&searcher, "vs code"), "vscode.dev");
}

#[test]
fn longer_leading_names_win() {
    let (_dir, searcher) = build(&lookalike_corpus());
    // chasecenter.com covers "chase center", chase.com only "chase".
    let hits = searcher.search("chase center tickets", 10).unwrap();
    assert_eq!(
        domains(&hits)[..2],
        ["chasecenter.com", "chase.com"],
        "{hits:#?}"
    );
    assert_eq!(top(&searcher, "chase login"), "chase.com");
}

#[test]
fn named_sites_are_ranked_outside_the_bm25_candidates() {
    let (_dir, searcher) = build(&lookalike_corpus());
    let cfg = RankConfig::default();
    let full = searcher.search_with("us bank login", 10, &cfg).unwrap();
    // With one BM25 candidate, the stuffed look-alike, usbank.com still
    // gets scored because the query names it.
    let narrow = RankConfig {
        candidates: 1,
        ..RankConfig::default()
    };
    let hits = searcher.search_with("us bank login", 1, &narrow).unwrap();
    assert_eq!(domains(&hits), ["usbank.com"]);
    assert_eq!(full[0].domain, "usbank.com");
    // Scored one by one rather than in a batch, so equal up to rounding.
    assert!((hits[0].text_score - full[0].text_score).abs() < 1e-5);
    assert!((hits[0].score - full[0].score).abs() < 1e-5);
}

#[test]
fn alpha_trades_text_for_popularity() {
    let (_dir, searcher) = build(&corpus());
    // Text only, no bonus: the page that best matches "bank" wins.
    let text_only = RankConfig {
        alpha: 0.0,
        exact_label_bonus: 0.0,
        exact_alias_bonus: 0.0,
        described_alpha: None,
        ..RankConfig::default()
    };
    let hits = searcher.search_with("bank", 20, &text_only).unwrap();
    assert_eq!(hits[0].text_score, 1.0);
    assert_eq!(hits[0].score, 1.0);
    // Popularity only: the most popular site matching "bank" wins.
    let prior_only = RankConfig {
        alpha: 1.0,
        ..text_only
    };
    let hits = searcher.search_with("bank", 20, &prior_only).unwrap();
    let best_link = hits.iter().map(|hit| hit.link_score).fold(0.0, f32::max);
    assert_eq!(hits[0].link_score, best_link);
    assert_eq!(hits[0].domain, "chase.com");
}

#[test]
fn described_alpha_only_applies_when_no_site_is_named_in_full() {
    let (_dir, searcher) = build(&corpus());
    let cfg = RankConfig {
        alpha: 0.0,
        exact_label_bonus: 0.0,
        exact_alias_bonus: 0.0,
        link_name_bonus: 0.0,
        described_alpha: Some(1.0),
        ..RankConfig::default()
    };
    // No site is named "bank": popularity alone decides.
    let hits = searcher.search_with("bank", 20, &cfg).unwrap();
    assert_eq!(hits[0].domain, "chase.com");
    assert_eq!(hits[0].score, hits[0].link_score);
    // usbank.com is named by all of "us bank": alpha stays 0.
    let hits = searcher.search_with("us bank", 20, &cfg).unwrap();
    assert_eq!(hits[0].domain, "usbank.com");
    assert_eq!(hits[0].score, hits[0].text_score);
}

#[test]
fn described_queries_need_some_match_for_popularity_to_count() {
    let records = vec![
        site(
            "youtube.com",
            Some("YouTube"),
            Some("Enjoy the videos and music you love, and keep a watch list."),
            &["YouTube"],
            &[("YouTube", 50)],
            popular(1, 50),
        ),
        site(
            "checklist.com",
            Some("Checklist.com | To do list and checklists"),
            Some("Make a to do list and share checklists."),
            &[],
            &[("checklist", 1)],
            obscure(500_000, 2),
        ),
    ];
    let (_dir, searcher) = build(&records);
    // Popularity weighing heavily, as among a million sites, where
    // youtube.com matches "to do list" far less than here.
    let on = RankConfig {
        described_alpha: Some(0.9),
        described_relevance: Some(0.25),
        ..RankConfig::default()
    };
    let hits = searcher.search_with("to do list", 2, &on).unwrap();
    assert_eq!(hits[0].domain, "checklist.com");
    let off = RankConfig {
        described_relevance: None,
        ..on
    };
    let hits = searcher.search_with("to do list", 2, &off).unwrap();
    assert_eq!(hits[0].domain, "youtube.com");
}

#[test]
fn partial_label_bonus_only_applies_to_part_of_the_query() {
    let (_dir, searcher) = build(&corpus());
    let no_partial = RankConfig {
        partial_label_bonus: None,
        ..RankConfig::default()
    };
    let full = searcher
        .search_with("us bank", 1, &RankConfig::default())
        .unwrap();
    let hits = searcher.search_with("us bank", 1, &no_partial).unwrap();
    assert_eq!(hits[0].domain, "usbank.com");
    assert!((hits[0].score - full[0].score).abs() < 1e-5);
}

#[test]
fn limits_are_respected() {
    let (_dir, searcher) = build(&corpus());
    assert_eq!(searcher.search("bank", 2).unwrap().len(), 2);
    assert!(searcher.search("bank", 0).unwrap().is_empty());
    let all = searcher.search("bank", 1000).unwrap();
    assert!(all.len() <= searcher.num_docs() as usize);
    let few_candidates = RankConfig {
        candidates: 0,
        ..keep_all()
    };
    let hits = searcher.search_with("us bank", 3, &few_candidates).unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].domain, "usbank.com");
}

#[test]
fn hits_carry_page_fields_and_url_fallback() {
    let (_dir, searcher) = build(&corpus());
    let hit = &searcher.search("us bank", 1).unwrap()[0];
    assert_eq!(hit.url, "https://www.usbank.com/");
    assert_eq!(
        hit.title.as_deref(),
        Some("U.S. Bank | Personal Banking, Credit Cards, Home Loans & More")
    );
    assert!(hit
        .description
        .as_deref()
        .unwrap()
        .starts_with("Discover U.S. Bank"));

    let hits = searcher.search("acme", 10).unwrap();
    let acme_net = hits.iter().find(|hit| hit.domain == "acme.net").unwrap();
    assert_eq!(acme_net.url, "https://acme.net/");
    assert_eq!(acme_net.description, None);
}

#[test]
fn popular_sites_a_little_further_in_meaning_are_ranked() {
    let mut records = vec![site(
        "tesla.com",
        Some("Tesla"),
        None,
        &[],
        &[],
        popular(500, 20_000),
    )];
    let mut near = Vec::new();
    for i in 0..60 {
        let domain: &'static str = Box::leak(format!("ev{i}.example").into_boxed_str());
        records.push(site(
            domain,
            Some("Electric car maker"),
            None,
            &[],
            &[],
            obscure(5_000_000 + i, 2),
        ));
        near.push((domain, 1.0 - i as f32 / 1_000.0));
    }
    // Small sites that repeat the query's words are the 60 nearest;
    // the big site it describes comes after them, and is ranked.
    near.push(("tesla.com", 0.85));
    let (_dir, searcher) = build(&records);
    let hits = searcher
        .search_meaning(
            "electric car maker",
            100,
            &RankConfig::default(),
            &SearchOptions::default(),
            Some(&FixedMeaning(near)),
        )
        .unwrap()
        .hits;
    assert!(domains(&hits).contains(&"tesla.com"), "{hits:?}");
}
