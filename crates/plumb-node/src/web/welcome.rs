//! The welcome page (Liz, 2026-10-07: "some startup page that lets you
//! enter in all this information on what you care about, include a city,
//! would be super helpful to make search better").
//!
//! - `GET /welcome` asks a new searcher for their city, what they are into
//!   (a few dozen topics to tick, and their own) and the sites they like.
//!   It fills the same About profile as `/about` (see [`crate::about`]),
//!   kept where that page keeps it (see [`KeptAbout`]).
//! - `POST /welcome` saves it and goes on to the home page, or stays to say
//!   that the city was not found.
//! - `POST /welcome/skip` goes back to the home page.
//!
//! Until the browser does either, or saves an About profile some other
//! way, the home page invites it here in one line (see [`invite_html`]).

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Form, Router};

use super::history::{
    cross_site, no_store, refuse_cross_site, welcomed_cookie, KeptAbout, SOME_LEFT_OUT,
};
use super::{escape_html, html_response, page, redirect, run_places, AppState};
use crate::about::{About, MAX_TOWN_CHARS};

/// Topics offered to tick, as they are kept: words that sites about them
/// say in their names and descriptions, since an interest matches a site
/// that has all its words.
pub(super) const TOPICS: &[&str] = &[
    "cooking",
    "programming",
    "machine learning",
    "games",
    "board games",
    "music",
    "movies",
    "television",
    "books",
    "anime",
    "sports",
    "football",
    "soccer",
    "basketball",
    "baseball",
    "running",
    "cycling",
    "hiking",
    "fitness",
    "health",
    "travel",
    "photography",
    "science",
    "space",
    "history",
    "politics",
    "finance",
    "cars",
    "gardening",
    "pets",
    "parenting",
    "fashion",
    "art",
];

pub(super) fn routes(router: Router<super::AppState>) -> Router<super::AppState> {
    router
        .route("/welcome", get(welcome_page).post(save_welcome))
        .route("/welcome/skip", post(skip))
}

/// The home page's line inviting a new searcher to the welcome page.
pub(super) fn invite_html() -> &'static str {
    "<div class=\"invite s\"><a href=\"/welcome\">Tell Plumb your city and what you\u{2019}re \
     into</a> to fit results to you. \
     <form method=\"post\" action=\"/welcome/skip\"><button type=\"submit\">No \
     thanks</button></form></div>"
}

/// `GET /welcome`.
async fn welcome_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let kept = KeptAbout::of(&state, &headers);
    no_store(html_response(
        StatusCode::OK,
        render_welcome(&kept.about, kept.in_browser(), None),
    ))
}

/// `POST /welcome`: the form's fields, some of them repeated (each topic
/// ticked is a `t`).
async fn save_welcome(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(fields): Form<Vec<(String, String)>>,
) -> Response {
    if cross_site(&headers) {
        return refuse_cross_site();
    }
    let field = |name: &'static str| {
        fields
            .iter()
            .filter(move |(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let mut interests: Vec<&str> = field("t").filter(|t| TOPICS.contains(t)).collect();
    interests.extend(field("more"));
    let pinned: Vec<&str> = field("pinned").collect();
    let mut kept = KeptAbout::of(&state, &headers);
    // Sites never shown are not asked for here, so they stay as they were.
    let hidden = kept.about.hidden.join("\n");
    // Nor are kinds of results.
    let kinds = kept.about.kinds.clone();
    let about = About::from_form(&interests.join("\n"), &pinned.join("\n"), &hidden)
        .with_town(field("town").next().unwrap_or_default())
        .with_kinds(kinds.iter().map(|(kind, amount)| (kind.as_str(), *amount)));
    let fit = match kept.save(about) {
        Ok(fit) => fit,
        Err(err) => {
            tracing::warn!("could not save an About profile: {err:#}");
            let page = render_welcome(
                &kept.about,
                kept.in_browser(),
                Some("This could not be saved. The server log has the details."),
            );
            return no_store(html_response(StatusCode::INTERNAL_SERVER_ERROR, page));
        }
    };
    let town = match kept.about.town() {
        Some(town) => town_found(&state, town).await,
        None => Town::NotGiven,
    };
    let response = match (&town, fit) {
        (Town::Unknown, _) => {
            let note = format!("Saved, but {}", unknown_town(&kept.about.town));
            no_store(html_response(
                StatusCode::OK,
                render_welcome(&kept.about, kept.in_browser(), Some(&note)),
            ))
        }
        (_, false) => {
            let note = format!("Saved.{SOME_LEFT_OUT}");
            no_store(html_response(
                StatusCode::OK,
                render_welcome(&kept.about, kept.in_browser(), Some(&note)),
            ))
        }
        _ => redirect("/"),
    };
    with_welcomed(kept.send_cookies(response))
}

/// `POST /welcome/skip`.
async fn skip(headers: HeaderMap) -> Response {
    if cross_site(&headers) {
        return refuse_cross_site();
    }
    with_welcomed(redirect("/"))
}

fn with_welcomed(mut response: Response) -> Response {
    if let Ok(value) = HeaderValue::from_str(&welcomed_cookie()) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// Whether the node's places know a searcher's town.
enum Town {
    NotGiven,
    /// This node has no places to look in.
    NoPlaces,
    /// Found, by its name, region and country: "Denver, CO, US".
    Found(String),
    Unknown,
}

/// Looks `town` up the way a search for places "near me" does.
async fn town_found(state: &AppState, town: &str) -> Town {
    // Any kind of place does: only where it looks matters.
    let Some(found) = run_places(state, "cafe near me", Some(town), None).await else {
        return Town::NoPlaces;
    };
    match found.center {
        Some(center) => {
            let mut name = center.name;
            for part in [center.region, center.country].into_iter().flatten() {
                name.push_str(", ");
                name.push_str(&part);
            }
            Town::Found(name)
        }
        None => Town::Unknown,
    }
}

/// What to say about a town Plumb does not know.
fn unknown_town(town: &str) -> String {
    format!(
        "Plumb does not know a town called \u{201c}{town}\u{201d}. Try adding its state or \
         country, like Portland, Maine."
    )
}

/// What the About page says, once saved, about where "near me" looks:
/// empty when the node has no places to look in.
pub(super) async fn town_note(state: &AppState, town: &str) -> String {
    match town_found(state, town).await {
        Town::Found(name) => format!("Searches for places near you look around {name}."),
        Town::Unknown => unknown_town(town),
        Town::NotGiven | Town::NoPlaces => String::new(),
    }
}

fn render_welcome(about: &About, in_browser: bool, note: Option<&str>) -> String {
    let note = note
        .map(|note| {
            format!(
                "<p class=\"s\"><strong>{}</strong></p>\n",
                escape_html(note)
            )
        })
        .unwrap_or_default();
    let ticked = |topic: &str| {
        about
            .interests
            .iter()
            .any(|interest| interest.eq_ignore_ascii_case(topic))
    };
    let topics: String = TOPICS
        .iter()
        .map(|topic| {
            format!(
                "<label><input type=\"checkbox\" name=\"t\" value=\"{0}\"{1}> {0}</label>",
                escape_html(topic),
                if ticked(topic) { " checked" } else { "" }
            )
        })
        .collect();
    // Interests that are not among the topics are the searcher's own.
    let more: Vec<&str> = about
        .interests
        .iter()
        .filter(|interest| !TOPICS.iter().any(|t| interest.eq_ignore_ascii_case(t)))
        .map(String::as_str)
        .collect();
    let kept = if in_browser {
        "It is kept in this browser only, and this server keeps no copy."
    } else {
        "It is kept on this node for this browser only."
    };
    let body = format!(
        "<div class=\"wrap hist about welcome\">\n<header><a class=\"logo\" href=\"/\">Plumb</a></header>\n<main>\n\
         <h1>Welcome to Plumb</h1>\n\
         <p class=\"s\">Tell Plumb a little about you and it fits results to you. All of it is \
         optional, and you can change it any time under About you in the search settings. \
         {kept} It is never part of a search sent to other Plumb nodes.</p>\n{note}\
         <form method=\"post\" action=\"/welcome\">\n\
         <label for=\"town\"><strong>Your city</strong></label>\n\
         <p class=\"m\">Such as Denver, CO. Searches like \u{201c}coffee near me\u{201d} list \
         places there. Plumb never works out where you are by itself.</p>\n\
         <input id=\"town\" name=\"town\" maxlength=\"{MAX_TOWN_CHARS}\" value=\"{}\" \
         autocomplete=\"address-level2\">\n\
         <fieldset class=\"topics\"><legend><strong>What are you into?</strong></legend>\n\
         <p class=\"m\">Results that match one move up a little and say so, so a name like \
         \u{201c}jaguar\u{201d} or \u{201c}rust\u{201d} leans your way.</p>\n{topics}</fieldset>\n\
         <label for=\"more\"><strong>Anything else you care about</strong></label>\n\
         <p class=\"m\">Your team, your school, a hobby: one per line.</p>\n\
         <textarea id=\"more\" name=\"more\" rows=\"3\">{}</textarea>\n\
         <label for=\"pinned\"><strong>Sites you like</strong></label>\n\
         <p class=\"m\">One per line, such as seriouseats.com. They come first whenever a \
         search finds them.</p>\n\
         <textarea id=\"pinned\" name=\"pinned\" rows=\"3\">{}</textarea>\n\
         <p><button type=\"submit\">Save and start searching</button></p>\n</form>\n\
         <form method=\"post\" action=\"/welcome/skip\"><button type=\"submit\">Skip for \
         now</button></form>\n</main>\n</div>",
        escape_html(&about.town),
        escape_html(&more.join("\n")),
        escape_html(&about.pinned.join("\n")),
    );
    page("Welcome - Plumb Search", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_welcome_page_ticks_what_was_saved_and_escapes_the_rest() {
        let about = About::from_form("Cooking\n<b>my team</b>", "seriouseats.com", "")
            .with_town("Denver, CO");
        let page = render_welcome(&about, true, None);
        assert!(page.contains("value=\"cooking\" checked"));
        assert!(page.contains("value=\"music\">"));
        assert!(page.contains(">&lt;b&gt;my team&lt;/b&gt;</textarea>"));
        assert!(!page.contains("<b>my team"));
        assert!(page.contains("value=\"Denver, CO\""));
        assert!(page.contains(">seriouseats.com</textarea>"));
        assert!(page.contains("this server keeps no copy"));
    }

    #[test]
    fn every_topic_is_kept_as_offered() {
        let all = TOPICS.join("\n");
        assert_eq!(About::topics_from_text(&all), TOPICS);
        assert!(TOPICS.len() <= crate::about::MAX_INTERESTS);
    }
}
