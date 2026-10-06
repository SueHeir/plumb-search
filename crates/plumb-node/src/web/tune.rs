//! Tuning: the searcher rates the results of a few searches picked at
//! random, and the node learns what they like to see in general (see
//! [`crate::learn::Trait`]): official sites or encyclopedias, forums or
//! code, small sites or well-known ones, and whether maps and headlines
//! help them. It then leans every search their way, and says what it
//! learned.
//!
//! - `GET /tune` shows a few searches to rate and what was learned so far,
//! - `POST /tune` takes the ratings.
//!
//! Kept with the browser's history on the node (see [`crate::history`]),
//! so only on nodes that keep history.

use std::fmt::Write as _;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Form, Router};
use plumb_core::{now_unix, truncate_chars};
use plumb_index::Hit;

use super::history::{
    cross_site, hit_traits, parse_traits, refuse_cross_site, traits_field, Visitor,
};
use super::{escape_html, html_response, page, run_places, run_search, AppState, SearchParams};
use crate::learn::{self, Block, Learned, Rating, Trait};

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
/// Searches shown at a time.
const SEARCHES_SHOWN: usize = 5;
/// Results rated per search.
const RESULTS_SHOWN: usize = 5;
/// Searches a form may rate, and results per search.
const MOST_RATED: usize = 10;

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    router.route("/tune", get(tune_page).post(save))
}

/// `SEARCHES_SHOWN` of the searches, picked at random.
fn pick() -> Vec<&'static str> {
    let mut order: Vec<(u32, &str)> = SEARCHES
        .iter()
        .map(|q| {
            let mut bytes = [0u8; 4];
            let _ = getrandom::fill(&mut bytes);
            (u32::from_le_bytes(bytes), *q)
        })
        .collect();
    order.sort_by_key(|(r, _)| *r);
    order
        .into_iter()
        .take(SEARCHES_SHOWN)
        .map(|(_, q)| q)
        .collect()
}

/// One search to rate: its results and the boxes it shows.
struct ToRate {
    query: String,
    hits: Vec<Hit>,
    /// The places box, and how it was shown.
    places: Option<&'static str>,
    /// The "Recent" box, and how it was shown.
    news: Option<&'static str>,
}

/// `GET /tune`.
async fn tune_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let params = SearchParams::default();
    let options = params.options(&state.settings.home, &headers);
    let mut searches = Vec::new();
    for query in pick() {
        let Ok(results) = run_search(&state, query, RESULTS_SHOWN, &options).await else {
            continue;
        };
        let places = run_places(&state, query, None, options.country.as_deref())
            .await
            .filter(|found| found.center.is_some() && !found.hits.is_empty())
            .filter(|found| !(found.guessed && results.hits.iter().any(|h| h.named)))
            .map(|found| learn::places_context(found.guessed));
        let news = state
            .recent(query, &results)
            .map(|recent| learn::news_context(recent.site.is_some()));
        searches.push(ToRate {
            query: query.to_owned(),
            hits: results.hits,
            places,
            news,
        });
    }
    let home = options.country.as_deref();
    super::history::no_store(html_response(
        StatusCode::OK,
        render_tune(&visitor.history.learned, &searches, home, None),
    ))
}

/// `POST /tune`: fields `r<i>_<j>` = `like|traits`, `neither|traits` or
/// `dislike|traits` for each result, and `b<i>_<block>` = `yes|context` or
/// `no|context` for each box.
async fn save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(fields): Form<Vec<(String, String)>>,
) -> Response {
    if cross_site(&headers) {
        return refuse_cross_site();
    }
    let Some(mut visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ratings = read_form(&fields);
    let rated = ratings.results.len();
    let saved = visitor.change_learned(|learned| apply(learned, &ratings, now_unix()));
    let note = if !saved {
        "Your ratings could not be saved. The server log has the details.".to_owned()
    } else if rated == 0 && ratings.boxes.is_empty() {
        "Nothing was rated.".to_owned()
    } else {
        format!(
            "Thanks: {rated} result{} rated. Rate more below, or go search.",
            if rated == 1 { "" } else { "s" }
        )
    };
    let response = super::history::no_store(html_response(
        StatusCode::OK,
        render_tune(&visitor.history.learned, &[], None, Some(&note)),
    ));
    visitor.send_cookies(response)
}

/// What a tuning form said.
#[derive(Debug, Default, PartialEq)]
struct Ratings {
    results: Vec<(Rating, Vec<Trait>)>,
    boxes: Vec<(Block, String, bool)>,
}

fn read_form(fields: &[(String, String)]) -> Ratings {
    let mut ratings = Ratings::default();
    for (name, value) in fields {
        let Some((how, what)) = value.split_once('|') else {
            continue;
        };
        if name.starts_with('r') && ratings.results.len() < MOST_RATED * MOST_RATED {
            let rating = match how {
                "like" => Rating::Like,
                "dislike" => Rating::Dislike,
                "neither" => Rating::Neither,
                _ => continue,
            };
            let traits = parse_traits(what);
            if !traits.is_empty() {
                ratings.results.push((rating, traits));
            }
        } else if name.starts_with('b') && ratings.boxes.len() < MOST_RATED * 2 {
            let block = if name.ends_with("_places") {
                Block::Places
            } else if name.ends_with("_news") {
                Block::News
            } else {
                continue;
            };
            if !matches!(what, "said" | "guessed" | "site" | "words") {
                continue;
            }
            match how {
                "yes" => ratings.boxes.push((block, what.to_owned(), true)),
                "no" => ratings.boxes.push((block, what.to_owned(), false)),
                _ => {}
            }
        }
    }
    ratings
}

fn apply(learned: &mut Learned, ratings: &Ratings, now: u64) {
    for (rating, traits) in &ratings.results {
        learned.rate(traits, *rating, 1.0);
    }
    for (block, context, useful) in &ratings.boxes {
        learned.rate_block(*block, context, *useful, now);
    }
}

fn render_tune(
    learned: &Learned,
    searches: &[ToRate],
    home: Option<&str>,
    note: Option<&str>,
) -> String {
    let mut body = String::from(
        "<div class=\"wrap hist tune\">\n<header><a class=\"logo\" href=\"/\">Plumb</a></header>\n\
         <main>\n<h1>Tune your search</h1>\n\
         <p class=\"s\">Rate the results of a few searches, and Plumb learns what you like to \
         see: official sites or encyclopedias, forums or code, small sites or well-known ones, \
         and whether maps and headlines help you. Every search then leans your way. It is \
         kept on this node for this browser only, and never sent anywhere.</p>\n",
    );
    if let Some(note) = note {
        let _ = writeln!(body, "<p class=\"op\">{}</p>", escape_html(note));
    }
    render_leanings(&mut body, learned);
    if searches.is_empty() {
        body.push_str(
            "<p><a href=\"/tune\">Rate a few more searches</a> &middot; \
             <a href=\"/\">Search</a></p>\n",
        );
    } else {
        body.push_str("<form method=\"post\" action=\"/tune\">\n");
        for (i, search) in searches.iter().enumerate() {
            render_search(&mut body, i, search, home);
        }
        body.push_str(
            "<p><button type=\"submit\">Save my ratings</button> \
             <a href=\"/tune\">Other searches</a></p>\n</form>\n",
        );
    }
    body.push_str(
        "<p class=\"s\"><a href=\"/history#learned\">Everything learned, and how to forget \
         it</a></p>\n</main>\n</div>",
    );
    page("Tune your search - Plumb Search", &body)
}

/// "What your ratings say": the kinds of result liked and disliked.
fn render_leanings(body: &mut String, learned: &Learned) {
    let (liked, disliked) = learned.leanings();
    if liked.is_empty() && disliked.is_empty() {
        return;
    }
    let list = |traits: &[Trait]| {
        traits
            .iter()
            .map(|t| t.label())
            .collect::<Vec<_>>()
            .join(", ")
    };
    body.push_str("<h2>What your ratings say</h2>\n<ul>\n");
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

fn render_search(body: &mut String, i: usize, search: &ToRate, home: Option<&str>) {
    let _ = writeln!(
        body,
        "<fieldset class=\"tr\"><legend>{}</legend>",
        escape_html(&search.query)
    );
    if search.hits.is_empty() {
        body.push_str("<p class=\"s\">This node has no results for it.</p>\n");
    }
    for (j, hit) in search.hits.iter().enumerate() {
        let traits = hit_traits(hit, home);
        let value = traits_field(&traits);
        let name = hit
            .title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(&hit.domain);
        let kinds = traits
            .iter()
            .map(|t| t.label())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(
            body,
            "<div class=\"tri\"><div><strong>{}</strong> <span class=\"m\">{}</span>",
            escape_html(&truncate_chars(name, 100)),
            escape_html(&hit.domain)
        );
        if let Some(description) = hit.description.as_deref().filter(|d| !d.trim().is_empty()) {
            let _ = write!(
                body,
                "<div class=\"m\">{}</div>",
                escape_html(&truncate_chars(description, 160))
            );
        }
        if !kinds.is_empty() {
            let _ = write!(body, "<div class=\"m\">{kinds}</div>");
        }
        body.push_str("</div><div class=\"tro\">");
        if traits.is_empty() {
            body.push_str("<span class=\"m\">Nothing to learn from this one</span>");
        } else {
            for (how, label, checked) in [
                ("like", "Good", false),
                ("neither", "Fine", true),
                ("dislike", "Not this", false),
            ] {
                let _ = write!(
                    body,
                    "<label><input type=\"radio\" name=\"r{i}_{j}\" value=\"{how}|{value}\"{}> \
                     {label}</label>",
                    if checked { " checked" } else { "" }
                );
            }
        }
        body.push_str("</div></div>\n");
    }
    for (block, context, question) in [
        (
            Block::Places,
            search.places,
            "It shows places and a map. Useful here?",
        ),
        (
            Block::News,
            search.news,
            "It shows recent headlines. Useful here?",
        ),
    ] {
        let Some(context) = context else {
            continue;
        };
        let _ = write!(
            body,
            "<div class=\"tri\"><div>{question}</div><div class=\"tro\">"
        );
        for (how, label) in [("yes", "Yes"), ("no", "No"), ("skip", "Not sure")] {
            let _ = write!(
                body,
                "<label><input type=\"radio\" name=\"b{i}_{}\" value=\"{how}|{context}\"{}> \
                 {label}</label>",
                block.as_str(),
                if how == "skip" { " checked" } else { "" }
            );
        }
        body.push_str("</div></div>\n");
    }
    body.push_str("</fieldset>\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searches_are_picked_at_random_without_repeats() {
        let picked = pick();
        assert_eq!(picked.len(), SEARCHES_SHOWN);
        for (i, q) in picked.iter().enumerate() {
            assert!(!picked[..i].contains(q));
        }
    }

    #[test]
    fn the_form_is_read_and_junk_left_out() {
        let fields: Vec<(String, String)> = [
            ("r0_0", "like|code,popular"),
            ("r0_1", "dislike|social"),
            ("r0_2", "neither|small"),
            ("r0_3", "love|code"),
            ("r0_4", "like|nonsense"),
            ("b0_places", "no|guessed"),
            ("b1_news", "yes|words"),
            ("b2_places", "skip|said"),
            ("b3_places", "no|<script>"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let ratings = read_form(&fields);
        assert_eq!(
            ratings.results,
            [
                (Rating::Like, vec![Trait::Code, Trait::Popular]),
                (Rating::Dislike, vec![Trait::Social]),
                (Rating::Neither, vec![Trait::Small]),
            ]
        );
        assert_eq!(
            ratings.boxes,
            [
                (Block::Places, "guessed".to_owned(), false),
                (Block::News, "words".to_owned(), true)
            ]
        );
        let mut learned = Learned::default();
        apply(&mut learned, &ratings, 1);
        assert!(learned.taste_score(&[Trait::Code]).0 > 0.0);
        let page = render_tune(&learned, &[], None, Some("Thanks"));
        assert!(
            page.contains("You like <strong>code and software docs"),
            "{page}"
        );
        assert!(
            page.contains("rather not see <strong>social media"),
            "{page}"
        );
    }
}
