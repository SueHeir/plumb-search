use super::*;

#[test]
fn official_site_keeps_api_evidence_when_separating_docs_navigation() {
    // Neither the company's name nor a same-name SDK proves the requested API.
    let mcp = scripted(|query| {
        if query.ends_with("package") {
            return results(
                Vec::new(),
                vec![package_page(
                    "pypi",
                    "anthropic",
                    None,
                    Some("https://anthropic.readthedocs.io/"),
                )],
            );
        }
        let mut anthropic = titled("anthropic.com", "Home \\ Anthropic", 0.8, false);
        anthropic.official = true;
        results(vec![anthropic], Vec::new())
    });
    let answer = official(&mcp, "anthropic api docs");
    assert_eq!(answer["found"], false, "{answer}");
    assert_eq!(answer["confidence"], "low");
    assert!(answer["url"].is_null());
}

#[test]
fn official_site_does_not_erase_an_email_category_missing_from_the_identity() {
    let mcp = scripted(|_| {
        let mut office = titled("office.com", "Office.com", 0.9, false);
        office.official = true;
        let mut outlook = titled("outlook.live.com", "Outlook", 0.8, false);
        outlook.official = true;
        outlook.score = 0.9;
        outlook.description = Some("Microsoft free web-based email service".into());
        results(vec![office, outlook], Vec::new())
    });
    let answer = official(&mcp, "outlook email");
    assert_eq!(answer["found"], false, "{answer}");
    assert!(answer["alternatives"]
        .as_array()
        .unwrap()
        .iter()
        .any(|alt| alt["domain"] == "outlook.live.com"));
}

#[test]
fn official_site_keeps_a_partial_numeric_address_as_a_suggestion() {
    let mcp = scripted(|_| {
        let mut rubiks = titled("rubiks.com", "Rubik's", 0.8, false);
        rubiks.official = true;
        let mut cube20 = hit("cube20.org", 0.5, 0.1, false);
        cube20.title = None;
        results(vec![rubiks, cube20], Vec::new())
    });
    let answer = official(&mcp, "cube20 God's number");
    assert_eq!(answer["found"], false, "{answer}");
    assert_eq!(answer["alternatives"][0]["domain"], "cube20.org");
    assert_eq!(answer["confidence"], "low");
}

#[test]
fn official_site_binds_package_docs_to_the_typed_entity_despite_popular_spelling() {
    for mode in 0..3 {
        let mcp = scripted(move |query| {
            if query.ends_with("package") {
                return results(
                    Vec::new(),
                    vec![package_page(
                        "pypi",
                        "mypy",
                        Some("https://mypy-lang.org/"),
                        Some("https://mypy.readthedocs.io/en/stable/"),
                    )],
                );
            }
            let mut hits = Vec::new();
            if mode != 2 {
                let mut mapy = titled("mapy.com", "Mapy.com", 0.9, true);
                mapy.official = true;
                hits.push(mapy);
            }
            if mode != 0 {
                let mut project = titled("mypy-lang.org", "mypy", 0.9, true);
                project.official = true;
                project.score = 0.5;
                hits.push(project);
            }
            let mut found = results(hits, vec![article("Mapy", Some("mapy.com"), None)]);
            found.spelling = Some(plumb_index::Spelling {
                query: "mapy documentation".into(),
                site: Some("mapy.com".into()),
                applied: true,
            });
            found
        });
        let answer = official(&mcp, "mypy documentation");
        if mode == 0 {
            assert_eq!(answer["found"], false, "{answer}");
            assert_eq!(answer["confidence"], "low");
            assert_eq!(
                answer["package_home"],
                "https://mypy.readthedocs.io/en/stable/"
            );
            continue;
        }
        assert_eq!(answer["found"], true, "{answer}");
        assert_eq!(answer["url"], "https://mypy.readthedocs.io/en/stable/");
        assert_eq!(answer["confidence"], "medium");
        assert!(answer["did_you_mean"].is_null());
        let why = answer["why"].to_string();
        assert!(
            why.contains("PyPI") && !why.contains("Wikidata") && !why.contains("well-known"),
            "{why}"
        );
    }
}

#[test]
fn official_site_task_domains_cannot_displace_the_entity_even_for_short_brands() {
    for (name, task_domain, task_title, entity_domain, entity_title) in [
        (
            "Xe currency converter",
            "converter.app",
            "Text converter",
            "xe.com",
            "Xe currency converter",
        ),
        (
            "Chroma vector database",
            "vector.dev",
            "Vector",
            "trychroma.com",
            "Chroma vector database",
        ),
    ] {
        let mut entity = titled(entity_domain, entity_title, 0.5, true);
        entity.official = true;
        let mcp = server(vec![titled(task_domain, task_title, 0.9, true), entity]);
        let answer = official(&mcp, name);
        assert_eq!(answer["domain"], entity_domain, "{name}: {answer}");
        assert_eq!(answer["found"], true);
    }
    // A short string inside an unrelated word does not identify Xe.
    let answer = official(
        &server(vec![titled("example.com", "Text converter", 0.9, true)]),
        "Xe currency converter",
    );
    assert_eq!(answer["found"], false, "{answer}");
    let mut namesake = titled("chromakey.com", "ChromaKey", 0.9, true);
    namesake.official = true;
    let answer = official(&server(vec![namesake]), "Chroma vector database");
    assert_eq!(answer["found"], false, "{answer}");
}

#[test]
fn official_site_categories_need_full_identity_evidence_without_an_ontology_table() {
    for (name, brand, domain) in [
        ("Xe currency converter", "Xe", "xe.com"),
        ("Chroma vector database", "Chroma", "trychroma.com"),
        ("Outlook email", "Outlook", "outlook.live.com"),
        ("Orion observability pipeline", "Orion", "orion.example"),
    ] {
        assert_eq!(identity::Intent::of(name).entity, name.to_lowercase());
        let mut bare_owner = titled(domain, brand, 0.9, true);
        bare_owner.official = true;
        let answer = official(&server(vec![bare_owner]), name);
        assert_eq!(answer["found"], false, "{name}: {answer}");
        assert_eq!(answer["confidence"], "low");
        assert_eq!(answer["alternatives"][0]["domain"], domain);
        let mut complete = titled(domain, name, 0.9, true);
        complete.official = true;
        let answer = official(&server(vec![complete]), name);
        assert_eq!(answer["found"], true, "{name}: {answer}");
        assert_eq!(answer["domain"], domain);
    }
}

#[test]
fn official_site_cannot_validate_another_entity_from_spelling_or_official_status() {
    let mcp = scripted(|_| {
        let mut mapy = titled("mapy.com", "Mapy", 0.9, true);
        mapy.official = true;
        let mut found = results(vec![mapy], vec![article("Mapy", Some("mapy.com"), None)]);
        found.spelling = Some(plumb_index::Spelling {
            query: "mapy".into(),
            site: Some("mapy.com".into()),
            applied: true,
        });
        found
    });
    let answer = official(&mcp, "mypy");
    assert_eq!(answer["found"], false, "{answer}");
    assert_eq!(answer["status"], "unresolved");
    assert_eq!(answer["confidence"], "low");
    assert!(answer["url"].is_null());
    assert_eq!(answer["did_you_mean"], "mapy");
    assert_eq!(answer["alternatives"][0]["domain"], "mapy.com");
    assert!(!answer["why"].to_string().contains("Wikidata lists"));
    let mut amazon = titled("amazon.com", "Amazon", 0.9, true);
    amazon.official = true;
    let answer = official(
        &server(vec![titled("river.example", "River", 0.1, false), amazon]),
        "Amazon River length",
    );
    assert_eq!(answer["found"], false, "{answer}");
}

#[test]
fn official_site_keeps_matching_brands_that_include_country_names() {
    for (name, title, domain) in [
        ("Bank of America", "Bank of America", "bankofamerica.com"),
        ("Air France", "Air France", "airfrance.com"),
        ("Mapy", "Mapy", "mapy.com"),
        ("Vector", "Vector", "vector.dev"),
        ("X", "X", "x.com"),
        ("R", "The R Project", "r-project.org"),
        ("H&M", "H&M", "hm.com"),
    ] {
        let mut brand = titled(domain, title, 0.8, true);
        brand.official = true;
        let answer = official(&server(vec![brand]), name);
        assert_eq!(answer["found"], true, "{answer}");
        assert_eq!(answer["domain"], domain);
        assert_eq!(answer["confidence"], "high");
    }
}

#[test]
fn official_site_keeps_a_place_whose_name_ends_in_a_country() {
    // New Mexico is not "new" in Mexico, nor New South Wales "new south" in
    // Wales: Wikidata knows each by its whole name.
    let mcp = scripted(|_| {
        let mut state = titled("newmexico.gov", "New Mexico", 0.7, true);
        state.official = true;
        state.country = Some("US".into());
        results(
            vec![state],
            vec![article("New Mexico", Some("newmexico.gov"), None)],
        )
    });
    let answer = official(&mcp, "New Mexico");
    assert_eq!(answer["domain"], "newmexico.gov", "{answer}");
    assert_eq!(answer["confidence"], "high");
    assert!(answer["country_conflict"].is_null(), "{answer}");

    let mcp = scripted(|_| {
        let tourism = titled(
            "newsouthwalestourism.com",
            "New South Wales Tourism - Events Australia",
            0.3,
            false,
        );
        let mut government = titled("nsw.gov.au", "NSW Government", 0.7, true);
        government.official = true;
        government.country = Some("AU".into());
        results(
            vec![tourism, government],
            vec![article("New South Wales", Some("nsw.gov.au"), None)],
        )
    });
    let answer = official(&mcp, "New South Wales");
    assert_eq!(answer["domain"], "nsw.gov.au", "{answer}");
    assert_eq!(answer["confidence"], "high");
}

#[test]
fn official_site_uses_the_query_jurisdiction_and_reports_conflicting_preferences() {
    struct Countries;
    impl SearchBackend for Countries {
        fn search(&self, _: &str, _: usize) -> Result<Vec<Hit>> {
            unreachable!()
        }
        fn num_docs(&self) -> u64 {
            2
        }
        fn search_full(&self, _: &str, _: usize, options: &SearchOptions) -> Result<SearchResults> {
            assert_eq!(options.country.as_deref(), Some("MX"));
            let mut gt = titled("sat.gob.gt", "SAT Guatemala", 0.9, true);
            gt.country = Some("GT".into());
            gt.official = true;
            let mut mx = titled("sat.gob.mx", "SAT", 0.6, true);
            mx.country = Some("MX".into());
            mx.official = true;
            // Deliberately keep the default-country result first to exercise
            // the identity gate independently of the backend's country boost.
            Ok(results(vec![gt, mx], Vec::new()))
        }
    }
    let mcp = Mcp::new(Arc::new(Countries), Some("GT".into()));
    for country in [None, Some("MX"), Some("GT"), Some("any")] {
        let mut args = json!({ "name": "SAT México" });
        if let Some(country) = country {
            args["country"] = json!(country);
        }
        let reply = call(&mcp, "official_site", args);
        let answer = &reply["result"]["structuredContent"];
        assert_eq!(answer["domain"], "sat.gob.mx", "{answer}");
        assert_eq!(answer["found"], true);
        if country.is_none() || country == Some("GT") {
            assert_eq!(answer["country_conflict"]["query"], "MX");
            assert_eq!(answer["country_conflict"]["preference"], "GT");
        } else {
            assert!(answer["country_conflict"].is_null());
        }
    }
}

#[test]
fn official_site_abstains_on_domain_namesakes_and_copied_owner_titles() {
    // These reproduce evidence shapes from the combined candidate run.
    // Even a strong ranking/name/popularity signal is not owner evidence.
    for (name, domain, title, description) in [
        (
            "LifeWiki",
            "life-wiki.com",
            Some("Free encyclopedia, created and edited by volunteers around the world"),
            None,
        ),
        ("MathWorld", "mathworld.com", None, None),
        ("NSSDC", "nssdc.ac.cn", None, None),
        (
            "European Central Bank",
            "zentral-bank.eu",
            Some("European Central Bank"),
            Some("The European Central Bank (ECB) is the central bank of the European Union."),
        ),
        (
            "Yosemite National Park",
            "yosemite.com",
            Some("Yosemite National Park | Lodging, Camping, Attractions | Yosemite.com"),
            Some("Plan your vacation to Yosemite today!"),
        ),
        // A word normally ignored during search is part of this entity.
        ("Delta Center", "delta.com", Some("Delta"), None),
    ] {
        for link_score in [0.1, 0.9] {
            let mut candidate = hit(domain, 2.0, link_score, true);
            candidate.title = title.map(str::to_string);
            candidate.description = description.map(str::to_string);
            let answer = official(&server(vec![candidate]), name);
            assert_eq!(answer["found"], false, "{name}: {answer}");
            assert_eq!(answer["status"], "unresolved");
            assert_eq!(answer["confidence"], "low");
            assert!(answer["url"].is_null());
            assert_eq!(answer["alternatives"][0]["domain"], domain);
        }
    }
}

#[test]
fn official_site_abbreviation_cannot_verify_an_unbound_resource() {
    for (name, acronym, domain, title) in [
        (
            "NSSDC planetary fact sheet",
            "NSSDC",
            "nssdc.ac.cn",
            "NSSDC",
        ),
        ("NASA Science", "NASA", "nasa.gov", "NASA"),
        ("NIST Atomic Spectra Database", "NIST", "nist.gov", "NIST"),
        ("NIST physical constants", "NIST", "nist.gov", "NIST"),
        (
            "US Naval Observatory Astronomical Applications",
            "US",
            "us.es",
            "Universidad de Sevilla",
        ),
    ] {
        let mcp = scripted(move |query| {
            let mut candidate = titled(domain, title, 0.9, true);
            candidate.official = true;
            if query == acronym {
                results(vec![candidate], Vec::new())
            } else {
                results(
                    vec![titled("unrelated.example", "Unrelated", 0.1, false)],
                    Vec::new(),
                )
            }
        });
        let answer = official(&mcp, name);
        assert_eq!(answer["found"], false, "{name}: {answer}");
        assert_eq!(answer["confidence"], "low");
        assert!(answer["url"].is_null());
        assert!(
            answer["alternatives"]
                .as_array()
                .unwrap()
                .iter()
                .any(|alt| alt["domain"] == domain),
            "{answer}"
        );
    }
}

#[test]
fn official_site_database_qualifier_cannot_select_a_newspaper() {
    let mut newspaper = titled("tz.de", "tz", 0.9, true);
    newspaper.description = Some("German daily newspaper published in Munich".into());
    newspaper.official = true;
    let mcp = server(vec![newspaper]);
    let answer = official(&mcp, "tz database");
    assert_eq!(answer["found"], false, "{answer}");
    assert_eq!(answer["alternatives"][0]["domain"], "tz.de");
    assert_eq!(official(&mcp, "tz")["domain"], "tz.de");
}

#[test]
fn official_site_an_official_flag_does_not_bind_a_bare_domain_namesake() {
    for (name, domain) in [("MathWorld", "mathworld.com"), ("NSSDC", "nssdc.ac.cn")] {
        let mut namesake = hit(domain, 2.0, 0.9, true);
        namesake.title = None;
        namesake.official = true;
        let answer = official(&server(vec![namesake]), name);
        assert_eq!(answer["found"], false, "{answer}");
        assert_eq!(answer["alternatives"][0]["domain"], domain);
    }
}

#[test]
fn official_site_package_owner_overrides_generated_framework_docs() {
    for (title, named, popularity) in [("FastAPI - Swagger UI", false, 0.1), ("FastAPI", true, 0.9)]
    {
        let mcp = scripted(move |query| {
            if query.ends_with("package") {
                return results(
                    Vec::new(),
                    vec![package_page(
                        "pypi",
                        "fastapi",
                        Some("https://fastapi.tiangolo.com/"),
                        None,
                    )],
                );
            }
            let mut generated = titled("dalicc.net", title, popularity, named);
            generated.url = "http://api.dalicc.net/docs".into();
            let mut unrelated = titled("standards.example", "Other Standards Body", 0.9, true);
            unrelated.official = true;
            results(vec![generated, unrelated], Vec::new())
        });
        let answer = official(&mcp, "FastAPI");
        assert_eq!(answer["url"], "https://fastapi.tiangolo.com/", "{answer}");
        assert_eq!(answer["confidence"], "medium");
        assert_eq!(answer["alternatives"][0]["domain"], "dalicc.net");
        assert!(answer["why"].to_string().contains("PyPI"));
    }
    let mut generated = titled("dalicc.net", "FastAPI - Swagger UI", 0.9, true);
    generated.url = "http://api.dalicc.net/docs".into();
    let answer = official(&server(vec![generated]), "FastAPI");
    assert_eq!(answer["found"], false, "{answer}");
    assert_eq!(
        answer["alternatives"][0]["url"],
        "http://api.dalicc.net/docs"
    );
}

#[test]
fn official_site_owner_article_overrides_popular_domain_namesake() {
    let mcp = scripted(|_| {
        results(
            vec![titled("life-wiki.com", "LifeWiki", 0.9, true)],
            vec![article(
                "LifeWiki",
                Some("conwaylife.com"),
                Some("https://conwaylife.com/wiki/"),
            )],
        )
    });
    let answer = official(&mcp, "LifeWiki");
    assert_eq!(answer["url"], "https://conwaylife.com/wiki/", "{answer}");
    assert_eq!(answer["confidence"], "high");
}

#[test]
fn official_site_package_namesake_does_not_verify_a_different_known_entity() {
    let mcp = scripted(|query| {
        if query.ends_with("package") {
            return results(
                Vec::new(),
                vec![package_page(
                    "pypi",
                    "vector",
                    Some("https://geometry.example/vector/"),
                    None,
                )],
            );
        }
        results(vec![titled("vector.dev", "Vector", 0.9, true)], Vec::new())
    });
    let answer = official(&mcp, "Vector");
    assert_eq!(answer["domain"], "vector.dev", "{answer}");
    assert_eq!(answer["confidence"], "low");
    assert_eq!(answer["package_home"], "https://geometry.example/vector/");
}

#[test]
fn official_site_package_cannot_resolve_an_unverified_organization_namesake() {
    for (name, domain, title, description, country, registry, home) in [
        (
            "ISO",
            "iso.org",
            "International Organization for Standardization",
            "international standards development organization",
            "CH",
            "gem",
            "http://github.com/tigrish/iso",
        ),
        (
            "CNA",
            "cna.example",
            "Federation of National Academies",
            "research coordination organization",
            "CA",
            "pypi",
            "https://code.example/cna/",
        ),
    ] {
        // Exercise an organization ranked first and behind generated docs.
        // Neither label is derived by simple initials of its supplied title.
        for org_first in [true, false] {
            let mcp = scripted(move |query| {
                if query.ends_with("package") {
                    return results(
                        Vec::new(),
                        vec![package_page(
                            registry,
                            &name.to_lowercase(),
                            Some(home),
                            None,
                        )],
                    );
                }
                let mut organization = titled(domain, title, 0.9, true);
                organization.official = true;
                organization.description = Some(description.into());
                organization.country = Some(country.into());
                organization.url = format!("https://{domain}/");
                let mut generated = titled("unrelated.example", name, 0.9, true);
                generated.url = "https://unrelated.example/docs/".into();
                let hits = if org_first {
                    vec![organization, generated]
                } else {
                    vec![generated, organization]
                };
                results(hits, Vec::new())
            });
            let reply = call(&mcp, "official_site", json!({ "name": name }));
            let answer = &reply["result"]["structuredContent"];
            assert_eq!(answer["found"], false, "{name}: {answer}");
            assert_eq!(answer["status"], "ambiguous");
            assert_eq!(answer["confidence"], "low");
            assert!(answer["url"].is_null());
            assert_eq!(answer["package_home"], home);
            let alternatives = answer["alternatives"].as_array().unwrap();
            assert!(
                alternatives.iter().any(|alt| alt["domain"] == domain),
                "{answer}"
            );
            assert!(
                alternatives.iter().any(|alt| alt["url"] == home),
                "{answer}"
            );
            let text = reply["result"]["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(home) && text.contains(domain), "{text}");
            assert!(!text.contains("Official site for"), "{text}");
        }
    }
}

#[test]
fn official_site_known_owner_namesakes_remain_ambiguous_without_a_task() {
    let mcp = scripted(|query| {
        if query.ends_with("package") {
            return results(
                Vec::new(),
                vec![package_page(
                    "pypi",
                    "vector",
                    None,
                    Some("https://vector.readthedocs.io/"),
                )],
            );
        }
        let mut company = titled(
            "vector.com",
            "Vector: Building the Intelligent Foundation | Vector",
            0.9,
            true,
        );
        company.official = true;
        company.score = 2.0;
        let mut pipeline = titled(
            "vector.dev",
            "Vector | A lightweight, ultra-fast tool for building observability pipelines",
            0.1,
            true,
        );
        pipeline.score = 0.5;
        let mut game = titled(
            "vector.game",
            "Vector — Free Online Parkour Runner Game | Nekki",
            0.1,
            false,
        );
        game.official = true;
        game.score = 0.3;
        results(vec![company, pipeline, game], Vec::new())
    });
    let answer = official(&mcp, "Vector");
    // Both company and game have owner metadata for this single name;
    // popularity cannot resolve which owner was requested.
    assert_eq!(answer["confidence"], "low", "{answer}");
    assert_eq!(answer["status"], "ambiguous");
    assert_eq!(answer["found"], false);
    assert_ne!(answer["url"], "https://vector.readthedocs.io/");
    let urls: Vec<_> = answer["alternatives"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|alt| alt["url"].as_str())
        .collect();
    for domain in ["vector.com", "vector.dev", "vector.game"] {
        assert!(urls.iter().any(|url| url.contains(domain)), "{answer}");
    }
}

#[test]
fn official_site_does_not_bind_owner_metadata_through_its_address_alone() {
    let mcp = scripted(|_| {
        results(
            vec![titled("life-wiki.com", "Free encyclopedia", 0.9, true)],
            vec![article(
                "Unrelated encyclopedia",
                Some("life-wiki.com"),
                Some("https://life-wiki.com/"),
            )],
        )
    });
    let answer = official(&mcp, "LifeWiki");
    assert_eq!(answer["found"], false, "{answer}");
    assert_eq!(answer["confidence"], "low");

    let mut owner = titled("python.org", "Python", 0.9, true);
    owner.official = true;
    owner.url = "https://python.example/docs".into();
    let answer = official(&server(vec![owner]), "Python docs");
    assert_eq!(answer["found"], false, "{answer}");
    assert_eq!(
        answer["alternatives"][0]["url"],
        "https://python.example/docs"
    );
}

#[test]
fn official_site_text_preserves_unverified_suggestions_without_owner_claims() {
    let reply = call(
        &server(vec![titled(
            "zentral-bank.eu",
            "European Central Bank",
            0.9,
            true,
        )]),
        "official_site",
        json!({ "name": "European Central Bank" }),
    );
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("could not establish an official destination"),
        "{text}"
    );
    assert!(text.contains("Other candidates: zentral-bank.eu"), "{text}");
    assert!(!text.contains("Official site for"), "{text}");

    let reply = call(
        &server(vec![titled("trychroma.com", "Chroma", 0.9, true)]),
        "official_site",
        json!({ "name": "Chroma" }),
    );
    assert_eq!(reply["result"]["structuredContent"]["found"], true);
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Unverified candidate for"), "{text}");
    assert!(!text.contains("Official site for"), "{text}");
}

#[test]
fn official_site_arbitrary_namesakes_require_disambiguation() {
    for name in ["Brindle", "Velora"] {
        for reversed in [false, true] {
            let mut museum = titled("museum.example", &format!("{name} Museum"), 0.9, true);
            museum.official = true;
            museum.score = 10.0;
            let mut laboratory =
                titled("laboratory.test", &format!("{name} Laboratory"), 0.1, false);
            laboratory.official = true;
            laboratory.score = 0.01;
            let mut hits = vec![museum, laboratory];
            if reversed {
                hits.reverse();
            }
            let mcp = server(hits);
            let ambiguous = official(&mcp, name);
            assert_eq!(ambiguous["status"], "ambiguous", "{ambiguous}");
            assert_eq!(ambiguous["confidence"], "low");
            assert_eq!(ambiguous["found"], false);
            assert!(ambiguous["url"].is_null());
            assert_eq!(ambiguous["alternatives"].as_array().unwrap().len(), 2);
            let qualified = official(&mcp, &format!("{name} Museum"));
            assert_eq!(qualified["found"], true, "{qualified}");
            assert_eq!(qualified["domain"], "museum.example");
            assert_ne!(qualified["status"], "ambiguous");
        }
    }
}

#[test]
fn official_site_technical_qualifiers_distinguish_arbitrary_acronym_owners() {
    for (acronym, expansion) in [
        ("QRS", "Quantum Research Society"),
        ("ABC", "Assembly of Botanical Curators"),
    ] {
        for qualifier in [
            "API developer",
            "APIs developers",
            "API reference",
            "spec",
            "specification",
        ] {
            let name = format!("{acronym} {qualifier}");
            assert_eq!(identity::Intent::of(&name).entity, name.to_lowercase());
            let mut namesake = titled("namesake.example", expansion, 0.9, true);
            namesake.official = true;
            let answer = official(&server(vec![namesake.clone()]), &name);
            assert_eq!(answer["found"], false, "{name}: {answer}");
            assert_eq!(answer["confidence"], "low");
            assert!(answer["url"].is_null());

            let mut resource = titled("owner.example", &name, 0.1, false);
            resource.official = true;
            resource.score = 0.1;
            let answer = official(&server(vec![namesake, resource]), &name);
            assert_eq!(answer["found"], true, "{name}: {answer}");
            assert_eq!(answer["domain"], "owner.example");
            let typed = official(
                &server(vec![titled("owner.example", "Owner", 0.1, false)]),
                "owner.example",
            );
            assert_eq!(typed["found"], true, "{typed}");
        }
    }
}

#[test]
fn official_site_cached_docs_require_indexed_owner_or_complete_destination_evidence() {
    for indexed in 0..4 {
        let mcp = scripted(move |query| {
            if query.ends_with("package") {
                return results(
                    Vec::new(),
                    vec![package_page(
                        "pypi",
                        "brindle",
                        Some("https://brindle.example/"),
                        Some("https://brindle.docs.test/"),
                    )],
                );
            }
            let hits = match indexed {
                0 => Vec::new(),
                1 => vec![titled("unrelated.example", "Brindle", 0.1, true)],
                2 => vec![titled("brindle.example", "Brindle", 0.1, true)],
                _ => {
                    let mut docs = titled("docs.test", "Brindle", 0.1, true);
                    docs.url = "https://brindle.docs.test/".into();
                    docs.official = true;
                    vec![docs]
                }
            };
            results(hits, Vec::new())
        });
        let answer = official(&mcp, "Brindle docs");
        if indexed < 2 {
            assert_eq!(answer["found"], false, "{answer}");
            assert_eq!(answer["confidence"], "low");
            assert!(answer["url"].is_null());
            assert_eq!(answer["package_home"], "https://brindle.docs.test/");
        } else {
            assert_eq!(answer["found"], true, "{answer}");
            assert_eq!(answer["url"], "https://brindle.docs.test/");
        }
    }
}

#[test]
fn official_site_article_settles_owners_that_share_a_name() {
    let mcp = scripted(|_| {
        let mut company = titled(
            "microsoft.com",
            "Microsoft – AI, Cloud, Productivity, Computing, Gaming & Apps",
            0.9,
            true,
        );
        company.official = true;
        let mut registry = titled("nic.microsoft", "Microsoft", 0.2, true);
        registry.official = true;
        results(
            vec![company, registry],
            vec![article("Microsoft", Some("microsoft.com"), None)],
        )
    });
    let answer = official(&mcp, "Microsoft");
    assert_eq!(answer["found"], true, "{answer}");
    assert_eq!(answer["domain"], "microsoft.com");
}

#[test]
fn official_site_owner_read_far_more_than_its_namesakes_is_chosen() {
    for (town_demand, found) in [(0.3, true), (0.75, false)] {
        let mcp = scripted(move |_| {
            let mut bank = titled(
                "chase.com",
                "Chase Bank: Credit Cards, Mortgages, Banking, Auto | Chase.com",
                0.9,
                true,
            );
            bank.official = true;
            bank.demand = Some(0.8);
            let mut town = titled("chase.example", "Chase, British Columbia", 0.1, true);
            town.official = true;
            town.demand = Some(town_demand);
            results(vec![bank, town], Vec::new())
        });
        let answer = official(&mcp, "Chase login");
        assert_eq!(answer["found"], found, "{answer}");
        if found {
            assert_eq!(answer["domain"], "chase.com");
            assert_eq!(answer["confidence"], "medium");
        } else {
            assert_eq!(answer["status"], "ambiguous");
        }
    }
}

#[test]
fn official_site_names_the_owner_of_its_first_words_when_the_site_shows_the_rest() {
    let mcp = scripted(|query| {
        let mut brand = titled("brita.com", "BRITA", 0.9, true);
        brand.official = true;
        brand.description = Some("Water filters and filtered water bottles.".into());
        let pages = if query == "Brita" {
            vec![article("Brita", Some("brita.com"), None)]
        } else {
            Vec::new()
        };
        results(vec![brand], pages)
    });
    let answer = official(&mcp, "Brita water filter");
    assert_eq!(answer["found"], true, "{answer}");
    assert_eq!(answer["domain"], "brita.com");
    assert_eq!(answer["confidence"], "low");
    assert!(
        answer["why"][0].as_str().unwrap().contains("\"Brita\""),
        "{answer}"
    );

    let answer = official(&mcp, "Brita stock price");
    assert!(
        !answer["why"].to_string().contains("first word"),
        "{answer}"
    );
}
