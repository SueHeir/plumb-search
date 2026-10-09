//! Wikipedia articles' leads and other names ([`Article::lead`],
//! [`Article::names`]) from Wikimedia's weekly dump of its search index,
//! without crawling Wikipedia.
//!
//! The dump of `{lang}wiki_content` (under [`CIRRUS_URL`], about 66 files
//! of 600 MB for English) has one JSON line per page with its plain
//! `opening_text`, the paragraph before the first heading, and the
//! `redirect`s that lead to it, those to one of its sections too. Each
//! file is downloaded, read into a small file of `title\tlead\tnames` in
//! the work directory and deleted, a few at a time; a stopped run carries
//! on from the files already read. The leads and names of the most read
//! articles are then added to the articles file.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use futures_util::{stream, StreamExt};
use plumb_core::article::{articles_of, lead_of, write_article, ARTICLES_HEADER, MAX_OTHER_NAMES};
use plumb_core::normalize_text;
use serde::Deserialize;
use tracing::info;

use crate::download::{download_to_file, part_path};
use crate::open_maybe_gz;

/// Where the weekly dumps of Wikimedia's search indexes are.
pub const CIRRUS_URL: &str = "https://dumps.wikimedia.org/other/cirrus_search_index";

/// Files downloaded at once: Wikimedia asks for no more than a few
/// connections.
pub const PARALLEL_DOWNLOADS: usize = 3;

/// The files of the latest whole dump of the articles of Wikipedia in
/// `lang`, from `base` ([`CIRRUS_URL`]): the dump's date and the files'
/// addresses.
pub async fn latest_dump_files(
    client: &reqwest::Client,
    base: &str,
    lang: &str,
) -> Result<(String, Vec<String>)> {
    let listing = get_text(client, &format!("{base}/")).await?;
    let mut dates: Vec<&str> = links(&listing)
        .filter_map(|link| link.strip_suffix('/'))
        .filter(|date| date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    dates.sort_unstable();
    for date in dates.into_iter().rev() {
        let dir = format!("{base}/{date}/index_name={lang}wiki_content/");
        let Ok(listing) = get_text(client, &dir).await else {
            continue;
        };
        // A dump still being written has no _SUCCESS yet.
        if !links(&listing).any(|link| link == "_SUCCESS") {
            continue;
        }
        let files: Vec<String> = links(&listing)
            .filter(|link| link.ends_with(".json.bz2") || link.ends_with(".json.gz"))
            .map(|link| format!("{dir}{link}"))
            .collect();
        if !files.is_empty() {
            return Ok((date.to_string(), files));
        }
    }
    bail!("no whole dump of {lang}wiki_content under {base}")
}

async fn get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {url}"))?;
    Ok(response.text().await?)
}

/// The `href`s of a directory listing.
fn links(listing: &str) -> impl Iterator<Item = &str> {
    listing
        .split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .filter(|link| !link.starts_with('?') && !link.starts_with('/') && *link != "../")
}

/// One page of the dump; other fields are left out.
#[derive(Debug, Deserialize)]
struct DumpPage {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    namespace: Option<i64>,
    #[serde(default)]
    page_type: Option<String>,
    #[serde(default)]
    opening_text: Option<String>,
    #[serde(default)]
    redirect: Vec<DumpRedirect>,
}

#[derive(Debug, Deserialize)]
struct DumpRedirect {
    #[serde(default)]
    namespace: i64,
    title: String,
}

/// An article's lead and the titles that lead to it, as read from the
/// dump.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArticleText {
    pub lead: Option<String>,
    pub redirects: Vec<String>,
}

/// Reads the articles of one file of the dump (bzip2, gzip or plain JSON
/// lines) into `out` as `title\tlead\tredirects` lines, redirects
/// separated by `|`. Returns how many articles it had.
pub fn read_dump_file(path: &Path, out: &mut impl Write) -> Result<u64> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader: Box<dyn BufRead> = if path.extension().is_some_and(|e| e == "bz2") {
        Box::new(BufReader::with_capacity(
            1 << 16,
            bzip2::read::MultiBzDecoder::new(BufReader::new(file)),
        ))
    } else {
        drop(file);
        open_maybe_gz(path)?
    };
    let mut articles = 0u64;
    for line in reader.lines() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        // The bulk format puts an `{"index": ...}` line before each page.
        if !line.contains("\"title\"") {
            continue;
        }
        let Ok(page) = serde_json::from_str::<DumpPage>(&line) else {
            continue;
        };
        if page.namespace.unwrap_or(0) != 0 || page.page_type.as_deref() == Some("redirect") {
            continue;
        }
        let Some(title) = page.title.filter(|t| !t.trim().is_empty()) else {
            continue;
        };
        let lead = page.opening_text.as_deref().and_then(lead_of);
        let redirects: Vec<String> = page
            .redirect
            .into_iter()
            .filter(|r| r.namespace == 0)
            .map(|r| clean(&r.title))
            .filter(|t| !t.is_empty())
            .collect();
        if lead.is_none() && redirects.is_empty() {
            continue;
        }
        writeln!(
            out,
            "{}\t{}\t{}",
            clean(&title),
            lead.as_deref().map(clean).unwrap_or_default(),
            redirects.join("|")
        )?;
        articles += 1;
    }
    Ok(articles)
}

/// `text` fit for a field of a tab-separated, `|`-separated line.
fn clean(text: &str) -> String {
    plumb_core::collapse_whitespace(&text.replace(['\t', '\n', '\r', '|'], " "))
}

/// The file of the articles read from the dump file `url`, in `work`.
fn read_path(work: &Path, url: &str) -> PathBuf {
    let name = url.rsplit('/').next().unwrap_or(url);
    let stem = name
        .trim_end_matches(".bz2")
        .trim_end_matches(".gz")
        .trim_end_matches(".json");
    work.join(format!("{stem}.leads.tsv.gz"))
}

/// Downloads and reads each of `urls` into `work` ([`read_dump_file`]),
/// [`PARALLEL_DOWNLOADS`] at a time, deleting each download once read
/// unless `keep`. A file read by an earlier run is not fetched again.
/// Returns the files read, in the order of `urls`.
pub async fn fetch_dump(
    client: &reqwest::Client,
    urls: &[String],
    work: &Path,
    keep: bool,
) -> Result<Vec<PathBuf>> {
    tokio::fs::create_dir_all(work)
        .await
        .with_context(|| format!("creating {}", work.display()))?;
    let total = urls.len();
    let results: Vec<Result<PathBuf>> = stream::iter(urls.iter().enumerate())
        .map(|(n, url)| async move {
            let read = read_path(work, url);
            if read.is_file() {
                return Ok(read);
            }
            let name = url.rsplit('/').next().unwrap_or(url);
            let dump = work.join(name);
            download_to_file(client, url, &dump).await?;
            let read_to = read.clone();
            let dump_path = dump.clone();
            let articles = tokio::task::spawn_blocking(move || -> Result<u64> {
                let part = part_path(&read_to);
                let file = std::fs::File::create(&part)
                    .with_context(|| format!("creating {}", part.display()))?;
                let mut out = flate2::write::GzEncoder::new(
                    std::io::BufWriter::new(file),
                    flate2::Compression::fast(),
                );
                let articles = read_dump_file(&dump_path, &mut out)?;
                out.finish()?
                    .into_inner()
                    .map_err(|e| e.into_error())?
                    .sync_all()?;
                std::fs::rename(&part, &read_to)?;
                Ok(articles)
            })
            .await??;
            if !keep {
                let _ = tokio::fs::remove_file(&dump).await;
            }
            info!("read {articles} articles of {name} ({} of {total})", n + 1);
            Ok(read)
        })
        .buffered(PARALLEL_DOWNLOADS)
        .collect()
        .await;
    results.into_iter().collect()
}

/// What [`add_leads_to_file`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AddedLeads {
    pub articles: u64,
    pub with_lead: u64,
    pub with_names: u64,
    pub names: u64,
}

/// Adds to the first `top` articles of the articles file `path` their
/// lead and other names from the files `read` ([`fetch_dump`]),
/// replacing what they had. Other names are the titles that lead to an
/// article and are not already its title or an alias, the shortest
/// first (a redirect to a section is a short name of what it covers:
/// "Manubrium"), at most [`MAX_OTHER_NAMES`].
pub fn add_leads_to_file(path: &Path, read: &[PathBuf], top: usize) -> Result<AddedLeads> {
    // The titles of the first `top` articles, then their text.
    let mut wanted: HashMap<String, usize> = HashMap::new();
    {
        let reader = open_maybe_gz(path)?;
        for (_, article) in articles_of(reader.lines().map_while(Result::ok)) {
            if wanted.len() >= top {
                break;
            }
            if let Ok(article) = article {
                let next = wanted.len();
                wanted.entry(article.title).or_insert(next);
            }
        }
    }
    let mut texts: Vec<Option<ArticleText>> = vec![None; wanted.len()];
    for file in read {
        let reader = open_maybe_gz(file)?;
        for line in reader.lines() {
            let line = line.with_context(|| format!("reading {}", file.display()))?;
            let mut cols = line.split('\t');
            let (Some(title), Some(lead), Some(redirects)) =
                (cols.next(), cols.next(), cols.next())
            else {
                continue;
            };
            let Some(&at) = wanted.get(title) else {
                continue;
            };
            texts[at] = Some(ArticleText {
                lead: (!lead.is_empty()).then(|| lead.to_string()),
                redirects: redirects
                    .split('|')
                    .filter(|r| !r.is_empty())
                    .map(str::to_string)
                    .collect(),
            });
        }
    }
    let reader = open_maybe_gz(path)?;
    let mut failed = None;
    let lines = reader.lines().map_while(|line| match line {
        Ok(line) => Some(line),
        Err(err) => {
            failed = Some(err);
            None
        }
    });
    let part = part_path(path);
    let file =
        std::fs::File::create(&part).with_context(|| format!("creating {}", part.display()))?;
    let mut out = flate2::write::GzEncoder::new(
        std::io::BufWriter::new(file),
        flate2::Compression::default(),
    );
    out.write_all(ARTICLES_HEADER.as_bytes())?;
    let mut added = AddedLeads::default();
    for (n, article) in articles_of(lines) {
        let mut article = article.with_context(|| format!("{} line {n}", path.display()))?;
        added.articles += 1;
        if let Some(text) = wanted.get(&article.title).and_then(|&at| texts[at].take()) {
            article.lead = text.lead;
            article.names = other_names(&article.title, &article.aliases, text.redirects);
            added.with_lead += u64::from(article.lead.is_some());
            added.with_names += u64::from(!article.names.is_empty());
            added.names += article.names.len() as u64;
        }
        write_article(&mut out, &article)?;
    }
    if let Some(err) = failed {
        return Err(anyhow::Error::new(err).context(format!("reading {}", path.display())));
    }
    out.finish()?
        .into_inner()
        .map_err(|e| e.into_error())?
        .sync_all()?;
    std::fs::rename(&part, path)
        .with_context(|| format!("renaming {} to {}", part.display(), path.display()))?;
    Ok(added)
}

/// The `redirects` to an article that are not its `title` or one of its
/// `aliases` written another way, the shortest first, at most
/// [`MAX_OTHER_NAMES`].
fn other_names(title: &str, aliases: &[String], mut redirects: Vec<String>) -> Vec<String> {
    let mut seen: Vec<String> = std::iter::once(title)
        .chain(aliases.iter().map(String::as_str))
        .map(normalize_text)
        .collect();
    redirects.sort_by_key(|r| (r.split_whitespace().count(), r.chars().count()));
    let mut names = Vec::new();
    for redirect in redirects {
        if names.len() == MAX_OTHER_NAMES {
            break;
        }
        let key = normalize_text(&redirect);
        if key.is_empty() || seen.contains(&key) {
            continue;
        }
        seen.push(key);
        names.push(redirect);
    }
    names
}

#[cfg(test)]
mod tests {
    use plumb_core::article::{read_articles, Article};

    use super::*;

    #[test]
    fn reads_leads_and_redirects_from_the_dump() {
        let dump = [
            r#"{"index": {"_id": 1}}"#,
            r#"{"page_id": 1, "namespace": 0, "page_type": "primary", "title": "Sternum", "opening_text": "The sternum or breastbone is a long flat bone located in the central part of the chest. It connects to the ribs.", "redirect": [{"namespace": 0, "title": "Breastbone"}, {"namespace": 0, "title": "Manubrium"}, {"namespace": 1, "title": "Talk thing"}]}"#,
            r#"{"index": {"_id": 2}}"#,
            r#"{"page_id": 2, "namespace": 0, "page_type": "redirect", "title": "Manubrium", "redirect": []}"#,
            r#"{"page_id": 3, "namespace": 0, "page_type": "primary", "title": "Empty", "redirect": []}"#,
        ]
        .join("\n");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, dump).unwrap();
        let mut out = Vec::new();
        assert_eq!(read_dump_file(&path, &mut out).unwrap(), 1);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Sternum\tThe sternum or breastbone is a long flat bone located in the central part of the chest. It connects to the ribs.\tBreastbone|Manubrium\n"
        );
    }

    #[test]
    fn adds_leads_and_other_names_to_the_most_read() {
        let dir = tempfile::tempdir().unwrap();
        let articles = dir.path().join("wikipedia-en.tsv.gz");
        crate::articles::write_articles_file(
            &articles,
            &[
                Article {
                    title: "Sternum".into(),
                    item: Some("Q1".into()),
                    views: 10,
                    aliases: vec!["Breastbone".into()],
                    ..Article::default()
                },
                Article {
                    title: "Clavicle".into(),
                    item: Some("Q2".into()),
                    views: 5,
                    ..Article::default()
                },
            ],
        )
        .unwrap();
        let read = dir.path().join("x.leads.tsv.gz");
        let mut out = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        writeln!(
            out,
            "Sternum\tThe sternum is a bone.\tbreastbone|Manubrium sterni|Manubrium|STERNUM"
        )
        .unwrap();
        writeln!(out, "Clavicle\tThe clavicle is a bone.\tCollarbone").unwrap();
        std::fs::write(&read, out.finish().unwrap()).unwrap();
        let added = add_leads_to_file(&articles, &[read], 1).unwrap();
        assert_eq!(
            added,
            AddedLeads {
                articles: 2,
                with_lead: 1,
                with_names: 1,
                names: 2
            }
        );
        let back = read_articles(open_maybe_gz(&articles).unwrap(), 10).unwrap();
        assert_eq!(back[0].lead.as_deref(), Some("The sternum is a bone."));
        assert_eq!(back[0].names, ["Manubrium", "Manubrium sterni"]);
        assert_eq!(back[1].lead, None);
    }

    #[test]
    fn finds_the_files_of_a_listing() {
        let listing = r#"<a href="../">../</a>
<a href="_SUCCESS">_SUCCESS</a>
<a href="enwiki_content-20261004-00000.json.bz2">x</a>"#;
        assert_eq!(
            links(listing).collect::<Vec<_>>(),
            ["_SUCCESS", "enwiki_content-20261004-00000.json.bz2"]
        );
        assert_eq!(
            read_path(
                Path::new("w"),
                "https://x/index_name=enwiki_content/enwiki_content-20261004-00000.json.bz2"
            ),
            Path::new("w/enwiki_content-20261004-00000.leads.tsv.gz")
        );
    }
}
