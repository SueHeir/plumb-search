//! Layers joined onto the items they extend, as a node merges them before
//! it indexes them (`merge-rules.md` in the design notes). This is how a
//! node merges, not part of the frozen format: two nodes with different
//! settings already merge differently, and it may change in any release.
//!
//! - [`Join`] finds the things an extension names. `wd:` and `id:` refs
//!   name every item with that outside id; `url:` the items whose URL has
//!   the same match key; `site:` the item that stands for the site, whose
//!   id and site are both the domain, as a crawled homepage's are; `item:`
//!   exactly one item. An extension whose target the node doesn't have
//!   does nothing (rule 4).
//! - [`merge`] puts a thing and the extensions on it together. Who wins
//!   goes by [`Trust`]: the owner's own layers, then the project's, then
//!   each publisher the owner added, by its [`Weight`]. Extensions never
//!   change a thing's id, kind, title, URL, site or license (rule 1), and
//!   its site is the one this node works out from its URL
//!   ([`node_site`]), never the one its publisher wrote. Each other field
//!   comes from the highest ranked source that sets it (rule 3). Lists are joined in rank order, each entry once, up to a cap
//!   (rule 2). Vectors count only while they match the text they were made
//!   from (rule 5). A tombstone takes away only its own entry (rule 6).
//!   Names keep the weight of their source, which is how much their words
//!   count (rule 7).

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};

use plumb_format::urls::match_key;
use plumb_format::value::unix_seconds;
use plumb_format::{Edge, Item, Manifest, Ref, Vector};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::items::node_site;

/// Names a merged item keeps; more are dropped.
pub const MAX_NAMES: usize = 50;
/// Labels a merged item keeps.
pub const MAX_LABELS: usize = 50;
/// Edges a merged item keeps.
pub const MAX_EDGES: usize = plumb_format::item::MAX_EDGES;
/// Vectors a merged item keeps, at most one for each model.
pub const MAX_VECTORS: usize = plumb_format::item::MAX_VECTORS;

/// How much a node's owner trusts a publisher they added. A publisher set
/// to *off* has no weight: its sets aren't read at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Weight {
    /// The default for a newly added publisher.
    Low,
    Medium,
    /// Nearly as much as the project.
    High,
}

impl Weight {
    /// How much the publisher's words count against the project's own.
    pub fn value(self) -> f64 {
        match self {
            Weight::Low => 0.25,
            Weight::Medium => 0.5,
            Weight::High => 0.75,
        }
    }
}

/// Whom a set comes from, as the merge ranks it. A publisher the owner
/// set to *off* has none: its sets are left out of the merge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Trust {
    /// The node owner's own layers, personal ones included.
    Owner,
    /// The project: its key, or a key it handed over to. Crawled
    /// homepages count as the project's.
    Project,
    /// A publisher the owner added, at a weight. Between equal weights the
    /// one added first (the lower `added`) ranks first.
    Outside { weight: Weight, added: usize },
}

impl Trust {
    /// How much words from it count (rule 7): 1 for the owner and the
    /// project, less for anyone else.
    pub fn weight(self) -> f64 {
        match self {
            Trust::Owner | Trust::Project => 1.0,
            Trust::Outside { weight, .. } => weight.value(),
        }
    }

    /// Lower ranks first.
    fn rank(self) -> (u8, Reverse<Option<Weight>>, usize) {
        match self {
            Trust::Owner => (0, Reverse(None), 0),
            Trust::Project => (1, Reverse(None), 0),
            Trust::Outside { weight, added } => (2, Reverse(Some(weight)), added),
        }
    }
}

/// A set a node reads, with what the merge needs from its manifest.
#[derive(Clone, Debug)]
pub struct Source {
    /// The publisher's key, as written (`ed25519:...`).
    pub publisher: String,
    /// `<scope>/<name>`.
    pub set: String,
    /// When the generation was built, in Unix seconds. Only ever compared
    /// with another set of the same publisher.
    pub generation: i64,
    pub trust: Trust,
    /// Each field that means a Wikidata property (`same_as`), with it
    /// (`P36`). Fields of two sets are one field only when both name the
    /// same property.
    pub same_as: BTreeMap<String, String>,
    /// Labels on every item of the set.
    pub labels: Vec<String>,
}

impl Source {
    /// The source of the set `manifest` describes, trusted as `trust`.
    /// `None` when its generation has no time.
    pub fn of(manifest: &Manifest, trust: Trust) -> Option<Source> {
        let same_as = manifest
            .fields
            .iter()
            .filter_map(|(name, spec)| match &spec.same_as {
                Some(Ref::Wd(property)) => Some((name.clone(), property.clone())),
                _ => None,
            })
            .collect();
        Some(Source {
            publisher: manifest.publisher.to_string(),
            set: manifest.set.clone(),
            generation: unix_seconds(&manifest.generation)?,
            trust,
            same_as,
            labels: manifest.labels.clone(),
        })
    }
}

/// An item with the set it came from, as an index into the sources.
#[derive(Clone, Copy, Debug)]
pub struct Sourced<'a> {
    pub item: &'a Item,
    pub source: usize,
}

/// The things a node holds, found by each kind of ref.
#[derive(Debug, Default)]
pub struct Join {
    ids: HashMap<(String, String), Vec<usize>>,
    urls: HashMap<String, Vec<usize>>,
    sites: HashMap<String, Vec<usize>>,
    items: HashMap<(String, String, String), usize>,
}

impl Join {
    /// Finds `things` by ref, each by its position. Tombstones and
    /// extensions are passed over.
    pub fn new(sources: &[Source], things: &[Sourced<'_>]) -> Join {
        let mut join = Join::default();
        for (n, thing) in things.iter().enumerate() {
            let item = thing.item;
            if item.is_extension() || item.is_deleted() {
                continue;
            }
            for (namespace, value) in &item.ids {
                join.ids
                    .entry((namespace.clone(), value.clone()))
                    .or_default()
                    .push(n);
            }
            if let Some(key) = item.url.as_deref().and_then(match_key) {
                join.urls.entry(key).or_default().push(n);
            }
            if node_site(item).as_deref() == Some(item.id.as_str()) {
                join.sites.entry(item.id.clone()).or_default().push(n);
            }
            let source = &sources[thing.source];
            join.items.insert(
                (
                    source.publisher.clone(),
                    source.set.clone(),
                    item.id.clone(),
                ),
                n,
            );
        }
        join
    }

    /// The positions of the things `target` names.
    pub fn targets(&self, target: &Ref) -> Vec<usize> {
        let found = match target {
            Ref::Wd(id) => self.ids.get(&("wd".to_string(), id.clone())),
            Ref::Id { namespace, value } => self.ids.get(&(namespace.clone(), value.clone())),
            Ref::Url(url) => match_key(url).and_then(|key| self.urls.get(&key)),
            Ref::Site(domain) => self.sites.get(domain),
            Ref::Item { publisher, set, id } => {
                return self
                    .items
                    .get(&(publisher.clone(), set.clone(), id.clone()))
                    .map(|n| vec![*n])
                    .unwrap_or_default();
            }
            Ref::Blob(_) => None,
        };
        found.cloned().unwrap_or_default()
    }
}

/// How exactly a ref names its target: lower is more exact, and wins
/// between two layers of one publisher.
fn exactness(target: &Ref) -> u8 {
    match target {
        Ref::Item { .. } => 1,
        Ref::Url(_) => 2,
        Ref::Site(_) => 3,
        Ref::Id { .. } => 4,
        Ref::Wd(_) => 5,
        Ref::Blob(_) => 6,
    }
}

/// A field, by what it means.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldKey {
    /// A field that means a Wikidata property (`P36`), from whichever set.
    SameAs(String),
    /// A field of one set alone.
    Own { set: String, name: String },
}

/// A field of a merged item, with where it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct MergedField {
    /// Its name in the set it came from.
    pub name: String,
    pub value: Value,
    pub source: usize,
}

/// What the caps and the text check left out of a merged item, so "why
/// is X missing" has an answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    pub names: usize,
    pub labels: usize,
    pub edges: usize,
    /// Over the cap, or a second one of a model.
    pub vectors: usize,
    /// Made from text the item no longer holds.
    pub stale_vectors: usize,
}

/// A thing with the layers on it put together.
#[derive(Clone, Debug, PartialEq)]
pub struct Merged {
    /// The thing, with its layers' fixed fields on it. Its `fields` are
    /// empty: they are in [`Merged::fields`], by what they mean.
    pub item: Item,
    /// The source of the thing itself.
    pub source: usize,
    /// How much each of `item.names` counts, in the same order: the weight
    /// of the source it came from (rule 7).
    pub name_weights: Vec<f64>,
    /// The source of each of `item.labels`, in the same order. A label
    /// from outside the owner and the project only warns unless the owner
    /// said otherwise.
    pub label_sources: Vec<usize>,
    /// The source of each of `item.edges`, in the same order.
    pub edge_sources: Vec<usize>,
    /// The source of each of the fixed fields set: `about`, `text`,
    /// `body`, `lang`, `weight`, `published`, `expires`, and `ids.<ns>`.
    pub sources: BTreeMap<String, usize>,
    pub fields: BTreeMap<FieldKey, MergedField>,
    pub dropped: Dropped,
}

/// One item that adds to the merge: the thing itself, or an extension.
struct Part<'a> {
    item: &'a Item,
    source: usize,
    /// 0 for the thing itself, else how exactly its ref names the thing.
    exactness: u8,
}

/// Merges `thing` with the extensions `on` it (see the module notes).
/// `None` when the thing is a tombstone.
pub fn merge(sources: &[Source], thing: Sourced<'_>, on: &[Sourced<'_>]) -> Option<Merged> {
    if thing.item.is_deleted() || thing.item.is_extension() {
        return None;
    }
    let mut parts: Vec<Part<'_>> = vec![Part {
        item: thing.item,
        source: thing.source,
        exactness: 0,
    }];
    parts.extend(
        on.iter()
            .filter(|layer| !layer.item.is_deleted())
            .filter_map(|layer| {
                Some(Part {
                    item: layer.item,
                    source: layer.source,
                    exactness: exactness(layer.item.extends.as_ref()?),
                })
            }),
    );
    parts.sort_by(|a, b| {
        let (sa, sb) = (&sources[a.source], &sources[b.source]);
        let key = |part: &Part<'_>, source: &Source| {
            (
                source.trust.rank(),
                part.exactness,
                source.publisher.clone(),
                Reverse(source.generation),
                source.set.clone(),
            )
        };
        key(a, sa).cmp(&key(b, sb))
    });

    let mut merged = Merged {
        item: Item {
            fields: BTreeMap::new(),
            names: Vec::new(),
            labels: Vec::new(),
            edges: Vec::new(),
            vectors: Vec::new(),
            ids: BTreeMap::new(),
            about: None,
            text: None,
            body: None,
            lang: None,
            weight: None,
            published: None,
            expires: None,
            site: node_site(thing.item),
            ..thing.item.clone()
        },
        source: thing.source,
        name_weights: Vec::new(),
        label_sources: Vec::new(),
        edge_sources: Vec::new(),
        sources: BTreeMap::new(),
        fields: BTreeMap::new(),
        dropped: Dropped::default(),
    };
    let mut names: HashSet<String> = HashSet::new();
    let mut labels: HashSet<String> = HashSet::new();
    let mut edges: HashSet<(String, Ref)> = HashSet::new();
    let mut vectors: Vec<&Vector> = Vec::new();
    for part in &parts {
        let item = part.item;
        let source = &sources[part.source];
        let out = &mut merged.item;
        let from = &mut merged.sources;
        let mut first = |name: &str, set: bool| {
            if set && !from.contains_key(name) {
                from.insert(name.to_string(), part.source);
                true
            } else {
                false
            }
        };
        if first("about", item.about.is_some()) {
            out.about = item.about.clone();
        }
        if first("text", item.text.is_some()) {
            out.text = item.text.clone();
        }
        if first("body", item.body.is_some()) {
            out.body = item.body.clone();
        }
        if first("lang", item.lang.is_some()) {
            out.lang = item.lang.clone();
        }
        if first("weight", item.weight.is_some()) {
            out.weight = item.weight;
        }
        if first("published", item.published.is_some()) {
            out.published = item.published.clone();
        }
        if first("expires", item.expires.is_some()) {
            out.expires = item.expires.clone();
        }
        for (namespace, value) in &item.ids {
            if first(&format!("ids.{namespace}"), true) {
                out.ids.insert(namespace.clone(), value.clone());
            }
        }
        for name in &item.names {
            if !names.insert(name.to_lowercase()) {
                continue;
            }
            if out.names.len() < MAX_NAMES {
                out.names.push(name.clone());
                merged.name_weights.push(source.trust.weight());
            } else {
                merged.dropped.names += 1;
            }
        }
        for label in item.labels.iter().chain(&source.labels) {
            if !labels.insert(label.clone()) {
                continue;
            }
            if out.labels.len() < MAX_LABELS {
                out.labels.push(label.clone());
                merged.label_sources.push(part.source);
            } else {
                merged.dropped.labels += 1;
            }
        }
        for edge in &item.edges {
            if !edges.insert((edge.rel.clone(), edge.to.clone())) {
                continue;
            }
            if out.edges.len() < MAX_EDGES {
                out.edges.push(Edge::clone(edge));
                merged.edge_sources.push(part.source);
            } else {
                merged.dropped.edges += 1;
            }
        }
        vectors.extend(&item.vectors);
        for (name, value) in &item.fields {
            let key = match source.same_as.get(name) {
                Some(property) => FieldKey::SameAs(property.clone()),
                None => FieldKey::Own {
                    set: source.set.clone(),
                    name: name.clone(),
                },
            };
            merged.fields.entry(key).or_insert_with(|| MergedField {
                name: name.clone(),
                value: value.clone(),
                source: part.source,
            });
        }
    }
    // Once the text is settled: vectors made from other text don't count.
    let mut models: HashSet<String> = HashSet::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for vector in vectors {
        if !seen.insert((vector.model.clone(), vector.of.clone())) {
            continue;
        }
        if !merged.item.is_current(vector) {
            merged.dropped.stale_vectors += 1;
        } else if merged.item.vectors.len() < MAX_VECTORS && models.insert(vector.model.clone()) {
            merged.item.vectors.push(vector.clone());
        } else {
            merged.dropped.vectors += 1;
        }
    }
    Some(merged)
}

/// Merges every thing in `things` with the extensions in `layers` that
/// name it, in the order of `things`. Tombstones give nothing; an
/// extension that names nothing the node holds does nothing.
pub fn merge_all(
    sources: &[Source],
    things: &[Sourced<'_>],
    layers: &[Sourced<'_>],
) -> Vec<Merged> {
    let join = Join::new(sources, things);
    let mut on: Vec<Vec<Sourced<'_>>> = vec![Vec::new(); things.len()];
    for layer in layers {
        let Some(target) = &layer.item.extends else {
            continue;
        };
        if layer.item.is_deleted() {
            continue;
        }
        for n in join.targets(target) {
            on[n].push(*layer);
        }
    }
    things
        .iter()
        .zip(&on)
        .filter_map(|(thing, on)| merge(sources, *thing, on))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    fn item(value: Value) -> Item {
        serde_json::from_value(value).unwrap()
    }

    fn source(publisher: &str, set: &str, generation: i64, trust: Trust) -> Source {
        Source {
            publisher: publisher.into(),
            set: set.into(),
            generation,
            trust,
            same_as: BTreeMap::new(),
            labels: Vec::new(),
        }
    }

    const PROJECT: &str = "ed25519:project";

    fn outside(weight: Weight, added: usize) -> Trust {
        Trust::Outside { weight, added }
    }

    fn sha256(text: &str) -> String {
        Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn the_owner_then_the_project_then_outside_publishers_by_weight() {
        let mut ranked = [
            outside(Weight::Low, 0),
            Trust::Project,
            outside(Weight::High, 5),
            outside(Weight::Low, 1),
            Trust::Owner,
            outside(Weight::Medium, 2),
        ];
        ranked.sort_by_key(|trust| trust.rank());
        assert_eq!(
            ranked,
            [
                Trust::Owner,
                Trust::Project,
                outside(Weight::High, 5),
                outside(Weight::Medium, 2),
                outside(Weight::Low, 0),
                outside(Weight::Low, 1),
            ]
        );
        assert_eq!(Trust::Project.weight(), 1.0);
        assert_eq!(outside(Weight::Low, 0).weight(), 0.25);
        assert!(Weight::High.value() < Trust::Project.weight());
    }

    #[test]
    fn the_highest_ranked_source_sets_each_field_and_extensions_never_rename() {
        let sources = [
            source(
                "ed25519:example",
                "example/places",
                300,
                outside(Weight::Low, 0),
            ),
            source(PROJECT, "plumb/wikidata-facts", 100, Trust::Project),
            source(
                "ed25519:other",
                "other/notes",
                900,
                outside(Weight::High, 1),
            ),
        ];
        let thing = item(json!({
            "id": "osm:n1", "kind": "place", "title": "Café Uno",
            "url": "https://cafe-uno.example/", "about": "a cafe",
            "lang": "it", "ids": {"wd": "Q1", "osm": "n1"},
        }));
        let facts = item(json!({
            "id": "Q1", "extends": "wd:Q1", "about": "café in Rome",
            "ids": {"osm": "n2", "viaf": "9"},
        }));
        let notes = item(json!({
            "id": "n", "extends": "wd:Q1", "about": "best coffee", "lang": "en",
            "kind": "renamed", "site": "elsewhere.example", "license": "CC0-1.0",
        }));
        let merged = merge(
            &sources,
            Sourced {
                item: &thing,
                source: 0,
            },
            &[
                Sourced {
                    item: &facts,
                    source: 1,
                },
                Sourced {
                    item: &notes,
                    source: 2,
                },
            ],
        )
        .unwrap();
        // The project beats both outside publishers, and a high weight a
        // low one, though the thing itself is the low one's.
        assert_eq!(merged.item.about.as_deref(), Some("café in Rome"));
        assert_eq!(merged.sources["about"], 1);
        assert_eq!(merged.item.lang.as_deref(), Some("en"));
        assert_eq!(merged.item.ids["osm"], "n2");
        assert_eq!(merged.item.ids["viaf"], "9");
        assert_eq!(merged.item.ids["wd"], "Q1");
        // What an extension can't change.
        assert_eq!(merged.item.id, "osm:n1");
        assert_eq!(merged.item.kind, "place");
        assert_eq!(merged.item.title.as_deref(), Some("Café Uno"));
        assert_eq!(
            merged.item.url.as_deref(),
            Some("https://cafe-uno.example/")
        );
        assert_eq!(merged.item.site.as_deref(), Some("cafe-uno.example"));
        assert_eq!(merged.item.license, None);
    }

    #[test]
    fn within_one_publisher_the_thing_then_the_exact_ref_then_the_later_generation() {
        let sources = [
            source(PROJECT, "plumb/sites", 100, Trust::Project),
            source(PROJECT, "plumb/leads", 50, Trust::Project),
            source(PROJECT, "plumb/leads-new", 200, Trust::Project),
            source("ed25519:a", "a/old", 1, outside(Weight::Medium, 0)),
            source("ed25519:b", "b/new", 999, outside(Weight::Medium, 1)),
        ];
        let thing = item(json!({
            "id": "python.org", "kind": "site", "url": "https://www.python.org/",
            "about": "own", "ids": {"wd": "Q28865"},
        }));
        let by_wd =
            item(json!({"id": "1", "extends": "wd:Q28865", "about": "by wd", "text": "wd text"}));
        let by_site = item(
            json!({"id": "2", "extends": "site:python.org", "text": "site text", "lang": "en"}),
        );
        let newer = item(json!({"id": "3", "extends": "wd:Q28865", "lang": "en-GB", "weight": 7}));
        let older = item(json!({"id": "4", "extends": "wd:Q28865", "weight": 3}));
        let from_a = item(json!({"id": "5", "extends": "site:python.org", "published": "2020"}));
        let from_b = item(json!({"id": "6", "extends": "site:python.org", "published": "2026"}));
        let merged = merge(
            &sources,
            Sourced {
                item: &thing,
                source: 0,
            },
            &[
                Sourced {
                    item: &by_wd,
                    source: 1,
                },
                Sourced {
                    item: &by_site,
                    source: 1,
                },
                Sourced {
                    item: &older,
                    source: 1,
                },
                Sourced {
                    item: &newer,
                    source: 2,
                },
                Sourced {
                    item: &from_b,
                    source: 4,
                },
                Sourced {
                    item: &from_a,
                    source: 3,
                },
            ],
        )
        .unwrap();
        assert_eq!(
            merged.item.about.as_deref(),
            Some("own"),
            "the thing itself"
        );
        assert_eq!(
            merged.item.text.as_deref(),
            Some("site text"),
            "site: is more exact"
        );
        assert_eq!(
            merged.item.lang.as_deref(),
            Some("en"),
            "site: is more exact"
        );
        assert_eq!(merged.item.weight, Some(7.0), "the later generation");
        // Two publishers' timestamps are never compared: a was added first.
        assert_eq!(merged.item.published.as_deref(), Some("2020"));
    }

    #[test]
    fn lists_are_joined_in_rank_order_once_each_up_to_their_caps() {
        let sources = [
            source("ed25519:x", "x/things", 1, outside(Weight::Low, 0)),
            source(PROJECT, "plumb/names", 1, Trust::Project),
            Source {
                labels: vec!["adult".into()],
                ..source("ed25519:y", "y/labels", 1, outside(Weight::Medium, 1))
            },
        ];
        let thing = item(json!({
            "id": "a", "kind": "thing", "title": "A", "url": "https://a.example/",
            "names": ["Alpha", "Ay"], "labels": ["machine-written"],
            "edges": [{"rel": "links_to", "to": "site:b.example"}],
        }));
        let many: Vec<String> = (0..60).map(|n| format!("name {n}")).collect();
        let project = item(json!({
            "id": "p", "extends": "url:http://a.example/", "names": ["ALPHA"],
            "edges": [{"rel": "links_to", "to": "site:b.example"}, {"rel": "by", "to": "wd:Q5"}],
        }));
        let labeler = item(json!({
            "id": "l", "extends": "url:https://a.example/", "names": many, "labels": ["scam"],
        }));
        let merged = merge(
            &sources,
            Sourced {
                item: &thing,
                source: 0,
            },
            &[
                Sourced {
                    item: &labeler,
                    source: 2,
                },
                Sourced {
                    item: &project,
                    source: 1,
                },
            ],
        )
        .unwrap();
        assert_eq!(merged.item.names[..3], ["ALPHA", "name 0", "name 1"]);
        assert_eq!(merged.item.names.len(), MAX_NAMES);
        assert_eq!(merged.name_weights[..2], [1.0, 0.5]);
        // The thing's own names come last: its publisher ranks lowest.
        assert_eq!(merged.dropped.names, 60 + 1 - (MAX_NAMES - 1));
        assert_eq!(merged.item.labels, ["scam", "adult", "machine-written"]);
        assert_eq!(merged.label_sources, [2, 2, 0]);
        assert_eq!(merged.item.edges.len(), 2);
        assert_eq!(merged.edge_sources, [1, 1]);
    }

    #[test]
    fn vectors_count_only_while_they_match_the_text() {
        let sources = [
            source(PROJECT, "plumb/docs", 1, Trust::Project),
            source(PROJECT, "plumb/docs-vectors", 1, Trust::Project),
        ];
        let text = "tomllib parses TOML.";
        let body = "a".repeat(64);
        let thing = item(json!({
            "id": "https://docs.python.org/3/library/tomllib.html", "kind": "docs-page",
            "url": "https://docs.python.org/3/library/tomllib.html", "text": text,
            "body": format!("blob:sha256-{body}"),
        }));
        let vector = |model: &str, of: &str| json!({"model": model, "of": of, "b64": "AAAA"});
        let layer = item(json!({
            "id": "v", "extends": "url:https://docs.python.org/3/library/tomllib.html",
            "vectors": [
                vector("bge-small", &sha256(text)),
                vector("bge-small", &body),
                vector("minilm", &sha256("tomllib parses TOML (old).")),
                vector("minilm", &body),
                vector("e5", &body), vector("gte", &body), vector("nomic", &body),
            ],
        }));
        let merged = merge(
            &sources,
            Sourced {
                item: &thing,
                source: 0,
            },
            &[Sourced {
                item: &layer,
                source: 1,
            }],
        )
        .unwrap();
        let models: Vec<&str> = merged
            .item
            .vectors
            .iter()
            .map(|v| v.model.as_str())
            .collect();
        assert_eq!(models, ["bge-small", "minilm", "e5", "gte"]);
        assert_eq!(merged.dropped.stale_vectors, 1);
        assert_eq!(
            merged.dropped.vectors, 2,
            "a second bge-small, and nomic past the cap"
        );
    }

    #[test]
    fn fields_are_one_only_when_both_sets_say_what_they_mean() {
        let mut facts = source(PROJECT, "plumb/wikidata-facts", 1, Trust::Project);
        facts.same_as.insert("capital".into(), "P36".into());
        let mut atlas = source("ed25519:atlas", "atlas/cities", 1, outside(Weight::High, 0));
        atlas.same_as.insert("seat".into(), "P36".into());
        let sources = [
            source(PROJECT, "plumb/wikipedia-en", 1, Trust::Project),
            facts,
            atlas,
        ];
        let thing = item(json!({
            "id": "Australia", "kind": "article", "url": "https://en.wikipedia.org/wiki/Australia",
            "ids": {"wd": "Q408"}, "fields": {"capital": "Sydney?"},
        }));
        let from_facts =
            item(json!({"id": "Q408", "extends": "wd:Q408", "fields": {"capital": "Canberra"}}));
        let from_atlas = item(
            json!({"id": "au", "extends": "wd:Q408", "fields": {"seat": "Sydney", "flag": "AU"}}),
        );
        let merged = merge(
            &sources,
            Sourced {
                item: &thing,
                source: 0,
            },
            &[
                Sourced {
                    item: &from_atlas,
                    source: 2,
                },
                Sourced {
                    item: &from_facts,
                    source: 1,
                },
            ],
        )
        .unwrap();
        assert!(merged.item.fields.is_empty());
        let capital = &merged.fields[&FieldKey::SameAs("P36".into())];
        assert_eq!(
            (capital.value.clone(), capital.source),
            (json!("Canberra"), 1)
        );
        // The article's own `capital` names no property: its own field.
        let own = FieldKey::Own {
            set: "plumb/wikipedia-en".into(),
            name: "capital".into(),
        };
        assert_eq!(merged.fields[&own].value, json!("Sydney?"));
        let flag = FieldKey::Own {
            set: "atlas/cities".into(),
            name: "flag".into(),
        };
        assert_eq!(merged.fields[&flag].source, 2);
    }

    #[test]
    fn refs_find_their_targets_and_tombstones_take_only_their_own_entry() {
        let sources = [
            source(PROJECT, "plumb/sites", 1, Trust::Project),
            source(PROJECT, "plumb/wikipedia-en", 1, Trust::Project),
            source(PROJECT, "plumb/docs", 1, Trust::Project),
            source("ed25519:l", "l/notes", 1, outside(Weight::Low, 0)),
        ];
        let site = item(json!({
            "id": "python.org", "kind": "site", "url": "https://www.python.org/",
            "ids": {"wd": "Q28865"},
        }));
        let article = item(json!({
            "id": "https://en.wikipedia.org/wiki/Python_(programming_language)",
            "kind": "article", "url": "https://en.wikipedia.org/wiki/Python_(programming_language)",
            "ids": {"wd": "Q28865"},
        }));
        let page = item(json!({
            "id": "https://docs.python.org/3/", "kind": "docs-page",
            "url": "https://docs.python.org/3/", "ids": {"pypi": "python"},
        }));
        let gone = item(json!({"id": "x", "kind": "site", "deleted": {"at": "2026-10-01"}}));
        let things = [
            Sourced {
                item: &site,
                source: 0,
            },
            Sourced {
                item: &article,
                source: 1,
            },
            Sourced {
                item: &page,
                source: 2,
            },
            Sourced {
                item: &gone,
                source: 0,
            },
        ];
        let join = Join::new(&sources, &things);
        let targets = |text: &str| join.targets(&text.parse().unwrap());
        assert_eq!(targets("wd:Q28865"), [0, 1], "every item about it");
        assert_eq!(
            targets("site:python.org"),
            [0],
            "only the item that is the site"
        );
        assert_eq!(targets("url:http://docs.python.org/3"), [2], "by match key");
        assert_eq!(targets("id:pypi:python"), [2]);
        assert_eq!(
            targets("item:ed25519:project/plumb%2Fwikipedia-en/https:%2F%2Fen.wikipedia.org%2Fwiki%2FPython_(programming_language)"),
            [1]
        );
        assert!(targets("item:ed25519:other/plumb%2Fwikipedia-en/x").is_empty());
        assert!(
            targets("site:example.org").is_empty(),
            "no target, no effect"
        );

        let named = item(json!({"id": "n1", "extends": "wd:Q28865", "names": ["CPython"]}));
        let taken_back = item(json!({
            "id": "n2", "extends": "site:python.org", "names": ["Python"],
            "deleted": {"at": "2026-10-02"},
        }));
        let merged = merge_all(
            &sources,
            &things,
            &[
                Sourced {
                    item: &named,
                    source: 3,
                },
                Sourced {
                    item: &taken_back,
                    source: 3,
                },
            ],
        );
        assert_eq!(merged.len(), 3, "the tombstoned thing gives nothing");
        assert_eq!(merged[0].item.names, ["CPython"]);
        assert_eq!(merged[0].name_weights, [0.25]);
        assert_eq!(merged[1].item.names, ["CPython"]);
        assert!(merged[2].item.names.is_empty());
    }
}
