//! The Plumb Search node: the `plumb` command-line tool and its web server.
//!
//! `plumb` runs the whole Phase 1 pipeline on one machine:
//!
//! 1. `fetch-data` downloads the seed datasets,
//! 2. `ingest` folds them into a records file (one JSON line per site),
//! 3. `crawl` refreshes the best-ranked homepages and discovers new sites,
//! 4. `index` builds the local search index,
//! 5. `search`, `serve` and `eval` query it, and `mcp` lets AI apps ask
//!    it for official sites ([`mcp`]).
//!
//! `run` does all of that as one long-running node: it sets up the index on
//! first start, serves it, and keeps crawling and rebuilding ([`node`]).
//!
//! The binary is a thin wrapper around [`run`]. The [`web`] and [`eval`]
//! modules are public so their handlers and metrics can be tested directly,
//! and [`node`] so that the desktop app can embed a node.

use std::fs::File;
use std::future::Future;
use std::io::{BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use plumb_core::SiteRecord;
use plumb_index::RankConfig;
use tracing_subscriber::EnvFilter;

pub mod about;
pub mod cli;
pub mod country;
pub mod eval;
pub mod findings;
pub mod history;
pub mod learn;
pub mod mcp;
pub mod meaning;
pub mod node;
pub mod pages;
pub mod places;
pub mod plugins;
pub mod storage;
pub mod sync;
pub mod tls;
pub mod web;
pub mod websearch;

mod crawl;
mod dead;
mod fetch;
mod icons;
mod ingest;
mod limits;
pub mod news;
mod outline;
mod records;
mod run;
mod search;
mod terms;

use cli::{Cli, Command};

/// Log filter used when `RUST_LOG` is unset or empty: `info` for everything
/// except Tantivy, which logs every commit and merge step at `info`, and
/// rustls-platform-verifier, which logs each homepage with a bad
/// certificate as an error, though the crawler expects and counts those.
pub const DEFAULT_LOG_FILTER: &str = "info,tantivy=warn,rustls_platform_verifier=off";

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
    limits::raise_open_file_limit();
    match cli.command {
        Command::Run(args) => run::run(args),
        Command::FetchData(args) => fetch::run(args),
        Command::FetchPages(args) => fetch::run_pages(args),
        Command::FetchProfiles(args) => fetch::run_profiles(args),
        Command::Ingest(args) => ingest::run(args),
        Command::Crawl(args) => crawl::run(args),
        Command::Index(args) => search::run_index(args),
        Command::Search(args) => search::run_search(args),
        Command::Serve(args) => web::run(args),
        Command::Eval(args) => eval::run(args),
        Command::Embed(args) => meaning::run_embed(args),
        Command::FetchText(args) => terms::run_fetch_text(args),
        Command::Terms(args) => terms::run_terms(args),
        Command::RemoteControl(args) => run::remote_control(args),
        Command::Storage(args) => storage::run(args),
        Command::DeadSites(args) => dead::run(&args),
        Command::Mcp(args) => mcp::run(args),
        Command::TryPlugin(args) => plugins::try_plugin(
            &args.plugin,
            &args.query.join(" "),
            args.act.as_deref(),
            args.annotate.as_deref(),
        ),
        Command::Healthcheck(args) => healthcheck(&args),
    }
}

/// `plumb healthcheck`: GET `<url>/api/status`, an error unless it answers
/// with a 2xx status within the timeout.
fn healthcheck(args: &cli::HealthcheckArgs) -> Result<()> {
    let url = format!("{}/api/status", args.url.trim_end_matches('/'));
    let timeout = std::time::Duration::from_secs(args.timeout);
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        // The node is on this machine (or in this container), which a proxy
        // from the environment would not reach.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("making the HTTP client")?;
    let status = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?
        // `send` sets its timer up when called, so inside the runtime.
        .block_on(async { client.get(&url).send().await })
        .with_context(|| format!("asking {url}"))?
        .status();
    if !status.is_success() {
        anyhow::bail!("{url} answered {status}");
    }
    Ok(())
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

/// Writes records as JSON lines to a temporary file next to `path`, flushes
/// it to disk, renames it over `path` and flushes the directory too (on
/// Unix), so an interrupted run, a crash or a power cut leaves the old file
/// or the new one, never a half-written or empty one. Creates the parent
/// directory when missing.
pub(crate) fn write_records_atomically<'a, I>(path: &Path, records: I) -> Result<usize>
where
    I: IntoIterator<Item = &'a SiteRecord>,
{
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    let tmp = temp_path_for(path);
    let written = match write_lines_durably(&tmp, records) {
        Ok(n) => n,
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(err.context(format!("writing {}", tmp.display())));
        }
    };
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("moving {} to {}", tmp.display(), path.display()));
    }
    sync_parent_dir(path);
    Ok(written)
}

/// Writes one JSON line per record to a new file at `path` and flushes it to
/// disk. Returns the number of records written.
fn write_lines_durably<'a, I>(path: &Path, records: I) -> Result<usize>
where
    I: IntoIterator<Item = &'a SiteRecord>,
{
    let mut writer = BufWriter::with_capacity(1 << 20, File::create(path)?);
    let mut written = 0;
    for record in records {
        serde_json::to_writer(&mut writer, record)?;
        writer.write_all(b"\n")?;
        written += 1;
    }
    let file = writer.into_inner().map_err(|err| err.into_error())?;
    file.sync_all()?;
    Ok(written)
}

/// Flushes the directory holding `path` to disk, so that a file just
/// created, renamed or deleted there stays that way after a power cut.
/// Best effort, and only on Unix: other systems cannot open a directory
/// as a file, and some file systems refuse to flush one.
pub(crate) fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    {
        let dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        if let Err(err) = File::open(dir).and_then(|dir| dir.sync_all()) {
            tracing::debug!("cannot flush the directory {}: {err}", dir.display());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Hands memory that big jobs (loading records, building an index) freed
/// back to the system. glibc keeps freed memory in its arenas for reuse, so
/// a long-running node would otherwise sit on it between refreshes: idle
/// after indexing 300,000 sites, a node held about 390 MB without this and
/// 60 to 90 MB with it. Does nothing on other systems.
pub(crate) fn release_freed_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> std::os::raw::c_int;
        }
        // SAFETY: malloc_trim only gives free memory back to the system, and
        // glibc lets any thread call it at any time.
        unsafe {
            malloc_trim(0);
        }
    }
}

/// `dir/records.jsonl` -> `dir/.records.jsonl.<pid>.tmp`.
pub(crate) fn temp_path_for(path: &Path) -> PathBuf {
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
