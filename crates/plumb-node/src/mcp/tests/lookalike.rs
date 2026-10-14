use super::*;
use crate::mcp::lookalike::{borrowed_at, edit_distance, name_queries, squash};

#[test]
fn name_queries_read_the_brand_out_of_a_host() {
    assert_eq!(
        name_queries("usbank-login-help.com", "usbank-login-help.com"),
        ["usbank"]
    );
    assert_eq!(
        name_queries("paypal.com.secure-check.io", "secure-check.io"),
        ["secure check", "paypal", "check"]
    );
    assert_eq!(
        name_queries(
            "www.microsoft-support-helpline.com",
            "microsoft-support-helpline.com"
        ),
        ["microsoft support helpline", "microsoft", "helpline"]
    );
}

#[test]
fn resembles_spelled_out_names_and_typos() {
    let titled = |host: &str, real: &str, title: Option<&str>| {
        let domain = registrable_domain(host).unwrap();
        borrowed_at(&squash(host), &squash(&domain_label(&domain)), real, title)
    };
    let at = |host: &str, real: &str| titled(host, real, None);
    let check = |host: &str, real: &str| at(host, real).is_some();
    assert!(check("paypal-login.us", "paypal.com"));
    assert!(check("paypa1.com", "paypal.com"));
    assert!(check("twiter.com", "twitter.com"));
    assert!(check("rnicrosoft.com", "microsoft.com"));
    assert!(check("wellsfargo.com.account-check.io", "wellsfargo.com"));
    assert!(check("amazno.com", "amazon.com"));
    assert!(!check("deltafaucet.com", "united.com"));
    assert!(!check("bing.com", "ebay.com"));
    // Two-letter names match too much to count.
    assert!(!check("aardvark.com", "aa.com"));
    // A site on another's domain is named by its first name when its
    // title says it in capitals.
    let aws = Some("Cloud Computing Services - Amazon Web Services (AWS)");
    assert_eq!(
        titled("aws-console-login.com", "aws.amazon.com", aws),
        Some(0)
    );
    assert!(!check("aws-console-login.com", "aws.amazon.com"));
    assert!(titled("console-login.com", "aws.amazon.com", aws).is_none());
    let maps = Some("Google Maps");
    assert!(titled("maps-of-europe.com", "maps.google.com", maps).is_none());
    // Where the name is borrowed: the BBC's comes before live.com's.
    assert_eq!(at("bbc-news-live.com", "bbc.co.uk"), Some(0));
    assert_eq!(at("bbc-news-live.com", "live.com"), Some(7));
    assert_eq!(at("paypa1.com", "paypal.com"), Some(0));
    // A typo is where the host's own label is.
    let host = "secure.paypal.amazno.com";
    assert!(at(host, "paypal.com") < at(host, "amazon.com"));
    assert_eq!(edit_distance("kitten", "sitting"), 3);
    assert_eq!(edit_distance("ab", "ba"), 1);
}

#[test]
fn check_lookalike_requires_backend_evidence_for_previously_verified_hosts() {
    let empty = server(Vec::new());
    let broken = Mcp::new(Arc::new(Broken), None);
    for host in [
        "console.hetzner.cloud",
        "console.hetzner.com",
        "api.semanticscholar.org",
    ] {
        let answer = empty
            .check_lookalike(host, &SearchOptions::default())
            .unwrap();
        assert_eq!(answer["verdict"], "unknown", "{answer}");
        assert_eq!(answer["lookalike"], false);
        assert!(answer["affiliation"].is_null());
        // No embedded owner assertion survives a failed index lookup.
        assert!(broken
            .check_lookalike(host, &SearchOptions::default())
            .is_err());
    }
}

#[test]
fn check_lookalike_uses_indexed_public_owner_evidence_and_abstains_on_alternates() {
    for (owner, public_host, alternate) in [
        (
            "hetzner.com",
            "console.hetzner.com",
            "console.hetzner.cloud",
        ),
        (
            "semanticscholar.org",
            "api.semanticscholar.org",
            "api.semanticscholar.cloud",
        ),
        ("quartz.example", "api.quartz.example", "api.quartz.cloud"),
    ] {
        let mut indexed = hit(owner, 2.0, 0.9, true);
        indexed.official = true;
        let mcp = server(vec![indexed]);
        let answer = mcp
            .check_lookalike(public_host, &SearchOptions::default())
            .unwrap();
        assert_eq!(answer["verdict"], "official", "{answer}");
        assert_eq!(answer["site"]["domain"], owner);
        assert_eq!(answer["lookalike"], false);
        let answer = mcp
            .check_lookalike(alternate, &SearchOptions::default())
            .unwrap();
        assert_eq!(answer["verdict"], "suspected", "{answer}");
        // Unverified is not safe for credentials, as in 0.2.0.
        assert_eq!(answer["lookalike"], true);
        assert!(answer["site"].is_null());
        assert!(answer["affiliation"].is_null());
    }
}

#[test]
fn check_lookalike_preserves_impersonation_and_unverified_tenant_controls() {
    let mcp = scripted(|query| {
        let brand = if query.contains("hetzner") {
            "hetzner.com"
        } else {
            "paypal.com"
        };
        let mut official = hit(brand, 2.0, 0.9, true);
        official.official = true;
        results(vec![official], Vec::new())
    });
    for host in [
        "paypa1.com",
        "paypal-login.us",
        "paypal.com.attacker.example",
        "раypal.com",
        "console.hetzner.cloud.attacker.example",
    ] {
        let answer = mcp
            .check_lookalike(host, &SearchOptions::default())
            .unwrap();
        assert_eq!(answer["verdict"], "lookalike", "{host}: {answer}");
        assert_eq!(answer["lookalike"], true);
        assert!(answer["affiliation"].is_null());
    }
    // Exact-name alternate namespaces can host legitimate services/docs.
    // Neither credential words nor private hosting proves who operates them.
    for host in [
        "console.hetzner.net",
        "login.console.hetzner.cloud",
        "paypal.readthedocs.io",
    ] {
        let answer = mcp
            .check_lookalike(host, &SearchOptions::default())
            .unwrap();
        assert_eq!(answer["verdict"], "suspected", "{host}: {answer}");
        assert_eq!(answer["lookalike"], true);
        assert_eq!(answer["suspected"], true);
    }
}

#[test]
fn check_lookalike_names_the_site_whose_name_comes_first() {
    let mcp = scripted(|query| {
        let site = |domain: &str, link_score: f32| {
            let mut site = hit(domain, 2.0, link_score, true);
            site.official = true;
            site
        };
        let hits = if query.contains("bbc") {
            vec![site("bbc.co.uk", 0.8)]
        } else if query == "live" {
            vec![site("live.com", 0.85)]
        } else if query.contains("paypal") {
            vec![site("paypal.com", 0.9)]
        } else if query == "shop" {
            vec![site("shop.app", 0.95)]
        } else if query.contains("aws") {
            let mut aws = site("aws.amazon.com", 0.8);
            aws.title = Some("Cloud Computing Services - Amazon Web Services (AWS)".into());
            vec![aws]
        } else {
            Vec::new()
        };
        results(hits, Vec::new())
    });
    for (host, real) in [
        ("bbc-news-live.com", "bbc.co.uk"),
        ("aws-console-login.com", "aws.amazon.com"),
    ] {
        let answer = mcp
            .check_lookalike(host, &SearchOptions::default())
            .unwrap();
        assert_eq!(answer["verdict"], "lookalike", "{host}: {answer}");
        assert_eq!(answer["imitates"]["domain"], real, "{host}: {answer}");
    }
    // The same name on another ending is only suspected, and comes before
    // a word ahead of it.
    for (host, real) in [
        ("shop.paypal.de", "paypal.com"),
        ("aws.dev", "aws.amazon.com"),
    ] {
        let answer = mcp
            .check_lookalike(host, &SearchOptions::default())
            .unwrap();
        assert_eq!(answer["verdict"], "suspected", "{host}: {answer}");
        assert_eq!(answer["imitates"]["domain"], real, "{host}: {answer}");
    }
}
