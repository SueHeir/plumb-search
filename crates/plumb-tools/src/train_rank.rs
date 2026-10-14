//! `plumb-tools train-rank`: trains the learned ranking
//! ([`plumb_index::learned`]) on the test searches `plumb-tools eval
//! --features-out` wrote, and measures it.
//!
//! The model is trained on one half of the searches (`--half tune` by
//! default) and judged on both: the other half is the honest number.
//! Searches marked `"half": "test"` are only judged. For each queries
//! file and half it prints top-1, top-3 and MRR of the expected answer
//! within the first [`LEARNED_ROWS`] rows, and NDCG of the rows' LLM
//! grades there, as listed by the hand-made ranking and as the model
//! puts them.

use std::fmt::Write as _;
use std::io::BufRead;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Args;
use plumb_index::learned::{Model, RowSignals, LEARNED_ROWS};
use serde::Deserialize;
use tracing::info;

use crate::cli::Half;
use crate::eval::Metrics;

mod learn;
use learn::{discount, gain, ideal_dcg, train, Learner, Objective, TrainOptions, TrainingQuery};

#[derive(Debug, Args)]
pub struct TrainRankArgs {
    /// Files `plumb-tools eval --features-out` wrote (gzipped or not). Can be
    /// given more than once.
    #[arg(long, value_name = "JSONL", required = true)]
    pub features: Vec<PathBuf>,
    /// The half of the searches to train on; the other half judges it.
    #[arg(long, value_name = "HALF", default_value = "tune")]
    pub half: Half,
    /// Write the trained model here, as JSON (the built-in one is
    /// crates/plumb-index/src/learned_model.json).
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
    /// Judge this model instead of training one: `builtin` for the one
    /// nodes use, or a model file.
    #[arg(long, value_name = "MODEL", conflicts_with = "out")]
    pub judge: Option<String>,
    /// Also write the order the model gives each search's first rows
    /// here, as JSON lines (`suite`, `half`, `query`, `order`: indexes
    /// into the rows), to look at what it moved.
    #[arg(long, value_name = "JSONL")]
    pub orders_out: Option<PathBuf>,
    /// The kind of model: `trees` or `net`.
    #[arg(long, value_name = "KIND", default_value = "trees")]
    pub learner: LearnerArg,
    /// What training makes better: `lambda-rank`, `lambda-loss`
    /// (NDCG-Loss2++) or `softmax`.
    #[arg(long, value_name = "LOSS", default_value = "lambda-rank")]
    pub objective: ObjectiveArg,
    /// Trees to grow.
    #[arg(long, value_name = "N", default_value_t = TrainOptions::default().trees)]
    pub trees: usize,
    /// Leaves of each tree.
    #[arg(long, value_name = "N", default_value_t = TrainOptions::default().leaves)]
    pub leaves: usize,
    /// Fewest rows in a tree's leaf.
    #[arg(long, value_name = "N", default_value_t = TrainOptions::default().min_rows_in_leaf)]
    pub min_rows: usize,
    /// How much of each tree's values is kept.
    #[arg(long, value_name = "RATE", default_value_t = TrainOptions::default().learning_rate)]
    pub learning_rate: f64,
    /// A net's hidden layers; 0 is a linear model.
    #[arg(long, value_name = "N", default_value_t = TrainOptions::default().layers)]
    pub layers: usize,
    /// Units in each of a net's hidden layers.
    #[arg(long, value_name = "N", default_value_t = TrainOptions::default().width)]
    pub width: usize,
    /// Passes over the searches a net trains for.
    #[arg(long, value_name = "N", default_value_t = TrainOptions::default().epochs)]
    pub epochs: usize,
    /// Noise added to a net's scaled inputs while it trains.
    #[arg(long, value_name = "SD", default_value_t = TrainOptions::default().noise)]
    pub noise: f64,
    /// Seed of a net's starting weights and noise.
    #[arg(long, value_name = "N", default_value_t = TrainOptions::default().seed)]
    pub seed: u64,
    /// Also train on all but one of this many parts of the training half
    /// and judge on the part left out, in turn, to choose options without
    /// looking at the other half.
    #[arg(long, value_name = "K")]
    pub folds: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LearnerArg {
    Trees,
    Net,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ObjectiveArg {
    LambdaRank,
    LambdaLoss,
    Softmax,
}

impl TrainRankArgs {
    fn options(&self) -> TrainOptions {
        TrainOptions {
            learner: match self.learner {
                LearnerArg::Trees => Learner::Trees,
                LearnerArg::Net => Learner::Net,
            },
            objective: match self.objective {
                ObjectiveArg::LambdaRank => Objective::LambdaRank,
                ObjectiveArg::LambdaLoss => Objective::LambdaLoss,
                ObjectiveArg::Softmax => Objective::Softmax,
            },
            trees: self.trees,
            leaves: self.leaves,
            min_rows_in_leaf: self.min_rows,
            learning_rate: self.learning_rate,
            layers: self.layers,
            width: self.width,
            epochs: self.epochs,
            noise: self.noise,
            seed: self.seed,
        }
    }
}

/// One test search as `--features-out` writes it.
#[derive(Debug, Deserialize)]
struct Written {
    suite: String,
    half: String,
    query: String,
    searched: String,
    /// The answers expected; none for a search that is only graded.
    #[serde(default)]
    expected: Vec<String>,
    rows: Vec<RowSignals>,
}

pub fn run(args: TrainRankArgs) -> Result<()> {
    let mut searches = Vec::new();
    for path in &args.features {
        let reader = plumb_ingest::open_maybe_gz(path)?;
        for (i, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("reading {}", path.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            let written: Written = serde_json::from_str(&line)
                .with_context(|| format!("{} line {}", path.display(), i + 1))?;
            searches.push(written);
        }
    }
    if searches.is_empty() {
        bail!("no searches in the features files");
    }
    let half_name = |half: Half| match half {
        Half::Tune => "tune",
        Half::HeldOut => "held-out",
    };
    let model = match &args.judge {
        Some(name) if name == "builtin" => Model::builtin().clone(),
        Some(path) => Model::from_json(
            &std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?,
        )?,
        None => {
            let half: Vec<&Written> = searches
                .iter()
                .filter(|s| s.half == half_name(args.half))
                .collect();
            let options = args.options();
            if let Some(folds) = args.folds {
                print!("{}", cross_validate(&half, folds, options)?);
            }
            info!("training on {} searches", half.len());
            let model = train(&training(&half), options);
            if let Some(out) = &args.out {
                std::fs::write(out, serde_json::to_string(&model)? + "\n")
                    .with_context(|| format!("writing {}", out.display()))?;
            }
            model
        }
    };
    if let Some(path) = &args.orders_out {
        let mut out = String::new();
        for s in &searches {
            let rows = &s.rows[..s.rows.len().min(LEARNED_ROWS)];
            let order = model.order(&s.searched, rows);
            let line = serde_json::json!({
                "suite": s.suite, "half": s.half, "query": s.query, "order": order,
            });
            let _ = writeln!(out, "{line}");
        }
        std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
    }
    print!("{}", report(&model, &searches));
    Ok(())
}

fn training(searches: &[&Written]) -> Vec<TrainingQuery> {
    searches
        .iter()
        .map(|s| TrainingQuery {
            searched: s.searched.clone(),
            rows: s.rows.clone(),
        })
        .collect()
}

/// Top-1, top-3, MRR and NDCG of `half` judged a part at a time by a
/// model trained on the other `folds - 1` parts, the parts taken by turns.
fn cross_validate(half: &[&Written], folds: usize, options: TrainOptions) -> Result<String> {
    if folds < 2 {
        bail!("--folds needs at least 2 parts");
    }
    let (mut base, mut learned, mut graded) = (Vec::new(), Vec::new(), Vec::new());
    for fold in 0..folds {
        let in_fold = |i: &usize| i % folds == fold;
        let judged = half.iter().enumerate().filter(|(i, _)| in_fold(i));
        let trained: Vec<&Written> = half
            .iter()
            .enumerate()
            .filter(|(i, _)| !in_fold(i))
            .map(|(_, s)| *s)
            .collect();
        let model = train(&training(&trained), options);
        for (_, s) in judged {
            graded.extend(ndcg(&model, s));
            if !s.expected.is_empty() {
                let (b, l) = ranks(&model, s);
                base.push(b);
                learned.push(l);
            }
        }
    }
    let (b, l) = (
        Metrics::from_ranks(&base, LEARNED_ROWS),
        Metrics::from_ranks(&learned, LEARNED_ROWS),
    );
    Ok(format!(
        "{:<9} {:<20} {:>5} {:>6.1}->{:>5.1}% {:>6.1}->{:>5.1}% {:>7.3}->{:.3} {:>13}{}\n",
        format!("cv{folds}"),
        "ALL",
        base.len(),
        b.top1_rate() * 100.0,
        l.top1_rate() * 100.0,
        b.top3_rate() * 100.0,
        l.top3_rate() * 100.0,
        b.mrr,
        l.mrr,
        "",
        ndcg_column(&graded),
    ))
}

/// The mean NDCG of `graded` searches as listed and as the model puts
/// them, for a report line; blank when none are graded.
fn ndcg_column(graded: &[(f64, f64)]) -> String {
    if graded.is_empty() {
        return String::new();
    }
    let mean =
        |pick: fn(&(f64, f64)) -> f64| graded.iter().map(pick).sum::<f64>() / graded.len() as f64;
    format!(
        " {:>5} {:>7.3}->{:.3}",
        graded.len(),
        mean(|g| g.0),
        mean(|g| g.1)
    )
}

/// NDCG of the graded rows within the first [`LEARNED_ROWS`], in the
/// order listed and in the order `model` gives; `None` for a search
/// whose rows are not graded or all graded 0.
fn ndcg(model: &Model, search: &Written) -> Option<(f64, f64)> {
    let rows = &search.rows[..search.rows.len().min(LEARNED_ROWS)];
    if rows.iter().all(|r| r.grade.is_none()) {
        return None;
    }
    let gains: Vec<f64> = rows.iter().map(gain).collect();
    let best = ideal_dcg(&gains);
    if best <= 0.0 {
        return None;
    }
    let dcg = |order: &mut dyn Iterator<Item = usize>| -> f64 {
        order.enumerate().map(|(p, i)| gains[i] * discount(p)).sum()
    };
    let base = dcg(&mut (0..rows.len()));
    let learned = dcg(&mut model.order(&search.searched, rows).into_iter());
    Some((base / best, learned / best))
}

/// The rank of the first labelled row within the first [`LEARNED_ROWS`],
/// in the order listed and in the order `model` gives.
fn ranks(model: &Model, search: &Written) -> (Option<usize>, Option<usize>) {
    let rows = &search.rows[..search.rows.len().min(LEARNED_ROWS)];
    let base = rows.iter().position(|r| r.label > 0).map(|i| i + 1);
    let learned = model
        .order(&search.searched, rows)
        .iter()
        .position(|&i| rows[i].label > 0)
        .map(|i| i + 1);
    (base, learned)
}

fn report(model: &Model, searches: &[Written]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<9} {:<20} {:>5} {:>15} {:>15} {:>15} {:>6} {:>6} {:>5} {:>15}",
        "half", "suite", "n", "top-1", "top-3", "MRR", "better", "worse", "n", "NDCG@10"
    );
    let mut suites: Vec<&str> = searches.iter().map(|s| s.suite.as_str()).collect();
    suites.sort_unstable();
    suites.dedup();
    suites.push("ALL");
    for half in ["tune", "held-out", "test"] {
        for suite in &suites {
            let judged = || {
                searches
                    .iter()
                    .filter(|s| s.half == half && (*suite == "ALL" || s.suite == *suite))
            };
            let (base, learned): (Vec<_>, Vec<_>) = judged()
                .filter(|s| !s.expected.is_empty())
                .map(|s| ranks(model, s))
                .unzip();
            let graded: Vec<(f64, f64)> = judged().filter_map(|s| ndcg(model, s)).collect();
            if base.is_empty() && graded.is_empty() {
                continue;
            }
            let place = |r: Option<usize>| r.unwrap_or(usize::MAX);
            let better = base
                .iter()
                .zip(&learned)
                .filter(|(b, l)| place(**l) < place(**b))
                .count();
            let worse = base
                .iter()
                .zip(&learned)
                .filter(|(b, l)| place(**l) > place(**b))
                .count();
            let (b, l) = (
                Metrics::from_ranks(&base, LEARNED_ROWS),
                Metrics::from_ranks(&learned, LEARNED_ROWS),
            );
            let _ = writeln!(
                out,
                "{half:<9} {suite:<20} {:>5} {:>6.1}->{:>5.1}% {:>6.1}->{:>5.1}% {:>7.3}->{:.3} {better:>6} {worse:>6}{}",
                base.len(),
                b.top1_rate() * 100.0,
                l.top1_rate() * 100.0,
                b.top3_rate() * 100.0,
                l.top3_rate() * 100.0,
                b.mrr,
                l.mrr,
                ndcg_column(&graded),
            );
        }
    }
    out
}
