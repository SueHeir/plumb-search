//! A node's search by meaning ([`super::NodeConfig::search_by_meaning`]):
//! after each index is built, the sites whose text changed are embedded in
//! the background, best first, and searches use the new vectors at once.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use plumb_embed::MODEL_FILES;

use plumb_core::now_unix;

use super::shared_vectors::Taken;
use super::{Inner, LastError, MeaningWork};
use crate::meaning::{
    embed_sites, ensure_gemma, ensure_model, load_embedder, load_vectors_for,
    sites_to_embed_from_file, MeaningIndex, MeaningModel,
};

/// Wait after a failure (no network for the model download, a bad file).
const RETRY_WAIT: Duration = Duration::from_secs(30 * 60);
/// How often the job looks for a new index.
const LOOK_EVERY: Duration = Duration::from_secs(5);
/// How often a model download waiting on a pause looks again.
#[cfg(not(test))]
const PAUSED_LOOK: Duration = Duration::from_secs(60);
#[cfg(test)]
const PAUSED_LOOK: Duration = Duration::from_millis(200);
/// How often a wait looks for shutdown.
const TICK: Duration = Duration::from_secs(1);
/// Sites embedded per turn, best first: each one's text is held until it
/// is embedded, and a node with millions of sites to embed (a new model)
/// would otherwise hold all their text at once.
const EMBED_AT_ONCE: usize = 50_000;
/// Sites waiting for a vector before the node asks trusted nodes for theirs.
#[cfg(not(test))]
const TAKE_AT_LEAST: usize = 10_000;
#[cfg(test)]
const TAKE_AT_LEAST: usize = 1;
/// How often it asks, at most.
const TAKE_EVERY: Duration = Duration::from_secs(24 * 3600);
/// How long after starting it waits for a trusted node to connect before
/// embedding the sites itself.
#[cfg(not(test))]
const WAIT_FOR_NODES: Duration = Duration::from_secs(120);
#[cfg(test)]
const WAIT_FOR_NODES: Duration = Duration::from_secs(20);

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
    let model = inner.config.meaning_model;
    let model_dir = inner.paths.data.join(model.dir_name());
    let vectors_path = inner.paths.data.join(plumb_embed::VECTORS_FILE_NAME);
    let meaning = match inner.meaning.get() {
        Some(meaning) => meaning,
        None => {
            // Not while paused or past the day's download limit; the model
            // already here is loaded all the same.
            let mut told = false;
            while let Some(pause) = inner.download_pause() {
                if model_here(inner, &model_dir) {
                    break;
                }
                if !told {
                    told = true;
                    inner.journal.info(format!(
                        "Search by meaning: the model is downloaded later ({})",
                        pause.reason
                    ));
                }
                if !nap_until_stop(inner, PAUSED_LOOK) {
                    return Ok(());
                }
            }
            inner.set_meaning_work(Some(MeaningWork::Downloading));
            let before = super::store::dir_size(&model_dir);
            // A stop does not wait for the download, which can take minutes.
            let downloaded = tokio::runtime::Handle::current().block_on(async {
                tokio::select! {
                    downloaded = async {
                        let sources = &inner.config.sources;
                        match model {
                            MeaningModel::Small => ensure_model(&model_dir, &sources.model_base_url).await,
                            MeaningModel::Gemma => ensure_gemma(&model_dir, &sources.gemma_downloads).await,
                        }
                    } => Some(downloaded),
                    () = inner.stopped() => None,
                }
            });
            let Some(downloaded) = downloaded else {
                return Ok(());
            };
            let fetched = super::store::dir_size(&model_dir).saturating_sub(before);
            if let Err(err) = inner.add_downloaded(fetched) {
                warn!("search by meaning: counting the model download: {err:#}");
            }
            downloaded.context("downloading the embedding model")?;
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
    // Unless set, leave half the CPUs to searches and crawls.
    let threads = inner.config.embed_threads.unwrap_or_else(|| {
        std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1))
    });
    let mut embedded_for = None;
    let mut taken_at: Option<Instant> = None;
    let started_at = Instant::now();
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
        let (todo, more) = {
            // Read a record at a time, while no crawl or index build changes
            // the file.
            let _records = inner.hold_records();
            if inner.stopping() {
                break;
            }
            sites_to_embed_from_file(
                meaning.vectors(),
                &inner.paths.records,
                EMBED_AT_ONCE,
                meaning.embedder().text_words(),
            )
            .with_context(|| format!("reading {}", inner.paths.records.display()))?
        };
        // Many sites to embed (search by meaning just turned on, or a new
        // model): a trusted node may have made their vectors already.
        let take_now = (more || todo.len() >= TAKE_AT_LEAST)
            && taken_at.is_none_or(|at| at.elapsed() >= TAKE_EVERY);
        if let Some(net) = super::network::handle(inner).cloned().filter(|_| take_now) {
            taken_at = Some(Instant::now());
            drop(todo);
            match super::shared_vectors::take(inner, &net, &meaning, &vectors_path) {
                // No trusted node connected yet: wait a little for one.
                Ok(Taken::NoNode) if started_at.elapsed() < WAIT_FOR_NODES => {
                    taken_at = None;
                    nap(inner, Duration::from_secs(1));
                }
                Ok(_) => {}
                Err(err) => warn!("search by meaning: taking vectors: {err:#}"),
            }
            continue;
        }
        let started = Instant::now();
        let embedded = embed_sites(
            meaning.embedder(),
            meaning.vectors(),
            todo,
            threads,
            &|| inner.stopping(),
            &mut |vectors| plumb_embed::Vectors::save_shared(vectors, &vectors_path),
            &mut |done, total| {
                inner.set_meaning_work((done < total).then_some(MeaningWork::Embedding {
                    done: done as u64,
                    total: total as u64,
                }));
            },
        )?;
        inner.set_meaning_work(None);
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
        // With more sites waiting, the next turn picks the next best.
        if !more || embedded.done == 0 {
            embedded_for = Some(index);
        }
    }
    Ok(())
}

/// Sleeps for `wait`, or until the node stops; whether it is still running.
/// Whether the model's files are all in `dir` already, so loading it
/// downloads nothing.
fn model_here(inner: &Inner, dir: &std::path::Path) -> bool {
    match inner.config.meaning_model {
        MeaningModel::Small => {
            MODEL_FILES.iter().all(|name| dir.join(name).is_file())
                || dir.join(plumb_embed::SERVER_FILE).is_file()
        }
        MeaningModel::Gemma => inner
            .config
            .sources
            .gemma_downloads
            .iter()
            .all(|(name, _)| dir.join(name).is_file()),
    }
}

pub(super) fn nap_until_stop(inner: &Inner, wait: Duration) -> bool {
    nap(inner, wait);
    !inner.stopping()
}

/// Sleeps for `wait`, or until the node stops.
fn nap(inner: &Inner, wait: Duration) {
    let until = Instant::now() + wait;
    while !inner.stopping() && Instant::now() < until {
        std::thread::sleep(TICK.min(until - Instant::now()));
    }
}
