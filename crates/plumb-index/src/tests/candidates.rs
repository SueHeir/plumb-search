//! The raw signals of a query's deep candidates (`plumb-tools eval
//! --candidates-out`).

use super::*;
use crate::candidates::FIELDS;

#[test]
fn candidates_carry_each_fields_words_and_where_they_came_from() {
    let field = |name| FIELDS.iter().position(|f| *f == name).unwrap();
    let (_dir, searcher) = build(&[
        site(
            "toolshed.example",
            Some("Garden Tools and Supplies"),
            Some("Spades, rakes and shears"),
            &[],
            &[],
            obscure(200_000, 10),
        ),
        site(
            "spades.example",
            Some("Spades"),
            Some("Garden spades"),
            &[],
            &[],
            obscure(300_000, 5),
        ),
        site(
            "faraway.example",
            Some("Weather today"),
            None,
            &[],
            &[],
            obscure(400_000, 2),
        ),
        site(
            "unrelated.example",
            Some("Cooking"),
            None,
            &[],
            &[],
            obscure(500_000, 2),
        ),
    ]);
    let meaning = FixedMeaning(vec![("faraway.example", 0.4)]);
    let signals = searcher
        .candidate_signals(
            "garden spades",
            10,
            &RankConfig::default(),
            Some(&meaning),
            &["unrelated.example".to_string()],
        )
        .unwrap()
        .unwrap();
    assert_eq!(signals.words[..2], ["garden", "spades"]);
    assert_eq!(signals.docs, 4);
    assert_eq!(
        signals.df[field("title")][0],
        1,
        "only one title says garden"
    );
    assert!(signals.avg_len[field("title")] > 0.0);
    let site = |domain| {
        signals
            .sites
            .iter()
            .find(|site| site.domain == domain)
            .unwrap_or_else(|| panic!("{domain} is no candidate"))
    };
    let toolshed = site("toolshed.example");
    assert!(
        toolshed.tf.contains(&(0, field("title"), 1)),
        "{toolshed:?}"
    );
    assert!(
        toolshed.tf.contains(&(1, field("description"), 1)),
        "{toolshed:?}"
    );
    assert!(toolshed.words_rank.is_some() && toolshed.all_rank.is_some());
    assert!(toolshed.bm25 > 0.0);
    assert_eq!(toolshed.len[field("title")], 4);
    let spades = site("spades.example");
    assert!(spades.tf.contains(&(1, field("title"), 1)), "{spades:?}");
    assert!(spades.all_rank.is_some());
    let far = site("faraway.example");
    assert_eq!((far.meaning_rank, far.closeness), (Some(1), Some(0.4)));
    assert_eq!((far.words_rank, far.bm25), (None, 0.0));
    assert!(far.tf.is_empty());
    let asked = site("unrelated.example");
    assert!(asked.tf.is_empty() && asked.words_rank.is_none() && asked.meaning_rank.is_none());
}
