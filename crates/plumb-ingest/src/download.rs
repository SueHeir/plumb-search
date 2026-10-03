//! Downloads the seed datasets. These hosts must be reachable from the
//! machine running `plumb fetch-data`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tracing::info;

use crate::wikidata::bare_item_id;

/// Sent with every request, so site owners and dataset hosts can see who is fetching.
pub const USER_AGENT: &str = concat!(
    "PlumbSearch/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/SueHeir/plumb-search)"
);

/// The latest Tranco list, as a zip holding `top-1m.csv`.
pub const TRANCO_LATEST_URL: &str = "https://tranco-list.eu/top-1m.csv.zip";

/// Wikidata's public SPARQL endpoint.
pub const WIKIDATA_SPARQL_URL: &str = "https://query.wikidata.org/sparql";

/// File name [`download_tranco`] saves to.
pub const TRANCO_FILE_NAME: &str = "tranco-top-1m.csv.zip";

/// File name [`download_wikidata_official_sites`] saves to.
pub const WIKIDATA_FILE_NAME: &str = "wikidata-official-sites.tsv";

/// Progress is logged about this often when the download size is unknown.
const PROGRESS_BYTES_UNKNOWN_LENGTH: u64 = 64 << 20;

/// A client with [`USER_AGENT`], gzip off (we store files as served) and generous timeouts.
///
/// Connecting may take 30 seconds and each read 120 seconds; there is no
/// overall timeout, since dataset files can be gigabytes. Every response
/// decompression is turned off, even when another crate in the build enables
/// reqwest's `gzip` feature.
pub fn http_client() -> Result<reqwest::Client> {
    client_builder().build().context("building the HTTP client")
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
}

/// Streams `url` into `dest` (via a `.part` file renamed when complete),
/// logging progress. Fails on non-2xx responses. Returns bytes written.
///
/// Creates `dest`'s parent directory, replaces an existing `dest`, and
/// removes the `.part` file again when the download fails.
pub async fn download_to_file(client: &reqwest::Client, url: &str, dest: &Path) -> Result<u64> {
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("downloading {url} failed: HTTP {status}");
    }
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let part = part_path(dest);
    let total = response.content_length();
    info!(
        "downloading {url} to {} ({})",
        dest.display(),
        total.map_or_else(|| "size unknown".to_string(), megabytes)
    );
    let written = match stream_body(&mut response, &part, total).await {
        Ok(written) => written,
        Err(err) => {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(err.context(format!("downloading {url}")));
        }
    };
    tokio::fs::rename(&part, dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!("saved {} ({})", dest.display(), megabytes(written));
    Ok(written)
}

/// Writes the response body to `part`, returning the bytes written.
async fn stream_body(
    response: &mut reqwest::Response,
    part: &Path,
    total: Option<u64>,
) -> Result<u64> {
    let mut file = tokio::fs::File::create(part)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    let mut progress = Progress::new(total);
    let mut written = 0u64;
    while let Some(chunk) = response.chunk().await.context("reading the response")? {
        file.write_all(&chunk)
            .await
            .with_context(|| format!("writing {}", part.display()))?;
        written += chunk.len() as u64;
        if progress.should_report(written) {
            match total {
                Some(total) => info!(
                    "{}: {}% ({} of {})",
                    part.display(),
                    written * 100 / total.max(1),
                    megabytes(written),
                    megabytes(total)
                ),
                None => info!("{}: {}", part.display(), megabytes(written)),
            }
        }
    }
    file.flush()
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    file.sync_all()
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    if let Some(total) = total {
        if written != total {
            bail!("expected {total} bytes but received {written}");
        }
    }
    Ok(written)
}

/// `dest` with `.part` appended to its file name.
fn part_path(dest: &Path) -> PathBuf {
    let mut part = OsString::from(dest.as_os_str());
    part.push(".part");
    PathBuf::from(part)
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// Decides when to log download progress: about every 5% of a known
/// length, or every 64 MB when the length is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Progress {
    step: u64,
    next: u64,
}

impl Progress {
    fn new(total: Option<u64>) -> Self {
        let step = match total {
            Some(total) if total > 0 => (total / 20).max(1),
            _ => PROGRESS_BYTES_UNKNOWN_LENGTH,
        };
        Progress { step, next: step }
    }

    /// True when `written` bytes reach the next reporting point.
    fn should_report(&mut self, written: u64) -> bool {
        if written < self.next {
            return false;
        }
        self.next = (written / self.step + 1) * self.step;
        true
    }
}

/// Downloads the latest Tranco list to `dir/tranco-top-1m.csv.zip` and returns that path.
pub async fn download_tranco(client: &reqwest::Client, dir: &Path) -> Result<PathBuf> {
    let dest = dir.join(TRANCO_FILE_NAME);
    download_to_file(client, TRANCO_LATEST_URL, &dest).await?;
    Ok(dest)
}

/// URL of the domain ranks file for a web graph release name such as
/// `cc-main-2025-26-nov-dec-jan` (names are listed on
/// <https://commoncrawl.org/web-graphs>):
/// `https://data.commoncrawl.org/projects/hyperlinkgraph/<release>/domain/<release>-domain-ranks.txt.gz`.
pub fn cc_domain_ranks_url(release: &str) -> String {
    format!(
        "https://data.commoncrawl.org/projects/hyperlinkgraph/{release}/domain/{release}-domain-ranks.txt.gz"
    )
}

/// Downloads `url` (normally [`cc_domain_ranks_url`]) to `dir/<file name in the URL>`.
pub async fn download_cc_domain_ranks(
    client: &reqwest::Client,
    url: &str,
    dir: &Path,
) -> Result<PathBuf> {
    let dest = dir.join(file_name_from_url(url)?);
    download_to_file(client, url, &dest).await?;
    Ok(dest)
}

/// The last path segment of `url`, e.g. `x-domain-ranks.txt.gz`; an error
/// when the URL is invalid or its path ends in `/`.
pub fn file_name_from_url(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url).with_context(|| format!("invalid URL {url:?}"))?;
    let name = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("");
    if name.is_empty() || name == "." || name == ".." || name.contains('\\') {
        bail!("cannot tell a file name from the URL {url:?}");
    }
    Ok(name.to_string())
}

/// Asks Wikidata's SPARQL endpoint for items with an official website (P856)
/// and at least `min_sitelinks` Wikipedia sitelinks (a notability filter
/// that keeps the query small enough to finish), and writes
/// `dir/wikidata-official-sites.tsv` with the header `item\tlabel\twebsite`.
///
/// The query is [`wikidata_sparql_query`], sent as a form POST; the JSON
/// results go through [`wikidata_json_to_tsv`].
pub async fn download_wikidata_official_sites(
    client: &reqwest::Client,
    dir: &Path,
    min_sitelinks: u32,
) -> Result<PathBuf> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("query", &wikidata_sparql_query(min_sitelinks))
        .finish();
    info!("asking Wikidata for official websites of items with at least {min_sitelinks} sitelinks");
    let response = client
        .post(WIKIDATA_SPARQL_URL)
        .header(reqwest::header::ACCEPT, "application/sparql-results+json")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form)
        .send()
        .await
        .with_context(|| format!("querying {WIKIDATA_SPARQL_URL}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "Wikidata query failed: HTTP {status}: {}",
            plumb_core::truncate_chars(body.trim(), 500)
        );
    }
    let json = response
        .bytes()
        .await
        .context("reading the Wikidata response")?;
    let tsv = wikidata_json_to_tsv(&json)?;

    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(WIKIDATA_FILE_NAME);
    let part = part_path(&dest);
    tokio::fs::write(&part, tsv.as_bytes())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    tokio::fs::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!(
        "wrote {} official websites to {}",
        tsv.lines().count().saturating_sub(1),
        dest.display()
    );
    Ok(dest)
}

/// The SPARQL query for items with an official website and at least
/// `min_sitelinks` sitelinks, with English (or multilingual) labels.
pub fn wikidata_sparql_query(min_sitelinks: u32) -> String {
    format!(
        "SELECT ?item ?itemLabel ?website WHERE {{ ?item wdt:P856 ?website ; wikibase:sitelinks ?sitelinks . FILTER(?sitelinks >= {min_sitelinks}) SERVICE wikibase:label {{ bd:serviceParam wikibase:language \"en,mul\". }} }}"
    )
}

/// Converts SPARQL JSON results (`results.bindings[]` with `item`,
/// `itemLabel` and `website`) into the TSV that
/// [`crate::load_wikidata_official_sites`] reads: the header
/// `item\tlabel\twebsite`, then one row per binding.
///
/// Items lose their entity URL prefix (`http://www.wikidata.org/entity/Q1`
/// becomes `Q1`), tabs and line breaks inside values become spaces, and rows
/// missing a field (or with an empty one) are skipped.
pub fn wikidata_json_to_tsv(json: &[u8]) -> Result<String> {
    let response: SparqlResponse =
        serde_json::from_slice(json).context("parsing Wikidata SPARQL results")?;
    let mut tsv = String::from("item\tlabel\twebsite\n");
    for binding in &response.results.bindings {
        let (Some(item), Some(label), Some(website)) = (
            tsv_field(&binding.item),
            tsv_field(&binding.item_label),
            tsv_field(&binding.website),
        ) else {
            continue;
        };
        let item = bare_item_id(&item);
        if item.is_empty() {
            continue;
        }
        tsv.push_str(item);
        tsv.push('\t');
        tsv.push_str(&label);
        tsv.push('\t');
        tsv.push_str(&website);
        tsv.push('\n');
    }
    Ok(tsv)
}

/// A binding value made safe for one TSV cell; `None` when missing or blank.
fn tsv_field(term: &Option<SparqlTerm>) -> Option<String> {
    let value = term.as_ref()?.value.replace(['\t', '\n', '\r'], " ");
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[derive(Debug, Deserialize)]
struct SparqlResponse {
    results: SparqlResults,
}

#[derive(Debug, Deserialize)]
struct SparqlResults {
    bindings: Vec<SparqlBinding>,
}

#[derive(Debug, Deserialize)]
struct SparqlBinding {
    item: Option<SparqlTerm>,
    #[serde(rename = "itemLabel")]
    item_label: Option<SparqlTerm>,
    website: Option<SparqlTerm>,
}

#[derive(Debug, Deserialize)]
struct SparqlTerm {
    value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparql_query_text() {
        assert_eq!(
            wikidata_sparql_query(20),
            "SELECT ?item ?itemLabel ?website WHERE { ?item wdt:P856 ?website ; wikibase:sitelinks ?sitelinks . FILTER(?sitelinks >= 20) SERVICE wikibase:label { bd:serviceParam wikibase:language \"en,mul\". } }"
        );
    }

    #[test]
    fn sparql_json_to_tsv() {
        let json = r#"{
          "head": {"vars": ["item", "itemLabel", "website"]},
          "results": {"bindings": [
            {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q739868"},
             "website": {"type": "uri", "value": "https://www.usbank.com/"},
             "itemLabel": {"xml:lang": "en", "type": "literal", "value": "U.S. Bancorp"}},
            {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q2"},
             "website": {"type": "uri", "value": "https://example.org/"},
             "itemLabel": {"type": "literal", "value": "Tabbed\tand\nsplit\r\nlabel"}},
            {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q3"},
             "itemLabel": {"type": "literal", "value": "No website"}},
            {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q4"},
             "website": {"type": "uri", "value": "https://nolabel.org/"}},
            {"website": {"type": "uri", "value": "https://noitem.org/"},
             "itemLabel": {"type": "literal", "value": "No item"}},
            {"item": {"type": "uri", "value": "http://www.wikidata.org/entity/Q6"},
             "website": {"type": "uri", "value": "https://blank.org/"},
             "itemLabel": {"type": "literal", "value": " \t "}},
            {"item": {"type": "uri", "value": "Q7"},
             "website": {"type": "uri", "value": "https://bare.org/"},
             "itemLabel": {"type": "literal", "value": "Bare id"}}
          ]}
        }"#;
        assert_eq!(
            wikidata_json_to_tsv(json.as_bytes()).unwrap(),
            "item\tlabel\twebsite\n\
             Q739868\tU.S. Bancorp\thttps://www.usbank.com/\n\
             Q2\tTabbed and split  label\thttps://example.org/\n\
             Q7\tBare id\thttps://bare.org/\n"
        );
        let empty = r#"{"head": {"vars": []}, "results": {"bindings": []}}"#;
        assert_eq!(
            wikidata_json_to_tsv(empty.as_bytes()).unwrap(),
            "item\tlabel\twebsite\n"
        );
        assert!(wikidata_json_to_tsv(b"<html>Query timeout</html>").is_err());
        assert!(wikidata_json_to_tsv(br#"{"head": {}}"#).is_err());
    }

    #[test]
    fn tsv_round_trips_through_the_loader() {
        let json = r#"{"results": {"bindings": [
            {"item": {"value": "http://www.wikidata.org/entity/Q739868"},
             "itemLabel": {"value": "U.S. Bancorp"},
             "website": {"value": "https://www.usbank.com/"}}
        ]}}"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(WIKIDATA_FILE_NAME);
        std::fs::write(&path, wikidata_json_to_tsv(json.as_bytes()).unwrap()).unwrap();
        let sites = crate::load_wikidata_official_sites(&path).unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].item, "Q739868");
        assert_eq!(sites[0].label, "U.S. Bancorp");
        assert_eq!(sites[0].domain, "usbank.com");
    }

    #[test]
    fn file_names_from_urls() {
        assert_eq!(
            file_name_from_url(&cc_domain_ranks_url("cc-main-2025-26-nov-dec-jan")).unwrap(),
            "cc-main-2025-26-nov-dec-jan-domain-ranks.txt.gz"
        );
        assert_eq!(
            file_name_from_url("https://example.org/a/b.txt?x=1#frag").unwrap(),
            "b.txt"
        );
        assert!(file_name_from_url("https://example.org/dir/").is_err());
        assert!(file_name_from_url("https://example.org").is_err());
        assert!(file_name_from_url("not a url").is_err());
    }

    #[test]
    fn part_files_sit_next_to_the_destination() {
        assert_eq!(
            part_path(Path::new("data/ranks.txt.gz")),
            PathBuf::from("data/ranks.txt.gz.part")
        );
    }

    #[test]
    fn progress_every_five_percent_or_64_mb() {
        let mut known = Progress::new(Some(1000));
        let reports: Vec<u64> = (1..=100)
            .map(|i| i * 10)
            .filter(|&w| known.should_report(w))
            .collect();
        assert_eq!(reports, (1..=20).map(|i| i * 50).collect::<Vec<u64>>());

        // A big chunk that jumps several steps logs once.
        let mut jump = Progress::new(Some(1000));
        assert!(jump.should_report(730));
        assert!(!jump.should_report(749));
        assert!(jump.should_report(750));

        let mb = 1 << 20;
        let mut unknown = Progress::new(None);
        assert!(!unknown.should_report(63 * mb));
        assert!(unknown.should_report(64 * mb));
        assert!(!unknown.should_report(100 * mb));
        assert!(unknown.should_report(128 * mb));

        let mut tiny = Progress::new(Some(3));
        assert!(tiny.should_report(1));
        assert!(tiny.should_report(3));
    }

    #[test]
    fn client_builds() {
        http_client().unwrap();
    }

    /// Serves one canned HTTP response on a loopback port; no outside network.
    async fn serve_once(response: Vec<u8>) -> String {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let _ = socket.write_all(&response).await;
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}/files/data.bin")
    }

    fn loopback_client() -> reqwest::Client {
        client_builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn downloads_via_part_file() {
        let body = vec![b'x'; 100_000];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let url = serve_once(response).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("nested/data.bin");
        let written = download_to_file(&loopback_client(), &url, &dest)
            .await
            .unwrap();
        assert_eq!(written, body.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!part_path(&dest).exists());
    }

    #[tokio::test]
    async fn downloads_without_content_length() {
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nstreamed until close".to_vec(),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("data.bin");
        let written = download_to_file(&loopback_client(), &url, &dest)
            .await
            .unwrap();
        assert_eq!(written, 20);
        assert_eq!(std::fs::read(&dest).unwrap(), b"streamed until close");
    }

    #[tokio::test]
    async fn non_success_status_is_an_error() {
        let url = serve_once(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found"
                .to_vec(),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("data.bin");
        let err = download_to_file(&loopback_client(), &url, &dest)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("404"), "{err:#}");
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
    }

    #[tokio::test]
    async fn truncated_body_is_an_error_and_leaves_no_files() {
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\nshort".to_vec(),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("data.bin");
        assert!(download_to_file(&loopback_client(), &url, &dest)
            .await
            .is_err());
        assert!(!dest.exists());
        assert!(!part_path(&dest).exists());
    }
}
