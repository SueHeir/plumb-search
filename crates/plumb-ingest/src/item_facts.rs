//! Facts about the items of a Wikipedia articles file ([`plumb_core::facts`]):
//! a country's capital and population, a mountain's elevation, a person's
//! birth date, a company's founders and CEO, from Wikidata.
//!
//! Each kind's property is asked for whole, [`FACTS_PAGE`] statements at a
//! time, as [`crate::profiles`] asks for profiles: a query for one
//! property is a scan of one index. Only best-ranked statements count
//! (the current CEO, not past ones), and only items with an article are
//! kept. Values that are other items (Canberra, a founder) are then named
//! by their English labels, [`LABELS_BATCH`] items at a time. The facts
//! are added to the articles file's lines of profiles
//! ([`add_facts_to_file`]).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use plumb_core::article::{articles_of, write_article, ARTICLES_HEADER};
use plumb_core::facts::{Date, Fact, FactKind, ValueType, KINDS};
use serde::Deserialize;
use tracing::info;

use crate::download::{part_path, WikidataPacing};
use crate::facts::sparql_json;
use crate::open_maybe_gz;

/// Statements asked for in one query.
pub const FACTS_PAGE: usize = 200_000;

/// Items asked about in one query for their labels.
pub const LABELS_BATCH: usize = 400;

/// Facts by Wikidata item (`Q408`).
pub type FactsByItem = HashMap<String, Vec<Fact>>;

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

fn bindings(json: &[u8]) -> Result<Vec<HashMap<String, Term>>> {
    let response: Response = serde_json::from_slice(json).context("reading the Wikidata answer")?;
    Ok(response.results.bindings)
}

/// `Q42` of `http://www.wikidata.org/entity/Q42`.
fn entity_id(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or(uri)
}

fn is_item_id(id: &str) -> bool {
    id.len() > 1 && id.starts_with('Q') && id[1..].bytes().all(|b| b.is_ascii_digit())
}

/// The query for a page of `kind`'s best-ranked statements: `?item` and
/// `?v`, and the precision `?p` of a date or the year `?t` a population
/// was counted.
fn page_query(kind: FactKind, offset: usize) -> String {
    let p = kind.property();
    let value = match kind.value_type() {
        ValueType::Item => format!("ps:{p} ?v ."),
        // A count has no unit to normalize.
        ValueType::Quantity if kind == FactKind::Population => {
            format!("psv:{p}/wikibase:quantityAmount ?v . OPTIONAL {{ ?s pq:P585 ?t . }}")
        }
        // Metres and square metres, whatever unit the statement is in.
        ValueType::Quantity => format!("psn:{p}/wikibase:quantityAmount ?v ."),
        ValueType::Time => {
            format!("psv:{p} ?tv . ?tv wikibase:timeValue ?v ; wikibase:timePrecision ?p .")
        }
    };
    format!(
        "SELECT ?item ?v ?p ?t WHERE {{ ?item p:{p} ?s . ?s a wikibase:BestRank ; {value} }} \
         LIMIT {FACTS_PAGE} OFFSET {offset}"
    )
}

/// The facts read before items are named: values that are items are
/// their ids.
#[derive(Debug, Default)]
pub struct RawFacts {
    facts: FactsByItem,
    /// A population's year, to keep the latest count when there are more.
    counted: HashMap<String, i32>,
}

impl RawFacts {
    /// Adds the rows of an answer to [`page_query`], for items in
    /// `wanted`; returns how many rows there were.
    fn add_page(&mut self, kind: FactKind, wanted: &HashSet<String>, json: &[u8]) -> Result<usize> {
        let rows = bindings(json)?;
        for row in &rows {
            let (Some(item), Some(v)) = (row.get("item"), row.get("v")) else {
                continue;
            };
            let item = entity_id(&item.value);
            if !wanted.contains(item) {
                continue;
            }
            let value = match kind.value_type() {
                ValueType::Item => {
                    let id = entity_id(&v.value);
                    if !is_item_id(id) {
                        continue;
                    }
                    id.to_string()
                }
                ValueType::Quantity => {
                    let Ok(amount) = v.value.trim_start_matches('+').parse::<f64>() else {
                        continue;
                    };
                    if !amount.is_finite() || amount < 0.0 {
                        continue;
                    }
                    let year = row
                        .get("t")
                        .and_then(|t| Date::from_wikidata(&t.value, 9))
                        .map(|date| date.year);
                    if kind == FactKind::Population {
                        let latest = self.counted.get(item).copied();
                        let kept = self
                            .facts
                            .get(item)
                            .is_some_and(|facts| facts.iter().any(|fact| fact.kind == kind));
                        if kept && year.unwrap_or(i32::MIN) <= latest.unwrap_or(i32::MIN) {
                            continue;
                        }
                        if let Some(facts) = self.facts.get_mut(item) {
                            facts.retain(|fact| fact.kind != kind);
                        }
                        if let Some(year) = year {
                            self.counted.insert(item.to_string(), year);
                        }
                    }
                    let amount = format_amount(amount);
                    match year {
                        Some(year) if kind == FactKind::Population => format!("{amount};{year}"),
                        _ => amount,
                    }
                }
                ValueType::Time => {
                    let precision = row.get("p").and_then(|p| p.value.parse().ok()).unwrap_or(0);
                    match Date::from_wikidata(&v.value, precision) {
                        Some(date) => date.write(),
                        None => continue,
                    }
                }
            };
            let facts = self.facts.entry(item.to_string()).or_default();
            if facts.iter().filter(|fact| fact.kind == kind).count() < kind.most_values()
                && !facts
                    .iter()
                    .any(|fact| fact.kind == kind && fact.value == value)
            {
                facts.push(Fact { kind, value });
            }
        }
        Ok(rows.len())
    }

    /// The items the facts name, to look up their labels.
    fn named_items(&self) -> Vec<String> {
        let mut items: Vec<String> = self
            .facts
            .values()
            .flatten()
            .filter(|fact| fact.kind.value_type() == ValueType::Item)
            .map(|fact| fact.value.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        items.sort_unstable();
        items
    }

    /// The facts, items named by `labels`; facts naming an item without an
    /// English label are left out.
    fn named(self, labels: &HashMap<String, String>) -> FactsByItem {
        let mut out = FactsByItem::new();
        for (item, facts) in self.facts {
            let facts: Vec<Fact> = facts
                .into_iter()
                .filter_map(|fact| match fact.kind.value_type() {
                    ValueType::Item => Some(Fact {
                        value: labels.get(&fact.value)?.clone(),
                        kind: fact.kind,
                    }),
                    _ => Some(fact),
                })
                .collect();
            if !facts.is_empty() {
                out.insert(item, facts);
            }
        }
        out
    }
}

/// `amount` without needless digits: `8848.86`, `27204809`.
fn format_amount(amount: f64) -> String {
    if amount.fract() == 0.0 && amount.abs() < 1e15 {
        format!("{}", amount as i64)
    } else {
        let text = format!("{amount:.3}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn labels_query(items: &[String]) -> String {
    let values: Vec<String> = items.iter().map(|item| format!("wd:{item}")).collect();
    format!(
        "SELECT ?item ?label WHERE {{ VALUES ?item {{ {} }} ?item rdfs:label ?label . \
         FILTER(LANG(?label) = \"en\") }}",
        values.join(" ")
    )
}

fn add_labels(labels: &mut HashMap<String, String>, json: &[u8]) -> Result<()> {
    for row in bindings(json)? {
        if let (Some(item), Some(label)) = (row.get("item"), row.get("label")) {
            let label = plumb_core::collapse_whitespace(&label.value);
            if !label.is_empty() {
                labels.insert(entity_id(&item.value).to_string(), label);
            }
        }
    }
    Ok(())
}

/// Asks Wikidata's query service at `endpoint` for the facts of the items
/// in `wanted` (those with an article).
pub async fn fetch_facts(
    client: &reqwest::Client,
    endpoint: &str,
    pacing: WikidataPacing,
    wanted: &HashSet<String>,
) -> Result<FactsByItem> {
    let mut raw = RawFacts::default();
    for &kind in KINDS {
        let mut offset = 0;
        loop {
            tokio::time::sleep(pacing.pause).await;
            let json = sparql_json(client, endpoint, &page_query(kind, offset), pacing)
                .await
                .with_context(|| format!("asking Wikidata for {} facts", kind.key()))?;
            let rows = raw.add_page(kind, wanted, &json)?;
            info!(
                "{} ({}): {rows} statements from {offset}",
                kind.key(),
                kind.property()
            );
            if rows < FACTS_PAGE {
                break;
            }
            offset += FACTS_PAGE;
        }
    }
    let items = raw.named_items();
    info!("naming {} items the facts are about", items.len());
    let mut labels = HashMap::new();
    for (n, batch) in items.chunks(LABELS_BATCH).enumerate() {
        tokio::time::sleep(pacing.pause).await;
        let json = sparql_json(client, endpoint, &labels_query(batch), pacing)
            .await
            .context("asking Wikidata for labels")?;
        add_labels(&mut labels, &json)?;
        if n % 50 == 0 {
            info!(
                "labels: {} of {} items",
                (n + 1) * LABELS_BATCH,
                items.len()
            );
        }
    }
    Ok(raw.named(&labels))
}

/// What [`add_facts_to_file`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AddedFacts {
    pub articles: u64,
    pub with_facts: u64,
    pub facts: u64,
}

/// Rewrites the articles file `path` with the facts in `facts`, replacing
/// any it had and keeping everything else.
pub fn add_facts_to_file(path: &Path, facts: &FactsByItem) -> Result<AddedFacts> {
    let reader = open_maybe_gz(path)?;
    let mut failed = None;
    let lines = std::io::BufRead::lines(reader).map_while(|line| match line {
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
    let mut added = AddedFacts::default();
    for (n, article) in articles_of(lines) {
        let mut article = article.with_context(|| format!("{} line {n}", path.display()))?;
        article.facts = article
            .item
            .as_deref()
            .and_then(|item| facts.get(item))
            .cloned()
            .unwrap_or_default();
        added.articles += 1;
        if !article.facts.is_empty() {
            added.with_facts += 1;
            added.facts += article.facts.len() as u64;
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

#[cfg(test)]
mod tests {
    use plumb_core::article::{read_articles, Article};

    use super::*;

    fn answer(rows: &[&[(&str, &str)]]) -> Vec<u8> {
        let bindings: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                let mut map = serde_json::Map::new();
                for (k, v) in *row {
                    map.insert((*k).into(), serde_json::json!({ "value": v }));
                }
                serde_json::Value::Object(map)
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({ "results": { "bindings": bindings } })).unwrap()
    }

    const E: &str = "http://www.wikidata.org/entity/";

    #[test]
    fn queries_ask_for_best_ranked_statements() {
        let q = page_query(FactKind::Elevation, 0);
        assert!(q.contains("p:P2044 ?s"), "{q}");
        assert!(q.contains("wikibase:BestRank"), "{q}");
        assert!(q.contains("psn:P2044/wikibase:quantityAmount ?v"), "{q}");
        let q = page_query(FactKind::Population, 400_000);
        assert!(q.contains("psv:P1082/wikibase:quantityAmount ?v"), "{q}");
        assert!(q.contains("pq:P585 ?t"), "{q}");
        assert!(q.ends_with("LIMIT 200000 OFFSET 400000"), "{q}");
        let q = page_query(FactKind::Born, 0);
        assert!(q.contains("wikibase:timePrecision ?p"), "{q}");
    }

    #[test]
    fn facts_are_read_named_and_written() {
        let wanted: HashSet<String> = ["Q408", "Q513", "Q937", "Q478214"]
            .into_iter()
            .map(String::from)
            .collect();
        let mut raw = RawFacts::default();
        let au = format!("{E}Q408");
        let canberra = format!("{E}Q3114");
        raw.add_page(
            FactKind::Capital,
            &wanted,
            &answer(&[
                &[("item", &au), ("v", &canberra)],
                // Not an item with an article.
                &[("item", &format!("{E}Q1")), ("v", &canberra)],
            ]),
        )
        .unwrap();
        raw.add_page(
            FactKind::Population,
            &wanted,
            &answer(&[
                &[
                    ("item", &au),
                    ("v", "+25690023"),
                    ("t", "2021-01-01T00:00:00Z"),
                ],
                &[
                    ("item", &au),
                    ("v", "+27204809"),
                    ("t", "2024-01-01T00:00:00Z"),
                ],
                &[
                    ("item", &au),
                    ("v", "+19000000"),
                    ("t", "2000-01-01T00:00:00Z"),
                ],
            ]),
        )
        .unwrap();
        let everest = format!("{E}Q513");
        raw.add_page(
            FactKind::Elevation,
            &wanted,
            &answer(&[&[("item", &everest), ("v", "8848.86")]]),
        )
        .unwrap();
        let einstein = format!("{E}Q937");
        raw.add_page(
            FactKind::Born,
            &wanted,
            &answer(&[
                &[
                    ("item", &einstein),
                    ("v", "1879-03-14T00:00:00Z"),
                    ("p", "11"),
                ],
                // A second, less precise date is left out.
                &[
                    ("item", &einstein),
                    ("v", "1879-01-01T00:00:00Z"),
                    ("p", "9"),
                ],
            ]),
        )
        .unwrap();
        let tesla = format!("{E}Q478214");
        raw.add_page(
            FactKind::Ceo,
            &wanted,
            &answer(&[&[("item", &tesla), ("v", &format!("{E}Q317521"))]]),
        )
        .unwrap();
        assert_eq!(raw.named_items(), ["Q3114", "Q317521"]);
        let mut labels = HashMap::new();
        add_labels(
            &mut labels,
            &answer(&[&[("item", &canberra), ("label", "Canberra")]]),
        )
        .unwrap();
        let facts = raw.named(&labels);
        let fact = |kind, value: &str| Fact {
            kind,
            value: value.into(),
        };
        assert_eq!(
            facts["Q408"],
            [
                fact(FactKind::Capital, "Canberra"),
                fact(FactKind::Population, "27204809;2024")
            ]
        );
        assert_eq!(facts["Q513"], [fact(FactKind::Elevation, "8848.86")]);
        assert_eq!(facts["Q937"], [fact(FactKind::Born, "1879-03-14")]);
        // The CEO has no English label, so Tesla keeps no fact.
        assert!(!facts.contains_key("Q478214"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wikipedia-en.tsv.gz");
        let australia = Article {
            title: "Australia".into(),
            item: Some("Q408".into()),
            views: 10,
            ..Article::default()
        };
        crate::articles::write_articles_file(&path, std::slice::from_ref(&australia)).unwrap();
        let added = add_facts_to_file(&path, &facts).unwrap();
        assert_eq!(added.with_facts, 1);
        let back = read_articles(open_maybe_gz(&path).unwrap(), 10).unwrap();
        assert_eq!(back[0].facts, facts["Q408"]);
    }

    #[test]
    fn amounts_keep_what_they_need() {
        assert_eq!(format_amount(27204809.0), "27204809");
        assert_eq!(format_amount(8848.86), "8848.86");
        assert_eq!(format_amount(0.5), "0.5");
    }
}
