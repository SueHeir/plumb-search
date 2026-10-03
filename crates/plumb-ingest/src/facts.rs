//! Wikidata facts about the organizations behind official websites: the
//! country (P17) and what kind of thing each is (instance of, P31, and
//! industry, P452), for the "your country" setting and for queries naming
//! a kind, like "banks".
//!
//! They come from two small SPARQL queries, separate from the official
//! websites query so that each stays well inside the query service's time
//! limit, and are saved as `wikidata-site-facts.tsv` with the header
//! `item\tcountry\tkind`, one fact per row (the other column empty).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use plumb_core::normalize_country;
use serde::Deserialize;
use tracing::{info, warn};

use crate::download::part_path;
use crate::wikidata::bare_item_id;
use crate::{open_maybe_gz, Line, LineReader, OfficialSite};

/// File name of the facts in a seed directory.
pub const FACTS_FILE_NAME: &str = "wikidata-site-facts.tsv";

/// What Wikidata says about one item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteFacts {
    /// ISO 3166-1 alpha-2 code, when the item has exactly one country. An
    /// item with several (a multinational) belongs to none in particular.
    pub country: Option<String>,
    /// English labels of what the item is ("bank", "airline"), in file order.
    pub kinds: Vec<String>,
}

/// Facts by Wikidata item id (`Q739868`).
pub type FactsByItem = HashMap<String, SiteFacts>;

/// The SPARQL query for the country codes of items with an official
/// website and at least `min_sitelinks` sitelinks.
pub fn country_query(min_sitelinks: u32) -> String {
    format!(
        "SELECT DISTINCT ?item ?country WHERE {{ ?item wdt:P856 [] ; wikibase:sitelinks ?sitelinks . FILTER(?sitelinks >= {min_sitelinks}) ?item wdt:P17 ?c . ?c wdt:P297 ?country . }}"
    )
}

/// The SPARQL query for the English labels of what those items are
/// (instance of and industry).
pub fn kind_query(min_sitelinks: u32) -> String {
    format!(
        "SELECT DISTINCT ?item ?kind WHERE {{ ?item wdt:P856 [] ; wikibase:sitelinks ?sitelinks . FILTER(?sitelinks >= {min_sitelinks}) ?item wdt:P31|wdt:P452 ?k . ?k rdfs:label ?kind . FILTER(LANG(?kind) = \"en\") }}"
    )
}

/// Runs both queries against the SPARQL `endpoint` and writes
/// `dir/`[`FACTS_FILE_NAME`]. Fails if either query fails, leaving any
/// earlier file in place.
pub async fn download_site_facts(
    client: &reqwest::Client,
    endpoint: &str,
    dir: &Path,
    min_sitelinks: u32,
) -> Result<PathBuf> {
    info!("asking Wikidata for the countries and kinds of items with at least {min_sitelinks} sitelinks");
    let countries = sparql(client, endpoint, &country_query(min_sitelinks)).await?;
    let kinds = sparql(client, endpoint, &kind_query(min_sitelinks)).await?;
    let tsv = facts_tsv(&countries, &kinds)?;

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
        "wrote {} Wikidata facts to {}",
        tsv.lines().count().saturating_sub(1),
        dest.display()
    );
    Ok(dest)
}

async fn sparql(client: &reqwest::Client, endpoint: &str, query: &str) -> Result<Vec<u8>> {
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
        .body(form)
        .send()
        .await
        .with_context(|| format!("querying {endpoint}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "Wikidata query failed: HTTP {status}: {}",
            plumb_core::truncate_chars(body.trim(), 500)
        );
    }
    Ok(response
        .bytes()
        .await
        .context("reading the Wikidata response")?
        .to_vec())
}

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

/// The facts file for the SPARQL JSON results of [`country_query`] and
/// [`kind_query`].
pub fn facts_tsv(countries_json: &[u8], kinds_json: &[u8]) -> Result<String> {
    let mut tsv = String::from("item\tcountry\tkind\n");
    for (json, column) in [(countries_json, "country"), (kinds_json, "kind")] {
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
            let (Some(item), Some(value)) = (cell("item"), cell(column)) else {
                continue;
            };
            let item = bare_item_id(&item);
            if item.is_empty() {
                continue;
            }
            let (country, kind) = if column == "country" {
                (value.as_str(), "")
            } else {
                ("", value.as_str())
            };
            tsv.push_str(&format!("{item}\t{country}\t{kind}\n"));
        }
    }
    Ok(tsv)
}

/// Reads a facts file ([`FACTS_FILE_NAME`]); it may be gzipped. Rows with
/// an unknown country code are skipped, and an item with more than one
/// country gets none.
pub fn load_site_facts(path: &Path) -> Result<FactsByItem> {
    let mut lines = LineReader::new(open_maybe_gz(path)?);
    let read_err = || format!("reading {}", path.display());
    let mut columns: Option<(usize, usize, usize)> = None;
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
        let Some((item_col, country_col, kind_col)) = columns else {
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
            columns = Some((position("item")?, position("country")?, position("kind")?));
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_becomes_tsv_and_loads() {
        let countries = br#"{"results":{"bindings":[
            {"item":{"value":"http://www.wikidata.org/entity/Q1"},"country":{"value":"US"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q2"},"country":{"value":"DE"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q3"},"country":{"value":"FR"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q3"},"country":{"value":"DE"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q4"},"country":{"value":"XX"}}
        ]}}"#;
        let kinds = br#"{"results":{"bindings":[
            {"item":{"value":"http://www.wikidata.org/entity/Q1"},"kind":{"value":"bank"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q1"},"kind":{"value":"public\tcompany"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q2"},"kind":{"value":"bank"}},
            {"item":{"value":"http://www.wikidata.org/entity/Q5"}}
        ]}}"#;
        let tsv = facts_tsv(countries, kinds).unwrap();
        assert!(tsv.starts_with("item\tcountry\tkind\nQ1\tUS\t\n"));
        assert!(tsv.contains("Q1\t\tpublic company\n"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FACTS_FILE_NAME);
        std::fs::write(&path, &tsv).unwrap();
        let facts = load_site_facts(&path).unwrap();
        assert_eq!(facts["Q1"].country.as_deref(), Some("US"));
        assert_eq!(facts["Q1"].kinds, ["bank", "public company"]);
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
        assert_eq!(sites[1].country, None);
    }

    #[test]
    fn queries_name_the_properties() {
        assert!(country_query(25).contains("wdt:P17"));
        assert!(country_query(25).contains("?sitelinks >= 25"));
        assert!(kind_query(10).contains("wdt:P31|wdt:P452"));
    }
}
