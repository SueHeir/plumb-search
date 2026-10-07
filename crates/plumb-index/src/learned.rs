//! Learned ranking: a small model, trained on the test searches in
//! `eval/`, that puts the first results of a search in a better order.
//!
//! The hand-made ranking ([`crate::Searcher`] and
//! [`crate::pages::place_pages`]) lists sites and pages. The model reads
//! the first [`LEARNED_ROWS`] of them, each with the signals that ranked it
//! (its place, score, text match, link score, whether it is named, the
//! pages under a site), and scores every row. The rows are then listed by
//! that score. It is gradient-boosted trees trained with LambdaMART
//! ([`train`]), which learns from which row each test search expects
//! first; no search logs, only the labelled test searches.

use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::pages::{LearnedPlace, PageHit, PlacedPage};
use crate::Hit;

/// How many of the first listed rows the model puts in order.
pub const LEARNED_ROWS: usize = 10;

/// Page sets the model tells apart, by the start of their name
/// ([`crate::pages::Page::set`]).
const SETS: [&str; 8] = [
    "wikipedia",
    "stackoverflow",
    "stackexchange",
    "github",
    "books",
    "papers",
    "packages",
    "podcasts",
];

/// The signals of one page, as the model reads them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PageSignals {
    pub set: String,
    pub score: f32,
    pub named: bool,
    pub popularity: f32,
    pub whole: bool,
}

impl PageSignals {
    fn of(hit: &PageHit) -> Self {
        PageSignals {
            set: hit.page.set.clone(),
            score: hit.score,
            named: hit.named,
            popularity: hit.popularity,
            whole: hit.whole,
        }
    }
}

/// One listed row, as the model reads it: a site with the pages shown
/// under it, or a page listed on its own. `plumb eval --features-out`
/// writes rows in this shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RowSignals {
    /// `site` or `page`.
    pub kind: String,
    /// A page's set; empty for a site.
    pub set: String,
    pub score: f32,
    /// A site's text match.
    pub text_score: f32,
    /// A site's text match without the search instruction, when the
    /// meaning of the query was found with one ([`Hit::placing_text_score`]).
    pub placing_text_score: Option<f32>,
    pub link_score: f32,
    pub named: bool,
    pub official: bool,
    pub demand: Option<f32>,
    /// A site's country, `None` for global sites.
    pub country: Option<String>,
    pub title: Option<String>,
    /// A page's popularity.
    pub popularity: f32,
    /// A page the query asks for in full.
    pub whole: bool,
    /// Pages shown under a site.
    pub under: Vec<PageSignals>,
    /// In training data: 1 when the row holds an expected answer.
    pub label: u8,
}

impl RowSignals {
    /// The row of a site result and the pages under it.
    pub fn site(hit: &Hit, under: &[&PageHit]) -> Self {
        RowSignals {
            kind: "site".into(),
            score: hit.score,
            text_score: hit.text_score,
            placing_text_score: hit.placing_text_score,
            link_score: hit.link_score,
            named: hit.named,
            official: hit.official,
            demand: hit.demand,
            country: hit.country.clone(),
            title: hit.title.clone(),
            under: under.iter().map(|p| PageSignals::of(p)).collect(),
            ..RowSignals::default()
        }
    }

    /// The row of a page listed on its own.
    pub fn page(hit: &PageHit) -> Self {
        RowSignals {
            kind: "page".into(),
            set: hit.page.set.clone(),
            score: hit.score,
            named: hit.named,
            popularity: hit.popularity,
            whole: hit.whole,
            ..RowSignals::default()
        }
    }

    fn is_page(&self) -> bool {
        self.kind == "page"
    }
}

/// The names of the values [`features`] gives each row, in order.
pub const FEATURES: [&str; 31] = [
    "pos_inv",
    "pos",
    "nwords",
    "question",
    "is_page",
    "set_wikipedia",
    "set_stackoverflow",
    "set_stackexchange",
    "set_github",
    "set_books",
    "set_papers",
    "set_packages",
    "set_podcasts",
    "p_score",
    "p_named",
    "p_pop",
    "p_whole",
    "s_score",
    "s_score_gap",
    "s_text",
    "s_link",
    "s_link_gap",
    "s_named",
    "s_official",
    "s_demand",
    "s_country",
    "s_under",
    "s_under_named",
    "s_under_pop",
    "s_has_title",
    "s_text_plain",
];

/// What the model reads of each of `rows`, listed in this order for
/// `query`: one value per name of [`FEATURES`]. Values a row does not
/// have (a page's text match) are 0.
pub fn features(query: &str, rows: &[RowSignals]) -> Vec<[f64; FEATURES.len()]> {
    let words = query.split_whitespace().count() as f64;
    let question = f64::from(u8::from(crate::pages::asked_as_question(query)));
    let sites = || rows.iter().filter(|r| !r.is_page());
    let best_score = sites().map(|r| f64::from(r.score)).fold(0.0, f64::max);
    let best_link = sites().map(|r| f64::from(r.link_score)).fold(0.0, f64::max);
    let flag = |b: bool| f64::from(u8::from(b));
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let mut f = [0.0; FEATURES.len()];
            let pos = (i + 1) as f64;
            f[0] = 1.0 / pos;
            f[1] = pos;
            f[2] = words;
            f[3] = question;
            f[4] = flag(row.is_page());
            if row.is_page() {
                if let Some(set) = SETS.iter().position(|s| row.set.starts_with(s)) {
                    f[5 + set] = 1.0;
                }
                f[13] = f64::from(row.score);
                f[14] = flag(row.named);
                f[15] = f64::from(row.popularity);
                f[16] = flag(row.whole);
            } else {
                let score = f64::from(row.score);
                let link = f64::from(row.link_score);
                f[17] = score;
                f[18] = best_score - score;
                f[19] = f64::from(row.text_score);
                f[20] = link;
                f[21] = best_link - link;
                f[22] = flag(row.named);
                f[23] = flag(row.official);
                f[24] = row.demand.map_or(-1.0, f64::from);
                f[25] = flag(row.country.is_some());
                f[26] = row.under.len() as f64;
                f[27] = flag(row.under.iter().any(|p| p.named));
                f[28] = row
                    .under
                    .iter()
                    .map(|p| f64::from(p.popularity))
                    .fold(-1.0, f64::max);
                let titled = row.title.as_deref().is_some_and(|t| !t.trim().is_empty());
                f[29] = flag(titled);
                f[30] = f64::from(row.placing_text_score.unwrap_or(row.text_score));
            }
            f
        })
        .collect()
}

/// One regression tree: `x[feature] <= threshold` goes left.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum Node {
    Leaf {
        value: f64,
    },
    Split {
        feature: usize,
        threshold: f64,
        left: Box<Node>,
        right: Box<Node>,
    },
}

impl Node {
    fn value(&self, x: &[f64]) -> f64 {
        match self {
            Node::Leaf { value } => *value,
            Node::Split {
                feature,
                threshold,
                left,
                right,
            } => {
                if x[*feature] <= *threshold {
                    left.value(x)
                } else {
                    right.value(x)
                }
            }
        }
    }
}

/// A learned ranking: trees whose values add up to a row's score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    /// [`FEATURES`] when the model was trained, to refuse a model made
    /// for other features.
    pub features: Vec<String>,
    trees: Vec<Node>,
}

impl Model {
    /// The model in `json` (as [`train`] writes it).
    pub fn from_json(json: &str) -> Result<Self> {
        let model: Model = serde_json::from_str(json).context("reading a ranking model")?;
        if model.features != FEATURES {
            bail!("the ranking model was made for other features");
        }
        Ok(model)
    }

    /// The model every node ranks with, trained on `eval/`
    /// (`plumb train-rank`).
    pub fn builtin() -> &'static Model {
        static MODEL: OnceLock<Model> = OnceLock::new();
        MODEL.get_or_init(|| {
            Model::from_json(include_str!("learned_model.json"))
                .expect("the built-in ranking model reads")
        })
    }

    /// The score of a row with features `x`; higher comes first.
    pub fn score(&self, x: &[f64]) -> f64 {
        self.trees.iter().map(|tree| tree.value(x)).sum()
    }

    /// The order the model lists `rows` of `query` in: indexes into
    /// `rows`, best first. Only the first [`LEARNED_ROWS`] move; ties
    /// keep their order.
    pub fn order(&self, query: &str, rows: &[RowSignals]) -> Vec<usize> {
        let top = rows.len().min(LEARNED_ROWS);
        let scores: Vec<f64> = features(query, &rows[..top])
            .iter()
            .map(|x| self.score(x))
            .collect();
        let mut order: Vec<usize> = (0..top).collect();
        order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
        order.extend(top..rows.len());
        order
    }
}

/// One row as listed: a site (by index into the hits) or a page listed
/// on its own (by index into the placed pages).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Listed {
    Site(usize),
    Page(usize),
}

/// The rows `hits` and `placed` list, in order, as
/// [`crate::pages::place_pages`] places them: pages on their own before
/// the site at their `at`, after all sites when past the end.
fn listed(hits: &[Hit], placed: &[PlacedPage]) -> Vec<Listed> {
    let mut rows = Vec::with_capacity(hits.len() + placed.len());
    let alone = |at: usize| {
        placed
            .iter()
            .enumerate()
            .filter(move |(_, p)| p.under.is_none() && p.at == at)
            .map(|(i, _)| Listed::Page(i))
    };
    for i in 0..hits.len() {
        rows.extend(alone(i));
        rows.push(Listed::Site(i));
    }
    rows.extend(
        placed
            .iter()
            .enumerate()
            .filter(|(_, p)| p.under.is_none() && p.at >= hits.len())
            .map(|(i, _)| Listed::Page(i)),
    );
    rows
}

/// Puts the first rows of `query`'s results in the order `model` gives:
/// reorders `hits` and moves the pages listed on their own in `placed`.
/// Pages under a site stay under it.
///
/// The moved sites trade scores so that scores still go down the list:
/// what a node adds later (a browser's own likes, results from the
/// network) sorts by score and keeps this order. Each page listed on its
/// own notes where it went ([`PageHit::learned`]), which
/// [`crate::pages::place_pages`] keeps when the results are placed again.
pub fn reorder(model: &Model, query: &str, hits: &mut Vec<Hit>, placed: &mut Vec<PlacedPage>) {
    let rows = listed(hits, placed);
    if rows.len() < 2 {
        return;
    }
    let signals: Vec<RowSignals> = rows
        .iter()
        .map(|row| match *row {
            Listed::Site(i) => {
                let under: Vec<&PageHit> = placed
                    .iter()
                    .filter(|p| p.under.as_deref() == Some(hits[i].domain.as_str()))
                    .map(|p| &p.hit)
                    .collect();
                RowSignals::site(&hits[i], &under)
            }
            Listed::Page(i) => RowSignals::page(&placed[i].hit),
        })
        .collect();
    let order = model.order(query, &signals);
    let mut new_hits: Vec<Hit> = Vec::with_capacity(hits.len());
    let mut alone: Vec<PlacedPage> = Vec::new();
    for &row in &order {
        match rows[row] {
            Listed::Site(i) => new_hits.push(hits[i].clone()),
            Listed::Page(i) => {
                let mut page = placed[i].clone();
                page.at = new_hits.len();
                alone.push(page);
            }
        }
    }
    // The moved sites' scores, highest first, in their new order.
    let moved = new_hits
        .iter()
        .zip(hits.iter())
        .rposition(|(new, old)| new.domain != old.domain)
        .map_or(0, |last| last + 1);
    let mut scores: Vec<f32> = new_hits[..moved].iter().map(|h| h.score).collect();
    scores.sort_by(|a, b| b.total_cmp(a));
    for (hit, score) in new_hits.iter_mut().zip(scores) {
        hit.score = score;
    }
    for page in &mut alone {
        page.hit.learned = Some(match new_hits.get(page.at) {
            Some(site) => LearnedPlace::Before(site.domain.clone()),
            None => LearnedPlace::Last,
        });
    }
    let under = placed.iter().filter(|p| p.under.is_some()).cloned();
    *placed = alone.into_iter().chain(under).collect();
    *hits = new_hits;
}

/// One test search for [`train`]: its query and listed rows, labelled.
#[derive(Debug, Clone, Deserialize)]
pub struct TrainingQuery {
    /// What was searched (after following a spelling suggestion).
    pub searched: String,
    pub rows: Vec<RowSignals>,
}

/// How [`train`] grows its trees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrainOptions {
    pub trees: usize,
    pub leaves: usize,
    pub min_rows_in_leaf: usize,
    pub learning_rate: f64,
}

impl Default for TrainOptions {
    fn default() -> Self {
        TrainOptions {
            trees: 200,
            leaves: 7,
            min_rows_in_leaf: 20,
            learning_rate: 0.05,
        }
    }
}

/// Discount of the row at 0-based `place` in NDCG.
fn discount(place: usize) -> f64 {
    1.0 / ((place + 2) as f64).log2()
}

/// Trains a model on `queries` with LambdaMART: each tree fits how much
/// each row should move up or down to put the labelled rows first, as
/// LightGBM's `lambdarank` does. Deterministic: the same queries give the
/// same model.
pub fn train(queries: &[TrainingQuery], options: TrainOptions) -> Model {
    // Rows of every query, its first LEARNED_ROWS, end to end.
    let mut x: Vec<[f64; FEATURES.len()]> = Vec::new();
    let mut labels: Vec<f64> = Vec::new();
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for q in queries {
        let rows = &q.rows[..q.rows.len().min(LEARNED_ROWS)];
        if !rows.iter().any(|r| r.label > 0) || rows.len() < 2 {
            continue;
        }
        let start = x.len();
        x.extend(features(&q.searched, rows));
        labels.extend(rows.iter().map(|r| f64::from(r.label.min(1))));
        groups.push((start, x.len()));
    }
    let n = x.len();
    // Every feature's row order, sorted by value, for finding splits.
    let sorted: Vec<Vec<usize>> = (0..FEATURES.len())
        .map(|f| {
            let mut order: Vec<usize> = (0..n).collect();
            order.sort_by(|&a, &b| x[a][f].total_cmp(&x[b][f]).then(a.cmp(&b)));
            order
        })
        .collect();
    let mut scores = vec![0.0; n];
    let mut trees = Vec::with_capacity(options.trees);
    for _ in 0..options.trees {
        let (grad, hess) = lambdas(&scores, &labels, &groups);
        let tree = grow(&x, &sorted, &grad, &hess, options);
        for (i, row) in x.iter().enumerate() {
            scores[i] += tree.value(row);
        }
        trees.push(tree);
    }
    Model {
        features: FEATURES.iter().map(|f| f.to_string()).collect(),
        trees,
    }
}

/// LambdaRank gradients and second derivatives of every row: for each
/// pair of a labelled and an unlabelled row of a query, one of them in
/// the first [`LEARNED_ROWS`] by the current scores, how much swapping
/// them would change NDCG.
fn lambdas(scores: &[f64], labels: &[f64], groups: &[(usize, usize)]) -> (Vec<f64>, Vec<f64>) {
    let mut grad = vec![0.0; scores.len()];
    let mut hess = vec![0.0; scores.len()];
    for &(start, end) in groups {
        let mut by_score: Vec<usize> = (start..end).collect();
        by_score.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
        let mut place = vec![0; end - start];
        for (p, &i) in by_score.iter().enumerate() {
            place[i - start] = p;
        }
        let relevant = labels[start..end].iter().filter(|&&l| l > 0.0).count();
        let best: f64 = (0..relevant).map(discount).sum();
        if best <= 0.0 {
            continue;
        }
        let mut sum_lambdas = 0.0;
        for hi in start..end {
            for lo in start..end {
                if labels[hi] <= labels[lo] {
                    continue;
                }
                let (p_hi, p_lo) = (place[hi - start], place[lo - start]);
                if p_hi.min(p_lo) >= LEARNED_ROWS {
                    continue;
                }
                let delta = (labels[hi] - labels[lo]).abs()
                    * (discount(p_hi) - discount(p_lo)).abs()
                    / best;
                let diff = scores[hi] - scores[lo];
                // As LightGBM: pairs already far apart count less.
                let delta = delta / (0.01 + diff.abs());
                let p = 1.0 / (1.0 + diff.exp());
                let lambda = -p * delta;
                let h = p * (1.0 - p) * delta;
                grad[hi] += lambda;
                grad[lo] -= lambda;
                hess[hi] += h;
                hess[lo] += h;
                sum_lambdas -= 2.0 * lambda;
            }
        }
        if sum_lambdas > 0.0 {
            let norm = (1.0 + sum_lambdas).log2() / sum_lambdas;
            for i in start..end {
                grad[i] *= norm;
                hess[i] *= norm;
            }
        }
    }
    (grad, hess)
}

/// The best split of the rows `in_leaf` marks: feature, threshold, gain.
fn best_split(
    x: &[[f64; FEATURES.len()]],
    sorted: &[Vec<usize>],
    in_leaf: &[bool],
    grad: &[f64],
    hess: &[f64],
    min_rows: usize,
) -> Option<(usize, f64, f64)> {
    let (mut g_all, mut h_all, mut count) = (0.0, 0.0, 0);
    for i in 0..x.len() {
        if in_leaf[i] {
            g_all += grad[i];
            h_all += hess[i];
            count += 1;
        }
    }
    if count < 2 * min_rows {
        return None;
    }
    let parent = g_all * g_all / (h_all + HESS_FLOOR);
    let mut best: Option<(usize, f64, f64)> = None;
    for (f, order) in sorted.iter().enumerate() {
        let (mut g_left, mut h_left, mut n_left) = (0.0, 0.0, 0);
        let mut last: Option<usize> = None;
        for &i in order.iter().filter(|&&i| in_leaf[i]) {
            if let Some(prev) = last {
                // A split between two different values only.
                if x[prev][f] < x[i][f] && n_left >= min_rows && count - n_left >= min_rows {
                    let (g_right, h_right) = (g_all - g_left, h_all - h_left);
                    let gain = g_left * g_left / (h_left + HESS_FLOOR)
                        + g_right * g_right / (h_right + HESS_FLOOR)
                        - parent;
                    if gain > best.map_or(MIN_GAIN, |b| b.2) {
                        best = Some((f, (x[prev][f] + x[i][f]) / 2.0, gain));
                    }
                }
            }
            g_left += grad[i];
            h_left += hess[i];
            n_left += 1;
            last = Some(i);
        }
    }
    best
}

/// Added to sums of second derivatives, so a leaf with almost none does
/// not get a huge value.
const HESS_FLOOR: f64 = 1e-3;
/// Least gain a split must bring.
const MIN_GAIN: f64 = 1e-9;

/// Grows one tree leaf by leaf, always splitting the leaf whose best
/// split gains most, up to `options.leaves` leaves.
fn grow(
    x: &[[f64; FEATURES.len()]],
    sorted: &[Vec<usize>],
    grad: &[f64],
    hess: &[f64],
    options: TrainOptions,
) -> Node {
    struct Leaf {
        rows: Vec<bool>,
        split: Option<(usize, f64, f64)>,
        /// Path from the root: true for right.
        path: Vec<bool>,
    }
    let n = x.len();
    let all = vec![true; n];
    let split = best_split(x, sorted, &all, grad, hess, options.min_rows_in_leaf);
    let mut leaves = vec![Leaf {
        rows: all,
        split,
        path: Vec::new(),
    }];
    // The splits made, by path, to build the tree from afterwards.
    let mut splits: Vec<(Vec<bool>, usize, f64)> = Vec::new();
    while leaves.len() < options.leaves {
        let Some(at) = leaves
            .iter()
            .enumerate()
            .filter(|(_, l)| l.split.is_some())
            .max_by(|a, b| {
                let gain = |l: &Leaf| l.split.map_or(0.0, |s| s.2);
                gain(a.1).total_cmp(&gain(b.1)).then(b.0.cmp(&a.0))
            })
            .map(|(i, _)| i)
        else {
            break;
        };
        let leaf = leaves.remove(at);
        let (feature, threshold, _) = leaf.split.expect("filtered on a split");
        splits.push((leaf.path.clone(), feature, threshold));
        let mut left = vec![false; n];
        let mut right = vec![false; n];
        for i in 0..n {
            if leaf.rows[i] {
                if x[i][feature] <= threshold {
                    left[i] = true;
                } else {
                    right[i] = true;
                }
            }
        }
        for (rows, side) in [(left, false), (right, true)] {
            let split = best_split(x, sorted, &rows, grad, hess, options.min_rows_in_leaf);
            let mut path = leaf.path.clone();
            path.push(side);
            leaves.insert(at, Leaf { rows, split, path });
        }
    }
    let value_of = |path: &[bool]| {
        let leaf = leaves
            .iter()
            .find(|l| l.path == path)
            .expect("every path ends in a leaf");
        let (mut g, mut h) = (0.0, 0.0);
        for i in 0..n {
            if leaf.rows[i] {
                g += grad[i];
                h += hess[i];
            }
        }
        -g / (h + HESS_FLOOR) * options.learning_rate
    };
    build(&mut Vec::new(), &splits, &value_of)
}

/// The tree under `path`, from the splits made.
fn build(
    path: &mut Vec<bool>,
    splits: &[(Vec<bool>, usize, f64)],
    value_of: &dyn Fn(&[bool]) -> f64,
) -> Node {
    match splits.iter().find(|(p, _, _)| p == path) {
        None => Node::Leaf {
            value: value_of(path),
        },
        Some(&(_, feature, threshold)) => {
            path.push(false);
            let left = build(path, splits, value_of);
            path.pop();
            path.push(true);
            let right = build(path, splits, value_of);
            path.pop();
            Node::Split {
                feature,
                threshold,
                left: Box::new(left),
                right: Box::new(right),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::{place_pages, Page};
    use plumb_core::article::Article;

    fn site(domain: &str, score: f32, named: bool) -> Hit {
        Hit {
            domain: domain.into(),
            url: format!("https://{domain}/"),
            title: Some(domain.into()),
            description: None,
            score,
            text_score: 1.0,
            link_score: 0.5,
            country: None,
            named,
            official: false,
            key_pages: Vec::new(),
            demand: None,
            placing_text_score: None,
        }
    }

    fn article(title: &str, named: bool) -> PageHit {
        PageHit {
            page: Page::from_article(
                "en",
                Article {
                    title: title.into(),
                    views: 1_000,
                    ..Article::default()
                },
            ),
            score: 0.5,
            named,
            popularity: 0.9,
            whole: false,
            learned: None,
        }
    }

    /// Searches whose answer is the named site, listed second after an
    /// unnamed one, and searches whose answer is a named article listed
    /// last.
    fn training() -> Vec<TrainingQuery> {
        let mut queries = Vec::new();
        for i in 0..60 {
            let mut first = RowSignals::site(&site("first.com", 1.0, false), &[]);
            first.text_score = 0.3;
            let mut named = RowSignals::site(&site("named.com", 0.9, true), &[]);
            named.label = 1;
            let third = RowSignals::site(&site("third.com", 0.8, false), &[]);
            let mut page = RowSignals::page(&article("Thing", i % 2 == 0));
            let rows = if i % 2 == 0 {
                page.label = 1;
                named.label = 0;
                vec![first, named, third, page]
            } else {
                vec![first, named, third, page]
            };
            queries.push(TrainingQuery {
                searched: format!("query {i}"),
                rows,
            });
        }
        queries
    }

    #[test]
    fn learns_to_put_the_labelled_rows_first() {
        let model = train(&training(), TrainOptions::default());
        for (i, q) in training().iter().enumerate() {
            let order = model.order(&q.searched, &q.rows);
            assert_eq!(q.rows[order[0]].label, 1, "query {i}: {order:?}");
        }
        // The same data trains the same model.
        assert_eq!(model, train(&training(), TrainOptions::default()));
        // It reads back as written, to the last digit or so.
        let json = serde_json::to_string(&model).unwrap();
        let read = Model::from_json(&json).unwrap();
        for q in training() {
            for x in features(&q.searched, &q.rows) {
                assert!((read.score(&x) - model.score(&x)).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn a_model_for_other_features_is_refused() {
        assert!(Model::from_json(r#"{"features":["pos"],"trees":[]}"#).is_err());
    }

    #[test]
    fn the_builtin_model_reads() {
        let model = Model::builtin();
        assert!(!model.trees.is_empty());
    }

    #[test]
    fn reordered_results_stay_put_when_placed_again() {
        let model = train(&training(), TrainOptions::default());
        let query = "query 0";
        let mut hits = vec![
            site("first.com", 1.0, false),
            site("named.com", 0.9, true),
            site("third.com", 0.8, false),
        ];
        let found = vec![article("Thing", true)];
        let mut placed = place_pages(query, &hits, found.clone());
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].under, None);
        reorder(&model, query, &mut hits, &mut placed);
        // The named article leads, before the named site.
        assert_eq!(placed[0].at, 0);
        assert_eq!(
            placed[0].hit.learned,
            Some(LearnedPlace::Before(hits[0].domain.clone()))
        );
        assert_eq!(hits[0].domain, "named.com");
        // Scores still go down the list.
        assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
        // Placing the pages again (as the results page does) keeps it first.
        let again = place_pages(query, &hits, placed.iter().map(|p| p.hit.clone()).collect());
        assert_eq!(again[0].at, 0);
    }

    #[test]
    fn sites_trade_scores_when_they_move() {
        let model = train(&training(), TrainOptions::default());
        let query = "query 1";
        let mut hits = vec![
            site("first.com", 1.0, false),
            site("named.com", 0.9, true),
            site("third.com", 0.8, false),
        ];
        let mut placed = Vec::new();
        reorder(&model, query, &mut hits, &mut placed);
        let domains: Vec<&str> = hits.iter().map(|h| h.domain.as_str()).collect();
        assert_eq!(domains[0], "named.com");
        assert_eq!(hits[0].score, 1.0);
        assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
    }
}
