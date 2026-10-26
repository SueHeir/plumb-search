//! `plumb-tools eval --candidates-out`: each query's deep site candidates,
//! with the raw signals a first stage could score them by
//! ([`plumb_index::Searcher::candidate_signals`]), one JSON line per query.
//! What it is for: trying first stages offline against where the expected
//! sites stand, without an index at hand.

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use plumb_index::{Meaning, RankConfig, SearchOptions};
use serde_json::json;
use tracing::info;

use super::{evaluate, EvalArgs, Setup, Suite};

/// Writes every query of `suites` that expects a site to `path`: its
/// candidates and their signals ([`plumb_index::candidates`]), where the
/// ranking listed its answer, and the sites ranked because the query names
/// them, names their kind or because they are the most linked with its
/// words. The expected sites are always among the candidates.
pub(super) fn write(
    args: &EvalArgs,
    setup: &Setup,
    suites: &[Suite],
    cfg: &RankConfig,
    path: &Path,
) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut out = std::io::BufWriter::new(file);
    let options = SearchOptions {
        country: args.country(),
        exact: args.exact,
        language: args.language(),
        ..SearchOptions::default()
    };
    for suite in suites {
        info!("writing the candidates of {} queries", suite.queries.len());
        let ranks = evaluate(args, setup, &suite.queries, cfg, false, None)?;
        for (q, rank) in suite.queries.iter().zip(ranks) {
            let answers: Vec<String> = q
                .expected
                .iter()
                .filter(|e| !e.contains("://"))
                .cloned()
                .collect();
            if answers.is_empty() {
                continue;
            }
            let query_meaning = setup
                .meaning
                .as_ref()
                .and_then(|meaning| meaning.query(&q.query));
            let meaning = query_meaning.as_ref().map(|m| m as &dyn Meaning);
            let pool = setup
                .searcher
                .candidate_pool(&q.query, cfg, &options, meaning)?;
            let mut domains = answers.clone();
            domains.extend(
                pool.named
                    .iter()
                    .chain(&pool.kind)
                    .chain(&pool.popular)
                    .cloned(),
            );
            let Some(signals) = setup.searcher.candidate_signals(
                &q.query,
                args.candidates_depth,
                cfg,
                meaning,
                &domains,
            )?
            else {
                continue;
            };
            let line = json!({
                "suite": suite.name,
                "line": q.line,
                "query": q.query,
                "answers": answers,
                "rank": rank,
                "named": pool.named,
                "kind": pool.kind,
                "popular": pool.popular,
                "signals": signals,
            });
            writeln!(out, "{line}").with_context(|| format!("writing {}", path.display()))?;
        }
    }
    out.flush()
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
