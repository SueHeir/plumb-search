//! Wikidata facts about the organizations behind official websites: the
//! country (P17), what kind of thing each is (instance of, P31, and
//! industry, P452), for the "your country" setting and for queries naming
//! a kind, like "banks", the English names it is also known by ("NYT",
//! "AA"), so people can name a site the way they say it, and its English
//! description ("American bank holding company").
//!
//! They are asked for by item: the items of the official websites file go
//! to the query service in batches of [`FACTS_BATCH`] ids (SPARQL
//! `VALUES`), each answered in a few seconds, since a query over every item
//! with a website runs past the service's 60-second limit. They are saved as
//! `wikidata-site-facts.tsv` with the header
//! `item\tcountry\tkind\talias\tabout`, one fact per row (the other columns
//! empty). Older files may lack the `alias` and `about` columns.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use plumb_core::normalize_country;
use serde::Deserialize;
use tracing::{info, warn};

use crate::download::{part_path, WikidataPacing};
use crate::wikidata::bare_item_id;
use crate::{load_wikidata_official_sites, open_maybe_gz, Line, LineReader, OfficialSite};

/// File name of the facts in a seed directory.
pub const FACTS_FILE_NAME: &str = "wikidata-site-facts.tsv";

/// The facts file's first line.
pub const FACTS_HEADER: &str = "item\tcountry\tkind\talias\tabout\n";

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
/// names (`skos:altLabel`) and the English description of `items`, Wikidata item ids such as
/// `Q739868`.
pub fn facts_query(items: &[String]) -> String {
    let values: String = items.iter().map(|item| format!(" wd:{item}")).collect();
    format!(
        "SELECT DISTINCT ?item ?country ?kind ?alias ?about WHERE {{ VALUES ?item {{{values} }} \
         {{ ?item wdt:P17 ?c . ?c wdt:P297 ?country . }} UNION \
         {{ ?item wdt:P31|wdt:P452 ?k . ?k rdfs:label ?kind . FILTER(LANG(?kind) = \"en\") }} UNION \
         {{ ?item skos:altLabel ?alias . FILTER(LANG(?alias) = \"en\") }} UNION \
         {{ ?item schema:description ?about . FILTER(LANG(?about) = \"en\") }} }}"
    )
}

/// The distinct items of an official websites file whose claim is a front
/// page ([`OfficialSite::is_root_homepage`]), the only claims facts are
/// used for, in file order.
pub fn official_items(sites_file: &Path) -> Result<Vec<String>> {
    let sites = load_wikidata_official_sites(sites_file)?;
    let mut seen = HashSet::new();
    Ok(sites
        .into_iter()
        .filter(|site| site.is_root_homepage() && is_item_id(&site.item))
        .filter_map(|site| seen.insert(site.item.clone()).then_some(site.item))
        .collect())
}

/// True for an item id the query can name: `Q` and digits.
fn is_item_id(item: &str) -> bool {
    item.len() > 1 && item.starts_with('Q') && item[1..].bytes().all(|b| b.is_ascii_digit())
}

/// Asks the SPARQL `endpoint` for the facts of the items of the official
/// websites files `sites_files` ([`official_items`]; files that do not
/// exist are skipped), [`FACTS_BATCH`] at a
/// time with `pacing.pause` between queries, and writes
/// `dir/`[`FACTS_FILE_NAME`]. A batch that fails after a few tries (HTTP
/// 429 or 5xx, a cut-off answer, a lost connection) is asked for again in
/// halves, down to [`MIN_BATCH`] items. Fails if any batch still fails,
/// leaving any earlier file in place.
pub async fn download_site_facts(
    client: &reqwest::Client,
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
    let batches = items.len().div_ceil(FACTS_BATCH);
    info!(
        "asking Wikidata for the countries and kinds of {} items, in {batches} queries",
        items.len()
    );
    let started = Instant::now();
    let mut tsv = String::from(FACTS_HEADER);
    // The batches still to ask for, the next one last.
    let mut todo: Vec<&[String]> = items.chunks(FACTS_BATCH).rev().collect();
    let mut queries = 0u32;
    while let Some(batch) = todo.pop() {
        if queries > 0 {
            tokio::time::sleep(pacing.pause).await;
        }
        queries += 1;
        match sparql_json(client, endpoint, &facts_query(batch), pacing).await {
            Ok(json) => {
                push_facts(&mut tsv, &json)?;
            }
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
                return Err(err.context(format!("asking Wikidata about {} items", batch.len())))
            }
        }
    }

    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(FACTS_FILE_NAME);
    let part = part_path(&dest);
    tokio::fs::write(&part, tsv.as_bytes())
        .await
        .with_context(|| format!("writing {}", part.display()))?;
    tokio::fs::rename(&part, &dest)
        .await
        .with_context(|| format!("renaming {} to {}", part.display(), dest.display()))?;
    info!(
        "wrote {} Wikidata facts to {} after {queries} queries in {:.0} s",
        tsv.lines().count().saturating_sub(1),
        dest.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(dest)
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
        if let Some(country) = cell("country") {
            tsv.push_str(&format!("{item}\t{country}\t\t\t\n"));
        }
        if let Some(kind) = cell("kind") {
            tsv.push_str(&format!("{item}\t\t{kind}\t\t\n"));
        }
        if let Some(alias) = cell("alias") {
            tsv.push_str(&format!("{item}\t\t\t{alias}\t\n"));
        }
        if let Some(about) = cell("about") {
            tsv.push_str(&format!("{item}\t\t\t\t{about}\n"));
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
    // item, country, kind, then alias and about when the file has them.
    type Columns = (usize, usize, usize, Option<usize>, Option<usize>);
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
        let Some((item_col, country_col, kind_col, alias_col, about_col)) = columns else {
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
                optional("alias"),
                optional("about"),
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
            {"item":{"value":"http://www.wikidata.org/entity/Q1"},"kind":{"value":"bank"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q1"},"kind":{"value":"public\tcompany"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q2"},"kind":{"value":"bank"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q5"}}
        ]}}"#;
        let mut tsv = String::from(FACTS_HEADER);
        push_facts(&mut tsv, answer).unwrap();
        assert!(tsv.starts_with("item\tcountry\tkind\talias\tabout\nQ1\tUS\t\t\t\n"));
        assert!(tsv.contains("Q1\t\tpublic company\t\t\n"));
        assert!(tsv.contains("Q1\t\t\tUS Bank\t\n"));
        assert!(tsv.contains("Q1\t\t\t\tAmerican bank\n"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FACTS_FILE_NAME);
        std::fs::write(&path, &tsv).unwrap();
        let facts = load_site_facts(&path).unwrap();
        assert_eq!(facts["Q1"].country.as_deref(), Some("US"));
        assert_eq!(facts["Q1"].kinds, ["bank", "public company"]);
        assert_eq!(facts["Q1"].names, ["US Bank"]);
        assert_eq!(facts["Q1"].about.as_deref(), Some("American bank"));
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
        let rows: String = (0..10).map(|i| format!("Q2\t\t\tName {i}\t\n")).collect();
        std::fs::write(&path, format!("{FACTS_HEADER}{rows}Q3\t\t\t{long}\t\n")).unwrap();
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
