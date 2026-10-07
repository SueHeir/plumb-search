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
//! post to `/history/clear` with it, and `HttpOnly`; posts that say they
//! come from another site are refused as well. The choices (show past
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
use crate::learn::{
    describe_key, traits, Block, Choice, Learned, Rating, Taste, Trait, Verdict, JUDGED_PER_PAGE,
};

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
        .route("/history/forget-clicks", post(forget_clicks))
        .route("/feedback", post(feedback))
        .route("/about", get(about_page).post(save_about))
}

/// What a browser chose to do with its history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Prefs {
    /// Show past searches on the home page and label sites opened before.
    pub show: bool,
    /// Rank sites opened before higher.
    pub rank: bool,
    /// Learn from clicks which boxes (places and map, headlines) to fold
    /// or unfold, and which sites are passed over (see [`crate::learn`]).
    pub learn: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            show: true,
            rank: true,
            learn: true,
        }
    }
}

impl Prefs {
    /// Whether anything is noted at all.
    pub fn on(self) -> bool {
        self.show || self.rank || self.learn
    }

    /// `s1r1l1`; cookies set before learning was a choice have no `l`.
    fn cookie_value(self) -> String {
        format!(
            "s{}r{}l{}",
            u8::from(self.show),
            u8::from(self.rank),
            u8::from(self.learn)
        )
    }

    fn from_cookie(value: &str) -> Prefs {
        let bit = |c: u8| match c {
            b'1' => Some(true),
            b'0' => Some(false),
            _ => None,
        };
        let parsed = match value.as_bytes() {
            [b's', s, b'r', r] => bit(*s).zip(bit(*r)).map(|(show, rank)| Prefs {
                show,
                rank,
                learn: true,
            }),
            [b's', s, b'r', r, b'l', l] => bit(*s)
                .zip(bit(*r))
                .zip(bit(*l))
                .map(|((show, rank), learn)| Prefs { show, rank, learn }),
            _ => None,
        };
        parsed.unwrap_or_default()
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
    /// The results page is in edit mode: results hidden from the search
    /// stay listed, last, so they can be brought back.
    pub editing: bool,
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
                set_cookies.push(cookie(PREFS_COOKIE, &prefs.cookie_value()));
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
            editing: false,
            set_cookies,
        })
    }

    /// Notes a search, giving the browser a profile if it has none yet.
    /// The search itself is kept only while past searches are shown; with
    /// just ranking or learning on, the browser still gets a profile for
    /// the sites it opens.
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

    /// Notes that `domain` was opened for `query`, from this node's own
    /// results.
    pub fn note_opened(&mut self, query: &str, domain: &str) {
        let (keep, learn) = (self.prefs.show || self.prefs.rank, self.prefs.learn);
        if !keep && !learn {
            return;
        }
        // A browser opens results only from a page that gave it a profile.
        let Some(profile) = self.profile.clone() else {
            return;
        };
        let now = now_unix();
        if let Err(err) = self.store.update(&profile, |h| {
            if keep {
                h.add_opened(query, domain, now);
            }
            if learn {
                h.learned.note_picked(query, domain, now);
            }
        }) {
            warn!("could not note an opened site in the history: {err:#}");
        }
    }

    /// Gives the browser a profile if it has none yet, for buttons that
    /// change it.
    pub fn ensure_profile(&mut self) {
        let _ = self.profile_or_new();
    }

    /// Whether this browser's clicks are learned from.
    pub fn learns(&self) -> bool {
        self.prefs.learn
    }

    /// What to do with `block` for `query`, shown in `context`.
    /// What the searcher said wins; their clicks count only while learning.
    pub fn choice(&self, block: Block, query: &str, context: &str) -> Choice {
        if let Some(fold) = self.history.learned.box_verdict(block, query, context) {
            return if fold { Choice::Fold } else { Choice::Open };
        }
        if !self.prefs.learn {
            return Choice::Usual;
        }
        self.history.learned.choice(block, query, context)
    }

    /// Notes the boxes of a results page for `query` and this node's
    /// results on it, best first, when learning. The browser has a profile
    /// by then ([`Visitor::note_search`]).
    pub fn note_page(
        &mut self,
        query: &str,
        blocks: &[(Block, &str)],
        folded: &[(Block, &str)],
        sites: &[String],
    ) {
        if !self.prefs.learn {
            return;
        }
        let Some(profile) = self.profile.clone() else {
            return;
        };
        let now = now_unix();
        if let Err(err) = self.store.update(&profile, |h| {
            h.learned.note_shown(query, blocks, folded, sites, now);
        }) {
            warn!("could not note a results page in the history: {err:#}");
        }
    }

    /// Notes the first results of a page shown in edit mode, by their
    /// traits, for telling what kinds of result the searcher likes.
    pub fn note_judged(&mut self, query: &str, hits: &[Hit], home: Option<&str>) {
        let Some(profile) = self.profile.clone() else {
            return;
        };
        let results: Vec<Vec<Trait>> = hits
            .iter()
            .take(JUDGED_PER_PAGE)
            .map(|hit| hit_traits(hit, home))
            .collect();
        if let Err(err) = self.store.update(&profile, |h| {
            h.learned.note_judged(query, &results);
        }) {
            warn!("could not note a page in edit mode in the history: {err:#}");
        }
    }

    /// Notes that a link of `block` was opened for `query`.
    pub fn note_used(&mut self, query: &str, block: Block) {
        if !self.prefs.learn {
            return;
        }
        let Some(profile) = self.profile.clone() else {
            return;
        };
        let now = now_unix();
        if let Err(err) = self
            .store
            .update(&profile, |h| h.learned.note_used(query, block, now))
        {
            warn!("could not note a box opened in the history: {err:#}");
        }
    }

    pub fn profile_or_new(&mut self) -> Option<String> {
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

    /// Applies the browser's About profile to `hits`, then its history,
    /// then what it said about them for `query` in edit mode.
    /// `home` is the searcher's country, for the sites from it.
    pub fn rank(&self, query: &str, hits: &mut Vec<Hit>, home: Option<&str>) {
        self.about.apply(hits);
        self.rank_opened(query, hits);
        self.apply_tastes(hits, home);
        self.apply_verdicts(query, hits);
    }

    /// Moves results with traits the searcher likes up and ones they
    /// dislike down (see [`crate::learn::Trait`]).
    fn apply_tastes(&self, hits: &mut [Hit], home: Option<&str>) {
        let learned = &self.history.learned;
        if learned.tastes.is_empty() {
            return;
        }
        let mut changed = false;
        for hit in hits.iter_mut() {
            let (score, _) = learned.taste_score(&hit_traits(hit, home));
            if score != 0.0 {
                hit.score += score;
                changed = true;
            }
        }
        if changed {
            hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        }
    }

    /// Whether the searcher hid `domain` from `query`.
    pub fn hid(&self, query: &str, domain: &str) -> bool {
        self.history.learned.verdict(query, domain) == Some(Verdict::Hide)
    }

    /// Moves results put higher or lower for `query`, and leaves out the
    /// ones hidden from it (lists them last while editing).
    fn apply_verdicts(&self, query: &str, hits: &mut Vec<Hit>) {
        let verdicts = self.history.learned.verdicts_for(query);
        if verdicts.is_empty() {
            return;
        }
        let of = |domain: &str| {
            verdicts
                .iter()
                .find(|(d, _)| d == domain)
                .map(|(_, verdict)| *verdict)
        };
        for hit in hits.iter_mut() {
            if let Some(verdict) = of(&hit.domain) {
                hit.score += verdict.score();
            }
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        if self.editing {
            // Stable: the hidden ones go last, in their order.
            hits.sort_by_key(|hit| of(&hit.domain) == Some(Verdict::Hide));
        } else {
            hits.retain(|hit| of(&hit.domain) != Some(Verdict::Hide));
        }
    }

    /// Moves the sites opened before up `hits`, when the browser asked for
    /// that, and the sites it keeps passing over down, when it learns.
    fn rank_opened(&self, query: &str, hits: &mut [Hit]) {
        let opened = self.prefs.rank && !self.history.opened.is_empty();
        let passed = self.prefs.learn && !self.history.learned.sites.is_empty();
        if !opened && !passed {
            return;
        }
        let mut changed = false;
        for hit in hits.iter_mut() {
            let mut change = 0.0;
            if opened {
                change += self.history.bonus(query, &hit.domain);
            }
            if passed {
                change += self.history.learned.passed_over(&hit.domain);
            }
            if change != 0.0 {
                hit.score += change;
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
            learns: self.prefs.learn,
            open_news: false,
            news_verdict: None,
            verdicts: HashMap::new(),
            edit: None,
            tastes: self.history.learned.tastes.clone(),
            home: None,
            tune_bar: None,
        }
    }

    /// The browser's profile id, if it has one.
    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    /// The folder the profiles are kept in.
    pub fn dir(&self) -> &std::path::Path {
        self.store.dir()
    }

    /// Switches the browser to profile `id` (see [`crate::sync`]).
    pub fn use_profile(&mut self, id: &str) {
        self.set_cookies.push(cookie(PROFILE_COOKIE, id));
        self.profile = Some(id.to_owned());
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
    /// Clicks are learned from: links of the boxes go through `/go`.
    pub learns: bool,
    /// Unfold the "Recent" headlines: they are often read for searches
    /// like this one.
    pub open_news: bool,
    /// The searcher's own choice for the "Recent" box on this search:
    /// folded, or open.
    pub news_verdict: Option<bool>,
    /// What the searcher said about this search's results, by domain.
    pub verdicts: HashMap<String, Verdict>,
    /// The page is in edit mode.
    pub edit: Option<Editing>,
    /// What the searcher likes in results in general.
    pub tastes: Vec<Taste>,
    /// The searcher's country.
    pub home: Option<String>,
    /// While tuning: the bar on top of the page.
    pub tune_bar: Option<String>,
}

/// The traits of a result, for a searcher from `home`.
pub(super) fn hit_traits(hit: &Hit, home: Option<&str>) -> Vec<Trait> {
    traits(
        &hit.domain,
        hit.official,
        hit.link_score,
        hit.country.as_deref(),
        home,
    )
}

/// Traits as a form field: `code,popular`.
pub(super) fn traits_field(traits: &[Trait]) -> String {
    traits
        .iter()
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Traits from a form field; unknown ones are left out.
pub(super) fn parse_traits(field: &str) -> Vec<Trait> {
    let mut found = Vec::new();
    for kind in field.split(',').filter_map(|t| Trait::parse(t.trim())) {
        if !found.contains(&kind) {
            found.push(kind);
        }
    }
    found
}

/// A results page in edit mode.
#[derive(Debug, Clone, Default)]
pub(super) struct Editing {
    pub query: String,
    /// The page's own address, to come back to after a button.
    pub back: String,
}

/// Small buttons under a result in edit mode: higher, lower or hidden for
/// this search; the one pressed takes it back.
fn result_buttons(edit: &Editing, domain: &str, traits: &[Trait], now: Option<Verdict>) -> String {
    let mut out = format!(
        "<form class=\"fb\" method=\"post\" action=\"/feedback\">\
         <input type=\"hidden\" name=\"q\" value=\"{}\">\
         <input type=\"hidden\" name=\"d\" value=\"{}\">\
         <input type=\"hidden\" name=\"t\" value=\"{}\">\
         <input type=\"hidden\" name=\"back\" value=\"{}\">",
        escape_html(&edit.query),
        escape_html(domain),
        traits_field(traits),
        escape_html(&edit.back)
    );
    for (verdict, sign, title) in [
        (Verdict::Up, "\u{25b2} Up", "Higher for this search"),
        (Verdict::Down, "\u{25bc} Down", "Lower for this search"),
        (Verdict::Hide, "\u{2715} Hide", "Not for this search"),
    ] {
        let pressed = now == Some(verdict);
        let _ = write!(
            out,
            "<button name=\"v\" value=\"{}\" title=\"{}\" aria-label=\"{}\" aria-pressed=\"{pressed}\">{sign}</button>",
            if pressed { "none" } else { verdict.as_str() },
            if pressed { "Undo" } else { title },
            title,
        );
    }
    out.push_str("</form>");
    out
}

/// Buttons on a box in edit mode: fold it for this search or every
/// search like it, keep it open for every one, or take that back.
pub(super) fn box_buttons(
    edit: &Editing,
    block: Block,
    context: &str,
    now: Option<bool>,
) -> String {
    let mut out = format!(
        "<form class=\"fb fbx\" method=\"post\" action=\"/feedback\">\
         <input type=\"hidden\" name=\"q\" value=\"{}\">\
         <input type=\"hidden\" name=\"b\" value=\"{}\">\
         <input type=\"hidden\" name=\"c\" value=\"{}\">\
         <input type=\"hidden\" name=\"back\" value=\"{}\">",
        escape_html(&edit.query),
        block.as_str(),
        escape_html(context),
        escape_html(&edit.back)
    );
    let buttons: &[(&str, &str)] = match now {
        Some(_) => &[("none", "Undo my choice")],
        None => &[
            ("fold", "Fold for this search"),
            ("fold-all", "Fold for searches like this"),
            ("open-all", "Always open"),
        ],
    };
    for (value, label) in buttons {
        let _ = write!(out, "<button name=\"v\" value=\"{value}\">{label}</button>");
    }
    out.push_str("</form>");
    out
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
             <label><input type=\"checkbox\" name=\"hl\" value=\"1\"{}> Learn from my clicks \
             which boxes, like maps, I use</label>\
             <p class=\"hint\">Kept on this node for this browser only, and never sent \
             anywhere. \
             <a href=\"/history\">See or clear my history</a> \
             <a href=\"/about\">About you: interests and sites</a> \
             <a href=\"/tune\">Tune your search</a></p>",
            checked(self.prefs.show),
            checked(self.prefs.rank),
            checked(self.prefs.learn)
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
        let traits = hit_traits(hit, self.home.as_deref());
        if !self.tastes.is_empty() {
            let learned = Learned {
                tastes: self.tastes.clone(),
                ..Learned::default()
            };
            if let (_, Some(kind)) = learned.taste_score(&traits) {
                notes.push(format!(
                    "<span class=\"op\">You like {}</span>",
                    kind.label()
                ));
            }
        }
        let verdict = self.verdicts.get(&hit.domain).copied();
        match verdict {
            Some(Verdict::Up) => notes.push("<span class=\"op\">You put this higher</span>".into()),
            Some(Verdict::Down) => {
                notes.push("<span class=\"op\">You put this lower</span>".into())
            }
            Some(Verdict::Hide) => {
                notes.push("<span class=\"op\">Hidden from this search</span>".into());
            }
            None => {}
        }
        if let Some(edit) = &self.edit {
            notes.push(result_buttons(edit, &hit.domain, &traits, verdict));
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
/// them: `hist=1`, then `hs`, `hr` and `hl` for the boxes ticked.
pub(super) fn prefs_from_form(
    hist: &Option<String>,
    show: &Option<String>,
    rank: &Option<String>,
    learn: &Option<String>,
) -> Option<Prefs> {
    super::flag(hist).then(|| Prefs {
        show: super::flag(show),
        rank: super::flag(rank),
        learn: super::flag(learn),
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

/// Whether a page of another site sent the form. Without the profile
/// cookie (`SameSite=Lax` keeps it off such posts), saving would give the
/// browser a new profile chosen by that page.
pub(super) fn cross_site(headers: &HeaderMap) -> bool {
    let site = headers
        .get("sec-fetch-site")
        .and_then(|site| site.to_str().ok());
    match site {
        // Set by the browser, which pages cannot change: it settles it.
        // These pages' `no-referrer` policy makes browsers send
        // `Origin: null` with their own forms, so the origin cannot.
        Some("same-origin" | "none") => return false,
        Some(_) => return true,
        None => {}
    }
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok());
    let origin_host = headers
        .get(header::ORIGIN)
        .map(|origin| origin.to_str().ok().and_then(|o| url::Url::parse(o).ok()));
    match origin_host {
        None => false,
        Some(Some(origin)) => {
            let origin = match (origin.host_str(), origin.port()) {
                (Some(name), Some(port)) => format!("{name}:{port}"),
                (Some(name), None) => name.to_string(),
                (None, _) => return true,
            };
            !host.is_some_and(|host| host.eq_ignore_ascii_case(&origin))
        }
        Some(None) => true,
    }
}

pub(super) fn refuse_cross_site() -> Response {
    html_response(
        StatusCode::FORBIDDEN,
        page(
            "About you - Plumb Search",
            "<main class=\"wrap\"><p class=\"none\">Pages of other sites cannot change \
             this.</p></main>",
        ),
    )
}

/// `POST /history/clear`.
async fn clear(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if cross_site(&headers) {
        return refuse_cross_site();
    }
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

/// What the browser's clicks taught the node, with a button to forget it.
fn render_learned(body: &mut String, learned: &Learned) {
    if learned.is_empty() {
        return;
    }
    body.push_str(
        "<h2 id=\"learned\">Learned from your clicks and ratings</h2>\n\
         <p class=\"s\">Boxes you pass by for searches like these are folded to one line, \
         and headlines you read come unfolded. Opening a folded box once brings it back.</p>\n",
    );
    let (liked, disliked) = learned.leanings();
    for (traits, how) in [(liked, "You like"), (disliked, "You'd rather not see")] {
        if !traits.is_empty() {
            let labels: Vec<&str> = traits.iter().map(|t| t.label()).collect();
            let _ = writeln!(
                body,
                "<p class=\"s\">{how} <strong>{}</strong>. <a href=\"/tune\">Tune</a></p>",
                labels.join(", ")
            );
        }
    }
    let decided = learned.decided();
    if decided.is_empty() && learned.boxes.is_empty() {
        body.push_str("<p class=\"s\">Nothing folded or unfolded yet.</p>\n");
    } else if !decided.is_empty() {
        body.push_str("<ul>\n");
        for (count, choice) in decided.iter().take(MAX_LEARNED_SHOWN) {
            let what = match choice {
                Choice::Fold => "folded",
                _ => "unfolded",
            };
            let _ = writeln!(
                body,
                "<li><strong>{}</strong> {what} for {} \
                 <span class=\"m\">used {} of {} times</span></li>",
                count.block.label(),
                escape_html(&truncate_chars(&describe_key(&count.key), 100)),
                count.used.min(count.shown),
                count.shown
            );
        }
        body.push_str("</ul>\n");
    }
    let boxes: Vec<String> = learned
        .boxes
        .iter()
        .take(MAX_LEARNED_SHOWN)
        .map(|b| {
            format!(
                "<li><strong>{}</strong> {} for {} <span class=\"m\">your choice</span></li>",
                b.block.label(),
                if b.fold { "folded" } else { "open" },
                escape_html(&truncate_chars(&describe_key(&b.key), 100))
            )
        })
        .collect();
    if !boxes.is_empty() {
        let _ = writeln!(body, "<ul>\n{}\n</ul>", boxes.join("\n"));
    }
    if !learned.verdicts.is_empty() {
        body.push_str("<p class=\"s\">Results you moved in edit mode:</p>\n<ul>\n");
        let options = SearchOptions::default();
        for verdict in learned.verdicts.iter().take(MAX_LEARNED_SHOWN) {
            let _ = writeln!(
                body,
                "<li><strong>{}</strong> {} for <a href=\"{}\">{}</a></li>",
                escape_html(&verdict.domain),
                verdict.verdict.label(),
                escape_html(&format!(
                    "{}&edit=1",
                    search_link("/search", &verdict.query, &options, false)
                )),
                escape_html(&truncate_chars(&verdict.query, 80))
            );
        }
        body.push_str("</ul>\n");
    }
    let down = learned.moved_down();
    if !down.is_empty() {
        body.push_str(
            "<p class=\"s\">Moved down a little, because you passed them over for a result \
             below them every time:</p>\n<ul>\n",
        );
        for site in down.iter().take(MAX_LEARNED_SHOWN) {
            let _ = writeln!(
                body,
                "<li><strong>{}</strong> <span class=\"m\">passed over {} times</span></li>",
                escape_html(&site.domain),
                site.passed
            );
        }
        body.push_str("</ul>\n");
    }
    body.push_str(
        "<form method=\"post\" action=\"/history/forget-clicks\"><button type=\"submit\">\
         Forget what was learned</button></form>\n",
    );
}

/// Lines of each list on the history page's "Learned" part.
const MAX_LEARNED_SHOWN: usize = 30;

/// The edit mode's buttons.
#[derive(Debug, Default, Deserialize)]
struct FeedbackForm {
    #[serde(default)]
    q: String,
    /// A result's domain, or else a box `b` shown in the way `c`.
    d: Option<String>,
    b: Option<String>,
    c: Option<String>,
    /// The result's traits: a result put higher counts as liked, one put
    /// lower or hidden as disliked.
    #[serde(default)]
    t: String,
    #[serde(default)]
    v: String,
    #[serde(default)]
    back: String,
}

/// A change to a browser's history.
type Change = Box<dyn FnOnce(&mut History)>;

/// `POST /feedback`: says what the searcher thinks of a result or a box
/// for a search, then goes back to the results.
async fn feedback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<FeedbackForm>,
) -> Response {
    if cross_site(&headers) {
        return refuse_cross_site();
    }
    let Some(mut visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Only back to a results page of this node.
    let back = if form.back.starts_with("/search?") {
        form.back.clone()
    } else {
        "/".to_owned()
    };
    let query = truncate_chars(&plumb_core::collapse_whitespace(&form.q), 200);
    let now = now_unix();
    let change: Option<Change> = match (&form.d, &form.b) {
        (Some(domain), _) => {
            let domain = domain.trim().to_ascii_lowercase();
            let verdict = Verdict::parse(&form.v);
            let traits = parse_traits(&form.t);
            (verdict.is_some() || form.v == "none").then(|| {
                Box::new(move |h: &mut History| {
                    let rating = |verdict: Option<Verdict>| match verdict {
                        Some(Verdict::Up) => Some(Rating::Like),
                        Some(Verdict::Down | Verdict::Hide) => Some(Rating::Dislike),
                        None => None,
                    };
                    // A changed mind takes back what was said before.
                    let before = rating(h.learned.verdict(&query, &domain));
                    let after = rating(verdict);
                    if before != after {
                        if let Some(before) = before {
                            h.learned.rate(&traits, before, -1.0);
                        }
                        if let Some(after) = after {
                            h.learned.rate(&traits, after, 1.0);
                        }
                    }
                    h.learned.set_verdict(&query, &domain, verdict, now);
                }) as Change
            })
        }
        (None, Some(block)) => {
            let block = Block::parse(block);
            let context = form
                .c
                .as_deref()
                .filter(|c| matches!(*c, "said" | "guessed" | "site" | "words"))
                .map(str::to_owned);
            let what = match form.v.as_str() {
                "fold" => Some((false, Some(true))),
                "fold-all" => Some((true, Some(true))),
                "open-all" => Some((true, Some(false))),
                "none" => Some((true, None)),
                _ => None,
            };
            match (block, context, what) {
                (Some(block), Some(context), Some((every, fold))) => {
                    Some(Box::new(move |h: &mut History| {
                        h.learned.set_box(block, &query, &context, every, fold, now);
                    }) as Change)
                }
                _ => None,
            }
        }
        (None, None) => None,
    };
    let Some(change) = change else {
        return super::redirect(&back);
    };
    let Some(profile) = visitor.profile_or_new() else {
        return super::redirect(&back);
    };
    if let Err(err) = visitor.store.update(&profile, change) {
        warn!("could not save a result's verdict: {err:#}");
    }
    visitor.send_cookies(super::redirect(&back))
}

/// `POST /history/forget-clicks`: forgets what was learned from clicks,
/// keeping the rest of the history.
async fn forget_clicks(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if cross_site(&headers) {
        return refuse_cross_site();
    }
    let Some(visitor) = Visitor::of(&state, &headers, None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(profile) = &visitor.profile {
        if let Err(err) = visitor
            .store
            .update(profile, |h| h.learned = Learned::default())
        {
            warn!("could not forget what was learned from clicks: {err:#}");
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
            "<p class=\"s\">Nothing new is noted: all history choices are off in the search \
             page's settings.</p>\n",
        );
    }
    if history.searches.is_empty() && history.opened.is_empty() && history.learned.is_empty() {
        body.push_str("<p class=\"none\">No searches yet.</p>\n</main>\n</div>");
        return page("History - Plumb Search", &body);
    }
    render_learned(&mut body, &history.learned);
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
         history</button></form>\n\
         <p class=\"m\"><a href=\"/link\">Use your history on your other computers</a></p>\n\
         </main>\n</div>",
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
    if cross_site(&headers) {
        return refuse_cross_site();
    }
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

pub(super) fn no_store(mut response: Response) -> Response {
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
         <p class=\"m\"><a href=\"/history\">Your history</a> \u{b7} <a href=\"/link\">Use \
         this on your other computers</a></p>\n</main>\n</div>",
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
    fn forms_from_other_sites_are_refused() {
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, "127.0.0.1:7586".parse().unwrap());
            for (name, value) in pairs {
                headers.insert(*name, value.parse().unwrap());
            }
            headers
        };
        assert!(!cross_site(&headers(&[])));
        assert!(!cross_site(&headers(&[
            ("origin", "http://127.0.0.1:7586"),
            ("sec-fetch-site", "same-origin"),
        ])));
        assert!(cross_site(&headers(&[("sec-fetch-site", "cross-site")])));
        assert!(cross_site(&headers(&[("origin", "https://evil.example")])));
        assert!(cross_site(&headers(&[("origin", "null")])));
        // A form of this node's own pages, under their no-referrer policy.
        assert!(!cross_site(&headers(&[
            ("origin", "null"),
            ("sec-fetch-site", "same-origin"),
        ])));
        assert!(cross_site(&headers(&[
            ("origin", "http://127.0.0.1:7586"),
            ("sec-fetch-site", "same-site"),
        ])));
    }

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
            demand: None,
            placing_text_score: None,
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
            key_pages: Vec::new(),
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
        for show in [true, false] {
            for rank in [true, false] {
                for learn in [true, false] {
                    let prefs = Prefs { show, rank, learn };
                    assert_eq!(Prefs::from_cookie(&prefs.cookie_value()), prefs);
                }
            }
        }
        assert_eq!(Prefs::from_cookie("junk"), Prefs::default());
        // Set before learning was a choice.
        assert_eq!(
            Prefs::from_cookie("s0r1"),
            Prefs {
                show: false,
                rank: true,
                learn: true
            }
        );
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
