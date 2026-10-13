//! A search's parameters, from the address and the browser's settings.

use anyhow::Result;
use axum::http::{header, HeaderMap};
use plumb_core::{collapse_whitespace, language_code, truncate_chars, RecentNews, SafeSearch};
use plumb_index::SearchOptions;
use serde::Deserialize;

use super::html::LANGUAGE_CHOICES;
use super::{history, AppState, DEFAULT_LIMIT, MAX_LIMIT, MAX_QUERY_CHARS};
use crate::about::About;
use crate::country::HomeCountry;
use crate::websearch::WebSettings;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SearchParams {
    #[serde(default)]
    pub(crate) q: String,
    /// Paper-only API/HTML request and inclusive publication bounds.
    pub(crate) kind: Option<String>,
    pub(crate) after: Option<String>,
    pub(crate) before: Option<String>,
    pub(crate) order: Option<String>,
    /// A plugin's folder name: run it for this search, as a link it
    /// offers asks (see [`crate::plugins::Offer`]).
    pub(crate) run: Option<String>,
    #[serde(default, deserialize_with = "parse_search_limit")]
    pub(crate) limit: Option<usize>,
    /// A two-letter code, `any` for no home country, or empty for the default.
    pub(crate) country: Option<String>,
    /// `1` (or `on`, `true`): only the home country's sites and global ones.
    pub(crate) only: Option<String>,
    /// `1`: `/api/search` answers with a
    /// [`SearchResults`](plumb_index::SearchResults) object.
    pub(crate) full: Option<String>,
    /// `1` (or `on`, `true`): `/search` asks the Plumb network too.
    pub(crate) net: Option<String>,
    /// `1` (or `on`, `true`): search for the query as typed, without
    /// correcting typos.
    pub(crate) exact: Option<String>,
    pub(crate) debug: Option<String>,
    /// `1`: the settings gear's form sent the history choices below.
    pub(crate) hist: Option<String>,
    /// `1`: show past searches.
    pub(crate) hs: Option<String>,
    /// `1`: rank sites opened before higher.
    pub(crate) hr: Option<String>,
    /// `1`: learn from clicks which boxes to fold (see [`crate::learn`]).
    pub(crate) hl: Option<String>,
    /// `1`: keep searches as examples to train the ranking.
    pub(crate) ht: Option<String>,
    /// `1`: edit mode, with buttons to move results and fold boxes.
    pub(crate) edit: Option<String>,
    /// `<seed>.<step>`: tuning, a round of random searches in edit mode
    /// (see [`tune`](super::tune)).
    pub(crate) tune: Option<String>,
    /// Safe search: `off`, `moderate` (the default) or `strict`.
    pub(crate) safe: Option<String>,
    /// Only sites in this language (a language code), `any` (or empty)
    /// for any; without it, the browser's first language when the gear
    /// offers it, else [`DEFAULT_LANGUAGE`].
    pub(crate) lang: Option<String>,
    /// The "Recent" headlines: `collapsed` (the default), `expanded` or
    /// `off`.
    pub(crate) news: Option<String>,
    /// `json`: `/search` answers in SearXNG's JSON format, for AI apps
    /// that take a SearXNG address (see [`searxng`](super::searxng)).
    pub(crate) format: Option<String>,
    /// SearXNG's page of results, from 1.
    pub(crate) pageno: Option<usize>,
    /// SearXNG's safe search: 0, 1 or 2.
    pub(crate) safesearch: Option<String>,
    /// SearXNG's categories, comma separated: `news` alone asks for
    /// recent headlines only.
    pub(crate) categories: Option<String>,
    /// SearXNG's time range (`day`, `week`, ...): recent headlines first.
    pub(crate) time_range: Option<String>,
}

fn parse_search_limit<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<usize>, D::Error> {
    let value = String::deserialize(deserializer)?;
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let digits = value
        .strip_prefix('-')
        .or_else(|| value.strip_prefix('+'))
        .unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Err(serde::de::Error::custom("Use a whole number for limit."));
    }
    let limit = if value.starts_with('-') {
        0
    } else {
        digits.parse::<usize>().unwrap_or(MAX_LIMIT).min(MAX_LIMIT)
    };
    Ok(Some(limit))
}

pub(crate) fn error_settings(
    state: &AppState,
    params: &SearchParams,
    headers: &HeaderMap,
) -> Settings {
    Settings {
        manage: state.local_controls(headers),
        options: params.page_options(&state.settings, headers),
        network: state.net_setting(params),
        scope: state.search_scope(),
        private: state.private_search(),
        country_mode: params.country_mode(),
        debug: flag(&params.debug),
        request_fields: params.request_fields(),
        history: None,
        browser_about: None,
        welcome: false,
        experiment: None,
    }
}

/// The language filter of a search that names none, when the browser's
/// first language is not one the settings gear offers.
const DEFAULT_LANGUAGE: &str = "en";

/// The language filter of a search that names none: the first language of
/// the browser's `Accept-Language` when the settings gear offers it
/// (`de-DE,de;q=0.9` -> `de`), else [`DEFAULT_LANGUAGE`].
fn default_language(accept_language: Option<&str>) -> String {
    accept_language
        .and_then(|header| header.split(',').next())
        .and_then(|first| language_code(first.split(';').next().unwrap_or_default()))
        .filter(|code| LANGUAGE_CHOICES.iter().any(|(offered, _)| offered == code))
        .unwrap_or_else(|| DEFAULT_LANGUAGE.to_string())
}

/// Whether a flag parameter is set: `1`, `on`, `true` or `yes`.
pub(crate) fn flag(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "on" | "true" | "yes"
        )
    })
}

impl SearchParams {
    pub(crate) fn paper_search_query(&self) -> Result<Option<String>> {
        let typed = self.after.is_some() || self.before.is_some() || self.order.is_some();
        if self.kind.is_some() && self.kind.as_deref() != Some("paper") && typed {
            anyhow::bail!("publication bounds and order require kind=paper");
        }
        let paper = plumb_core::paper_query::PaperQuery::with_options(
            &self.query(),
            self.after.as_deref(),
            self.before.as_deref(),
            self.order.as_deref(),
        )?;
        if paper.constrained && self.kind.as_deref().is_some_and(|kind| kind != "paper") {
            anyhow::bail!("publication bounds and order require kind=paper");
        }
        if self.kind.as_deref() == Some("paper") || typed || paper.constrained {
            Ok(Some(format!(
                "{} sort:{}",
                paper.text(),
                if paper.newest { "newest" } else { "relevance" }
            )))
        } else {
            Ok(None)
        }
    }
    pub(crate) fn request_fields(&self) -> Vec<(&'static str, String)> {
        [
            ("kind", &self.kind),
            ("after", &self.after),
            ("before", &self.before),
            ("order", &self.order),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.as_ref().map(|v| (name, v.clone())))
        .collect()
    }

    pub(crate) fn country_mode(&self) -> String {
        match self
            .country
            .as_deref()
            .and_then(|c| HomeCountry::parse(c).ok())
        {
            Some(HomeCountry::Fixed(code)) => code,
            Some(HomeCountry::Off) => "any".into(),
            _ => "auto".into(),
        }
    }

    /// The query with whitespace collapsed, cut to [`MAX_QUERY_CHARS`].
    pub(crate) fn query(&self) -> String {
        truncate_chars(&collapse_whitespace(&self.q), MAX_QUERY_CHARS)
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
    }

    /// The history choices the settings gear's form sent, if it sent them.
    pub(crate) fn history_prefs(&self) -> Option<history::Prefs> {
        history::prefs_from_form(&self.hist, &self.hs, &self.hr, &self.hl, &self.ht)
    }

    /// The searcher's choices: the `country` and `lang` parameters when
    /// they are valid, else the server's settings for a request with these
    /// headers.
    pub(crate) fn options(&self, settings: &WebSettings, headers: &HeaderMap) -> SearchOptions {
        let home = &settings.home;
        let asked = self
            .country
            .as_deref()
            .filter(|c| !c.trim().is_empty())
            .and_then(|c| HomeCountry::parse(c).ok());
        let accept_language = headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|value| value.to_str().ok());
        let country = match asked {
            Some(HomeCountry::Auto) | None => home.resolve(accept_language),
            Some(asked) => asked.resolve(accept_language),
        };
        SearchOptions {
            only_country: flag(&self.only) && country.is_some(),
            country,
            exact: flag(&self.exact),
            safe: self
                .safe
                .as_deref()
                .and_then(SafeSearch::parse)
                .unwrap_or_default(),
            language: match self.lang.as_deref().map(str::trim) {
                None => Some(
                    settings
                        .language
                        .clone()
                        .unwrap_or_else(|| default_language(accept_language)),
                ),
                Some(lang) if lang.is_empty() || lang.eq_ignore_ascii_case("any") => None,
                Some(lang) => language_code(lang),
            },
            recent: self
                .news
                .as_deref()
                .and_then(RecentNews::parse)
                .unwrap_or_default(),
        }
    }

    /// [`Self::options`] for a page with the settings gear, which starts
    /// with the server's "Only this country" when the address names no
    /// country. The gear's form and the pages' own links always name one,
    /// so turning it off there sticks.
    pub(crate) fn page_options(
        &self,
        settings: &WebSettings,
        headers: &HeaderMap,
    ) -> SearchOptions {
        let mut options = self.options(settings, headers);
        if settings.only_country && self.country.is_none() && self.only.is_none() {
            options.only_country = options.country.is_some();
        }
        options
    }
}

/// Whether a search asks the Plumb network, as the settings gear shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetSetting {
    /// This node has not joined the network: its own index only.
    Unavailable,
    Off,
    On,
}

/// What the settings gear holds.
pub(crate) struct Settings {
    /// Offer node controls on the node's own local address.
    pub(crate) manage: bool,
    pub(crate) options: SearchOptions,
    pub(crate) network: NetSetting,
    /// Which nodes the network part of a search asks.
    pub(crate) scope: plumb_net::SearchScope,
    /// This node offers private search (`/private`).
    pub(crate) private: bool,
    pub(crate) country_mode: String,
    pub(crate) debug: bool,
    /// Typed paper request constraints carried through form/error retries.
    pub(crate) request_fields: Vec<(&'static str, String)>,
    /// The searcher's history, on a node that keeps one.
    pub(crate) history: Option<history::HistoryView>,
    /// On a node that keeps no profiles, the About profile the browser
    /// keeps itself (see [`history::KeptAbout`]).
    pub(crate) browser_about: Option<About>,
    /// The home page invites the searcher to the welcome page.
    pub(crate) welcome: bool,
    /// The page's token, when the search is in ranking experiments: its
    /// result links say it and their place (see [`crate::experiments`]).
    pub(crate) experiment: Option<u64>,
}

impl Settings {
    /// The searcher's About profile, wherever it is kept.
    pub(crate) fn about(&self) -> Option<&About> {
        match &self.history {
            Some(history) => Some(&history.about),
            None => self.browser_about.as_ref(),
        }
    }
}
