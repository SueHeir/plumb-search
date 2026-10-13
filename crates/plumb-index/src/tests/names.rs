//! What a search finds a site by: its page terms, names, titles, Wikidata
//! descriptions and redirects.

use super::*;

#[test]
fn sites_on_subdomains_are_found_by_name() {
    // Hacker News takes Y Combinator's ranks (see plumb-ingest), so it
    // beats a little-known site named after it.
    let records = [
        site(
            "news.ycombinator.com",
            Some("Hacker News"),
            None,
            &["Hacker News"],
            &[],
            popular(900, 50),
        ),
        site(
            "hackernews.cc",
            Some("HackerNews - threat intelligence"),
            None,
            &[],
            &[],
            obscure(400_000, 5),
        ),
        site(
            "ycombinator.com",
            Some("Y Combinator"),
            None,
            &["Y Combinator"],
            &[],
            popular(900, 50),
        ),
    ];
    let (_dir, searcher) = build(&records);
    assert_eq!(top(&searcher, "hacker news"), "news.ycombinator.com");
    assert_eq!(
        top(&searcher, "news.ycombinator.com"),
        "news.ycombinator.com"
    );
    assert_eq!(top(&searcher, "y combinator"), "ycombinator.com");
}

#[test]
fn sites_without_a_title_go_by_their_name() {
    let records = [
        site("gmail.com", None, None, &["Gmail"], &[], popular(30, 100)),
        site("gmail.ru", None, None, &[], &[], obscure(900_000, 1)),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search("gmail", 10).unwrap();
    assert_eq!(domains(&hits), ["gmail.com"]);
    assert_eq!(hits[0].title.as_deref(), Some("Gmail"));
    let hits = searcher.search("gmail.ru", 10).unwrap();
    assert_eq!(hits[0].domain, "gmail.ru");
    assert_eq!(hits[0].title, None);
}

#[test]
fn little_known_sites_without_a_title_come_after_titled_ones() {
    let records = [
        site(
            "netflix.com",
            Some("Netflix"),
            None,
            &["Netflix"],
            &[],
            popular(20, 500),
        ),
        site("netflix.net", None, None, &[], &[], obscure(200_000, 3)),
        site("netflix.info", None, None, &[], &[], obscure(250_000, 2)),
        site(
            "netflixfans.org",
            Some("Netflix fans: what to watch on Netflix"),
            None,
            &[],
            &[],
            obscure(900_000, 1),
        ),
    ];
    let (_dir, searcher) = build(&records);
    // The bare copies of netflix.com are left out altogether.
    let hits = searcher.search_with("netflix", 10, &keep_all()).unwrap();
    assert_eq!(domains(&hits), ["netflix.com", "netflixfans.org"]);
    // Typed, a bare one is found.
    let hits = searcher
        .search_with("netflix.net", 10, &keep_all())
        .unwrap();
    assert_eq!(hits[0].domain, "netflix.net");
}

#[test]
fn a_leading_www_names_no_site() {
    let records = [
        site(
            "ikea.com",
            Some("IKEA"),
            None,
            &["IKEA"],
            &[],
            popular(300, 20_000),
        ),
        // On a public suffix, so its domain label is "www".
        site(
            "www.gov.uk",
            Some("Welcome to GOV.UK"),
            None,
            &[],
            &[],
            popular(100, 90_000),
        ),
        site(
            "www.nhs.uk",
            Some("NHS website for England"),
            None,
            &[],
            &[],
            popular(400, 40_000),
        ),
        site(
            "prsa.org",
            Some("Public Relations Society of America"),
            None,
            &[],
            &[("www", 50)],
            popular(20_000, 3_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    for query in ["www.ikea", "www ikea", "WWW.IKEA.COM"] {
        let hits = searcher.search_with(query, 10, &keep_all()).unwrap();
        assert_eq!(domains(&hits), ["ikea.com"], "{query}");
    }
    // An address whose site needs its "www" keeps it.
    assert_eq!(top(&searcher, "www.gov.uk"), "www.gov.uk");
    assert_eq!(crate::names::without_www("www.gov.uk"), "www.gov.uk");
    assert_eq!(crate::names::without_www("www"), "www");
    assert_eq!(crate::names::without_www("wwwikea.com"), "wwwikea.com");
}

#[test]
fn a_little_known_namesake_of_the_words_leaves_the_rest_listed() {
    let records = [
        site(
            "vacationrentals.com",
            Some("Vacation Rentals"),
            None,
            &[],
            &[],
            obscure(150_000, 20),
        ),
        site(
            "airbnb.com",
            Some("Airbnb: holiday rentals, cabins, beach houses"),
            None,
            &["Airbnb"],
            &[],
            popular(150, 5_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search("vacation rentals", 10).unwrap();
    assert_eq!(hits[0].domain, "vacationrentals.com");
    assert!(hits[0].link_score < WELL_KNOWN_LINK_SCORE);
    assert!(
        domains(&hits).contains(&"airbnb.com"),
        "{:?}",
        domains(&hits)
    );
}

#[test]
fn asking_and_wanting_words_name_no_site() {
    let records = [
        site(
            "who.int",
            Some("World Health Organization (WHO)"),
            None,
            &["WHO"],
            &[("who", 300)],
            popular(200, 5_000),
        ),
        site(
            "in.gov",
            Some("Austin Indiana"),
            None,
            &[],
            &[],
            popular(900, 2_000),
        ),
        site(
            "website.ws",
            Some("WebSite.ws"),
            None,
            &[],
            &[],
            obscure(20_000, 300),
        ),
        site(
            "ups.com",
            Some("UPS"),
            Some("Shipping and tracking."),
            &["UPS"],
            &[("ups", 300)],
            popular(300, 3_000),
        ),
        site(
            "hamletguide.org",
            Some("Hamlet study guide"),
            Some("Who wrote Hamlet, and when."),
            &[],
            &[],
            obscure(300_000, 3),
        ),
        site(
            "amtrak.com",
            Some("Amtrak"),
            None,
            &["Amtrak"],
            &[("amtrak", 200)],
            popular(800, 2_000),
        ),
    ];
    let mut records = records.to_vec();
    records.push(site(
        "rid.it",
        Some("RID"),
        None,
        &[],
        &[],
        popular(5_000, 900),
    ));
    records.push(site(
        "aphidfacts.org",
        Some("Aphids"),
        Some("How to get rid of aphids in the garden."),
        &[],
        &[],
        obscure(300_000, 3),
    ));
    records.push(site(
        "earthhour.org",
        Some("Earth Hour"),
        None,
        &["Earth Hour"],
        &[("earth hour", 50)],
        popular(3_000, 900),
    ));
    records.push(site(
        "rei.com",
        Some("REI Co-op"),
        None,
        &["REI"],
        &[("rei", 100)],
        popular(900, 1_000),
    ));
    let (_dir, searcher) = build(&records);
    for (query, wrong) in [
        ("rei hours", "earthhour.org"),
        ("who wrote hamlet", "who.int"),
        ("ups sign in", "in.gov"),
        ("amtrak website", "website.ws"),
    ] {
        let hits = searcher.search(query, 10).unwrap();
        assert!(
            !domains(&hits).contains(&wrong),
            "{query:?}: {:?}",
            domains(&hits)
        );
    }
    assert_eq!(top(&searcher, "who wrote hamlet"), "hamletguide.org");
    // A question names nothing: rid.it, popular and with "RID" in its
    // title, has one word of it, but not as its name.
    let rid = |cfg: &RankConfig| {
        searcher
            .search_with("how to get rid of aphids", 10, cfg)
            .unwrap()
            .into_iter()
            .find(|hit| hit.domain == "rid.it")
            .map_or(0.0, |hit| hit.score)
    };
    let naming = RankConfig {
        questions_name_nothing: false,
        ..RankConfig::default()
    };
    assert!(rid(&RankConfig::default()) < rid(&naming));
    // Alone, they are names.
    assert_eq!(top(&searcher, "who"), "who.int");
}

#[test]
fn sites_missing_a_main_word_are_marked() {
    let records = [
        site(
            "rawstory.com",
            Some("Raw Story"),
            Some("News and story."),
            &[],
            &[],
            popular(3_000, 900),
        ),
        site(
            "toystoryfans.net",
            Some("Toy Story fans"),
            None,
            &[],
            &[],
            obscure(90_000, 3),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search("toy story", 10).unwrap();
    let missing = |domain: &str| {
        hits.iter()
            .find(|h| h.domain == domain)
            .unwrap()
            .missing_words
    };
    assert!(missing("rawstory.com"));
    assert!(!missing("toystoryfans.net"));
    // Filler words do not count: "my" is not a main word.
    let hits = searcher.search("my toy story", 10).unwrap();
    assert!(
        !hits
            .iter()
            .find(|h| h.domain == "toystoryfans.net")
            .unwrap()
            .missing_words
    );
}

#[test]
fn sites_with_one_word_of_a_known_name_are_left_out() {
    let records = [
        site(
            "usbank.com",
            None,
            None,
            &["U.S. Bank"],
            &[("us bank", 40)],
            obscure(20_000, 300),
        ),
        site(
            "worldbank.org",
            Some("World Bank Group"),
            Some("The World Bank works to end poverty."),
            &["World Bank"],
            &[("world bank", 300)],
            popular(300, 4_000),
        ),
        site(
            "vogue.com",
            Some("Vogue US"),
            Some("Fashion news from us."),
            &["Vogue"],
            &[("vogue", 300)],
            popular(400, 4_000),
        ),
        site(
            "us.es",
            Some("Universidad de Sevilla"),
            None,
            &[],
            &[],
            obscure(9_000, 200),
        ),
        site(
            "usbankreviews.net",
            Some("US Bank reviews"),
            Some("Reviews of US Bank branches."),
            &[],
            &[],
            obscure(400_000, 2),
        ),
        site(
            "youtube.com",
            Some("YouTube"),
            Some("Enjoy the videos you love."),
            &["YouTube"],
            &[("youtube", 300)],
            popular(9, 6_000),
        ),
        site(
            "youtubemusic.net",
            Some("YouTube Music"),
            None,
            &["YouTube Music"],
            &[("youtube music", 30)],
            obscure(50_000, 100),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search("us bank", 10).unwrap();
    assert_eq!(domains(&hits), ["usbank.com", "usbankreviews.net"]);
    let keep = RankConfig {
        named_needs_all_words: false,
        ..RankConfig::default()
    };
    let hits = searcher.search_with("us bank", 10, &keep).unwrap();
    assert!(
        domains(&hits).contains(&"worldbank.org"),
        "{:?}",
        domains(&hits)
    );
    // A site known by the query's first words stays.
    let hits = searcher.search("youtube music", 10).unwrap();
    assert!(
        domains(&hits).contains(&"youtube.com"),
        "{:?}",
        domains(&hits)
    );
}

#[test]
fn a_site_said_what_it_is_keeps_its_place_without_a_title() {
    let records = [
        site(
            "vanguard.edu",
            Some("Vanguard University"),
            None,
            &["Vanguard University"],
            &[],
            popular(40_000, 250),
        ),
        // Never crawled, but Wikidata says what it is.
        site(
            "vanguard.com",
            None,
            Some("American investment management company"),
            &[],
            &[],
            ranked(30_000, 300),
        ),
        site(
            "vanguardmil.com",
            Some("Vanguard Industries military insignia"),
            None,
            &[],
            &[],
            obscure(200_000, 10),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let hits = searcher.search_with("vanguard", 10, &keep_all()).unwrap();
    let at = |domain: &str| domains(&hits).iter().position(|d| *d == domain);
    assert!(
        at("vanguard.com") < at("vanguardmil.com"),
        "{:?}",
        domains(&hits)
    );
}

#[test]
fn a_titleless_full_query_domain_needs_corroboration_to_precede_real_matches() {
    let mut records = vec![
        site(
            "awesome.com",
            Some("Awesome"),
            None,
            &["Awesome"],
            &[],
            popular(500, 2_000),
        ),
        // Bare: never crawled, and less linked than awesome.com, which
        // the query names by its first word.
        site(
            "awesome-python.com",
            None,
            None,
            &[],
            &[],
            obscure(400_000, 3),
        ),
    ];
    for i in 0..30 {
        records.push(site(
            &format!("pythontips{i}.org"),
            Some(&format!("Awesome Python tips {i}")),
            None,
            &[],
            &[],
            ranked(5_000 + i, 60),
        ));
    }
    let (_dir, searcher) = build(&records);
    let hits = experimental_search(&searcher, "awesome python", 10);
    let at = domains(&hits)
        .iter()
        .position(|d| *d == "awesome-python.com");
    assert!(at.is_none_or(|at| at >= 3), "{:?}", domains(&hits));
    let pool = searcher
        .candidate_pool(
            "awesome python",
            &experimental_rank_config(),
            &SearchOptions::default(),
            None,
        )
        .unwrap();
    let bare = pool
        .evidence
        .iter()
        .find(|e| e.domain == "awesome-python.com")
        .unwrap();
    assert!(bare.full_name);
    assert_eq!(bare.relevance_tier, 0);
    assert_eq!(bare.subject_coverage, Some(0.0));
    assert_eq!(top(&searcher, "awesome-python.com"), "awesome-python.com");
    // An independently indexed full alias establishes the name even
    // when the homepage itself could not be crawled.
    records[1].add_alias("Awesome Python");
    let (_dir, searcher) = build(&records);
    let hits = experimental_search(&searcher, "awesome python", 10);
    let at = domains(&hits)
        .iter()
        .position(|d| *d == "awesome-python.com");
    assert!(at.is_some_and(|at| at < 3), "{:?}", domains(&hits));
}

#[test]
fn the_networks_own_site_comes_first_for_its_name() {
    let records = vec![
        plumb_core::home_site_record(),
        site(
            "plumbs.com",
            Some("Plumbs"),
            None,
            &[],
            &[],
            popular(5_000, 2_000),
        ),
        site(
            "plumbfounded.com",
            Some("Plumbfounded Plumbing"),
            None,
            &[],
            &[],
            ranked(40_000, 200),
        ),
        site(
            "search.com",
            Some("Search"),
            None,
            &[],
            &[],
            popular(3_000, 5_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    for query in ["plumbsearch", "plumbsearch.org"] {
        let hits = searcher.search(query, 10).unwrap();
        assert_eq!(hits[0].domain, plumb_core::HOME_SITE, "{query}: {hits:?}");
    }
}

#[test]
fn wikidata_descriptions_are_searched_and_shown() {
    let mut navy = site("navyfederal.org", None, None, &[], &[], ranked(4_000, 500));
    navy.about = Some("American credit union".into());
    let records = vec![
        navy,
        site("navy.mil", None, None, &[], &[], popular(2_000, 9_000)),
        site("credit.com", None, None, &[], &[], ranked(30_000, 300)),
    ];
    let (_dir, searcher) = build(&records);
    // Names still count for more than descriptions, but a site that
    // only its description matches is found, and shows it.
    let hits = searcher.search("american credit union", 10).unwrap();
    let navy = hits.iter().find(|hit| hit.domain == "navyfederal.org");
    assert_eq!(
        navy.and_then(|hit| hit.description.as_deref()),
        Some("American credit union"),
        "{hits:?}"
    );
}

#[test]
fn summaries_stand_in_for_a_missing_description() {
    let mut recipes = site("allrecipes.com", None, None, &[], &[], ranked(3_000, 500));
    recipes.summary = Some("Allrecipes is a site where home cooks share recipes.".into());
    let mut described = site(
        "food.com",
        None,
        Some("Our own words"),
        &[],
        &[],
        ranked(5_000, 400),
    );
    described.summary = Some("Food.com shares recipes from home cooks.".into());
    let (_dir, searcher) = build(&[recipes, described]);
    let hits = searcher.search("home cooks recipes", 10).unwrap();
    let found = |domain: &str| hits.iter().find(|hit| hit.domain == domain);
    assert_eq!(
        found("allrecipes.com").and_then(|hit| hit.description.as_deref()),
        Some("Allrecipes is a site where home cooks share recipes."),
        "{hits:?}"
    );
    // A site's own description wins: its summary is not searched.
    assert!(found("food.com").is_none(), "{hits:?}");
}

#[test]
fn sites_that_redirect_name_the_site_they_redirect_to() {
    let mut lookalike = site(
        "pncbank.com",
        Some("PNC Bank"),
        None,
        &[],
        &[],
        ranked(9_000, 400),
    );
    lookalike.redirect = Some(plumb_core::Redirect {
        to: "pnc.com".into(),
        at: 1,
    });
    // A redirect to a site not in the index changes nothing.
    let mut elsewhere = site("fb.example", None, None, &[], &[], ranked(50_000, 10));
    elsewhere.redirect = Some(plumb_core::Redirect {
        to: "facebook.example".into(),
        at: 1,
    });
    let records = vec![
        site("pnc.com", None, None, &[], &[], ranked(2_000, 900)),
        lookalike,
        elsewhere,
        site("bank.com", None, None, &[], &[], ranked(3_000, 900)),
    ];
    let dir = TempDir::new().unwrap();
    let stats = build_index(dir.path(), &records).unwrap();
    assert_eq!((stats.docs, stats.redirected), (3, 1));
    let searcher = Searcher::open(dir.path()).unwrap();
    let hits = searcher.search("pnc bank", 10).unwrap();
    assert_eq!(hits[0].domain, "pnc.com", "{hits:?}");
    assert!(domains(&hits).iter().all(|d| *d != "pncbank.com"));
    assert_eq!(top(&searcher, "pncbank"), "pnc.com");
    assert_eq!(top(&searcher, "fb example"), "fb.example");
}
