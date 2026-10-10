//! Opt-in, bounded OpenAlex publication-date ingestion. This is not a
//! provider change feed: publication dates can be backdated by publishers.
//! No paid created/updated filters, per-query fetches or live-set writes.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use plumb_core::article::{read_articles, write_article, Article, ARTICLES_HEADER};
use plumb_core::papers::valid_date;
use serde::{Deserialize, Serialize};

use crate::openalex::{Work, PER_PAGE};

pub const DEFAULT_WINDOW_DAYS: usize = 90;
pub const DEFAULT_RECORD_BUDGET: usize = 50_000;
const FETCH_VERSION: u32 = 1;
const MAX_CACHE_BYTES: u64 = 1024 * 1024 * 1024;
const SELECT: &str = "id,doi,display_name,publication_year,publication_date,cited_by_count,authorships,primary_location,open_access,best_oa_location,locations,primary_topic";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentOptions {
    /// Inclusive ISO dates; not inferred from a file modification time.
    pub from_date: String,
    pub to_date: String,
    pub record_budget: usize,
    /// Per invocation, including refused requests; resuming cannot crawl
    /// unboundedly because the saved record quotas still apply.
    pub request_budget: usize,
    /// OpenAlex's four primary-topic domains (1..=4). Each date/domain
    /// partition receives its own reserved quota, with no spillover.
    pub domains: Vec<u8>,
}

impl RecentOptions {
    pub fn ending(to_date: &str, days: usize) -> Result<Self> {
        if !valid_date(to_date) || to_date < "1001-01-01" || !(1..=366).contains(&days) {
            bail!("recent-paper window must be 1..366 days with a valid end date");
        }
        let mut from_date = to_date.to_string();
        for _ in 1..days {
            from_date = previous_day(&from_date);
        }
        Ok(Self {
            from_date,
            to_date: to_date.into(),
            record_budget: DEFAULT_RECORD_BUDGET,
            request_budget: 1000,
            domains: vec![1, 2, 3, 4],
        })
    }

    fn partitions(&self) -> Result<Vec<Partition>> {
        if !valid_date(&self.from_date)
            || !valid_date(&self.to_date)
            || self.from_date.as_str() < "1000-01-01"
            || self.from_date > self.to_date
            || self.record_budget == 0
            || self.record_budget > DEFAULT_RECORD_BUDGET
            || self.request_budget > 1000
            || self.request_budget == 0
            || self.domains.is_empty()
            || self.domains.iter().any(|id| !(1..=4).contains(id))
            || self.domains.iter().collect::<HashSet<_>>().len() != self.domains.len()
        {
            bail!("invalid recent-paper date, domain or bounded budget");
        }
        let mut periods = Vec::new();
        let mut end = self.to_date.clone();
        let mut days = 0;
        loop {
            let mut start = end.clone();
            for _ in 0..29 {
                if start <= self.from_date {
                    break;
                }
                start = previous_day(&start);
                days += 1;
            }
            days += 1;
            if days > 366 {
                bail!("recent-paper window exceeds 366 days");
            }
            periods.push((start.clone(), end.clone()));
            if start == self.from_date {
                break;
            }
            end = previous_day(&start);
        }
        let count = periods.len() * self.domains.len();
        if self.record_budget < count {
            bail!("record budget must reserve at least one record per date/domain partition");
        }
        let mut partitions = Vec::new();
        for (start, end) in periods {
            for &domain in &self.domains {
                let index = partitions.len();
                partitions.push(Partition {
                    filter: format!("from_publication_date:{start},to_publication_date:{end},primary_topic.domain.id:{domain},is_paratext:false"),
                    from_date: start.clone(), to_date: end.clone(), domain,
                    quota: self.record_budget / count + usize::from(index < self.record_budget % count),
                    accepted: 0, rejected: 0, cursor: Some("*".into()),
                });
            }
        }
        Ok(partitions)
    }
}

fn previous_day(date: &str) -> String {
    let mut year: u32 = date[..4].parse().expect("validated year");
    let mut month: u32 = date[5..7].parse().expect("validated month");
    let mut day: u32 = date[8..].parse().expect("validated day");
    if day > 1 {
        day -= 1;
    } else {
        if month > 1 {
            month -= 1;
        } else {
            year -= 1;
            month = 12;
        }
        day = 31;
        while !valid_date(&format!("{year:04}-{month:02}-{day:02}")) {
            day -= 1;
        }
    }
    format!("{year:04}-{month:02}-{day:02}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Partition {
    pub filter: String,
    pub from_date: String,
    pub to_date: String,
    pub domain: u8,
    pub quota: usize,
    pub accepted: usize,
    pub rejected: usize,
    pub cursor: Option<String>,
}

impl Partition {
    fn finished(&self) -> bool {
        self.accepted >= self.quota || self.cursor.is_none()
    }

    fn accepts(&self, work: &Work) -> bool {
        work.publication_date.as_deref().is_some_and(|date| {
            valid_date(date) && date >= self.from_date.as_str() && date <= self.to_date.as_str()
        }) && work
            .primary_topic
            .as_ref()
            .and_then(|t| t.domain.as_ref())
            .is_some_and(|d| {
                d.id == format!("https://openalex.org/domains/{}", self.domain)
                    || d.id == self.domain.to_string()
            })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct State {
    version: u32,
    /// Contains filters, window, quotas, sort, select, and supported page
    /// size. A completed citation cache can never satisfy this key.
    key: String,
    partitions: Vec<Partition>,
    records: usize,
    papers_bytes: u64,
    requests: usize,
    next_partition: usize,
}

struct Progress {
    papers: PathBuf,
    state: PathBuf,
}

impl Progress {
    fn new(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            papers: dir.join("recent-papers.tsv"),
            state: dir.join("recent-state.json"),
        })
    }

    fn resume(&self, key: &str, partitions: Vec<Partition>) -> Result<(Vec<Article>, State)> {
        if let Ok(bytes) = std::fs::read(&self.state) {
            if let Ok(state) = serde_json::from_slice::<State>(&bytes) {
                if state.version == FETCH_VERSION
                    && state.key == key
                    && state.records <= DEFAULT_RECORD_BUDGET
                    && state.papers_bytes <= MAX_CACHE_BYTES
                    && state.partitions.len() == partitions.len()
                    && state
                        .partitions
                        .iter()
                        .zip(&partitions)
                        .all(|(saved, wanted)| {
                            saved.filter == wanted.filter
                                && saved.quota == wanted.quota
                                && saved.accepted <= saved.quota
                        })
                {
                    if let Ok(file) = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&self.papers)
                    {
                        if file.metadata()?.len() >= state.papers_bytes {
                            // Discard an uncommitted append after a crash. The
                            // cursor is committed only after the appended rows.
                            file.set_len(state.papers_bytes)?;
                            let papers =
                                read_articles(std::io::BufReader::new(file), state.records + 1)?;
                            if papers.len() == state.records
                                && state.records
                                    == state.partitions.iter().map(|p| p.accepted).sum::<usize>()
                            {
                                return Ok((papers, state));
                            }
                        }
                    }
                }
            }
        }
        std::fs::write(&self.papers, ARTICLES_HEADER)?;
        let state = State {
            version: FETCH_VERSION,
            key: key.into(),
            partitions,
            records: 0,
            papers_bytes: ARTICLES_HEADER.len() as u64,
            requests: 0,
            next_partition: 0,
        };
        self.save(&state)?;
        Ok((vec![], state))
    }

    fn append(&self, papers: &[Article]) -> Result<u64> {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.papers)?;
        for paper in papers {
            write_article(&mut file, paper)?;
        }
        file.flush()?;
        file.sync_data()?;
        let bytes = file.metadata()?.len();
        if bytes > MAX_CACHE_BYTES {
            bail!("recent-paper cache exceeded 1 GiB; retain the previous generation");
        }
        Ok(bytes)
    }

    fn save(&self, state: &State) -> Result<()> {
        let part = self.state.with_extension("json.part");
        std::fs::write(&part, serde_json::to_vec(state)?)?;
        std::fs::rename(&part, &self.state)?;
        Ok(())
    }
}

pub struct RecentFetched {
    pub papers: Vec<Article>,
    /// All reserved quotas reached or provider cursors exhausted. This
    /// describes the bounded lane, not every recent scholarly work.
    pub complete: bool,
    pub requests_this_run: usize,
    pub partitions: Vec<Partition>,
    /// Refusal is resumable and must not be silently published as complete.
    pub stopped_status: Option<u16>,
}

#[derive(Deserialize)]
struct Page {
    meta: PageMeta,
    results: Vec<Work>,
}
#[derive(Deserialize)]
struct PageMeta {
    next_cursor: Option<String>,
}

pub async fn fetch_recent_papers(
    client: &reqwest::Client,
    options: &RecentOptions,
    api_key: Option<&str>,
    progress: &Path,
) -> Result<RecentFetched> {
    fetch_from(
        client,
        options,
        api_key,
        progress,
        "https://api.openalex.org/works",
    )
    .await
}

async fn fetch_from(
    client: &reqwest::Client,
    options: &RecentOptions,
    api_key: Option<&str>,
    progress: &Path,
    endpoint: &str,
) -> Result<RecentFetched> {
    let partitions = options.partitions()?;
    let key = serde_json::to_string(&(
        FETCH_VERSION,
        &partitions,
        "publication_date:desc",
        SELECT,
        PER_PAGE,
        endpoint,
    ))?;
    let progress = Progress::new(progress)?;
    let (mut papers, mut state) = progress.resume(&key, partitions)?;
    let mut known: HashSet<String> = papers.iter().flat_map(identity_keys).collect();
    let mut requests = 0;
    let mut stopped_status = None;
    while requests < options.request_budget {
        let count = state.partitions.len();
        let Some(index) = (0..count)
            .map(|offset| (state.next_partition + offset) % count)
            .find(|&i| !state.partitions[i].finished())
        else {
            break;
        };
        let partition = &state.partitions[index];
        let cursor = partition
            .cursor
            .as_ref()
            .expect("unfinished cursor")
            .clone();
        let per_page = PER_PAGE.min(partition.quota - partition.accepted);
        let mut params = vec![
            ("filter", partition.filter.clone()),
            ("sort", "publication_date:desc".into()),
            ("per_page", per_page.to_string()),
            ("cursor", cursor.clone()),
            ("select", SELECT.into()),
        ];
        if let Some(key) = api_key {
            params.push(("api_key", key.into()));
        }
        let url = reqwest::Url::parse_with_params(endpoint, &params)?;
        requests += 1;
        state.requests += 1;
        // Never include the credential-bearing URL or response body in errors.
        let response = client
            .get(url)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .context("asking OpenAlex for recent publications")?;
        let status = response.status();
        if status.as_u16() == 429 || status.is_server_error() {
            stopped_status = Some(status.as_u16());
            progress.save(&state)?;
            break;
        }
        if !status.is_success() {
            bail!("recent-paper provider answered {status}");
        }
        if response
            .content_length()
            .is_some_and(|bytes| bytes > 4 * 1024 * 1024)
        {
            bail!("recent-paper provider exceeded the response size budget");
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(reqwest::Error::without_url)?;
            if bytes.len() + chunk.len() > 4 * 1024 * 1024 {
                bail!("recent-paper provider exceeded the response size budget");
            }
            bytes.extend_from_slice(&chunk);
        }
        let page: Page =
            serde_json::from_slice(&bytes).context("reading recent-paper provider metadata")?;
        if page.results.len() > per_page {
            bail!("recent-paper provider exceeded the page contract");
        }
        let partition = &mut state.partitions[index];
        let mut new = Vec::new();
        for work in &page.results {
            if !partition.accepts(work) {
                partition.rejected += 1;
                continue;
            }
            let Some(article) = work.to_article() else {
                partition.rejected += 1;
                continue;
            };
            let identities = identity_keys(&article);
            if identities.is_empty() || identities.iter().any(|key| known.contains(key)) {
                partition.rejected += 1;
                continue;
            }
            if partition.accepted + new.len() == partition.quota {
                break;
            }
            known.extend(identities);
            new.push(article);
        }
        if page.meta.next_cursor.as_deref() == Some(cursor.as_str()) {
            bail!("recent-paper provider repeated its cursor");
        }
        partition.cursor = page.meta.next_cursor.filter(|_| !page.results.is_empty());
        partition.accepted += new.len();
        state.records += new.len();
        state.next_partition = (index + 1) % count;
        state.papers_bytes = progress.append(&new)?;
        progress.save(&state)?;
        papers.extend(new);
        if requests < options.request_budget && !state.partitions.iter().all(Partition::finished) {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    Ok(RecentFetched {
        papers,
        complete: state.partitions.iter().all(Partition::finished),
        requests_this_run: requests,
        partitions: state.partitions,
        stopped_status,
    })
}

fn identity_keys(paper: &Article) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(item) = &paper.item {
        keys.push(item.to_ascii_lowercase());
    }
    if let Some(metadata) = &paper.paper {
        if let Some(doi) = &metadata.doi {
            keys.push(doi.to_ascii_lowercase());
        }
        if let Some(openalex) = &metadata.openalex_id {
            keys.push(openalex.to_ascii_lowercase());
        }
    }
    keys.sort();
    keys.dedup();
    keys
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Merged {
    pub added: usize,
    pub matched: usize,
    pub conflicting_ids: Vec<String>,
}

/// Merge by DOI/OpenAlex identity, never by title. Verified canonical
/// records keep their dates and provenance. Journal and preprint records
/// with separate primary IDs remain independently dated versions.
pub fn merge_recent(established: &mut Vec<Article>, recent: Vec<Article>) -> Merged {
    let mut by_id: HashMap<String, usize> = established
        .iter()
        .enumerate()
        .flat_map(|(i, paper)| identity_keys(paper).into_iter().map(move |key| (key, i)))
        .collect();
    let mut done = Merged::default();
    for paper in recent {
        let keys = identity_keys(&paper);
        if let Some(index) = keys.iter().find_map(|key| by_id.get(key).copied()) {
            if plumb_core::normalize_text(&established[index].title)
                != plumb_core::normalize_text(&paper.title)
            {
                done.conflicting_ids.push(paper.item.unwrap_or_default());
                continue;
            }
            if established[index].paper.is_none() {
                established[index].paper = paper.paper;
            }
            if established[index].website.is_none() {
                established[index].website = paper.website;
            }
            for key in keys {
                by_id.insert(key, index);
            }
            done.matched += 1;
        } else if !keys.is_empty() {
            for key in keys {
                by_id.insert(key, established.len());
            }
            established.push(paper);
            done.added += 1;
        }
    }
    established.sort_by_key(|paper| std::cmp::Reverse(paper.views));
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn work(id: &str, date: &str, domain: u8) -> serde_json::Value {
        serde_json::json!({"id": format!("https://openalex.org/{id}"), "display_name": format!("Research {id}"),
            "publication_year": 2026, "publication_date": date, "cited_by_count": 0,
            "authorships": [{"author": {"display_name": "Research Author"}}],
            "primary_topic": {"domain": {"id": format!("https://openalex.org/domains/{domain}")}}})
    }

    async fn server(
        responses: Vec<(u16, serde_json::Value)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/works", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, json) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 8192];
                let n = socket.read(&mut bytes).await.unwrap();
                requests.push(String::from_utf8(bytes[..n].to_vec()).unwrap());
                let body = serde_json::to_string(&json).unwrap();
                let response = format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (url, task)
    }

    fn options() -> RecentOptions {
        let mut options = RecentOptions::ending("2026-10-09", 9).unwrap();
        options.record_budget = 5;
        options.request_budget = 1;
        options.domains = vec![1];
        options
    }

    #[test]
    fn quotas_reserve_dates_and_domains_without_a_citation_floor() {
        let options = RecentOptions::ending("2026-10-09", DEFAULT_WINDOW_DAYS).unwrap();
        let partitions = options.partitions().unwrap();
        assert_eq!(options.from_date, "2026-07-12");
        assert_eq!(partitions.len(), 12);
        assert_eq!(
            partitions.iter().map(|p| p.quota).sum::<usize>(),
            DEFAULT_RECORD_BUDGET
        );
        assert!(partitions
            .iter()
            .all(|p| !p.filter.contains("cited_by_count")));
        assert_eq!(
            RecentOptions::ending("2024-03-01", 3).unwrap().from_date,
            "2024-02-28"
        );
        assert!(RecentOptions::ending("2025-02-29", 90).is_err());
        assert!(RecentOptions::ending("2026-10-09", 1000).is_err());
        let mut options = options;
        options.from_date = "2020-01-01".into();
        assert!(options.partitions().is_err());
    }

    #[tokio::test]
    async fn cursor_resume_keeps_low_citation_rows_and_rejects_wrong_dates() {
        let (url, task) = server(vec![
            (200, serde_json::json!({"meta":{"next_cursor":"next"},"results":[work("W1", "2026-10-09", 1)]})),
            (200, serde_json::json!({"meta":{"next_cursor":null},"results":[
                work("W2", "2026-10-05", 1), work("W3", "2016-10-05", 1), work("W4", "2026-10-05", 2),
                work("W5", "", 1)]})),
        ]).await;
        let dir = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();
        let first = fetch_from(&client, &options(), None, dir.path(), &url)
            .await
            .unwrap();
        assert!(!first.complete);
        assert_eq!(first.papers.len(), 1);
        assert_eq!(first.papers[0].views, 0);
        assert_eq!(
            first.papers[0]
                .paper
                .as_ref()
                .unwrap()
                .publication_date
                .as_deref(),
            Some("2026-10-09")
        );
        // Simulate a crash between append and cursor commit. The orphan row
        // must disappear before the saved cursor is resumed.
        let progress = Progress::new(dir.path()).unwrap();
        progress.append(&[first.papers[0].clone()]).unwrap();
        let second = fetch_from(&client, &options(), None, dir.path(), &url)
            .await
            .unwrap();
        assert!(second.complete);
        assert_eq!(second.papers.len(), 2);
        assert_eq!(second.partitions[0].rejected, 3);
        let cached = fetch_from(&client, &options(), None, dir.path(), &url)
            .await
            .unwrap();
        assert_eq!(cached.requests_this_run, 0);
        let requests = task.await.unwrap();
        let request_url = |request: &str| {
            reqwest::Url::parse(&format!(
                "http://localhost{}",
                request.split_whitespace().nth(1).unwrap()
            ))
            .unwrap()
        };
        let first_params: HashMap<String, String> = request_url(&requests[0])
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(first_params["cursor"], "*");
        assert_eq!(first_params["sort"], "publication_date:desc");
        assert!(first_params["filter"].contains("from_publication_date:2026-10-01"));
        let next_params: HashMap<String, String> = request_url(&requests[1])
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(next_params["cursor"], "next");
    }

    #[tokio::test]
    async fn completed_windows_cannot_be_reused_for_another_window() {
        let (url, task) = server(vec![
            (200, serde_json::json!({"meta":{"next_cursor":null},"results":[work("W1", "2026-10-09", 1)]})),
            (200, serde_json::json!({"meta":{"next_cursor":null},"results":[work("W2", "2026-10-10", 1)]})),
        ]).await;
        let dir = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();
        fetch_from(&client, &options(), None, dir.path(), &url)
            .await
            .unwrap();
        let mut next = options();
        next.to_date = "2026-10-10".into();
        let fetched = fetch_from(&client, &next, None, dir.path(), &url)
            .await
            .unwrap();
        assert_eq!(fetched.papers.len(), 1);
        assert_eq!(fetched.papers[0].item.as_deref(), Some("W2"));
        assert_eq!(fetched.requests_this_run, 1);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn refused_provider_does_not_mark_the_lane_complete_or_leak_credentials() {
        let (url, task) = server(vec![(
            429,
            serde_json::json!({"message":"secret-provider-body"}),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let fetched = fetch_from(
            &reqwest::Client::new(),
            &options(),
            Some("secret-key"),
            dir.path(),
            &url,
        )
        .await
        .unwrap();
        assert!(!fetched.complete);
        assert_eq!(fetched.stopped_status, Some(429));
        let state = std::fs::read_to_string(dir.path().join("recent-state.json")).unwrap();
        assert!(!state.contains("secret"));
        task.await.unwrap();
    }

    #[test]
    fn merge_uses_confirmed_identity_and_preserves_canonical_records() {
        let article = |id: &str, title: &str| Article {
            item: Some(id.into()),
            title: title.into(),
            ..Article::default()
        };
        let original = article("10.1/original", "A research title");
        let unrelated = article("10.1/unrelated", "A research title");
        let mut papers = vec![original.clone()];
        let done = merge_recent(
            &mut papers,
            vec![
                original.clone(),
                unrelated.clone(),
                article("10.1/original", "Wrong title"),
            ],
        );
        assert_eq!((done.added, done.matched), (1, 1));
        assert_eq!(done.conflicting_ids, ["10.1/original"]);
        assert_eq!(papers, [original, unrelated]);
    }
}
