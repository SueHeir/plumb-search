//! The site's own page a search names with the site, listed under it.

use super::*;

fn inner(url: &str, title: &str, views: u64) -> Page {
    Page::from_set(
        SUBPAGES_SET,
        Article {
            title: title.into(),
            description: Some(format!("{title} on its site.")),
            item: Some(url.into()),
            views,
            ..Article::default()
        },
    )
    .unwrap()
}

fn titled_site(domain: &str, title: &str) -> crate::Hit {
    crate::Hit {
        title: Some(title.into()),
        text_score: 0.4,
        ..site(domain, false)
    }
}

/// The pages `query` finds, and `also`, with the site's own page it
/// names, as the node places them under `sites`: (under, title) of each.
fn placed(
    searcher: &PageSearcher,
    query: &str,
    sites: &[crate::Hit],
    also: &[PageHit],
) -> Vec<(Option<String>, String)> {
    let mut found = searcher.search(query, 10).unwrap();
    found.extend_from_slice(also);
    searcher
        .add_named_site_page(query, sites, &mut found)
        .unwrap();
    place_pages(query, sites, found)
        .into_iter()
        .map(|p| (p.under, p.hit.page.title))
        .collect()
}

#[test]
fn the_page_a_search_names_with_its_site_goes_under_it() {
    let mut bureau = found(
        "United States Census Bureau",
        Some("census.gov"),
        false,
        0.4,
    );
    bureau.page.item = Some("Q637413".into());
    let (_dir, searcher) = searcher(&[
        inner("https://www.census.gov/popclock/", "Population Clock", 900),
        inner(
            "https://www.census.gov/quickfacts/fact/table/US/PST045224",
            "U.S. Census Bureau QuickFacts: United States",
            700,
        ),
        inner("https://www.who.int/health-topics/malaria", "Malaria", 300),
        // Better read, but in another language.
        inner("https://www.who.int/es/health-topics/malaria", "Malaria", 3_000),
        inner(
            "https://www.who.int/publications-detail-redirect/9789241514965",
            "Malaria rapid diagnostic test performance. Results of WHO product testing of \
             malaria RDTs: Round 8 (2016-2018)",
            3_000,
        ),
        inner(
            "https://www.mayoclinic.org/diseases-conditions/plantar-fasciitis/symptoms-causes/syc-20354846",
            "Plantar fasciitis - Symptoms and causes - Mayo Clinic",
            500,
        ),
        inner("https://www.nps.gov/yose/", "Yosemite National Park", 800),
    ]);
    // Found by words its text shares with the search.
    let story = PageHit {
        page: inner(
            "https://www.census.gov/newsroom/stories/mexico-independence-day.html",
            "Mexico Independence Day (1810): September 16, 2023",
            4_000,
        ),
        score: 0.55,
        named: false,
        popularity: 0.7,
        whole: false,
        learned: None,
    };
    let census = [
        titled_site("census.gov", "Census.gov | U.S. Census Bureau Homepage"),
        site("worldometers.info", false),
    ];
    let under = |title: &str| (Some("census.gov".to_string()), title.to_string());
    // The clock, not a newsroom story; the article stays under the site.
    assert_eq!(
        placed(
            &searcher,
            "census population clock",
            &census,
            &[story, bureau]
        ),
        [
            under("Population Clock"),
            under("United States Census Bureau")
        ]
    );
    // The site's name in the title counts for nothing.
    assert_eq!(
        placed(&searcher, "census quickfacts", &census, &[])[0],
        under("U.S. Census Bureau QuickFacts: United States")
    );
    // The page on malaria, not the report that also says "malaria".
    let who = [titled_site("who.int", "World Health Organization (WHO)")];
    assert_eq!(
        placed(&searcher, "who malaria", &who, &[]),
        [(Some("who.int".to_string()), "Malaria".to_string())]
    );
    // The site's name may end the search, and the title go on after a
    // break.
    let mayo = [titled_site("mayoclinic.org", "Mayo Clinic")];
    let fasciitis = [(
        Some("mayoclinic.org".to_string()),
        "Plantar fasciitis - Symptoms and causes - Mayo Clinic".to_string(),
    )];
    assert_eq!(
        placed(&searcher, "plantar fasciitis mayo clinic", &mayo, &[]),
        fasciitis
    );
    assert_eq!(
        placed(
            &searcher,
            "what is plantar fasciitis mayo clinic",
            &mayo,
            &[]
        ),
        fasciitis
    );
    // Another name of the site the ranking found the search to start
    // with.
    let mut bureau = titled_site("census.gov", "Census.gov | U.S. Census Bureau Homepage");
    bureau.query_evidence = Some(crate::CandidateEvidence {
        domain: "census.gov".into(),
        lexical_score: 0.3,
        semantic_closeness: None,
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
        partial_name_words: 2,
        relevance_tier: 1,
        source_quality: None,
    });
    assert_eq!(
        placed(&searcher, "census bureau population clock", &[bureau], &[])[0],
        under("Population Clock")
    );
}

#[test]
fn only_a_page_the_rest_of_the_search_names_is_taken() {
    let (_dir, searcher) = searcher(&[
        inner("https://www.census.gov/popclock/", "Population Clock", 900),
        inner(
            "https://www.who.int/about/governance/president",
            "President of the World Health Assembly",
            300,
        ),
        inner(
            "https://www.census.gov/library/stories/clock.html",
            "How the population clock is built, and what it says about births",
            300,
        ),
    ]);
    let census = [titled_site(
        "census.gov",
        "Census.gov | U.S. Census Bureau Homepage",
    )];
    let taken = |query: &str, sites: &[crate::Hit]| {
        let mut found = Vec::new();
        searcher
            .add_named_site_page(query, sites, &mut found)
            .unwrap();
        found
            .into_iter()
            .map(|hit| hit.page.title)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        taken("census population clock", &census),
        ["Population Clock"]
    );
    // A question that starts with the site's name asks nothing of it.
    let who = [titled_site("who.int", "World Health Organization (WHO)")];
    assert!(taken("who is the president", &who).is_empty());
    // Words the site has no page titled with, or only as a small part of
    // a title.
    assert!(taken("census births", &census).is_empty());
    assert!(taken("census unemployment", &census).is_empty());
    // A site the whole query names, or one that is not first.
    let named = [site("census.gov", true)];
    assert!(taken("census population clock", &named).is_empty());
    let second = [site("worldometers.info", false), census[0].clone()];
    assert!(taken("census population clock", &second).is_empty());
    // A search that names a page is for that page.
    let mut found = vec![found("Census Population Clock", None, true, 0.9)];
    searcher
        .add_named_site_page("census population clock", &census, &mut found)
        .unwrap();
    assert_eq!(titles(&found), ["Census Population Clock"]);
}

#[test]
fn the_model_reads_a_site_as_it_did_without_its_own_page() {
    let mut article = found(
        "United States Census Bureau",
        Some("census.gov"),
        false,
        0.4,
    );
    article.page.item = Some("Q637413".into());
    let mut clock = PageHit {
        page: inner("https://www.census.gov/popclock/", "Population Clock", 900),
        score: 0.5,
        named: false,
        popularity: 0.6,
        whole: false,
        learned: None,
    };
    clock.page.site = Some("census.gov".into());
    let census = titled_site("census.gov", "Census.gov | U.S. Census Bureau Homepage");
    let row = |under: &[&PageHit]| crate::learned::RowSignals::site(&census, under);
    assert_eq!(
        crate::learned::features("census population clock", &[row(&[&clock, &article])]),
        crate::learned::features("census population clock", &[row(&[&article])])
    );
}
