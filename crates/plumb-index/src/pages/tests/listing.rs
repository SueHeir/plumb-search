//! Which pages found for a search may be listed, which keep places among
//! the pages looked at, and the order a medical search lists them in.

use super::*;

/// A reference page at `url` titled `title`, found by its words.
fn reference(url: &str, title: &str, score: f32) -> PageHit {
    PageHit {
        page: Page::from_set(
            REFERENCE_SET,
            Article {
                title: title.into(),
                item: Some(url.into()),
                views: 5_000,
                ..Article::default()
            },
        )
        .unwrap(),
        score,
        named: false,
        popularity: score,
        whole: false,
        learned: None,
        meant: false,
    }
}

#[test]
fn pages_that_cannot_be_listed_take_no_place_of_those_that_can() {
    let sites = [
        site("mckinleymuseum.org", false),
        site("mckinley.com", false),
    ];
    let article = |title: &str, score: f32| found(title, None, true, score);
    // Too few of their titles' words are asked for.
    let facts = reference(
        "https://www.history.com/articles/william-mckinley",
        "William McKinley: Facts, Assassination & Presidency",
        0.95,
    );
    let tour = reference(
        "https://www.history.com/articles/mckinley-birthplace",
        "Niles, Ohio Birthplace Tour",
        0.94,
    );
    let asked = reference(
        "https://www.history.com/articles/mckinley-presidency",
        "McKinley Presidency",
        0.5,
    );
    let kept = |pages: Vec<PageHit>| {
        let mut pages = pages;
        keep_listable("mckinley president", &sites, &mut pages, 2);
        pages
            .into_iter()
            .map(|hit| hit.page.title)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        kept(vec![
            article("William McKinley", 0.99),
            facts.clone(),
            tour.clone(),
            article("Presidency of William McKinley", 0.8),
            article("McKinley Tariff", 0.7),
        ]),
        [
            "William McKinley",
            facts.page.title.as_str(),
            tour.page.title.as_str(),
            "Presidency of William McKinley"
        ]
    );
    // The best reference page that may be listed still takes the last
    // place, and one that may not is not brought in.
    assert_eq!(
        kept(vec![
            article("William McKinley", 0.99),
            article("Presidency of William McKinley", 0.8),
            article("McKinley Tariff", 0.7),
            asked.clone(),
        ]),
        ["William McKinley", asked.page.title.as_str()]
    );
    assert_eq!(
        kept(vec![
            article("William McKinley", 0.99),
            article("Presidency of William McKinley", 0.8),
            facts,
        ]),
        ["William McKinley", "Presidency of William McKinley"]
    );
}

#[test]
fn the_article_in_the_other_number_takes_no_place_of_a_page_that_may_be_listed() {
    let (_dir, s) = searcher(&[page("Tariff", 90_000, &[])]);
    let sites = [known_site("tariffs.net", true, 0.015)];
    let mut pages = vec![
        reference(
            "https://www.history.com/articles/customs",
            "Customs Duties Explained",
            0.99,
        ),
        found("History of tariffs", None, false, 0.01),
    ];
    s.add_other_number("tariffs", &sites, &mut pages, 2)
        .unwrap();
    // The reference page shares no word of the search, so may not be listed.
    assert_eq!(
        titles(&pages),
        ["Tariff", "Customs Duties Explained", "History of tariffs"]
    );
}

#[test]
fn a_reference_page_on_a_best_site_shares_a_word_besides_the_sites_name() {
    let query = "webmd zyrtec mucinex interaction";
    let sites = [site("webmd.com", true), site("medlineplus.gov", false)];
    let eczema = reference(
        "https://www.webmd.com/skin-problems-and-treatments/eczema/self-care-for-eczema",
        "Self-Care for Eczema",
        0.9,
    );
    assert!(!subpage_asked(query, &sites, &eczema));
    let zyrtec = reference(
        "https://www.webmd.com/drugs/2/drug-5485/zyrtec-oral/details",
        "Zyrtec Oral: Uses, Side Effects, Interactions",
        0.9,
    );
    assert!(subpage_asked(query, &sites, &zyrtec));
    // The site's name is no such word: "apple i phone s" is not Apple's
    // recall of a plug.
    let recall = reference(
        "https://support.apple.com/ac-wallplug-adapter",
        "Apple AC Wall Plug Adapter Recall Program",
        0.9,
    );
    assert!(!subpage_asked(
        "apple i phone s",
        &[site("apple.com", true)],
        &recall
    ));
    // A site's own page needs none: "met museum hours".
    let mut visit = reference(
        "https://www.metmuseum.org/plan-your-visit",
        "Plan Your Visit",
        0.9,
    );
    visit.page.set = SUBPAGES_SET.into();
    assert!(subpage_asked(
        "met museum hours",
        &[site("metmuseum.org", true)],
        &visit
    ));
}

#[test]
fn a_number_with_a_comma_is_one_word() {
    let diet = reference(
        "https://www.healthline.com/nutrition/1500-calorie-diet",
        "A 1,500-Calorie Diet: Food Lists, Meal Plan and More",
        0.9,
    );
    assert!(!subpage_asked("500 calories a day diet", &[], &diet));
    assert!(subpage_asked("1500 calorie diet", &[], &diet));
    assert!(subpage_asked("1,500 calorie diet", &[], &diet));
}

#[test]
fn a_medical_search_lists_public_health_pages_before_other_sites_pages() {
    let guide = reference(
        "https://www.webmd.com/women/your-guide-urinary-tract-infections",
        "Your Guide to Urinary Tract Infections",
        0.95,
    );
    let medline = reference(
        "https://medlineplus.gov/urinarytractinfections.html",
        "Urinary Tract Infections",
        0.8,
    );
    let nhs = reference(
        "https://www.nhs.uk/conditions/urinary-tract-infections-utis/",
        "Urinary tract infections (UTIs)",
        0.7,
    );
    let article = found("Urinary tract infection", None, true, 0.9);
    let order = |query: &str| {
        let mut pages = vec![guide.clone(), article.clone(), medline.clone(), nhs.clone()];
        crate::health::sources_pages_first(query, &mut pages);
        pages
            .into_iter()
            .map(|hit| hit.page.url)
            .collect::<Vec<_>>()
    };
    let urls = |pages: [&PageHit; 4]| pages.map(|hit| hit.page.url.clone());
    // The article keeps its place among them.
    assert_eq!(
        order("urinary tract infection symptoms men"),
        urls([&medline, &article, &nhs, &guide])
    );
    // A source the search names comes first.
    assert_eq!(
        order("webmd urinary tract infection symptoms"),
        urls([&guide, &article, &medline, &nhs])
    );
    // Other searches keep the order found.
    assert_eq!(
        order("your guide to tracts"),
        urls([&guide, &article, &medline, &nhs])
    );
}

#[test]
fn another_sites_page_comes_after_the_source_a_search_asks_for() {
    // cdc.gov's homepage shares few of the words, so a page would lead it.
    let mut cdc = site("cdc.gov", false);
    cdc.text_score = 0.2;
    let sites = [cdc, site("medlineplus.gov", false)];
    let contagious = reference(
        "https://www.healthline.com/health/how-long-is-the-flu-contagious",
        "How Long Is the Flu Contagious?",
        0.9,
    );
    let at = |query: &str, page: &PageHit| {
        let placed = place_pages(query, &sites, vec![page.clone()]);
        placed.first().map(|p| p.at)
    };
    assert_eq!(at("how long is the flu contagious", &contagious), Some(0));
    assert_eq!(
        at("CDC how long is the flu contagious", &contagious),
        Some(1)
    );
    assert_eq!(
        at("how long is the flu contagious cdc", &contagious),
        Some(1)
    );
    // The source's own page still may.
    let spreads = reference(
        "https://www.cdc.gov/flu/spread/index.html",
        "How Long Is the Flu Contagious?",
        0.9,
    );
    assert_eq!(at("CDC how long is the flu contagious", &spreads), Some(0));
    // So after the learned ranking had its say.
    let mut sites = sites.to_vec();
    let mut placed = vec![PlacedPage {
        hit: contagious,
        under: None,
        at: 0,
    }];
    crate::health::sources_lead(
        "how long is the flu contagious cdc",
        &mut sites,
        &mut placed,
        &|_| false,
    );
    assert_eq!(placed[0].at, 1);
}
