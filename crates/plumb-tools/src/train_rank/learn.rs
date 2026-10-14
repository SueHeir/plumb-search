//! How `train-rank` trains a [`Model`]: trees grown as in LambdaMART, or
//! a small neural network ([`net`]), on the labelled or graded rows of
//! the test searches.

use plumb_index::learned::{features, Model, Node, RowSignals, FEATURES, LEARNED_ROWS};
use serde::Deserialize;

mod net;

/// One test search for [`train`]: its query and listed rows, labelled.
#[derive(Debug, Clone, Deserialize)]
pub struct TrainingQuery {
    /// What was searched (after following a spelling suggestion).
    pub searched: String,
    pub rows: Vec<RowSignals>,
}

/// The kind of model [`train`] makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Learner {
    /// Gradient-boosted trees (LambdaMART with [`Objective::LambdaRank`]).
    Trees,
    /// A small neural network ([`plumb_index::learned::Net`]).
    Net,
}

/// What [`train`] makes the scores of each search better at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Objective {
    /// Pairs of rows weighted by how much swapping them changes NDCG, as
    /// LightGBM's `lambdarank` does.
    LambdaRank,
    /// LambdaLoss's NDCG-Loss2++ (Wang et al. 2018): pairs weighted by a
    /// bound on NDCG that, unlike LambdaRank's, is a loss the training
    /// really goes down on.
    LambdaLoss,
    /// Softmax cross entropy over each search's rows: the share of the
    /// search's score the labelled rows get.
    Softmax,
}

/// How [`train`] makes its model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrainOptions {
    pub learner: Learner,
    pub objective: Objective,
    pub trees: usize,
    pub leaves: usize,
    pub min_rows_in_leaf: usize,
    pub learning_rate: f64,
    /// Hidden layers of a [`Learner::Net`]; 0 is a linear model.
    pub layers: usize,
    /// Units in each hidden layer.
    pub width: usize,
    /// Passes over the searches a net is trained for.
    pub epochs: usize,
    /// Standard deviation of the noise added to a net's inputs while it
    /// trains, after they are scaled.
    pub noise: f64,
    /// Seed of a net's starting weights and noise.
    pub seed: u64,
}

impl Default for TrainOptions {
    fn default() -> Self {
        TrainOptions {
            learner: Learner::Trees,
            objective: Objective::LambdaRank,
            trees: 200,
            leaves: 7,
            min_rows_in_leaf: 20,
            learning_rate: 0.05,
            layers: 2,
            width: 32,
            epochs: 60,
            noise: 0.1,
            seed: 1,
        }
    }
}

/// Discount of the row at 0-based `place` in NDCG.
pub fn discount(place: usize) -> f64 {
    1.0 / ((place + 2) as f64).log2()
}

/// What `row` is worth to its search, as NDCG's gain `2^grade - 1`: its
/// LLM grade, an expected answer counting as the best grade, 3.
pub fn gain(row: &RowSignals) -> f64 {
    let grade = row.grade.unwrap_or(0).min(3).max(3 * row.label.min(1));
    f64::from((1u8 << grade) - 1)
}

/// The DCG of `gains` in their best order.
pub fn ideal_dcg(gains: &[f64]) -> f64 {
    let mut sorted = gains.to_vec();
    sorted.sort_by(|a, b| b.total_cmp(a));
    sorted
        .iter()
        .enumerate()
        .map(|(i, g)| g * discount(i))
        .sum()
}

/// The training rows of `queries`: every query's first
/// [`LEARNED_ROWS`] rows end to end, their [`gain`]s, and where each
/// query's rows start and end. Queries whose rows there are all worth
/// the same are left out.
type Rows = (Vec<[f64; FEATURES.len()]>, Vec<f64>, Vec<(usize, usize)>);

fn training_rows(queries: &[TrainingQuery]) -> Rows {
    let mut x: Vec<[f64; FEATURES.len()]> = Vec::new();
    let mut labels: Vec<f64> = Vec::new();
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for q in queries {
        let rows = &q.rows[..q.rows.len().min(LEARNED_ROWS)];
        if rows.iter().all(|r| gain(r) == gain(&rows[0])) {
            continue;
        }
        let start = x.len();
        x.extend(features(&q.searched, rows));
        labels.extend(rows.iter().map(gain));
        groups.push((start, x.len()));
    }
    (x, labels, groups)
}

/// Trains a model on `queries`. Trees are grown as in LambdaMART: each
/// tree fits how much each row should move up or down to put the
/// labelled rows first, by `options.objective`. Deterministic: the same
/// queries and options give the same model.
pub fn train(queries: &[TrainingQuery], options: TrainOptions) -> Model {
    let (x, labels, groups) = training_rows(queries);
    if options.learner == Learner::Net {
        return Model::new(Vec::new(), Some(net::train(&x, &labels, &groups, options)));
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
        let (grad, hess) = gradients(options.objective, &scores, &labels, &groups);
        let tree = grow(&x, &sorted, &grad, &hess, options);
        for (i, row) in x.iter().enumerate() {
            scores[i] += tree.value(row);
        }
        trees.push(tree);
    }
    Model::new(trees, None)
}

/// How the loss of `objective` changes with every row's score: first and
/// second derivatives.
fn gradients(
    objective: Objective,
    scores: &[f64],
    labels: &[f64],
    groups: &[(usize, usize)],
) -> (Vec<f64>, Vec<f64>) {
    match objective {
        Objective::LambdaRank => lambdas(scores, labels, groups),
        Objective::LambdaLoss => lambda_loss(scores, labels, groups),
        Objective::Softmax => softmax(scores, labels, groups),
    }
}

/// Weight of the NDCG-Loss2++ pairs' second term (the paper's μ).
const LAMBDA_LOSS_MU: f64 = 5.0;

/// LambdaLoss's NDCG-Loss2++ (Wang et al. 2018, eq. 13 and 16): for every
/// pair of a labelled and an unlabelled row of a query, a logistic loss
/// on their score difference weighted by how far apart their places are
/// in discount (ρ) plus μ times how much one place apart costs at that
/// distance (δ), over the best DCG.
fn lambda_loss(scores: &[f64], labels: &[f64], groups: &[(usize, usize)]) -> (Vec<f64>, Vec<f64>) {
    let mut grad = vec![0.0; scores.len()];
    let mut hess = vec![0.0; scores.len()];
    for &(start, end) in groups {
        let place = places(scores, start, end);
        let best = ideal_dcg(&labels[start..end]);
        if best <= 0.0 {
            continue;
        }
        for hi in start..end {
            for lo in start..end {
                if labels[hi] <= labels[lo] {
                    continue;
                }
                let (p_hi, p_lo) = (place[hi - start], place[lo - start]);
                let rho = (discount(p_hi) - discount(p_lo)).abs();
                // |i - j| places apart, 1-based: 1/D(|i-j|) - 1/D(|i-j|+1).
                let apart = p_hi.abs_diff(p_lo);
                let delta = discount(apart - 1) - discount(apart);
                let weight = (labels[hi] - labels[lo]) * (rho + LAMBDA_LOSS_MU * delta) / best;
                let p = 1.0 / (1.0 + (scores[hi] - scores[lo]).exp());
                grad[hi] -= weight * p;
                grad[lo] += weight * p;
                let h = weight * p * (1.0 - p);
                hess[hi] += h;
                hess[lo] += h;
            }
        }
    }
    (grad, hess)
}

/// Softmax cross entropy of every query: its rows' scores as a softmax
/// against its labels, shared out.
fn softmax(scores: &[f64], labels: &[f64], groups: &[(usize, usize)]) -> (Vec<f64>, Vec<f64>) {
    let mut grad = vec![0.0; scores.len()];
    let mut hess = vec![0.0; scores.len()];
    for &(start, end) in groups {
        let total: f64 = labels[start..end].iter().sum();
        if total <= 0.0 {
            continue;
        }
        let top = scores[start..end].iter().copied().fold(f64::MIN, f64::max);
        let sum: f64 = scores[start..end].iter().map(|s| (s - top).exp()).sum();
        for i in start..end {
            let p = (scores[i] - top).exp() / sum;
            grad[i] = p - labels[i] / total;
            hess[i] = p * (1.0 - p);
        }
    }
    (grad, hess)
}

/// The 0-based place of each row of `start..end` when listed by score.
fn places(scores: &[f64], start: usize, end: usize) -> Vec<usize> {
    let mut by_score: Vec<usize> = (start..end).collect();
    by_score.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    let mut place = vec![0; end - start];
    for (p, &i) in by_score.iter().enumerate() {
        place[i - start] = p;
    }
    place
}

/// LambdaRank gradients and second derivatives of every row: for each
/// pair of a labelled and an unlabelled row of a query, one of them in
/// the first [`LEARNED_ROWS`] by the current scores, how much swapping
/// them would change NDCG.
fn lambdas(scores: &[f64], labels: &[f64], groups: &[(usize, usize)]) -> (Vec<f64>, Vec<f64>) {
    let mut grad = vec![0.0; scores.len()];
    let mut hess = vec![0.0; scores.len()];
    for &(start, end) in groups {
        let place = places(scores, start, end);
        let best = ideal_dcg(&labels[start..end]);
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
    use plumb_core::article::Article;
    use plumb_index::pages::{Page, PageHit};
    use plumb_index::Hit;

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
            missing_words: false,
            query_evidence: None,
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
    fn every_objective_and_learner_learns() {
        for (learner, objective) in [
            (Learner::Trees, Objective::LambdaLoss),
            (Learner::Trees, Objective::Softmax),
            (Learner::Net, Objective::Softmax),
            (Learner::Net, Objective::LambdaLoss),
        ] {
            let options = TrainOptions {
                learner,
                objective,
                ..TrainOptions::default()
            };
            let model = train(&training(), options);
            for (i, q) in training().iter().enumerate() {
                let order = model.order(&q.searched, &q.rows);
                assert_eq!(
                    q.rows[order[0]].label, 1,
                    "{learner:?} {objective:?} query {i}: {order:?}"
                );
            }
            assert_eq!(model, train(&training(), options));
            let read = Model::from_json(&serde_json::to_string(&model).unwrap()).unwrap();
            for q in training() {
                for x in features(&q.searched, &q.rows) {
                    assert!((read.score(&x) - model.score(&x)).abs() < 1e-9);
                }
            }
        }
    }
}
