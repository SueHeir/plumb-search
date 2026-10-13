//! Short official names, aliases and the names links give a site.

use super::*;

#[test]
fn short_official_names_beat_spelled_out_domains() {
    let (_dir, searcher) = build(&short_names_corpus());
    for (query, expected) in [
        ("wall street journal", "wsj.com"),
        ("the wall street journal", "wsj.com"),
        ("wsj", "wsj.com"),
        ("new york times", "nytimes.com"),
        ("The New York Times", "nytimes.com"),
        ("american airlines", "aa.com"),
        ("wall", "wall.org"),
    ] {
        assert_eq!(top(&searcher, query), expected, "{query}");
    }
}

#[test]
fn unofficial_aliases_stay_weaker_than_labels() {
    // Anyone can call their site anything in og:site_name.
    let records = vec![
        site(
            "usbank.com",
            None,
            None,
            &["U.S. Bank"],
            &[],
            popular(1_500, 9_000),
        ),
        site(
            "cheap-loans.biz",
            None,
            None,
            &["US Bank"],
            &[],
            ranked(800_000, 3),
        ),
    ];
    let (_dir, searcher) = build(&records);
    assert_eq!(top(&searcher, "us bank"), "usbank.com");
}

#[test]
fn link_texts_most_links_agree_on_name_the_site() {
    let records = vec![
        // Most sites linking with "the guardian" link here, though the
        // site's own pages never say it.
        site(
            "gdn.example",
            Some("Latest news, sport and opinion"),
            None,
            &[],
            &[("the guardian", 40), ("click here", 5)],
            obscure(5_000, 900),
        ),
        site(
            "guardian-insurance.example",
            Some("Guardian Insurance"),
            None,
            &[],
            &[("the guardian", 4), ("click here", 6)],
            obscure(4_000, 1_000),
        ),
        // Two sites use the text: not independent agreement yet.
        site(
            "rare.example",
            Some("Welcome"),
            None,
            &[],
            &[("rare words", 2)],
            obscure(9_000, 10),
        ),
        site(
            "news.example",
            Some("Click here for news"),
            None,
            &[],
            &[("click here", 5)],
            obscure(6_000, 50),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let named = |query: &str, cfg: &RankConfig| -> Vec<String> {
        searcher
            .search_with(query, 10, cfg)
            .unwrap()
            .into_iter()
            .filter(|hit| hit.named)
            .map(|hit| hit.domain)
            .collect()
    };
    let on = keep_all();
    let off = RankConfig {
        link_names: false,
        ..keep_all()
    };
    assert_eq!(named("the guardian", &on), ["gdn.example"]);
    assert_eq!(named("guardian", &on), ["gdn.example"]);
    assert_eq!(top(&searcher, "the guardian"), "gdn.example");
    assert!(named("the guardian", &off).is_empty());
    // A text spread over many sites names none of them.
    assert!(named("click here", &on).is_empty());
    assert!(named("rare words", &on).is_empty());
}

#[test]
fn link_names_beat_a_domain_that_is_the_name() {
    let records = vec![
        site(
            "steampowered.com",
            Some("Welcome to Steam"),
            None,
            &[],
            &[("steam", 300), ("steam store", 40)],
            ranked(2_000, 30_000),
        ),
        site(
            "steam.tv",
            Some("Steam TV"),
            None,
            &[],
            &[("steam tv", 4)],
            ranked(20_000, 2_000),
        ),
    ];
    let (_dir, searcher) = build(&records);
    let off = RankConfig {
        link_names: false,
        ..RankConfig::default()
    };
    let first = |cfg: &RankConfig| {
        searcher.search_with("steam", 10, cfg).unwrap()[0]
            .domain
            .clone()
    };
    assert_eq!(first(&off), "steam.tv");
    assert_eq!(first(&RankConfig::default()), "steampowered.com");
}
