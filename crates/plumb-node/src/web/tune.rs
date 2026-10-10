//! Tuning: the searcher goes through a few searches picked at random, on
//! the real results page in edit mode, and moves results up or down or
//! hides them, and folds or keeps the boxes. Each result has traits (see
//! [`crate::learn::Trait`]): official sites or encyclopedias, forums or
//! code, small sites or well-known ones. What they do to results teaches
//! the node which traits they like, and every search then leans their
//! way. At the end the node says what it learned.
//!
//! - `GET /tune` starts: it sends the browser to the first search, as
//!   `/search?q=...&edit=1&tune=<seed>.<step>`,
//! - `GET /tune?done=1` says what was learned.
//!
//! Kept with the browser's history on the node (see [`crate::history`]),
//! so only on nodes that keep history. The searches gone through are not
//! kept as past searches.

use std::fmt::Write as _;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use plumb_index::SearchOptions;
use serde::Deserialize;

use super::history::Visitor;
use super::{escape_html, html_response, page, search_link, AppState};
use crate::learn::{Learned, Trait};

/// Searches picked from, of many kinds.
const SEARCHES: &[&str] = &[
    "python sort a list",
    "rust error handling",
    "pizza in denver",
    "hotels in chicago",
    "tacos in austin",
    "taylor swift",
    "the godfather",
    "us bank",
    "how to fix a leaky faucet",
    "best running shoes",
    "climate change",
    "world cup",
    "linux",
    "iphone",
    "chocolate chip cookie recipe",
    "learn spanish",
    "tax forms",
    "nasa",
    "bitcoin",
    "minecraft",
    "electric cars",
    "harvard",
    "javascript fetch api",
    "denver brewery",
];
/// Searches gone through in one round.
pub(super) const ROUND: usize = 5;

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    router.route("/tune", get(tune_page))
}

/// Where a round of tuning is: which searches (`seed`) and how far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Tuning {
    pub seed: u32,
    /// From 0.
    pub step: usize,
}

impl Tuning {
    /// Reads `<seed>.<step>`.
    pub fn parse(value: &str) -> Option<Tuning> {
        let (seed, step) = value.split_once('.')?;
        let tuning = Tuning {
            seed: seed.parse().ok()?,
            step: step.parse().ok()?,
        };
        (tuning.step < ROUND).then_some(tuning)
    }

    pub fn value(self) -> String {
        format!("{}.{}", self.seed, self.step)
    }

    /// The results page of this step, in edit mode.
    fn link(self, options: &SearchOptions) -> String {
        let query = round(self.seed)[self.step];
        format!(
            "{}&edit=1&tune={}",
            search_link("/search", query, options, false),
            self.value()
        )
    }

    /// The bar on top of a results page while tuning: how far, how, and
    /// the way on.
    pub fn bar(self, options: &SearchOptions) -> String {
        let next = if self.step + 1 < ROUND {
            let next = Tuning {
                step: self.step + 1,
                ..self
            };
            format!(
                "<a class=\"tnext\" href=\"{}\">Next search &rarr;</a>",
                escape_html(&next.link(options))
            )
        } else {
            "<a class=\"tnext\" href=\"/tune?done=1\">See what Plumb learned &rarr;</a>".to_owned()
        };
        format!(
            "<div class=\"tbar\"><div class=\"th\"><strong>Tuning your search</strong> \
             <span>{} of {ROUND}</span></div>\
             <p>Move results <strong>up</strong> or <strong>down</strong>, or \
             <strong>hide</strong> them, and fold the boxes you don't want. Plumb learns \
             which kinds of result you like, for every search.</p>\
             <div class=\"tgo\">{next}<a href=\"/tune?done=1\">Finish</a></div></div>\n",
            self.step + 1
        )
    }
}

/// The `ROUND` searches of a round, picked from [`SEARCHES`] by `seed`.
fn round(seed: u32) -> Vec<&'static str> {
    let mut order: Vec<(u64, &str)> = SEARCHES
        .iter()
        .enumerate()
        .map(|(i, q)| {
            // splitmix64 of the seed and the place in the list.
            let mut x = (u64::from(seed) << 32 | i as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
            x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            x ^= x >> 31;
            (x, *q)
        })
        .collect();
    order.sort_by_key(|(key, _)| *key);
    order.into_iter().take(ROUND).map(|(_, q)| q).collect()
}

#[derive(Debug, Default, Deserialize)]
struct TuneParams {
    done: Option<String>,
}

/// `GET /tune`.
async fn tune_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<TuneParams>,
) -> Response {
    let Some(visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if super::flag(&params.done) {
        return super::history::no_store(html_response(
            StatusCode::OK,
            render_done(&visitor.history.learned),
        ));
    }
    let mut bytes = [0u8; 4];
    let _ = getrandom::fill(&mut bytes);
    let tuning = Tuning {
        seed: u32::from_le_bytes(bytes),
        step: 0,
    };
    let options = super::SearchParams::default().options(&state.settings, &headers);
    super::redirect(&tuning.link(&options))
}

/// The end of a round: what was learned, and the way on.
fn render_done(learned: &Learned) -> String {
    let bar = super::app_bar("", false);
    let mut body = format!(
        "<div class=\"wrap hist\">\n{bar}\n\
         <main>\n<h1>Tune your search</h1>\n",
    );
    let (liked, disliked) = learned.leanings();
    if liked.is_empty() && disliked.is_empty() {
        body.push_str(
            "<p>Nothing learned yet. Move results up or down, or hide them, over a few \
             searches. Once one kind of result clearly fares better or worse with you \
             than the rest do, Plumb says so here and leans that way.</p>\n",
        );
    } else {
        body.push_str("<h2>What your choices say</h2>\n<ul>\n");
        let list = |traits: &[Trait]| {
            traits
                .iter()
                .map(|t| t.label())
                .collect::<Vec<_>>()
                .join(", ")
        };
        if !liked.is_empty() {
            let _ = writeln!(
                body,
                "<li>You like <strong>{}</strong>: they come up a little higher.</li>",
                list(&liked)
            );
        }
        if !disliked.is_empty() {
            let _ = writeln!(
                body,
                "<li>You'd rather not see <strong>{}</strong>: they go a little lower.</li>",
                list(&disliked)
            );
        }
        body.push_str("</ul>\n");
    }
    body.push_str(
        "<p><a href=\"/tune\">Tune with a few more searches</a> &middot; \
         <a href=\"/\">Go search</a></p>\n\
         <p class=\"s\">Kept on this node for this browser only. \
         <a href=\"/history#learned\">Everything learned, and how to forget it</a></p>\n\
         </main>\n</div>",
    );
    page("Tune your search - Plumb Search", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_round_is_the_same_searches_for_its_seed_without_repeats() {
        let picked = round(42);
        assert_eq!(picked.len(), ROUND);
        assert_eq!(picked, round(42));
        assert_ne!(picked, round(43));
        for (i, q) in picked.iter().enumerate() {
            assert!(!picked[..i].contains(q));
        }
    }

    #[test]
    fn steps_go_on_to_the_end() {
        let tuning = Tuning::parse("7.0").unwrap();
        assert_eq!(tuning.value(), "7.0");
        assert_eq!(Tuning::parse("7.5"), None);
        assert_eq!(Tuning::parse("x.1"), None);
        let options = SearchOptions::default();
        let first = tuning.bar(&options);
        assert!(first.contains("1 of 5"), "{first}");
        assert!(first.contains("&amp;edit=1&amp;tune=7.1"), "{first}");
        let last = Tuning { seed: 7, step: 4 }.bar(&options);
        assert!(
            last.contains("href=\"/tune?done=1\">See what Plumb learned"),
            "{last}"
        );
    }

    #[test]
    fn the_end_says_what_was_learned() {
        let mut learned = Learned::default();
        let results = [vec![Trait::Code], vec![Trait::Social], vec![], vec![]];
        for search in 0..8 {
            learned.note_judged(&format!("search {search}"), &results);
            learned.rate(&results[0], crate::learn::Rating::Like, 1.0);
            learned.rate(&results[1], crate::learn::Rating::Dislike, 1.0);
        }
        let page = render_done(&learned);
        assert!(
            page.contains("You like <strong>code and software docs"),
            "{page}"
        );
        assert!(
            page.contains("rather not see <strong>social media"),
            "{page}"
        );
        assert!(render_done(&Learned::default()).contains("Nothing learned yet"));
    }
}
