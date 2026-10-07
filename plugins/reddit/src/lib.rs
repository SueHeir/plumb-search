//! A Plumb Search plugin: Reddit threads for searches that start or end
//! with "reddit", from Reddit's official Data API
//! (<https://www.reddit.com/dev/api>).
//!
//! It signs in as the node owner's own Reddit app (application-only
//! OAuth with the app's id and secret from `config.json`) and sends the
//! User-Agent Reddit asks for, naming the app and its owner. It makes two
//! requests per search (a token, then the search), and the node reuses
//! its results for an hour, well inside the API's free limit of 100
//! requests a minute. It never fetches reddit.com's pages.
//!
//! `reddit r/rust async` searches only r/rust.
//!
//! Build it with
//! `cargo build --release -p plumb-plugin-reddit --target wasm32-unknown-unknown`,
//! then see `plugins/reddit/README.md` to install it.

use plumb_plugin::{encode, Error, Item, Query, Request, Response};
use serde::Deserialize;

/// Threads asked for.
const THREADS: usize = 8;

/// Characters of a text post shown under its title.
const EXCERPT: usize = 240;

/// The owner's Reddit app, from `config.json`.
#[derive(Debug, PartialEq)]
struct App {
    id: String,
    secret: String,
    username: String,
}

impl App {
    fn from_config(config: &serde_json::Value) -> Result<App, Error> {
        let field = |name: &str| {
            config[name]
                .as_str()
                .map(|v| v.trim().trim_start_matches("u/").to_string())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    Error::Other(format!(
                        "config.json needs {name}; see plugins/reddit/README.md"
                    ))
                })
        };
        Ok(App {
            id: field("client_id")?,
            secret: field("client_secret")?,
            username: field("username")?,
        })
    }

    /// The User-Agent Reddit's API rules ask for:
    /// `<platform>:<app id>:<version> (by /u/<username>)`.
    fn user_agent(&self) -> String {
        format!(
            "plumbsearch:{}:{} (by /u/{})",
            self.id,
            env!("CARGO_PKG_VERSION"),
            self.username
        )
    }
}

#[derive(Debug, Deserialize)]
struct Token {
    access_token: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Listing {
    data: ListingData,
}

#[derive(Debug, Deserialize)]
struct ListingData {
    children: Vec<Child>,
}

#[derive(Debug, Deserialize)]
struct Child {
    kind: String,
    data: Post,
}

#[derive(Debug, Deserialize)]
struct Post {
    title: Option<String>,
    permalink: Option<String>,
    subreddit_name_prefixed: Option<String>,
    score: Option<i64>,
    num_comments: Option<u64>,
    created_utc: Option<f64>,
    #[serde(default)]
    over_18: bool,
    #[serde(default)]
    is_self: bool,
    selftext: Option<String>,
    domain: Option<String>,
}

fn search(query: &Query) -> Result<Vec<Item>, Error> {
    let Some((subreddit, terms)) = split_subreddit(&query.terms) else {
        return Ok(Vec::new());
    };
    let app = App::from_config(&query.config)?;
    let token = token(&app)?;
    let adult = query.safe == "off";
    let found: Listing = check(
        Request::get(search_url(subreddit, terms, adult))
            .header("User-Agent", app.user_agent())
            .header("Authorization", format!("Bearer {token}"))
            .send()?,
    )?
    .json()?;
    Ok(items(found, adult))
}

/// An application-only token for the owner's app.
fn token(app: &App) -> Result<String, Error> {
    let response = Request::post(
        "https://www.reddit.com/api/v1/access_token",
        "grant_type=client_credentials",
    )
    .header("User-Agent", app.user_agent())
    .header(
        "Authorization",
        format!("Basic {}", base64(&format!("{}:{}", app.id, app.secret))),
    )
    .header("Content-Type", "application/x-www-form-urlencoded")
    .send()?;
    let token: Token = check(response)?.json()?;
    match token {
        Token {
            access_token: Some(token),
            ..
        } => Ok(token),
        Token { error, .. } => Err(Error::Other(format!(
            "Reddit gave no token ({}); check client_id and client_secret",
            error.unwrap_or_else(|| "no reason given".into())
        ))),
    }
}

/// Turns Reddit's refusals into a line the owner can act on.
fn check(response: Response) -> Result<Response, Error> {
    match response.status {
        200..=299 => Ok(response),
        401 => Err(Error::Other(
            "Reddit refused the app's keys: check client_id and client_secret, \
             and that Reddit has approved API access for the app"
                .into(),
        )),
        403 => Err(Error::Other(
            "Reddit refused the request (403): it may not have approved API access for the app"
                .into(),
        )),
        429 => Err(Error::Other(
            "Reddit says the app made too many requests; try again in a few minutes".into(),
        )),
        status => Err(Error::Status(status)),
    }
}

/// `r/name rest` searches only r/name for `rest`. `None` when there is
/// nothing to search for.
fn split_subreddit(terms: &str) -> Option<(Option<&str>, &str)> {
    let terms = terms.trim();
    let (first, rest) = terms.split_once(char::is_whitespace).unwrap_or((terms, ""));
    let name = first
        .strip_prefix("/r/")
        .or_else(|| first.strip_prefix("r/"))
        .filter(|name| {
            (2..=21).contains(&name.len())
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        });
    match name {
        Some(name) if !rest.trim().is_empty() => Some((Some(name), rest.trim())),
        Some(_) => None,
        None if terms.is_empty() => None,
        None => Some((None, terms)),
    }
}

fn search_url(subreddit: Option<&str>, terms: &str, adult: bool) -> String {
    let base = match subreddit {
        Some(name) => format!("https://oauth.reddit.com/r/{name}/search?restrict_sr=1&"),
        None => "https://oauth.reddit.com/search?".to_string(),
    };
    format!(
        "{base}q={}&type=link&sort=relevance&limit={THREADS}&raw_json=1&include_over_18={}",
        encode(terms),
        if adult { "on" } else { "off" }
    )
}

fn items(found: Listing, adult: bool) -> Vec<Item> {
    found
        .data
        .children
        .into_iter()
        .filter(|child| child.kind == "t3")
        .map(|child| child.data)
        .filter(|post| adult || !post.over_18)
        .filter_map(|post| {
            let title = post.title.filter(|t| !t.trim().is_empty())?;
            let permalink = post.permalink.filter(|p| p.starts_with('/'))?;
            let mut about = Vec::new();
            if let Some(subreddit) = post.subreddit_name_prefixed {
                about.push(subreddit);
            }
            if let Some(score) = post.score {
                about.push(format!("{score} points"));
            }
            if let Some(comments) = post.num_comments {
                about.push(format!("{comments} comments"));
            }
            if !post.is_self {
                if let Some(domain) = post.domain.filter(|d| !d.is_empty()) {
                    about.push(format!("links to {domain}"));
                }
            }
            let mut snippet = about.join(" · ");
            if let Some(text) = post.selftext.as_deref().map(excerpt) {
                if !text.is_empty() {
                    snippet.push_str(": ");
                    snippet.push_str(&text);
                }
            }
            let mut item = Item::new(title, format!("https://www.reddit.com{permalink}"));
            if !snippet.is_empty() {
                item = item.snippet(snippet);
            }
            if let Some(at) = post.created_utc.filter(|t| *t > 0.0) {
                item = item.published(at as u64);
            }
            if post.over_18 {
                item = item.badge("NSFW");
            }
            Some(item)
        })
        .collect()
}

/// The start of a text post, on one line.
fn excerpt(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= EXCERPT {
        return line;
    }
    let cut: String = line.chars().take(EXCERPT).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{cut}…")
}

/// Standard base64, for HTTP Basic authentication.
fn base64(text: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(text.len().div_ceil(3) * 4);
    for chunk in text.as_bytes().chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

plumb_plugin::plugin!(search);

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Listing {
        serde_json::from_str(include_str!("../samples/search.json")).unwrap()
    }

    #[test]
    fn lists_threads_from_a_search_answer() {
        let items = items(sample(), false);
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0].url,
            "https://www.reddit.com/r/rust/comments/1abcde/how_do_you_structure_async_code/"
        );
        assert_eq!(
            items[0].title,
            "How do you structure async code in a large project?"
        );
        assert_eq!(
            items[0].snippet.as_deref(),
            Some("r/rust · 412 points · 87 comments: We have a tokio service that grew to 40k lines & I keep going back and forth on channels vs shared state.")
        );
        assert_eq!(items[0].published, Some(1_759_000_000));
        assert_eq!(
            items[1].snippet.as_deref(),
            Some("r/programming · 1530 points · 301 comments · links to without.boats")
        );
        // The comment (t1) and the title-less post are left out.
        assert!(items.iter().all(|i| !i.title.is_empty()));
    }

    #[test]
    fn adult_threads_only_without_safe_search() {
        assert!(items(sample(), false).iter().all(|i| i.badge.is_none()));
        let all = items(sample(), true);
        assert_eq!(all.len(), 4);
        assert_eq!(all[3].badge.as_deref(), Some("NSFW"));
    }

    #[test]
    fn long_text_posts_are_cut_at_a_word() {
        let text = "word ".repeat(100);
        let cut = excerpt(&text);
        assert!(cut.ends_with("word…"));
        assert!(cut.chars().count() <= EXCERPT + 1);
    }

    #[test]
    fn r_slash_name_searches_one_subreddit() {
        assert_eq!(
            split_subreddit("r/rust async traits"),
            Some((Some("rust"), "async traits"))
        );
        assert_eq!(
            split_subreddit("/r/AskHistorians rome"),
            Some((Some("AskHistorians"), "rome"))
        );
        assert_eq!(split_subreddit("rust async"), Some((None, "rust async")));
        assert_eq!(split_subreddit("r/rust"), None);
        assert_eq!(split_subreddit("  "), None);
        // Not a subreddit name: searched as typed.
        assert_eq!(split_subreddit("r/a b"), Some((None, "r/a b")));
        assert_eq!(
            search_url(Some("rust"), "async traits", false),
            "https://oauth.reddit.com/r/rust/search?restrict_sr=1&q=async%20traits&type=link&sort=relevance&limit=8&raw_json=1&include_over_18=off"
        );
    }

    #[test]
    fn reads_the_app_from_config_and_names_it_in_the_user_agent() {
        let app = App::from_config(&serde_json::json!({
            "client_id": "abc123", "client_secret": "s3cret", "username": "u/liz"
        }))
        .unwrap();
        assert_eq!(app.user_agent(), "plumbsearch:abc123:0.1.0 (by /u/liz)");
        let missing = App::from_config(&serde_json::Value::Null).unwrap_err();
        assert!(missing.to_string().contains("client_id"));
    }

    #[test]
    fn reads_token_answers() {
        let ok: Token = serde_json::from_str(include_str!("../samples/token.json")).unwrap();
        assert_eq!(
            ok.access_token.as_deref(),
            Some("eyJhbGciOiJSUzI1NiJ9.sample")
        );
        let refused: Token = serde_json::from_str(r#"{"error": "invalid_grant"}"#).unwrap();
        assert_eq!(refused.error.as_deref(), Some("invalid_grant"));
    }

    #[test]
    fn encodes_basic_auth() {
        assert_eq!(base64("abc123:s3cret"), "YWJjMTIzOnMzY3JldA==");
        assert_eq!(base64("ab"), "YWI=");
        assert_eq!(base64("abc"), "YWJj");
        assert_eq!(base64(""), "");
    }
}
