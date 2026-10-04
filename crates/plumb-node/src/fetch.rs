//! `plumb fetch-data`: downloads the seed datasets.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use plumb_ingest::{articles, download, facts, intros, kind_sites};
use tracing::{error, info};

use crate::block_on;
use crate::cli::{FetchDataArgs, FetchPagesArgs};

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
    /// Saved by an earlier run within `--keep-days`, so not fetched again.
    Kept(PathBuf),
    Skipped(String),
    Failed(anyhow::Error),
}

/// `plumb fetch-pages`: makes a page set file from Wikimedia's dumps.
pub fn run_pages(args: FetchPagesArgs) -> Result<()> {
    let Some(set) = crate::pages::SetInfo::find(&args.set) else {
        bail!(
            "unknown page set {:?}; there are: {}",
            args.set,
            crate::pages::SETS
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    let Some(lang) = set.id.strip_prefix("wikipedia-") else {
        bail!("fetch-pages cannot make {} yet", set.id);
    };
    let dest = match (&args.out, &args.data) {
        (Some(out), _) => out.clone(),
        (None, Some(data)) => set.file(data),
        (None, None) => bail!("pass --data DIR or --out PATH"),
    };
    let mut dumps = if args.dumps.is_empty() {
        let client = download::http_client()?;
        let days = articles::pageview_days(plumb_core::now_unix(), args.pageview_days);
        block_on(articles::download_article_dumps(
            &client,
            &args.work,
            lang,
            &days,
            args.keep_days,
        ))??
    } else {
        let mut files = args.dumps.iter().cloned();
        articles::ArticleDumps {
            page: files.next().context("no page dump")?,
            page_props: files.next().context("no page_props dump")?,
            redirect: files.next().context("no redirect dump")?,
            pageviews: files.collect(),
            official_sites: None,
        }
    };
    dumps.official_sites = args.official_sites.clone();
    let articles = articles::build_articles(lang, &dumps)?;
    if articles.is_empty() {
        bail!("the dumps gave no articles that were read; nothing was written");
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    articles::write_articles_file(&dest, &articles)?;
    let size = std::fs::metadata(&dest).map_or(0, |m| m.len());
    let with_description = articles.iter().filter(|a| a.description.is_some()).count();
    let with_site = articles.iter().filter(|a| a.site.is_some()).count();
    let views: u64 = articles.iter().map(|a| a.views).sum();
    let share = |n: usize| {
        let top: u64 = articles.iter().take(n).map(|a| a.views).sum();
        100.0 * top as f64 / views.max(1) as f64
    };
    info!(
        "wrote {} articles to {} ({:.1} MB): {} with a description, {} with an official site; \
         the top 100,000 have {:.1}% of the views, the top 1,000,000 {:.1}%",
        articles.len(),
        dest.display(),
        size as f64 / 1e6,
        with_description,
        with_site,
        share(100_000),
        share(1_000_000)
    );
    Ok(())
}

pub fn run(args: FetchDataArgs) -> Result<()> {
    let cc_url = cc_ranks_url(&args)?;
    std::fs::create_dir_all(&args.dir)
        .with_context(|| format!("creating {}", args.dir.display()))?;
    let client = download::http_client()?;
    let kept = |name: &str| recent_file(&args.dir.join(name), args.keep_days);

    let outcomes = block_on(async {
        let tranco = if args.skip_tranco {
            Outcome::Skipped("--skip-tranco".to_string())
        } else if let Some(path) = kept(download::TRANCO_FILE_NAME) {
            Outcome::Kept(path)
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
        } else if let Some(path) = kept(download::WIKIDATA_FILE_NAME) {
            Outcome::Kept(path)
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
        let kind_sites = if args.skip_wikidata {
            Outcome::Skipped("--skip-wikidata".to_string())
        } else if let Some(path) = kept(kind_sites::KIND_SITES_FILE_NAME) {
            Outcome::Kept(path)
        } else {
            outcome(
                kind_sites::download_kind_sites(
                    &client,
                    download::WIKIDATA_SPARQL_URL,
                    &args.dir,
                    download::WikidataPacing::default(),
                )
                .await,
            )
        };
        let sites_files = facts_sources(&args.dir);
        let facts = if args.skip_wikidata {
            Outcome::Skipped("--skip-wikidata".to_string())
        } else if let Some(path) = kept(facts::FACTS_FILE_NAME) {
            Outcome::Kept(path)
        } else if !args.dir.join(download::WIKIDATA_FILE_NAME).is_file() {
            // Facts for the by-kind sites alone would leave out most
            // official sites, and the intros picked from them too.
            Outcome::Skipped("needs the official websites, which are missing".to_string())
        } else {
            outcome(
                facts::download_site_facts(
                    &client,
                    download::WIKIDATA_SPARQL_URL,
                    &args.dir,
                    &sites_files,
                    download::WikidataPacing::default(),
                )
                .await,
            )
        };
        let facts_file = args.dir.join(facts::FACTS_FILE_NAME);
        let intros = if args.skip_wikidata {
            Outcome::Skipped("--skip-wikidata".to_string())
        } else if let Some(path) = kept(intros::INTROS_FILE_NAME) {
            Outcome::Kept(path)
        } else if !facts_file.is_file() {
            Outcome::Skipped("needs the Wikidata facts, which are missing".to_string())
        } else {
            outcome(
                intros::download_wikipedia_intros(
                    &client,
                    download::WIKIDATA_SPARQL_URL,
                    intros::WIKIPEDIA_API_URL,
                    &args.dir,
                    &facts_file,
                    download::WikidataPacing::default(),
                )
                .await,
            )
        };
        [
            ("tranco", tranco),
            ("cc-ranks", cc_ranks),
            ("wikidata", wikidata),
            ("wikidata-kinds", kind_sites),
            ("wikidata-facts", facts),
            ("wikipedia-intros", intros),
        ]
    })?;

    let mut failed = Vec::new();
    for (name, outcome) in &outcomes {
        match outcome {
            Outcome::Saved(path) => println!("{name:<9} saved {}", path.display()),
            Outcome::Kept(path) => println!(
                "{name:<9} kept {} (saved within --keep-days {})",
                path.display(),
                args.keep_days
            ),
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

/// The official-site files on disk that facts are fetched for. A file whose
/// download failed this run keeps its earlier copy, which still counts: facts
/// for only the files saved this run would replace a complete facts file with
/// one that leaves the other file's sites out.
fn facts_sources(dir: &Path) -> Vec<PathBuf> {
    [
        download::WIKIDATA_FILE_NAME,
        kind_sites::KIND_SITES_FILE_NAME,
    ]
    .into_iter()
    .map(|name| dir.join(name))
    .filter(|path| path.is_file())
    .collect()
}

/// `path` if it is a file saved within the last `days` days (never for 0).
fn recent_file(path: &Path, days: u64) -> Option<PathBuf> {
    let modified = std::fs::metadata(path)
        .ok()
        .filter(|meta| meta.is_file())?
        .modified()
        .ok()?;
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    (days > 0 && age < std::time::Duration::from_secs(days * 24 * 60 * 60))
        .then(|| path.to_path_buf())
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

/// The `plumb ingest` command for the files just saved or kept, keeping the best
/// [`SUGGESTED_TOP`] sites.
fn ingest_hint(outcomes: &[(&str, Outcome)], dir: &Path) -> Option<String> {
    let wikidata_saved = outcomes.iter().any(|(name, outcome)| {
        *name == "wikidata" && matches!(outcome, Outcome::Saved(_) | Outcome::Kept(_))
    });
    let flags: Vec<String> = outcomes
        .iter()
        .filter_map(|(name, outcome)| match outcome {
            Outcome::Saved(path) | Outcome::Kept(path) => {
                Some(format!("--{name} {}", path.display()))
            }
            _ => None,
        })
        // Facts, kind sites and intros only go next to the official websites.
        .filter(|flag| {
            !(flag.starts_with("--wikidata-facts ")
                || flag.starts_with("--wikidata-kinds ")
                || flag.starts_with("--wikipedia-intros "))
                || wikidata_saved
        })
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
            keep_days: 0,
        }
    }

    #[test]
    fn facts_cover_official_sites_from_earlier_runs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(facts_sources(dir.path()).is_empty());
        // Only the kinds download worked this run; the main file is from the
        // last run and its sites still need facts.
        let sites = dir.path().join(download::WIKIDATA_FILE_NAME);
        let kinds = dir.path().join(kind_sites::KIND_SITES_FILE_NAME);
        std::fs::write(&sites, "item\tsite\n").unwrap();
        std::fs::write(&kinds, "item\tsite\n").unwrap();
        assert_eq!(facts_sources(dir.path()), [sites.clone(), kinds]);
        std::fs::remove_file(dir.path().join(kind_sites::KIND_SITES_FILE_NAME)).unwrap();
        assert_eq!(facts_sources(dir.path()), [sites]);
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
    fn only_files_saved_within_keep_days_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("facts.tsv");
        assert_eq!(recent_file(&path, 1), None);
        std::fs::write(&path, "item\n").unwrap();
        assert_eq!(recent_file(&path, 1), Some(path.clone()));
        assert_eq!(recent_file(&path, 0), None);
        assert_eq!(recent_file(dir.path(), 1), None);
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
        let kept = [
            ("wikidata", Outcome::Kept(PathBuf::from("data/sites.tsv"))),
            (
                "wikipedia-intros",
                Outcome::Saved(PathBuf::from("data/intros.tsv")),
            ),
        ];
        assert_eq!(
            ingest_hint(&kept, Path::new("data")).as_deref(),
            Some(
                "plumb ingest --wikidata data/sites.tsv --wikipedia-intros data/intros.tsv \
                 --top 1000000 --out data/records.jsonl"
            )
        );
        let nothing = [("tranco", Outcome::Skipped("--skip-tranco".into()))];
        assert_eq!(ingest_hint(&nothing, Path::new("data")), None);
    }
}
