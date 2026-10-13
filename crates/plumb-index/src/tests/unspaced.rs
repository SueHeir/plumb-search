//! Words of scripts written without spaces: Japanese, Chinese and Thai.

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
