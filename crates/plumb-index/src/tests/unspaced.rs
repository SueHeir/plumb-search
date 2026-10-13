//! Words of other scripts: Japanese, Chinese and Thai, written without
//! spaces, and Arabic, Greek and Russian, written with or without some of
//! their marks.

use super::*;

fn unspaced_corpus() -> Vec<SiteRecord> {
    vec![
        site(
            "tenki.example",
            Some("日本気象協会 tenki.jp / 全国の天気予報・地震・台風"),
            None,
            &[],
            &[],
            ranked(900, 5_000),
        ),
        site(
            "tower.example",
            Some("東京タワー公式サイト"),
            None,
            &[],
            &[],
            ranked(950, 5_000),
        ),
        site(
            "dome.example",
            Some("東京ドーム"),
            None,
            &[],
            &[],
            ranked(500, 9_000),
        ),
        site(
            "sanook.example",
            Some("Sanook.com รวมข่าว บันเทิง ดูดวง"),
            None,
            &[],
            &[],
            ranked(800, 5_000),
        ),
        site(
            "rakuten.example",
            Some("楽天市場"),
            None,
            &["楽天"],
            &[],
            ranked(100, 50_000),
        ),
    ]
}

fn found(searcher: &Searcher, query: &str) -> Vec<String> {
    let hits = searcher
        .search_full(query, 10, &keep_all(), &SearchOptions::default())
        .unwrap()
        .hits;
    domains(&hits).into_iter().map(str::to_string).collect()
}

#[test]
fn a_word_is_found_inside_a_title_without_spaces() {
    let (_dir, searcher) = build(&unspaced_corpus());
    assert_eq!(found(&searcher, "天気予報"), ["tenki.example"]);
    assert_eq!(found(&searcher, "天気予報 tenki"), ["tenki.example"]);
    assert_eq!(found(&searcher, "東京タワー 公式")[0], "tower.example");
    assert_eq!(found(&searcher, "東京タワー")[0], "tower.example");
    // A Thai word, tone mark and all.
    assert_eq!(found(&searcher, "ข่าว"), ["sanook.example"]);
    // Names still match whole.
    assert_eq!(found(&searcher, "楽天")[0], "rakuten.example");
    // All of a word's pieces are needed, not some: 東京 alone is not
    // 東京タワー, and 予報天気 has the pieces' characters but not them.
    assert!(!found(&searcher, "東京タワー").contains(&"dome.example".to_string()));
    assert!(found(&searcher, "予報天気").is_empty());
}

#[test]
fn arabic_is_found_however_its_alefs_and_yehs_are_written() {
    let site_titled =
        |domain: &str, title: &str| site(domain, Some(title), None, &[], &[], ranked(900, 5_000));
    let (_dir, searcher) = build(&[
        site_titled("ahly.example", "أخبار النادي الأهلي"),
        site_titled("bank.example", "البنك الاهلي"),
        site_titled("iran.example", "اخبار ایران"),
        site_titled("masry.example", "مصرى سات"),
    ]);
    // Each spelling finds the other, the one typed first.
    for (query, first, then) in [
        ("الأهلي", "ahly.example", "bank.example"),
        ("الاهلي", "bank.example", "ahly.example"),
        ("أخبار", "ahly.example", "iran.example"),
        ("اخبار", "iran.example", "ahly.example"),
    ] {
        assert_eq!(found(&searcher, query), [first, then], "{query}");
    }
    let mut stretched = found(&searcher, "اخـبـار");
    stretched.sort();
    assert_eq!(stretched, ["ahly.example", "iran.example"]);
    assert_eq!(found(&searcher, "ايران"), ["iran.example"]);
    for query in ["مصري", "مصرى"] {
        assert_eq!(found(&searcher, query), ["masry.example"], "{query}");
    }
}

#[test]
fn greek_and_russian_are_found_with_or_without_their_accents() {
    let site_titled =
        |domain: &str, title: &str| site(domain, Some(title), None, &[], &[], ranked(900, 5_000));
    let (_dir, searcher) = build(&[
        site_titled("thess.example", "Δήμος Θεσσαλονίκη"),
        site_titled("blog.example", "ΘΕΣΣΑΛΟΝΙΚΗ ΣΗΜΕΡΑ"),
        site_titled("athens.example", "ΑΘΗΝΑΣ news"),
        site_titled("korolev.example", "Королёв наукоград"),
        site_titled("orel.example", "Королев и Орел"),
    ]);
    // Each spelling finds the other, the one typed first.
    for (query, first, then) in [
        ("θεσσαλονίκη", "thess.example", "blog.example"),
        ("θεσσαλονικη", "blog.example", "thess.example"),
        ("королёв", "korolev.example", "orel.example"),
        ("королев", "orel.example", "korolev.example"),
    ] {
        assert_eq!(found(&searcher, query), [first, then], "{query}");
    }
    // A capital sigma lowercases to σ, not the final ς typed.
    assert_eq!(found(&searcher, "αθήνας"), ["athens.example"]);
}
