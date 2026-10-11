use super::*;
use plumb_core::article::Article;
use plumb_index::pages::{Page, PageHit};

fn site(domain: &str, title: &str, links: &[(&str, &str)]) -> Hit {
    Hit {
        domain: domain.into(),
        url: format!("https://www.{domain}/"),
        title: Some(title.into()),
        description: Some("Source description of the site, not a section excerpt.".into()),
        score: 0.5,
        text_score: 0.5,
        link_score: 0.5,
        placing_text_score: None,
        country: None,
        named: false,
        official: true,
        key_pages: links
            .iter()
            .map(|(label, path)| KeyPage {
                label: (*label).into(),
                url: format!("https://www.{domain}{path}"),
            })
            .collect(),
        demand: None,
        missing_words: true,
        query_evidence: None,
    }
}

#[test]
fn task_navigation_selects_dining_shopping_and_combined_sections() {
    for (query, domain, title, label, path) in [
        (
            "Find restaurants within 2 km of Meridian Center, with names and map locations.",
            "meridiancenter.example",
            "Welcome | Meridian Center",
            "Dine & Shop",
            "/dine-shop/",
        ),
        (
            "Where can I go shopping at Cedar Pavilion?",
            "cedarpavilion.example",
            "Cedar Pavilion",
            "Shopping",
            "/stores/",
        ),
        (
            "Find shops and restaurants at Silver Arcade",
            "silverarcade.example",
            "Silver Arcade",
            "Dine and Shop",
            "/dine-and-shop/",
        ),
    ] {
        let original = site(domain, title, &[(label, path)]);
        let (selected, navigation) = site_destination(query, &original, &[]);
        let navigation = navigation.expect("an explicit, corroborated section");
        assert_eq!(selected.url, format!("https://www.{domain}{path}"));
        assert_eq!(navigation.source, "site_navigation");
        assert_eq!(navigation.label, label);
        assert_eq!(navigation.homepage_url, original.url);
        assert_eq!(selected.title, original.title);
        assert_eq!(selected.description, original.description);
        assert_eq!(selected.score, original.score);
        assert_eq!(original.url, format!("https://www.{domain}/"));
    }
}

#[test]
fn task_navigation_keeps_homepage_navigation_and_weak_or_unrelated_requests() {
    let mut original = site(
        "meridiancenter.example",
        "Meridian Center",
        &[("Dining", "/dining/")],
    );
    for query in [
        "Meridian Center",
        "Meridian Center website",
        "Meridian Center official site",
        "Meridian Center dining homepage",
        "Meridian Center dining home page",
        "https://www.meridiancenter.example/",
        "meridiancenter.example",
        "restaurants site:meridiancenter.example",
        "restaurants near Meridian",
        "Find restaurants",
        "Meridian Center parking",
        "Meridian Center shopping",
        "Meridian Center without restaurants",
        "Meridian Center -restaurants",
    ] {
        let (selected, navigation) = site_destination(query, &original, &[]);
        assert!(navigation.is_none(), "{query}");
        assert_eq!(selected.url, original.url);
    }
    original.title = Some("An unrelated observatory".into());
    assert!(
        site_destination("restaurants at Meridian Center", &original, &[])
            .1
            .is_none()
    );
    original.title = Some("Meridian Center".into());
    original.url = "https://www.meridiancenter.example/existing-document/".into();
    assert!(
        site_destination("restaurants at Meridian Center", &original, &[])
            .1
            .is_none()
    );
}

#[test]
fn task_navigation_rejects_unrelated_labels_and_untrusted_addresses() {
    let query = "Find restaurants at Meridian Center";
    for url in [
        "javascript:alert(1)",
        "https://other.example/restaurants/",
        "https://meridiancenter.example.other.example/restaurants/",
        "https://user@meridiancenter.example/restaurants/",
        "https://www.meridiancenter.example:8443/restaurants/",
        "https://www.meridiancenter.example/",
        "https://www.meridiancenter.example/en/",
        "https://www.meridiancenter.example/account/",
        "https://www.meridiancenter.example/restaurants/?filter=dine",
        "https://www.meridiancenter.example/restaurants/#map",
        "https://www.meridiancenter.example/restaurants/../restaurants/",
        "https://www.meridiancenter.example/%2e%2e/restaurants/",
        "https://www.meridiancenter.example/\\restaurants/",
        "https://www.meridiancenter.example/%0arestaurants/",
        "https://www.meridiancenter.example/%7f/restaurants/",
        "https://www.meridiancenter.example/%zz/restaurants/",
        " https://www.meridiancenter.example/restaurants/",
    ] {
        let mut original = site("meridiancenter.example", "Meridian Center", &[]);
        original.key_pages.push(KeyPage {
            label: "Restaurants".into(),
            url: url.into(),
        });
        assert!(site_destination(query, &original, &[]).1.is_none(), "{url}");
    }
    for label in [
        "Dining room furniture",
        "Restaurant news",
        "Account",
        "About",
        "Restaurants <script>",
    ] {
        let original = site(
            "meridiancenter.example",
            "Meridian Center",
            &[(label, "/restaurants/")],
        );
        assert!(
            site_destination(query, &original, &[]).1.is_none(),
            "{label}"
        );
    }
}

#[test]
fn task_navigation_rejects_competing_sections_but_deduplicates_the_same_url() {
    let query = "Find restaurants at Meridian Center";
    let original = site(
        "meridiancenter.example",
        "Meridian Center",
        &[("Restaurants", "/restaurants/"), ("Dining", "/dining/")],
    );
    assert!(site_destination(query, &original, &[]).1.is_none());
    let original = site(
        "meridiancenter.example",
        "Meridian Center",
        &[
            ("Restaurants", "/restaurants/"),
            ("Dining", "/restaurants/"),
            ("Dining", "/restaurants/?filter=dine"),
        ],
    );
    assert!(site_destination(query, &original, &[]).1.is_some());
}

#[test]
fn task_navigation_yields_to_strong_retrieved_pages_and_preserves_row_budget() {
    let original = site(
        "meridiancenter.example",
        "Meridian Center",
        &[("Dining", "/dining/")],
    );
    let page = Page::from_reference(Article {
        title: "Restaurants at Meridian Center".into(),
        item: Some("https://www.meridiancenter.example/restaurants/guide/".into()),
        ..Article::default()
    })
    .unwrap();
    let mut pages = vec![PlacedPage {
        hit: PageHit {
            page,
            score: 0.8,
            popularity: 0.0,
            named: false,
            whole: false,
            learned: None,
        },
        under: None,
        at: 0,
    }];
    let query = "Find restaurants at Meridian Center";
    for (named, whole, score) in [(false, false, 0.8), (true, false, 0.1), (false, true, 0.1)] {
        pages[0].hit.named = named;
        pages[0].hit.whole = whole;
        pages[0].hit.score = score;
        assert!(site_destination(query, &original, &pages).1.is_none());
    }
    let sites = [original];
    let rows = crate::assembly::ordered_rows(query, &sites, &pages, &pages, 1);
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0], crate::assembly::Row::Page { .. }));
    pages[0].under = Some(sites[0].domain.clone());
    assert!(site_destination(query, &sites[0], &pages).1.is_none());
}

#[test]
fn task_navigation_frozen_pg10_regression_preserves_provenance_and_site_metadata() {
    // Tuning/regression fixture only: the captured baseline and all qrels stay frozen.
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/frozen-pg10-navigation.json")).unwrap();
    let site: Hit = serde_json::from_value(fixture["site"].clone()).unwrap();
    let sites = [site];
    let rows =
        crate::assembly::ordered_rows(fixture["query"].as_str().unwrap(), &sites, &[], &[], 10);
    let output = serde_json::to_value(rows).unwrap();
    assert_eq!(output[0]["site"]["url"], fixture["candidate_url"]);
    assert_eq!(output[0]["navigation"]["homepage_url"], sites[0].url);
    assert_eq!(output[0]["navigation"]["label"], "Dine & Shop");
    assert_eq!(output[0]["navigation"]["source"], "site_navigation");
    assert_eq!(output[0]["site"]["title"], fixture["site"]["title"]);
    assert_eq!(
        output[0]["site"]["description"],
        fixture["site"]["description"]
    );
    assert!(output[0]["pages"].as_array().unwrap().is_empty());
    assert!(output[0]["navigation"].get("title").is_none());
    assert!(output[0]["navigation"].get("text").is_none());
    assert!(output[0]["navigation"].get("published").is_none());
    assert_eq!(sites[0].url, fixture["site"]["url"]);
}
