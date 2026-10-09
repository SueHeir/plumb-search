//! Ways out of Plumb for searches it is not made for: a link that hands the
//! query to a web search engine (off unless the node is set to show one),
//! and "bangs" like `!g` that send a query straight to another search.
//!
//! Plumb finds sites and pages by name and topic; "how long to boil an egg" is a question
//! for a full-text engine. Both only ever link out: Plumb never fetches
//! another engine's results.

use std::fmt;

use crate::country::HomeCountry;

/// A web search engine the results page can hand a query to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    DuckDuckGo,
    Google,
    Bing,
    Brave,
    Startpage,
}

impl Engine {
    /// Every engine, in the order `--web-search` lists them.
    pub const ALL: [Engine; 5] = [
        Engine::DuckDuckGo,
        Engine::Google,
        Engine::Bing,
        Engine::Brave,
        Engine::Startpage,
    ];

    /// The engine's name as people know it.
    pub fn name(self) -> &'static str {
        match self {
            Engine::DuckDuckGo => "DuckDuckGo",
            Engine::Google => "Google",
            Engine::Bing => "Bing",
            Engine::Brave => "Brave Search",
            Engine::Startpage => "Startpage",
        }
    }

    /// The name `--web-search` takes.
    pub fn key(self) -> &'static str {
        match self {
            Engine::DuckDuckGo => "duckduckgo",
            Engine::Google => "google",
            Engine::Bing => "bing",
            Engine::Brave => "brave",
            Engine::Startpage => "startpage",
        }
    }

    fn template(self) -> &'static str {
        match self {
            Engine::DuckDuckGo => "https://duckduckgo.com/?q={}",
            Engine::Google => "https://www.google.com/search?q={}",
            Engine::Bing => "https://www.bing.com/search?q={}",
            Engine::Brave => "https://search.brave.com/search?q={}",
            Engine::Startpage => "https://www.startpage.com/do/search?query={}",
        }
    }

    /// The engine's results page for `query`.
    pub fn url(self, query: &str) -> String {
        fill(self.template(), query)
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A `--web-search` value: an engine, or `None` for `off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WebSearch(pub Option<Engine>);

/// Reads a `--web-search` value: `off` (or `none`) for no link, or an
/// engine's [`Engine::key`] (`ddg` for DuckDuckGo).
pub fn parse_web_search(text: &str) -> Result<WebSearch, String> {
    let key = text.trim().to_ascii_lowercase();
    if matches!(key.as_str(), "off" | "none" | "") {
        return Ok(WebSearch(None));
    }
    let key = match key.as_str() {
        "ddg" => "duckduckgo",
        other => other,
    };
    Engine::ALL
        .into_iter()
        .find(|engine| engine.key() == key)
        .map(|engine| WebSearch(Some(engine)))
        .ok_or_else(|| {
            let keys: Vec<&str> = Engine::ALL.iter().map(|e| e.key()).collect();
            format!("expected off or one of {}, got {text:?}", keys.join(", "))
        })
}

/// What the web app shows and does beyond Plumb's own results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSettings {
    /// The home country of searches that do not name one.
    pub home: HomeCountry,
    /// The engine the results page offers to hand each query to; `None`
    /// shows no such link.
    pub web_search: Option<Engine>,
    /// Whether every client of `/mcp` may use its `read_page` tool. When
    /// false, only AI apps on this computer may (see [`crate::web`]).
    pub read_pages_for_all: bool,
    /// How `read_page` fetches pages.
    pub page_reader: plumb_crawl::ReadConfig,
    /// The plugins whose results show with the node's own.
    pub plugins: crate::plugins::Plugins,
}

impl Default for WebSettings {
    fn default() -> Self {
        WebSettings {
            home: HomeCountry::Auto,
            web_search: None,
            read_pages_for_all: false,
            page_reader: plumb_crawl::ReadConfig::default(),
            plugins: crate::plugins::Plugins::default(),
        }
    }
}

impl From<HomeCountry> for WebSettings {
    fn from(home: HomeCountry) -> Self {
        WebSettings {
            home,
            ..WebSettings::default()
        }
    }
}

/// Bangs: a word like `!g` at the start or end of a query sends the rest
/// of it to that search. The `{}` takes the query.
const BANGS: &[(&str, &str)] = &[
    ("!g", "https://www.google.com/search?q={}"),
    ("!google", "https://www.google.com/search?q={}"),
    ("!ddg", "https://duckduckgo.com/?q={}"),
    ("!d", "https://duckduckgo.com/?q={}"),
    ("!b", "https://www.bing.com/search?q={}"),
    ("!bing", "https://www.bing.com/search?q={}"),
    ("!brave", "https://search.brave.com/search?q={}"),
    ("!sp", "https://www.startpage.com/do/search?query={}"),
    ("!w", "https://en.wikipedia.org/w/index.php?search={}"),
    ("!yt", "https://www.youtube.com/results?search_query={}"),
    ("!gh", "https://github.com/search?q={}"),
    ("!a", "https://www.amazon.com/s?k={}"),
    ("!m", "https://www.google.com/maps/search/{}"),
    ("!r", "https://www.reddit.com/search/?q={}"),
];

/// Where a query with a bang goes: `!g rust traits` and `rust traits !g`
/// both go to Google's results for "rust traits". `None` when the query's
/// first and last words are no known bang (case does not matter).
pub fn bang_url(query: &str) -> Option<String> {
    let words: Vec<&str> = query.split_whitespace().collect();
    let (first, last) = (words.first()?, words.last()?);
    let find = |word: &str| {
        BANGS
            .iter()
            .find(|(bang, _)| bang.eq_ignore_ascii_case(word))
            .map(|&(_, template)| template)
    };
    let (template, rest) = if let Some(template) = find(first) {
        (template, &words[1..])
    } else {
        (find(last)?, &words[..words.len() - 1])
    };
    Some(fill(template, &rest.join(" ")))
}

/// `template` with its `{}` replaced by `query`, percent-encoded.
fn fill(template: &str, query: &str) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    template.replacen("{}", &encoded.replace('+', "%20"), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bangs_at_either_end_send_the_rest_on() {
        assert_eq!(
            bang_url("!g rust traits").as_deref(),
            Some("https://www.google.com/search?q=rust%20traits")
        );
        assert_eq!(
            bang_url("how long to boil an egg !DDG").as_deref(),
            Some("https://duckduckgo.com/?q=how%20long%20to%20boil%20an%20egg")
        );
        assert_eq!(
            bang_url("!w C++ & Rust").as_deref(),
            Some("https://en.wikipedia.org/w/index.php?search=C%2B%2B%20%26%20Rust")
        );
        assert_eq!(
            bang_url("!yt").as_deref(),
            Some("https://www.youtube.com/results?search_query=")
        );
    }

    #[test]
    fn other_exclamation_marks_are_just_words() {
        assert_eq!(bang_url("yahoo!"), None);
        assert_eq!(bang_url("rust !g traits"), None);
        assert_eq!(bang_url("!nope rust"), None);
        assert_eq!(bang_url(""), None);
    }

    #[test]
    fn web_search_settings_parse() {
        assert_eq!(parse_web_search("off"), Ok(WebSearch(None)));
        assert_eq!(
            parse_web_search("DDG"),
            Ok(WebSearch(Some(Engine::DuckDuckGo)))
        );
        assert_eq!(parse_web_search("bing"), Ok(WebSearch(Some(Engine::Bing))));
        assert!(parse_web_search("altavista").is_err());
        assert_eq!(
            Engine::Startpage.url("a b"),
            "https://www.startpage.com/do/search?query=a%20b"
        );
    }
}
