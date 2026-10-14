//! Searching with meaning: what it adds to a word search and what it leaves
//! alone.

use super::*;

#[test]
fn whole_query_subjects_precede_domain_words_with_and_without_meaning() {
    let cases = [
        (
            "asyncio",
            "async.com",
            "Async telecommunications",
            "Python asyncio asynchronous I/O reference",
        ),
        (
            "tomllib",
            "tomayko.com",
            "Ryan Tomayko personal website",
            "Python tomllib TOML parser reference",
        ),
        (
            "K2 height",
            "k2.com",
            "K2 business workflow automation",
            "K2 height mountain geography guide",
        ),
        (
            "Amazon River length",
            "amazon.com",
            "Amazon online shopping and retail",
            "Amazon River length and geography",
        ),
        (
            "Logan Square neighborhood",
            "square.com",
            "Square payment processing",
            "Logan Square neighborhood in Chicago",
        ),
        (
            "budget meal planning",
            "budget.com",
            "Budget car rental",
            "Budget meal planning and recipes",
        ),
        (
            "deadline management tips",
            "deadline.com",
            "Deadline Hollywood entertainment news",
            "Deadline management tips for projects",
        ),
        (
            "climate change effects",
            "change.org",
            "Change petitions and campaigns",
            "Climate change effects and science",
        ),
    ];
    for (query, wrong, wrong_title, subject) in cases {
        let mut records = vec![site(
            wrong,
            Some(wrong_title),
            None,
            &[],
            &[],
            popular(10, 50_000),
        )];
        // Three independently retrieved subject rows ensure partial
        // names cannot occupy the requested top three, even when the
        // misleading name has much greater popularity and BM25.
        for domain in [
            "subject-guide.example",
            "subject-reference.example",
            "subject-library.example",
        ] {
            records.push(site(
                domain,
                Some(subject),
                Some(subject),
                &[],
                &[],
                obscure(700_000, 5),
            ));
        }
        let (_dir, searcher) = build(&records);
        let meaning = FixedMeaning(vec![(wrong, 0.1), ("subject-guide.example", 0.9)]);
        let options = SearchOptions {
            exact: true,
            ..Default::default()
        };
        for vector in [None, Some(&meaning as &dyn Meaning)] {
            let hits = searcher
                .search_meaning(query, 10, &experimental_rank_config(), &options, vector)
                .unwrap()
                .hits;
            assert_eq!(hits.len().min(3), 3, "{query}: {hits:?}");
            assert!(
                !domains(&hits).iter().take(3).any(|domain| *domain == wrong),
                "{query}: {hits:?}"
            );
            assert!(
                hits.windows(2).all(|pair| pair[0].score >= pair[1].score),
                "{query}: scores must preserve ranking"
            );
            let mut learned_hits = hits.clone();
            learned::reorder(
                learned::Model::builtin(),
                query,
                &mut learned_hits,
                &mut Vec::new(),
            );
            assert!(
                !domains(&learned_hits)
                    .iter()
                    .take(3)
                    .any(|domain| *domain == wrong),
                "{query}: learned {learned_hits:?}"
            );
            for hit in &learned_hits {
                assert_eq!(
                    hit.query_evidence,
                    hits.iter()
                        .find(|before| before.domain == hit.domain)
                        .unwrap()
                        .query_evidence
                );
                let mut legacy = serde_json::to_value(hit).unwrap();
                legacy.as_object_mut().unwrap().remove("query_evidence");
                let legacy: Hit = serde_json::from_value(legacy).unwrap();
                assert!(legacy.query_evidence.is_none());
            }
            let pool = searcher
                .candidate_pool(query, &experimental_rank_config(), &options, vector)
                .unwrap();
            let correct = pool
                .evidence
                .iter()
                .find(|e| e.domain == "subject-reference.example")
                .unwrap();
            assert_eq!(
                correct.semantic_closeness, None,
                "a missing vector is unknown"
            );
            assert!(correct.substantive_coverage > 0.99, "{query}: {correct:?}");
            assert_eq!(correct.relevance_tier, 1);
            if let Some(wrong) = pool.evidence.iter().find(|e| e.domain == wrong) {
                assert_eq!(wrong.relevance_tier, 0, "{query}: {wrong:?}");
                assert!(wrong.substantive_coverage < correct.substantive_coverage);
            }
        }
    }
}

#[test]
fn partial_brands_keep_their_matching_subjects_and_explicit_navigation() {
    let mut records = vec![
        site(
            "budget.com",
            Some("Budget car rental"),
            Some("Car rental reservations"),
            &["Budget"],
            &[],
            popular(500, 8_000),
        ),
        site(
            "deadline.com",
            Some("Deadline Hollywood entertainment news"),
            None,
            &["Deadline"],
            &[],
            popular(500, 8_000),
        ),
        site(
            "change.org",
            Some("Change petitions and campaigns"),
            None,
            &["Change"],
            &[],
            popular(500, 8_000),
        ),
        site(
            "chase.com",
            Some("Chase bank"),
            None,
            &["Chase"],
            &[],
            popular(100, 50_000),
        ),
        site(
            "chase-login.example",
            Some("Chase login account"),
            Some("Chase login account"),
            &[],
            &[],
            Signals::default(),
        ),
    ];
    records[3].about = Some("American bank".to_string());
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![
        ("budget.com", 0.1),
        ("deadline.com", 0.1),
        ("change.org", 0.1),
        ("chase-login.example", 0.9),
    ]);
    for vector in [None, Some(&meaning as &dyn Meaning)] {
        for (query, expected) in [
            ("Budget car rental", "budget.com"),
            ("Deadline Hollywood", "deadline.com"),
            ("Change.org", "change.org"),
            ("Chase login", "chase.com"),
        ] {
            let results = searcher
                .search_meaning(
                    query,
                    10,
                    &experimental_rank_config(),
                    &SearchOptions::default(),
                    vector,
                )
                .unwrap();
            assert_eq!(results.hits[0].domain, expected, "{query}: {results:?}");
        }
    }
}

#[test]
fn nearest_membership_does_not_save_a_weak_partial_match() {
    let records = vec![
        site(
            "hbo.com",
            Some("HBO series and movies"),
            None,
            &[],
            &[],
            popular(10, 50_000),
        ),
        site(
            "physics.example",
            Some("Atomic spectral lines"),
            Some("Electromagnetic transitions in hydrogen atoms"),
            &[],
            &[],
            obscure(700_000, 5),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("hbo.com", 0.05), ("physics.example", 0.85)]);
    let results = searcher
        .search_meaning(
            "paschen series",
            10,
            &RankConfig::default(),
            &SearchOptions {
                exact: true,
                ..Default::default()
            },
            Some(&meaning),
        )
        .unwrap();
    assert_eq!(domains(&results.hits), ["physics.example"]);
}

#[test]
fn drug_sales_patterns_are_excluded_but_small_guidance_and_navigation_survive() {
    let records = vec![
        site(
            "quick-meds.info",
            Some("Buy Viagra online without a prescription"),
            Some("Viagra dosage and side effects"),
            &[],
            &[],
            popular(100, 20_000),
        ),
        site(
            "small-clinic.pics",
            Some("Viagra dosage and side effects"),
            Some("Clinical guidance about Viagra dosage and side effects"),
            &[],
            &[],
            obscure(900_000, 0),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("quick-meds.info", 0.99), ("small-clinic.pics", 0.7)]);
    for vector in [None, Some(&meaning as &dyn Meaning)] {
        let results = searcher
            .search_meaning(
                "Viagra dosage side effects",
                10,
                &RankConfig::default(),
                &SearchOptions {
                    exact: true,
                    ..Default::default()
                },
                vector,
            )
            .unwrap();
        assert_eq!(domains(&results.hits), ["small-clinic.pics"]);
        let explicit = searcher
            .search_meaning(
                "quick-meds.info",
                1,
                &RankConfig::default(),
                &SearchOptions::default(),
                vector,
            )
            .unwrap();
        assert_eq!(domains(&explicit.hits), ["quick-meds.info"]);
        let pool = searcher
            .candidate_pool(
                "Viagra dosage side effects",
                &RankConfig::default(),
                &SearchOptions::default(),
                vector,
            )
            .unwrap();
        let offer = pool
            .evidence
            .iter()
            .find(|e| e.domain == "quick-meds.info")
            .unwrap();
        assert_eq!(
            offer.source_quality.as_ref().unwrap().reason,
            health::SourceQualityReason::UnprescribedDrugOffer
        );
    }
}

#[test]
fn big_sites_far_in_meaning_do_not_fill_a_weak_search() {
    let records = vec![
        site(
            "stripe.com",
            Some("Stripe | Financial Infrastructure"),
            Some("Online payments for businesses."),
            &["Stripe"],
            &[],
            popular(50, 20_000),
        ),
        site(
            "drainhelp.net",
            Some("Drain help"),
            Some("How to unclog a drain with a plunger or a snake."),
            &[],
            &[],
            ranked(400_000, 5),
        ),
        site(
            "plumbers.org",
            Some("Find a plumber"),
            None,
            &[],
            &[],
            ranked(200_000, 20),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let options = SearchOptions::default();
    // Among the thousand nearest, as every big site is for some query.
    let meaning = near_and_far(
        &[("drainhelp.net", 1.0)],
        &[("plumbers.org", 0.3), ("stripe.com", 0.08)],
    );
    let score = |cfg: &RankConfig, domain: &str| {
        searcher
            .search_meaning("how to unclog a drain", 10, cfg, &options, Some(&meaning))
            .unwrap()
            .hits
            .into_iter()
            .find(|hit| hit.domain == domain)
            .map_or(0.0, |hit| hit.score)
    };
    let before = RankConfig {
        meaning_only_relevance: None,
        question_relevance: None,
        partial_closeness: None,
        whole_query_relevance: false,
        ..RankConfig::default()
    };
    // On popularity alone it would have come before the plumbers...
    assert!(score(&before, "stripe.com") > score(&before, "plumbers.org"));
    // ...but nothing of the query's speaks for it.
    let now = RankConfig {
        partial_closeness: None,
        ..RankConfig::default()
    };
    assert!(score(&now, "stripe.com") < score(&now, "plumbers.org"));
    assert!(score(&now, "stripe.com") < score(&now, "drainhelp.net"));
    // And with neither word nor much meaning, it is not listed at all.
    let hits = searcher
        .search_meaning(
            "how to unclog a drain",
            10,
            &RankConfig::default(),
            &options,
            Some(&meaning),
        )
        .unwrap()
        .hits;
    assert_eq!(domains(&hits), ["drainhelp.net"]);
}

#[test]
fn a_word_or_two_of_a_longer_search_is_no_answer() {
    let records = vec![
        site(
            "victoriassecret.com",
            Some("Victoria's Secret: Bras, Lingerie, Beauty"),
            None,
            &["Victoria's Secret"],
            &[],
            popular(300, 20_000),
        ),
        site(
            "rpgguides.net",
            Some("RPG guides"),
            Some("Walkthroughs of classic role-playing games."),
            &[],
            &[],
            ranked(300_000, 10),
        ),
        site(
            "manaworld.org",
            Some("Mana World"),
            Some("Secret of Mana walkthrough, maps and bosses."),
            &[],
            &[],
            ranked(500_000, 5),
        ),
        site(
            "cricket.com.au",
            Some("Home | cricket.com.au"),
            None,
            &["Cricket Australia"],
            &[],
            popular(2_000, 8_000),
        ),
        site(
            "fables.org",
            Some("Aesop's fables"),
            Some("The ant and the grasshopper, and other fables."),
            &[],
            &[],
            ranked(400_000, 10),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let options = SearchOptions::default();
    let found = |query: &str, meaning: &FixedMeaning, cfg: &RankConfig| {
        let hits = searcher
            .search_meaning(query, 10, cfg, &options, Some(meaning))
            .unwrap()
            .hits;
        domains(&hits)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let off = RankConfig {
        partial_closeness: None,
        ..RankConfig::default()
    };
    let now = RankConfig::default();

    // "secret" alone makes no answer of a lingerie shop; a site near in
    // meaning that lacks a word is still one.
    let mana = near_and_far(
        &[("manaworld.org", 1.0), ("rpgguides.net", 0.8)],
        &[("victoriassecret.com", 0.1)],
    );
    let query = "secret of mana walkthrough";
    assert!(found(query, &mana, &off).contains(&"victoriassecret.com".to_string()));
    assert_eq!(
        found(query, &mana, &now),
        ["manaworld.org", "rpgguides.net"]
    );

    // A name the words go on from as one phrase names nothing.
    let fable = near_and_far(&[("fables.org", 1.0)], &[("cricket.com.au", 0.2)]);
    let query = "the cricket and the ant";
    assert!(found(query, &fable, &off).contains(&"cricket.com.au".to_string()));
    assert_eq!(found(query, &fable, &now), ["fables.org"]);

    // A name followed by what is wanted of it keeps the site.
    let scores = near_and_far(&[], &[("cricket.com.au", 0.2)]);
    assert_eq!(found("cricket scores", &scores, &now)[0], "cricket.com.au");
    // So does a name further in ("back bay hilton").
    assert!(found("grasshopper cricket", &scores, &now).contains(&"cricket.com.au".to_string()));
    // Not one nowhere near the query in meaning with no other word of it:
    // that is a namesake of the word (x.com for "derivative of sin x").
    let nowhere = near_and_far(&[], &[("cricket.com.au", 0.0)]);
    let cricket = "cricket.com.au".to_string();
    assert!(!found("grasshopper cricket", &nowhere, &now).contains(&cricket));
    assert!(found("grasshopper cricket", &nowhere, &off).contains(&cricket));
    // Another word of the query, or the query starting with the name,
    // keeps it.
    assert!(found("australia grasshopper cricket", &nowhere, &now).contains(&cricket));
    assert_eq!(found("cricket scores", &nowhere, &now)[0], "cricket.com.au");
    assert!(found("www.cricket", &nowhere, &now).contains(&cricket));
}

#[test]
fn a_name_some_little_site_has_does_not_keep_namesakes_of_its_words() {
    let records = vec![
        site(
            "solarsystem.io",
            Some("Solar System"),
            None,
            &[],
            &[],
            obscure(900_000, 3),
        ),
        site(
            "systemofadown.com",
            Some("System of a Down"),
            None,
            &["System of a Down"],
            &[],
            popular(3_000, 9_000),
        ),
        site(
            "nasa.gov",
            Some("NASA"),
            Some("Exploring the solar system and beyond."),
            &["NASA"],
            &[],
            popular(200, 50_000),
        ),
        site(
            "hbo.com",
            Some("HBO: series and movies"),
            None,
            &["HBO"],
            &[],
            popular(400, 30_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let options = SearchOptions::default();
    let meaning = near_and_far(
        &[("nasa.gov", 1.0)],
        &[("systemofadown.com", 0.05), ("hbo.com", 0.05)],
    );
    let found = |query: &str| {
        let hits = searcher
            .search_meaning(query, 10, &RankConfig::default(), &options, Some(&meaning))
            .unwrap()
            .hits;
        domains(&hits)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    // Named in full by a little site, the query still only wants what
    // has all of it or is near it in meaning.
    let solar = found("solar system");
    assert!(solar.contains(&"nasa.gov".to_string()), "{solar:?}");
    assert!(
        !solar.contains(&"systemofadown.com".to_string()),
        "{solar:?}"
    );
    // A word no site has does not leave the other word to stand alone.
    let paschen = found("paschen series");
    assert!(!paschen.contains(&"hbo.com".to_string()), "{paschen:?}");
}

#[test]
fn an_official_site_is_named_inside_by_its_wikidata_name_only() {
    let bank = Signals {
        official_site: true,
        ..obscure(300_000, 40)
    };
    let mut valley = site(
        "valley.com",
        Some("Valley Bank"),
        None,
        &["Valley National Bank"],
        &[],
        bank,
    );
    valley.about = Some("American bank".to_string());
    let records = vec![
        valley,
        site(
            "elephantnaturepark.org",
            Some("Elephant Nature Park"),
            Some("An elephant sanctuary in a valley near Chiang Mai."),
            &[],
            &[],
            obscure(400_000, 20),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let options = SearchOptions::default();
    let meaning = near_and_far(&[("elephantnaturepark.org", 1.0)], &[("valley.com", 0.05)]);
    let found = |query: &str| {
        let hits = searcher
            .search_meaning(query, 10, &RankConfig::default(), &options, Some(&meaning))
            .unwrap()
            .hits;
        domains(&hits)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let elephant = found("elephant valley");
    assert!(
        !elephant.contains(&"valley.com".to_string()),
        "{elephant:?}"
    );
    let routing = found("routing number valley national bank");
    assert!(routing.contains(&"valley.com".to_string()), "{routing:?}");
}

#[test]
fn a_question_names_a_site_by_what_links_call_it() {
    // Official, though not well known.
    let mut wikihow = site(
        "wikihow.com",
        Some("wikiHow"),
        None,
        &["wikiHow"],
        &[("wikiHow", 900)],
        obscure(300_000, 40),
    );
    wikihow.about = Some("how-to website".to_string());
    let records = vec![
        wikihow,
        site(
            "netflix.com",
            Some("Netflix"),
            None,
            &["Netflix"],
            &[("Netflix", 5_000)],
            popular(30, 200_000),
        ),
        site("rid.it", Some("RID"), None, &[], &[], popular(5_000, 900)),
        site(
            "tie.org",
            Some("TiE Global"),
            None,
            &["TiE"],
            &[("TiE", 2_000)],
            popular(9_000, 5_000),
        ),
        site(
            "helpguides.net",
            Some("Help guides"),
            Some("How to cancel a subscription, tie a tie or get rid of aphids."),
            &[],
            &[],
            ranked(300_000, 10),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let options = SearchOptions::default();
    let meaning = near_and_far(
        &[("helpguides.net", 1.0)],
        &[
            ("netflix.com", 0.2),
            ("wikihow.com", 0.2),
            ("rid.it", 0.2),
            ("tie.org", 0.0),
        ],
    );
    let found = |query: &str| {
        let hits = searcher
            .search_meaning(query, 10, &RankConfig::default(), &options, Some(&meaning))
            .unwrap()
            .hits;
        domains(&hits)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let netflix = found("how to cancel netflix");
    assert!(netflix.contains(&"netflix.com".to_string()), "{netflix:?}");
    let tie = found("how to tie a tie wikihow");
    assert!(tie.contains(&"wikihow.com".to_string()), "{tie:?}");
    // A name nowhere near the question in meaning is a namesake of its
    // word, as in any other search.
    assert!(!tie.contains(&"tie.org".to_string()), "{tie:?}");
    // Only its domain calls rid.it "rid": the question names nothing.
    let aphids = found("how to get rid of aphids");
    assert!(!aphids.contains(&"rid.it".to_string()), "{aphids:?}");
}

#[test]
fn a_misspelled_name_keeps_the_site_meaning_finds_for_it() {
    let records = vec![
        site(
            "adobe.com",
            Some("Adobe: Creative, marketing and document software"),
            None,
            &["Adobe"],
            &[("adobe", 300)],
            popular(60, 15_000),
        ),
        site(
            "visualpde.com",
            Some("VisualPDE"),
            Some("Interactive equations in the browser."),
            &[],
            &[],
            ranked(300_000, 5),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("visualpde.com", 0.3), ("adobe.com", 0.06)]);
    let found = searcher
        .search_meaning(
            "adobee",
            10,
            &RankConfig::default(),
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap();
    let spelling = found.spelling.expect("a suggestion");
    assert_eq!(spelling.site.as_deref(), Some("adobe.com"));
    // The results are still those of "adobee", but what meaning finds
    // for it is not cut as filler.
    assert_eq!(
        domains(&found.hits)[0],
        "adobe.com",
        "{:?}",
        domains(&found.hits)
    );
}

#[test]
fn the_candidate_pool_says_where_each_site_came_from() {
    let records = vec![
        site(
            "tesla.com",
            None,
            None,
            &["Tesla"],
            &[],
            popular(500, 20_000),
        ),
        site("electric.com", None, None, &[], &[], ranked(40_000, 200)),
        site("carmaker.net", None, None, &[], &[], ranked(90_000, 50)),
    ];
    let (_dir, searcher) = build(&records);
    let cfg = RankConfig::default();
    let options = SearchOptions::default();
    let meaning = FixedMeaning(vec![("tesla.com", 0.9)]);
    let pool = searcher
        .candidate_pool("electric car maker", &cfg, &options, Some(&meaning))
        .unwrap();
    assert_eq!(pool.words, ["electric.com"], "{pool:?}");
    assert_eq!(pool.meaning, ["tesla.com"], "{pool:?}");
    assert!(pool.has("tesla.com") && !pool.has("carmaker.net"));
    let pool = searcher
        .candidate_pool("tesla", &cfg, &options, Some(&meaning))
        .unwrap();
    assert_eq!(pool.named, ["tesla.com"], "{pool:?}");
    assert!(pool.meaning.is_empty(), "a name is not searched by meaning");

    let found = searcher
        .words_order(
            "electric car",
            10,
            &cfg,
            &["electric.com".into(), "tesla.com".into()],
        )
        .unwrap()
        .unwrap();
    assert_eq!(found.order, ["electric.com"]);
    assert!(found.matched.contains("electric.com") && !found.matched.contains("tesla.com"));
}

#[test]
fn meaning_finds_described_sites_but_leaves_names_alone() {
    let records = vec![
        site(
            "tesla.com",
            None,
            None,
            &["Tesla"],
            &[],
            popular(500, 20_000),
        ),
        site(
            "rivian.com",
            None,
            None,
            &["Rivian"],
            &[],
            popular(9_000, 3_000),
        ),
        site("electric.com", None, None, &[], &[], ranked(40_000, 200)),
        site("carmaker.net", None, None, &[], &[], ranked(90_000, 50)),
    ];
    let (_dir, searcher) = build(&records);
    let cfg = RankConfig::default();
    let options = SearchOptions::default();
    let meaning = FixedMeaning(vec![
        ("tesla.com", 0.9),
        ("rivian.com", 0.85),
        ("electric.com", 0.2),
    ]);
    let search = |query: &str, meaning: Option<&dyn Meaning>| {
        let results = searcher
            .search_meaning(query, 10, &cfg, &options, meaning)
            .unwrap();
        results
            .hits
            .into_iter()
            .map(|hit| hit.domain)
            .collect::<Vec<_>>()
    };
    // Without meaning, the words win; tesla.com and rivian.com share
    // none with the query.
    let by_words = search("electric car maker", None);
    assert_eq!(by_words.first().map(String::as_str), Some("electric.com"));
    assert!(!by_words.contains(&"tesla.com".to_string()), "{by_words:?}");
    // With it, the sites the query describes come first.
    let by_meaning = search("electric car maker", Some(&meaning));
    assert_eq!(
        &by_meaning[..2],
        ["tesla.com", "rivian.com"],
        "{by_meaning:?}"
    );
    // A site with no embedding that one query word matches does not
    // beat the sites the query describes (as on plumbsearch.org, where
    // electric.net and friends had no vector)...
    let unembedded = FixedMeaning(vec![("tesla.com", 1.0), ("rivian.com", 0.9)]);
    assert_eq!(
        &search("electric car maker", Some(&unembedded))[..2],
        ["tesla.com", "rivian.com"]
    );
    // A nearest result below the release descriptive floor is not
    // enough, even when it shares no words. The weak pure-semantic
    // fallback remains available only to the opt-in experiment.
    let partial = FixedMeaning(vec![("tesla.com", 0.3)]);
    assert!(search("electric car maker", Some(&partial)).is_empty());
    let experimental = searcher
        .search_meaning(
            "electric car maker",
            10,
            &experimental_rank_config(),
            &options,
            Some(&partial),
        )
        .unwrap();
    assert_eq!(domains(&experimental.hits), ["tesla.com"]);
    // A query naming a site is ranked as before.
    let named = FixedMeaning(vec![("tesla.com", 1.0)]);
    assert_eq!(search("rivian", Some(&named))[0], "rivian.com");
    assert_eq!(search("rivian", Some(&named)).len(), 1);
}

#[test]
fn sites_with_no_embedding_matching_the_whole_query_keep_their_words() {
    let records = vec![
        site(
            "schwab.com",
            Some("Charles Schwab"),
            None,
            &[],
            &[],
            ranked(1_500, 3_000),
        ),
        site(
            "wsj.com",
            None,
            None,
            &["The Wall Street Journal"],
            &[],
            popular(300, 40_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    // The query names no site in full, so meaning counts, and wsj.com
    // is the nearest site with a vector; schwab.com has none.
    let meaning = FixedMeaning(vec![("wsj.com", 1.0)]);
    let hits = searcher
        .search_meaning(
            "charles schwab",
            10,
            &RankConfig::default(),
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap()
        .hits;
    assert_eq!(hits[0].domain, "schwab.com", "{hits:?}");
}

#[test]
fn bare_sites_only_meaning_finds_are_left_out() {
    let records = vec![
        site(
            "realpython.com",
            Some("Real Python"),
            Some("Python tutorials, from managing versions with pyenv to packaging"),
            &[],
            &[],
            obscure(60_000, 200),
        ),
        // No title or description, near "pyenv" in meaning only: expired
        // domains, better linked than the tutorials.
        site("makewebeasy.co", None, None, &[], &[], obscure(20_000, 400)),
        site(
            "deleciousfood.com",
            None,
            None,
            &[],
            &[],
            obscure(900_000, 3),
        ),
        // Bare too, but its address shows the query's word.
        site("pyenvtips.net", None, None, &[], &[], obscure(900_000, 3)),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![
        ("makewebeasy.co", 1.0),
        ("deleciousfood.com", 1.0),
        ("pyenvtips.net", 0.9),
        ("realpython.com", 0.6),
    ]);
    let found = |query: &str| -> Vec<String> {
        let results = searcher
            .search_meaning(
                query,
                10,
                &RankConfig::default(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap();
        results.hits.into_iter().map(|hit| hit.domain).collect()
    };
    assert_eq!(found("pyenv"), ["realpython.com", "pyenvtips.net"]);
    let unrelated = found("starship prompt");
    assert!(
        !unrelated
            .iter()
            .any(|d| d == "makewebeasy.co" || d == "deleciousfood.com"),
        "{unrelated:?}"
    );
    // Named, a bare site is still found.
    assert_eq!(found("makewebeasy")[0], "makewebeasy.co");
}

#[test]
fn a_number_alone_matches_nothing_in_a_query_of_other_words() {
    let records = vec![
        site(
            "glastonburyfestivals.co.uk",
            Some("Glastonbury Festival"),
            Some("Line-up, tickets and news of the festival."),
            &[],
            &[],
            ranked(40_000, 900),
        ),
        site(
            "nationschampionshiprugby.com",
            Some("Nations Championship"),
            Some("The rugby Nations Championship: 2026 fixtures and results."),
            &[],
            &[],
            ranked(20_000, 2_000),
        ),
        site(
            "lottery.example",
            Some("Lottery"),
            Some("Lottery 2026 draws and results."),
            &[],
            &[],
            ranked(30_000, 800),
        ),
        site(
            "163.com",
            Some("NetEase"),
            None,
            &[],
            &[],
            popular(100, 50_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = near_and_far(
        &[
            ("glastonburyfestivals.co.uk", 1.0),
            ("nationschampionshiprugby.com", 0.45),
            ("lottery.example", 0.4),
        ],
        &[],
    );
    let found = |query: &str| {
        let hits = searcher
            .search_meaning(
                query,
                10,
                &RankConfig::default(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap()
            .hits;
        domains(&hits)
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    // The year says which festival, not what: sites with only "2026" go,
    // however near in meaning.
    assert_eq!(found("glastonbury 2026"), ["glastonburyfestivals.co.uk"]);
    // With another word they have, they stay.
    let rugby = found("rugby 2026");
    assert!(
        rugby.contains(&"nationschampionshiprugby.com".to_string()),
        "{rugby:?}"
    );
    // A number alone is the whole query.
    assert!(found("2026").contains(&"lottery.example".to_string()));
    // A site the query starts by naming keeps its place.
    assert_eq!(found("163 mail")[0], "163.com");
}
