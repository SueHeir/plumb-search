//! Listing pages with the sites a search found.

use super::*;

#[test]
fn podcasts_come_after_the_site_and_the_article_unless_asked_for() {
    let sites = [site("comedycentral.com", false), site("amc.com", false)];
    let podcast = |title: &str| {
        let mut hit = found(title, None, true, 0.95);
        hit.page.set = PODCASTS_SET.into();
        hit.learned = Some(LearnedPlace::Before("comedycentral.com".into()));
        hit
    };
    let article = found("Better Call Saul", None, true, 0.9);
    let pages = vec![
        podcast("Better Call Saul Insider"),
        podcast("Better Call Saul Podcast"),
        article,
    ];
    let placed = place_pages("better call saul", &sites, pages.clone());
    let alone: Vec<(&str, usize)> = placed
        .iter()
        .filter(|p| p.under.is_none())
        .map(|p| (p.hit.page.title.as_str(), p.at))
        .collect();
    let article_at = alone[0].1;
    assert_eq!(alone[0].0, "Better Call Saul");
    assert!(alone[1..].iter().all(|&(_, at)| at >= article_at.max(1)));
    // Asked for, they stay where they were put.
    let placed = place_pages("better call saul podcast", &sites, pages);
    assert!(placed
        .iter()
        .any(|p| p.hit.page.set == PODCASTS_SET && p.at == 0));
}

#[test]
fn pages_with_the_words_never_crowd_out_the_page_named() {
    let mut pages: Vec<Page> = (0..2 * CANDIDATES)
        .map(|i| {
            page(
                &format!("How do I zoom Google Maps to marker {i}"),
                900_000,
                &[],
            )
        })
        .collect();
    pages.push(page("Google Maps", 22_902, &["Gmaps"]));
    let (_dir, s) = searcher(&pages);
    let hits = s.search("google maps", 10).unwrap();
    assert_eq!(titles(&hits)[0], "Google Maps");
    assert!(hits[0].named);
}

#[test]
fn main_articles_beat_their_namesakes() {
    let (_dir, s) = searcher(&[
        page("Albert Einstein", 400_000, &[]),
        page("Albert Einstein (album)", 900, &[]),
        page("Wolfgang Amadeus Mozart", 200_000, &["Mozart"]),
        page("Mozart (film)", 3_000, &[]),
        page("Mozart!", 5_000, &[]),
    ]);
    let hits = s.search("albert einstein", 5).unwrap();
    assert_eq!(titles(&hits)[0], "Albert Einstein");
    let hits = s.search("mozart", 5).unwrap();
    assert_eq!(titles(&hits)[0], "Wolfgang Amadeus Mozart");
    // Once the main article is under its site, a namesake waits until
    // after the sites.
    let sites = [
        known_site("toureiffel.paris", false, 0.5),
        site("a.com", false),
        site("b.com", false),
        site("c.com", false),
    ];
    let mut namesake = found("Eiffel Tower (Six Flags)", None, true, 0.6);
    let placed = place_pages(
        "eiffel tower",
        &sites,
        vec![
            found("Eiffel Tower", Some("toureiffel.paris"), true, 0.9),
            namesake.clone(),
        ],
    );
    assert_eq!(placed[0].under.as_deref(), Some("toureiffel.paris"));
    assert_eq!(placed[1].at, PARTIAL_AFTER);
    // Whatever the learned ranking says: "taylor swift" listed the album
    // "Taylor Swift" second.
    namesake.learned = Some(LearnedPlace::Before("a.com".into()));
    let placed = place_pages(
        "eiffel tower",
        &sites,
        vec![
            found("Eiffel Tower", Some("toureiffel.paris"), true, 0.9),
            namesake,
        ],
    );
    assert_eq!(placed[1].at, PARTIAL_AFTER);
    // Of two namesakes on their own, the better one stays.
    let mut placed = vec![
        PlacedPage {
            at: 0,
            under: None,
            hit: found("The Bear (1984 film)", None, true, 0.6),
        },
        PlacedPage {
            at: 1,
            under: None,
            hit: found("The Bear (1988 film)", None, true, 0.7),
        },
    ];
    keep_page_rules("the bear", &sites, &mut placed);
    let at: Vec<usize> = placed.iter().map(|p| p.at).collect();
    assert_eq!(at, [PARTIAL_AFTER, 1]);
}

#[test]
fn pages_about_a_listed_site_go_under_it() {
    let sites = [site("python.org", true), site("pythonanywhere.com", false)];
    let placed = place_pages(
        "",
        &sites,
        vec![
            found(
                "Python (programming language)",
                Some("python.org"),
                true,
                0.9,
            ),
            found("Python (genus)", None, true, 0.7),
            found("Monty Python", None, false, 0.5),
        ],
    );
    assert_eq!(placed.len(), 2);
    assert_eq!(placed[0].under.as_deref(), Some("python.org"));
    // The site is named, so one page, and the namesake of the page
    // under it only after the sites.
    assert_eq!(placed[1].hit.page.title, "Python (genus)");
    assert_eq!((placed[1].under.as_deref(), placed[1].at), (None, 2));
}

#[test]
fn the_site_of_the_page_the_query_names_goes_first() {
    let mut official = known_site("youtube.com", false, 1.0);
    official.official = true;
    let mut sites = vec![
        site("youtube.de", false),
        site("youtube.nl", false),
        official,
        site("a.com", false),
    ];
    let mut music = found("YouTube Music", Some("youtube.com"), true, 0.9);
    music.page.item = Some("Q28404534".into());
    lift_named_sites(&mut sites, &[music.clone()]);
    let order: Vec<&str> = sites.iter().map(|s| s.domain.as_str()).collect();
    assert_eq!(order, ["youtube.com", "youtube.de", "youtube.nl", "a.com"]);
    // Not for a page named only in part, nor a site Wikidata does
    // not call official.
    let mut sites = vec![site("youtube.de", false), site("youtube.com", false)];
    lift_named_sites(&mut sites, &[music.clone()]);
    assert_eq!(sites[0].domain, "youtube.de");
    let mut google = known_site("google.com", true, 1.0);
    google.official = true;
    let mut about = known_site("about.google", false, 0.6);
    about.official = true;
    let mut sites = vec![google, about];
    let mut company = found("Google", Some("about.google"), true, 0.9);
    company.page.item = Some("Q95".into());
    lift_named_sites(&mut sites, &[company]);
    assert_eq!(sites[0].domain, "google.com");
    let toyota = known_site("toyota.com", true, 0.9);
    let mut global = known_site("global.toyota", false, 0.6);
    global.official = true;
    let mut sites = vec![toyota, global];
    let mut maker = found("Toyota", Some("global.toyota"), true, 0.9);
    maker.page.item = Some("Q53268".into());
    lift_named_sites(&mut sites, &[maker]);
    assert_eq!(sites[0].domain, "toyota.com");
    // Far less well known than the official site, which both are named
    // by the query: the official site goes first.
    let us = known_site("aliexpress.us", true, 0.41);
    let mut com = known_site("aliexpress.com", true, 0.75);
    com.official = true;
    let mut sites = vec![us, com];
    let mut shop = found("AliExpress", Some("aliexpress.com"), true, 0.9);
    shop.page.item = Some("Q2647593".into());
    lift_named_sites(&mut sites, &[shop]);
    assert_eq!(sites[0].domain, "aliexpress.com");
    // Less well known than global.toyota, but scored higher, and both
    // are named by "toyota".
    let toyota = known_site("toyota.com", true, 0.47);
    let mut global = known_site("global.toyota", true, 0.62);
    global.official = true;
    let mut sites = vec![toyota, global];
    let mut maker = found("Toyota", Some("global.toyota"), true, 0.9);
    maker.page.item = Some("Q53268".into());
    lift_named_sites(&mut sites, &[maker]);
    assert_eq!(sites[0].domain, "toyota.com");
    music.named = false;
    let mut sites = vec![
        site("youtube.de", false),
        known_site("youtube.com", false, 1.0),
    ];
    sites[1].official = true;
    lift_named_sites(&mut sites, &[music]);
    assert_eq!(sites[0].domain, "youtube.de");
}

#[test]
fn sites_with_some_words_of_a_named_article_are_left_out() {
    let mut sites = vec![
        site("toystory.disney.com", false),
        site("rawstory.com", false),
        site("toys.com", false),
    ];
    sites[1].missing_words = true;
    sites[2].missing_words = true;
    let mut film = found("Toy Story", Some("toys.com"), true, 0.9);
    film.page.item = Some("Q171048".into());
    // Not named in full: all stay.
    film.named = false;
    assert_eq!(drop_namesakes_of_words(&mut sites, &[film.clone()]), 0);
    film.named = true;
    assert_eq!(drop_namesakes_of_words(&mut sites, &[film]), 1);
    let order: Vec<&str> = sites.iter().map(|s| s.domain.as_str()).collect();
    // The article's own site stays.
    assert_eq!(order, ["toystory.disney.com", "toys.com"]);
}

#[test]
fn sites_only_a_later_word_of_a_named_article_finds_are_left_out() {
    // Each lacks a word of "airpods pro": (domain, words of it its name
    // covers from the first, closeness in meaning).
    let evidence = |domain: &str, partial_name_words: usize, closeness: Option<f32>| {
        let mut hit = known_site(domain, false, 0.7);
        hit.missing_words = true;
        hit.query_evidence = Some(crate::CandidateEvidence {
            domain: domain.into(),
            lexical_score: 30.0,
            semantic_closeness: closeness,
            query_coverage: 0.5,
            substantive_coverage: 0.5,
            remaining_coverage: 0.5,
            named_subject_words: 0,
            named_subject_domain: None,
            named_subject_reason: None,
            subject_coverage: None,
            subject_remaining_coverage: None,
            full_name: false,
            typed_domain: false,
            partial_name_words,
            relevance_tier: 0,
            source_quality: None,
        });
        hit
    };
    let mut sites = vec![
        site("airpodspro.ru", true),
        // Named by "pro", an alias of the Public Record Office.
        evidence("nationalarchives.gov.uk", 0, Some(0.0)),
        evidence("airpods.example", 1, Some(0.1)),
        evidence("earbuds.example", 0, Some(0.6)),
        evidence("novector.example", 0, None),
        evidence("pro.example", 0, Some(0.2)),
    ];
    let mut earbuds = found("AirPods Pro", Some("pro.example"), true, 0.9);
    earbuds.page.item = Some("Q72725578".into());
    let floor = crate::RankConfig::default().meaning_only_relevance;
    // Not named in full, or no floor: all stay.
    earbuds.named = false;
    assert_eq!(drop_far_namesakes(&mut sites, &[earbuds.clone()], floor), 0);
    earbuds.named = true;
    assert_eq!(drop_far_namesakes(&mut sites, &[earbuds.clone()], None), 0);
    assert_eq!(drop_far_namesakes(&mut sites, &[earbuds], floor), 1);
    let order: Vec<&str> = sites.iter().map(|s| s.domain.as_str()).collect();
    // The site the name starts with, one near in meaning, one with no
    // vector and the article's own site stay.
    assert_eq!(
        order,
        [
            "airpodspro.ru",
            "airpods.example",
            "earbuds.example",
            "novector.example",
            "pro.example"
        ]
    );
}

#[test]
fn sites_only_a_first_name_finds_are_left_out_for_people() {
    // Each lacks a word of "ted kaczynski" and is named by "ted".
    let evidence = |domain: &str, closeness: f32| {
        let mut hit = known_site(domain, false, 0.7);
        hit.missing_words = true;
        hit.query_evidence = Some(crate::CandidateEvidence {
            domain: domain.into(),
            lexical_score: 30.0,
            semantic_closeness: Some(closeness),
            query_coverage: 0.5,
            substantive_coverage: 0.5,
            remaining_coverage: 0.0,
            named_subject_words: 0,
            named_subject_domain: None,
            named_subject_reason: None,
            subject_coverage: None,
            subject_remaining_coverage: None,
            full_name: false,
            typed_domain: false,
            partial_name_words: 1,
            relevance_tier: 0,
            source_quality: None,
        });
        hit
    };
    let sites = vec![
        evidence("ted.com", 0.0),
        evidence("tedfans.example", 0.6),
        evidence("own.example", 0.1),
    ];
    let mut article = found("Ted Kaczynski", Some("own.example"), true, 0.9);
    article.page.item = Some("Q222134".into());
    let floor = crate::RankConfig::default().meaning_only_relevance;
    // Not a person: the site the name starts with stays.
    assert_eq!(
        drop_far_namesakes(&mut sites.clone(), &[article.clone()], floor),
        0
    );
    article.page.facts = vec![plumb_core::facts::Fact {
        kind: plumb_core::facts::FactKind::Born,
        value: "1942-05-22".into(),
    }];
    let mut left = sites.clone();
    assert_eq!(drop_far_namesakes(&mut left, &[article], floor), 1);
    let order: Vec<&str> = left.iter().map(|s| s.domain.as_str()).collect();
    // One near in meaning and the person's own site stay.
    assert_eq!(order, ["tedfans.example", "own.example"]);
}

#[test]
fn the_site_of_the_article_named_is_added_when_missing() {
    let mut sites = vec![
        site("comedycentral.com", false),
        site("callofduty.com", false),
        site("walmart.com", false),
    ];
    for (i, s) in sites.iter_mut().enumerate() {
        s.score = 1.0 - i as f32 * 0.1;
    }
    let mut show = found("Better Call Saul", Some("amc.com"), true, 0.9);
    show.page.item = Some("Q3010697".into());
    let amc = |domain: &str| {
        let mut hit = known_site(domain, false, 0.7);
        hit.official = domain == "amc.com";
        Some(hit)
    };
    assert!(add_named_site(&mut sites, &[show.clone()], amc));
    let order: Vec<&str> = sites.iter().map(|s| s.domain.as_str()).collect();
    assert_eq!(
        order,
        [
            "comedycentral.com",
            "callofduty.com",
            "walmart.com",
            "amc.com"
        ]
    );
    assert!(sites[3].score < sites[2].score);
    // Once is enough, and a site the index does not hold is not added.
    assert!(!add_named_site(&mut sites, &[show.clone()], amc));
    show.page.site = Some("fans.example".into());
    assert!(!add_named_site(&mut sites, &[show], |_| None));
}

#[test]
fn the_homepage_of_the_repository_the_query_names_goes_first() {
    let mut python = known_site("python.org", false, 0.9);
    python.official = true;
    let mut sites = vec![
        python.clone(),
        site("realpython.com", false),
        known_site("awesome-python.com", true, 0.2),
    ];
    let mut repo = found(
        "vinta/awesome-python",
        Some("awesome-python.com"),
        true,
        0.8,
    );
    repo.page.set = GITHUB_SET.into();
    lift_named_sites(&mut sites, &[repo.clone()]);
    assert_eq!(sites[0].domain, "awesome-python.com");
    let placed = place_pages("awesome python", &sites, vec![repo.clone()]);
    assert_eq!(placed[0].under.as_deref(), Some("awesome-python.com"));
    // Not a homepage the query does not name.
    let mut sites = vec![python, known_site("awesomelists.dev", false, 0.2)];
    repo.page.site = Some("awesomelists.dev".into());
    lift_named_sites(&mut sites, &[repo]);
    assert_eq!(sites[0].domain, "python.org");
}

#[test]
fn the_page_the_query_names_goes_under_its_site() {
    let sites = [site("google.com", false), site("a.com", false)];
    let placed = place_pages(
        "google maps",
        &sites,
        vec![
            found("Google", Some("google.com"), false, 0.9),
            found("Google Maps", Some("google.com"), true, 0.8),
            found("Google Search", Some("google.com"), false, 0.7),
        ],
    );
    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0].under.as_deref(), Some("google.com"));
    assert_eq!(placed[0].hit.page.title, "Google Maps");
}

#[test]
fn items_without_articles_only_go_under_their_site() {
    let item = |site: &str| {
        let mut hit = found("Linus Tech Tips", Some(site), true, 0.9);
        hit.page.set = WIKIDATA_SET.into();
        hit
    };
    let sites = [site("linustechtips.com", false), site("a.com", false)];
    let placed = place_pages("", &sites, vec![item("linustechtips.com")]);
    assert_eq!(placed[0].under.as_deref(), Some("linustechtips.com"));
    assert!(place_pages("", &sites, vec![item("elsewhere.com")]).is_empty());
}

#[test]
fn named_pages_lead_when_no_site_is_named() {
    let sites = [
        known_site("curie.fr", false, 0.4),
        site("a.com", false),
        site("b.com", false),
    ];
    let placed = place_pages(
        "",
        &sites,
        vec![
            found("Marie Curie", None, true, 0.9),
            found("Pierre Curie", None, false, 0.35),
            found("Curie (unit)", None, false, 0.2),
        ],
    );
    let at: Vec<(&str, usize)> = placed
        .iter()
        .map(|p| (p.hit.page.title.as_str(), p.at))
        .collect();
    assert_eq!(at, [("Marie Curie", 0), ("Pierre Curie", 3)]);
    assert!(place_pages("", &[], vec![found("Marie Curie", None, true, 0.9)])[0].at == 0);
}

#[test]
fn sites_the_page_may_be_about_stay_first() {
    let described = |title: &str, description: &str, score: f32| {
        let mut hit = found(title, None, true, score);
        hit.page.description = Some(description.into());
        hit
    };
    // A much read page comes before a little known namesake...
    let placed = place_pages(
        "",
        &[known_site("mariecurie.org", true, 0.2)],
        vec![described("Marie Curie", "Polish-French physicist", 0.8)],
    );
    assert_eq!(placed[0].at, 0);
    // ...but after the website of a company or product it names, even
    // when the page found first is about something else.
    let placed = place_pages(
        "",
        &[known_site("robinhood.com", true, 0.47)],
        vec![
            described("Robin Hood", "Legendary English outlaw", 0.9),
            described(
                "Robinhood Markets",
                "American financial services company",
                0.6,
            ),
        ],
    );
    assert_eq!(placed[0].at, 1);
    let placed = place_pages(
        "",
        &[known_site("cvs.com", false, 0.43)],
        vec![described(
            "CVS Pharmacy",
            "American retail pharmacy chain",
            0.6,
        )],
    );
    assert_eq!(placed[0].at, 1);
    // A one-word query spelling the site, not the page ("robinhood").
    let placed = place_pages(
        "robinhood",
        &[known_site("robinhood.com", true, 0.47)],
        vec![described(
            "Robin Hood",
            "Heroic outlaw in English folklore",
            0.9,
        )],
    );
    assert_eq!(placed[0].at, 1);
    let placed = place_pages(
        "robin hood",
        &[known_site("robinhood.com", true, 0.47)],
        vec![described(
            "Robin Hood",
            "Heroic outlaw in English folklore",
            0.9,
        )],
    );
    assert_eq!(placed[0].at, 0);
    // A government site called after the page.
    let placed = place_pages(
        "fafsa",
        &[known_site("fafsa.gov", true, 0.34)],
        vec![described(
            "FAFSA",
            "Form to determine eligibility for US student aid",
            0.7,
        )],
    );
    assert_eq!(placed[0].at, 1);
    // Not one only the page's first word spells.
    let placed = place_pages(
        "ada lovelace",
        &[known_site("ada.gov", false, 0.48)],
        vec![described(
            "Ada Lovelace",
            "English mathematician (1815-1852)",
            0.9,
        )],
    );
    assert_eq!(placed[0].at, 0);
    // ...and after an official website or a well known site the query
    // names too...
    let mut official = known_site("example.org", true, 0.3);
    official.official = true;
    let known = known_site("example.com", true, 0.9);
    for site in [official, known] {
        let placed = place_pages("", &[site], vec![found("Example", None, true, 0.8)]);
        assert_eq!(placed[0].at, 1);
    }
    // ...but not after one that only shares a word with the query.
    let mut tesla = known_site("tesla.com", false, 0.7);
    tesla.official = true;
    tesla.text_score = 0.37;
    let placed = place_pages(
        "nikola tesla",
        &[tesla],
        vec![found("Nikola Tesla", None, true, 0.8)],
    );
    assert_eq!(placed[0].at, 0);
    // A site of the kind searched for matches it in full and stays.
    let mut nasa = known_site("nasa.gov", false, 0.8);
    nasa.official = true;
    let placed = place_pages(
        "space agency",
        &[nasa],
        vec![found("Space agency", None, true, 0.6)],
    );
    assert_eq!(placed[0].at, 1);
}

#[test]
fn organizations_by_description() {
    for yes in [
        "Open-source software framework",
        "American auto insurance company",
        "Credit card brand",
        "Online food ordering services",
        "Credit card",
        "Specification for machine-readable interface files",
    ] {
        assert!(describes_an_organization(Some(yes)), "{yes}");
    }
    for no in [
        "Polish-French physicist (1867–1934)",
        "Region of spacetime",
        "1889 painting by Vincent van Gogh",
    ] {
        assert!(!describes_an_organization(Some(no)), "{no}");
    }
    assert!(!describes_an_organization(None));
}

#[test]
fn sites_called_after_pages() {
    assert!(site_is_titled("cvs.com", "CVS Pharmacy"));
    assert!(site_is_titled("capitalone.com", "Capital One"));
    assert!(site_is_titled("turbotax.intuit.com", "TurboTax"));
    assert!(site_is_titled("robinhood.com", "Robin Hood"));
    assert!(site_is_titled("tauri.app", "Tauri (software framework)"));
    assert!(!site_is_titled("leonardodavinci.net", "Marie Curie"));
    assert!(!site_is_titled("curie.fr", "Marie Curie"));
    assert!(site_is_titled(
        "navyfederal.org",
        "Navy Federal Credit Union"
    ));
    assert!(!site_is_titled("navyfed.org", "Navy Federal Credit Union"));
}
