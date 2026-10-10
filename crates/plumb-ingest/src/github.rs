//! The GitHub repositories page set: well-starred public repositories,
//! listed next to the sites ("ripgrep" finds BurntSushi/ripgrep).
//!
//! Repositories are taken from GitHub's repository search API, most starred
//! first, and written as an articles file ([`plumb_core::article`]): the
//! title is `owner/name`, the views are the stars, the site is the
//! registrable domain of the repository's homepage (so tauri-apps/tauri
//! goes under tauri.app's result), and the one alias is the repository's
//! name. Only what GitHub shows publicly is kept: no code, no README.
//!
//! The search API gives at most 1,000 results a query, so the stars are
//! walked down in bands: each band asks for `stars:LOW..HIGH`, most
//! starred first, and the next band ends at the stars of the last
//! repository seen ([`Bands`]). Without a token GitHub allows 10 searches
//! a minute (about 1,000 repositories), with one 30; the fetcher waits
//! whenever GitHub says to.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};
use serde::Deserialize;
use tracing::{info, warn};

/// Repositories per search page, GitHub's most.
pub const PER_PAGE: usize = 100;
/// Pages GitHub gives for one search.
pub const PAGES_PER_SEARCH: usize = 10;
/// Fewest stars of a repository kept, unless asked otherwise.
pub const DEFAULT_MIN_STARS: u64 = 500;

const SEARCH_URL: &str = "https://api.github.com/search/repositories";

/// One repository as the search API gives it.
#[derive(Debug, Clone, Deserialize)]
pub struct Repo {
    pub full_name: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub stargazers_count: u64,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub fork: bool,
}

#[derive(Debug, Deserialize)]
struct SearchPage {
    /// GitHub gave up on the search before finding everything (it times
    /// out on big star ranges), so a short page does not mean the end.
    #[serde(default)]
    incomplete_results: bool,
    /// Repositories GitHub counts for the search, of which it gives at
    /// most a thousand.
    #[serde(default)]
    total_count: Option<u64>,
    #[serde(default)]
    items: Vec<Repo>,
}

/// Times a short, incomplete page is asked again before the band is ended
/// where it got to.
const INCOMPLETE_RETRIES: u32 = 5;

impl Repo {
    /// The repository as an article line (see the module docs).
    pub fn to_article(&self) -> Article {
        let description = self
            .description
            .as_deref()
            .map(|d| {
                plumb_core::truncate_chars(
                    &plumb_core::collapse_whitespace(d),
                    MAX_ARTICLE_DESCRIPTION_CHARS,
                )
            })
            .filter(|d| !d.is_empty());
        let site = self
            .homepage
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .and_then(plumb_core::registrable_domain)
            // A homepage on GitHub itself says nothing more.
            .filter(|domain| domain != "github.com");
        let aliases = if self.name.eq_ignore_ascii_case(&self.full_name) {
            Vec::new()
        } else {
            vec![self.name.clone()]
        };
        Article {
            title: self.full_name.clone(),
            description,
            item: None,
            site,
            views: self.stargazers_count,
            aliases,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            language: None,
        }
    }
}

/// Walks the stars down in bands of at most [`PAGES_PER_SEARCH`] pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bands {
    min_stars: u64,
    /// Most stars of the current band; `None` for no bound.
    high: Option<u64>,
    /// Page of the current band asked next, from 1.
    page: usize,
    /// Stars of the last repository seen in the current band.
    last_stars: Option<u64>,
    /// Repositories the current band has given so far.
    given: usize,
    done: bool,
}

impl Bands {
    pub fn new(min_stars: u64) -> Self {
        Bands {
            min_stars,
            high: None,
            page: 1,
            last_stars: None,
            given: 0,
            done: false,
        }
    }

    /// The search to make next (`q`, `page`), `None` when all are made.
    pub fn next(&self) -> Option<(String, usize)> {
        if self.done {
            return None;
        }
        let q = match self.high {
            None => format!("stars:>={}", self.min_stars),
            Some(high) => format!("stars:{}..{high}", self.min_stars),
        };
        Some((q, self.page))
    }

    /// Ends the current band at `last_stars` as if it were used up, when
    /// GitHub keeps giving up on it.
    pub fn end_band(&mut self, last_stars: Option<u64>) {
        self.page = PAGES_PER_SEARCH;
        self.record(PER_PAGE, last_stars);
    }

    /// Whether a page of `count` repositories, for a search GitHub counts
    /// `total` for, stops before the band was given all it can give: a
    /// short page like that is not the end of the stars.
    pub fn stops_early(&self, count: usize, total: Option<u64>) -> bool {
        let Some(total) = total else {
            return false;
        };
        let can_give = total.min((PER_PAGE * PAGES_PER_SEARCH) as u64);
        count < PER_PAGE && ((self.given + count) as u64) < can_give
    }

    /// Takes in what the last search gave: `count` repositories, the last
    /// of them with `last_stars`.
    pub fn record(&mut self, count: usize, last_stars: Option<u64>) {
        if last_stars.is_some() {
            self.last_stars = last_stars;
        }
        self.given += count;
        if count < PER_PAGE {
            // The band had no more, so neither has anything below it.
            self.done = true;
            return;
        }
        if self.page < PAGES_PER_SEARCH {
            self.page += 1;
            return;
        }
        // The band is used up: the next one ends where it stopped. When a
        // whole band had the same stars, skip past them rather than ask
        // again forever.
        // A band that gave nothing at all is stepped past by one star.
        let last = self.last_stars.or(self.high).unwrap_or(self.min_stars);
        let high = if Some(last) == self.high {
            last.saturating_sub(1)
        } else {
            last
        };
        self.high = Some(high);
        self.page = 1;
        self.last_stars = None;
        self.given = 0;
        if high < self.min_stars {
            self.done = true;
        }
    }
}

/// Fetches public repositories with at least `min_stars` stars, at most
/// `limit` of them, most starred first. `token` is a GitHub token, which
/// only raises the rate limit.
pub async fn fetch_repos(
    client: &reqwest::Client,
    min_stars: u64,
    limit: usize,
    token: Option<&str>,
) -> Result<Vec<Article>> {
    let mut bands = Bands::new(min_stars);
    let mut seen: HashSet<String> = HashSet::new();
    let mut articles = Vec::new();
    let mut failures = 0u32;
    let mut incomplete = 0u32;
    while let Some((q, page)) = bands.next() {
        if articles.len() >= limit {
            break;
        }
        let url = reqwest::Url::parse_with_params(
            SEARCH_URL,
            &[
                ("q", q.as_str()),
                ("sort", "stars"),
                ("order", "desc"),
                ("per_page", &PER_PAGE.to_string()),
                ("page", &page.to_string()),
            ],
        )?;
        let mut request = client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => {
                failures += 1;
                if failures > 10 {
                    return Err(err).context("searching GitHub");
                }
                warn!("searching GitHub ({q} page {page}): {err}; trying again");
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        let status = response.status();
        let wait = rate_limit_wait(response.headers(), plumb_core::now_unix());
        if status.as_u16() == 403 || status.as_u16() == 429 {
            // A 403 without rate limit headers may be a secondary limit
            // (GitHub says to wait a minute) or a refusal that waiting will
            // not end (a revoked token, a blocked address), so it counts
            // toward giving up.
            if status.as_u16() == 403 && wait.is_none() {
                failures += 1;
                if failures > 10 {
                    bail!("GitHub keeps answering {status} to {q} page {page}");
                }
            }
            let wait = wait.unwrap_or(Duration::from_secs(60));
            info!("GitHub asks to wait {}s", wait.as_secs());
            tokio::time::sleep(wait).await;
            continue;
        }
        if status.as_u16() == 422 && page > 1 {
            // Past the last page GitHub allows; the band is used up.
            warn!("GitHub has no page {page} of {q}; going on below it");
            bands.end_band(None);
            continue;
        }
        if !status.is_success() {
            failures += 1;
            if failures > 10 {
                bail!("GitHub answered {status} to {q} page {page}");
            }
            warn!("GitHub answered {status} to {q} page {page}; trying again");
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        }
        failures = 0;
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("reading GitHub's answer to {q} page {page}"))?;
        let body: SearchPage = serde_json::from_slice(&bytes)
            .with_context(|| format!("reading GitHub's answer to {q} page {page}"))?;
        let count = body.items.len();
        let last_stars = body.items.last().map(|repo| repo.stargazers_count);
        // A short page is the end only when GitHub gave all it counts.
        let cut_short = count < PER_PAGE
            && (body.incomplete_results || bands.stops_early(count, body.total_count));
        for repo in body.items {
            if repo.fork || !seen.insert(repo.full_name.to_lowercase()) {
                continue;
            }
            articles.push(repo.to_article());
        }
        if cut_short {
            incomplete += 1;
            if incomplete <= INCOMPLETE_RETRIES {
                warn!(
                    "GitHub gave {count} on {q} page {page} of {:?} (incomplete: {}); asking again",
                    body.total_count, body.incomplete_results
                );
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
            warn!("GitHub keeps giving up on {q}; going on below {last_stars:?} stars");
            incomplete = 0;
            bands.end_band(last_stars);
            continue;
        }
        incomplete = 0;
        if count < PER_PAGE {
            info!(
                "GitHub has no more after {q} page {page} ({count} on the page, {:?} counted)",
                body.total_count
            );
        }
        bands.record(count, last_stars);
        if articles.len() % 10_000 < count {
            info!(
                "{} repositories so far, down to {} stars",
                articles.len(),
                last_stars.unwrap_or(0)
            );
        }
        if let Some(wait) = wait {
            tokio::time::sleep(wait).await;
        }
    }
    articles.sort_by(|a, b| b.views.cmp(&a.views).then_with(|| a.title.cmp(&b.title)));
    articles.truncate(limit);
    Ok(articles)
}

/// How long to wait before the next search, from GitHub's rate limit
/// headers: until the reset when no searches are left, else nothing.
fn rate_limit_wait(headers: &reqwest::header::HeaderMap, now: u64) -> Option<Duration> {
    let number =
        |name: &str| -> Option<u64> { headers.get(name)?.to_str().ok()?.trim().parse().ok() };
    if let Some(after) = number("retry-after") {
        return Some(Duration::from_secs(after.max(1)));
    }
    if number("x-ratelimit-remaining")? > 0 {
        return None;
    }
    let reset = number("x-ratelimit-reset")?;
    Some(Duration::from_secs(reset.saturating_sub(now) + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repos_become_articles() {
        let repo = Repo {
            full_name: "tauri-apps/tauri".into(),
            name: "tauri".into(),
            description: Some("Build smaller,\n faster, and more secure apps".into()),
            stargazers_count: 90_000,
            homepage: Some("https://tauri.app".into()),
            fork: false,
        };
        let article = repo.to_article();
        assert_eq!(article.title, "tauri-apps/tauri");
        assert_eq!(article.site.as_deref(), Some("tauri.app"));
        assert_eq!(article.views, 90_000);
        assert_eq!(article.aliases, ["tauri"]);
        assert_eq!(
            article.description.as_deref(),
            Some("Build smaller, faster, and more secure apps")
        );
        let on_github = Repo {
            homepage: Some("https://github.com/x/y/wiki".into()),
            description: Some("  ".into()),
            ..repo
        };
        let article = on_github.to_article();
        assert_eq!(article.site, None);
        assert_eq!(article.description, None);
    }

    #[test]
    fn bands_walk_the_stars_down() {
        let mut bands = Bands::new(500);
        assert_eq!(bands.next(), Some(("stars:>=500".to_string(), 1)));
        for page in 2..=PAGES_PER_SEARCH {
            bands.record(PER_PAGE, Some(90_000));
            assert_eq!(bands.next().unwrap().1, page);
        }
        bands.record(PER_PAGE, Some(30_000));
        assert_eq!(bands.next(), Some(("stars:500..30000".to_string(), 1)));
        // A whole band with the same stars moves past them.
        for _ in 0..PAGES_PER_SEARCH {
            bands.record(PER_PAGE, Some(30_000));
        }
        assert_eq!(bands.next(), Some(("stars:500..29999".to_string(), 1)));
        // A band GitHub keeps giving up on ends where it got to.
        bands.end_band(Some(20_000));
        assert_eq!(bands.next(), Some(("stars:500..20000".to_string(), 1)));
        // A band that gave nothing steps past its top star.
        bands.end_band(None);
        assert_eq!(bands.next(), Some(("stars:500..19999".to_string(), 1)));
        // A short page ends it all.
        bands.record(40, Some(510));
        assert_eq!(bands.next(), None);
    }

    #[test]
    fn short_pages_short_of_the_count_are_not_the_end() {
        let mut bands = Bands::new(500);
        // GitHub counts plenty but gives nothing: not the end.
        assert!(bands.stops_early(0, Some(91_320)));
        assert!(!bands.stops_early(PER_PAGE, Some(91_320)));
        for _ in 0..3 {
            bands.record(PER_PAGE, Some(2_100));
        }
        // 340 of 340: the band is done.
        assert!(!bands.stops_early(40, Some(340)));
        assert!(bands.stops_early(40, Some(341)));
        // No count, no telling.
        assert!(!bands.stops_early(0, None));
    }

    #[test]
    fn waits_as_github_says() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "3".parse().unwrap());
        headers.insert("x-ratelimit-reset", "1100".parse().unwrap());
        assert_eq!(rate_limit_wait(&headers, 1000), None);
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        assert_eq!(
            rate_limit_wait(&headers, 1000),
            Some(Duration::from_secs(101))
        );
        headers.insert("retry-after", "7".parse().unwrap());
        assert_eq!(
            rate_limit_wait(&headers, 1000),
            Some(Duration::from_secs(7))
        );
    }
}
