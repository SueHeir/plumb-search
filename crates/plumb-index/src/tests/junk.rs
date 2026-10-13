//! Sign-in pages of sites a search does not name are left out.

use super::*;
use crate::junk::{is_sign_in_page, names_label};

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
        at(
            "retireguide.example",
            "https://retireguide.example/",
            Some("Workplace retirement plan guide"),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let meaning = FixedMeaning(vec![
        ("datahubnet.example", 0.95),
        ("sb24.example", 0.94),
        ("jenta.example", 0.93),
        ("wealthscape.com", 0.92),
        ("retireguide.example", 0.9),
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
    // What meaning finds stays, short of other sites' sign-in forms.
    assert!(found.contains(&"retireguide.example"), "{found:?}");
    for login in [
        "datahubnet.example",
        "qris.example",
        "jenta.example",
        "wealthscape.com",
    ] {
        assert!(!found.contains(&login), "{login} in {found:?}");
    }
    // Named, a site's own sign-in page is what was asked for.
    let hits = search("wealthscape login");
    assert_eq!(domains(&hits)[0], "wealthscape.com");
    assert_eq!(top(&searcher, "datahubnet"), "datahubnet.example");
}

#[test]
fn sign_in_pages_are_told_by_title_or_address() {
    for title in [
        "Login - QRIS",
        "Login - Arvixe",
        "Login | SCARLETT",
        "Login Page",
        "Wealthscape Investor Login",
        "User Login | The Digital Jungle",
        "Sign in to your account - Jenta Tech",
        "Sign In — FAST X OTP",
        "Sign-In with Ethereum",
        "The Grid - Please log in to continue",
        "ePIS | Login",
    ] {
        assert!(
            is_sign_in_page("https://example.com/", Some(title)),
            "{title:?}"
        );
    }
    for title in [
        "Fidelity Investments",
        "Login.gov",
        "Blog on gardening",
        "How logins work",
        "Signing in Spanish",
        "Catalog online",
        "DeepSeek AI – Free AI Chat Online & DeepSeek-V4 No Login",
    ] {
        assert!(
            !is_sign_in_page("https://example.com/", Some(title)),
            "{title:?}"
        );
    }
    for url in [
        "https://qrisajaib.com/login",
        "https://www.its52.com/login.aspx",
        "https://id.koder.dev/oauth/v2/authorize?client_id=x",
        "https://login.example.com/",
        "https://example.com/users/sign_in",
    ] {
        assert!(is_sign_in_page(url, None), "{url}");
    }
    for url in [
        "https://example.com/",
        "https://example.com/blog/login-tips",
        "https://authors.example.com/",
    ] {
        assert!(!is_sign_in_page(url, None), "{url}");
    }
}

#[test]
fn a_query_names_a_label_by_its_words_in_a_row() {
    assert!(names_label("wealthscape login", "wealthscape.com"));
    assert!(names_label("us bank login", "usbank.com"));
    assert!(names_label("hdfc login", "hdfcbank.com"));
    assert!(names_label("my chart sign in", "mychart.com"));
    assert!(!names_label("v8 login", "v8engines.com"));
    assert!(!names_label("fidelity netbenefits login", "login.xyz"));
    assert!(!names_label("xfinity sign in", "signin.example"));
    assert!(!names_label("hulu login", "arvixe.com"));
    assert!(names_label("uc davis canvas login", "canvas.ucdavis.edu"));
    assert!(!names_label("att net account login", "datahub.net"));
    assert!(!names_label("bank online login", "datahubnet.online"));
}
