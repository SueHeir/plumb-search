//! The search page's side of [`crate::history`]: who is searching (a
//! profile cookie), what they chose to keep, and the history page.
//!
//! - `GET /history` lists this browser's past searches and the sites it
//!   opened from them, newest first,
//! - `POST /history/clear` deletes them,
//! - `GET /about` shows what the browser told the node about its searcher
//!   (see [`crate::about`]) and `POST /about` changes it.
//!
//! The profile cookie is `SameSite=Lax`, so a page on another site cannot
//! post to `/history/clear` with it, and `HttpOnly`. The choices (show past
//! searches, rank opened sites higher) are a second cookie, set by the
//! settings gear's form; both are on until changed.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Form;
use axum::Router;
use plumb_core::{now_unix, truncate_chars};
use plumb_index::{Hit, SearchOptions};
use serde::Deserialize;
use tracing::warn;

use super::{escape_html, html_response, page, search_link, time_ago, AppState};
use crate::about::{About, AboutStore, Reason, MAX_INTERESTS, MAX_SITES, MAX_TOWN_CHARS};
use crate::history::{new_profile, valid_profile, History, HistoryStore};

/// The cookie holding a browser's profile id.
const PROFILE_COOKIE: &str = "plumb_profile";
/// The cookie holding a browser's history choices.
const PREFS_COOKIE: &str = "plumb_history";
/// A year, in seconds: how long the cookies last.
const COOKIE_SECONDS: u32 = 365 * 24 * 60 * 60;
/// Past searches shown on the home page.
const RECENT_ON_HOME: usize = 8;

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/history", get(history_page))
        .route("/history/clear", post(clear))
        .route("/about", get(about_page).post(save_about))
}

/// What a browser chose to do with its history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Prefs {
    /// Show past searches on the home page and label sites opened before.
    pub show: bool,
    /// Rank sites opened before higher.
    pub rank: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            show: true,
            rank: true,
        }
    }
}

impl Prefs {
    /// Whether anything is noted at all.
    pub fn on(self) -> bool {
        self.show || self.rank
    }

    fn cookie_value(self) -> &'static str {
        match (self.show, self.rank) {
            (true, true) => "s1r1",
            (true, false) => "s1r0",
            (false, true) => "s0r1",
            (false, false) => "s0r0",
        }
    }

    fn from_cookie(value: &str) -> Prefs {
        match value {
            "s1r0" => Prefs {
                show: true,
                rank: false,
            },
            "s0r1" => Prefs {
                show: false,
                rank: true,
            },
            "s0r0" => Prefs {
                show: false,
                rank: false,
            },
            _ => Prefs::default(),
        }
    }
}

/// The searcher's history, as the search pages use it.
pub(super) struct Visitor {
    store: HistoryStore,
    about_store: AboutStore,
    /// `None` until the browser searches with history on or saves an About
    /// profile.
    profile: Option<String>,
    pub prefs: Prefs,
    pub history: History,
    pub about: About,
    /// Cookies to send with the page.
    set_cookies: Vec<String>,
}

impl Visitor {
    /// The visitor of a request, on a node that keeps history; `None`
    /// otherwise. With `changed`, the gear's form just set new choices.
    pub fn of(state: &AppState, headers: &HeaderMap, changed: Option<Prefs>) -> Option<Visitor> {
        let store = state.node.as_ref()?.search_history()?;
        let cookies = cookies(headers);
        let profile = cookies
            .get(PROFILE_COOKIE)
            .filter(|id| valid_profile(id))
            .map(|id| (*id).to_owned());
        let mut set_cookies = Vec::new();
        let prefs = match changed {
            Some(prefs) => {
                set_cookies.push(cookie(PREFS_COOKIE, prefs.cookie_value()));
                prefs
            }
            None => cookies
                .get(PREFS_COOKIE)
                .map_or_else(Prefs::default, |v| Prefs::from_cookie(v)),
        };
        let about_store = AboutStore::new(store.dir());
        let (history, about) = match &profile {
            Some(profile) => (store.load(profile), about_store.load(profile)),
            None => (History::default(), About::default()),
        };
        Some(Visitor {
            store,
            about_store,
            profile,
            prefs,
            history,
            about,
            set_cookies,
        })
    }

    /// Notes a search, giving the browser a profile if it has none yet.
    /// The search itself is kept only while past searches are shown; with
    /// just ranking on, the browser still gets a profile for the sites it
    /// opens.
    pub fn note_search(&mut self, query: &str) {
        if !self.prefs.on() {
            return;
        }
        let Some(profile) = self.profile_or_new() else {
            return;
        };
        if !self.prefs.show {
            return;
        }
        let now = now_unix();
        self.history.add_search(query, now);
        if let Err(err) = self.store.update(&profile, |h| h.add_search(query, now)) {
            warn!("could not note a search in the history: {err:#}");
        }
    }

    /// Notes that `domain` was opened for `query`.
    pub fn note_opened(&mut self, query: &str, domain: &str) {
        if !self.prefs.on() {
            return;
        }
        // A browser opens results only from a page that gave it a profile.
        let Some(profile) = self.profile.clone() else {
            return;
        };
        let now = now_unix();
        if let Err(err) = self
            .store
            .update(&profile, |h| h.add_opened(query, domain, now))
        {
            warn!("could not note an opened site in the history: {err:#}");
        }
    }

    fn profile_or_new(&mut self) -> Option<String> {
        if self.profile.is_none() {
            match new_profile() {
                Ok(id) => {
                    self.set_cookies.push(cookie(PROFILE_COOKIE, &id));
                    self.profile = Some(id);
                }
                Err(err) => warn!("could not make a history profile: {err:#}"),
            }
        }
        self.profile.clone()
    }

    /// Applies the browser's About profile to `hits`, then its history.
    pub fn rank(&self, query: &str, hits: &mut Vec<Hit>) {
        self.about.apply(hits);
        self.rank_opened(query, hits);
    }

    /// Moves the sites opened before up `hits`, when the browser asked for
    /// that.
    fn rank_opened(&self, query: &str, hits: &mut [Hit]) {
        if !self.prefs.rank || self.history.opened.is_empty() {
            return;
        }
        let mut changed = false;
        for hit in hits.iter_mut() {
            let bonus = self.history.bonus(query, &hit.domain);
            if bonus > 0.0 {
                hit.score += bonus;
                changed = true;
            }
        }
        if changed {
            // Stable, so equal scores keep their order.
            hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        }
    }

    /// What the pages show of it.
    pub fn view(&self) -> HistoryView {
        let (opened, recent) = if self.prefs.show {
            (
                self.history
                    .opened
                    .iter()
                    .map(|o| o.domain.clone())
                    .collect(),
                self.history
                    .searches
                    .iter()
                    .take(RECENT_ON_HOME)
                    .map(|s| s.query.clone())
                    .collect(),
            )
        } else {
            (HashSet::new(), Vec::new())
        };
        HistoryView {
            prefs: self.prefs,
            opened,
            recent,
            about: self.about.clone(),
        }
    }

    /// Adds the cookies this visit set to `response`.
    pub fn send_cookies(&self, mut response: Response) -> Response {
        for value in &self.set_cookies {
            if let Ok(value) = HeaderValue::from_str(value) {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
        }
        response
    }
}

/// What the search pages show of a visitor's history.
#[derive(Debug, Clone, Default)]
pub(super) struct HistoryView {
    pub prefs: Prefs,
    /// Sites opened before, to label; empty unless shown.
    pub opened: HashSet<String>,
    /// The latest searches, newest first; empty unless shown.
    pub recent: Vec<String>,
    /// What the browser told the node about its searcher.
    pub about: About,
}

impl HistoryView {
    /// The gear's history choices.
    pub fn settings_html(&self) -> String {
        let checked = |on: bool| if on { " checked" } else { "" };
        format!(
            "<input type=\"hidden\" name=\"hist\" value=\"1\">\
             <label><input type=\"checkbox\" name=\"hs\" value=\"1\"{}> Show my past \
             searches</label>\
             <label><input type=\"checkbox\" name=\"hr\" value=\"1\"{}> Put sites I opened \
             before first</label>\
             <p class=\"hint\">Kept on this node for this browser only. \
             <a href=\"/history\">See or clear my history</a> \
             <a href=\"/about\">About you: interests and sites</a></p>",
            checked(self.prefs.show),
            checked(self.prefs.rank)
        )
    }

    /// The notes under a result about why it is where it is: opened
    /// before, put first, or matching an interest. HTML, escaped.
    pub fn notes(&self, hit: &Hit) -> Vec<String> {
        let mut notes = Vec::new();
        if self.opened.contains(&hit.domain) {
            notes.push("<span class=\"op\">You opened this before</span>".to_owned());
        }
        match self.about.reason(hit) {
            Some(Reason::Pinned) => {
                notes.push("<span class=\"op\">One of your sites</span>".to_owned());
            }
            Some(Reason::Interest(interest)) => notes.push(format!(
                "<span class=\"op\">Matches your interest: {}</span>",
                escape_html(interest)
            )),
            None => {}
        }
        notes
    }

    /// The home page's list of past searches, if any are shown.
    pub fn recent_html(&self, options: &SearchOptions) -> String {
        if self.recent.is_empty() {
            return String::new();
        }
        let mut out = String::from("<nav class=\"recent\" aria-label=\"Past searches\"><ul>");
        for query in &self.recent {
            let _ = write!(
                out,
                "<li><a href=\"{}\">{}</a></li>",
                escape_html(&search_link("/search", query, options, false)),
                escape_html(&truncate_chars(query, 60))
            );
        }
        out.push_str("</ul><a class=\"all\" href=\"/history\">History</a></nav>");
        out
    }
}

/// The gear's history choices in a search's parameters, if its form sent
/// them: `hist=1`, then `hs` and `hr` for the boxes ticked.
pub(super) fn prefs_from_form(
    hist: &Option<String>,
    show: &Option<String>,
    rank: &Option<String>,
) -> Option<Prefs> {
    super::flag(hist).then(|| Prefs {
        show: super::flag(show),
        rank: super::flag(rank),
    })
}

/// `GET /history`.
async fn history_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let page = render_history(&visitor.history, visitor.prefs, now_unix());
    let mut response = html_response(StatusCode::OK, page);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `POST /history/clear`.
async fn clear(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(profile) = &visitor.profile {
        if let Err(err) = visitor.store.clear(profile) {
            warn!("could not clear a search history: {err:#}");
            return html_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                page(
                    "History - Plumb Search",
                    "<main class=\"wrap\"><p class=\"none\">The history could not be cleared. \
                     The server log has the details.</p><p><a href=\"/history\">Back</a></p></main>",
                ),
            );
        }
    }
    super::redirect("/history")
}

fn render_history(history: &History, prefs: Prefs, now: u64) -> String {
    let options = SearchOptions::default();
    let mut body = String::from(
        "<div class=\"wrap hist\">\n<header><a class=\"logo\" href=\"/\">Plumb</a></header>\n<main>\n\
         <h1>Your history</h1>\n\
         <p class=\"s\">Kept on this node for this browser only. Other people searching here \
         have their own.</p>\n",
    );
    if !prefs.on() {
        body.push_str(
            "<p class=\"s\">Nothing new is noted: both history choices are off in the search \
             page's settings.</p>\n",
        );
    }
    if history.searches.is_empty() && history.opened.is_empty() {
        body.push_str("<p class=\"none\">No searches yet.</p>\n</main>\n</div>");
        return page("History - Plumb Search", &body);
    }
    if !history.opened.is_empty() {
        body.push_str("<h2>Sites you opened</h2>\n<ul>\n");
        for opened in &history.opened {
            let times = if opened.times == 1 {
                String::new()
            } else {
                format!(", {} times", opened.times)
            };
            let _ = writeln!(
                body,
                "<li><strong>{}</strong> for <a href=\"{}\">{}</a> \
                 <span class=\"m\">{}{times}</span></li>",
                escape_html(&opened.domain),
                escape_html(&search_link("/search", &opened.query, &options, false)),
                escape_html(&truncate_chars(&opened.query, 80)),
                time_ago(opened.at, now)
            );
        }
        body.push_str("</ul>\n");
    }
    if !history.searches.is_empty() {
        body.push_str("<h2>Searches</h2>\n<ul>\n");
        for search in &history.searches {
            let _ = writeln!(
                body,
                "<li><a href=\"{}\">{}</a> <span class=\"m\">{}</span></li>",
                escape_html(&search_link("/search", &search.query, &options, false)),
                escape_html(&truncate_chars(&search.query, 80)),
                time_ago(search.at, now)
            );
        }
        body.push_str("</ul>\n");
    }
    body.push_str(
        "<form method=\"post\" action=\"/history/clear\"><button type=\"submit\">Clear my \
         history</button></form>\n</main>\n</div>",
    );
    page("History - Plumb Search", &body)
}

/// The About page's form.
#[derive(Debug, Default, Deserialize)]
struct AboutForm {
    #[serde(default)]
    interests: String,
    #[serde(default)]
    pinned: String,
    #[serde(default)]
    hidden: String,
    #[serde(default)]
    town: String,
    /// `1`: forget it all.
    clear: Option<String>,
}

/// `GET /about`.
async fn about_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    no_store(html_response(
        StatusCode::OK,
        render_about(&visitor.about, None),
    ))
}

/// `POST /about`: saves the form, giving the browser a profile if it has
/// none yet.
async fn save_about(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AboutForm>,
) -> Response {
    let Some(mut visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let about = if super::flag(&form.clear) {
        About::default()
    } else {
        About::from_form(&form.interests, &form.pinned, &form.hidden).with_town(&form.town)
    };
    let saved = match (&visitor.profile, about.is_empty()) {
        // Nothing to save, and nothing saved before.
        (None, true) => Ok(()),
        _ => match visitor.profile_or_new() {
            Some(profile) => visitor.about_store.save(&profile, &about),
            None => Err(anyhow::anyhow!("no profile")),
        },
    };
    let (status, note) = match saved {
        Ok(()) => (StatusCode::OK, "Saved."),
        Err(err) => {
            warn!("could not save an About profile: {err:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "This could not be saved. The server log has the details.",
            )
        }
    };
    let response = no_store(html_response(status, render_about(&about, Some(note))));
    visitor.send_cookies(response)
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn render_about(about: &About, note: Option<&str>) -> String {
    let lines = |items: &[String]| escape_html(&items.join("\n"));
    let note = note
        .map(|note| {
            format!(
                "<p class=\"s\"><strong>{}</strong></p>\n",
                escape_html(note)
            )
        })
        .unwrap_or_default();
    let body = format!(
        "<div class=\"wrap hist about\">\n<header><a class=\"logo\" href=\"/\">Plumb</a></header>\n<main>\n\
         <h1>About you</h1>\n\
         <p class=\"s\">Kept on this node for this browser only, and used only here, after \
         results are found. It is never part of a search sent to other Plumb nodes. Other \
         people searching here have their own.</p>\n{note}\
         <form method=\"post\" action=\"/about\">\n\
         <label for=\"interests\"><strong>Your interests</strong></label>\n\
         <p class=\"m\">One per line, such as cooking or rust programming. Sites that match \
         one move up a little and say so, so a name like \u{201c}rust\u{201d} or \
         \u{201c}jaguar\u{201d} leans your way. Up to {MAX_INTERESTS}.</p>\n\
         <textarea id=\"interests\" name=\"interests\" rows=\"5\">{}</textarea>\n\
         <label for=\"pinned\"><strong>Sites always first</strong></label>\n\
         <p class=\"m\">One per line, such as seriouseats.com. They come first whenever a \
         search finds them. Up to {MAX_SITES}.</p>\n\
         <textarea id=\"pinned\" name=\"pinned\" rows=\"4\">{}</textarea>\n\
         <label for=\"hidden\"><strong>Sites never shown</strong></label>\n\
         <p class=\"m\">One per line. They and their subdomains are left out of your \
         results. Up to {MAX_SITES}.</p>\n\
         <textarea id=\"hidden\" name=\"hidden\" rows=\"4\">{}</textarea>\n\
         <label for=\"town\"><strong>Your town</strong></label>\n\
         <p class=\"m\">Such as Denver, CO. Searches like \u{201c}coffee near me\u{201d} \
         list places here. Plumb never works out where you are by itself.</p>\n\
         <input id=\"town\" name=\"town\" maxlength=\"{MAX_TOWN_CHARS}\" value=\"{}\">\n\
         <p><button type=\"submit\">Save</button></p>\n</form>\n\
         <form method=\"post\" action=\"/about\"><input type=\"hidden\" name=\"clear\" \
         value=\"1\"><button type=\"submit\">Forget all of this</button></form>\n\
         <p class=\"m\"><a href=\"/history\">Your history</a></p>\n</main>\n</div>",
        lines(&about.interests),
        lines(&about.pinned),
        lines(&about.hidden),
        escape_html(&about.town),
    );
    page("About you - Plumb Search", &body)
}

/// The cookies of a request, by name.
fn cookies(headers: &HeaderMap) -> HashMap<&str, &str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            Some((name.trim(), value.trim()))
        })
        .collect()
}

fn cookie(name: &str, value: &str) -> String {
    format!("{name}={value}; Path=/; Max-Age={COOKIE_SECONDS}; SameSite=Lax; HttpOnly")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_about_page_escapes_what_was_typed() {
        let about = About::from_form("<script>x</script>", "example.com", "");
        let page = render_about(&about, Some("Saved."));
        assert!(page.contains("&lt;script&gt;x&lt;/script&gt;"));
        assert!(!page.contains("<script>"));
        assert!(page.contains(">example.com</textarea>"));
        assert!(page.contains("action=\"/about\""));
    }

    #[test]
    fn results_say_why_they_moved() {
        let view = HistoryView {
            about: About::from_form("programming", "github.com", ""),
            ..HistoryView::default()
        };
        let mut hit = Hit {
            domain: "rust-lang.org".into(),
            url: "https://rust-lang.org/".into(),
            title: Some("Rust Programming Language".into()),
            description: None,
            score: 1.0,
            text_score: 1.0,
            link_score: 0.0,
            country: None,
            named: true,
            official: false,
        };
        assert_eq!(
            view.notes(&hit),
            ["<span class=\"op\">Matches your interest: programming</span>"]
        );
        hit.domain = "github.com".into();
        assert_eq!(
            view.notes(&hit),
            ["<span class=\"op\">One of your sites</span>"]
        );
    }

    #[test]
    fn prefs_round_trip_through_their_cookie() {
        for (show, rank) in [(true, true), (true, false), (false, true), (false, false)] {
            let prefs = Prefs { show, rank };
            assert_eq!(Prefs::from_cookie(prefs.cookie_value()), prefs);
        }
        assert_eq!(Prefs::from_cookie("junk"), Prefs::default());
    }

    #[test]
    fn cookies_are_read_from_every_header() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("a=1; plumb_history=s0r1"),
        );
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("plumb_profile=abc"),
        );
        let cookies = cookies(&headers);
        assert_eq!(cookies.get(PREFS_COOKIE), Some(&"s0r1"));
        assert_eq!(cookies.get(PROFILE_COOKIE), Some(&"abc"));
    }

    #[test]
    fn the_history_page_escapes_what_was_searched() {
        let mut history = History::default();
        history.add_search("<b>bank</b>", 100);
        history.add_opened("<b>bank</b>", "usbank.com", 100);
        let page = render_history(&history, Prefs::default(), 160);
        assert!(page.contains("&lt;b&gt;bank&lt;/b&gt;"));
        assert!(!page.contains("<b>bank"));
        assert!(page.contains("usbank.com"));
        assert!(page.contains("action=\"/history/clear\""));
    }
}
