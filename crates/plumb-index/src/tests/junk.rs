//! Sign-in pages of sites a search does not name are left out.

use super::*;

/// A site found only by meaning, whose homepage is at `url`.
fn at(domain: &str, url: &str, title: Option<&str>) -> SiteRecord {
    let mut record = site(domain, title, None, &[], &[], obscure(400_000, 3));
    record.url = Some(url.to_string());
    record
}

#[test]
fn sign_in_pages_of_sites_the_query_does_not_name_are_left_out() {
    let records = [
        site(
            "fidelity.com",
            Some("Fidelity Investments"),
            Some("Retirement plans, brokerage accounts and NetBenefits."),
            &["Fidelity Investments"],
            &[("fidelity", 300)],
            popular(400, 9_000),
        ),
        // Found by meaning, as on plumbsearch.org for "Fidelity
        // NetBenefits login" and "hulu login".
        at(
            "datahubnet.example",
            "https://datahubnet.example/",
            Some("DataHubNet - Login"),
        ),
        at("qris.example", "https://qris.example/login", Some("QRIS")),
        at(
            "jenta.example",
            "https://jenta.example/",
            Some("Sign in to your account - Jenta Tech"),
        ),
        at(
            "wealthscape.com",
            "https://www.wealthscape.com/",
            Some("Wealthscape Login"),
        ),
        SiteRecord {
            description: Some("How workplace retirement plans work.".into()),
            ..at(
                "retireguide.example",
                "https://retireguide.example/",
                Some("Workplace retirement plan guide"),
            )
        },
        // Says nothing of itself, and only meaning found it.
        at(
            "cardgate.example",
            "https://cardgate.example/",
            Some("My CardGate"),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![
        ("datahubnet.example", 0.95),
        ("sb24.example", 0.94),
        ("jenta.example", 0.93),
        ("wealthscape.com", 0.92),
        ("retireguide.example", 0.9),
        ("cardgate.example", 0.89),
    ]);
    let search = |query: &str| {
        searcher
            .search_meaning(
                query,
                10,
                &keep_all(),
                &SearchOptions::default(),
                Some(&meaning),
            )
            .unwrap()
            .hits
    };
    let hits = search("fidelity netbenefits login");
    let found = domains(&hits);
    assert!(found.contains(&"fidelity.com"), "{found:?}");
    // What meaning finds stays when it says what it is, short of other
    // sites' sign-in forms.
    assert!(found.contains(&"retireguide.example"), "{found:?}");
    for login in [
        "datahubnet.example",
        "qris.example",
        "jenta.example",
        "wealthscape.com",
        "cardgate.example",
    ] {
        assert!(!found.contains(&login), "{login} in {found:?}");
    }
    // Named, a site's own sign-in page is what was asked for.
    let hits = search("wealthscape login");
    assert_eq!(domains(&hits)[0], "wealthscape.com");
    // Not asked to sign in, meaning's finds stay.
    let hits = search("fidelity netbenefits");
    let found = domains(&hits);
    assert!(found.contains(&"cardgate.example"), "{found:?}");
    assert_eq!(top(&searcher, "datahubnet"), "datahubnet.example");
}
