//! Which sites found by meaning a search keeps, and how far meaning can
//! stand in for words.

use super::*;

#[test]
fn release_defaults_keep_the_guard_opt_in() {
    assert!(!RankConfig::default().whole_query_relevance);
    assert!(
        !serde_json::from_str::<RankConfig>("{}")
            .unwrap()
            .whole_query_relevance
    );
    assert!(
        serde_json::from_str::<RankConfig>("{\"whole_query_relevance\":true}")
            .unwrap()
            .whole_query_relevance
    );
}

#[test]
fn default_retention_keeps_sparse_authority_paraphrases() {
    let records = [
        site(
            "w3.org",
            Some("World Wide Web Consortium"),
            None,
            &["W3C"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "travel.state.gov",
            Some("Bureau of Consular Affairs"),
            Some("State Department passport services and application guidance"),
            &[],
            &[],
            popular(1_000, 20_000),
        ),
        // Independent links to an unrelated name cannot impose an
        // experimental subject scope in the conservative release mode.
        site(
            "american.example",
            Some("American magazine"),
            None,
            &[],
            &[("American", 100)],
            popular(500, 20_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    for (query, domain, similarity) in [
        ("Where is W3C online", "w3.org", 0.45623),
        ("US passport renewal guidance", "travel.state.gov", 0.4102),
        (
            "United States passport renew application",
            "travel.state.gov",
            0.3979,
        ),
        (
            "American passport renewal government instructions",
            "travel.state.gov",
            0.4743,
        ),
    ] {
        let meaning = FixedMeaning(vec![(domain, similarity), ("american.example", 0.0)]);
        let hits = searcher
            .search_meaning(
                query,
                10,
                &RankConfig::default(),
                &SearchOptions {
                    exact: true,
                    ..Default::default()
                },
                Some(&meaning),
            )
            .unwrap()
            .hits;
        let owner = hits
            .iter()
            .find(|hit| hit.domain == domain)
            .unwrap_or_else(|| panic!("{query}: {hits:?}"));
        let evidence = owner.query_evidence.as_ref().unwrap();
        assert_eq!(evidence.semantic_closeness, Some(similarity));
        assert_eq!(evidence.relevance_tier, 0);
        assert_eq!(evidence.named_subject_words, 0);
        assert_eq!(evidence.subject_coverage, None);
    }
}

#[test]
fn descriptive_retention_requires_calibrated_vectors_and_keeps_collision_floor() {
    for (domain, similarity, retained) in [
        ("authority.example", Some(0.35), true),
        ("authority.example", Some(0.34), false),
        ("authority.example", None, false),
        ("nebula.example", Some(0.4), false),
    ] {
        let records = [
            site(
                domain,
                Some("Application office"),
                None,
                &[],
                &[],
                obscure(700_000, 5),
            ),
            site(
                "reference.example",
                Some("Nebula application guidance"),
                None,
                &[],
                &[],
                popular(500, 20_000),
            ),
        ];
        let (_dir, searcher) = build(&records);
        let mut scores = vec![("reference.example", 0.9)];
        if let Some(similarity) = similarity {
            scores.push((domain, similarity));
        }
        let meaning = FixedMeaning(scores);
        let hits = searcher
            .search_meaning(
                "nebula application guidance",
                10,
                &RankConfig::default(),
                &SearchOptions {
                    exact: true,
                    ..Default::default()
                },
                Some(&meaning),
            )
            .unwrap()
            .hits;
        assert_eq!(
            hits.iter().any(|hit| hit.domain == domain),
            retained,
            "{domain} {similarity:?}: {hits:?}"
        );
    }
}

#[test]
fn calibrated_semantic_candidates_keep_their_scores_and_recall() {
    let records = [
        site(
            "travel.state.gov",
            Some("Bureau of Consular Affairs"),
            Some("State Department services for citizens abroad"),
            &[],
            &[],
            popular(1_000, 20_000),
        ),
        site(
            "passport-services.example",
            Some("Passport renewal services"),
            None,
            &[],
            &[],
            obscure(600_000, 5),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![
        ("passport-services.example", 0.8),
        ("travel.state.gov", 0.48),
    ]);
    for query in [
        "US passport renewal guidance",
        "United States passport renew application",
        "American passport renewal government instructions",
    ] {
        let options = SearchOptions {
            exact: true,
            ..Default::default()
        };
        let hits = searcher
            .search_meaning(query, 10, &RankConfig::default(), &options, Some(&meaning))
            .unwrap()
            .hits;
        let authority = hits
            .iter()
            .find(|hit| hit.domain == "travel.state.gov")
            .unwrap_or_else(|| panic!("{query}: {hits:?}"));
        assert_eq!(authority.query_evidence.as_ref().unwrap().relevance_tier, 0);
        // The semantic score was already calibrated by meaning_weight;
        // whole-query coverage must not multiply it a second time.
        assert!(
            authority.text_score >= 0.7 * 0.48 - 1e-5,
            "{query}: {authority:?}"
        );
        assert!(
            authority.score
                >= 0.5 * authority.link_score * (0.7 * 0.48 / 0.35) + 0.5 * 0.7 * 0.48 - 1e-5,
            "{query}: the calibrated prior must survive too: {authority:?}"
        );
    }
}

#[test]
fn task_wording_and_qualifier_shape_do_not_protect_partial_names() {
    let records = [site(
        "aster.example",
        Some("Aster research platform"),
        None,
        &["Aster"],
        &[],
        popular(100, 30_000),
    )];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("aster.example", 0.1)]);
    for query in [
        "aster qc docs",
        "aster q7 api",
        "aster zephyr2025 guidance",
        "aster lunar refund",
        "aster lunar refund status",
        "aster api",
        "aster manual",
        "aster policy",
        "aster retirement benefits",
        "aster baggage policy",
        "aster student login",
        "aster order tracking",
        "aster online banking",
        "aster tickets",
        "aster agency website",
        "government aster website",
    ] {
        for vector in [None, Some(&meaning as &dyn Meaning)] {
            let pool = searcher
                .candidate_pool(
                    query,
                    &experimental_rank_config(),
                    &SearchOptions::default(),
                    vector,
                )
                .unwrap();
            let evidence = pool
                .evidence
                .iter()
                .find(|e| e.domain == "aster.example")
                .unwrap();
            assert_eq!(evidence.relevance_tier, 0, "{query}: {evidence:?}");
            let hits = searcher
                .search_meaning(
                    query,
                    10,
                    &experimental_rank_config(),
                    &SearchOptions::default(),
                    vector,
                )
                .unwrap()
                .hits;
            if let Some(hit) = hits.iter().find(|hit| hit.domain == "aster.example") {
                assert_eq!(
                    hit.query_evidence.as_ref().unwrap().relevance_tier,
                    0,
                    "{query}: {hit:?}"
                );
            }
        }
    }
}

#[test]
fn plain_semantics_cannot_restore_a_missing_named_subject() {
    struct PlainMeaning(FixedMeaning);
    impl Meaning for PlainMeaning {
        fn nearest(&self) -> Vec<String> {
            self.0.nearest()
        }
        fn closeness(&self, domain: &str) -> Option<f32> {
            self.0.closeness(domain)
        }
        fn plain_closeness(&self, domain: &str) -> Option<Option<f32>> {
            Some(self.0.closeness(domain))
        }
    }
    let records = [
        site(
            "aster.example",
            Some("Aster organization"),
            None,
            &["Aster"],
            &[],
            popular(100, 30_000),
        ),
        site(
            "nimbus.example",
            Some("Refund guidance"),
            None,
            &[],
            &[],
            popular(50, 40_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = PlainMeaning(FixedMeaning(vec![
        ("aster.example", 0.4),
        ("nimbus.example", 1.0),
    ]));
    let hits = searcher
        .search_meaning(
            "aster refund",
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
    let provider = hits
        .iter()
        .find(|hit| hit.domain == "nimbus.example")
        .unwrap();
    assert_eq!(provider.placing_text_score, Some(0.0));
    assert_eq!(provider.score, 0.0);
    let hits = searcher
        .search_meaning(
            "refund guidance",
            10,
            &experimental_rank_config(),
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap()
        .hits;
    assert_eq!(hits[0].placing_text_score, Some(hits[0].text_score));
}

#[test]
fn semantic_support_keeps_missing_task_evidence_visible() {
    let records = [site(
        "aster.example",
        Some("Aster platform"),
        Some("Docs and online services"),
        &["Aster"],
        &[],
        popular(100, 30_000),
    )];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("aster.example", 0.9)]);
    for (query, task) in [("aster q9 docs", Some(0.0)), ("aster online", None)] {
        let pool = searcher
            .candidate_pool(
                query,
                &experimental_rank_config(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap();
        let owner = &pool.evidence[0];
        assert_eq!(owner.subject_remaining_coverage, task, "{query}: {owner:?}");
        assert_eq!(owner.relevance_tier, 1, "{query}: {owner:?}");
    }
}

#[test]
fn self_labels_and_competing_unsupported_aliases_do_not_scope_descriptions() {
    for (aliases, first, second) in [
        (false, "nimbus.example", "nimbus-town.example"),
        (true, "nimbus.example", "nimbus-town.example"),
        (true, "first.github.io", "second.github.io"),
    ] {
        let names: &[&str] = if aliases { &["Nimbus"] } else { &[] };
        let records = [
            site(
                first,
                Some("Nimbus village"),
                None,
                names,
                &[],
                popular(100, 30_000),
            ),
            site(
                second,
                Some("Nimbus magazine"),
                None,
                names,
                &[],
                popular(200, 20_000),
            ),
            site(
                "aster.example",
                Some("Research project guidance status"),
                None,
                &[],
                &[],
                obscure(500_000, 5),
            ),
        ];
        let (_dir, searcher) = build(&records);
        let meaning = FixedMeaning(vec![(first, 0.1), (second, 0.1), ("aster.example", 1.0)]);
        for vector in [None, Some(&meaning as &dyn Meaning)] {
            let hits = searcher
                .search_meaning(
                    "nimbus research project guidance status",
                    10,
                    &experimental_rank_config(),
                    &SearchOptions {
                        exact: true,
                        ..Default::default()
                    },
                    vector,
                )
                .unwrap()
                .hits;
            assert_eq!(hits[0].domain, "aster.example", "{aliases}: {hits:?}");
            let evidence = hits[0].query_evidence.as_ref().unwrap();
            assert_eq!(evidence.named_subject_words, 0);
            assert_eq!(evidence.named_subject_domain, None);
            assert_eq!(evidence.subject_coverage, None);
            assert_eq!(evidence.relevance_tier, 1);
        }
    }
}

#[test]
fn weak_known_subjects_keep_task_only_nearest_results_as_fallbacks() {
    for (query, domain, name, score) in [
        ("openai api", "openai.com", "OpenAI", 0.231),
        ("aws s3 docs", "aws.amazon.com", "AWS", 0.126),
        ("gitlab ci docs", "gitlab.com", "GitLab", 0.467),
    ] {
        let records = [
            site(domain, Some(name), None, &[name], &[], popular(100, 30_000)),
            site(
                "task-provider.example",
                Some("API S3 CI docs reference"),
                None,
                &[],
                &[],
                popular(50, 40_000),
            ),
        ];
        let (_dir, searcher) = build(&records);
        let meaning = FixedMeaning(vec![(domain, score), ("task-provider.example", 1.0)]);
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
        let owner = hits
            .iter()
            .find(|hit| hit.domain == domain)
            .unwrap()
            .query_evidence
            .as_ref()
            .unwrap();
        assert_eq!(owner.subject_coverage, Some(1.0));
        assert_eq!(owner.subject_remaining_coverage, Some(0.0));
        assert_eq!(owner.relevance_tier, 0);
        assert_eq!(owner.named_subject_domain.as_deref(), Some(domain));
        assert!(matches!(
            owner.named_subject_reason.as_deref(),
            Some("alias" | "unambiguous_alias" | "structured_name" | "unambiguous_structured_name")
        ));
        let pool = searcher
            .candidate_pool(
                query,
                &experimental_rank_config(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap();
        let provider = pool
            .evidence
            .iter()
            .find(|e| e.domain == "task-provider.example")
            .unwrap();
        assert_eq!(provider.subject_coverage, Some(0.0));
        assert_eq!(provider.relevance_tier, 0);
        if let Some(provider) = hits
            .iter()
            .find(|hit| hit.domain == "task-provider.example")
        {
            assert_eq!(provider.score, 0.0);
        }
        assert_eq!(hits[0].domain, domain, "{query}: {hits:?}");
        learned::reorder(learned::Model::builtin(), query, &mut hits, &mut Vec::new());
        assert_eq!(hits[0].domain, domain, "{query}: {hits:?}");
    }
}

#[test]
fn sparse_subject_owners_keep_existing_strong_semantic_calibration() {
    let records = [site(
        "aster.example",
        Some("Aster platform"),
        None,
        &["Aster"],
        &[],
        popular(100, 30_000),
    )];
    let (_dir, searcher) = build(&records);
    for score in [0.55, 0.793, 0.970] {
        let meaning = FixedMeaning(vec![("aster.example", score)]);
        let hits = searcher
            .search_meaning(
                "aster runtime api",
                10,
                &experimental_rank_config(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap()
            .hits;
        let owner = &hits[0];
        let evidence = owner.query_evidence.as_ref().unwrap();
        assert_eq!(evidence.subject_remaining_coverage, Some(0.0));
        assert_eq!(evidence.relevance_tier, 1);
        assert!(owner.text_score >= 0.7 * score, "{owner:?}");
    }
    // An independent alias on another hostname keeps the existing
    // descriptive calibration, as a sparse authority homepage does.
    let records = [site(
        "authority.example",
        Some("Nimbus office"),
        None,
        &["Nimbus"],
        &[],
        popular(100, 30_000),
    )];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![("authority.example", 0.406)]);
    let hits = searcher
        .search_meaning(
            "nimbus renewal",
            10,
            &experimental_rank_config(),
            &SearchOptions::default(),
            Some(&meaning),
        )
        .unwrap()
        .hits;
    let evidence = hits[0].query_evidence.as_ref().unwrap();
    assert_eq!(evidence.subject_remaining_coverage, Some(0.0));
    assert_eq!(evidence.relevance_tier, 1);
}

#[test]
fn complete_structured_identity_precedes_partial_scopes_with_borrowed_text() {
    let mut owner = site(
        "aster.example",
        Some("Borrowed portal"),
        None,
        &["Nimbus Research"],
        &[],
        popular(100, 30_000),
    );
    owner.url = Some("https://other.example/".into());
    site::set_wikidata_about(&mut owner, Some("Research organization".into()));
    let records = [
        owner,
        site(
            "nimbus.example",
            Some("Nimbus Research guidance"),
            None,
            &["Nimbus"],
            &[],
            obscure(500_000, 5),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let hits = experimental_search(&searcher, "nimbus research", 10);
    assert_eq!(hits[0].domain, "aster.example", "{hits:?}");
    let evidence = hits[0].query_evidence.as_ref().unwrap();
    assert!(evidence.full_name);
    assert_eq!(evidence.relevance_tier, 1);
    assert_eq!(evidence.named_subject_words, 0);
}
