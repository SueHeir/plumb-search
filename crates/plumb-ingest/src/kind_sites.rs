//! Official websites of organizations of the kinds people look for by name
//! (banks, credit unions, airlines, universities, government agencies),
//! however few Wikipedia articles they have.
//!
//! The official websites download keeps items with a few sitelinks or more
//! (25 when only Wikidata's own endpoint answers), which leaves out many
//! credit unions, local banks and government agencies
//! (Navy Federal Credit Union, the Social Security Administration's site).
//! Asking for every item of a few kinds is small and fast instead: one
//! query per kind, found by its English label, so no item ids are written
//! down here. The rows are saved as `wikidata-kind-sites.tsv`, in the same
//! format as the official websites file, and are read the same way.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use crate::download::{part_path, wikidata_json_to_tsv, WikidataPacing};
use crate::facts::sparql_json;

/// File name of the kind sites in a seed directory.
pub const KIND_SITES_FILE_NAME: &str = "wikidata-kind-sites.tsv";

/// The kinds asked for, by their English labels in Wikidata: those whose
/// members people mostly reach by typing their name.
pub const KIND_LABELS: &[&str] = &[
    "bank",
    "credit union",
    "savings bank",
    "cooperative bank",
    "insurance company",
    "airline",
    "newspaper",
    "news website",
    "television station",
    "radio station",
    "university",
    "public university",
    "private university",
    "college",
    "community college",
    "hospital",
    "public library",
    "government agency",
    "independent agency of the United States government",
    "ministry",
    "retail chain",
    "supermarket chain",
    "restaurant chain",
    "telecommunications company",
    "internet service provider",
    "electric utility",
];

/// The SPARQL query for the items that are instances of a kind labelled
/// `label` in English, with their official websites and English labels.
pub fn kind_sites_query(label: &str) -> String {
    let label = label.replace(['\\', '"'], "");
    format!(
        "SELECT ?item ?itemLabel ?website WHERE {{ ?kind rdfs:label \"{label}\"@en . \
         ?item wdt:P31 ?kind ; wdt:P856 ?website . \
         SERVICE wikibase:label {{ bd:serviceParam wikibase:language \"en,mul\". }} }}"
    )
}

/// Asks the SPARQL `endpoint` for the official websites of every item of
/// each of [`KIND_LABELS`], one query per kind with `pacing.pause` between
/// them, and writes `dir/`[`KIND_SITES_FILE_NAME`]. A kind whose query
/// fails is left out with a warning, and the rows of any earlier file are
/// kept, so its sites of that kind are not lost; the download fails only
/// when every kind does, leaving any earlier file in place.
pub async fn download_kind_sites(
    client: &reqwest::Client,
    endpoint: &str,
    dir: &Path,
    pacing: WikidataPacing,
) -> Result<PathBuf> {
    info!(
        "asking Wikidata for the official websites of {} kinds of organizations",
        KIND_LABELS.len()
    );
    let started = Instant::now();
    let mut tsv = String::from("item\tlabel\twebsite\n");
    let mut seen: HashSet<String> = HashSet::new();
    let mut failed = Vec::new();
    for (i, label) in KIND_LABELS.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(pacing.pause).await;
        }
        let rows = match sparql_json(client, endpoint, &kind_sites_query(label), pacing).await {
            Ok(json) => wikidata_json_to_tsv(&json),
            Err(err) => Err(err),
        };
        match rows {
            Ok(rows) => {
                let mut added = 0;
                for row in rows.lines().skip(1) {
                    if seen.insert(row.to_string()) {
                        tsv.push_str(row);
                        tsv.push('\n');
                        added += 1;
                    }
                }
                info!("Wikidata: {added} official websites of kind \"{label}\"");
            }
            Err(err) => {
                warn!("could not get the official websites of kind \"{label}\": {err:#}");
                failed.push(*label);
            }
        }
    }
    if failed.len() == KIND_LABELS.len() {
        bail!("every Wikidata query for official websites by kind failed");
    }

    crate::storage::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(KIND_SITES_FILE_NAME);
    if !failed.is_empty() {
        // The rows carry no kind, so keep all of the earlier ones.
        if let Ok(earlier) = tokio::fs::read_to_string(&dest).await {
            for row in earlier.lines().skip(1) {
                if !row.is_empty() && seen.insert(row.to_string()) {
                    tsv.push_str(row);
                    tsv.push('\n');
                }
            }
        }
    }
    let part = part_path(&dest);
    crate::storage::write(&part, tsv.as_bytes())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    crate::storage::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!(
        "wrote {} official websites by kind to {} in {:.0} s{}",
        seen.len(),
        dest.display(),
        started.elapsed().as_secs_f64(),
        if failed.is_empty() {
            String::new()
        } else {
            format!(" (left out: {})", failed.join(", "))
        }
    );
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// A SPARQL endpoint that fails every query for banks and answers the
    /// others with one site named after the query's kind.
    async fn banks_fail() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/sparql", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                // Read until the form body, which ends the request, is in.
                while !String::from_utf8_lossy(&request).contains("%7D+%7D") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&request).replace('+', " ");
                let kind = KIND_LABELS
                    .iter()
                    .find(|label| request.contains(&format!("%22{label}%22")))
                    .copied()
                    .unwrap_or("unknown");
                let response = if kind == "bank" {
                    "HTTP/1.1 500 Oops\r\ncontent-length: 0\r\n\r\n".to_string()
                } else {
                    let slug = kind.replace(' ', "-");
                    let body = format!(
                        r#"{{"results":{{"bindings":[{{"item":{{"value":"http://www.wikidata.org/entity/Q1"}},"itemLabel":{{"value":"A {kind}"}},"website":{{"value":"https://{slug}.example/"}}}}]}}}}"#
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/sparql-results+json\r\n\
                         content-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        url
    }

    #[tokio::test]
    async fn a_failed_kind_keeps_its_earlier_sites() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(KIND_SITES_FILE_NAME);
        std::fs::write(
            &dest,
            "item\tlabel\twebsite\nQ9\tFirst Bank\thttps://firstbank.example/\n",
        )
        .unwrap();
        let pacing = WikidataPacing {
            pause: Duration::ZERO,
            retry_wait: Duration::from_millis(1),
        };
        let client = reqwest::Client::new();
        let path = download_kind_sites(&client, &banks_fail().await, dir.path(), pacing)
            .await
            .unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("https://airline.example/"), "{text}");
        assert!(
            text.contains("Q9\tFirst Bank\thttps://firstbank.example/\n"),
            "{text}"
        );
        assert_eq!(text.matches("https://firstbank.example/").count(), 1);
    }

    #[test]
    fn the_query_finds_the_kind_by_label() {
        let query = kind_sites_query("credit union");
        assert!(
            query.contains("?kind rdfs:label \"credit union\"@en"),
            "{query}"
        );
        assert!(query.contains("wdt:P31 ?kind ; wdt:P856 ?website"));
        assert!(!kind_sites_query("a\"b").contains("a\"b"));
    }
}
