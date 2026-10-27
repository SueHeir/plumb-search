//! Which reference pages found for a search may be listed, and the order a
//! medical search lists pages in.

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
        unsure: false,
    }
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
