//! The papers page set: the most cited scholarly works, listed next to the
//! sites ("attention is all you need" finds the paper).
//!
//! Papers come from OpenAlex's API (its data is CC0), most cited first, and
//! are written as an articles file ([`plumb_core::article`]): the title is
//! the work's title, the description "Paper by AUTHOR et al., YEAR, VENUE",
//! the item its DOI (`10.48550/arXiv.1706.03762`) or else its OpenAlex id
//! (`W2741809807`), from which the address is made, and the views its
//! citations. No abstract or text is kept. A work that can be read free
//! keeps where (its `website`, see [`free_copy`]): its arXiv copy, or the
//! best free copy Unpaywall's data in OpenAlex knows of (the publisher's
//! open version, a university repository, PubMed Central). A work cited more often a year
//! than any real paper (OpenAlex credits a 2020 plasma camera paper with
//! 800,000 citations) is left out as a data error.
//!
//! OpenAlex pages through results with a cursor, 200 works a request. Set
//! `OPENALEX_API_KEY` if OpenAlex asks for a key.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};
use plumb_core::papers::{valid_date, PaperCountKind, PaperMetadata, MAX_PAPER_AUTHORS};
use serde::Deserialize;
use tracing::{info, warn};

const WORKS_URL: &str = "https://api.openalex.org/works";
/// Supported maximum; the legacy 200-row behavior is deprecated.
pub const PER_PAGE: usize = 100;
/// Fewest citations of a paper kept, unless asked otherwise.
pub const DEFAULT_MIN_CITATIONS: u64 = 200;
/// Most citations a year a paper is believed to have: the most cited
/// papers of recent years get about 25,000.
const MAX_CITATIONS_A_YEAR: u64 = 40_000;
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
    pub publication_date: Option<String>,
    #[serde(default)]
    pub cited_by_count: u64,
    #[serde(default)]
    pub authorships: Vec<Authorship>,
    #[serde(default)]
    pub primary_location: Option<Location>,
    #[serde(default)]
    pub open_access: Option<OpenAccess>,
    #[serde(default)]
    pub best_oa_location: Option<Location>,
    #[serde(default)]
    pub locations: Vec<Location>,
    #[serde(default)]
    pub primary_topic: Option<PrimaryTopic>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrimaryTopic {
    #[serde(default)]
    pub domain: Option<TopicDomain>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TopicDomain {
    pub id: String,
}

/// Whether a work is free to read and where, from Unpaywall's data.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenAccess {
    #[serde(default)]
    pub is_oa: bool,
    #[serde(default)]
    pub oa_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Authorship {
    #[serde(default)]
    pub author: Option<Named>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Location {
    #[serde(default)]
    pub source: Option<Named>,
    #[serde(default)]
    pub is_oa: bool,
    #[serde(default)]
    pub landing_page_url: Option<String>,
    #[serde(default)]
    pub pdf_url: Option<String>,
    /// `publishedVersion`, `acceptedVersion` or `submittedVersion`.
    #[serde(default)]
    pub version: Option<String>,
}

/// `url` as an `https://` address that fits an articles file, or `None`.
/// arXiv's and most repositories' `http://` addresses also answer on
/// `https://`.
fn web_address(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() || !host.contains('.') || url.contains(['\t', '\n', '\r', '|', ' ']) {
        return None;
    }
    Some(
        if url.starts_with("http://") && host.ends_with("arxiv.org") {
            format!("https://{rest}")
        } else {
            url.to_string()
        },
    )
}

/// The arXiv id of a location on arXiv (`1706.03762`, `hep-th/9711200`),
/// from its address, without a version.
fn arxiv_id(location: &Location) -> Option<String> {
    [&location.landing_page_url, &location.pdf_url]
        .into_iter()
        .flatten()
        .find_map(|url| {
            let rest = url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_start_matches("www.")
                .strip_prefix("arxiv.org/")?;
            let id = rest
                .strip_prefix("abs/")
                .or_else(|| rest.strip_prefix("pdf/"))?
                .trim_end_matches(".pdf");
            let id = match id.rfind('v') {
                Some(i) if i + 1 < id.len() && id[i + 1..].bytes().all(|b| b.is_ascii_digit()) => {
                    &id[..i]
                }
                _ => id,
            };
            (!id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/' | b'-')))
            .then(|| id.to_string())
        })
}

/// Where `work` can be read free, if anywhere: the publisher's own free
/// PDF, else its arXiv page (full text, and never moves), else the best
/// free copy Unpaywall's data knows of (a PDF before a page), else
/// OpenAlex's free address for it.
pub fn free_copy(work: &Work) -> Option<String> {
    let best = work.best_oa_location.as_ref().filter(|l| l.is_oa);
    if let Some(pdf) = best
        .filter(|l| l.version.as_deref() == Some("publishedVersion"))
        .and_then(|l| l.pdf_url.as_deref())
        .and_then(web_address)
    {
        return Some(pdf);
    }
    if let Some(id) = work.locations.iter().find_map(arxiv_id) {
        return Some(format!("https://arxiv.org/abs/{id}"));
    }
    best.into_iter()
        .flat_map(|l| [&l.pdf_url, &l.landing_page_url])
        .chain([&work
            .open_access
            .as_ref()
            .filter(|oa| oa.is_oa)
            .and_then(|oa| oa.oa_url.clone())])
        .flatten()
        .find_map(|url| web_address(url))
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
        if title.is_empty() || title.chars().count() > 2000 {
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
        // A paper whose own address (an arXiv DOI) is already free keeps
        // no other.
        let website = if item.to_ascii_lowercase().starts_with(ARXIV_DOI) {
            None
        } else {
            free_copy(self)
        };
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
            website,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            language: None,
            paper: Some(PaperMetadata {
                doi: self.doi.as_ref().map(|doi| {
                    doi.trim_start_matches("https://doi.org/")
                        .trim_start_matches("http://doi.org/")
                        .to_ascii_lowercase()
                }),
                openalex_id: Some(
                    self.id
                        .trim_start_matches("https://openalex.org/")
                        .to_string(),
                ),
                arxiv_id: self
                    .doi
                    .as_ref()
                    .and_then(|doi| {
                        doi.to_ascii_lowercase()
                            .split_once(ARXIV_DOI)
                            .map(|(_, id)| id.to_string())
                    })
                    .or_else(|| self.locations.iter().find_map(arxiv_id)),
                authors: authors
                    .iter()
                    .take(MAX_PAPER_AUTHORS)
                    .map(|a| a.to_string())
                    .collect(),
                publication_date: self
                    .publication_date
                    .as_ref()
                    .filter(|date| valid_date(date))
                    .cloned(),
                publication_year: self.publication_year,
                raw_publication_date: self.publication_date.clone(),
                venue: self
                    .primary_location
                    .as_ref()
                    .and_then(|l| l.source.as_ref()?.display_name.clone()),
                source: "openalex".into(),
                count: self.cited_by_count,
                count_kind: PaperCountKind::Citations,
                alternate_urls: free_copy(self).into_iter().collect(),
                ..PaperMetadata::default()
            }),
        })
    }
}

/// The DOI prefix arXiv gives its papers, which lead to the paper on arXiv.
pub const ARXIV_DOI: &str = "10.48550/arxiv.";

/// The year of a paper written by [`Work::to_article`], from its
/// description ("Paper by A et al., 2017, Venue").
fn year_of(paper: &Article) -> Option<u64> {
    if let Some(year) = paper.paper.as_ref().and_then(|m| m.publication_year) {
        return u64::try_from(year).ok();
    }
    paper
        .description
        .as_deref()?
        .split(", ")
        .skip(1)
        .find(|part| part.len() == 4 && part.bytes().all(|b| b.is_ascii_digit()))?
        .parse()
        .ok()
}

/// Whether a paper's citations could be real in `this_year`: no more than
/// [`MAX_CITATIONS_A_YEAR`] for each year since it came out.
fn plausible(paper: &Article, this_year: u64) -> bool {
    let Some(year) = year_of(paper) else {
        return true;
    };
    let years = this_year.saturating_sub(year).max(1);
    paper.views <= years.saturating_mul(MAX_CITATIONS_A_YEAR)
}

/// The year now.
fn this_year() -> u64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    1970 + secs / 31_556_952
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
        // Written aside and renamed over, so a crash mid-write leaves the
        // last state whole.
        let tmp = self.state.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&state)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.state)
            .with_context(|| format!("saving {}", self.state.display()))
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
    // What the progress folder is kept for: papers fetched before free
    // copies were kept are fetched again.
    let key = format!("{filter};paper-metadata-v1;per-page={PER_PAGE}");
    let progress = progress.map(Progress::new).transpose()?;
    let (mut articles, mut cursor) = match &progress {
        Some(progress) => progress.resume(&key)?,
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
                "id,doi,display_name,publication_year,publication_date,cited_by_count,authorships,primary_location,\
                 open_access,best_oa_location,locations"
                    .to_string(),
            ),
        ];
        if let Some(key) = api_key {
            params.push(("api_key", key.to_string()));
        }
        let url = reqwest::Url::parse_with_params(WORKS_URL, &params)?;
        // The URL carries the API key, so it is left out of errors.
        let response = match client.get(url).send().await {
            Ok(response) => response,
            Err(err) => {
                let err = err.without_url();
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
            .map_err(reqwest::Error::without_url)
            .context("reading OpenAlex's answer")?;
        let page: WorksPage =
            serde_json::from_slice(&bytes).context("reading OpenAlex's answer")?;
        let count = page.results.len();
        let new: Vec<Article> = page.results.iter().filter_map(Work::to_article).collect();
        cursor = page.meta.next_cursor.filter(|_| count > 0);
        if let Some(progress) = &progress {
            progress.append(&new)?;
            progress.save(&key, cursor.as_deref())?;
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
    let now = this_year();
    let before = articles.len();
    articles.retain(|paper| plausible(paper, now));
    if articles.len() < before {
        info!(
            "left out {} papers cited implausibly often for their age",
            before - articles.len()
        );
    }
    articles.truncate(limit);
    Ok(Fetched {
        papers: articles,
        complete,
    })
}

/// The wait a `Retry-After` header asks for, in seconds.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
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
       "primary_location": {"source": {"display_name": "arXiv (Cornell University)"}},
       "open_access": {"is_oa": true, "oa_url": "https://arxiv.org/pdf/1706.03762"},
       "locations": [{"is_oa": true, "landing_page_url": "https://arxiv.org/abs/1706.03762"}]},
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
    fn papers_keep_where_they_can_be_read_free() {
        let work = |json: &str| -> Work { serde_json::from_str(json).unwrap() };
        // The publisher's own free PDF comes first.
        let gold = work(
            r#"{"id": "W1", "doi": "https://doi.org/10.1/a", "display_name": "A",
                "best_oa_location": {"is_oa": true, "version": "publishedVersion",
                  "pdf_url": "https://journal.example/a.pdf",
                  "landing_page_url": "https://journal.example/a"},
                "locations": [{"is_oa": true, "landing_page_url": "http://arxiv.org/abs/2101.00001v2"}]}"#,
        );
        assert_eq!(
            gold.to_article().unwrap().website.as_deref(),
            Some("https://journal.example/a.pdf")
        );
        // Then arXiv, over an accepted version elsewhere.
        let preprint = work(
            r#"{"id": "W2", "doi": "https://doi.org/10.1/b", "display_name": "B",
                "open_access": {"is_oa": true, "oa_url": "https://repo.example.edu/b"},
                "best_oa_location": {"is_oa": true, "version": "acceptedVersion",
                  "landing_page_url": "https://repo.example.edu/b"},
                "locations": [{"is_oa": false, "landing_page_url": "https://doi.org/10.1/b"},
                  {"is_oa": true, "landing_page_url": "http://arxiv.org/abs/hep-th/9711200v3",
                   "pdf_url": "http://arxiv.org/pdf/hep-th/9711200v3"}]}"#,
        );
        assert_eq!(
            free_copy(&preprint).as_deref(),
            Some("https://arxiv.org/abs/hep-th/9711200")
        );
        // Then the best copy Unpaywall's data knows of, a PDF first.
        let repository = work(
            r#"{"id": "W3", "doi": "https://doi.org/10.1/c", "display_name": "C",
                "open_access": {"is_oa": true, "oa_url": "https://repo.example.edu/c"},
                "best_oa_location": {"is_oa": true, "version": "acceptedVersion",
                  "pdf_url": "https://repo.example.edu/c.pdf",
                  "landing_page_url": "https://repo.example.edu/c"}}"#,
        );
        assert_eq!(
            free_copy(&repository).as_deref(),
            Some("https://repo.example.edu/c.pdf")
        );
        let bare = work(
            r#"{"id": "W4", "display_name": "D",
                "open_access": {"is_oa": true, "oa_url": "https://europepmc.org/articles/pmc1"}}"#,
        );
        assert_eq!(
            free_copy(&bare).as_deref(),
            Some("https://europepmc.org/articles/pmc1")
        );
        // Closed papers, and ones on arXiv by their DOI, keep none.
        let closed = work(
            r#"{"id": "W5", "display_name": "E", "open_access": {"is_oa": false, "oa_url": null}}"#,
        );
        assert_eq!(free_copy(&closed), None);
        let page: WorksPage = serde_json::from_str(PAGE).unwrap();
        let attention = page.results[0].to_article().unwrap();
        assert_eq!(attention.website, None);
        // A free copy survives the articles file.
        let mut line = Vec::new();
        let paper = preprint.to_article().unwrap();
        plumb_core::article::write_article(&mut line, &paper).unwrap();
        let read = plumb_core::article::read_articles(&line[..], 10).unwrap();
        assert_eq!(read, vec![paper]);
    }

    #[test]
    fn papers_cited_more_than_any_real_paper_are_left_out() {
        let paper = |description: &str, views: u64| Article {
            title: "A paper".to_string(),
            description: Some(description.to_string()),
            views,
            ..Article::default()
        };
        let camera = paper(
            "Paper by M. Shoji et al., 2020, Plasma and Fusion Research",
            801_217,
        );
        assert_eq!(year_of(&camera), Some(2020));
        assert!(!plausible(&camera, 2026));
        let attention = paper("Paper by Ashish Vaswani et al., 2017, arXiv", 180_000);
        assert!(plausible(&attention, 2026));
        let lowry = paper(
            "Paper by Oliver H. Lowry et al., 1951, J. Biol. Chem.",
            318_762,
        );
        assert!(plausible(&lowry, 2026));
        // Without a year nothing can be said.
        assert!(plausible(&paper("Paper", 5_000_000), 2026));
        assert!(this_year() >= 2026);
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
        // The state is saved by renaming, which leaves nothing beside it.
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["papers-so-far.json", "papers-so-far.tsv"]);
        // Another filter starts over.
        assert_eq!(
            progress.resume("cited_by_count:>9").unwrap(),
            (vec![], Some("*".into()))
        );
    }
}
