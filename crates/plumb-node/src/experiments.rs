//! Trying ranking changes on a share of real searches and comparing what
//! people open, the way Google's "Overlapping Experiment Infrastructure"
//! (Tang et al. 2010) does it, cut down to one node.
//!
//! A node runs experiments when its data directory holds
//! `experiments.json`, read when the node starts:
//!
//! ```json
//! {"layers": [
//!   {"name": "ranking", "diversion": "query", "experiments": [
//!     {"name": "control", "percent": 10},
//!     {"name": "more-popularity", "percent": 10, "rank": {"alpha": 0.5}}
//!   ]}
//! ]}
//! ```
//!
//! - An experiment changes ranking knobs ([`plumb_index::RankConfig`], the
//!   same JSON `plumb eval --rank` takes) for a share of searches. One
//!   with no changes is the control the others are compared with: the one
//!   named `control`, else the first that changes nothing.
//! - Each search falls into one bucket of 1,000 in each layer, and each
//!   experiment of the layer takes `percent` of them, so experiments of one
//!   layer never share a search. Buckets are drawn anew for each layer
//!   (from the layer's name), so experiments of different layers overlap
//!   independently and a search can be in one of each. A knob belongs to
//!   one layer; two layers changing it are refused.
//! - What a search falls by (`diversion`): `query`, the search's words, so
//!   a search gives the same results however often it is made; or
//!   `browser`, the browser's history profile, so a person sees one
//!   ranking throughout (browsers with no profile are left out of the
//!   layer).
//!
//! Results of searches in an experiment link through `/go` with a token
//! of the page and the place of the result. What is counted, for each
//! experiment and nothing finer, is how many searches it got, how many
//! had a result opened, at which place the first one was, and how many
//! were opened at each place, in `DIR/experiments-results.json`. Which
//! page holds which token is kept in memory for the last
//! [`MAX_PAGES`] pages only. No search, site or browser is kept.
//!
//! `plumb experiments --data DIR` compares each experiment with its
//! layer's control: the share of searches with a result opened, and the
//! mean reciprocal place of the first one opened (1 for the first result,
//! 1/2 for the second, 0 for none), with 95% confidence intervals. A
//! control against a copy of itself (an A/A test) is a good first run:
//! its intervals should take in 0.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{bail, Context, Result};
use plumb_index::RankConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The file that sets up experiments, in the data directory.
pub const CONFIG_FILE: &str = "experiments.json";
/// What they found, in the data directory.
pub const RESULTS_FILE: &str = "experiments-results.json";
/// Buckets of each layer.
pub const BUCKETS: u32 = 1_000;
/// Places on the page counted.
pub const PLACES: usize = 10;
/// Pages remembered, to tell which experiments a click on one counts for.
pub const MAX_PAGES: usize = 10_000;
/// Searches each of two experiments needs before they are compared.
pub const MIN_SEARCHES: u64 = 100;

/// What falls into a layer's buckets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Diversion {
    /// The search's words.
    #[default]
    Query,
    /// The browser's history profile.
    Browser,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    layers: Vec<LayerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerConfig {
    name: String,
    #[serde(default)]
    diversion: Diversion,
    experiments: Vec<ExperimentConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExperimentConfig {
    name: String,
    percent: f64,
    #[serde(default)]
    rank: Map<String, Value>,
}

/// An experiment as set up.
#[derive(Debug, Clone, PartialEq)]
pub struct Experiment {
    pub name: String,
    /// Its name and a fingerprint of how it is set up, so results of an
    /// experiment set up anew under the same name count apart.
    pub id: String,
    /// Its buckets, `from..to`.
    pub from: u32,
    pub to: u32,
    /// The ranking knobs it changes.
    pub rank: Map<String, Value>,
}

/// A layer as set up.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub name: String,
    pub diversion: Diversion,
    pub experiments: Vec<Experiment>,
}

impl Layer {
    /// The control: the experiment named `control`, else the first that
    /// changes nothing.
    pub fn control(&self) -> Option<&Experiment> {
        self.experiments
            .iter()
            .find(|e| e.name == "control")
            .or_else(|| self.experiments.iter().find(|e| e.rank.is_empty()))
    }
}

/// An experiment a search is in: the layer's and the experiment's place
/// in the set-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Arm {
    pub layer: usize,
    pub experiment: usize,
}

/// What an experiment's searches did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Counts {
    pub searches: u64,
    /// Searches with a result opened.
    pub clicked: u64,
    /// Results opened, at any place.
    pub clicks: u64,
    /// The sum, over searches, of 1 / the place of the first result
    /// opened, and of its square (for the interval).
    pub rr: f64,
    pub rr_sq: f64,
    /// Results opened at each place.
    pub at: Vec<u64>,
}

impl Counts {
    pub fn click_rate(&self) -> f64 {
        ratio(self.clicked as f64, self.searches)
    }

    pub fn mean_rr(&self) -> f64 {
        ratio(self.rr, self.searches)
    }
}

fn ratio(of: f64, n: u64) -> f64 {
    if n == 0 {
        0.0
    } else {
        of / n as f64
    }
}

/// A page of results shown in experiments.
#[derive(Debug, Clone)]
struct Page {
    arms: Vec<Arm>,
    clicked: bool,
    /// Places already counted as opened, so a reload counts once.
    opened: Vec<usize>,
}

#[derive(Debug, Default)]
struct Live {
    results: BTreeMap<String, Counts>,
    pages: HashMap<u64, Page>,
    order: VecDeque<u64>,
}

/// A node's experiments.
#[derive(Debug)]
pub struct Lab {
    layers: Vec<Layer>,
    results_path: Option<PathBuf>,
    live: Mutex<Live>,
}

impl Lab {
    /// The experiments set up in `dir`; `None` when it sets up none.
    pub fn in_dir(dir: &Path) -> Result<Option<Lab>> {
        let path = dir.join(CONFIG_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let mut lab =
            Lab::parse(&text).with_context(|| format!("setting up {}", path.display()))?;
        let results_path = dir.join(RESULTS_FILE);
        if let Ok(bytes) = fs::read(&results_path) {
            if let Ok(results) = serde_json::from_slice(&bytes) {
                lab.live
                    .get_mut()
                    .unwrap_or_else(PoisonError::into_inner)
                    .results = results;
            }
        }
        lab.results_path = Some(results_path);
        Ok((!lab.layers.is_empty()).then_some(lab))
    }

    /// Experiments set up by the JSON `text`, counted in memory only.
    pub fn parse(text: &str) -> Result<Lab> {
        let config: ConfigFile = serde_json::from_str(text)?;
        let known: HashSet<String> = match serde_json::to_value(RankConfig::default())? {
            Value::Object(map) => map.into_iter().map(|(key, _)| key).collect(),
            _ => HashSet::new(),
        };
        let mut names = HashSet::new();
        let mut owner: HashMap<String, String> = HashMap::new();
        let mut layers = Vec::new();
        for layer in config.layers {
            if !names.insert(format!("layer {}", layer.name)) {
                bail!("two layers are named {:?}", layer.name);
            }
            let mut from = 0u32;
            let mut experiments = Vec::new();
            for experiment in layer.experiments {
                if !names.insert(experiment.name.clone()) {
                    bail!("two experiments are named {:?}", experiment.name);
                }
                if !(experiment.percent > 0.0 && experiment.percent <= 100.0) {
                    bail!(
                        "{}: percent must be above 0 and at most 100",
                        experiment.name
                    );
                }
                let buckets = (experiment.percent * f64::from(BUCKETS) / 100.0).round() as u32;
                let to = from + buckets.max(1);
                if to > BUCKETS {
                    bail!("layer {}: its experiments take more than 100%", layer.name);
                }
                for key in experiment.rank.keys() {
                    if !known.contains(key) {
                        bail!("{}: no ranking knob is called {key:?}", experiment.name);
                    }
                    match owner.get(key) {
                        Some(other) if *other != layer.name => bail!(
                            "{key:?} is changed in layers {other:?} and {:?}; a knob \
                             belongs to one layer",
                            layer.name
                        ),
                        _ => {
                            owner.insert(key.clone(), layer.name.clone());
                        }
                    }
                }
                overlay(RankConfig::default(), &experiment.rank)
                    .with_context(|| format!("{}: bad ranking knobs", experiment.name))?;
                let id = format!(
                    "{}@{}",
                    experiment.name,
                    &hex(&Sha256::digest(
                        format!(
                            "{}|{:?}|{from}|{to}|{}",
                            layer.name,
                            layer.diversion,
                            Value::Object(experiment.rank.clone())
                        )
                        .as_bytes()
                    ))[..8]
                );
                experiments.push(Experiment {
                    name: experiment.name,
                    id,
                    from,
                    to,
                    rank: experiment.rank,
                });
                from = to;
            }
            if !experiments.is_empty() {
                layers.push(Layer {
                    name: layer.name,
                    diversion: layer.diversion,
                    experiments,
                });
            }
        }
        Ok(Lab {
            layers,
            results_path: None,
            live: Mutex::new(Live::default()),
        })
    }

    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    pub fn experiment(&self, arm: Arm) -> &Experiment {
        &self.layers[arm.layer].experiments[arm.experiment]
    }

    /// The experiments a search for `query` is in, from a browser with
    /// history profile `profile` if it has one.
    pub fn assign(&self, query: &str, profile: Option<&str>) -> Vec<Arm> {
        let words = crate::learn::query_key(query);
        let mut arms = Vec::new();
        for (l, layer) in self.layers.iter().enumerate() {
            let unit = match layer.diversion {
                Diversion::Query if !words.is_empty() => words.as_str(),
                Diversion::Browser => match profile {
                    Some(profile) => profile,
                    None => continue,
                },
                _ => continue,
            };
            let bucket = bucket(&layer.name, unit);
            if let Some(e) = layer
                .experiments
                .iter()
                .position(|e| (e.from..e.to).contains(&bucket))
            {
                arms.push(Arm {
                    layer: l,
                    experiment: e,
                });
            }
        }
        arms
    }

    /// `base` with the knobs the experiments `arms` change.
    pub fn rank(&self, base: RankConfig, arms: &[Arm]) -> RankConfig {
        arms.iter().fold(base, |rank, &arm| {
            // Checked when set up.
            overlay(rank, &self.experiment(arm).rank).unwrap_or(rank)
        })
    }

    /// Counts a search in the experiments `arms`, and returns the token
    /// its result links carry; `None` when it is in none.
    pub fn note_search(&self, arms: &[Arm]) -> Option<u64> {
        if arms.is_empty() {
            return None;
        }
        let mut token = [0u8; 8];
        getrandom::fill(&mut token).ok()?;
        let token = u64::from_le_bytes(token);
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        for &arm in arms {
            let id = self.experiment(arm).id.clone();
            live.results.entry(id).or_default().searches += 1;
        }
        live.pages.insert(
            token,
            Page {
                arms: arms.to_vec(),
                clicked: false,
                opened: Vec::new(),
            },
        );
        live.order.push_back(token);
        while live.order.len() > MAX_PAGES {
            if let Some(old) = live.order.pop_front() {
                live.pages.remove(&old);
            }
        }
        self.save(&live);
        Some(token)
    }

    /// Counts the result at place `k` (0 for the first) opened from the
    /// page `token`; a page not shown lately counts nothing.
    pub fn note_click(&self, token: u64, k: usize) {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(page) = live.pages.get_mut(&token) else {
            return;
        };
        if page.opened.contains(&k) {
            return;
        }
        page.opened.push(k);
        let first = !page.clicked;
        page.clicked = true;
        let arms = page.arms.clone();
        for arm in arms {
            let id = self.experiment(arm).id.clone();
            let counts = live.results.entry(id).or_default();
            counts.clicks += 1;
            if k < PLACES {
                if counts.at.len() < PLACES {
                    counts.at.resize(PLACES, 0);
                }
                counts.at[k] += 1;
            }
            if first {
                let rr = 1.0 / (k + 1) as f64;
                counts.clicked += 1;
                counts.rr += rr;
                counts.rr_sq += rr * rr;
            }
        }
        self.save(&live);
    }

    fn save(&self, live: &Live) {
        let Some(path) = &self.results_path else {
            return;
        };
        let written = serde_json::to_vec(&live.results)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| crate::node::store::write_atomically(path, &bytes));
        if let Err(err) = written {
            tracing::warn!("could not save the experiments' results: {err:#}");
        }
    }

    /// What each experiment's searches did so far.
    pub fn counts(&self, experiment: &Experiment) -> Counts {
        let live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        live.results
            .get(&experiment.id)
            .cloned()
            .unwrap_or_default()
    }

    /// Each experiment compared with its layer's control, as text.
    pub fn report(&self) -> String {
        let mut out = String::new();
        for layer in &self.layers {
            let _ = writeln!(
                out,
                "layer {} (by {})",
                layer.name,
                match layer.diversion {
                    Diversion::Query => "query",
                    Diversion::Browser => "browser",
                }
            );
            let control = layer.control();
            let base = control.map(|c| self.counts(c));
            for experiment in &layer.experiments {
                let counts = self.counts(experiment);
                let _ = writeln!(
                    out,
                    "  {:<24} {:>5.1}%  {:>7} searches  opened {:>5.1}%  first opened at 1/{:.2}{}",
                    experiment.name,
                    f64::from(experiment.to - experiment.from) * 100.0 / f64::from(BUCKETS),
                    counts.searches,
                    counts.click_rate() * 100.0,
                    if counts.mean_rr() > 0.0 {
                        1.0 / counts.mean_rr()
                    } else {
                        f64::INFINITY
                    },
                    if experiment.rank.is_empty() {
                        String::new()
                    } else {
                        format!("  {}", Value::Object(experiment.rank.clone()))
                    }
                );
                let (Some(control), Some(base)) = (control, &base) else {
                    continue;
                };
                if control.id == experiment.id {
                    continue;
                }
                match compare(base, &counts) {
                    None => {
                        let _ = writeln!(
                            out,
                            "    vs {}: too few searches ({MIN_SEARCHES} each needed)",
                            control.name
                        );
                    }
                    Some(c) => {
                        let _ = writeln!(
                            out,
                            "    vs {}: opened {}, first-opened place {}",
                            control.name,
                            c.click_rate.describe(100.0, " points"),
                            c.mean_rr.describe(1.0, ""),
                        );
                    }
                }
            }
        }
        out
    }
}

/// A difference with its 95% confidence interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Difference {
    pub diff: f64,
    pub low: f64,
    pub high: f64,
}

impl Difference {
    fn of(diff: f64, se: f64) -> Self {
        Difference {
            diff,
            low: diff - 1.96 * se,
            high: diff + 1.96 * se,
        }
    }

    /// Whether the interval leaves out 0.
    pub fn clear(&self) -> bool {
        self.low > 0.0 || self.high < 0.0
    }

    fn describe(&self, scale: f64, unit: &str) -> String {
        let verdict = if !self.clear() {
            "no clear difference"
        } else if self.diff > 0.0 {
            "better"
        } else {
            "worse"
        };
        format!(
            "{:+.3}{unit} [{:+.3}, {:+.3}] {verdict}",
            self.diff * scale,
            self.low * scale,
            self.high * scale
        )
    }
}

/// An experiment against its control.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Comparison {
    /// The share of searches with a result opened.
    pub click_rate: Difference,
    /// The mean reciprocal place of the first result opened.
    pub mean_rr: Difference,
}

/// `treatment` against `control`; `None` until each has
/// [`MIN_SEARCHES`].
pub fn compare(control: &Counts, treatment: &Counts) -> Option<Comparison> {
    if control.searches < MIN_SEARCHES || treatment.searches < MIN_SEARCHES {
        return None;
    }
    let (n0, n1) = (control.searches as f64, treatment.searches as f64);
    let (p0, p1) = (control.click_rate(), treatment.click_rate());
    let rate_se = (p0 * (1.0 - p0) / n0 + p1 * (1.0 - p1) / n1).sqrt();
    let variance = |c: &Counts| {
        let mean = c.mean_rr();
        (ratio(c.rr_sq, c.searches) - mean * mean).max(0.0)
    };
    let rr_se = (variance(control) / n0 + variance(treatment) / n1).sqrt();
    Some(Comparison {
        click_rate: Difference::of(p1 - p0, rate_se),
        mean_rr: Difference::of(treatment.mean_rr() - control.mean_rr(), rr_se),
    })
}

/// `base` with the knobs in `changes`.
fn overlay(base: RankConfig, changes: &Map<String, Value>) -> Result<RankConfig> {
    if changes.is_empty() {
        return Ok(base);
    }
    let mut value = serde_json::to_value(base)?;
    if let Value::Object(map) = &mut value {
        for (key, change) in changes {
            map.insert(key.clone(), change.clone());
        }
    }
    Ok(serde_json::from_value(value)?)
}

/// The bucket `unit` falls in, in layer `layer`.
fn bucket(layer: &str, unit: &str) -> u32 {
    let digest = Sha256::digest(format!("{layer}\0{unit}").as_bytes());
    let n = u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"));
    (n % u64::from(BUCKETS)) as u32
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `plumb experiments`: prints how a node's experiments are doing.
pub fn run(args: &crate::cli::ExperimentsArgs) -> Result<()> {
    match Lab::in_dir(&args.data)? {
        Some(lab) => print!("{}", lab.report()),
        None => println!(
            "No experiments: {} sets up none.",
            args.data.join(CONFIG_FILE).display()
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clicks::sim::{clicks, Rng};

    const TWO_ARMS: &str = r#"{"layers": [{"name": "ranking", "experiments": [
        {"name": "control", "percent": 50},
        {"name": "treatment", "percent": 50, "rank": {"alpha": 0.5}}
    ]}]}"#;

    #[test]
    fn set_ups_that_cannot_work_are_refused() {
        for (bad, says) in [
            (
                r#"{"layers": [{"name": "a", "experiments": [{"name": "x", "percent": 60}, {"name": "y", "percent": 60}]}]}"#,
                "more than 100%",
            ),
            (
                r#"{"layers": [{"name": "a", "experiments": [{"name": "x", "percent": 10, "rank": {"alpah": 1}}]}]}"#,
                "no ranking knob",
            ),
            (
                r#"{"layers": [{"name": "a", "experiments": [{"name": "x", "percent": 10, "rank": {"alpha": "high"}}]}]}"#,
                "bad ranking knobs",
            ),
            (
                r#"{"layers": [{"name": "a", "experiments": [{"name": "x", "percent": 10, "rank": {"alpha": 1}}]},
                            {"name": "b", "experiments": [{"name": "y", "percent": 10, "rank": {"alpha": 2}}]}]}"#,
                "belongs to one layer",
            ),
            (
                r#"{"layers": [{"name": "a", "experiments": [{"name": "x", "percent": 10}, {"name": "x", "percent": 10}]}]}"#,
                "two experiments",
            ),
            (
                r#"{"layers": [{"name": "a", "experiments": [{"name": "x", "percent": 0}]}]}"#,
                "above 0",
            ),
            (
                r#"{"layers": [{"name": "a", "diversion": "cookie", "experiments": []}]}"#,
                "unknown variant",
            ),
        ] {
            let err = format!("{:#}", Lab::parse(bad).unwrap_err());
            assert!(err.contains(says), "{bad}: {err}");
        }
    }

    #[test]
    fn an_experiment_changes_only_its_knobs() {
        let lab = Lab::parse(TWO_ARMS).unwrap();
        let base = RankConfig::default();
        let treated = (0..200)
            .map(|i| format!("query {i}"))
            .find_map(|q| {
                let arms = lab.assign(&q, None);
                (lab.experiment(arms[0]).name == "treatment").then_some(arms)
            })
            .unwrap();
        let rank = lab.rank(base, &treated);
        assert_eq!(rank.alpha, 0.5);
        assert_eq!(
            RankConfig {
                alpha: base.alpha,
                ..rank
            },
            base
        );
    }

    #[test]
    fn a_search_falls_the_same_way_every_time_and_layers_overlap_independently() {
        let lab = Lab::parse(
            r#"{"layers": [
                {"name": "one", "experiments": [{"name": "a", "percent": 50}, {"name": "b", "percent": 50}]},
                {"name": "two", "experiments": [{"name": "c", "percent": 50}, {"name": "d", "percent": 50}]},
                {"name": "people", "diversion": "browser", "experiments": [{"name": "e", "percent": 100}]}
            ]}"#,
        )
        .unwrap();
        let mut both = HashMap::new();
        for i in 0..4_000 {
            let query = format!("search number {i}");
            let arms = lab.assign(&query, None);
            assert_eq!(arms, lab.assign(&format!("  Search  number {i} "), None));
            // No profile: not in the browser layer.
            assert_eq!(arms.len(), 2);
            *both
                .entry((arms[0].experiment, arms[1].experiment))
                .or_insert(0) += 1;
        }
        // Each pair about a quarter of searches: what one layer picks says
        // nothing of the other.
        for (pair, n) in &both {
            assert!((850..1150).contains(n), "{pair:?}: {n} of 4000");
        }
        assert_eq!(lab.assign("anything", Some("abc")).len(), 3);
        // Partly covered layers leave the rest of the searches alone.
        let lab = Lab::parse(
            r#"{"layers": [{"name": "x", "experiments": [{"name": "a", "percent": 10}]}]}"#,
        )
        .unwrap();
        let inside = (0..2_000)
            .filter(|i| !lab.assign(&format!("q{i}"), None).is_empty())
            .count();
        assert!((140..260).contains(&inside), "{inside}");
    }

    #[test]
    fn clicks_count_once_per_place_and_only_on_pages_shown() {
        let lab = Lab::parse(TWO_ARMS).unwrap();
        let arms = lab.assign("us bank", None);
        let token = lab.note_search(&arms).unwrap();
        lab.note_click(token, 2);
        lab.note_click(token, 2);
        lab.note_click(token, 0);
        lab.note_click(token ^ 1, 0);
        let counts = lab.counts(lab.experiment(arms[0]));
        assert_eq!(counts.searches, 1);
        assert_eq!(counts.clicked, 1);
        assert_eq!(counts.clicks, 2);
        assert!((counts.rr - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(counts.at[..3], [1, 0, 1]);
        assert_eq!(lab.note_search(&[]), None);
    }

    #[test]
    fn results_are_kept_in_the_data_directory_and_only_counts() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Lab::in_dir(dir.path()).unwrap().is_none());
        fs::write(dir.path().join(CONFIG_FILE), TWO_ARMS).unwrap();
        let lab = Lab::in_dir(dir.path()).unwrap().unwrap();
        let arms = lab.assign("my secret search", None);
        let token = lab.note_search(&arms).unwrap();
        lab.note_click(token, 0);
        let text = fs::read_to_string(dir.path().join(RESULTS_FILE)).unwrap();
        assert!(!text.contains("secret"), "{text}");
        let again = Lab::in_dir(dir.path()).unwrap().unwrap();
        assert_eq!(again.counts(again.experiment(arms[0])).clicked, 1);
        // Set up anew, an experiment starts from nothing.
        fs::write(dir.path().join(CONFIG_FILE), TWO_ARMS.replace("0.5", "0.6")).unwrap();
        let changed = Lab::in_dir(dir.path()).unwrap().unwrap();
        let treatment = &changed.layers()[0].experiments[1];
        assert_eq!(changed.counts(treatment), Counts::default());
    }

    /// Simulated searchers on a simulated ranking: the treatment puts the
    /// sites they want in a better order than the control does.
    fn simulate(lab: &Lab, rng: &mut Rng, searches: usize, better: &str) {
        let wanted: Vec<Vec<f64>> = (0..300)
            .map(|_| (0..PLACES).map(|_| rng.unit() * 0.6).collect())
            .collect();
        for _ in 0..searches {
            let s = rng.below(wanted.len());
            let arms = lab.assign(&format!("search {s}"), None);
            let Some(token) = lab.note_search(&arms) else {
                continue;
            };
            let noise = if lab.experiment(arms[0]).name == better {
                0.15
            } else {
                0.6
            };
            let mut order: Vec<(f64, f64)> = wanted[s]
                .iter()
                .map(|&w| (w + rng.unit() * noise, w))
                .collect();
            order.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (k, &(_, w)) in order.iter().enumerate() {
                if clicks(rng, k, w) {
                    lab.note_click(token, k);
                }
            }
        }
    }

    #[test]
    fn a_better_ranking_shows_and_a_copy_of_the_control_does_not() {
        let lab = Lab::parse(TWO_ARMS).unwrap();
        simulate(&lab, &mut Rng::new(3), 20_000, "treatment");
        let layer = &lab.layers()[0];
        let c = compare(
            &lab.counts(&layer.experiments[0]),
            &lab.counts(&layer.experiments[1]),
        )
        .unwrap();
        assert!(c.mean_rr.clear() && c.mean_rr.diff > 0.0, "{c:?}");
        let report = lab.report();
        assert!(report.contains("vs control"), "{report}");
        assert!(report.contains("better"), "{report}");

        // An A/A test: the same ranking on both sides.
        let lab = Lab::parse(
            r#"{"layers": [{"name": "aa", "experiments": [
                {"name": "control", "percent": 50},
                {"name": "copy", "percent": 50}
            ]}]}"#,
        )
        .unwrap();
        simulate(&lab, &mut Rng::new(5), 20_000, "neither");
        let layer = &lab.layers()[0];
        let c = compare(
            &lab.counts(&layer.experiments[0]),
            &lab.counts(&layer.experiments[1]),
        )
        .unwrap();
        assert!(c.mean_rr.diff.abs() < 0.03, "{c:?}");
        assert!(c.click_rate.diff.abs() < 0.03, "{c:?}");
    }

    #[test]
    fn few_searches_are_not_compared() {
        let lab = Lab::parse(TWO_ARMS).unwrap();
        simulate(&lab, &mut Rng::new(9), 50, "treatment");
        assert!(
            lab.report().contains("too few searches"),
            "{}",
            lab.report()
        );
    }
}
