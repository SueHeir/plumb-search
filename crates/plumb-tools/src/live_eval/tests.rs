//! Tests of `live-eval`'s reading of a node's answer. They ask no node:
//! the answers here are the shape [docs/searching.md](../../../../docs/searching.md)
//! describes, cut to what this module reads.

use super::*;
use crate::cli::Half;
use serde_json::json;

fn want(answers: &[&str]) -> Vec<String> {
    answers.iter().map(ToString::to_string).collect()
}

#[test]
fn a_site_row_counts_at_its_place() {
    let answer = json!({"assembled": {"rows": [
        {"kind": "site", "site": {"domain": "www.chase.com", "url": "https://www.chase.com/"}},
        {"kind": "site", "site": {"domain": "chasebank.co.uk", "url": "https://chasebank.co.uk/"}},
    ]}});
    assert_eq!(found_in(&answer, &want(&["chase.com"])), Found::Row(1));
    assert_eq!(
        found_in(&answer, &want(&["chasebank.co.uk"])),
        Found::Row(2)
    );
    assert_eq!(
        found_in(&answer, &want(&["wellsfargo.com"])),
        Found::Nowhere
    );
}

#[test]
fn a_page_row_counts_by_its_address_in_either_form() {
    let item = json!({"kind": "page", "page": {"item":
        {"url": "https://en.wikipedia.org/wiki/Marie_Curie", "title": "Marie Curie"}, "set": "wikipedia-en"}});
    let older = json!({"kind": "page", "page":
        {"page": {"url": "https://en.wikipedia.org/wiki/Marie_Curie"}}});
    for row in [item, older] {
        let answer = json!({"assembled": {"rows": [row]}});
        assert_eq!(
            found_in(
                &answer,
                &want(&["https://en.wikipedia.org/wiki/Marie_Curie"])
            ),
            Found::Row(1)
        );
    }
}

#[test]
fn an_answer_ending_in_a_star_takes_any_page_under_it() {
    let answer = json!({"assembled": {"rows": [{"kind": "page", "page": {"item":
        {"url": "https://diy.stackexchange.com/questions/13279"}}}]}});
    assert_eq!(
        found_in(
            &answer,
            &want(&["https://diy.stackexchange.com/questions/*"])
        ),
        Found::Row(1)
    );
}

#[test]
fn a_page_past_the_rows_is_deeper_and_a_panel_is_a_panel() {
    let answer = json!({
        "assembled": {"rows": [
            {"kind": "site", "site": {"domain": "dunemovie.net", "url": "https://www.dunemovie.net/"}},
        ]},
        "pages": [{"item": {"url": "https://en.wikipedia.org/wiki/Dune_(2021_film)"}, "set": "films"}],
        "profile": {"of": "Dancing Queen", "service": "Genius",
                    "url": "https://genius.com/Abba-dancing-queen-lyrics"},
    });
    assert_eq!(
        found_in(
            &answer,
            &want(&["https://en.wikipedia.org/wiki/Dune_(2021_film)"])
        ),
        Found::Deeper
    );
    assert_eq!(
        found_in(
            &answer,
            &want(&["https://genius.com/Abba-dancing-queen-lyrics"])
        ),
        Found::Panel
    );
    // A row still wins over both.
    assert_eq!(found_in(&answer, &want(&["dunemovie.net"])), Found::Row(1));
}

#[test]
fn a_fact_answer_is_read_without_commas_or_case() {
    let answer = json!({"answer": {"kind": "fact", "question": "Elevation of Mount Kilimanjaro",
                                   "answer": "5,895 m (19,341 ft)"}});
    assert!(answers_fact(&answer, &want(&["5895"])));
    assert!(!answers_fact(&answer, &want(&["5900"])));
    let none = json!({"answer": Value::Null});
    assert!(!answers_fact(&none, &want(&["5895"])));
}

#[test]
fn totals_count_each_outcome_once() {
    let mut file = FileTotals::default();
    for found in [
        Found::Row(1),
        Found::Row(3),
        Found::Row(7),
        Found::Deeper,
        Found::Panel,
        Found::Nowhere,
    ] {
        file.count(found);
    }
    assert_eq!(file.asked, 6);
    assert_eq!((file.top1, file.top3), (1, 2));
    assert_eq!((file.deeper, file.panel, file.nowhere), (1, 1, 1));
}

#[test]
fn a_sample_spreads_over_the_whole_file() {
    let queries: Vec<EvalQuery> = (0..10)
        .map(|i| EvalQuery {
            line: i + 1,
            query: format!("query {i}"),
            expected: vec!["example.com".into()],
        })
        .collect();
    let args = LiveEvalArgs {
        node: "http://127.0.0.1:8080".into(),
        queries: Vec::new(),
        limit: Some(3),
        half: None,
        facts: false,
        misses: false,
    };
    let picked = chosen(queries.clone(), &args);
    assert_eq!(picked.iter().map(|q| q.line).collect::<Vec<_>>(), [1, 4, 7]);
    // No limit keeps every query, and a limit past the end keeps them too.
    let all = LiveEvalArgs {
        limit: None,
        ..args.clone()
    };
    assert_eq!(chosen(queries.clone(), &all).len(), 10);
    let many = LiveEvalArgs {
        limit: Some(50),
        ..args
    };
    assert_eq!(chosen(queries, &many).len(), 10);
}

#[test]
fn only_one_half_of_the_queries_is_asked() {
    let queries: Vec<EvalQuery> = ["chase", "paypal", "wikipedia", "github", "reddit", "nasa"]
        .iter()
        .enumerate()
        .map(|(i, query)| EvalQuery {
            line: i + 1,
            query: (*query).to_string(),
            expected: vec![format!("{query}.com")],
        })
        .collect();
    let args = LiveEvalArgs {
        node: "http://127.0.0.1:8080".into(),
        queries: Vec::new(),
        limit: None,
        half: Some(Half::Tune),
        facts: false,
        misses: false,
    };
    let tune = chosen(queries.clone(), &args);
    let held_out = chosen(
        queries.clone(),
        &LiveEvalArgs {
            half: Some(Half::HeldOut),
            ..args
        },
    );
    assert_eq!(tune.len() + held_out.len(), queries.len());
    assert!(tune.iter().all(|q| half_of(&q.query) == Half::Tune));
}
