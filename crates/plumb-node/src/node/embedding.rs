//! A node's search by meaning ([`super::NodeConfig::search_by_meaning`]):
//! after each index is built, the sites whose text changed are embedded in
//! the background, best first, and searches use the new vectors at once.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use plumb_core::now_unix;

use super::{Inner, LastError, MeaningWork};
use crate::meaning::{embed_records, ensure_model, load_embedder, load_vectors_for, MeaningIndex};
use crate::records::load_records;

/// Directory of the model's files in the data directory.
pub(super) const MODEL_DIR: &str = "model";
/// About how big the model's files are, for the panel's progress.
pub(super) const MODEL_MB: u64 = 130;
/// Wait after a failure (no network for the model download, a bad file).
const RETRY_WAIT: Duration = Duration::from_secs(30 * 60);
/// How often the job looks for a new index.
const LOOK_EVERY: Duration = Duration::from_secs(5);
/// How often a wait looks for shutdown.
const TICK: Duration = Duration::from_secs(1);

/// Loads (downloading when missing) the model and the saved vectors, then
/// brings the vectors up to date with the records each time a new index is
/// served, until the node stops. Runs on a blocking thread of the node's
/// runtime.
pub(super) fn run(inner: Arc<Inner>) {
    while !inner.stopping() {
        let Err(err) = work(&inner) else {
            return;
        };
        warn!(
            "search by meaning: {err:#}; trying again in {} minutes",
            RETRY_WAIT.as_secs() / 60
        );
        let now = now_unix();
        inner
            .journal
            .warning(format!("Search by meaning failed: {err:#}"));
        inner.set_meaning_work(Some(MeaningWork::Failed(LastError {
            message: format!("{err:#}"),
            at: now,
            retry_at: Some(now + RETRY_WAIT.as_secs()),
        })));
        let until = Instant::now() + RETRY_WAIT;
        while !inner.stopping() && Instant::now() < until {
            if inner
                .meaning_retry
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                break;
            }
            std::thread::sleep(TICK);
        }
    }
}

fn work(inner: &Arc<Inner>) -> Result<()> {
    let model_dir = inner.paths.data.join(MODEL_DIR);
    let vectors_path = inner.paths.data.join(plumb_embed::VECTORS_FILE_NAME);
    let meaning = match inner.meaning.get() {
        Some(meaning) => meaning,
        None => {
            inner.set_meaning_work(Some(MeaningWork::Downloading));
            tokio::runtime::Handle::current()
                .block_on(ensure_model(&model_dir))
                .context("downloading the embedding model")?;
            inner.set_meaning_work(Some(MeaningWork::Loading));
            inner.journal.info("Search by meaning: the model is ready");
            let embedder = load_embedder(&model_dir)?;
            let vectors = load_vectors_for(&vectors_path, &embedder)?;
            info!("search by meaning: {} site vectors loaded", vectors.len());
            let meaning = Arc::new(MeaningIndex::new(embedder, vectors));
            inner.meaning.set(Arc::clone(&meaning));
            meaning
        }
    };
    // Leave half the CPUs to searches and crawls.
    let threads = std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1));
    let mut embedded_for = None;
    while !inner.stopping() {
        let Some((index, _)) = inner.current_summary() else {
            nap(inner, LOOK_EVERY);
            continue;
        };
        if embedded_for == Some(index) {
            inner.set_meaning_work(None);
            nap(inner, LOOK_EVERY);
            continue;
        }
        let records = load_records(&inner.paths.records)
            .with_context(|| format!("loading {}", inner.paths.records.display()))?
            .into_sorted_vec();
        let started = Instant::now();
        let embedded = embed_records(
            meaning.embedder(),
            meaning.vectors(),
            &records,
            threads,
            &|| inner.stopping(),
            &mut |vectors| vectors.save(&vectors_path),
            &mut |done, total| {
                inner.set_meaning_work((done < total).then_some(MeaningWork::Embedding {
                    done: done as u64,
                    total: total as u64,
                }));
            },
        )?;
        inner.set_meaning_work(None);
        drop(records);
        if inner.stopping() {
            break;
        }
        info!(
            "search by meaning: {} sites embedded in {:.0} s ({} failed), {} have a vector",
            embedded.done,
            started.elapsed().as_secs_f64(),
            embedded.failed,
            meaning.len()
        );
        if embedded.done > 0 {
            inner.journal.info(format!(
                "Search by meaning: {} sites got a vector",
                crate::web::group_thousands(embedded.done as u64)
            ));
        }
        embedded_for = Some(index);
    }
    Ok(())
}

/// Sleeps for `wait`, or until the node stops.
fn nap(inner: &Inner, wait: Duration) {
    let until = Instant::now() + wait;
    while !inner.stopping() && Instant::now() < until {
        std::thread::sleep(TICK.min(until - Instant::now()));
    }
}
