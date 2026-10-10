//! The About you page, and its first-run version, the welcome page (Liz,
//! 2026-10-07: "some startup page that lets you enter in all this
//! information on what you care about, include a city, would be super
//! helpful to make search better"; then "welcome and about seem like the
//! same page", so they are one).
//!
//! - `GET /about` shows what the browser told the node about its searcher
//!   (see [`crate::about`]): their city, what they are into (a few dozen
//!   topics to tick, and their own), sites always first and never shown,
//!   and how much of each kind of result to list. It is kept where
//!   [`KeptAbout`] keeps it.
//! - `GET /welcome` is the same page for a new searcher: a welcome, and
//!   "Save and start searching" next to "Skip for now".
//! - `POST /about` saves it. From the welcome page it goes on to the home
//!   page, unless there is something to say (the city was not found).
//! - `POST /welcome/skip` goes back to the home page.
//!
//! Until the browser does either, or saves an About profile, the home page
//! invites it to the welcome page in one line (see [`invite_html`]).

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Form, Router};

use super::history::{
    cross_site, kinds_html, no_store, refuse_cross_site, welcomed_cookie, KeptAbout, SOME_LEFT_OUT,
};
use super::{escape_html, html_response, page, redirect, run_places, AppState};
use crate::about::{About, Amount, MAX_INTERESTS, MAX_SITES, MAX_TOWN_CHARS};

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
        .route("/about", get(about_page).post(save))
        .route("/welcome", get(welcome_page).post(save))
        .route("/welcome/skip", post(skip))
}

/// The home page's line inviting a new searcher to the welcome page.
pub(super) fn invite_html() -> &'static str {
    "<div class=\"invite s\"><a href=\"/welcome\">Tell Plumb your city and what you\u{2019}re \
     into</a> to fit results to you. \
     <form method=\"post\" action=\"/welcome/skip\"><button type=\"submit\">No \
     thanks</button></form></div>"
}

/// `GET /about`.
async fn about_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let kept = KeptAbout::of(&state, &headers);
    no_store(html_response(
        StatusCode::OK,
        render_about(&kept.about, kept.in_browser(), false, None),
    ))
}

/// `GET /welcome`.
async fn welcome_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let kept = KeptAbout::of(&state, &headers);
    no_store(html_response(
        StatusCode::OK,
        render_about(&kept.about, kept.in_browser(), true, None),
    ))
}

/// `POST /about`: the form's fields, some of them repeated (each topic
/// ticked is a `t`). `welcome=1` when it came from the welcome page,
/// `clear=1` to forget it all.
async fn save(
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
    let flag = |name: &'static str| field(name).any(|value| value == "1");
    let welcome = flag("welcome");
    let about = if flag("clear") {
        About::default()
    } else {
        let mut interests: Vec<&str> = field("t").filter(|t| TOPICS.contains(t)).collect();
        // `interests`: the one box the page had before it had topics.
        interests.extend(field("more").chain(field("interests")));
        let kinds = fields
            .iter()
            .filter_map(|(key, value)| Some((key.strip_prefix("kind-")?, Amount::parse(value)?)));
        About::from_form(
            &interests.join("\n"),
            &field("pinned").collect::<Vec<_>>().join("\n"),
            &field("hidden").collect::<Vec<_>>().join("\n"),
        )
        .with_town(field("town").next().unwrap_or_default())
        .with_kinds(kinds)
    };
    let mut kept = KeptAbout::of(&state, &headers);
    let fit = match kept.save(about) {
        Ok(fit) => fit,
        Err(err) => {
            tracing::warn!("could not save an About profile: {err:#}");
            let page = render_about(
                &kept.about,
                kept.in_browser(),
                welcome,
                Some("This could not be saved. The server log has the details."),
            );
            return no_store(html_response(StatusCode::INTERNAL_SERVER_ERROR, page));
        }
    };
    let town = match kept.about.town() {
        Some(town) => town_found(&state, town).await,
        None => Town::NotGiven,
    };
    let mut note = String::from("Saved.");
    if !fit {
        note.push_str(SOME_LEFT_OUT);
    }
    match &town {
        Town::Found(name) => {
            note.push_str(&format!(
                " Searches for places near you look around {name}."
            ));
        }
        Town::Unknown => {
            note.push(' ');
            note.push_str(&unknown_town(&kept.about.town));
        }
        Town::NotGiven | Town::NoPlaces => {}
    }
    // From the welcome page, on to searching, unless the searcher should
    // read something first.
    let response = if welcome && fit && !matches!(town, Town::Unknown) {
        redirect("/")
    } else {
        no_store(html_response(
            StatusCode::OK,
            render_about(&kept.about, kept.in_browser(), welcome, Some(&note)),
        ))
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

fn render_about(about: &About, in_browser: bool, welcome: bool, note: Option<&str>) -> String {
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
        "Kept in this browser only, in a cookie it sends with each search here. This server \
         uses it for that search and keeps no copy."
    } else {
        "Kept on this node for this browser only, and used only here, after results are \
         found. Other people searching here have their own."
    };
    let (title, heading, intro, actions) = if welcome {
        (
            "Welcome - Plumb Search",
            "Welcome to Plumb",
            "Tell Plumb a little about you and it fits results to you. All of it is optional, \
             and you can change it any time under About you in the search settings. ",
            "<input type=\"hidden\" name=\"welcome\" value=\"1\">\
             <div class=\"acts\"><button type=\"submit\">Save and start searching</button>\
             <button type=\"submit\" class=\"quiet\" formaction=\"/welcome/skip\">Skip for \
             now</button></div>\n</form>\n"
                .to_owned(),
        )
    } else {
        let links = if in_browser {
            ""
        } else {
            "<a href=\"/history\">Your history</a> \u{b7} <a href=\"/link\">Use this on your \
             other computers</a>"
        };
        (
            "About you - Plumb Search",
            "About you",
            "",
            format!(
                "<div class=\"acts\"><button type=\"submit\">Save</button>\
                 <button type=\"submit\" class=\"quiet\" name=\"clear\" value=\"1\">Forget all \
                 of this</button></div>\n</form>\n<p class=\"m\">{links}</p>\n"
            ),
        )
    };
    let bar = super::app_bar("", false);
    let body = format!(
        "<div class=\"wrap hist about welcome\">\n{bar}\n<main>\n\
         <h1>{heading}</h1>\n\
         <p class=\"s\">{intro}{kept} It is never part of a search sent to other Plumb \
         nodes.</p>\n{note}\
         <form method=\"post\" action=\"/about\">\n\
         <label for=\"town\"><strong>Your city</strong></label>\n\
         <p class=\"m\">Such as Denver, CO. Searches like \u{201c}coffee near me\u{201d} list \
         places there. Plumb never works out where you are by itself.</p>\n\
         <input id=\"town\" name=\"town\" maxlength=\"{MAX_TOWN_CHARS}\" value=\"{}\" \
         autocomplete=\"address-level2\">\n\
         <fieldset><legend><strong>What are you into?</strong></legend>\n\
         <p class=\"m\">Results that match one move up a little and say so, so a name like \
         \u{201c}jaguar\u{201d} or \u{201c}rust\u{201d} leans your way.</p>\n\
         <div class=\"topics\">{topics}</div></fieldset>\n\
         <label for=\"more\"><strong>Anything else you care about</strong></label>\n\
         <p class=\"m\">Your team, your school, a hobby: one per line. Up to \
         {MAX_INTERESTS} interests in all.</p>\n\
         <textarea id=\"more\" name=\"more\" rows=\"3\">{}</textarea>\n\
         <label for=\"pinned\"><strong>Sites you like</strong></label>\n\
         <p class=\"m\">One per line, such as seriouseats.com. They come first whenever a \
         search finds them. Up to {MAX_SITES}.</p>\n\
         <textarea id=\"pinned\" name=\"pinned\" rows=\"3\">{}</textarea>\n\
         <label for=\"hidden\"><strong>Sites never shown</strong></label>\n\
         <p class=\"m\">One per line. They and their subdomains are left out of your \
         results. Up to {MAX_SITES}.</p>\n\
         <textarea id=\"hidden\" name=\"hidden\" rows=\"3\">{}</textarea>\n\
         {}{actions}</main>\n</div>",
        escape_html(&about.town),
        escape_html(&more.join("\n")),
        escape_html(&about.pinned.join("\n")),
        escape_html(&about.hidden.join("\n")),
        kinds_html(about),
    );
    page(title, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_welcome_page_ticks_what_was_saved_and_escapes_the_rest() {
        let about = About::from_form("Cooking\n<b>my team</b>", "seriouseats.com", "")
            .with_town("Denver, CO");
        let page = render_about(&about, true, true, None);
        assert!(page.contains("value=\"cooking\" checked"));
        assert!(page.contains("value=\"music\">"));
        assert!(page.contains(">&lt;b&gt;my team&lt;/b&gt;</textarea>"));
        assert!(!page.contains("<b>my team"));
        assert!(page.contains("value=\"Denver, CO\""));
        assert!(page.contains(">seriouseats.com</textarea>"));
        assert!(page.contains("keeps no copy"));
        assert!(page.contains("Skip for now"));
        let page = render_about(&about, false, false, Some("Saved."));
        assert!(page.contains("<h1>About you</h1>"));
        assert!(page.contains("name=\"hidden\""));
        assert!(page.contains("name=\"kind-"));
        assert!(page.contains("href=\"/link\""));
        assert!(!page.contains("Skip for now"));
    }

    #[test]
    fn every_topic_is_kept_as_offered() {
        let all = TOPICS.join("\n");
        assert_eq!(About::topics_from_text(&all), TOPICS);
        assert!(TOPICS.len() <= crate::about::MAX_INTERESTS);
    }
}
