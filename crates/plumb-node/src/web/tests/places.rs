//! Places around a town and the sites about them.

use super::*;

#[tokio::test]
async fn sites_saying_what_come_first_around_a_town_with_no_places() {
    let (_, _, body) = send(
        router(Arc::new(BreweryPlaces)),
        "/api/search?q=brewery+in+aurora&full=1",
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let domains: Vec<&str> = json["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["domain"].as_str().unwrap())
        .collect();
    assert_eq!(
        domains,
        ["westword.com", "denvergov.org", "denverbroncos.com"]
    );
    assert_eq!(json["places"]["status"], "no_indexed_matches", "{body}");
    let (_, _, page) = send(
        router(Arc::new(BreweryPlaces)),
        "/search?q=brewery+in+aurora",
    )
    .await;
    assert!(!page.contains("aria-label=\"Places\""), "{page}");
}

#[tokio::test]
async fn the_api_reports_missing_location_around_nowhere() {
    let (_, _, body) = send(
        router(Arc::new(BreweryPlaces)),
        "/api/search?q=brewery+near+me&full=1",
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["places"]["status"], "missing_location", "{body}");
    assert!(json["places"]["hits"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn places_around_a_town_put_local_sites_before_the_town_s_own() {
    for query in ["brewery+in+denver", "denver+brewery"] {
        let (_, _, body) = send(
            router(Arc::new(BreweryPlaces)),
            &format!("/api/search?q={query}&full=1"),
        )
        .await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let domains: Vec<&str> = json["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["domain"].as_str().unwrap())
            .collect();
        // The breweries' own sites (one the index does not have, as the
        // place says), then the guide that says breweries; the sites
        // only named after Denver are left out.
        assert_eq!(
            domains,
            ["greatdivide.com", "unknown-ales.example", "westword.com"],
            "{query}"
        );
        assert_eq!(json["hits"][0]["title"], "Great Divide Brewing Co.");
        assert_eq!(json["hits"][1]["title"], "Unknown Ales");
        assert_eq!(json["hits"][1]["description"], "Brewery");
        assert_eq!(json["places"]["hits"].as_array().unwrap().len(), 3);

        let (_, _, page) = send(
            router(Arc::new(BreweryPlaces)),
            &format!("/search?q={query}"),
        )
        .await;
        let at = |d: &str| page.find(&format!("https://{d}")).unwrap();
        assert!(page.contains("<section class=\"pl\""), "{page}");
        assert!(at("www.westword.com") > 0, "{page}");
        assert!(!page.contains("denverbroncos.com"), "{page}");
    }
}

#[tokio::test]
async fn mcp_json_and_searxng_share_bounded_places_and_navigation_controls() {
    let query = "brewery in denver";
    let mcp = crate::mcp::Mcp::new(Arc::new(BreweryPlaces), None);
    let answer = mcp
        .search(query, Some(2), &SearchOptions::default())
        .unwrap();
    let (_, _, body) = send(
        router(Arc::new(BreweryPlaces)),
        "/api/search?q=brewery+in+denver&full=1&limit=2",
    )
    .await;
    let api: serde_json::Value = serde_json::from_str(&body).unwrap();
    let (_, _, body) = send(
        router(Arc::new(BreweryPlaces)),
        "/search?q=brewery+in+denver&format=json&limit=2",
    )
    .await;
    let searx: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["places"], api["places"]);
    assert_eq!(answer["places"], searx["places"]);
    assert_eq!(answer["places"]["hits"][0]["place"]["osm"], "n2");
    assert_eq!(answer["places"]["hits"][0]["km"], 1.0);
    assert_eq!(answer["places"]["license"], "ODbL");
    assert_eq!(answer["results"][0]["domain"], api["hits"][0]["domain"]);
    assert_eq!(searx["results"].as_array().unwrap().len(), 2);
    assert!(mcp
        .search_sites(query, Some(2), &SearchOptions::default())
        .unwrap()
        .get("places")
        .is_none());
    assert!(mcp
        .search(
            "brewery in denver site:greatdivide.com",
            Some(2),
            &SearchOptions::default()
        )
        .unwrap()
        .get("places")
        .is_none());
    let near = mcp
        .search(
            "brewery near me",
            Some(2),
            &SearchOptions {
                country: Some("US".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(near["places"]["status"], "missing_location");
    let named = crate::mcp::Mcp::new(Arc::new(BankPlaces), None)
        .search("us bank", None, &SearchOptions::default())
        .unwrap();
    assert!(named.get("places").is_none());
}
