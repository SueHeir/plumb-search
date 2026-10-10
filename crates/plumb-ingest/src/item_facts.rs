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
use serde::{Deserialize, Serialize};
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

/// A fixed item/property pair to retry in background ingestion.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FactRetry {
    pub item: String,
    pub kind: FactKind,
}

/// Completion of one property's scan and bounded targeted repair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyCompletion {
    pub kind: FactKind,
    pub endpoint: String,
    pub targeted_endpoint: String,
    pub stage: String,
    pub scan_complete: bool,
    pub stopped_at: Option<usize>,
    pub resumed_stopped_at: Option<usize>,
    pub targeted_items: usize,
    pub targeted_batches: usize,
    pub failed_items: usize,
    pub items_with_facts: usize,
}

/// Retrieval time is ingestion provenance, not a fact's observation date.
/// Interrupted scans cannot identify every omitted item; `retry` covers
/// failed targeted queries and unresolved labels only.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FactsCompletion {
    pub retrieved_at: u64,
    pub properties: Vec<PropertyCompletion>,
    pub missing_labels: usize,
    pub retry: Vec<FactRetry>,
    /// Successful targeted reads, including properties with no current
    /// statement. Only these may clear an existing fact during a refresh.
    pub checked: Vec<FactRetry>,
}

#[derive(Debug)]
pub struct FetchedFacts {
    pub facts: FactsByItem,
    pub completion: FactsCompletion,
}

fn retrieval_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}

fn endpoint_name(endpoint: &str) -> String {
    let Ok(mut url) = url::Url::parse(endpoint) else {
        return "invalid endpoint".into();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
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

/// The prefixes [`page_query`] uses. Wikidata's query service knows them
/// already; other endpoints, such as QLever's, need them said.
const PREFIXES: &str = "PREFIX wd: <http://www.wikidata.org/entity/> \
     PREFIX p: <http://www.wikidata.org/prop/> \
     PREFIX ps: <http://www.wikidata.org/prop/statement/> \
     PREFIX psv: <http://www.wikidata.org/prop/statement/value/> \
     PREFIX psn: <http://www.wikidata.org/prop/statement/value-normalized/> \
     PREFIX pq: <http://www.wikidata.org/prop/qualifier/> \
     PREFIX wikibase: <http://wikiba.se/ontology#> ";

/// Where [`fetch_facts`] reads on when Wikidata's query service stops
/// answering a kind's deep pages: QLever's copy of Wikidata, which answers
/// them in seconds.
pub const DEEP_SPARQL_URL: &str = "https://qlever.dev/api/wikidata";

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
                // `?dated`: whether any of the item's statements, ended
                // ones too, says when it started.
                format!(
                    " OPTIONAL {{ ?s pq:P580 ?t . }} \
                     BIND(EXISTS {{ ?item p:{p}/pq:P580 [] }} AS ?dated)"
                )
            } else {
                String::new()
            }
        ),
        // A count has no unit to normalize.
        ValueType::Quantity if kind == FactKind::Population => {
            format!("psv:{p}/wikibase:quantityAmount ?v . OPTIONAL {{ ?s pq:P585 ?t . }}")
        }
        ValueType::Quantity if kind.unitless() => {
            format!("psv:{p}/wikibase:quantityAmount ?v .")
        }
        // Metres, square metres and seconds, whatever unit the statement
        // is in.
        ValueType::Quantity => format!("psn:{p}/wikibase:quantityAmount ?v ."),
        // A WKT point, `Point(149.1269 -35.2931)`, with the globe first
        // when it is not Earth.
        ValueType::Coordinates => format!("ps:{p} ?v ."),
        ValueType::Time => {
            format!("psv:{p} ?tv . ?tv wikibase:timeValue ?v ; wikibase:timePrecision ?p .")
        }
    };
    format!(
        "{PREFIXES}SELECT ?item ?v ?p ?t ?part ?dated WHERE {{ ?item p:{p} ?s . ?s a wikibase:BestRank ; {value} }} \
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
                        // No start counts as the earliest. Where the item
                        // dates its CEOs, one with no dates at all is left
                        // out: Intel's interim co-CEO, recorded with no
                        // start or end, outlived the CEOs who ended.
                        let start = row.get("t").map_or("", |t| t.value.as_str());
                        let dated = row
                            .get("dated")
                            .is_some_and(|d| d.value == "true" || d.value == "1");
                        if start.is_empty() && dated {
                            continue;
                        }
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
                ValueType::Coordinates => {
                    let Some(place) = earth_point(&v.value) else {
                        continue;
                    };
                    place
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

    /// The (item, company) pairs where the item's founder or owner is an
    /// item, which may be the company it is named after.
    fn founded_by_items(&self) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = self
            .facts
            .iter()
            .flat_map(|(item, facts)| {
                facts
                    .iter()
                    .filter(|fact| matches!(fact.kind, FactKind::Founder | FactKind::Owner))
                    .filter(|fact| fact.value != *item)
                    .map(move |fact| (item.clone(), fact.value.clone()))
            })
            .collect();
        pairs.sort_unstable();
        pairs.dedup();
        pairs
    }

    /// Gives each item of `namesakes` (item, company) its company's
    /// [`NAMESAKE_KINDS`] from `companies` in place of the company itself:
    /// Netflix, the service, was founded by Netflix, Inc., which was
    /// founded by Reed Hastings and Marc Randolph.
    fn take_from_namesakes(&mut self, namesakes: &[(String, String)], companies: &RawFacts) {
        for (item, company) in namesakes {
            let Some(facts) = self.facts.get_mut(item) else {
                continue;
            };
            facts.retain(|fact| {
                !(matches!(fact.kind, FactKind::Founder | FactKind::Owner)
                    && fact.value == *company)
            });
            let Some(theirs) = companies.facts.get(company) else {
                continue;
            };
            for &kind in NAMESAKE_KINDS {
                if !facts.iter().any(|fact| fact.kind == kind) {
                    facts.extend(theirs.iter().filter(|fact| fact.kind == kind).cloned());
                }
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

/// A place on Earth as kept (`-35.2931,149.1269`) of a WKT point
/// (`Point(149.1269 -35.2931)`, longitude first); `None` for a point on
/// another globe (`<http://www.wikidata.org/entity/Q111> Point(...)`).
fn earth_point(wkt: &str) -> Option<String> {
    let inside = wkt.trim().strip_prefix("Point(")?.strip_suffix(')')?;
    let mut numbers = inside.split_whitespace().map(str::parse::<f64>);
    let lon = numbers.next()?.ok()?;
    let lat = numbers.next()?.ok()?;
    if numbers.next().is_some() {
        return None;
    }
    let degrees = |x: f64| {
        let text = format!("{x:.6}");
        let text = text.trim_end_matches('0').trim_end_matches('.');
        if text == "-0" {
            "0".to_string()
        } else {
            text.to_string()
        }
    };
    let value = format!("{},{}", degrees(lat), degrees(lon));
    plumb_core::facts::coordinates(&value).map(|_| value)
}

/// Kinds an item takes from the company it is named after
/// ([`RawFacts::take_from_namesakes`]).
const NAMESAKE_KINDS: &[FactKind] = &[
    FactKind::Founder,
    FactKind::Ceo,
    FactKind::Headquarters,
    FactKind::Founded,
];

/// `name` as a company is named, without its legal form: "netflix" of
/// "Netflix, Inc.".
fn company_name(name: &str) -> String {
    let mut name = name.trim().to_lowercase();
    loop {
        let before = name.len();
        for form in [
            "inc.",
            "inc",
            "llc",
            "l.l.c.",
            "corporation",
            "corp.",
            "corp",
            "company",
            "co.",
            "ltd.",
            "ltd",
            "limited",
            "plc",
            "ag",
            "gmbh",
            "s.a.",
            "sa",
            "group",
            "holdings",
        ] {
            if let Some(rest) = name.strip_suffix(form) {
                if rest.ends_with([' ', ',']) {
                    name = rest.trim_end_matches([' ', ',']).to_string();
                }
            }
        }
        if name.len() == before {
            return name;
        }
    }
}

/// The pairs of `pairs` (item, company) where the company is the item's
/// namesake by `labels`: "Netflix" and "Netflix, Inc.".
fn namesakes(
    pairs: &[(String, String)],
    labels: &HashMap<String, String>,
) -> Vec<(String, String)> {
    pairs
        .iter()
        .filter(|(item, company)| {
            let (Some(item), Some(company)) = (labels.get(item), labels.get(company)) else {
                return false;
            };
            let name = company_name(item);
            !name.is_empty() && company_name(company) == name
        })
        .cloned()
        .collect()
}

/// Asks for the labels of `items`, [`PARALLEL_QUERIES`] batches at once;
/// a batch Wikidata fails to answer is left out.
async fn fetch_labels(
    client: &reqwest::Client,
    endpoint: &str,
    pacing: WikidataPacing,
    items: &[String],
) -> Result<HashMap<String, String>> {
    use futures_util::stream::{self, StreamExt};
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
    Ok(labels)
}

/// Gives the items in `raw` of `pairs` ([`RawFacts::founded_by_items`])
/// whose founder or owner is the company they are named after, by
/// `labels`, that company's founders, CEO, headquarters and founding date
/// ([`RawFacts::take_from_namesakes`]).
async fn follow_namesakes(
    client: &reqwest::Client,
    endpoint: &str,
    pacing: WikidataPacing,
    raw: &mut RawFacts,
    pairs: &[(String, String)],
    labels: &HashMap<String, String>,
) -> Result<Vec<FactRetry>> {
    let namesakes = namesakes(pairs, labels);
    let companies: Vec<String> = namesakes
        .iter()
        .map(|(_, company)| company.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    info!(
        "{} items named after the company that founded or owns them",
        namesakes.len()
    );
    let wanted: HashSet<String> = companies.iter().cloned().collect();
    let mut theirs = RawFacts::default();
    let mut retry = Vec::new();
    for &kind in NAMESAKE_KINDS {
        for batch in companies.chunks(LABELS_BATCH) {
            tokio::time::sleep(pacing.pause).await;
            match sparql_json(client, endpoint, &items_query(kind, batch), pacing).await {
                Ok(json) => {
                    theirs.add_page(kind, &wanted, &json)?;
                }
                Err(err) => {
                    warn!(
                        "{} of {} companies left out: {err:#}",
                        kind.key(),
                        batch.len()
                    );
                    retry.extend(
                        namesakes
                            .iter()
                            .filter(|(_, company)| batch.contains(company))
                            .map(|(item, _)| FactRetry {
                                item: item.clone(),
                                kind,
                            }),
                    );
                }
            }
        }
    }
    raw.take_from_namesakes(&namesakes, &theirs);
    Ok(retry)
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
/// Each kind is read in pages of all its statements, [`PARALLEL_QUERIES`]
/// kinds at once. Deep pages time out, so a kind Wikidata stopped
/// answering is read on from where it stopped once the others are done,
/// one kind at a time, from `deep_endpoint` if given (Wikidata's query
/// service times out on them even alone), which reads the kind from its
/// start, as it lists the statements in another order. When it still stops, the first
/// [`FILL_IN_TOP`] items that lack it are asked about by name, so the
/// most read keep their facts. Then batches of labels are asked for,
/// [`PARALLEL_QUERIES`] at once.
pub async fn fetch_facts(
    client: &reqwest::Client,
    endpoint: &str,
    deep_endpoint: Option<&str>,
    pacing: WikidataPacing,
    items: &[String],
) -> Result<FactsByItem> {
    Ok(
        fetch_facts_reported(client, endpoint, deep_endpoint, pacing, items)
            .await?
            .facts,
    )
}

/// The existing scan/fallback workflow with completion diagnostics.
pub async fn fetch_facts_reported(
    client: &reqwest::Client,
    endpoint: &str,
    deep_endpoint: Option<&str>,
    pacing: WikidataPacing,
    items: &[String],
) -> Result<FetchedFacts> {
    use futures_util::stream::{self, StreamExt, TryStreamExt};
    let wanted: HashSet<String> = items.iter().cloned().collect();
    let wanted = &wanted;
    let read: Vec<(FactKind, RawFacts, Option<usize>)> = stream::iter(KINDS)
        .map(|&kind| async move {
            let mut raw = RawFacts::default();
            let stopped = read_pages(client, endpoint, pacing, &mut raw, kind, 0, wanted).await?;
            Ok::<_, anyhow::Error>((kind, raw, stopped))
        })
        .buffered(PARALLEL_QUERIES)
        .try_collect()
        .await?;
    let mut kinds = Vec::with_capacity(read.len());
    for (kind, mut raw, stopped) in read {
        let mut cut_short = kind.by_name_only();
        let mut resumed_stopped_at = None;
        if let Some(stopped_at) = stopped {
            let from = deep_endpoint.unwrap_or(endpoint);
            // Pages are read by offset without an order, and another
            // endpoint lists the statements in another order: reading on
            // from where Wikidata stopped would skip those the deep
            // endpoint lists first (facts7 lost a third of the areas,
            // elevations and heights so). It reads them all again instead;
            // statements already read are kept once.
            let offset = if from == endpoint { stopped_at } else { 0 };
            info!(
                "{} ({}): stopped at {stopped_at}, reading on from {offset} alone, from {from}",
                kind.key(),
                kind.property()
            );
            let again = read_pages(client, from, pacing, &mut raw, kind, offset, wanted).await?;
            cut_short |= again.is_some();
            resumed_stopped_at = again;
        }
        let report = PropertyCompletion {
            kind,
            endpoint: endpoint_name(if stopped.is_some() {
                deep_endpoint.unwrap_or(endpoint)
            } else {
                endpoint
            }),
            targeted_endpoint: endpoint_name(endpoint),
            stage: "scan_and_targeted_repair".into(),
            scan_complete: !cut_short,
            stopped_at: stopped,
            resumed_stopped_at,
            targeted_items: 0,
            targeted_batches: 0,
            failed_items: 0,
            items_with_facts: 0,
        };
        kinds.push((kind, raw, cut_short, report));
    }
    let mut filled = stream::iter(kinds)
        .map(|(kind, mut raw, cut_short, mut report)| async move {
            // Too big or too slow to read whole: the most read items by name.
            let top = if cut_short {
                FILL_IN_TOP
            } else {
                FILL_IN_ALWAYS
            };
            let most_read = &items[..top.min(items.len())];
            let targeted =
                fill_in(client, endpoint, pacing, &mut raw, kind, most_read, wanted).await?;
            report.targeted_items = targeted.checked.len() + targeted.retry.len();
            report.targeted_batches = report.targeted_items.div_ceil(LABELS_BATCH);
            report.failed_items = targeted.retry.len();
            report.items_with_facts = raw.facts.len();
            Ok::<_, anyhow::Error>((raw, report, targeted))
        })
        .buffered(PARALLEL_QUERIES);
    let mut raw = RawFacts::default();
    let mut completion = FactsCompletion {
        retrieved_at: retrieval_time(),
        ..Default::default()
    };
    while let Some((kind, report, targeted)) = filled.try_next().await? {
        raw.merge(kind);
        completion.properties.push(report);
        completion.checked.extend(targeted.checked);
        completion.retry.extend(targeted.retry);
    }
    name_facts(client, endpoint, pacing, raw, completion).await
}

/// The existing label/namesake pipeline shared by scans and repairs.
async fn name_facts(
    client: &reqwest::Client,
    endpoint: &str,
    pacing: WikidataPacing,
    mut raw: RawFacts,
    mut completion: FactsCompletion,
) -> Result<FetchedFacts> {
    raw.add_partial();
    // The items founded or owned by an item are named too, to tell which
    // are named after it.
    let pairs = raw.founded_by_items();
    let mut items = raw.named_items();
    items.extend(pairs.iter().map(|(item, _)| item.clone()));
    items.sort_unstable();
    items.dedup();
    info!("naming {} items the facts are about", items.len());
    let mut labels = fetch_labels(client, endpoint, pacing, &items).await?;
    let retry = follow_namesakes(client, endpoint, pacing, &mut raw, &pairs, &labels).await?;
    completion.checked.retain(|pair| !retry.contains(pair));
    completion.retry.extend(retry);
    let unnamed: Vec<String> = raw
        .named_items()
        .into_iter()
        .filter(|item| !labels.contains_key(item))
        .collect();
    labels.extend(fetch_labels(client, endpoint, pacing, &unnamed).await?);
    Ok(finish_named(raw, &labels, completion))
}

/// Queries to Wikidata at once: its query service allows five per client.
pub const PARALLEL_QUERIES: usize = 4;

/// Reads the pages of `kind` from `offset` into `raw`. Returns where it
/// stopped if Wikidata failed before the last page.
async fn read_pages(
    client: &reqwest::Client,
    endpoint: &str,
    pacing: WikidataPacing,
    raw: &mut RawFacts,
    kind: FactKind,
    mut offset: usize,
    wanted: &HashSet<String>,
) -> Result<Option<usize>> {
    if kind.by_name_only() {
        return Ok(None);
    }
    loop {
        tokio::time::sleep(pacing.pause).await;
        let json = match sparql_json(client, endpoint, &page_query(kind, offset), pacing).await {
            Ok(json) => json,
            Err(err) => {
                warn!(
                    "{} ({}): Wikidata failed at statement {offset}: {err:#}",
                    kind.key(),
                    kind.property()
                );
                return Ok(Some(offset));
            }
        };
        let rows = raw.add_page(kind, wanted, &json)?;
        info!(
            "{} ({}): {rows} statements from {offset}",
            kind.key(),
            kind.property()
        );
        if rows < FACTS_PAGE {
            return Ok(None);
        }
        offset += FACTS_PAGE;
    }
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
) -> Result<FactsCompletion> {
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
    let mut completion = FactsCompletion::default();
    for (n, batch) in lacking.chunks(LABELS_BATCH).enumerate() {
        tokio::time::sleep(pacing.pause).await;
        match sparql_json(client, endpoint, &items_query(kind, batch), pacing).await {
            Ok(json) => match raw.add_page(kind, wanted, &json) {
                Ok(rows) => {
                    found += rows;
                    completion
                        .checked
                        .extend(batch.iter().map(|item| FactRetry {
                            item: item.clone(),
                            kind,
                        }));
                }
                Err(err) => {
                    warn!(
                        "{}: malformed targeted answer for {} items: {err:#}",
                        kind.key(),
                        batch.len()
                    );
                    completion.retry.extend(batch.iter().map(|item| FactRetry {
                        item: item.clone(),
                        kind,
                    }));
                }
            },
            Err(err) => {
                warn!("{}: {} items left out: {err:#}", kind.key(), batch.len());
                completion.retry.extend(batch.iter().map(|item| FactRetry {
                    item: item.clone(),
                    kind,
                }));
            }
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
    Ok(completion)
}

fn finish_named(
    raw: RawFacts,
    labels: &HashMap<String, String>,
    mut completion: FactsCompletion,
) -> FetchedFacts {
    completion.retrieved_at = retrieval_time();
    let missing: HashSet<_> = raw
        .named_items()
        .into_iter()
        .filter(|item| !labels.contains_key(item))
        .collect();
    completion.missing_labels = missing.len();
    let failed: HashSet<_> = raw
        .facts
        .iter()
        .flat_map(|(item, facts)| {
            facts
                .iter()
                .filter(|fact| {
                    fact.kind.value_type() == ValueType::Item && missing.contains(&fact.value)
                })
                .map(|fact| FactRetry {
                    item: item.clone(),
                    kind: fact.kind,
                })
        })
        .collect();
    completion.checked.retain(|pair| !failed.contains(pair));
    completion.retry.extend(failed);
    let mut seen = HashSet::new();
    completion.retry.retain(|pair| seen.insert(pair.clone()));
    let mut facts = raw.named(labels);
    for pair in &completion.retry {
        if let Some(values) = facts.get_mut(&pair.item) {
            values.retain(|fact| fact.kind != pair.kind);
        }
    }
    for property in &mut completion.properties {
        property.items_with_facts = facts
            .values()
            .filter(|facts| facts.iter().any(|fact| fact.kind == property.kind))
            .count();
        property.failed_items = completion
            .retry
            .iter()
            .filter(|pair| pair.kind == property.kind)
            .count();
    }
    FetchedFacts { facts, completion }
}

/// Bounded, resumable repair of explicit item/property pairs. No worldwide
/// pagination, and no user search triggers this upstream request.
pub async fn fetch_targeted_facts(
    client: &reqwest::Client,
    endpoint: &str,
    pacing: WikidataPacing,
    pairs: &[FactRetry],
) -> Result<FetchedFacts> {
    anyhow::ensure!(
        pairs.len() <= FILL_IN_TOP,
        "too many targeted fact pairs (maximum {FILL_IN_TOP})"
    );
    anyhow::ensure!(
        pairs.iter().all(|pair| is_item_id(&pair.item)),
        "targeted facts require Wikidata item IDs"
    );
    let mut raw = RawFacts::default();
    let mut completion = FactsCompletion {
        retrieved_at: retrieval_time(),
        ..Default::default()
    };
    for &kind in KINDS {
        let items: Vec<_> = pairs
            .iter()
            .filter(|pair| pair.kind == kind)
            .map(|pair| pair.item.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if items.is_empty() {
            continue;
        }
        let wanted: HashSet<_> = items.iter().cloned().collect();
        let read = fill_in(client, endpoint, pacing, &mut raw, kind, &items, &wanted).await?;
        completion.properties.push(PropertyCompletion {
            kind,
            endpoint: endpoint_name(endpoint),
            targeted_endpoint: endpoint_name(endpoint),
            stage: "targeted".into(),
            scan_complete: false,
            stopped_at: None,
            resumed_stopped_at: None,
            targeted_items: items.len(),
            targeted_batches: items.len().div_ceil(LABELS_BATCH),
            failed_items: read.retry.len(),
            items_with_facts: 0,
        });
        completion.checked.extend(read.checked);
        completion.retry.extend(read.retry);
    }
    let mut fetched = name_facts(client, endpoint, pacing, raw, completion).await?;
    let requested: HashSet<_> = pairs.iter().cloned().collect();
    for (item, facts) in &mut fetched.facts {
        facts.retain(|fact| {
            requested.contains(&FactRetry {
                item: item.clone(),
                kind: fact.kind,
            })
        });
    }
    fetched
        .completion
        .retry
        .retain(|pair| requested.contains(pair));
    Ok(fetched)
}

/// What [`add_facts_to_file`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AddedFacts {
    pub articles: u64,
    pub with_facts: u64,
    pub facts: u64,
    /// Older or undated incoming population values did not replace a
    /// count with a later source observation year.
    pub kept_newer_populations: u64,
}

/// Replaces only the imported property kinds; keeps unrelated enrichment.
/// For authoritative empty targeted reads use [`apply_fetched_facts`].
pub fn add_facts_to_file(path: &Path, facts: &FactsByItem) -> Result<AddedFacts> {
    write_facts_to_file(path, facts, &[])
}

/// Successful targeted reads may clear a now-ended statement. Failed
/// reads and unresolved labels preserve the previous value.
pub fn apply_fetched_facts(path: &Path, fetched: &FetchedFacts) -> Result<AddedFacts> {
    write_facts_to_file(path, &fetched.facts, &fetched.completion.checked)
}

fn write_facts_to_file(
    path: &Path,
    facts: &FactsByItem,
    checked: &[FactRetry],
) -> Result<AddedFacts> {
    let mut checked_by_item: HashMap<&str, HashSet<FactKind>> = HashMap::new();
    for pair in checked {
        checked_by_item
            .entry(&pair.item)
            .or_default()
            .insert(pair.kind);
    }
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
        if let Some(item) = article.item.as_deref() {
            let incoming = facts.get(item).map(Vec::as_slice).unwrap_or(&[]);
            let checked = checked_by_item.get(item);
            let keep_population = article
                .facts
                .iter()
                .find(|fact| fact.kind == FactKind::Population)
                .and_then(Fact::observation_year)
                .zip(
                    incoming
                        .iter()
                        .find(|fact| fact.kind == FactKind::Population),
                )
                .is_some_and(|(kept, new)| kept > new.observation_year().unwrap_or(i32::MIN));
            added.kept_newer_populations += u64::from(keep_population);
            article.facts.retain(|fact| {
                keep_population && fact.kind == FactKind::Population
                    || !incoming.iter().any(|new| new.kind == fact.kind)
                        && !checked.is_some_and(|kinds| kinds.contains(&fact.kind))
            });
            article.facts.extend(
                incoming
                    .iter()
                    .filter(|fact| !keep_population || fact.kind != FactKind::Population)
                    .cloned(),
            );
        }
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

    fn fact(kind: FactKind, value: &str) -> Fact {
        Fact {
            kind,
            value: value.into(),
        }
    }

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
    fn partial_refresh_preserves_other_facts_profiles_and_leads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wikipedia-en.tsv.gz");
        let before = Article {
            title: "Japan".into(),
            item: Some("Q17".into()),
            description: Some("country".into()),
            lead: Some("Japan is a country in East Asia.".into()),
            profiles: vec![plumb_core::profiles::Profile {
                service: "youtube-handle".into(),
                id: "JapanGov".into(),
            }],
            facts: vec![
                fact(FactKind::Capital, "Tokyo"),
                fact(FactKind::Population, "125000000;2020"),
                fact(FactKind::Ceo, "Former CEO"),
            ],
            ..Default::default()
        };
        crate::articles::write_articles_file(&path, std::slice::from_ref(&before)).unwrap();
        let mut fetched = FetchedFacts {
            facts: HashMap::from([(
                "Q17".into(),
                vec![fact(FactKind::Population, "123802000;2024")],
            )]),
            completion: FactsCompletion {
                checked: vec![FactRetry {
                    item: "Q17".into(),
                    kind: FactKind::Population,
                }],
                retry: vec![FactRetry {
                    item: "Q17".into(),
                    kind: FactKind::Ceo,
                }],
                ..Default::default()
            },
        };
        apply_fetched_facts(&path, &fetched).unwrap();
        let after = read_articles(open_maybe_gz(&path).unwrap(), 10)
            .unwrap()
            .remove(0);
        assert_eq!(after.lead, before.lead);
        assert_eq!(after.profiles, before.profiles);
        assert!(after.facts.contains(&fact(FactKind::Capital, "Tokyo")));
        assert!(after.facts.contains(&fact(FactKind::Ceo, "Former CEO")));
        assert!(after
            .facts
            .contains(&fact(FactKind::Population, "123802000;2024")));
        fetched.facts.clear();
        fetched.completion.retry.clear();
        fetched.completion.checked = vec![FactRetry {
            item: "Q17".into(),
            kind: FactKind::Ceo,
        }];
        apply_fetched_facts(&path, &fetched).unwrap();
        let after = read_articles(open_maybe_gz(&path).unwrap(), 10)
            .unwrap()
            .remove(0);
        assert!(!after.facts.iter().any(|fact| fact.kind == FactKind::Ceo));
        assert_eq!(after.facts.len(), 2);
    }

    #[test]
    fn older_population_observations_cannot_regress_a_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wikipedia-en.tsv.gz");
        let before = Article {
            title: "Example country".into(),
            item: Some("Q17".into()),
            facts: vec![fact(FactKind::Population, "100;2025")],
            ..Default::default()
        };
        for (incoming, kept) in [("99;2024", true), ("99", true), ("101;2026", false)] {
            crate::articles::write_articles_file(&path, std::slice::from_ref(&before)).unwrap();
            let fetched = FetchedFacts {
                facts: HashMap::from([("Q17".into(), vec![fact(FactKind::Population, incoming)])]),
                completion: FactsCompletion {
                    checked: vec![FactRetry {
                        item: "Q17".into(),
                        kind: FactKind::Population,
                    }],
                    ..Default::default()
                },
            };
            let added = apply_fetched_facts(&path, &fetched).unwrap();
            assert_eq!(added.kept_newer_populations, u64::from(kept));
            let after = read_articles(open_maybe_gz(&path).unwrap(), 1)
                .unwrap()
                .remove(0);
            assert_eq!(
                after.facts,
                vec![fact(
                    FactKind::Population,
                    if kept { "100;2025" } else { incoming }
                )]
            );
        }
    }

    #[test]
    fn unresolved_labels_keep_the_entire_property_for_retry() {
        let mut raw = RawFacts::default();
        raw.facts.insert(
            "Q312".into(),
            vec![fact(FactKind::Founder, "Q1"), fact(FactKind::Founder, "Q2")],
        );
        let pair = FactRetry {
            item: "Q312".into(),
            kind: FactKind::Founder,
        };
        let completion = FactsCompletion {
            checked: vec![pair.clone()],
            ..Default::default()
        };
        let fetched = finish_named(
            raw,
            &HashMap::from([("Q1".into(), "Known founder".into())]),
            completion,
        );
        assert!(fetched.facts["Q312"].is_empty());
        assert_eq!(fetched.completion.missing_labels, 1);
        assert_eq!(fetched.completion.retry, vec![pair]);
        assert!(fetched.completion.checked.is_empty());
        assert_eq!(
            endpoint_name("https://name:password@example.test/sparql?key=secret#x"),
            "https://example.test/sparql"
        );
    }

    #[tokio::test]
    async fn targeted_completion_can_retry_failed_pairs_without_global_scans() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/sparql", listener.local_addr().unwrap());
        let queries = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = queries.clone();
        let server = tokio::spawn(async move {
            let mut capital_failed = false;
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                let body_start = loop {
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buf[..n]);
                };
                let head = String::from_utf8_lossy(&request[..body_start]).to_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                while request.len() < body_start + length {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buf[..n]);
                }
                let query = url::form_urlencoded::parse(&request[body_start..])
                    .find(|(name, _)| name == "query")
                    .unwrap()
                    .1
                    .into_owned();
                log.lock().unwrap().push(query.clone());
                let (status, body) = if query.contains("p:P36") && !capital_failed {
                    capital_failed = true;
                    ("400 Bad Request", Vec::new())
                } else if query.contains("p:P1082") {
                    (
                        "200 OK",
                        answer(&[&[
                            ("item", "Q17"),
                            ("v", "123802000"),
                            ("t", "+2024-01-01T00:00:00Z"),
                        ]]),
                    )
                } else if query.contains("p:P36") {
                    ("200 OK", answer(&[&[("item", "Q17"), ("v", "Q1490")]]))
                } else if query.contains("rdfs:label") {
                    (
                        "200 OK",
                        answer(&[&[("item", "Q1490"), ("label", "Tokyo")]]),
                    )
                } else {
                    ("200 OK", answer(&[]))
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });
        let pairs = vec![
            FactRetry {
                item: "Q17".into(),
                kind: FactKind::Population,
            },
            FactRetry {
                item: "Q17".into(),
                kind: FactKind::Capital,
            },
            FactRetry {
                item: "Q17".into(),
                kind: FactKind::Ceo,
            },
        ];
        let pacing = WikidataPacing {
            pause: std::time::Duration::ZERO,
            retry_wait: std::time::Duration::ZERO,
        };
        let client = reqwest::Client::new();
        let first = fetch_targeted_facts(&client, &endpoint, pacing, &pairs)
            .await
            .unwrap();
        assert_eq!(
            first.facts["Q17"],
            vec![fact(FactKind::Population, "123802000;2024")]
        );
        assert_eq!(first.completion.retry, vec![pairs[1].clone()]);
        assert!(first.completion.checked.contains(&pairs[2])); // no current CEO
        let saved = serde_json::to_vec(&first.completion).unwrap();
        let resume: FactsCompletion = serde_json::from_slice(&saved).unwrap();
        let second = fetch_targeted_facts(&client, &endpoint, pacing, &resume.retry)
            .await
            .unwrap();
        assert_eq!(second.facts["Q17"], vec![fact(FactKind::Capital, "Tokyo")]);
        assert!(second.completion.retry.is_empty());
        let queries = queries.lock().unwrap();
        assert!(queries
            .iter()
            .all(|query| query.contains("VALUES ?item") && !query.contains("OFFSET")));
        assert!(queries
            .iter()
            .find(|query| query.contains("p:P169"))
            .unwrap()
            .contains("FILTER NOT EXISTS { ?s pq:P582 [] }"));
        server.abort();
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
        let q = page_query(FactKind::Ceo, 0);
        assert!(q.contains("OPTIONAL { ?s pq:P580 ?t . }"), "{q}");
        assert!(
            q.contains("BIND(EXISTS { ?item p:P169/pq:P580 [] } AS ?dated)"),
            "{q}"
        );
        assert!(q.contains("FILTER NOT EXISTS { ?s pq:P582 [] }"), "{q}");
        assert!(!page_query(FactKind::Founder, 0).contains("FILTER"));
        let q = page_query(FactKind::AtomicNumber, 0);
        assert!(q.contains("psv:P1086/wikibase:quantityAmount ?v"), "{q}");
        assert!(!q.contains("P585"), "{q}");
        let q = page_query(FactKind::OrbitalPeriod, 0);
        assert!(q.contains("psn:P2146/wikibase:quantityAmount ?v"), "{q}");
        assert!(page_query(FactKind::Coordinates, 0).contains("ps:P625 ?v ."));
        let items = ["Q30".to_string(), "Q668".to_string()];
        let q = items_query(FactKind::Population, &items);
        assert!(
            q.strip_prefix(PREFIXES).is_some_and(|q| q.starts_with(
                "SELECT ?item ?v ?p ?t ?part ?dated WHERE { VALUES ?item { wd:Q30 wd:Q668 } ?item p:P1082 ?s ."
            )),
            "{q}"
        );
        // Endpoints other than Wikidata's own need every prefix declared.
        for kind in KINDS {
            let q = page_query(*kind, 0);
            for used in ["wd:", "p:", "ps:", "psv:", "psn:", "pq:", "wikibase:"] {
                if q.contains(&format!(" {used}")) || q.contains(&format!("/{used}")) {
                    assert!(q.contains(&format!("PREFIX {used} ")), "{used} in {q}");
                }
            }
        }
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
                &[
                    ("item", &intel),
                    ("v", &format!("{E}Q3")),
                    ("dated", "true"),
                ],
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
        // Intel as Wikidata has it now: the CEOs who ended are filtered
        // out by the query, and the undated interim one is not kept.
        let mut undated = RawFacts::default();
        undated
            .add_page(
                FactKind::Ceo,
                &wanted_intel,
                &answer(&[&[
                    ("item", &intel),
                    ("v", &format!("{E}Q131981722")),
                    ("dated", "true"),
                ]]),
            )
            .unwrap();
        assert!(!undated.facts.contains_key("Q248"));
        // Where no CEO is dated, an undated one is the CEO.
        undated
            .add_page(
                FactKind::Ceo,
                &wanted_intel,
                &answer(&[&[
                    ("item", &intel),
                    ("v", &format!("{E}Q3")),
                    ("dated", "false"),
                ]]),
            )
            .unwrap();
        assert_eq!(undated.facts["Q248"].len(), 1);
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
    fn points_on_earth_are_kept() {
        assert_eq!(
            earth_point("Point(149.126944 -35.293056)").unwrap(),
            "-35.293056,149.126944"
        );
        assert_eq!(
            earth_point("Point(2.2945 48.8584)").unwrap(),
            "48.8584,2.2945"
        );
        assert_eq!(
            earth_point("<http://www.wikidata.org/entity/Q111> Point(137.4 -4.6)"),
            None
        );
        assert_eq!(earth_point("Point(200 10)"), None);
        let wanted: HashSet<String> = ["Q90"].map(String::from).into();
        let mut raw = RawFacts::default();
        let paris = format!("{E}Q90");
        raw.add_page(
            FactKind::Coordinates,
            &wanted,
            &answer(&[&[("item", &paris), ("v", "Point(2.351388888 48.856944444)")]]),
        )
        .unwrap();
        assert_eq!(
            raw.facts["Q90"],
            [Fact {
                kind: FactKind::Coordinates,
                value: "48.856944,2.351389".into()
            }]
        );
    }

    #[test]
    fn items_take_facts_from_their_namesake_company() {
        assert_eq!(company_name("Netflix, Inc."), "netflix");
        assert_eq!(company_name("Samsung Group"), "samsung");
        assert_eq!(company_name("Inc."), "inc.");
        let fact = |kind, value: &str| Fact {
            kind,
            value: value.into(),
        };
        let mut raw = RawFacts::default();
        // Netflix, the service, founded by Netflix, Inc.; YouTube Kids by
        // YouTube, another thing.
        raw.facts.insert(
            "Q907311".into(),
            vec![
                fact(FactKind::Founder, "Q116452644"),
                fact(FactKind::Headquarters, "Q747509"),
            ],
        );
        raw.facts
            .insert("Q19599566".into(), vec![fact(FactKind::Founder, "Q866")]);
        let pairs = raw.founded_by_items();
        assert_eq!(pairs.len(), 2);
        let labels: HashMap<String, String> = [
            ("Q907311", "Netflix"),
            ("Q116452644", "Netflix, Inc."),
            ("Q19599566", "YouTube Kids"),
            ("Q866", "YouTube"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .into();
        let found = namesakes(&pairs, &labels);
        assert_eq!(found, [("Q907311".to_string(), "Q116452644".to_string())]);
        let mut companies = RawFacts::default();
        companies.facts.insert(
            "Q116452644".into(),
            vec![
                fact(FactKind::Founder, "Q18341330"),
                fact(FactKind::Founder, "Q7306657"),
                fact(FactKind::Ceo, "Q19661212"),
                fact(FactKind::Headquarters, "Q1"),
                fact(FactKind::Founded, "1997-08-29"),
            ],
        );
        raw.take_from_namesakes(&found, &companies);
        assert_eq!(
            raw.facts["Q907311"],
            [
                fact(FactKind::Headquarters, "Q747509"),
                fact(FactKind::Founder, "Q18341330"),
                fact(FactKind::Founder, "Q7306657"),
                fact(FactKind::Ceo, "Q19661212"),
                fact(FactKind::Founded, "1997-08-29"),
            ]
        );
        assert_eq!(raw.facts["Q19599566"], [fact(FactKind::Founder, "Q866")]);
    }

    #[test]
    fn statements_read_twice_are_kept_once() {
        // A kind the deep endpoint reads again from its start.
        let wanted: HashSet<String> = ["Q408", "Q513", "Q937", "Q248"].map(String::from).into();
        let au = format!("{E}Q408");
        let everest = format!("{E}Q513");
        let einstein = format!("{E}Q937");
        let intel = format!("{E}Q248");
        let pages: Vec<(FactKind, Vec<u8>)> = vec![
            (
                FactKind::Population,
                answer(&[&[
                    ("item", &au),
                    ("v", "+27204809"),
                    ("t", "2024-01-01T00:00:00Z"),
                ]]),
            ),
            (
                FactKind::Elevation,
                answer(&[&[("item", &everest), ("v", "8848.86")]]),
            ),
            (
                FactKind::Born,
                answer(&[&[
                    ("item", &einstein),
                    ("v", "1879-03-14T00:00:00Z"),
                    ("p", "11"),
                ]]),
            ),
            (
                FactKind::Founder,
                answer(&[
                    &[("item", &intel), ("v", &format!("{E}Q241735"))],
                    &[("item", &intel), ("v", &format!("{E}Q243969"))],
                ]),
            ),
            (
                FactKind::Ceo,
                answer(&[&[
                    ("item", &intel),
                    ("v", &format!("{E}Q2")),
                    ("t", "2025-03-18T00:00:00Z"),
                    ("dated", "true"),
                ]]),
            ),
            (
                FactKind::Coordinates,
                answer(&[&[("item", &everest), ("v", "Point(86.925 27.988)")]]),
            ),
        ];
        let read = |times: usize| {
            let mut raw = RawFacts::default();
            for _ in 0..times {
                for (kind, page) in &pages {
                    raw.add_page(*kind, &wanted, page).unwrap();
                }
            }
            let mut facts: Vec<_> = raw.facts.into_iter().collect();
            facts.sort_by(|a, b| a.0.cmp(&b.0));
            facts
        };
        assert_eq!(read(2), read(1));
    }

    #[test]
    fn amounts_keep_what_they_need() {
        assert_eq!(format_amount(27204809.0), "27204809");
        assert_eq!(format_amount(8848.86), "8848.86");
        assert_eq!(format_amount(0.5), "0.5");
    }
}
