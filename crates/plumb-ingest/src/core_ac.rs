//! Free copies of papers from CORE (core.ac.uk), which gathers the papers
//! of thousands of university and subject repositories: a paper that
//! OpenAlex knows no free copy of often has its authors' own version in
//! one.
//!
//! CORE's API needs a key (free for personal and research use), read from
//! `CORE_API_KEY`, and lets a key ask about a thousand times a day, so its
//! papers are asked for fifty DOIs at a time, the most cited first, at
//! most `max_requests` times a run. What it answered is kept in a cache
//! file, a DOI and its free copy (or nothing) a line, so later runs ask
//! only for papers not asked for before and every run gets the copies
//! found before. Only the copy's address is kept.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use plumb_core::article::Article;
use serde::Deserialize;
use tracing::{info, warn};

const SEARCH_URL: &str = "https://api.core.ac.uk/v3/search/works";
/// DOIs asked for in one request.
pub const DOIS_A_REQUEST: usize = 50;
/// Requests a run makes unless asked otherwise: under the thousand a day
/// CORE allows a free key.
pub const DEFAULT_MAX_REQUESTS: usize = 900;
/// Pause between requests, under CORE's ten a minute.
const PAUSE: Duration = Duration::from_secs(7);
/// Longest wait between refused requests.
const MAX_WAIT: Duration = Duration::from_secs(10 * 60);
/// How long CORE may keep refusing before the copies so far are kept.
const GIVE_UP_AFTER: Duration = Duration::from_secs(60 * 60);
/// The cache file in the folder given.
const CACHE_FILE: &str = "free-copies.tsv";

#[derive(Debug, Default, Deserialize)]
struct SearchPage {
    #[serde(default)]
    results: Vec<CoreWork>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CoreWork {
    #[serde(default)]
    doi: Option<String>,
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default)]
    links: Vec<Link>,
}

#[derive(Debug, Default, Deserialize)]
struct Link {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

impl CoreWork {
    /// Where the work's full text can be downloaded, as an `https://` or
    /// `http://` address that fits an articles file.
    fn download(&self) -> Option<String> {
        self.download_url
            .iter()
            .chain(
                self.links
                    .iter()
                    .filter(|l| l.kind.as_deref() == Some("download"))
                    .filter_map(|l| l.url.as_ref()),
            )
            .map(|url| url.trim())
            .find(|url| {
                (url.starts_with("https://") || url.starts_with("http://"))
                    && url.len() > "https://".len()
                    && !url.contains(['\t', '\n', '\r', '|', ' '])
            })
            .map(str::to_string)
    }
}

/// A paper's DOI as CORE is asked for it, lowercased: `None` for a paper
/// with only an OpenAlex id, one whose DOI won't quote, or one on arXiv,
/// whose address is already free.
fn doi_of(paper: &Article) -> Option<String> {
    let doi = paper.item.as_deref()?.trim().to_ascii_lowercase();
    (doi.starts_with("10.")
        && !doi.starts_with(crate::openalex::ARXIV_DOI)
        && !doi.contains(['"', '\\', ' ', '\t', '\n']))
    .then_some(doi)
}

/// CORE's query for the works of `dois`.
fn query(dois: &[String]) -> String {
    dois.iter()
        .map(|doi| format!("doi:\"{doi}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// The free copies CORE answered before: a DOI and its copy, or `None`
/// when it had none.
struct Cache {
    path: Option<PathBuf>,
    copies: HashMap<String, Option<String>>,
}

impl Cache {
    fn open(dir: Option<&Path>) -> Result<Self> {
        let mut copies = HashMap::new();
        let Some(dir) = dir else {
            return Ok(Cache { path: None, copies });
        };
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(CACHE_FILE);
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let (doi, url) = line.split_once('\t').unwrap_or((line, ""));
                if doi.is_empty() {
                    continue;
                }
                let url = url.trim();
                copies.insert(doi.to_string(), (!url.is_empty()).then(|| url.to_string()));
            }
        }
        Ok(Cache {
            path: Some(path),
            copies,
        })
    }

    /// Keeps what CORE answered for `dois`.
    fn add(&mut self, dois: &[String], found: &HashMap<String, String>) -> Result<()> {
        let mut text = String::new();
        for doi in dois {
            let url = found.get(doi).cloned();
            text.push_str(doi);
            text.push('\t');
            text.push_str(url.as_deref().unwrap_or(""));
            text.push('\n');
            self.copies.insert(doi.clone(), url);
        }
        if let Some(path) = &self.path {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening {}", path.display()))?;
            file.write_all(text.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(())
    }
}

/// What [`fill_free_copies`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Filled {
    /// Papers given a free copy, from this run's answers or earlier ones.
    pub found: usize,
    /// Requests made.
    pub requests: usize,
    /// Papers without a free copy not yet asked about, for a later run.
    pub left: usize,
}

/// Gives the papers of `papers` that have no free copy (their `website`)
/// the one CORE knows of, if any. With no `key`, only the copies found by
/// earlier runs (in the cache in `cache_dir`) are given; with one, CORE is
/// asked about at most `max_requests` times for the rest, the most cited
/// first.
pub async fn fill_free_copies(
    client: &reqwest::Client,
    key: Option<&str>,
    papers: &mut [Article],
    max_requests: usize,
    cache_dir: Option<&Path>,
) -> Result<Filled> {
    let mut cache = Cache::open(cache_dir)?;
    let mut filled = Filled::default();
    let wanted: Vec<String> = papers
        .iter()
        .filter(|p| p.website.is_none())
        .filter_map(doi_of)
        .filter(|doi| !cache.copies.contains_key(doi))
        .collect();
    if let Some(key) = key {
        let mut refused_since: Option<std::time::Instant> = None;
        let mut wait = Duration::from_secs(60);
        let mut chunks = wanted.chunks(DOIS_A_REQUEST).peekable();
        while let Some(&dois) = chunks.peek() {
            if filled.requests >= max_requests {
                break;
            }
            let body = serde_json::json!({
                "q": query(dois),
                // A DOI may match more than one of CORE's records.
                "limit": dois.len() * 2,
                "exclude": ["fullText"],
            });
            filled.requests += 1;
            let response = match client
                .post(SEARCH_URL)
                .bearer_auth(key)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_string())
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    warn!("asking CORE: {err}; trying again");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
            };
            let status = response.status();
            if status.as_u16() == 401 || status.as_u16() == 403 {
                bail!("CORE refused the key in CORE_API_KEY ({status})");
            }
            if status.as_u16() == 429 || status.is_server_error() {
                let since = *refused_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() > GIVE_UP_AFTER {
                    warn!("CORE has refused for an hour; keeping the free copies so far");
                    break;
                }
                let asked = crate::openalex::retry_after(response.headers());
                let pause = asked.unwrap_or(wait).min(MAX_WAIT);
                warn!(
                    "CORE answered {status}; waiting {} seconds",
                    pause.as_secs()
                );
                tokio::time::sleep(pause).await;
                wait = (wait * 2).min(MAX_WAIT);
                continue;
            }
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                bail!(
                    "CORE answered {status}: {}",
                    plumb_core::truncate_chars(&text, 300)
                );
            }
            refused_since = None;
            wait = Duration::from_secs(60);
            let bytes = response.bytes().await.context("reading CORE's answer")?;
            let page: SearchPage =
                serde_json::from_slice(&bytes).context("reading CORE's answer")?;
            cache.add(dois, &found_in(&page, dois))?;
            chunks.next();
            if filled.requests % 100 == 0 {
                info!(
                    "asked CORE about {} papers",
                    filled.requests * DOIS_A_REQUEST
                );
            }
            tokio::time::sleep(PAUSE).await;
        }
    }
    for paper in papers.iter_mut().filter(|p| p.website.is_none()) {
        let Some(doi) = doi_of(paper) else { continue };
        match cache.copies.get(&doi) {
            Some(Some(url)) => {
                paper.website = Some(url.clone());
                filled.found += 1;
            }
            Some(None) => {}
            None => filled.left += 1,
        }
    }
    Ok(filled)
}

/// The free copies `page` gives of the papers `dois` asked about.
fn found_in(page: &SearchPage, dois: &[String]) -> HashMap<String, String> {
    let mut found = HashMap::new();
    for work in &page.results {
        let (Some(doi), Some(url)) = (work.doi.as_deref(), work.download()) else {
            continue;
        };
        let doi = doi
            .trim()
            .trim_start_matches("https://doi.org/")
            .to_ascii_lowercase();
        if dois.contains(&doi) {
            found.entry(doi).or_insert(url);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paper(doi: &str, website: Option<&str>) -> Article {
        Article {
            title: "A paper".into(),
            item: Some(doi.into()),
            website: website.map(str::to_string),
            ..Article::default()
        }
    }

    #[test]
    fn answers_give_downloads_of_the_dois_asked() {
        let page: SearchPage = serde_json::from_str(
            r#"{"totalHits": 3, "results": [
              {"doi": "10.1000/ABC", "downloadUrl": "https://core.ac.uk/download/1.pdf"},
              {"doi": "10.1000/abc", "downloadUrl": "https://core.ac.uk/download/2.pdf"},
              {"doi": "10.1000/def", "downloadUrl": "",
               "links": [{"type": "display", "url": "https://core.ac.uk/works/3"},
                         {"type": "download", "url": "https://repo.example.edu/3.pdf"}]},
              {"doi": "10.1000/ghi", "downloadUrl": null, "links": []},
              {"doi": "10.1000/other", "downloadUrl": "https://core.ac.uk/download/4.pdf"}
            ]}"#,
        )
        .unwrap();
        let dois: Vec<String> = ["10.1000/abc", "10.1000/def", "10.1000/ghi"]
            .map(String::from)
            .to_vec();
        let found = found_in(&page, &dois);
        assert_eq!(found.len(), 2);
        assert_eq!(found["10.1000/abc"], "https://core.ac.uk/download/1.pdf");
        assert_eq!(found["10.1000/def"], "https://repo.example.edu/3.pdf");
        assert_eq!(
            query(&dois[..2]),
            r#"doi:"10.1000/abc" OR doi:"10.1000/def""#
        );
    }

    #[test]
    fn only_papers_with_a_doi_and_no_free_copy_are_asked_about() {
        assert_eq!(
            doi_of(&paper("10.1000/ABC", None)),
            Some("10.1000/abc".into())
        );
        assert_eq!(doi_of(&paper("W123", None)), None);
        assert_eq!(doi_of(&paper("10.48550/arXiv.1706.03762", None)), None);
        assert_eq!(doi_of(&paper("10.1000/a\"b", None)), None);
    }

    #[tokio::test]
    async fn the_cache_gives_copies_found_before_without_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(Some(dir.path())).unwrap();
        let found = HashMap::from([(
            "10.1000/a".to_string(),
            "https://x.example/a.pdf".to_string(),
        )]);
        cache
            .add(&["10.1000/a".into(), "10.1000/b".into()], &found)
            .unwrap();
        let mut papers = vec![
            paper("10.1000/A", None),
            paper("10.1000/b", None),
            paper("10.1000/c", None),
            paper("10.1000/d", Some("https://arxiv.org/abs/1")),
        ];
        let client = reqwest::Client::new();
        let filled = fill_free_copies(&client, None, &mut papers, 10, Some(dir.path()))
            .await
            .unwrap();
        assert_eq!(
            filled,
            Filled {
                found: 1,
                requests: 0,
                left: 1
            }
        );
        assert_eq!(
            papers[0].website.as_deref(),
            Some("https://x.example/a.pdf")
        );
        assert_eq!(papers[1].website, None);
        assert_eq!(
            papers[3].website.as_deref(),
            Some("https://arxiv.org/abs/1")
        );
    }
}
