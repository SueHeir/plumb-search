//! Facts about the items of a Wikipedia articles file ([`plumb_core::facts`]):
//! a country's capital and population, a mountain's elevation, a person's
//! birth date, a company's founders and CEO, from Wikidata.
//!
//! Each kind's property is asked for whole, [`FACTS_PAGE`] statements at a
//! time, as [`crate::profiles`] asks for profiles: a query for one
//! property is a scan of one index. Pages far into a big property
//! (populations, birth dates) take the query service longer, so pages are
//! smaller than profiles', and a kind the service keeps failing to answer
//! is left with what it gave so far rather than losing every other kind. Only best-ranked statements count
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
use tracing::{info, warn};

use crate::download::{part_path, WikidataPacing};
use crate::facts::sparql_json;
use crate::open_maybe_gz;

/// Statements asked for in one query.
pub const FACTS_PAGE: usize = 50_000;

/// Items asked about in one query for their labels.
pub const LABELS_BATCH: usize = 400;

/// Most-read items asked about one by one (in batches of
/// [`LABELS_BATCH`]) for a kind whose pages Wikidata stopped answering.
pub const FILL_IN_TOP: usize = 100_000;

/// Most-read items asked about by name for every other kind: pages read
/// by offset without an order can skip statements (France's area).
pub const FILL_IN_ALWAYS: usize = 20_000;

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
    #[serde(rename = "xml:lang", default)]
    lang: Option<String>,
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
        // Founders and authors stay theirs; a capital, currency, CEO or
        // spouse that ended is not the item's ([`FactKind::current`]). One
        // only some part's (the CFP franc of French Polynesia, South
        // Africa's three capitals) counts only when there is no other. A
        // CEO's start `?t` keeps the latest.
        ValueType::Item => format!(
            "ps:{p} ?v . {}OPTIONAL {{ ?s pq:P518 ?part . }}{}",
            if kind.current() {
                "FILTER NOT EXISTS { ?s pq:P582 [] } "
            } else {
                ""
            },
            if kind.latest_only() {
                " OPTIONAL { ?s pq:P580 ?t . }"
            } else {
                ""
            }
        ),
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
        "SELECT ?item ?v ?p ?t ?part WHERE {{ ?item p:{p} ?s . ?s a wikibase:BestRank ; {value} }} \
         LIMIT {FACTS_PAGE} OFFSET {offset}"
    )
}

/// [`page_query`]'s statements for `items` only.
fn items_query(kind: FactKind, items: &[String]) -> String {
    let values: Vec<String> = items.iter().map(|item| format!("wd:{item}")).collect();
    let page = page_query(kind, 0);
    let (head, rest) = page
        .split_once("WHERE { ")
        .expect("a page query has a WHERE");
    let body = rest.rsplit_once(" LIMIT").map_or(rest, |(body, _)| body);
    format!(
        "{head}WHERE {{ VALUES ?item {{ {} }} {body}",
        values.join(" ")
    )
}

/// The facts read before items are named: values that are items are
/// their ids.
#[derive(Debug, Default)]
pub struct RawFacts {
    facts: FactsByItem,
    /// A population's year, to keep the latest count when there are more.
    counted: HashMap<String, i32>,
    /// Items whose best statements of a date kind disagree on the year
    /// (France founded in 481 and in 843): no date is better than either.
    disputed: HashSet<(String, FactKind)>,
    /// Values only for some part of an item (P518), used when it has no
    /// other of their kind.
    partial: HashMap<(String, FactKind), Vec<String>>,
    /// The start (`2025-03-18T…`) of the CEOs kept, to keep the latest.
    started: HashMap<(String, FactKind), String>,
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
                    if row.contains_key("part") {
                        let values = self.partial.entry((item.to_string(), kind)).or_default();
                        if values.len() < kind.most_values() && !values.iter().any(|v| v == id) {
                            values.push(id.to_string());
                        }
                        continue;
                    }
                    if kind.latest_only() {
                        // No start counts as the earliest.
                        let start = row.get("t").map_or("", |t| t.value.as_str());
                        let key = (item.to_string(), kind);
                        let kept = self.started.get(&key).map_or("", String::as_str);
                        let has = self
                            .facts
                            .get(item)
                            .is_some_and(|facts| facts.iter().any(|fact| fact.kind == kind));
                        if has && start < kept {
                            continue;
                        }
                        if has && start > kept {
                            if let Some(facts) = self.facts.get_mut(item) {
                                facts.retain(|fact| fact.kind != kind);
                            }
                        }
                        self.started.insert(key, start.to_string());
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
                    let Some(date) = Date::from_wikidata(&v.value, precision) else {
                        continue;
                    };
                    if self.disputed.contains(&(item.to_string(), kind)) {
                        continue;
                    }
                    let kept = self.facts.get(item).and_then(|facts| {
                        facts
                            .iter()
                            .find(|fact| fact.kind == kind)
                            .and_then(|fact| Date::parse(&fact.value))
                    });
                    if let Some(kept) = kept {
                        let facts = self.facts.get_mut(item).expect("kept a date");
                        if kept.year != date.year {
                            facts.retain(|fact| fact.kind != kind);
                            self.disputed.insert((item.to_string(), kind));
                            continue;
                        }
                        // The same year: the more precise date stays.
                        if date.write().len() <= kept.write().len() {
                            continue;
                        }
                        facts.retain(|fact| fact.kind != kind);
                    }
                    date.write()
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

    /// Adds the facts of `other`, read for other kinds.
    fn merge(&mut self, other: RawFacts) {
        for (item, facts) in other.facts {
            self.facts.entry(item).or_default().extend(facts);
        }
        self.counted.extend(other.counted);
        self.disputed.extend(other.disputed);
        self.partial.extend(other.partial);
        self.started.extend(other.started);
    }

    /// Adds the values only for some part of an item to the items that
    /// have no other of their kind.
    fn add_partial(&mut self) {
        for ((item, kind), values) in std::mem::take(&mut self.partial) {
            let facts = self.facts.entry(item).or_default();
            if !facts.iter().any(|fact| fact.kind == kind) {
                facts.extend(values.into_iter().map(|value| Fact { kind, value }));
            }
        }
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
         FILTER(LANG(?label) = \"en\" || LANG(?label) = \"mul\") }}",
        values.join(" ")
    )
}

fn add_labels(labels: &mut HashMap<String, String>, json: &[u8]) -> Result<()> {
    for row in bindings(json)? {
        if let (Some(item), Some(label)) = (row.get("item"), row.get("label")) {
            let text = plumb_core::collapse_whitespace(&label.value);
            let id = entity_id(&item.value).to_string();
            // An English label wins over the label for all languages
            // ("mul"), which Wikidata gives names instead of an English one
            // that would say the same (the euro, many people).
            let english = label.lang.as_deref() != Some("mul");
            if !text.is_empty() && (english || !labels.contains_key(&id)) {
                labels.insert(id, text);
            }
        }
    }
    Ok(())
}

/// Asks Wikidata's query service at `endpoint` for the facts of the items
/// in `items` (those with an article, most read first).
///
/// Each kind is read in pages of all its statements. When Wikidata stops
/// answering a kind's pages (deep pages time out), the first
/// [`FILL_IN_TOP`] items that still lack it are asked about by name, so
/// the most read keep their facts. [`PARALLEL_QUERIES`] kinds, and then
/// batches of labels, are asked for at once.
pub async fn fetch_facts(
    client: &reqwest::Client,
    endpoint: &str,
    pacing: WikidataPacing,
    items: &[String],
) -> Result<FactsByItem> {
    use futures_util::stream::{self, StreamExt, TryStreamExt};
    let wanted: HashSet<String> = items.iter().cloned().collect();
    let wanted = &wanted;
    let mut raw = RawFacts::default();
    let mut kinds = stream::iter(KINDS)
        .map(|&kind| fetch_kind(client, endpoint, pacing, kind, items, wanted))
        .buffered(PARALLEL_QUERIES);
    while let Some(kind) = kinds.try_next().await? {
        raw.merge(kind);
    }
    raw.add_partial();
    let items = raw.named_items();
    info!("naming {} items the facts are about", items.len());
    let batches = items.chunks(LABELS_BATCH).count();
    let mut answers = stream::iter(items.chunks(LABELS_BATCH).enumerate())
        .map(|(n, batch)| async move {
            tokio::time::sleep(pacing.pause).await;
            let answer = sparql_json(client, endpoint, &labels_query(batch), pacing).await;
            (n, batch.len(), answer)
        })
        .buffered(PARALLEL_QUERIES);
    let mut labels = HashMap::new();
    while let Some((n, size, answer)) = answers.next().await {
        match answer {
            Ok(json) => add_labels(&mut labels, &json)?,
            // Facts naming these items are left out.
            Err(err) => warn!("labels of {size} items left out: {err:#}"),
        }
        if n % 50 == 0 {
            info!("labels: {} of {batches} batches", n + 1);
        }
    }
    Ok(raw.named(&labels))
}

/// Queries to Wikidata at once: its query service allows five per client.
pub const PARALLEL_QUERIES: usize = 4;

/// The facts of one kind: its pages, then the most read items that lack it.
async fn fetch_kind(
    client: &reqwest::Client,
    endpoint: &str,
    pacing: WikidataPacing,
    kind: FactKind,
    items: &[String],
    wanted: &HashSet<String>,
) -> Result<RawFacts> {
    let mut raw = RawFacts::default();
    let mut offset = 0;
    // Too big to read whole: the most read items only, by name.
    let mut cut_short = kind.by_name_only();
    while !kind.by_name_only() {
        tokio::time::sleep(pacing.pause).await;
        let json = match sparql_json(client, endpoint, &page_query(kind, offset), pacing).await {
            Ok(json) => json,
            Err(err) => {
                warn!(
                    "{} ({}): kept the {offset} statements read before Wikidata failed: {err:#}",
                    kind.key(),
                    kind.property()
                );
                cut_short = true;
                break;
            }
        };
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
    let top = if cut_short {
        FILL_IN_TOP
    } else {
        FILL_IN_ALWAYS
    };
    fill_in(
        client,
        endpoint,
        pacing,
        &mut raw,
        kind,
        &items[..top.min(items.len())],
        wanted,
    )
    .await?;
    Ok(raw)
}

/// Asks for `kind` of those of `items` that lack it.
async fn fill_in(
    client: &reqwest::Client,
    endpoint: &str,
    pacing: WikidataPacing,
    raw: &mut RawFacts,
    kind: FactKind,
    items: &[String],
    wanted: &HashSet<String>,
) -> Result<()> {
    let lacking: Vec<String> = items
        .iter()
        .filter(|item| {
            !raw.facts
                .get(*item)
                .is_some_and(|facts| facts.iter().any(|fact| fact.kind == kind))
        })
        .cloned()
        .collect();
    info!(
        "{} ({}): asking about {} of the {} most read items that lack it",
        kind.key(),
        kind.property(),
        lacking.len(),
        items.len()
    );
    let mut found = 0;
    for (n, batch) in lacking.chunks(LABELS_BATCH).enumerate() {
        tokio::time::sleep(pacing.pause).await;
        match sparql_json(client, endpoint, &items_query(kind, batch), pacing).await {
            Ok(json) => found += raw.add_page(kind, wanted, &json)?,
            Err(err) => warn!("{}: {} items left out: {err:#}", kind.key(), batch.len()),
        }
        if n % 50 == 0 {
            info!(
                "{}: {} of {} asked, {found} statements",
                kind.key(),
                ((n + 1) * LABELS_BATCH).min(lacking.len()),
                lacking.len()
            );
        }
    }
    Ok(())
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

    fn lang_answer(item: &str, labels: &[(&str, &str)]) -> Vec<u8> {
        let bindings: Vec<serde_json::Value> = labels
            .iter()
            .map(|(label, lang)| {
                serde_json::json!({
                    "item": { "value": item },
                    "label": { "value": label, "xml:lang": lang },
                })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({ "results": { "bindings": bindings } })).unwrap()
    }

    #[test]
    fn queries_ask_for_best_ranked_statements() {
        let q = page_query(FactKind::Elevation, 0);
        assert!(q.contains("p:P2044 ?s"), "{q}");
        assert!(q.contains("wikibase:BestRank"), "{q}");
        assert!(q.contains("psn:P2044/wikibase:quantityAmount ?v"), "{q}");
        let q = page_query(FactKind::Population, 400_000);
        assert!(q.contains("psv:P1082/wikibase:quantityAmount ?v"), "{q}");
        assert!(q.contains("pq:P585 ?t"), "{q}");
        assert!(q.ends_with("LIMIT 50000 OFFSET 400000"), "{q}");
        let q = page_query(FactKind::Born, 0);
        assert!(q.contains("wikibase:timePrecision ?p"), "{q}");
        let q = page_query(FactKind::Currency, 0);
        assert!(q.contains("OPTIONAL { ?s pq:P518 ?part . }"), "{q}");
        assert!(!q.contains("P580"), "{q}");
        assert!(page_query(FactKind::Ceo, 0).contains("OPTIONAL { ?s pq:P580 ?t . }"));
        assert!(q.contains("FILTER NOT EXISTS { ?s pq:P582 [] }"), "{q}");
        assert!(!page_query(FactKind::Founder, 0).contains("FILTER"));
        let items = ["Q30".to_string(), "Q668".to_string()];
        let q = items_query(FactKind::Population, &items);
        assert!(
            q.starts_with(
                "SELECT ?item ?v ?p ?t ?part WHERE { VALUES ?item { wd:Q30 wd:Q668 } ?item p:P1082 ?s ."
            ),
            "{q}"
        );
        assert!(q.contains("pq:P585 ?t"), "{q}");
        assert!(q.ends_with('}') && !q.contains("LIMIT"), "{q}");
        assert_eq!(q.matches('{').count(), q.matches('}').count(), "{q}");
    }

    #[test]
    fn facts_are_read_named_and_written() {
        let wanted: HashSet<String> = ["Q408", "Q513", "Q937", "Q478214", "Q142"]
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
        // France: founded in 481 and in 843, so in neither.
        let france = format!("{E}Q142");
        for v in [
            "0481-01-01T00:00:00Z",
            "0843-08-01T00:00:00Z",
            "0481-01-01T00:00:00Z",
        ] {
            let row = [("item", france.as_str()), ("v", v), ("p", "9")];
            raw.add_page(FactKind::Founded, &wanted, &answer(&[&row]))
                .unwrap();
        }
        assert!(raw.facts.get("Q142").is_none_or(Vec::is_empty));

        // South Africa's capitals are each for a branch of government:
        // kept, as it has no other. France's euro outranks the CFP franc.
        let za = format!("{E}Q258");
        let fr = format!("{E}Q142");
        let pretoria = format!("{E}Q3926");
        let cfp = format!("{E}Q214393");
        let euro = format!("{E}Q4916");
        let branch = format!("{E}Q35798");
        let mut parts = RawFacts::default();
        let wanted_parts: HashSet<String> = ["Q258", "Q142"].map(String::from).into();
        parts
            .add_page(
                FactKind::Capital,
                &wanted_parts,
                &answer(&[&[("item", &za), ("v", &pretoria), ("part", &branch)]]),
            )
            .unwrap();
        parts
            .add_page(
                FactKind::Currency,
                &wanted_parts,
                &answer(&[
                    &[("item", &fr), ("v", &cfp), ("part", &branch)],
                    &[("item", &fr), ("v", &euro)],
                ]),
            )
            .unwrap();
        parts.add_partial();
        assert_eq!(
            parts.facts["Q258"],
            [Fact {
                kind: FactKind::Capital,
                value: "Q3926".into()
            }]
        );
        assert_eq!(
            parts.facts["Q142"],
            [Fact {
                kind: FactKind::Currency,
                value: "Q4916".into()
            }]
        );

        // Intel: the CEO who started last.
        let intel = format!("{E}Q248");
        let mut ceos = RawFacts::default();
        let wanted_intel: HashSet<String> = ["Q248"].map(String::from).into();
        ceos.add_page(
            FactKind::Ceo,
            &wanted_intel,
            &answer(&[
                &[
                    ("item", &intel),
                    ("v", &format!("{E}Q1")),
                    ("t", "2024-12-02T00:00:00Z"),
                ],
                &[
                    ("item", &intel),
                    ("v", &format!("{E}Q2")),
                    ("t", "2025-03-18T00:00:00Z"),
                ],
                &[("item", &intel), ("v", &format!("{E}Q3"))],
            ]),
        )
        .unwrap();
        assert_eq!(
            ceos.facts["Q248"],
            [Fact {
                kind: FactKind::Ceo,
                value: "Q2".into()
            }]
        );
        assert_eq!(raw.named_items(), ["Q3114", "Q317521"]);
        let mut labels = HashMap::new();
        add_labels(
            &mut labels,
            &answer(&[&[("item", &canberra), ("label", "Canberra")]]),
        )
        .unwrap();
        // The label for all languages counts, after an English one.
        let euro = format!("{E}Q4916");
        let mut both = HashMap::new();
        add_labels(
            &mut both,
            &lang_answer(&euro, &[("euro", "mul"), ("Euro", "en")]),
        )
        .unwrap();
        add_labels(&mut both, &lang_answer(&canberra, &[("Canberra", "mul")])).unwrap();
        assert_eq!(both["Q4916"], "Euro");
        assert_eq!(both["Q3114"], "Canberra");
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
