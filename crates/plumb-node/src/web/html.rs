//! The pieces every page is made of: the page frame, the app bar, the
//! settings form, links, escaping and numbers in words.

use std::fmt::Write as _;

use axum::http::{header, HeaderName, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use plumb_core::{RecentNews, SafeSearch};
use plumb_index::{Hit, SearchOptions};
use url::Url;

use super::hit::append_filters;
use super::home::ICON_PNG_BASE64;
use super::params::{NetSetting, Settings};
use super::{history, CONTENT_SECURITY_POLICY, OPENSEARCH_LINK};
use crate::country::COUNTRY_CHOICES;

pub(crate) fn security_headers() -> [(HeaderName, &'static str); 3] {
    [
        (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ]
}

pub(crate) fn html_response(status: StatusCode, page: String) -> Response {
    (status, security_headers(), Html(page)).into_response()
}

/// Escapes text for HTML element content and quoted attribute values.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Where a hit links to: its URL when that is `http` or `https`, else its
/// domain's homepage, else nothing.
pub(crate) fn safe_href(hit: &Hit) -> Option<String> {
    http_url(&hit.url).or_else(|| homepage_url(&hit.domain))
}

/// `raw` re-serialized, when it is an absolute `http` or `https` URL with a host.
pub(crate) fn http_url(raw: &str) -> Option<String> {
    let url = Url::parse(raw.trim()).ok()?;
    let ok = matches!(url.scheme(), "http" | "https") && url.host_str().is_some();
    ok.then(|| url.to_string())
}

/// `https://<domain>/`, when `domain` is a plain hostname and nothing more.
pub(crate) fn homepage_url(domain: &str) -> Option<String> {
    let url = Url::parse(&format!("https://{domain}/")).ok()?;
    let plain = url.host_str() == Some(domain) && url.port().is_none() && url.path() == "/";
    plain.then(|| url.to_string())
}

/// `12345` -> `12,345`.
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A rough length of time in words: `90` -> `1 minute`, `7200` -> `2 hours`.
pub(crate) fn duration_words(seconds: u64) -> String {
    let (n, unit) = match seconds {
        0..=59 => (seconds, "second"),
        60..=3_599 => (seconds / 60, "minute"),
        3_600..=86_399 => (seconds / 3_600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// `at` (Unix seconds) seen from `now`: `just now`, `5 minutes ago`.
pub(crate) fn time_ago(at: u64, now: u64) -> String {
    match now.saturating_sub(at) {
        0..=9 => "just now".to_string(),
        age => format!("{} ago", duration_words(age)),
    }
}

/// `at` (Unix seconds) seen from `now`: `in 10 minutes`, `any moment now`.
pub(crate) fn time_until(at: u64, now: u64) -> String {
    match at.saturating_sub(now) {
        0 => "any moment now".to_string(),
        wait => format!("in {}", duration_words(wait)),
    }
}

pub(crate) const STYLE: &str = include_str!("style.css");

/// Shared chrome; public search pages do not advertise local node controls.
pub(crate) fn app_bar(active: &str, controls: bool) -> String {
    app_bar_for(active, controls, "/app")
}

pub(crate) fn app_bar_for(active: &str, controls: bool, panel: &str) -> String {
    let panel = escape_html(panel);
    let icon = &*ICON_PNG_BASE64;
    let current = |page: &str| {
        if page == active {
            " aria-current=\"page\""
        } else {
            ""
        }
    };
    let controls = if controls {
        // One link: the panel's Overview section carries the node's status.
        format!("<a href=\"{panel}\"{}>Settings</a>", current("settings"))
    } else {
        String::new()
    };
    format!(
        "<header class=\"app-bar\"><a class=\"app-brand\" href=\"/\" aria-label=\"Plumb Search home\">\
         <img src=\"data:image/png;base64,{icon}\" width=\"28\" height=\"28\" alt=\"\">Plumb</a>\
         <nav aria-label=\"App navigation\"><a href=\"/\"{}>Search</a>{controls}</nav></header>",
        current("search")
    )
}

/// The foot of the home and results pages: the code, and where the data
/// comes from and under which licences. A node does not serve the
/// website's pages, so both point to GitHub.
pub(crate) const FOOTER: &str =
    "<p class=\"ft\"><a href=\"https://github.com/SueHeir/plumb-search\" \
     rel=\"noreferrer\">Source code</a> &middot; <a \
     href=\"https://github.com/SueHeir/plumb-search#data-sources\" rel=\"noreferrer\">Data \
     sources and licences</a></p>";

/// A whole HTML document; `body` must already be escaped.
pub(crate) fn page(title: &str, body: &str) -> String {
    page_with_head(title, "", body)
}

/// [`page`] with more elements in its `<head>`, which must be safe HTML.
pub(crate) fn page_with_head(title: &str, head: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n{OPENSEARCH_LINK}\
         <title>{}</title>\n<style>{STYLE}</style>\n{head}</head>\n<body>\n{body}\n</body>\n</html>\n",
        escape_html(title)
    )
}

/// The search form with its settings tucked behind a gear: the home
/// country, whether to leave out other countries' sites, and whether to
/// ask the Plumb network too. A `<details>` opens the gear, so it needs no
/// script; the settings travel with the search as query parameters.
/// The languages the settings gear offers, by their own names.
pub(crate) const LANGUAGE_CHOICES: &[(&str, &str)] = &[
    ("en", "English"),
    ("de", "Deutsch"),
    ("es", "Español"),
    ("fr", "Français"),
    ("it", "Italiano"),
    ("nl", "Nederlands"),
    ("pl", "Polski"),
    ("pt", "Português"),
    ("sv", "Svenska"),
    ("tr", "Türkçe"),
    ("ru", "Русский"),
    ("uk", "Українська"),
    ("ar", "العربية"),
    ("hi", "हिन्दी"),
    ("ja", "日本語"),
    ("ko", "한국어"),
    ("zh", "中文"),
];

pub(crate) fn settings_form(query: &str, autofocus: bool, settings: &Settings) -> String {
    let options = &settings.options;
    let current = Some(settings.country_mode.as_str());
    let mut choices =
        format!(
        "<option value=\"auto\"{}>Automatic</option><option value=\"any\"{}>Any country</option>",
        if current == Some("auto") { " selected" } else { "" },
        if current == Some("any") { " selected" } else { "" }
    );
    listed_options(
        &mut choices,
        current.filter(|c| *c != "auto" && *c != "any"),
        COUNTRY_CHOICES,
        false,
    );
    let safe_choices = plain_options([
        (
            SafeSearch::Off.as_str(),
            "Off",
            options.safe == SafeSearch::Off,
        ),
        (
            SafeSearch::Moderate.as_str(),
            "Moderate",
            options.safe == SafeSearch::Moderate,
        ),
        (
            SafeSearch::Strict.as_str(),
            "Strict",
            options.safe == SafeSearch::Strict,
        ),
    ]);
    let news_choices = plain_options([
        (
            RecentNews::Collapsed.as_str(),
            "Folded",
            options.recent == RecentNews::Collapsed,
        ),
        (
            RecentNews::Expanded.as_str(),
            "Open",
            options.recent == RecentNews::Expanded,
        ),
        (
            RecentNews::Off.as_str(),
            "Off",
            options.recent == RecentNews::Off,
        ),
    ]);
    let language = options.language.as_deref();
    let mut language_choices = format!(
        "<option value=\"any\"{}>Any language</option>",
        if language.is_none() { " selected" } else { "" }
    );
    listed_options(&mut language_choices, language, LANGUAGE_CHOICES, true);
    let network = match settings.network {
        NetSetting::Unavailable => {
            "<label class=\"off\"><input type=\"checkbox\" disabled> Use the Plumb network \
             for search</label><p class=\"hint\">Not on this site: it has not joined the Plumb \
             network, so results come only from its own index.</p>"
        }
        NetSetting::Off | NetSetting::On => {
            if settings.network == NetSetting::On {
                "<label><input type=\"checkbox\" name=\"net\" value=\"1\" checked> Use the \
                 Plumb network for search</label>"
            } else {
                "<label><input type=\"checkbox\" name=\"net\" value=\"1\"> Use the Plumb \
                 network for search</label>"
            }
        }
    };
    let network_hint = match settings.network {
        NetSetting::Unavailable => String::new(),
        NetSetting::Off | NetSetting::On => format!(
            "<p class=\"hint\">Uses data from other Plumb nodes without sending query text. \
             Results only they found are tinted. Missing data may wait for a background \
             download.</p><p class=\"hint\">{} Set in this node's panel.</p>",
            settings.scope.explain()
        ),
    };
    let history = match (&settings.history, &settings.browser_about) {
        (Some(history), _) => history.settings_html(),
        (None, Some(_)) => history::browser_settings_html().to_owned(),
        (None, None) => String::new(),
    };
    let kinds = if settings.about().is_some() {
        "<p class=\"hint\"><a href=\"/about#kinds\">More or less of each kind of result: \
         podcasts, songs, papers and more</a></p>"
    } else {
        ""
    };
    let private = if settings.private {
        private_toggle_link(
            false,
            &settings_link("/private", "", settings, options, false).replace("?q=&", "?"),
        )
    } else {
        String::new()
    };
    format!(
        "<form action=\"/search\" method=\"get\" role=\"search\">{request_fields}\
         <input type=\"search\" name=\"q\" value=\"{}\" placeholder=\"Search sites, articles, questions and more\" \
         aria-label=\"Search\" autocomplete=\"off\"{}>\
         <details class=\"gear\"><summary title=\"Settings\" aria-label=\"Settings\">\
         &#9881;&#xFE0E;</summary><div class=\"panel\">\
         <label>Country <select name=\"country\">{choices}</select></label>\
         <label><input type=\"checkbox\" name=\"only\" value=\"1\"{}> Only this country</label>\
         <label>Language <select name=\"lang\">{language_choices}</select></label>\
         <label>Safe search <select name=\"safe\">{safe_choices}</select></label>\
         <label>Recent news <select name=\"news\">{news_choices}</select></label>\
         {network}{network_hint}{history}{kinds}{private}<label><input type=\"checkbox\" name=\"exact\" value=\"1\"{exact}> Search exactly as typed</label><label><input type=\"checkbox\" name=\"debug\" value=\"1\"{debug}> Show ranking details</label><button type=\"submit\">Apply</button></div></details>\
         <button type=\"submit\">Search</button></form>",
        escape_html(query),
        if autofocus { " autofocus" } else { "" },
        if options.only_country { " checked" } else { "" },
        request_fields = settings.request_fields.iter().map(|(name, value)| format!("<input type=\"hidden\" name=\"{name}\" value=\"{}\">", escape_html(value))).collect::<String>(),
        exact = if options.exact { " checked" } else { "" },
        debug = if settings.debug { " checked" } else { "" }
    )
}

/// Writes a `<select>`'s options from `list`, with `current` selected. A
/// current value not in the list (set by an older form or a hand-made link)
/// is kept as its own option so applying the settings doesn't drop it.
fn listed_options(out: &mut String, current: Option<&str>, list: &[(&str, &str)], lang: bool) {
    if let Some(code) = current.filter(|c| !list.iter().any(|(l, _)| l == c)) {
        let _ = write!(
            out,
            "<option value=\"{0}\" selected>{0}</option>",
            escape_html(code)
        );
    }
    for (code, name) in list {
        let selected = if current == Some(*code) {
            " selected"
        } else {
            ""
        };
        let lang = if lang {
            format!(" lang=\"{code}\"")
        } else {
            String::new()
        };
        let _ = write!(
            out,
            "<option value=\"{code}\"{lang}{selected}>{name}</option>"
        );
    }
}

/// Options of a fixed `<select>`: value, label and whether it is chosen.
fn plain_options<'a>(items: impl IntoIterator<Item = (&'a str, &'a str, bool)>) -> String {
    items
        .into_iter()
        .map(|(value, name, chosen)| {
            let selected = if chosen { " selected" } else { "" };
            format!("<option value=\"{value}\"{selected}>{name}</option>")
        })
        .collect()
}

/// The gear's "Private search" switch, shown only where the node serves
/// private search. It is a link, not a form field, so turning it on never
/// sends what is typed in the search box: it opens `/private`, and turning
/// it off there goes back to normal search.
pub(crate) fn private_toggle(on: bool) -> String {
    let (href, _checked) = if on {
        ("/", "true")
    } else {
        ("/private", "false")
    };
    private_toggle_link(on, href)
}

fn private_toggle_link(on: bool, href: &str) -> String {
    let checked = if on { "true" } else { "false" };
    let href = escape_html(href);
    format!(
        "<div class=\"pv\"><a class=\"tg\" href=\"{href}\" role=\"switch\" \
         aria-checked=\"{checked}\"><span class=\"knob\" aria-hidden=\"true\"></span>\
         Private search</a><p class=\"hint\">Your browser looks up the results itself, so \
         this site never sees what you search for. Needs JavaScript.</p></div>"
    )
}

/// The results page's header: the logo and the search form with its
/// settings gear.
pub(crate) fn results_form(query: &str, settings: &Settings) -> String {
    format!(
        "{}<header class=\"results-header\">{}</header>",
        app_bar("search", settings.manage),
        settings_form(query, false, settings)
    )
}

/// `path?q=...` with the searcher's choices, for links between pages.
pub(crate) fn search_link(path: &str, query: &str, options: &SearchOptions, net: bool) -> String {
    let mut params = url::form_urlencoded::Serializer::new(String::new());
    params.append_pair("q", query);
    params.append_pair("country", options.country.as_deref().unwrap_or("any"));
    if options.only_country {
        params.append_pair("only", "1");
    }
    if net {
        params.append_pair("net", "1");
    }
    if options.exact {
        params.append_pair("exact", "1");
    }
    append_filters(&mut params, options);
    format!("{path}?{}", params.finish())
}

pub(crate) fn settings_link(
    path: &str,
    query: &str,
    settings: &Settings,
    options: &SearchOptions,
    net: bool,
) -> String {
    let link = search_link(path, query, options, net);
    let (_, pairs) = link.split_once('?').expect("search link");
    let mut params = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in url::form_urlencoded::parse(pairs.as_bytes()) {
        params.append_pair(
            &key,
            if key == "country" {
                &settings.country_mode
            } else {
                &value
            },
        );
    }
    if settings.debug {
        params.append_pair("debug", "1");
    }
    for (name, value) in &settings.request_fields {
        params.append_pair(name, value);
    }
    format!("{path}?{}", params.finish())
}
