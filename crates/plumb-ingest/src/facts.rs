//! Wikidata facts about the organizations behind official websites: the
//! country (P17), what kind of thing each is (instance of, P31, and
//! industry, P452), for the "your country" setting and for queries naming
//! a kind, like "banks", the English names it is also known by ("NYT",
//! "AA"), so people can name a site the way they say it, its English
//! description ("American bank holding company"), and its number of
//! sitelinks (Wikipedia articles), how widely known it is.
//!
//! They are asked for by item: the items of the official websites file go
//! to the query service in batches of [`FACTS_BATCH`] ids (SPARQL
//! `VALUES`), each answered in a few seconds, since a query over every item
//! with a website runs past the service's 60-second limit. They are saved as
//! `wikidata-site-facts.tsv` with the header
//! `item\tcountry\tkind\talias\tabout\tsitelinks`, one fact per row (the
//! other columns empty). Older files may lack the last three columns.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use plumb_core::normalize_country;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::download::{WikidataPacing, WIKIDATA_PREFIXES};
use crate::wikidata::bare_item_id;
use crate::{load_wikidata_official_sites, open_maybe_gz, Line, LineReader, OfficialSite};

/// File name of the facts in a seed directory.
pub const FACTS_FILE_NAME: &str = "wikidata-site-facts.tsv";

/// The facts file's first line.
pub const FACTS_HEADER: &str = "item\tcountry\tkind\talias\tabout\tsitelinks\n";

/// What Wikidata says about one item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteFacts {
    /// ISO 3166-1 alpha-2 code, when the item has exactly one country. An
    /// item with several (a multinational) belongs to none in particular.
    pub country: Option<String>,
    /// English labels of what the item is ("bank", "airline"), in file order.
    pub kinds: Vec<String>,
    /// Other English names of the item ("NYT"), at most [`MAX_NAMES`], in
    /// file order.
    pub names: Vec<String>,
    /// The English description, cut to [`MAX_ABOUT_CHARS`].
    pub about: Option<String>,
    /// Number of sitelinks; 0 when unknown.
    pub sitelinks: u32,
}

/// Longest description kept, in characters.
pub const MAX_ABOUT_CHARS: usize = 200;

/// The most other names kept per item; items like countries have dozens.
pub const MAX_NAMES: usize = 6;

/// Longer other names are descriptions rather than names, and are skipped.
const MAX_NAME_CHARS: usize = 60;

/// Facts by Wikidata item id (`Q739868`).
pub type FactsByItem = HashMap<String, SiteFacts>;

/// How many items one facts query asks about.
pub const FACTS_BATCH: usize = 2000;

/// Tries of one batch that gets HTTP 429 or 5xx, or cannot connect, before
/// it is split in two.
const BATCH_TRIES: u32 = 3;

/// A batch this small that still fails fails the download.
const MIN_BATCH: usize = 125;

/// The SPARQL query for the country codes (P17), the English labels of
/// what they are (instance of, P31, and industry, P452), the English other
/// names (`skos:altLabel`), the English description and the number of
/// sitelinks of `items`, Wikidata item ids such as
/// `Q739868`.
pub fn facts_query(items: &[String]) -> String {
    let values: String = items.iter().map(|item| format!(" wd:{item}")).collect();
    format!(
        "{WIKIDATA_PREFIXES}SELECT DISTINCT ?item ?country ?kind ?alias ?about ?sitelinks WHERE {{ VALUES ?item {{{values} }} \
         {{ ?item wdt:P17 ?c . ?c wdt:P297 ?country . }} UNION \
         {{ ?item wdt:P31|wdt:P452 ?k . ?k rdfs:label ?kind . FILTER(LANG(?kind) = \"en\") }} UNION \
         {{ ?item skos:altLabel ?alias . FILTER(LANG(?alias) = \"en\") }} UNION \
         {{ ?item schema:description ?about . FILTER(LANG(?about) = \"en\") }} UNION \
         {{ ?item wikibase:sitelinks ?sitelinks . }} }}"
    )
}

/// The distinct items of an official websites file whose claim is a front
/// page ([`OfficialSite::is_root_homepage`]) or a named inner page
/// ([`OfficialSite::is_named_inner_page`]), the only claims facts are used
/// for, in file order.
pub fn official_items(sites_file: &Path) -> Result<Vec<String>> {
    let sites = load_wikidata_official_sites(sites_file)?;
    let mut seen = HashSet::new();
    Ok(sites
        .into_iter()
        .filter(|site| {
            (site.is_root_homepage() || site.is_named_inner_page()) && is_item_id(&site.item)
        })
        .filter_map(|site| seen.insert(site.item.clone()).then_some(site.item))
        .collect())
}

/// True for an item id the query can name: `Q` and digits.
fn is_item_id(item: &str) -> bool {
    item.len() > 1 && item.starts_with('Q') && item[1..].bytes().all(|b| b.is_ascii_digit())
}

/// Asks the SPARQL `endpoint` for the facts of the items of the official
/// websites files `sites_files`, as [`download_site_facts_with`] does
/// without a mirror.
pub async fn download_site_facts(
    client: &reqwest::Client,
    endpoint: &str,
    dir: &Path,
    sites_files: &[PathBuf],
    pacing: WikidataPacing,
) -> Result<PathBuf> {
    download_site_facts_with(client, None, endpoint, dir, sites_files, pacing).await
}

/// Items per query to a mirror such as QLever's: 20,000 took 13 seconds
/// there on 2026-10-09, no longer than 2,000.
pub const MIRROR_FACTS_BATCH: usize = 20_000;

/// Batches in a row that may fail at the mirror before the rest go
/// straight to the main endpoint.
const MIRROR_FAILURES: u32 = 2;

/// The file next to the facts file where each batch's facts are added as
/// they come in, so a download that stops part way can go on from there.
pub const FACTS_PARTIAL_NAME: &str = "wikidata-site-facts.tsv.partial";

/// Facts saved by an earlier download longer ago than this are asked for
/// again.
const PARTIAL_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Asks for the facts of the items of the official websites files
/// `sites_files` ([`official_items`]; files that do not exist are skipped)
/// and writes `dir/`[`FACTS_FILE_NAME`].
///
/// With a `mirror`, such as [`crate::download::QLEVER_WIKIDATA_URL`], the
/// items go to it [`MIRROR_FACTS_BATCH`] at a time; a batch it fails goes to
/// `endpoint` instead, and after two failed batches in a row everything
/// left does. At `endpoint` they go [`FACTS_BATCH`] at a time, with
/// `pacing.pause` between queries; a batch that fails after a few tries
/// (HTTP 429 or 5xx, a cut-off answer, a lost connection) is asked for again
/// in halves, down to [`MIN_BATCH`] items.
///
/// Each batch's facts are added to `dir/`[`FACTS_PARTIAL_NAME`] as they come
/// in, and a later download (within a week) skips the items found there:
/// every item has a sitelinks count, so every item asked about has a row.
/// The file becomes the facts file once all are in. Fails if a batch still
/// fails at `endpoint`, leaving any earlier facts file in place and the
/// facts so far for the next try.
pub async fn download_site_facts_with(
    client: &reqwest::Client,
    mirror: Option<&str>,
    endpoint: &str,
    dir: &Path,
    sites_files: &[PathBuf],
    pacing: WikidataPacing,
) -> Result<PathBuf> {
    let files = sites_files.to_vec();
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        let mut seen = HashSet::new();
        let mut items = Vec::new();
        for file in files.iter().filter(|file| file.is_file()) {
            for item in official_items(file)? {
                if seen.insert(item.clone()) {
                    items.push(item);
                }
            }
        }
        Ok(items)
    })
    .await
    .context("reading the official websites")??;
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let partial = dir.join(FACTS_PARTIAL_NAME);
    let done = saved_items(&partial)?;
    let items: Vec<String> = items
        .into_iter()
        .filter(|item| !done.contains(item))
        .collect();
    if done.is_empty() {
        tokio::fs::write(&partial, FACTS_HEADER)
            .await
            .with_context(|| format!("writing {}", partial.display()))?;
    }
    info!(
        "asking Wikidata for the countries and kinds of {} items{}",
        items.len(),
        match done.len() {
            0 => String::new(),
            n => format!(" ({n} more were saved by an earlier try)"),
        }
    );
    let started = Instant::now();
    let asked = items.len();
    let mut queries = 0u32;
    let mut left: Vec<String> = Vec::new();
    match mirror {
        Some(mirror) => {
            let mut failures = 0u32;
            for batch in items.chunks(MIRROR_FACTS_BATCH) {
                if failures >= MIRROR_FAILURES {
                    left.extend_from_slice(batch);
                    continue;
                }
                if queries > 0 {
                    tokio::time::sleep(pacing.pause).await;
                }
                queries += 1;
                match sparql_json(client, mirror, &facts_query(batch), pacing).await {
                    Ok(json) => {
                        append_facts(&partial, &json).await?;
                        failures = 0;
                    }
                    Err(err) => {
                        failures += 1;
                        warn!(
                            "facts for {} items failed at {mirror} ({err:#}); asking {endpoint}",
                            batch.len()
                        );
                        left.extend_from_slice(batch);
                    }
                }
            }
        }
        None => left = items,
    }

    // The batches still to ask for, the next one last.
    let mut todo: Vec<&[String]> = left.chunks(FACTS_BATCH).rev().collect();
    while let Some(batch) = todo.pop() {
        if queries > 0 {
            tokio::time::sleep(pacing.pause).await;
        }
        queries += 1;
        match sparql_json(client, endpoint, &facts_query(batch), pacing).await {
            Ok(json) => append_facts(&partial, &json).await?,
            Err(err) if batch.len() > MIN_BATCH => {
                warn!(
                    "Wikidata facts for {} items failed ({err:#}); asking for them in halves",
                    batch.len()
                );
                let (lower, upper) = batch.split_at(batch.len() / 2);
                todo.push(upper);
                todo.push(lower);
            }
            Err(err) => {
                return Err(err.context(format!(
                    "asking Wikidata about {} items (the facts so far are kept in {} \
                     for the next try)",
                    batch.len(),
                    partial.display()
                )))
            }
        }
    }

    let dest = dir.join(FACTS_FILE_NAME);
    tokio::fs::rename(&partial, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", partial.display(), dest.display()))?;
    info!(
        "wrote the Wikidata facts of {} items to {} after {queries} queries in {:.0} s",
        done.len() + asked,
        dest.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(dest)
}

/// The items with facts in the `partial` file an earlier download left, when
/// it is less than a week old; none otherwise.
fn saved_items(partial: &Path) -> Result<HashSet<String>> {
    let recent = std::fs::metadata(partial)
        .and_then(|meta| meta.modified())
        .is_ok_and(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .map_or(true, |age| age < PARTIAL_MAX_AGE)
        });
    if !recent {
        return Ok(HashSet::new());
    }
    let text = std::fs::read_to_string(partial)
        .with_context(|| format!("reading {}", partial.display()))?;
    if !text.starts_with(FACTS_HEADER) {
        return Ok(HashSet::new());
    }
    Ok(text
        .lines()
        .skip(1)
        .filter_map(|line| line.split('\t').next())
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect())
}

/// Adds the facts of a [`facts_query`] answer to the `partial` file.
async fn append_facts(partial: &Path, json: &[u8]) -> Result<()> {
    let mut rows = String::new();
    push_facts(&mut rows, json)?;
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(partial)
        .await
        .with_context(|| format!("opening {}", partial.display()))?;
    file.write_all(rows.as_bytes())
        .await
        .with_context(|| format!("writing {}", partial.display()))?;
    file.flush()
        .await
        .with_context(|| format!("writing {}", partial.display()))
}

/// Runs `query`, trying again after HTTP 429 or 5xx, a cut-off answer and
/// failed connections, waiting `pacing.retry_wait`, doubling; returns the
/// SPARQL JSON answer.
pub(crate) async fn sparql_json(
    client: &reqwest::Client,
    endpoint: &str,
    query: &str,
    pacing: WikidataPacing,
) -> Result<Vec<u8>> {
    let mut wait = pacing.retry_wait;
    let mut tries = 0;
    loop {
        tries += 1;
        match sparql(client, endpoint, query).await {
            Ok(json) => return Ok(json),
            Err(Query::Fatal(err)) => return Err(err),
            Err(Query::Again(err)) if tries >= BATCH_TRIES => {
                return Err(err.context(format!("tried {tries} times")))
            }
            Err(Query::Again(err)) => {
                warn!(
                    "Wikidata query: {err:#}; trying again in {:.1} s",
                    wait.as_secs_f64()
                );
                tokio::time::sleep(wait).await;
                wait = wait.saturating_mul(2);
            }
        }
    }
}

/// Why a query gave no answer.
enum Query {
    /// Worth another try, or a smaller batch.
    Again(anyhow::Error),
    /// Not worth either, such as HTTP 400.
    Fatal(anyhow::Error),
}

/// Sends a query once. A whole answer must be JSON, so one cut off at the
/// time limit is [`Query::Again`].
async fn sparql(client: &reqwest::Client, endpoint: &str, query: &str) -> Result<Vec<u8>, Query> {
    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("query", query)
        .finish();
    let response = client
        .post(endpoint)
        .header(reqwest::header::ACCEPT, "application/sparql-results+json")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .timeout(QUERY_TIMEOUT)
        .body(form)
        .send()
        .await
        .map_err(|err| {
            Query::Again(anyhow::Error::new(err).context(format!("querying {endpoint}")))
        })?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let err = anyhow::anyhow!(
            "Wikidata query failed: HTTP {status}: {}",
            plumb_core::truncate_chars(body.trim(), 500)
        );
        return Err(
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                Query::Again(err)
            } else {
                Query::Fatal(err)
            },
        );
    }
    let body = response.bytes().await.map_err(|err| {
        Query::Again(anyhow::Error::new(err).context("reading the Wikidata answer"))
    })?;
    if let Err(err) = serde_json::from_slice::<serde::de::IgnoredAny>(&body) {
        return Err(Query::Again(
            anyhow::Error::new(err).context("the Wikidata answer breaks off"),
        ));
    }
    Ok(body.to_vec())
}

/// A query without a whole answer after this long is tried again.
const QUERY_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Deserialize)]
struct Response {
    results: Results,
}

#[derive(Debug, Deserialize)]
struct Results {
    bindings: Vec<HashMap<String, Term>>,
}

#[derive(Debug, Deserialize)]
struct Term {
    value: String,
}

/// Appends the rows of the SPARQL JSON results of a [`facts_query`] to
/// `tsv`, one fact per row, the others of its five columns empty.
pub fn push_facts(tsv: &mut String, json: &[u8]) -> Result<()> {
    let response: Response =
        serde_json::from_slice(json).context("parsing Wikidata SPARQL results")?;
    for binding in &response.results.bindings {
        let cell = |name: &str| {
            binding
                .get(name)
                .map(|term| {
                    term.value
                        .replace(['\t', '\n', '\r'], " ")
                        .trim()
                        .to_string()
                })
                .filter(|value| !value.is_empty())
        };
        let Some(item) = cell("item") else {
            continue;
        };
        let item = bare_item_id(&item);
        if item.is_empty() {
            continue;
        }
        for (column, name) in ["country", "kind", "alias", "about", "sitelinks"]
            .into_iter()
            .enumerate()
        {
            if let Some(value) = cell(name) {
                let before = "\t".repeat(column + 1);
                let after = "\t".repeat(4 - column);
                tsv.push_str(&format!("{item}{before}{value}{after}\n"));
            }
        }
    }
    Ok(())
}

/// Reads a facts file ([`FACTS_FILE_NAME`]); it may be gzipped. Rows with
/// an unknown country code are skipped, and an item with more than one
/// country gets none. Other names past [`MAX_NAMES`] per item, or longer
/// than 60 characters, are skipped.
pub fn load_site_facts(path: &Path) -> Result<FactsByItem> {
    let mut lines = LineReader::new(open_maybe_gz(path)?);
    let read_err = || format!("reading {}", path.display());
    // item, country, kind, then alias, about and sitelinks when the file
    // has them.
    type Columns = (usize, usize, usize, [Option<usize>; 3]);
    let mut columns: Option<Columns> = None;
    let mut facts = FactsByItem::new();
    let mut countries: HashMap<String, Vec<String>> = HashMap::new();
    while let Some((line_no, line)) = lines.next_line().with_context(read_err)? {
        let Line::Text(line) = line else {
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').map(str::trim).collect();
        let Some((item_col, country_col, kind_col, [alias_col, about_col, sitelinks_col])) =
            columns
        else {
            let position = |name: &str| {
                fields
                    .iter()
                    .position(|f| f.eq_ignore_ascii_case(name))
                    .with_context(|| {
                        format!(
                            "{}:{line_no}: expected the header `item\\tcountry\\tkind`",
                            path.display()
                        )
                    })
            };
            let optional = |name: &str| fields.iter().position(|f| f.eq_ignore_ascii_case(name));
            columns = Some((
                position("item")?,
                position("country")?,
                position("kind")?,
                [optional("alias"), optional("about"), optional("sitelinks")],
            ));
            continue;
        };
        let field = |i: usize| fields.get(i).copied().unwrap_or_default();
        let item = bare_item_id(field(item_col));
        if item.is_empty() {
            continue;
        }
        if let Some(country) = normalize_country(field(country_col)) {
            let known = countries.entry(item.to_string()).or_default();
            if !known.contains(&country) {
                known.push(country);
            }
        }
        let kind = field(kind_col);
        if !kind.is_empty() {
            facts
                .entry(item.to_string())
                .or_default()
                .kinds
                .push(kind.to_string());
        }
        if let Some(sitelinks) = sitelinks_col.and_then(|c| field(c).parse::<u32>().ok()) {
            let entry = facts.entry(item.to_string()).or_default();
            entry.sitelinks = entry.sitelinks.max(sitelinks);
        }
        let about = about_col.map(field).unwrap_or_default();
        if !about.is_empty() {
            let entry = facts.entry(item.to_string()).or_default();
            if entry.about.is_none() {
                entry.about = Some(plumb_core::truncate_chars(about, MAX_ABOUT_CHARS));
            }
        }
        let alias = alias_col.map(field).unwrap_or_default();
        if !alias.is_empty() && alias.chars().count() <= MAX_NAME_CHARS {
            let names = &mut facts.entry(item.to_string()).or_default().names;
            if names.len() < MAX_NAMES && !names.iter().any(|n| n == alias) {
                names.push(alias.to_string());
            }
        }
    }
    if columns.is_none() {
        warn!("{} is empty", path.display());
    }
    for (item, codes) in countries {
        let entry = facts.entry(item).or_default();
        if let [only] = codes.as_slice() {
            entry.country = Some(only.clone());
        }
    }
    info!(
        "loaded Wikidata facts for {} items from {}",
        facts.len(),
        path.display()
    );
    Ok(facts)
}

/// Copies each item's facts onto its official website claims.
pub fn attach_facts(sites: &mut [OfficialSite], facts: &FactsByItem) {
    for site in sites {
        if let Some(found) = facts.get(&site.item) {
            site.country = found.country.clone();
            site.kinds = found.kinds.clone();
            site.names = found.names.clone();
            site.about = found.about.clone();
            site.sitelinks = found.sitelinks;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_becomes_tsv_and_loads() {
        let answer = br#"{"results":{"bindings":[
            {"item":{"value":"http://www.wikidata.org/entity/Q1"},"country":{"value":"US"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q2"},"country":{"value":"DE"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q3"},"country":{"value":"FR"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q3"},"country":{"value":"DE"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q4"},"country":{"value":"XX"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q1"},"alias":{"value":"US Bank"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q1"},"about":{"value":"American bank"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q1"},"sitelinks":{"value":"42"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q1"},"kind":{"value":"bank"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q1"},"kind":{"value":"public\tcompany"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q2"},"kind":{"value":"bank"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q5"}}
        ]}}"#;
        let mut tsv = String::from(FACTS_HEADER);
        push_facts(&mut tsv, answer).unwrap();
        assert!(tsv.starts_with("item\tcountry\tkind\talias\tabout\tsitelinks\nQ1\tUS\t\t\t\t\n"));
        assert!(tsv.contains("Q1\t\tpublic company\t\t\t\n"));
        assert!(tsv.contains("Q1\t\t\tUS Bank\t\t\n"));
        assert!(tsv.contains("Q1\t\t\t\tAmerican bank\t\n"));
        assert!(tsv.contains("Q1\t\t\t\t\t42\n"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FACTS_FILE_NAME);
        std::fs::write(&path, &tsv).unwrap();
        let facts = load_site_facts(&path).unwrap();
        assert_eq!(facts["Q1"].country.as_deref(), Some("US"));
        assert_eq!(facts["Q1"].kinds, ["bank", "public company"]);
        assert_eq!(facts["Q1"].names, ["US Bank"]);
        assert_eq!(facts["Q1"].about.as_deref(), Some("American bank"));
        assert_eq!(facts["Q1"].sitelinks, 42);
        assert_eq!(facts["Q2"].country.as_deref(), Some("DE"));
        // Several countries: a multinational, so none.
        assert_eq!(facts["Q3"].country, None);
        assert!(!facts.contains_key("Q4"));
        assert!(!facts.contains_key("Q5"));

        let mut sites = vec![
            OfficialSite::new("Q1", "U.S. Bancorp", "https://www.usbank.com/").unwrap(),
            OfficialSite::new("Q9", "Other", "https://other.com/").unwrap(),
        ];
        attach_facts(&mut sites, &facts);
        assert_eq!(sites[0].country.as_deref(), Some("US"));
        assert_eq!(sites[0].kinds, ["bank", "public company"]);
        assert_eq!(sites[0].names, ["US Bank"]);
        assert_eq!(sites[1].country, None);
    }

    #[test]
    fn files_without_names_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FACTS_FILE_NAME);
        std::fs::write(&path, "item\tcountry\tkind\nQ1\tUS\t\nQ1\t\tbank\n").unwrap();
        let facts = load_site_facts(&path).unwrap();
        assert_eq!(facts["Q1"].kinds, ["bank"]);
        assert!(facts["Q1"].names.is_empty());

        let long = "x".repeat(61);
        let rows: String = (0..10).map(|i| format!("Q2\t\t\tName {i}\t\t\n")).collect();
        std::fs::write(&path, format!("{FACTS_HEADER}{rows}Q3\t\t\t{long}\t\t\n")).unwrap();
        let facts = load_site_facts(&path).unwrap();
        assert_eq!(facts["Q2"].names.len(), MAX_NAMES);
        assert!(!facts.contains_key("Q3"));
    }

    #[test]
    fn the_query_names_the_items_and_properties() {
        let query = facts_query(&["Q1".to_string(), "Q22".to_string()]);
        assert!(query.contains("VALUES ?item { wd:Q1 wd:Q22 }"), "{query}");
        assert!(query.contains("wdt:P17"));
        assert!(query.contains("wdt:P31|wdt:P452"));
        assert!(query.contains("skos:altLabel"));
        assert!(query.contains("schema:description"));
        assert!(query.contains("wikibase:sitelinks"));
    }

    type Queries = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// A stand-in SPARQL endpoint on a loopback port: answers each query
    /// with `respond(query, n)`, `n` counting the queries before it, and
    /// keeps the queries.
    async fn endpoint<F>(respond: F) -> (String, Queries)
    where
        F: Fn(&str, usize) -> Vec<u8> + Send + 'static,
    {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/sparql", listener.local_addr().unwrap());
        let queries = Queries::default();
        let log = queries.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                let body_start = loop {
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&request[..body_start]).to_ascii_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|n| n.trim().parse().ok())
                    .unwrap_or(0);
                while request.len() < body_start + length {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let query = url::form_urlencoded::parse(&request[body_start..])
                    .find(|(name, _)| name == "query")
                    .map(|(_, query)| query.into_owned())
                    .unwrap_or_default();
                let n = {
                    let mut log = log.lock().unwrap();
                    log.push(query.clone());
                    log.len() - 1
                };
                let _ = socket.write_all(&respond(&query, n)).await;
                let _ = socket.shutdown().await;
            }
        });
        (url, queries)
    }

    /// The items a [`facts_query`] names.
    fn items_of(query: &str) -> Vec<String> {
        let values = query.split_once("VALUES ?item {").unwrap().1;
        let values = values.split_once('}').unwrap().0;
        values
            .split_whitespace()
            .map(|item| item.trim_start_matches("wd:").to_string())
            .collect()
    }

    fn http(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// An answer giving each item of `query` 7 sitelinks.
    fn sitelinks_answer(query: &str) -> Vec<u8> {
        let bindings: Vec<serde_json::Value> = items_of(query)
            .iter()
            .map(|item| {
                serde_json::json!({
                    "item": {"type": "uri", "value": format!("http://www.wikidata.org/entity/{item}")},
                    "sitelinks": {"type": "literal", "value": "7"},
                })
            })
            .collect();
        let body = serde_json::json!({"results": {"bindings": bindings}}).to_string();
        http("200 OK", &body)
    }

    fn quick() -> WikidataPacing {
        WikidataPacing {
            pause: Duration::ZERO,
            retry_wait: Duration::from_millis(1),
        }
    }

    fn sites_file(dir: &Path, items: std::ops::Range<usize>) -> PathBuf {
        let path = dir.join("sites.tsv");
        let rows: String = items
            .map(|i| format!("Q{i}\tItem {i}\thttps://item{i}.org/\n"))
            .collect();
        std::fs::write(&path, format!("item\tlabel\twebsite\n{rows}")).unwrap();
        path
    }

    #[tokio::test]
    async fn a_failing_mirror_hands_its_batches_to_the_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let count = MIRROR_FACTS_BATCH * 2 + 10;
        let sites = sites_file(dir.path(), 1..count + 1);
        let (mirror, asked_mirror) = endpoint(|_, _| http("502 Bad Gateway", "down")).await;
        let (main, asked_main) = endpoint(|query, _| sitelinks_answer(query)).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let path =
            download_site_facts_with(&client, Some(&mirror), &main, dir.path(), &[sites], quick())
                .await
                .unwrap();
        let facts = load_site_facts(&path).unwrap();
        assert_eq!(facts.len(), count);
        // Two batches failed at the mirror (3 tries each); the third was
        // never tried there.
        assert_eq!(asked_mirror.lock().unwrap().len(), 2 * BATCH_TRIES as usize);
        let main_items: usize = asked_main
            .lock()
            .unwrap()
            .iter()
            .map(|q| items_of(q).len())
            .sum();
        assert_eq!(main_items, count);
        assert!(!dir.path().join(FACTS_PARTIAL_NAME).exists());
    }

    #[tokio::test]
    async fn a_later_download_goes_on_from_the_saved_facts() {
        let dir = tempfile::tempdir().unwrap();
        let sites = sites_file(dir.path(), 1..4);
        std::fs::write(
            dir.path().join(FACTS_PARTIAL_NAME),
            format!("{FACTS_HEADER}Q1\t\t\t\t\t30\nQ2\tUS\t\t\t\t\n"),
        )
        .unwrap();
        let (mirror, asked) = endpoint(|query, _| sitelinks_answer(query)).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let path = download_site_facts_with(
            &client,
            Some(&mirror),
            "http://127.0.0.1:9/never",
            dir.path(),
            &[sites],
            quick(),
        )
        .await
        .unwrap();
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(items_of(&asked[0]), ["Q3"]);
        let facts = load_site_facts(&path).unwrap();
        assert_eq!(facts.len(), 3);
        assert_eq!(facts["Q1"].sitelinks, 30);
        assert_eq!(facts["Q3"].sitelinks, 7);
    }

    #[test]
    fn items_come_from_front_page_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sites.tsv");
        std::fs::write(
            &path,
            "item\tlabel\twebsite\nQ1\tA\thttps://a.com/\nQ2\tB\thttps://b.com/inner\nQ1\tA\thttps://a.org/\nbad\tC\thttps://c.com/\nQ3\tD\thttps://d.com\n",
        )
        .unwrap();
        assert_eq!(official_items(&path).unwrap(), ["Q1", "Q3"]);
    }
}
