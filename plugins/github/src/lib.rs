//! A Plumb Search plugin for GitHub, through its official API
//! (<https://docs.github.com/en/rest>):
//!
//! - `gh rust http client` or `tokio github` finds repositories, with their
//!   stars, language and last push;
//! - GitHub repositories among the node's own results get a star count
//!   badge;
//! - a browser extension can ask it about a repository's page.
//!
//! With a token in `config.json` (see `config.example.json`) it also shows
//! which repositories you have starred, gives the node's owner Star and
//! Unstar buttons, and searches with GitHub's higher limits. Without one it
//! only searches, within GitHub's 10 searches a minute for anyone.
//!
//! Build it with
//! `cargo build --release -p plumb-plugin-github --target wasm32-unknown-unknown`,
//! then see `docs/plugins.md` to install it.

use std::collections::BTreeMap;

use plumb_plugin::{encode, ActInput, Action, Error, Item, Note, Query, Request, Shown};
use serde::Deserialize;
use serde_json::json;

/// Repositories asked for in a search.
const REPOSITORIES: usize = 8;

/// Repositories among the node's results looked up at once.
const MARKED_UP: usize = 30;

/// First path segments of github.com pages that are not someone's account.
const NOT_OWNERS: &[&str] = &[
    "about",
    "apps",
    "collections",
    "customer-stories",
    "enterprise",
    "events",
    "explore",
    "features",
    "login",
    "marketplace",
    "new",
    "notifications",
    "orgs",
    "pricing",
    "readme",
    "search",
    "security",
    "settings",
    "site",
    "sponsors",
    "topics",
    "trending",
];

/// A repository, from either API.
#[derive(Debug, Clone, PartialEq)]
struct Repo {
    /// `owner/name`.
    full_name: String,
    url: String,
    description: Option<String>,
    stars: u64,
    language: Option<String>,
    /// `2026-09-30T12:00:00Z`.
    pushed_at: Option<String>,
    archived: bool,
    avatar: Option<String>,
    /// Whether the token's owner starred it; `None` without a token.
    starred: Option<bool>,
}

// The REST search API, used without a token.

#[derive(Debug, Deserialize)]
struct RestFound {
    items: Vec<RestRepo>,
}

#[derive(Debug, Deserialize)]
struct RestRepo {
    full_name: String,
    html_url: String,
    description: Option<String>,
    #[serde(default)]
    stargazers_count: u64,
    language: Option<String>,
    pushed_at: Option<String>,
    #[serde(default)]
    archived: bool,
    owner: Option<RestOwner>,
}

#[derive(Debug, Deserialize)]
struct RestOwner {
    avatar_url: Option<String>,
}

impl From<RestRepo> for Repo {
    fn from(repo: RestRepo) -> Self {
        Repo {
            full_name: repo.full_name,
            url: repo.html_url,
            description: repo.description,
            stars: repo.stargazers_count,
            language: repo.language,
            pushed_at: repo.pushed_at,
            archived: repo.archived,
            avatar: repo
                .owner
                .and_then(|owner| owner.avatar_url)
                .map(|url| sized_avatar(&url)),
            starred: None,
        }
    }
}

// The GraphQL API, used with a token: it says what you starred.

/// The fields asked for of each repository.
const FIELDS: &str = "nameWithOwner url description stargazerCount \
    primaryLanguage { name } pushedAt isArchived viewerHasStarred \
    owner { avatarUrl(size: 64) }";

#[derive(Debug, Deserialize)]
struct GraphRepo {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
    url: String,
    description: Option<String>,
    #[serde(rename = "stargazerCount", default)]
    stars: u64,
    #[serde(rename = "primaryLanguage")]
    language: Option<GraphLanguage>,
    #[serde(rename = "pushedAt")]
    pushed_at: Option<String>,
    #[serde(rename = "isArchived", default)]
    archived: bool,
    #[serde(rename = "viewerHasStarred", default)]
    starred: bool,
    owner: Option<GraphOwner>,
}

#[derive(Debug, Deserialize)]
struct GraphLanguage {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GraphOwner {
    #[serde(rename = "avatarUrl")]
    avatar_url: Option<String>,
}

impl From<GraphRepo> for Repo {
    fn from(repo: GraphRepo) -> Self {
        Repo {
            full_name: repo.name_with_owner,
            url: repo.url,
            description: repo.description,
            stars: repo.stars,
            language: repo.language.map(|language| language.name),
            pushed_at: repo.pushed_at,
            archived: repo.archived,
            avatar: repo.owner.and_then(|owner| owner.avatar_url),
            starred: Some(repo.starred),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphAnswer<T> {
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct GraphSearch {
    search: GraphNodes,
}

#[derive(Debug, Deserialize)]
struct GraphNodes {
    // A node that is not a repository comes back as `{}`.
    nodes: Vec<Option<serde_json::Value>>,
}

fn token(config: &serde_json::Value) -> Option<String> {
    config["token"]
        .as_str()
        .map(str::trim)
        .filter(|token| !token.is_empty() && token.chars().all(|c| c.is_ascii_graphic()))
        .map(String::from)
}

fn graphql(token: &str, query: &str, variables: serde_json::Value) -> Result<Vec<u8>, Error> {
    let body = json!({ "query": query, "variables": variables }).to_string();
    let response = Request::post("https://api.github.com/graphql", body)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .send()?
        .ok()?;
    Ok(response.body)
}

fn search(query: &Query) -> Result<Vec<Item>, Error> {
    let token = token(&query.config);
    if let Some(page) = &query.page {
        let Some(token) = token else {
            return Ok(Vec::new());
        };
        let Some(repo) = repository(page) else {
            return Ok(Vec::new());
        };
        let found = look_up(&token, &[repo])?;
        return Ok(found.into_values().map(|repo| item(repo, true)).collect());
    }
    let terms = query.terms.trim();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let repos: Vec<Repo> = match &token {
        Some(token) => {
            let query = format!(
                "query($q: String!) {{ search(query: $q, type: REPOSITORY, first: {REPOSITORIES}) \
                 {{ nodes {{ ... on Repository {{ {FIELDS} }} }} }} }}"
            );
            let body = graphql(token, &query, json!({ "q": terms }))?;
            searched(&body)?
        }
        None => {
            let url = format!(
                "https://api.github.com/search/repositories?q={}&per_page={REPOSITORIES}",
                encode(terms)
            );
            let found: RestFound = Request::get(url)
                .header("Accept", "application/vnd.github+json")
                .send()?
                .json()?;
            found.items.into_iter().map(Repo::from).collect()
        }
    };
    Ok(repos
        .into_iter()
        .map(|repo| item(repo, token.is_some()))
        .collect())
}

fn searched(body: &[u8]) -> Result<Vec<Repo>, Error> {
    let answer: GraphAnswer<GraphSearch> = serde_json::from_slice(body)?;
    let Some(data) = answer.data else {
        return Err(Error::Other("GitHub answered with no data".into()));
    };
    Ok(data
        .search
        .nodes
        .into_iter()
        .flatten()
        .filter_map(|node| serde_json::from_value::<GraphRepo>(node).ok())
        .map(Repo::from)
        .collect())
}

fn item(repo: Repo, buttons: bool) -> Item {
    let mut about = Vec::new();
    if let Some(description) = repo.description.as_deref().filter(|d| !d.trim().is_empty()) {
        about.push(description.trim().to_string());
    }
    about.push(format!("{} stars", count(repo.stars)));
    if let Some(language) = &repo.language {
        about.push(language.clone());
    }
    if let Some(day) = repo.pushed_at.as_deref().and_then(|at| at.get(..10)) {
        about.push(format!("last push {day}"));
    }
    let mut item = Item::new(repo.full_name.clone(), repo.url.clone()).snippet(about.join(" · "));
    if let Some(avatar) = &repo.avatar {
        item = item.image(avatar.clone());
    }
    if let Some(badge) = badge(&repo, false) {
        item = item.badge(badge);
    }
    if buttons {
        if let Some(action) = star_button(&repo) {
            item = item.action(action);
        }
    }
    item
}

/// "Starred", "Archived", and with `stars` the star count too.
fn badge(repo: &Repo, stars: bool) -> Option<String> {
    let mut parts = Vec::new();
    if repo.starred == Some(true) {
        parts.push("Starred".to_string());
    }
    if repo.archived {
        parts.push("Archived".to_string());
    }
    if stars {
        parts.push(format!("★ {}", count(repo.stars)));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn star_button(repo: &Repo) -> Option<Action> {
    let starred = repo.starred?;
    let label = if starred { "Unstar" } else { "Star" };
    Some(Action::new(
        label,
        json!({ "repo": repo.full_name, "star": !starred }),
    ))
}

fn annotate(shown: &Shown) -> Result<Vec<Note>, Error> {
    // Without a token every search would spend GitHub's small allowance for
    // anyone, so the node's own results are marked up only with one.
    let Some(token) = token(&shown.config) else {
        return Ok(Vec::new());
    };
    let mut wanted: Vec<(u32, (String, String))> = Vec::new();
    for result in &shown.results {
        if let Some(repo) = repository(&result.url) {
            wanted.push((result.id, repo));
        }
    }
    let mut repos: Vec<(String, String)> = wanted.iter().map(|(_, repo)| repo.clone()).collect();
    repos.sort();
    repos.dedup();
    repos.truncate(MARKED_UP);
    if repos.is_empty() {
        return Ok(Vec::new());
    }
    let found = look_up(&token, &repos)?;
    Ok(notes(&wanted, &found))
}

fn notes(wanted: &[(u32, (String, String))], found: &BTreeMap<String, Repo>) -> Vec<Note> {
    wanted
        .iter()
        .filter_map(|(id, (owner, name))| {
            let repo = found.get(&format!("{owner}/{name}").to_lowercase())?;
            let mut note = Note::new(*id).badge(badge(repo, true)?);
            if let Some(action) = star_button(repo) {
                note = note.action(action);
            }
            Some(note)
        })
        .collect()
}

/// Looks up `repos` in one request, by their `owner/name` in lowercase.
fn look_up(token: &str, repos: &[(String, String)]) -> Result<BTreeMap<String, Repo>, Error> {
    let body = graphql(token, &lookup_query(repos), lookup_variables(repos))?;
    looked_up(&body)
}

fn lookup_query(repos: &[(String, String)]) -> String {
    let mut parameters = Vec::new();
    let mut fields = Vec::new();
    for i in 0..repos.len() {
        parameters.push(format!("$o{i}: String!, $n{i}: String!"));
        fields.push(format!(
            "r{i}: repository(owner: $o{i}, name: $n{i}) {{ {FIELDS} }}"
        ));
    }
    format!(
        "query({}) {{ {} }}",
        parameters.join(", "),
        fields.join(" ")
    )
}

fn lookup_variables(repos: &[(String, String)]) -> serde_json::Value {
    let mut variables = serde_json::Map::new();
    for (i, (owner, name)) in repos.iter().enumerate() {
        variables.insert(format!("o{i}"), json!(owner));
        variables.insert(format!("n{i}"), json!(name));
    }
    serde_json::Value::Object(variables)
}

fn looked_up(body: &[u8]) -> Result<BTreeMap<String, Repo>, Error> {
    // A repository that does not exist is `null` beside an error, and the
    // rest still come back.
    let answer: GraphAnswer<BTreeMap<String, Option<GraphRepo>>> = serde_json::from_slice(body)?;
    Ok(answer
        .data
        .unwrap_or_default()
        .into_values()
        .flatten()
        .map(|repo| {
            let repo = Repo::from(repo);
            (repo.full_name.to_lowercase(), repo)
        })
        .collect())
}

/// The owner and name of the repository a github.com address is in.
fn repository(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let path = rest.strip_prefix("github.com/")?;
    let path = path.split(['?', '#']).next()?;
    let mut segments = path.split('/');
    let owner = segments.next()?;
    let name = segments.next()?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    let owner_ok = !owner.is_empty()
        && owner.len() <= 39
        && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !NOT_OWNERS.contains(&owner.to_ascii_lowercase().as_str());
    let name_ok = !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (owner_ok && name_ok).then(|| (owner.to_string(), name.to_string()))
}

fn act(input: &ActInput) -> Result<String, Error> {
    let token = token(&input.config)
        .ok_or_else(|| Error::Other("config.json needs a GitHub token as \"token\"".into()))?;
    let full_name = input.data["repo"].as_str().unwrap_or("");
    let star = input.data["star"].as_bool().unwrap_or(true);
    let (owner, name) = repository(&format!("https://github.com/{full_name}"))
        .ok_or_else(|| Error::Other("not a repository".into()))?;
    let url = format!(
        "https://api.github.com/user/starred/{}/{}",
        encode(&owner),
        encode(&name)
    );
    let request = if star {
        Request::put(url, "")
    } else {
        Request::delete(url)
    };
    request
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .send()?
        .ok()?;
    Ok(if star {
        format!("Starred {owner}/{name}.")
    } else {
        format!("Unstarred {owner}/{name}.")
    })
}

/// 950, 1.2k, 12k, 1.3M.
fn count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => tenths(n, 1_000, "k"),
        10_000..=999_999 => format!("{}k", n / 1_000),
        _ => tenths(n, 1_000_000, "M"),
    }
}

fn tenths(n: u64, unit: u64, suffix: &str) -> String {
    let tenths = n * 10 / unit;
    if tenths.is_multiple_of(10) {
        format!("{}{suffix}", tenths / 10)
    } else {
        format!("{}.{}{suffix}", tenths / 10, tenths % 10)
    }
}

/// A small avatar: GitHub's avatar addresses take a size.
fn sized_avatar(url: &str) -> String {
    if url.contains('?') {
        format!("{url}&s=64")
    } else {
        format!("{url}?s=64")
    }
}

plumb_plugin::plugin!(search, act);
plumb_plugin::annotate!(annotate);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_repositories_from_github_addresses() {
        let repo = |url| repository(url);
        assert_eq!(
            repo("https://github.com/tokio-rs/tokio"),
            Some(("tokio-rs".into(), "tokio".into()))
        );
        assert_eq!(
            repo("https://www.github.com/SueHeir/plumb-search/pull/12?x=1"),
            Some(("SueHeir".into(), "plumb-search".into()))
        );
        assert_eq!(
            repo("https://github.com/rust-lang/rust.git"),
            Some(("rust-lang".into(), "rust".into()))
        );
        assert_eq!(repo("https://github.com/tokio-rs"), None);
        assert_eq!(repo("https://github.com/topics/rust"), None);
        assert_eq!(repo("https://github.com/a/.."), None);
        assert_eq!(repo("https://gist.github.com/a/b"), None);
        assert_eq!(repo("https://example.org/github.com/a/b"), None);
    }

    #[test]
    fn counts_stars_briefly() {
        assert_eq!(count(950), "950");
        assert_eq!(count(1_000), "1k");
        assert_eq!(count(1_234), "1.2k");
        assert_eq!(count(12_345), "12k");
        assert_eq!(count(1_300_000), "1.3M");
    }

    #[test]
    fn lists_rest_search_results_without_buttons() {
        let found: RestFound = serde_json::from_str(
            r#"{"total_count":1,"items":[{"full_name":"tokio-rs/tokio",
                "html_url":"https://github.com/tokio-rs/tokio",
                "description":"A runtime for async Rust","stargazers_count":28500,
                "language":"Rust","pushed_at":"2026-09-30T12:00:00Z","archived":false,
                "owner":{"avatar_url":"https://avatars.githubusercontent.com/u/20248544?v=4"}}]}"#,
        )
        .unwrap();
        let repo = Repo::from(found.items.into_iter().next().unwrap());
        let item = item(repo, false);
        assert_eq!(item.title, "tokio-rs/tokio");
        assert_eq!(
            item.snippet.as_deref(),
            Some("A runtime for async Rust · 28k stars · Rust · last push 2026-09-30")
        );
        assert_eq!(
            item.image.as_deref(),
            Some("https://avatars.githubusercontent.com/u/20248544?v=4&s=64")
        );
        assert_eq!(item.badge, None);
        assert!(item.actions.is_empty());
    }

    #[test]
    fn graphql_search_says_what_you_starred() {
        let body = br#"{"data":{"search":{"nodes":[
            {"nameWithOwner":"tokio-rs/tokio","url":"https://github.com/tokio-rs/tokio",
             "description":null,"stargazerCount":28500,"primaryLanguage":{"name":"Rust"},
             "pushedAt":"2026-09-30T12:00:00Z","isArchived":false,"viewerHasStarred":true,
             "owner":{"avatarUrl":"https://avatars.githubusercontent.com/u/1?s=64"}},
            {}
        ]}}}"#;
        let repos = searched(body).unwrap();
        assert_eq!(repos.len(), 1);
        let item = item(repos[0].clone(), true);
        assert_eq!(item.badge.as_deref(), Some("Starred"));
        assert_eq!(item.actions[0].label, "Unstar");
        assert_eq!(
            item.actions[0].data,
            json!({"repo": "tokio-rs/tokio", "star": false})
        );
    }

    #[test]
    fn marks_up_repositories_among_results() {
        let repos = vec![
            ("tokio-rs".to_string(), "tokio".to_string()),
            ("gone".to_string(), "missing".to_string()),
        ];
        let query = lookup_query(&repos);
        assert!(query.starts_with(
            "query($o0: String!, $n0: String!, $o1: String!, $n1: String!) { r0: repository(owner: $o0, name: $n0)"
        ));
        assert_eq!(lookup_variables(&repos)["n1"], "missing");

        let body = br#"{"data":{"r0":{"nameWithOwner":"Tokio-rs/tokio",
            "url":"https://github.com/tokio-rs/tokio","description":"x","stargazerCount":1234,
            "primaryLanguage":null,"pushedAt":null,"isArchived":true,"viewerHasStarred":false,
            "owner":null},"r1":null},
            "errors":[{"type":"NOT_FOUND","message":"Could not resolve"}]}"#;
        let found = looked_up(body).unwrap();
        let wanted = vec![(7, repos[0].clone()), (8, repos[1].clone())];
        let notes = notes(&wanted, &found);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].id, 7);
        assert_eq!(notes[0].badge.as_deref(), Some("Archived, ★ 1.2k"));
        assert_eq!(notes[0].actions[0].label, "Star");
    }

    #[test]
    fn needs_a_token_to_mark_up_results() {
        let shown = Shown {
            query: "tokio".into(),
            results: Vec::new(),
            config: serde_json::Value::Null,
        };
        assert!(annotate(&shown).unwrap().is_empty());
        assert_eq!(token(&json!({"token": "  "})), None);
        assert_eq!(
            token(&json!({"token": "ghp_abc"})).as_deref(),
            Some("ghp_abc")
        );
    }
}
