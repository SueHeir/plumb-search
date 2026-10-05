//! The papers page set: the most cited scholarly works, listed next to the
//! sites ("attention is all you need" finds the paper).
//!
//! Papers come from OpenAlex's API (its data is CC0), most cited first, and
//! are written as an articles file ([`plumb_core::article`]): the title is
//! the work's title, the description "Paper by AUTHOR et al., YEAR, VENUE",
//! the item its DOI (`10.48550/arXiv.1706.03762`) or else its OpenAlex id
//! (`W2741809807`), from which the address is made, and the views its
//! citations. No abstract or text is kept.
//!
//! OpenAlex pages through results with a cursor, 200 works a request. Set
//! `OPENALEX_API_KEY` if OpenAlex asks for a key.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};
use serde::Deserialize;
use tracing::{info, warn};

const WORKS_URL: &str = "https://api.openalex.org/works";
/// Works a request, OpenAlex's most.
pub const PER_PAGE: usize = 200;
/// Fewest citations of a paper kept, unless asked otherwise.
pub const DEFAULT_MIN_CITATIONS: u64 = 200;
/// Pause between requests, well under OpenAlex's ten a second.
const PAUSE: Duration = Duration::from_millis(200);

#[derive(Debug, Deserialize)]
struct WorksPage {
    #[serde(default)]
    meta: Meta,
    #[serde(default)]
    results: Vec<Work>,
}

#[derive(Debug, Default, Deserialize)]
struct Meta {
    #[serde(default)]
    next_cursor: Option<String>,
}

/// One work as OpenAlex gives it.
#[derive(Debug, Clone, Deserialize)]
pub struct Work {
    pub id: String,
    #[serde(default)]
    pub doi: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub publication_year: Option<i32>,
    #[serde(default)]
    pub cited_by_count: u64,
    #[serde(default)]
    pub authorships: Vec<Authorship>,
    #[serde(default)]
    pub primary_location: Option<Location>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Authorship {
    #[serde(default)]
    pub author: Option<Named>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Location {
    #[serde(default)]
    pub source: Option<Named>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Named {
    #[serde(default)]
    pub display_name: Option<String>,
}

/// `text` without HTML tags (`<i>E. coli</i>` -> `E. coli`).
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

impl Work {
    /// The work as an articles file line (see the module docs), `None`
    /// without a title.
    pub fn to_article(&self) -> Option<Article> {
        let title = plumb_core::collapse_whitespace(&strip_tags(
            self.display_name.as_deref().unwrap_or(""),
        ));
        if title.is_empty() {
            return None;
        }
        let item = match self.doi.as_deref() {
            Some(doi) => doi
                .trim_start_matches("https://doi.org/")
                .trim_start_matches("http://doi.org/")
                .to_string(),
            None => self
                .id
                .trim_start_matches("https://openalex.org/")
                .to_string(),
        };
        let authors: Vec<&str> = self
            .authorships
            .iter()
            .filter_map(|a| a.author.as_ref()?.display_name.as_deref())
            .collect();
        let mut description = "Paper".to_string();
        if let Some(first) = authors.first() {
            description.push_str(" by ");
            description.push_str(first);
            if authors.len() > 1 {
                description.push_str(" et al.");
            }
        }
        if let Some(year) = self.publication_year {
            description.push_str(&format!(", {year}"));
        }
        if let Some(venue) = self
            .primary_location
            .as_ref()
            .and_then(|l| l.source.as_ref()?.display_name.as_deref())
        {
            description.push_str(", ");
            description.push_str(venue);
        }
        Some(Article {
            title,
            description: Some(plumb_core::truncate_chars(
                &plumb_core::collapse_whitespace(&description),
                MAX_ARTICLE_DESCRIPTION_CHARS,
            )),
            item: Some(item),
            site: None,
            views: self.cited_by_count,
            aliases: Vec::new(),
            profiles: Vec::new(),
        })
    }
}

/// Fetches works with at least `min_citations` citations, at most `limit`
/// of them, most cited first.
pub async fn fetch_papers(
    client: &reqwest::Client,
    min_citations: u64,
    limit: usize,
    api_key: Option<&str>,
) -> Result<Vec<Article>> {
    let filter = format!(
        "cited_by_count:>{},is_paratext:false",
        min_citations.saturating_sub(1)
    );
    let mut cursor = "*".to_string();
    let mut articles = Vec::new();
    let mut failures = 0u32;
    while articles.len() < limit {
        let mut params = vec![
            ("filter", filter.clone()),
            ("sort", "cited_by_count:desc".to_string()),
            ("per-page", PER_PAGE.to_string()),
            ("cursor", cursor.clone()),
            (
                "select",
                "id,doi,display_name,publication_year,cited_by_count,authorships,primary_location"
                    .to_string(),
            ),
        ];
        if let Some(key) = api_key {
            params.push(("api_key", key.to_string()));
        }
        let url = reqwest::Url::parse_with_params(WORKS_URL, &params)?;
        let response = match client.get(url).send().await {
            Ok(response) => response,
            Err(err) => {
                failures += 1;
                if failures > 10 {
                    return Err(err).context("asking OpenAlex");
                }
                warn!("asking OpenAlex: {err}; trying again");
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        let status = response.status();
        if status.as_u16() == 429 || status.is_server_error() {
            failures += 1;
            if failures > 10 {
                bail!("OpenAlex answered {status} ten times");
            }
            warn!("OpenAlex answered {status}; waiting");
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "OpenAlex answered {status}: {}",
                plumb_core::truncate_chars(&body, 300)
            );
        }
        failures = 0;
        let bytes = response
            .bytes()
            .await
            .context("reading OpenAlex's answer")?;
        let page: WorksPage =
            serde_json::from_slice(&bytes).context("reading OpenAlex's answer")?;
        let count = page.results.len();
        articles.extend(page.results.iter().filter_map(Work::to_article));
        if articles.len() % 20_000 < count {
            info!(
                "{} papers so far, down to {} citations",
                articles.len(),
                page.results.last().map_or(0, |w| w.cited_by_count)
            );
        }
        match page.meta.next_cursor {
            Some(next) if count > 0 => cursor = next,
            _ => break,
        }
        tokio::time::sleep(PAUSE).await;
    }
    articles.truncate(limit);
    Ok(articles)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"{"meta": {"count": 2, "next_cursor": "abc"}, "results": [
      {"id": "https://openalex.org/W2963403868", "doi": "https://doi.org/10.48550/arxiv.1706.03762",
       "display_name": "Attention Is All You Need", "publication_year": 2017, "cited_by_count": 90000,
       "authorships": [{"author": {"display_name": "Ashish Vaswani"}}, {"author": {"display_name": "Noam Shazeer"}}],
       "primary_location": {"source": {"display_name": "arXiv (Cornell University)"}}},
      {"id": "https://openalex.org/W1", "doi": null, "display_name": "Growth of <i>E. coli</i>",
       "publication_year": null, "cited_by_count": 300, "authorships": [], "primary_location": null},
      {"id": "https://openalex.org/W2", "display_name": null, "cited_by_count": 5}
    ]}"#;

    #[test]
    fn works_become_articles() {
        let page: WorksPage = serde_json::from_str(PAGE).unwrap();
        assert_eq!(page.meta.next_cursor.as_deref(), Some("abc"));
        let articles: Vec<Article> = page.results.iter().filter_map(Work::to_article).collect();
        assert_eq!(articles.len(), 2);
        assert_eq!(
            articles[0].item.as_deref(),
            Some("10.48550/arxiv.1706.03762")
        );
        assert_eq!(
            articles[0].description.as_deref(),
            Some("Paper by Ashish Vaswani et al., 2017, arXiv (Cornell University)")
        );
        assert_eq!(articles[0].views, 90_000);
        assert_eq!(articles[1].title, "Growth of E. coli");
        assert_eq!(articles[1].item.as_deref(), Some("W1"));
        assert_eq!(articles[1].description.as_deref(), Some("Paper"));
    }
}
