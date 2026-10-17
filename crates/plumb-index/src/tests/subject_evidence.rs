//! A query's subject: the sites that show they are about it rank before
//! partial names and bare domains.

use super::*;

#[test]
fn named_subject_evidence_guards_task_only_semantic_providers() {
    let records = [
        site(
            "aster.example",
            Some("Aster research organization"),
            None,
            &["Aster"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "nimbus.example",
            Some("Refund qz 7 API project government policy guidance status"),
            None,
            &[],
            &[],
            popular(50, 40_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("aster.example", 0.4), ("nimbus.example", 1.0)]);
    for query in [
        "aster refund",
        "aster qz",
        "aster 7",
        "aster api",
        "aster project guidance",
        "aster government policy guidance status",
    ] {
        let pool = searcher
            .candidate_pool(
                query,
                &experimental_rank_config(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap();
        let owner = pool
            .evidence
            .iter()
            .find(|e| e.domain == "aster.example")
            .unwrap();
        let provider = pool
            .evidence
            .iter()
            .find(|e| e.domain == "nimbus.example")
            .unwrap();
        assert_eq!(owner.named_subject_words, 1, "{query}: {owner:?}");
        assert_eq!(owner.subject_coverage, Some(1.0));
        assert_eq!(owner.subject_remaining_coverage, Some(0.0));
        assert_eq!(owner.relevance_tier, 0, "{query}: {owner:?}");
        assert_eq!(provider.named_subject_words, 1);
        assert_eq!(provider.subject_coverage, Some(0.0));
        assert_eq!(provider.semantic_closeness, Some(1.0));
        assert_eq!(provider.relevance_tier, 0, "{query}: {provider:?}");
        let hits = searcher
            .search_meaning(
                query,
                10,
                &RankConfig {
                    partial_closeness: None,
                    ..experimental_rank_config()
                },
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap()
            .hits;
        assert_eq!(hits[0].domain, "aster.example", "{query}: {hits:?}");
        let provider = hits.iter().find(|hit| hit.domain == "nimbus.example");
        if query == "aster 7" {
            // Sharing only a number with the query, it is not listed.
            assert!(provider.is_none(), "{hits:?}");
            continue;
        }
        let provider = provider.unwrap();
        assert_eq!(provider.text_score, 0.0);
        assert_eq!(provider.score, 0.0);
    }
}

#[test]
fn a_bare_full_query_domain_cannot_outrank_a_corroborated_subject_fallback() {
    let records = [
        site(
            "aster.example",
            Some("Aster research organization"),
            None,
            &["Aster"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "asterrefund.test",
            None,
            None,
            &[],
            &[],
            obscure(500_000, 0),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("aster.example", 0.4), ("asterrefund.test", 1.0)]);
    for vector in [None, Some(&meaning as &dyn Meaning)] {
        let hits = searcher
            .search_meaning(
                "aster refund",
                10,
                &experimental_rank_config(),
                &SearchOptions::default(),
                vector,
            )
            .unwrap()
            .hits;
        assert_eq!(hits[0].domain, "aster.example", "{hits:?}");
        let copier = hits
            .iter()
            .find(|hit| hit.domain == "asterrefund.test")
            .unwrap();
        let evidence = copier.query_evidence.as_ref().unwrap();
        assert!(evidence.full_name);
        assert_eq!(evidence.subject_coverage, Some(0.0));
        assert_eq!(evidence.relevance_tier, 0);
        assert_eq!(copier.score, 0.0);
    }
    assert_eq!(top(&searcher, "asterrefund.test"), "asterrefund.test");
    let mut records = records;
    site::add_alias(&mut records[1], "Aster refund");
    let (_dir, searcher) = build(&records);
    let pool = searcher
        .candidate_pool(
            "aster refund",
            &experimental_rank_config(),
            &SearchOptions::default(),
            None,
        )
        .unwrap();
    let corroborated = pool
        .evidence
        .iter()
        .find(|e| e.domain == "asterrefund.test")
        .unwrap();
    assert_eq!(corroborated.relevance_tier, 1);
    assert_eq!(corroborated.subject_coverage, None);
    assert_eq!(corroborated.named_subject_words, 0);
}

#[test]
fn strong_semantic_ordering_does_not_claim_indexed_task_verification() {
    let subject = "Aster Nimbus Zephyr Lunar Observatory Institute Lab";
    let records = [
        site(
            "aster.example",
            Some(subject),
            None,
            &[subject],
            &[],
            popular(100, 30_000),
        ),
        site(
            "nimbus.example",
            Some("Refund guidance"),
            None,
            &[],
            &[],
            obscure(500_000, 5),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("aster.example", 0.9), ("nimbus.example", 1.0)]);
    let pool = searcher
        .candidate_pool(
            &format!("{subject} refund"),
            &experimental_rank_config(),
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap();
    let owner = pool
        .evidence
        .iter()
        .find(|e| e.domain == "aster.example")
        .unwrap();
    assert!(owner.substantive_coverage > 0.75);
    assert_eq!(owner.named_subject_words, 7);
    assert_eq!(owner.subject_remaining_coverage, Some(0.0));
    assert_eq!(owner.relevance_tier, 1);
    // Without vectors, a long name alone cannot certify the remainder.
    let pool = searcher
        .candidate_pool(
            &format!("{subject} refund"),
            &experimental_rank_config(),
            &SearchOptions::default(),
            None,
        )
        .unwrap();
    let owner = pool
        .evidence
        .iter()
        .find(|e| e.domain == "aster.example")
        .unwrap();
    assert_eq!(owner.subject_remaining_coverage, Some(0.0));
    assert_eq!(owner.relevance_tier, 0);
}

#[test]
fn subject_and_task_support_use_one_semantic_floor_across_domains() {
    let records = [
        site(
            "aster.example",
            Some("Aster refunds"),
            None,
            &["Aster"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "nimbus.example",
            Some("Aster claim status"),
            None,
            &[],
            &[],
            popular(500, 20_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("aster.example", 0.4), ("nimbus.example", 0.4)]);
    let pool = searcher
        .candidate_pool(
            "aster refund claim status",
            &experimental_rank_config(),
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap();
    for evidence in &pool.evidence {
        assert_eq!(evidence.subject_coverage, Some(1.0), "{evidence:?}");
        assert!(evidence.subject_remaining_coverage.unwrap() > 0.0);
        assert!(evidence.subject_remaining_coverage.unwrap() < 1.0);
        assert!(evidence.substantive_coverage < 0.75, "{evidence:?}");
        assert_eq!(evidence.semantic_closeness, Some(0.4));
        assert_eq!(evidence.relevance_tier, 1, "{evidence:?}");
    }
}

#[test]
fn descriptive_semantic_recall_does_not_require_a_named_subject() {
    let records = [
        site(
            "aster.example",
            Some("Aster organization"),
            None,
            &[],
            &[],
            popular(100, 30_000),
        ),
        site(
            "nimbus.example",
            Some("Optical telescopes"),
            None,
            &[],
            &[],
            obscure(500_000, 5),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("nimbus.example", 0.4)]);
    let hits = searcher
        .search_meaning(
            "celestial observation instruments",
            10,
            &experimental_rank_config(),
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap()
        .hits;
    assert_eq!(hits[0].domain, "nimbus.example");
    let evidence = hits[0].query_evidence.as_ref().unwrap();
    assert_eq!(evidence.named_subject_words, 0);
    assert_eq!(evidence.subject_coverage, None);
    assert_eq!(evidence.query_coverage, 0.0);
    assert_eq!(evidence.relevance_tier, 1);
}

#[test]
fn a_name_inside_a_descriptive_phrase_does_not_block_broad_semantic_recall() {
    let records = [
        site(
            "aster.example",
            Some("Aster programming tool"),
            None,
            &["Aster"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "nimbus.example",
            Some("Astronomy encyclopedia"),
            Some("Dwarf planets and orbital bodies"),
            &[],
            &[],
            obscure(500_000, 5),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("aster.example", 0.2), ("nimbus.example", 1.0)]);
    let hits = searcher
        .search_meaning(
            "the aster and the dwarf planet",
            10,
            &experimental_rank_config(),
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap()
        .hits;
    assert_eq!(hits[0].domain, "nimbus.example", "{hits:?}");
    assert!(!hits.iter().take(3).any(|hit| hit.domain == "aster.example"));
    let evidence = hits[0].query_evidence.as_ref().unwrap();
    assert_eq!(evidence.subject_coverage, None);
    assert!(evidence.substantive_coverage < 0.75);
    assert_eq!(evidence.relevance_tier, 1);
}

#[test]
fn an_uncorroborated_domain_cannot_restrict_the_query_subject() {
    let records = [
        site(
            "aster.example",
            Some("Research platform"),
            None,
            &[],
            &[],
            popular(100, 30_000),
        ),
        site(
            "nimbus.example",
            Some("Refund guidance"),
            None,
            &[],
            &[],
            obscure(500_000, 5),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("nimbus.example", 1.0), ("aster.example", 0.4)]);
    let hits = searcher
        .search_meaning(
            "aster refund",
            10,
            &experimental_rank_config(),
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap()
        .hits;
    assert_eq!(hits[0].domain, "nimbus.example");
    let evidence = hits[0].query_evidence.as_ref().unwrap();
    assert_eq!(evidence.named_subject_words, 0);
    assert_eq!(evidence.subject_coverage, None);
    assert_eq!(evidence.relevance_tier, 1);
}

#[test]
fn partial_names_compete_with_indexed_subject_and_semantic_evidence() {
    let records = [
        site(
            "aster.example",
            Some("Aster API guidance refunds"),
            Some("Lunar status docs reference"),
            &["Aster", "Aster qc", "Aster q7", "Aster zephyr2025"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "nimbus.example",
            Some("Nimbus research platform"),
            None,
            &["Nimbus"],
            &[],
            popular(500, 20_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    for query in [
        "aster qc docs",
        "aster q7 api",
        "aster zephyr2025 guidance",
        "aster lunar refund status",
    ] {
        let pool = searcher
            .candidate_pool(
                query,
                &experimental_rank_config(),
                &SearchOptions::default(),
                None,
            )
            .unwrap();
        let evidence = pool
            .evidence
            .iter()
            .find(|e| e.domain == "aster.example")
            .unwrap();
        assert!(!evidence.full_name, "{query}: {evidence:?}");
        assert_eq!(evidence.relevance_tier, 1, "{query}: {evidence:?}");
        assert!(
            evidence.substantive_coverage > 0.99,
            "{query}: {evidence:?}"
        );
        assert_eq!(top(&searcher, query), "aster.example");
    }
    let meaning = FixedMeaning(vec![("nimbus.example", 0.6)]);
    let hits = searcher
        .search_meaning(
            "nimbus q9 api",
            10,
            &experimental_rank_config(),
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap()
        .hits;
    let supported = hits
        .iter()
        .find(|hit| hit.domain == "nimbus.example")
        .unwrap();
    assert_eq!(supported.query_evidence.as_ref().unwrap().relevance_tier, 1);
    assert_eq!(
        supported
            .query_evidence
            .as_ref()
            .unwrap()
            .subject_remaining_coverage,
        Some(0.0)
    );
    assert_eq!(
        supported
            .query_evidence
            .as_ref()
            .unwrap()
            .semantic_closeness,
        Some(0.6)
    );
    let mut records = records;
    records[1].about = Some("Nimbus q9 API reference".into());
    let (_dir, searcher) = build(&records);
    let hits = searcher
        .search_meaning(
            "nimbus q9 api",
            10,
            &experimental_rank_config(),
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap()
        .hits;
    let supported = hits
        .iter()
        .find(|hit| hit.domain == "nimbus.example")
        .unwrap()
        .query_evidence
        .as_ref()
        .unwrap();
    assert_eq!(supported.subject_coverage, Some(1.0));
    assert_eq!(supported.subject_remaining_coverage, Some(1.0));
    assert_eq!(supported.relevance_tier, 1);
}

#[test]
fn a_bare_full_query_domain_does_not_supply_a_partial_names_missing_subject() {
    let records = [
        site(
            "aster.example",
            Some("Aster research platform"),
            None,
            &["Aster"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "aster-lunar-nebula.example",
            None,
            None,
            &[],
            &[],
            obscure(300_000, 0),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let pool = searcher
        .candidate_pool(
            "aster lunar nebula",
            &experimental_rank_config(),
            &SearchOptions::default(),
            None,
        )
        .unwrap();
    let evidence = pool
        .evidence
        .iter()
        .find(|e| e.domain == "aster.example")
        .unwrap();
    assert_eq!(evidence.relevance_tier, 0, "{evidence:?}");
    // Strong semantic support survives disabled score blending while
    // diagnostics still report task words absent from indexed text.
    let meaning = FixedMeaning(vec![("aster.example", 0.6)]);
    let hits = searcher
        .search_meaning(
            "aster lunar nebula",
            10,
            &experimental_rank_config(),
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap()
        .hits;
    let supported = hits
        .iter()
        .find(|hit| hit.domain == "aster.example")
        .unwrap();
    let evidence = supported.query_evidence.as_ref().unwrap();
    assert_eq!(evidence.semantic_closeness, Some(0.6));
    assert_eq!(evidence.relevance_tier, 1);
    assert_eq!(evidence.subject_remaining_coverage, Some(0.0));
    let unadjusted = searcher
        .search_meaning(
            "aster lunar nebula",
            10,
            &RankConfig {
                whole_query_relevance: false,
                ..experimental_rank_config()
            },
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap()
        .hits;
    let unadjusted = unadjusted
        .iter()
        .find(|hit| hit.domain == "aster.example")
        .unwrap();
    let without_meaning = searcher
        .search_full(
            "aster lunar nebula",
            10,
            &RankConfig {
                whole_query_relevance: false,
                ..experimental_rank_config()
            },
            &SearchOptions::default(),
        )
        .unwrap()
        .hits;
    let without_meaning = without_meaning
        .iter()
        .find(|hit| hit.domain == "aster.example")
        .unwrap();
    // Nobody knows aster-lunar-nebula.example by that name, so it does not
    // make the query a name: meaning still counts in the text score.
    assert_ne!(
        unadjusted.text_score, without_meaning.text_score,
        "a bare full-name rival does not disable semantic score blending"
    );
}

#[test]
fn whole_query_and_alias_evidence_survives_a_low_semantic_score() {
    let records = [
        site(
            "w3.org",
            Some("W3C"),
            Some("Web standards consortium"),
            &["W3C"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "apache.org",
            Some("Apache Software Foundation"),
            None,
            &["Apache", "Kafka"],
            &[],
            popular(500, 20_000),
        ),
        site(
            "unrelated.example",
            Some("Online software"),
            None,
            &[],
            &[],
            obscure(500_000, 5),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![
        ("unrelated.example", 0.8),
        ("w3.org", 0.45),
        ("apache.org", 0.36),
    ]);
    for (query, wanted) in [
        ("Where is W3C online", "w3.org"),
        ("Where is W3C official website", "w3.org"),
        ("apache kafka", "apache.org"),
    ] {
        let mut hits = searcher
            .search_meaning(
                query,
                10,
                &experimental_rank_config(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap()
            .hits;
        assert!(
            hits.iter().any(|hit| hit.domain == wanted),
            "{query}: {hits:?}"
        );
        let wanted_hit = hits.iter().find(|hit| hit.domain == wanted).unwrap();
        assert_eq!(
            wanted_hit.query_evidence.as_ref().unwrap().relevance_tier,
            1,
            "{query}: {hits:?}"
        );
        learned::reorder(learned::Model::builtin(), query, &mut hits, &mut Vec::new());
        assert_eq!(hits[0].domain, wanted, "{query}: {hits:?}");
    }
}

#[test]
fn task_queries_require_subject_evidence_and_do_not_promote_animal_namesakes() {
    let records = [
        site(
            "openai.com",
            Some("OpenAI research"),
            None,
            &["OpenAI"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "gitlab.com",
            Some("GitLab software platform"),
            None,
            &["GitLab"],
            &[],
            popular(500, 20_000),
        ),
        site(
            "mozilla.org",
            Some("Mozilla browser project"),
            None,
            &["Mozilla"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "ssa.gov",
            Some("Social Security Administration"),
            Some("Independent federal agency"),
            &["SSA", "Social Security"],
            &[],
            popular(1_000, 20_000),
        ),
        site(
            "bankofamerica.com",
            Some("Bank of America"),
            None,
            &["Bank of America"],
            &[],
            popular(500, 30_000),
        ),
        site(
            "python.org",
            Some("Python programming language"),
            None,
            &["Python"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "jaguar.com",
            Some("Jaguar luxury cars"),
            None,
            &["Jaguar"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "subject-guide.example",
            Some("Python snake family Jaguar animal species Panthera onca jaguar"),
            None,
            &[],
            &[],
            obscure(500_000, 5),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![
        ("subject-guide.example", 0.8),
        ("openai.com", 0.23),
        ("gitlab.com", 0.46),
        ("mozilla.org", 0.0),
        ("ssa.gov", 0.238),
        ("bankofamerica.com", 0.0),
        ("python.org", 0.30),
        ("jaguar.com", 0.37),
    ]);
    // These original regression queries cannot borrow a name-only
    // score when the fixture lacks the requested task/qualifier and
    // supplies only a weak semantic match. Online is still a light
    // presentation modifier of an otherwise complete indexed name.
    for (query, wanted, tier) in [
        ("openai api", "openai.com", 0),
        ("gitlab ci docs", "gitlab.com", 0),
        ("Mozilla project website address", "mozilla.org", 0),
        ("Social Security retirement benefits", "ssa.gov", 0),
        ("SSA retirement benefits guidance", "ssa.gov", 0),
        ("Government Social Security benefit guidance", "ssa.gov", 0),
        ("Bank of America online", "bankofamerica.com", 1),
    ] {
        let pool = searcher
            .candidate_pool(
                query,
                &experimental_rank_config(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap();
        let evidence = pool.evidence.iter().find(|e| e.domain == wanted).unwrap();
        assert_eq!(evidence.relevance_tier, tier, "{query}: {evidence:?}");
        let mut hits = searcher
            .search_meaning(
                query,
                10,
                &experimental_rank_config(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap()
            .hits;
        if let Some(hit) = hits.iter().find(|hit| hit.domain == wanted) {
            assert_eq!(
                hit.query_evidence.as_ref().unwrap().relevance_tier,
                tier,
                "{query}: {hit:?}"
            );
        }
        learned::reorder(learned::Model::builtin(), query, &mut hits, &mut Vec::new());
        if tier == 1 {
            assert_eq!(hits[0].domain, wanted, "{query}: {hits:?}");
        }
    }
    for (query, wrong) in [
        ("Python snake family", "python.org"),
        ("Python snake family docs", "python.org"),
        ("Family of python snakes", "python.org"),
        ("Jaguar animal species", "jaguar.com"),
        ("Jaguar animal species guidance", "jaguar.com"),
        ("Panthera onca jaguar", "jaguar.com"),
    ] {
        let hits = searcher
            .search_meaning(
                query,
                10,
                &experimental_rank_config(),
                &SearchOptions {
                    exact: true,
                    ..Default::default()
                },
                Some(&meaning),
            )
            .unwrap()
            .hits;
        if let Some(hit) = hits.iter().find(|hit| hit.domain == wrong) {
            assert_eq!(
                hit.query_evidence.as_ref().unwrap().relevance_tier,
                0,
                "{query}: {hits:?}"
            );
        }
        assert_ne!(hits[0].domain, wrong, "{query}: {hits:?}");
    }
}
