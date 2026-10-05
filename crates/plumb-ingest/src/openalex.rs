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

use std::io::Write;
use std::path::Path;
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
            website: None,
        })
    }
}

/// What [`fetch_papers`] got.
pub struct Fetched {
    /// The papers, most cited first.
    pub papers: Vec<Article>,
    /// Every paper asked for was fetched; otherwise OpenAlex kept refusing
    /// and `papers` are the most cited of them, and a run with the same
    /// progress folder carries on where this one stopped.
    pub complete: bool,
}

/// Longest wait between refused requests.
const MAX_WAIT: Duration = Duration::from_secs(30 * 60);
/// How long OpenAlex may keep refusing before the papers so far are given
/// back.
const GIVE_UP_AFTER: Duration = Duration::from_secs(3 * 60 * 60);

/// Where a fetch keeps its progress: the papers so far (an articles file,
/// one line appended a paper) and the cursor to go on from.
struct Progress {
    papers: std::path::PathBuf,
    state: std::path::PathBuf,
}

#[derive(Debug, serde::Serialize, Deserialize)]
struct ProgressState {
    filter: String,
    cursor: Option<String>,
}

impl Progress {
    fn new(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Progress {
            papers: dir.join("papers-so-far.tsv"),
            state: dir.join("papers-so-far.json"),
        })
    }

    /// The papers and cursor of an earlier fetch with `filter`, or a fresh
    /// start. `None` for the cursor means that fetch got everything.
    fn resume(&self, filter: &str) -> Result<(Vec<Article>, Option<String>)> {
        let state: Option<ProgressState> = std::fs::read(&self.state)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        if let Some(state) = state.filter(|s| s.filter == filter) {
            if let Ok(file) = std::fs::File::open(&self.papers) {
                let papers =
                    plumb_core::article::read_articles(std::io::BufReader::new(file), usize::MAX)?;
                info!("carrying on from {} papers fetched before", papers.len());
                return Ok((papers, state.cursor));
            }
        }
        std::fs::write(&self.papers, plumb_core::article::ARTICLES_HEADER)
            .with_context(|| format!("writing {}", self.papers.display()))?;
        self.save(filter, Some("*"))?;
        Ok((Vec::new(), Some("*".to_string())))
    }

    fn append(&self, papers: &[Article]) -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.papers)
            .with_context(|| format!("opening {}", self.papers.display()))?;
        let mut text = Vec::new();
        for paper in papers {
            plumb_core::article::write_article(&mut text, paper)?;
        }
        file.write_all(&text)
            .with_context(|| format!("writing {}", self.papers.display()))
    }

    fn save(&self, filter: &str, cursor: Option<&str>) -> Result<()> {
        let state = ProgressState {
            filter: filter.to_string(),
            cursor: cursor.map(str::to_string),
        };
        std::fs::write(&self.state, serde_json::to_vec(&state)?)
            .with_context(|| format!("writing {}", self.state.display()))
    }
}

/// Fetches works with at least `min_citations` citations, at most `limit`
/// of them, most cited first. With `progress`, a folder, the papers so far
/// are kept there as they come and a later fetch carries on from them.
pub async fn fetch_papers(
    client: &reqwest::Client,
    min_citations: u64,
    limit: usize,
    api_key: Option<&str>,
    progress: Option<&Path>,
) -> Result<Fetched> {
    let filter = format!(
        "cited_by_count:>{},is_paratext:false",
        min_citations.saturating_sub(1)
    );
    let progress = progress.map(Progress::new).transpose()?;
    let (mut articles, mut cursor) = match &progress {
        Some(progress) => progress.resume(&filter)?,
        None => (Vec::new(), Some("*".to_string())),
    };
    let mut failures = 0u32;
    let mut refused_since: Option<std::time::Instant> = None;
    let mut wait = Duration::from_secs(60);
    let mut complete = true;
    while articles.len() < limit {
        let Some(at) = cursor.clone() else { break };
        let mut params = vec![
            ("filter", filter.clone()),
            ("sort", "cited_by_count:desc".to_string()),
            ("per-page", PER_PAGE.to_string()),
            ("cursor", at),
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
            let since = *refused_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() > GIVE_UP_AFTER {
                warn!(
                    "OpenAlex has refused for {} minutes; keeping the {} papers so far",
                    since.elapsed().as_secs() / 60,
                    articles.len()
                );
                complete = false;
                break;
            }
            let asked = retry_after(response.headers());
            let pause = asked.unwrap_or(wait).min(MAX_WAIT);
            warn!(
                "OpenAlex answered {status}; waiting {} seconds",
                pause.as_secs()
            );
            tokio::time::sleep(pause).await;
            wait = (wait * 2).min(MAX_WAIT);
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
        refused_since = None;
        wait = Duration::from_secs(60);
        let bytes = response
            .bytes()
            .await
            .context("reading OpenAlex's answer")?;
        let page: WorksPage =
            serde_json::from_slice(&bytes).context("reading OpenAlex's answer")?;
        let count = page.results.len();
        let new: Vec<Article> = page.results.iter().filter_map(Work::to_article).collect();
        cursor = page.meta.next_cursor.filter(|_| count > 0);
        if let Some(progress) = &progress {
            progress.append(&new)?;
            progress.save(&filter, cursor.as_deref())?;
        }
        articles.extend(new);
        if articles.len() % 20_000 < count {
            info!(
                "{} papers so far, down to {} citations",
                articles.len(),
                page.results.last().map_or(0, |w| w.cited_by_count)
            );
        }
        tokio::time::sleep(PAUSE).await;
    }
    articles.truncate(limit);
    Ok(Fetched {
        papers: articles,
        complete,
    })
}

/// The wait a `Retry-After` header asks for, in seconds.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let seconds: u64 = headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds.max(1)))
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

    #[test]
    fn progress_carries_on() {
        let dir = tempfile::tempdir().unwrap();
        let progress = Progress::new(dir.path()).unwrap();
        let filter = "cited_by_count:>199";
        assert_eq!(progress.resume(filter).unwrap(), (vec![], Some("*".into())));
        let page: WorksPage = serde_json::from_str(PAGE).unwrap();
        let papers: Vec<Article> = page.results.iter().filter_map(Work::to_article).collect();
        progress.append(&papers).unwrap();
        progress.save(filter, Some("abc")).unwrap();
        let (again, cursor) = progress.resume(filter).unwrap();
        assert_eq!(again, papers);
        assert_eq!(cursor.as_deref(), Some("abc"));
        // Another filter starts over.
        assert_eq!(
            progress.resume("cited_by_count:>9").unwrap(),
            (vec![], Some("*".into()))
        );
    }
}
