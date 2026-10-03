//! `plumb fetch-data`: downloads the seed datasets.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use plumb_ingest::{download, facts};
use tracing::{error, info};

use crate::block_on;
use crate::cli::FetchDataArgs;

/// Where release names for `--cc-release` are listed. We know of no
/// machine-readable index of releases, so we point people here instead.
const CC_WEB_GRAPHS_PAGE: &str = "https://commoncrawl.org/web-graphs";

/// `--top` of the suggested `plumb ingest`, as in the README. Besides the
/// records kept, it bounds the Common Crawl rows read, and so the memory used.
const SUGGESTED_TOP: usize = 1_000_000;

/// What happened to one dataset.
#[derive(Debug)]
enum Outcome {
    Saved(PathBuf),
    Skipped(String),
    Failed(anyhow::Error),
}

pub fn run(args: FetchDataArgs) -> Result<()> {
    let cc_url = cc_ranks_url(&args)?;
    std::fs::create_dir_all(&args.dir)
        .with_context(|| format!("creating {}", args.dir.display()))?;
    let client = download::http_client()?;

    let outcomes = block_on(async {
        let tranco = if args.skip_tranco {
            Outcome::Skipped("--skip-tranco".to_string())
        } else {
            info!("downloading the Tranco list");
            outcome(download::download_tranco(&client, &args.dir).await)
        };
        let cc_ranks = match &cc_url {
            Some(url) => {
                info!("downloading Common Crawl domain ranks from {url}");
                outcome(download::download_cc_domain_ranks(&client, url, &args.dir).await)
            }
            None => Outcome::Skipped(format!(
                "pass --cc-release NAME (release names are listed on {CC_WEB_GRAPHS_PAGE}) \
                 or --cc-ranks-url URL"
            )),
        };
        let wikidata = if args.skip_wikidata {
            Outcome::Skipped("--skip-wikidata".to_string())
        } else {
            info!(
                "asking Wikidata for official websites of items with at least {} sitelinks",
                args.wikidata_min_sitelinks
            );
            outcome(
                download::download_wikidata_official_sites(
                    &client,
                    &args.dir,
                    args.wikidata_min_sitelinks,
                )
                .await,
            )
        };
        let facts = if args.skip_wikidata {
            Outcome::Skipped("--skip-wikidata".to_string())
        } else {
            outcome(
                facts::download_site_facts(
                    &client,
                    download::WIKIDATA_SPARQL_URL,
                    &args.dir,
                    args.wikidata_min_sitelinks,
                )
                .await,
            )
        };
        [
            ("tranco", tranco),
            ("cc-ranks", cc_ranks),
            ("wikidata", wikidata),
            ("wikidata-facts", facts),
        ]
    })?;

    let mut failed = Vec::new();
    for (name, outcome) in &outcomes {
        match outcome {
            Outcome::Saved(path) => println!("{name:<9} saved {}", path.display()),
            Outcome::Skipped(why) => println!("{name:<9} skipped: {why}"),
            Outcome::Failed(err) => {
                println!("{name:<9} FAILED: {err:#}");
                failed.push(*name);
            }
        }
    }
    if let Some(command) = ingest_hint(&outcomes, &args.dir) {
        println!("next: {command}");
    }
    if !failed.is_empty() {
        bail!("could not download {}", failed.join(", "));
    }
    Ok(())
}

fn outcome(result: Result<PathBuf>) -> Outcome {
    match result {
        Ok(path) => Outcome::Saved(path),
        Err(err) => {
            error!("{err:#}");
            Outcome::Failed(err)
        }
    }
}

/// The Common Crawl ranks URL to fetch, if any. Release names are checked
/// loosely so that a pasted URL or path is caught before it is spliced into
/// another URL.
fn cc_ranks_url(args: &FetchDataArgs) -> Result<Option<String>> {
    if let Some(url) = &args.cc_ranks_url {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            bail!("--cc-ranks-url must be an http(s) URL, got {url:?}");
        }
        return Ok(Some(url.clone()));
    }
    let Some(release) = &args.cc_release else {
        return Ok(None);
    };
    let release = release.trim();
    let well_formed = !release.is_empty()
        && release
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !well_formed {
        bail!(
            "--cc-release takes a release name such as cc-main-2025-26-nov-dec-jan \
             (see {CC_WEB_GRAPHS_PAGE}), got {release:?}; use --cc-ranks-url for a URL"
        );
    }
    Ok(Some(download::cc_domain_ranks_url(release)))
}

/// The `plumb ingest` command for the files just saved, keeping the best
/// [`SUGGESTED_TOP`] sites.
fn ingest_hint(outcomes: &[(&str, Outcome)], dir: &Path) -> Option<String> {
    let wikidata_saved = outcomes
        .iter()
        .any(|(name, outcome)| *name == "wikidata" && matches!(outcome, Outcome::Saved(_)));
    let flags: Vec<String> = outcomes
        .iter()
        .filter_map(|(name, outcome)| match outcome {
            Outcome::Saved(path) => Some(format!("--{name} {}", path.display())),
            _ => None,
        })
        // Facts only make sense next to the official websites they describe.
        .filter(|flag| !flag.starts_with("--wikidata-facts ") || wikidata_saved)
        .collect();
    if flags.is_empty() {
        return None;
    }
    Some(format!(
        "plumb ingest {} --top {SUGGESTED_TOP} --out {}",
        flags.join(" "),
        dir.join("records.jsonl").display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(release: Option<&str>, url: Option<&str>) -> FetchDataArgs {
        FetchDataArgs {
            dir: PathBuf::from("data"),
            cc_release: release.map(str::to_string),
            cc_ranks_url: url.map(str::to_string),
            skip_tranco: false,
            skip_wikidata: false,
            wikidata_min_sitelinks: 25,
        }
    }

    #[test]
    fn common_crawl_is_optional() {
        assert_eq!(cc_ranks_url(&args(None, None)).unwrap(), None);
    }

    #[test]
    fn release_names_become_urls() {
        let url = cc_ranks_url(&args(Some("cc-main-2025-26-nov-dec-jan"), None))
            .unwrap()
            .unwrap();
        assert_eq!(
            url,
            download::cc_domain_ranks_url("cc-main-2025-26-nov-dec-jan")
        );
        let url = cc_ranks_url(&args(None, Some("https://example.org/r.txt.gz"))).unwrap();
        assert_eq!(url.as_deref(), Some("https://example.org/r.txt.gz"));
    }

    #[test]
    fn bad_release_names_and_urls_are_rejected() {
        for release in ["", "../etc", "https://data.commoncrawl.org/x", "a b"] {
            assert!(
                cc_ranks_url(&args(Some(release), None)).is_err(),
                "{release}"
            );
        }
        assert!(cc_ranks_url(&args(None, Some("ftp://example.org/r.gz"))).is_err());
    }

    #[test]
    fn hint_lists_saved_files_only() {
        let outcomes = [
            ("tranco", Outcome::Saved(PathBuf::from("data/tranco.zip"))),
            ("cc-ranks", Outcome::Skipped("no release".into())),
            ("wikidata", Outcome::Failed(anyhow::anyhow!("timeout"))),
        ];
        assert_eq!(
            ingest_hint(&outcomes, Path::new("data")).as_deref(),
            Some("plumb ingest --tranco data/tranco.zip --top 1000000 --out data/records.jsonl")
        );
        let nothing = [("tranco", Outcome::Skipped("--skip-tranco".into()))];
        assert_eq!(ingest_hint(&nothing, Path::new("data")), None);
    }
}
