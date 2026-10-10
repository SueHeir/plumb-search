//! `plumb relations`: learns each kind of Wikidata fact (capital, founder,
//! CEO...) as a map between the vectors of Wikipedia articles
//! ([`plumb_embed::relations`]), and measures how well the maps find the
//! facts they were not shown.
//!
//! The articles file is read twice: first for the facts whose value is
//! another item (the most read articles first, at most `--per-kind` of each
//! kind), then for the articles those values name. Each article in a fact
//! is embedded once, as its title and description; the vectors are kept in
//! a file so that a second run embeds nothing it embedded before.
//!
//! One fact in ten (by its subject's item, so a subject's facts stay
//! together) is held out. The rest fit the maps; one in nine of those picks
//! the ridge penalty. The report gives, for each kind, how often the held
//! out fact's object is the nearest of all that kind's objects, or among
//! the nearest ten, next to the same without the map (the subject's own
//! vector), and how well the map tells true facts from facts with a wrong
//! object.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use clap::Args;
use plumb_core::facts::{FactKind, ValueType, KINDS};
use plumb_embed::relations::{dot, unit};
use plumb_embed::{text_hash, Relation, Relations, Vectors, RELATIONS_FILE_NAME};
use plumb_index::pages::Page;

use crate::meaning::load_embedder;

/// File of the article vectors, next to the maps.
pub const ARTICLE_VECTORS_FILE: &str = "article-vectors.bin";

/// The articles with a vector, next to the maps: item, title, description
/// and the kinds of fact it is the object of, tab-separated.
pub const ARTICLES_FILE: &str = "articles.tsv";

/// The facts the maps were fitted on: kind, subject item, object item.
pub const FACTS_FILE: &str = "facts.tsv";

/// Ridge penalties tried, per fact.
/// The largest leaves each subject nearly where it is.
const RIDGES: &[f64] = &[0.001, 0.003, 0.01, 0.03, 0.1, 0.3, 1.0, 3.0, 10.0, 100.0];

/// Held-out facts the penalty is picked on, at most, per kind.
const MAX_TUNE: usize = 1_000;

#[derive(Debug, Args)]
pub struct RelationsArgs {
    /// English Wikipedia articles file (`pages/sets/` in a node's data
    /// directory), with facts.
    #[arg(long, value_name = "FILE")]
    pub articles: PathBuf,
    /// Directory of the embedding model.
    #[arg(long, value_name = "DIR")]
    pub model: PathBuf,
    /// Directory the maps and the article vectors are written to.
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
    /// Most facts of each kind used, from the most read articles.
    #[arg(long, value_name = "N", default_value_t = 20_000)]
    pub per_kind: usize,
    /// Texts embedded at once [default: one per CPU].
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,
}

/// A fact whose value is another article.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Link {
    pub kind: FactKind,
    pub subject: String,
    pub object: String,
}

/// An article in a fact: its item, title and description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entity {
    pub title: String,
    pub description: String,
}

impl Entity {
    /// What the model embeds.
    pub fn text(&self) -> String {
        if self.description.is_empty() {
            self.title.clone()
        } else {
            format!("{}. {}", self.title, self.description)
        }
    }
}

/// Kinds whose value is an item.
fn item_kinds() -> impl Iterator<Item = FactKind> {
    KINDS
        .iter()
        .copied()
        .filter(|kind| kind.value_type() == ValueType::Item)
}

/// The facts linking articles of `pages` (read twice), at most `per_kind`
/// of each kind, and the articles in them by item.
pub(crate) fn links<I>(
    mut pages: impl FnMut() -> Result<I>,
    per_kind: usize,
) -> Result<(Vec<Link>, HashMap<String, Entity>)>
where
    I: Iterator<Item = Page>,
{
    // The facts, by the name of their value.
    let mut named: Vec<(FactKind, String, String)> = Vec::new();
    let mut counts: HashMap<FactKind, usize> = HashMap::new();
    let mut entities: HashMap<String, Entity> = HashMap::new();
    for page in pages()? {
        let Some(item) = page.item.as_deref() else {
            continue;
        };
        let mut used = false;
        for fact in &page.facts {
            if fact.kind.value_type() != ValueType::Item {
                continue;
            }
            let count = counts.entry(fact.kind).or_default();
            if *count >= per_kind {
                continue;
            }
            *count += 1;
            used = true;
            named.push((fact.kind, item.to_string(), fact.value.clone()));
        }
        if used {
            entities.insert(item.to_string(), entity(&page));
        }
    }
    let wanted: HashSet<&str> = named.iter().map(|(_, _, name)| name.as_str()).collect();
    let mut by_title: HashMap<String, String> = HashMap::new();
    for page in pages()? {
        let Some(item) = page.item.as_deref() else {
            continue;
        };
        if !wanted.contains(page.title.as_str()) || by_title.contains_key(&page.title) {
            continue;
        }
        by_title.insert(page.title.clone(), item.to_string());
        entities
            .entry(item.to_string())
            .or_insert_with(|| entity(&page));
    }
    let links = named
        .into_iter()
        .filter_map(|(kind, subject, name)| {
            let object = by_title.get(&name)?.clone();
            (object != subject).then_some(Link {
                kind,
                subject,
                object,
            })
        })
        .collect();
    Ok((links, entities))
}

fn entity(page: &Page) -> Entity {
    Entity {
        title: page.title.clone(),
        description: page.description.clone().unwrap_or_default(),
    }
}

/// Which part a fact about `subject` falls in: 0 held out, 1 picks the
/// penalty, 2 fits the map.
pub(crate) fn part(subject: &str) -> u8 {
    use sha2::{Digest, Sha256};
    match Sha256::digest(subject.as_bytes())[0] % 10 {
        0 => 0,
        1 => 1,
        _ => 2,
    }
}

/// How well a map does on some facts.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Score {
    pub facts: usize,
    /// Facts whose object is the nearest of the kind's objects.
    pub first: usize,
    /// Facts whose object is among the nearest ten.
    pub top10: usize,
    /// The mean of 1 / the object's place.
    pub mrr: f64,
    /// How often a true fact is closer than the same subject with another
    /// object of the kind.
    pub auc: f64,
}

/// Scores `apply` on `facts` (subject and object rows of `vectors`), with
/// `candidates` the rows of every object of the kind. Another true object
/// of the same subject (`truths`) is not counted against it.
pub(crate) fn score(
    facts: &[(usize, usize)],
    candidates: &[usize],
    vectors: &[Vec<f32>],
    truths: &HashMap<usize, HashSet<usize>>,
    apply: impl Fn(&[f32]) -> Option<Vec<f32>>,
) -> Score {
    let mut out = Score::default();
    let mut pairs = 0usize;
    let mut wins = 0f64;
    for (n, &(subject, object)) in facts.iter().enumerate() {
        let Some(at) = apply(&vectors[subject]) else {
            continue;
        };
        out.facts += 1;
        let right = dot(&at, &vectors[object]);
        let others = truths.get(&subject);
        let ahead = candidates
            .iter()
            .filter(|&&c| c != object && others.is_none_or(|o| !o.contains(&c)))
            .filter(|&&c| dot(&at, &vectors[c]) > right)
            .count();
        if ahead == 0 {
            out.first += 1;
        }
        if ahead < 10 {
            out.top10 += 1;
        }
        out.mrr += 1.0 / (ahead + 1) as f64;
        // A wrong object: a fixed other candidate.
        if candidates.len() > 1 {
            let wrong = candidates[(n * 7919 + 1) % candidates.len()];
            if wrong != object && others.is_none_or(|o| !o.contains(&wrong)) {
                pairs += 1;
                let closeness = dot(&at, &vectors[wrong]);
                wins += if right > closeness {
                    1.0
                } else if right == closeness {
                    0.5
                } else {
                    0.0
                };
            }
        }
    }
    if out.facts > 0 {
        out.mrr /= out.facts as f64;
    }
    if pairs > 0 {
        out.auc = wins / pairs as f64;
    }
    out
}

/// One kind's results.
#[derive(Debug, Clone)]
pub(crate) struct KindReport {
    pub kind: FactKind,
    pub fitted_on: usize,
    pub candidates: usize,
    pub ridge: f64,
    pub with_map: Score,
    pub without_map: Score,
}

/// Fits each kind's map on `links` with the article `vectors` (unit, by
/// row of `rows`) and scores it on the held-out facts.
pub(crate) fn fit_all(
    links: &[Link],
    rows: &HashMap<String, usize>,
    vectors: &[Vec<f32>],
    relations: &mut Relations,
) -> Result<Vec<KindReport>> {
    let dim = relations.dim();
    let mut reports = Vec::new();
    for kind in item_kinds() {
        let mut parts: [Vec<(usize, usize)>; 3] = Default::default();
        let mut truths: HashMap<usize, HashSet<usize>> = HashMap::new();
        let mut candidates: Vec<usize> = Vec::new();
        let mut seen = HashSet::new();
        for link in links.iter().filter(|link| link.kind == kind) {
            let (Some(&s), Some(&o)) = (rows.get(&link.subject), rows.get(&link.object)) else {
                continue;
            };
            parts[usize::from(part(&link.subject))].push((s, o));
            truths.entry(s).or_default().insert(o);
            if seen.insert(o) {
                candidates.push(o);
            }
        }
        let [held_out, tune, fit] = parts;
        if fit.len() < 50 || held_out.is_empty() {
            continue;
        }
        let pairs = |facts: &[(usize, usize)]| -> Vec<(Vec<f32>, Vec<f32>)> {
            facts
                .iter()
                .map(|&(s, o)| (vectors[s].clone(), vectors[o].clone()))
                .collect()
        };
        let fit_pairs = pairs(&fit);
        let borrowed: Vec<(&[f32], &[f32])> = fit_pairs
            .iter()
            .map(|(s, o)| (s.as_slice(), o.as_slice()))
            .collect();
        let tune_on = &tune[..tune.len().min(MAX_TUNE)];
        let mut best: Option<(f64, f64)> = None;
        if !tune_on.is_empty() {
            for &ridge in RIDGES {
                let relation = Relation::fit(kind.key(), dim, &borrowed, ridge)?;
                let mrr = score(tune_on, &candidates, vectors, &truths, |s| {
                    relation.apply(s)
                })
                .mrr;
                if best.is_none_or(|(_, b)| mrr > b) {
                    best = Some((ridge, mrr));
                }
            }
        }
        let ridge = best.map_or(RIDGES[RIDGES.len() / 2], |(ridge, _)| ridge);
        // Refit with the penalty's facts too.
        let all: Vec<(usize, usize)> = fit.iter().chain(&tune).copied().collect();
        let all_pairs = pairs(&all);
        let borrowed: Vec<(&[f32], &[f32])> = all_pairs
            .iter()
            .map(|(s, o)| (s.as_slice(), o.as_slice()))
            .collect();
        let mut relation = Relation::fit(kind.key(), dim, &borrowed, ridge)?;
        let right: Vec<f32> = all
            .iter()
            .map(|&(s, o)| relation.closeness(&vectors[s], &vectors[o]))
            .collect();
        let wrong: Vec<f32> = all
            .iter()
            .enumerate()
            .filter_map(|(n, &(s, o))| {
                let w = candidates[(n * 7919 + 3) % candidates.len()];
                (w != o && !truths[&s].contains(&w))
                    .then(|| relation.closeness(&vectors[s], &vectors[w]))
            })
            .collect();
        relation.calibrate(&right, &wrong);
        let with_map = score(&held_out, &candidates, vectors, &truths, |s| {
            relation.apply(s)
        });
        let without_map = score(&held_out, &candidates, vectors, &truths, |s| {
            Some(s.to_vec())
        });
        relations.insert(relation)?;
        reports.push(KindReport {
            kind,
            fitted_on: all.len(),
            candidates: candidates.len(),
            ridge,
            with_map,
            without_map,
        });
    }
    Ok(reports)
}

/// The report as a table.
pub(crate) fn report_table(reports: &[KindReport]) -> String {
    let mut out = String::from(
        "kind                ridge  fitted  objects  held out   nearest  top 10    MRR   true>wrong  | no map: nearest  top 10  true>wrong\n",
    );
    let pct = |n: usize, of: usize| 100.0 * n as f64 / of.max(1) as f64;
    for r in reports {
        let w = &r.with_map;
        let wo = &r.without_map;
        out.push_str(&format!(
            "{:<18} {:>5}  {:>7}  {:>7}  {:>8}   {:>6.1}%  {:>5.1}%  {:.3}  {:>9.1}%  | {:>14.1}%  {:>5.1}%  {:>9.1}%\n",
            r.kind.key(),
            r.ridge,
            r.fitted_on,
            r.candidates,
            w.facts,
            pct(w.first, w.facts),
            pct(w.top10, w.facts),
            w.mrr,
            100.0 * w.auc,
            pct(wo.first, wo.facts),
            pct(wo.top10, wo.facts),
            100.0 * wo.auc,
        ));
    }
    out
}

/// Embeds the `entities` not in `vectors` yet (or whose text changed), on
/// `threads` threads, saving now and then.
fn embed_entities(
    embedder: &plumb_embed::Embedder,
    vectors: &Mutex<Vectors>,
    entities: &HashMap<String, Entity>,
    threads: usize,
    path: &Path,
) -> Result<()> {
    let mut todo: Vec<(&String, String)> = {
        let vectors = vectors.lock().unwrap_or_else(|e| e.into_inner());
        entities
            .iter()
            .map(|(item, entity)| (item, entity.text()))
            .filter(|(item, text)| {
                vectors
                    .get(item)
                    .is_none_or(|(hash, _)| *hash != text_hash(text))
            })
            .collect()
    };
    todo.sort();
    let total = todo.len();
    println!("embedding {total} articles");
    let started = std::time::Instant::now();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| loop {
                let n = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some((item, text)) = todo.get(n) else {
                    break;
                };
                match embedder.embed(text) {
                    Ok(vector) => {
                        let mut vectors = vectors.lock().unwrap_or_else(|e| e.into_inner());
                        if vectors.insert(item, text_hash(text), &vector).is_err() {
                            failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        if (n + 1).is_multiple_of(crate::meaning::SAVE_EVERY) {
                            if let Err(err) = vectors.save(path) {
                                eprintln!("saving {}: {err:#}", path.display());
                            }
                            let rate = (n + 1) as f64 / started.elapsed().as_secs_f64();
                            println!("  {} of {total} ({rate:.0} a second)", n + 1);
                        }
                    }
                    Err(_) => {
                        failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            });
        }
    });
    let failed = failed.into_inner();
    if failed > 0 {
        println!("{failed} articles could not be embedded");
    }
    vectors.lock().unwrap_or_else(|e| e.into_inner()).save(path)
}

/// Writes [`ARTICLES_FILE`] and [`FACTS_FILE`] to `dir`, for
/// [`RelationStore`].
fn write_tables(
    dir: &Path,
    links: &[Link],
    entities: &HashMap<String, Entity>,
    rows: &HashMap<String, usize>,
) -> Result<()> {
    let clean = |text: &str| text.replace(['\t', '\n', '\r'], " ");
    let mut objects: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut facts = String::new();
    for link in links {
        if !rows.contains_key(&link.subject) || !rows.contains_key(&link.object) {
            continue;
        }
        let kinds = objects.entry(link.object.as_str()).or_default();
        if !kinds.contains(&link.kind.key()) {
            kinds.push(link.kind.key());
        }
        facts.push_str(&format!(
            "{}\t{}\t{}\n",
            link.kind.key(),
            link.subject,
            link.object
        ));
    }
    let mut items: Vec<&String> = rows.keys().collect();
    items.sort();
    let mut articles = String::new();
    for item in items {
        let entity = &entities[item];
        articles.push_str(&format!(
            "{item}\t{}\t{}\t{}\n",
            clean(&entity.title),
            clean(&entity.description),
            objects
                .get(item.as_str())
                .map_or(String::new(), |k| k.join(","))
        ));
    }
    std::fs::write(dir.join(ARTICLES_FILE), articles)
        .with_context(|| format!("writing {}", dir.join(ARTICLES_FILE).display()))?;
    std::fs::write(dir.join(FACTS_FILE), facts)
        .with_context(|| format!("writing {}", dir.join(FACTS_FILE).display()))
}

/// Most answers [`RelationStore::relate`] gives.
pub const MAX_RELATE_ANSWERS: usize = 20;

/// The maps, the articles they were fitted on and those articles' vectors,
/// for following relations and checking claims (the MCP tool `relate`).
pub struct RelationStore {
    relations: Relations,
    items: Vec<String>,
    titles: Vec<String>,
    descriptions: Vec<String>,
    vectors: Vec<Vec<f32>>,
    by_title: HashMap<String, usize>,
    objects_of: HashMap<String, Vec<usize>>,
    stated: HashSet<(String, usize, usize)>,
    embedder: Option<plumb_embed::Embedder>,
}

/// A thing a relation starts or ends at: an article, or text the model
/// embedded.
struct Point {
    row: Option<usize>,
    title: String,
    vector: Vec<f32>,
}

impl RelationStore {
    /// Reads what `plumb relations` wrote to `dir`. With `model` (the model
    /// that made the vectors), names that are not articles there are
    /// embedded; without, they are not found.
    pub fn load(dir: &Path, model: Option<&Path>) -> Result<Self> {
        let relations = Relations::load(&dir.join(RELATIONS_FILE_NAME))?;
        let vectors = Vectors::load(&dir.join(ARTICLE_VECTORS_FILE))?;
        if vectors.model() != relations.model() {
            bail!("the article vectors and the maps were made by different models");
        }
        let embedder = model.map(load_embedder).transpose()?;
        if let Some(embedder) = &embedder {
            if embedder.id() != relations.model() {
                bail!("the maps were fitted on another model's vectors");
            }
        }
        let path = dir.join(ARTICLES_FILE);
        let articles = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut store = RelationStore {
            relations,
            items: Vec::new(),
            titles: Vec::new(),
            descriptions: Vec::new(),
            vectors: Vec::new(),
            by_title: HashMap::new(),
            objects_of: HashMap::new(),
            stated: HashSet::new(),
            embedder,
        };
        let mut rows = HashMap::new();
        for line in articles.lines() {
            let mut fields = line.split('\t');
            let (Some(item), Some(title), description, kinds) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let Some(vector) = vectors.get(item).and_then(|(_, v)| unit(v)) else {
                continue;
            };
            let row = store.items.len();
            rows.insert(item.to_string(), row);
            store.items.push(item.to_string());
            store.titles.push(title.to_string());
            store
                .descriptions
                .push(description.unwrap_or_default().to_string());
            store.vectors.push(vector);
            store.by_title.entry(title.to_lowercase()).or_insert(row);
            for kind in kinds
                .unwrap_or_default()
                .split(',')
                .filter(|k| !k.is_empty())
            {
                store
                    .objects_of
                    .entry(kind.to_string())
                    .or_default()
                    .push(row);
            }
        }
        let path = dir.join(FACTS_FILE);
        let facts = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        for line in facts.lines() {
            let mut fields = line.split('\t');
            if let (Some(kind), Some(s), Some(o)) = (fields.next(), fields.next(), fields.next()) {
                if let (Some(&s), Some(&o)) = (rows.get(s), rows.get(o)) {
                    store.stated.insert((kind.to_string(), s, o));
                }
            }
        }
        Ok(store)
    }

    /// The kinds there are maps of.
    pub fn kinds(&self) -> Vec<&str> {
        self.relations
            .all()
            .iter()
            .map(|r| r.key.as_str())
            .collect()
    }

    fn point(&self, name: &str) -> Result<Option<Point>> {
        if let Some(&row) = self.by_title.get(&name.to_lowercase()) {
            return Ok(Some(Point {
                row: Some(row),
                title: self.titles[row].clone(),
                vector: self.vectors[row].clone(),
            }));
        }
        let Some(embedder) = &self.embedder else {
            return Ok(None);
        };
        Ok(unit(&embedder.embed(name)?).map(|vector| Point {
            row: None,
            title: name.to_string(),
            vector,
        }))
    }

    fn article(&self, row: usize) -> serde_json::Value {
        serde_json::json!({
            "title": self.titles[row],
            "description": self.descriptions[row],
            "item": self.items[row],
            "item_url": format!("https://www.wikidata.org/wiki/{}", self.items[row]),
        })
    }

    /// Follows the kinds `chain` in turn from `subject`: the likeliest
    /// objects (at most `limit`), or with `object`, how likely it is the
    /// one.
    pub fn relate(
        &self,
        subject: &str,
        chain: &[String],
        object: Option<&str>,
        limit: usize,
    ) -> Result<serde_json::Value> {
        use serde_json::json;
        if chain.is_empty() {
            bail!(
                "say which relation to follow: one of {}",
                self.kinds().join(", ")
            );
        }
        for key in chain {
            if self.relations.get(key).is_none() {
                bail!(
                    "there is no map of {key:?}; there are maps of {}",
                    self.kinds().join(", ")
                );
            }
        }
        let last = chain.last().map(String::as_str).unwrap_or_default();
        let relation = self.relations.get(last).context("no map")?;
        let Some(from) = self.point(subject)? else {
            return Ok(json!({ "subject": subject, "relation": chain, "found": false }));
        };
        let keys: Vec<&str> = chain.iter().map(String::as_str).collect();
        let at = self
            .relations
            .follow(&keys, &from.vector)
            .context("the relation leads nowhere")?;
        let single = chain.len() == 1;
        let stated = |o: usize| {
            single
                && from
                    .row
                    .is_some_and(|s| self.stated.contains(&(last.to_string(), s, o)))
        };
        let mut out = json!({
            "subject": subject,
            "found": true,
            "subject_title": from.title,
            "relation": chain,
            "how": "learned maps between article vectors (EmbeddingGemma), fitted on Wikidata facts",
        });
        if let Some(row) = from.row {
            out["subject_item"] = json!(self.items[row]);
        }
        if let Some(object) = object {
            let Some(to) = self.point(object)? else {
                out["claim"] = json!({ "object": object, "found": false });
                return Ok(out);
            };
            let closeness = dot(&at, &to.vector);
            out["claim"] = json!({
                "object": object,
                "found": true,
                "object_title": to.title,
                "probability": round3(relation.probability(closeness)),
                "stated": to.row.is_some_and(stated),
            });
            return Ok(out);
        }
        let candidates: Vec<usize> = match self.objects_of.get(last) {
            Some(rows) => rows.clone(),
            None => (0..self.vectors.len()).collect(),
        };
        let mut scored: Vec<(usize, f32)> = candidates
            .into_iter()
            .filter(|&row| Some(row) != from.row)
            .map(|row| (row, dot(&at, &self.vectors[row])))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let answers: Vec<serde_json::Value> = scored
            .into_iter()
            .take(limit.clamp(1, MAX_RELATE_ANSWERS))
            .map(|(row, closeness)| {
                let mut answer = self.article(row);
                answer["probability"] = json!(round3(relation.probability(closeness)));
                answer["stated"] = json!(stated(row));
                answer
            })
            .collect();
        out["answers"] = json!(answers);
        Ok(out)
    }
}

fn round3(x: f32) -> f64 {
    (f64::from(x) * 1000.0).round() / 1000.0
}

pub fn run(args: &RelationsArgs) -> Result<()> {
    let set = crate::pages::SetInfo::find("wikipedia-en").context("no English Wikipedia set")?;
    if !args.articles.is_file() {
        bail!("no articles file at {}", args.articles.display());
    }
    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("creating {}", args.out.display()))?;
    let started = std::time::Instant::now();
    let (links, entities) = links(|| set.read(&args.articles, u64::MAX), args.per_kind)?;
    println!(
        "{} facts between {} articles ({:.0}s)",
        links.len(),
        entities.len(),
        started.elapsed().as_secs_f64()
    );

    let embedder = load_embedder(&args.model)?;
    let path = args.out.join(ARTICLE_VECTORS_FILE);
    let vectors = match Vectors::load(&path) {
        Ok(v) if v.model() == embedder.id() && v.dim() == embedder.dim() => v,
        _ => Vectors::new(embedder.id(), embedder.dim()),
    };
    let vectors = Mutex::new(vectors);
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    embed_entities(&embedder, &vectors, &entities, threads, &path)?;
    let vectors = vectors.into_inner().unwrap_or_else(|e| e.into_inner());

    let mut rows = HashMap::new();
    let mut units = Vec::new();
    for item in entities.keys() {
        if let Some(vector) = vectors.get(item).and_then(|(_, v)| unit(v)) {
            rows.insert(item.clone(), units.len());
            units.push(vector);
        }
    }
    let mut relations = Relations::new(embedder.id(), embedder.dim());
    let reports = fit_all(&links, &rows, &units, &mut relations)?;
    relations.save(&args.out.join(RELATIONS_FILE_NAME))?;
    write_tables(&args.out, &links, &entities, &rows)?;
    print!("{}", report_table(&reports));
    println!(
        "{} maps in {} ({:.0}s in all)",
        relations.all().len(),
        args.out.join(RELATIONS_FILE_NAME).display(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use plumb_core::facts::Fact;

    fn page(title: &str, item: &str, description: &str, facts: &[(FactKind, &str)]) -> Page {
        Page {
            set: "wikipedia-en".into(),
            url: format!("https://en.wikipedia.org/wiki/{title}"),
            title: title.into(),
            description: Some(description.into()),
            site: None,
            views: 1,
            aliases: Vec::new(),
            item: Some(item.into()),
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: facts
                .iter()
                .map(|&(kind, value)| Fact {
                    kind,
                    value: value.into(),
                })
                .collect(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            content_language: None,
            search: None,
        }
    }

    #[test]
    fn facts_link_articles_by_the_name_of_their_value() {
        let pages = vec![
            page(
                "Australia",
                "Q408",
                "country in Oceania",
                &[
                    (FactKind::Capital, "Canberra"),
                    (FactKind::Population, "27204809"),
                ],
            ),
            page("Canberra", "Q3114", "capital city of Australia", &[]),
            page(
                "France",
                "Q142",
                "country in Europe",
                &[(FactKind::Capital, "Paris")],
            ),
            page(
                "Atlantis",
                "Q1",
                "legendary island",
                &[(FactKind::Capital, "Nowhere")],
            ),
            page("Paris", "Q90", "capital of France", &[]),
        ];
        let (found, entities) = links(|| Ok(pages.clone().into_iter()), 10).unwrap();
        assert_eq!(
            found,
            [
                Link {
                    kind: FactKind::Capital,
                    subject: "Q408".into(),
                    object: "Q3114".into()
                },
                Link {
                    kind: FactKind::Capital,
                    subject: "Q142".into(),
                    object: "Q90".into()
                },
            ]
        );
        assert_eq!(
            entities["Q3114"].text(),
            "Canberra. capital city of Australia"
        );
        assert!(entities.contains_key("Q1"));
        assert!(!entities.contains_key("Q999"));
        // At most one fact of a kind.
        let (found, _) = links(|| Ok(pages.clone().into_iter()), 1).unwrap();
        assert_eq!(found.len(), 1);
    }

    /// A store in `dir` where each country's capital is its vector turned
    /// a quarter of the way around, and the maps were fitted on that.
    pub(crate) fn write_store(dir: &Path) {
        let dim = 8;
        let model = [9u8; 32];
        let countries = ["Australia", "France", "Japan", "Peru", "Chad", "Fiji"];
        let capitals = ["Canberra", "Paris", "Tokyo", "Lima", "Ndjamena", "Suva"];
        let mut vectors = Vectors::new(model, dim);
        let mut rows = HashMap::new();
        let mut units = Vec::new();
        let mut entities = HashMap::new();
        let mut links = Vec::new();
        for (n, (country, capital)) in countries.iter().zip(capitals).enumerate() {
            let mut c = [0i8; 8];
            c[n % 4] = 100;
            c[4 + n / 4] = 60;
            let mut o = [0i8; 8];
            for d in 0..dim {
                o[(d + 2) % dim] = c[d];
            }
            for (item, title, v) in [
                (format!("C{n}"), country.to_string(), c),
                (format!("K{n}"), capital.to_string(), o),
            ] {
                vectors.insert(&item, text_hash(&title), &v).unwrap();
                rows.insert(item.clone(), units.len());
                units.push(unit(&v).unwrap());
                entities.insert(
                    item,
                    Entity {
                        title,
                        description: String::new(),
                    },
                );
            }
            // The last country's capital is not stated.
            if n < 5 {
                links.push(Link {
                    kind: FactKind::Capital,
                    subject: format!("C{n}"),
                    object: format!("K{n}"),
                });
            }
        }
        let pairs: Vec<(&[f32], &[f32])> = (0..5)
            .map(|n| (units[2 * n].as_slice(), units[2 * n + 1].as_slice()))
            .collect();
        let mut relations = Relations::new(model, dim);
        relations
            .insert(Relation::fit("capital", dim, &pairs, 1e-4).unwrap())
            .unwrap();
        relations.save(&dir.join(RELATIONS_FILE_NAME)).unwrap();
        vectors.save(&dir.join(ARTICLE_VECTORS_FILE)).unwrap();
        write_tables(dir, &links, &entities, &rows).unwrap();
        // Suva is a capital the maps never saw as one.
        let articles = std::fs::read_to_string(dir.join(ARTICLES_FILE)).unwrap();
        let articles = articles.replace("K5\tSuva\t\t\n", "K5\tSuva\t\tcapital\n");
        std::fs::write(dir.join(ARTICLES_FILE), articles).unwrap();
    }

    #[test]
    fn the_store_follows_relations_and_checks_claims() {
        let dir = tempfile::tempdir().unwrap();
        write_store(dir.path());
        let store = RelationStore::load(dir.path(), None).unwrap();
        assert_eq!(store.kinds(), ["capital"]);

        let answer = store
            .relate("australia", &["capital".into()], None, 3)
            .unwrap();
        assert_eq!(answer["subject_item"], "C0");
        assert_eq!(answer["answers"][0]["title"], "Canberra");
        assert_eq!(answer["answers"][0]["stated"], true);
        assert_eq!(answer["answers"].as_array().unwrap().len(), 3);

        // A capital no fact names is found all the same, as a guess.
        let answer = store.relate("Fiji", &["capital".into()], None, 1).unwrap();
        assert_eq!(answer["answers"][0]["title"], "Suva");
        assert_eq!(answer["answers"][0]["stated"], false);

        let right = store
            .relate("France", &["capital".into()], Some("Paris"), 5)
            .unwrap();
        let wrong = store
            .relate("France", &["capital".into()], Some("Lima"), 5)
            .unwrap();
        assert_eq!(right["claim"]["stated"], true);
        assert!(
            right["claim"]["probability"].as_f64().unwrap()
                > wrong["claim"]["probability"].as_f64().unwrap()
        );

        assert_eq!(
            store
                .relate("Atlantis", &["capital".into()], None, 5)
                .unwrap()["found"],
            false
        );
        assert!(store.relate("France", &["ceo".into()], None, 5).is_err());
        assert!(store.relate("France", &[], None, 5).is_err());
    }

    #[test]
    fn maps_beat_plain_closeness_when_objects_are_elsewhere() {
        // Objects are their subjects moved along a fixed direction and
        // shuffled: plain closeness cannot find them, the map can.
        let dim = 12;
        let mut seed = 11u64;
        let mut noise = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        };
        let mut vectors = Vec::new();
        let mut rows = HashMap::new();
        let mut links = Vec::new();
        for i in 0..600 {
            let s: Vec<f32> = (0..dim).map(|_| noise()).collect();
            let o: Vec<f32> = (0..dim)
                .map(|d| s[(d + 4) % dim] + 0.02 * noise())
                .collect();
            let unit = |v: Vec<f32>| {
                let l = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                v.into_iter().map(|x| x / l).collect::<Vec<f32>>()
            };
            for (name, v) in [(format!("S{i}"), unit(s)), (format!("O{i}"), unit(o))] {
                rows.insert(name, vectors.len());
                vectors.push(v);
            }
            links.push(Link {
                kind: FactKind::Founder,
                subject: format!("S{i}"),
                object: format!("O{i}"),
            });
        }
        let mut relations = Relations::new([0; 32], dim);
        let reports = fit_all(&links, &rows, &vectors, &mut relations).unwrap();
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r.kind, FactKind::Founder);
        assert!(r.with_map.facts > 20);
        assert!(
            r.with_map.first as f64 > 0.9 * r.with_map.facts as f64,
            "{r:?}"
        );
        assert!(r.without_map.first * 3 < r.with_map.first, "{r:?}");
        assert!(r.with_map.auc > 0.95);
        let founder = relations.get("founder").unwrap();
        assert!(founder.plausibility(&vectors[0], &vectors[1]) > 0.5);
        assert!(report_table(&reports).contains("founder"));
    }
}
