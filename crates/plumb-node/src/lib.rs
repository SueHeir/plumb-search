//! The Plumb Search node: the `plumb` command-line tool and its web server.
//!
//! `plumb` runs the whole Phase 1 pipeline on one machine:
//!
//! 1. `fetch-data` downloads the seed datasets,
//! 2. `ingest` folds them into a records file (one JSON line per site),
//! 3. `crawl` refreshes the best-ranked homepages and discovers new sites,
//! 4. `index` builds the local search index,
//! 5. `search`, `serve` and `eval` query it.
//!
//! `run` does all of that as one long-running node: it sets up the index on
//! first start, serves it, and keeps crawling and rebuilding ([`node`]).
//!
//! The binary is a thin wrapper around [`run`]. The [`web`] and [`eval`]
//! modules are public so their handlers and metrics can be tested directly,
//! and [`node`] so that the desktop app can embed a node.

use std::future::Future;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use plumb_core::{write_jsonl, SiteRecord};
use plumb_index::RankConfig;
use tracing_subscriber::EnvFilter;

pub mod cli;
pub mod country;
pub mod eval;
pub mod node;
pub mod web;

mod crawl;
mod fetch;
mod ingest;
mod run;
mod search;

use cli::{Cli, Command};

/// Log filter used when `RUST_LOG` is unset or empty: `info` for everything
/// except Tantivy, which logs every commit and merge step at `info`.
pub const DEFAULT_LOG_FILTER: &str = "info,tantivy=warn";

/// Sends `tracing` output to stderr (stdout is kept for command output such
/// as `search --json`), filtered by `RUST_LOG` or [`DEFAULT_LOG_FILTER`].
/// Colors are used only on a terminal, and never when `NO_COLOR` is set.
pub fn init_logging() {
    let filter = match std::env::var("RUST_LOG") {
        Ok(spec) if !spec.trim().is_empty() => EnvFilter::try_new(&spec).unwrap_or_else(|err| {
            eprintln!("warning: ignoring RUST_LOG={spec:?}: {err}");
            EnvFilter::new(DEFAULT_LOG_FILTER)
        }),
        _ => EnvFilter::new(DEFAULT_LOG_FILTER),
    };
    let color = std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    // Fails only when a subscriber is already installed, which is fine.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(color)
        .try_init();
}

/// Runs one `plumb` subcommand.
pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Run(args) => run::run(args),
        Command::FetchData(args) => fetch::run(args),
        Command::Ingest(args) => ingest::run(args),
        Command::Crawl(args) => crawl::run(args),
        Command::Index(args) => search::run_index(args),
        Command::Search(args) => search::run_search(args),
        Command::Serve(args) => web::run(args),
        Command::Eval(args) => eval::run(args),
    }
}

/// A multi-threaded Tokio runtime. Only the commands that do network I/O
/// need one; `crawl` keeps one for all its batches.
pub(crate) fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")
}

/// Runs `future` to completion on a new [`runtime`].
pub(crate) fn block_on<F: Future>(future: F) -> Result<F::Output> {
    Ok(runtime()?.block_on(future))
}

/// [`RankConfig::default`], with the popularity weight replaced when given.
pub(crate) fn rank_config(alpha: Option<f32>) -> RankConfig {
    let mut cfg = RankConfig::default();
    if let Some(alpha) = alpha {
        cfg.alpha = alpha;
    }
    cfg
}

/// Writes records as JSON lines to a temporary file next to `path`, then
/// renames it over `path`, so an interrupted run never leaves a half-written
/// records file behind (`crawl` overwrites its input by default, after
/// every batch).
pub(crate) fn write_records_atomically<'a, I>(path: &Path, records: I) -> Result<usize>
where
    I: IntoIterator<Item = &'a SiteRecord>,
{
    let tmp = temp_path_for(path);
    let written = match write_jsonl(&tmp, records) {
        Ok(n) => n,
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(err);
        }
    };
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("moving {} to {}", tmp.display(), path.display()));
    }
    Ok(written)
}

/// `dir/records.jsonl` -> `dir/.records.jsonl.<pid>.tmp`.
fn temp_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "records".to_string());
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_config_overrides_alpha_only() {
        let default = RankConfig::default();
        assert_eq!(rank_config(None), default);
        let cfg = rank_config(Some(0.8));
        assert_eq!(cfg.alpha, 0.8);
        assert_eq!(cfg.candidates, default.candidates);
        assert_eq!(cfg.exact_label_bonus, default.exact_label_bonus);
    }

    #[test]
    fn atomic_write_replaces_the_file_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.jsonl");
        std::fs::write(&path, "old contents\n").unwrap();
        let records = vec![SiteRecord::new("usbank.com"), SiteRecord::new("chase.com")];
        assert_eq!(write_records_atomically(&path, &records).unwrap(), 2);
        let back: Vec<SiteRecord> = plumb_core::read_jsonl(&path).unwrap();
        assert_eq!(back, records);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name != "records.jsonl")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn temp_path_stays_in_the_same_directory() {
        let tmp = temp_path_for(Path::new("data/records.jsonl"));
        assert_eq!(tmp.parent(), Some(Path::new("data")));
        assert!(tmp
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".records.jsonl."));
    }
}
