//! `plumb train-rank`: trains the learned ranking
//! ([`plumb_index::learned`]) on the test searches `plumb eval
//! --features-out` wrote, and measures it.
//!
//! The model is trained on one half of the searches (`--half tune` by
//! default) and judged on both: the other half is the honest number. For
//! each queries file and half it prints top-1, top-3 and MRR within the
//! first [`LEARNED_ROWS`] rows, as listed by the hand-made ranking and as
//! the model puts them.

use std::fmt::Write as _;
use std::io::BufRead;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Args;
use plumb_index::learned::{train, Model, RowSignals, TrainOptions, TrainingQuery, LEARNED_ROWS};
use serde::Deserialize;
use tracing::info;

use crate::cli::Half;
use crate::eval::Metrics;

#[derive(Debug, Args)]
pub struct TrainRankArgs {
    /// Files `plumb eval --features-out` wrote (gzipped or not). Can be
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
    /// Trees to grow.
    #[arg(long, value_name = "N", default_value_t = TrainOptions::default().trees)]
    pub trees: usize,
}

/// One test search as `--features-out` writes it.
#[derive(Debug, Deserialize)]
struct Written {
    suite: String,
    half: String,
    searched: String,
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
            let trained: Vec<TrainingQuery> = searches
                .iter()
                .filter(|s| s.half == half_name(args.half))
                .map(|s| TrainingQuery {
                    searched: s.searched.clone(),
                    rows: s.rows.clone(),
                })
                .collect();
            info!("training on {} searches", trained.len());
            let options = TrainOptions {
                trees: args.trees,
                ..TrainOptions::default()
            };
            let model = train(&trained, options);
            if let Some(out) = &args.out {
                std::fs::write(out, serde_json::to_string(&model)? + "\n")
                    .with_context(|| format!("writing {}", out.display()))?;
            }
            model
        }
    };
    print!("{}", report(&model, &searches));
    Ok(())
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
        "{:<9} {:<20} {:>5} {:>15} {:>15} {:>15} {:>6} {:>6}",
        "half", "suite", "n", "top-1", "top-3", "MRR", "better", "worse"
    );
    let mut suites: Vec<&str> = searches.iter().map(|s| s.suite.as_str()).collect();
    suites.sort_unstable();
    suites.dedup();
    suites.push("ALL");
    for half in ["tune", "held-out"] {
        for suite in &suites {
            let (base, learned): (Vec<_>, Vec<_>) = searches
                .iter()
                .filter(|s| s.half == half && (*suite == "ALL" || s.suite == *suite))
                .map(|s| ranks(model, s))
                .unzip();
            if base.is_empty() {
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
                "{half:<9} {suite:<20} {:>5} {:>6.1}->{:>5.1}% {:>6.1}->{:>5.1}% {:>7.3}->{:.3} {better:>6} {worse:>6}",
                base.len(),
                b.top1_rate() * 100.0,
                l.top1_rate() * 100.0,
                b.top3_rate() * 100.0,
                l.top3_rate() * 100.0,
                b.mrr,
                l.mrr,
            );
        }
    }
    out
}
