//! Keeps the node's page index ([`crate::pages`]) in step with its page
//! set files and settings, and adds pages to search results.
//!
//! A node in the network that keeps a set but has no file of it, or too
//! few pages of it, takes the file from a node it trusts (see
//! `plumb_net::pages`), only as far as the pages it keeps, and takes it
//! again once it is [`REFRESH_AFTER`] old and the other node has a newer
//! one.

use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use std::io::Write;

use anyhow::{bail, Context, Result};
use plumb_core::{now_unix, Operators};
use plumb_index::pages::{options_allow, place_operator_pages, place_pages, OPERATOR_PAGES};
use plumb_index::{SearchOptions, SearchResults};
use plumb_net::pages::MAX_PAGES_CHUNK;
use plumb_net::NetHandle;
use tracing::{debug, info, warn};

use super::Inner;
use crate::pages::{
    notes_path, open_or_build, remove_other_indexes, thousands, wanted_counts, SetFileCutter,
    SetFileNotes, SetInfo, Wanted,
};

/// How often the job looks for changed settings or set files.
#[cfg(not(test))]
const LOOK_EVERY: Duration = Duration::from_secs(10);
#[cfg(test)]
const LOOK_EVERY: Duration = Duration::from_millis(200);
/// Wait after a failed build before trying the same pages again.
const RETRY_WAIT: Duration = Duration::from_secs(30 * 60);
/// How often a wait looks for shutdown.
const TICK: Duration = Duration::from_millis(250);
/// Pages looked at per search, before [`place_pages`] picks.
const PAGES_PER_SEARCH: usize = 10;
/// A set file taken from another node is taken again after this long,
/// when that node has a newer one.
const REFRESH_AFTER: Duration = Duration::from_secs(30 * 24 * 3600);
/// Wait after a node said it was busy.
const BUSY_WAIT: Duration = Duration::from_secs(5);
/// Wait before asking again after no trusted node had the set, or a
/// download failed.
const FETCH_RETRY_WAIT: Duration = Duration::from_secs(15 * 60);

/// Runs until the node stops, on a blocking thread.
pub(super) fn run(inner: Arc<Inner>) {
    let mut failed: Option<(String, Instant)> = None;
    let mut fetch_failed: Option<Instant> = None;
    let mut places_failed: Option<(String, Instant)> = None;
    while !inner.stopping() {
        let mut settings = inner.settings();
        if inner.config.blackhole {
            settings.page_sets = settings.page_sets.all_unless_set();
        }
        if fetch_failed.is_none_or(|at| at.elapsed() >= FETCH_RETRY_WAIT) {
            fetch_failed = None;
            if let Some(net) = super::network::handle(&inner).cloned() {
                for (set, pages) in wanted_counts(&settings.page_sets, settings.storage_limit_mb) {
                    if inner.stopping() {
                        break;
                    }
                    if let Err(err) = fetch_if_needed(&inner, &net, set, pages) {
                        warn!("page set {}: {err:#}", set.id);
                        fetch_failed = Some(Instant::now());
                    }
                }
            }
        }
        let wanted = Wanted::new(
            &inner.paths.data,
            &settings.page_sets,
            settings.storage_limit_mb,
        );
        let key = wanted.key();
        let current = inner.page_key();
        let retry_later = failed
            .as_ref()
            .is_some_and(|(k, at)| Some(k) == key.as_ref() && at.elapsed() < RETRY_WAIT);
        if key != current && !retry_later {
            match open_or_build(&inner.paths.data, &wanted) {
                Ok(found) => {
                    let pages = found.as_ref().map_or(0, |(_, s)| s.num_pages());
                    let kept = found.as_ref().map(|(k, _)| k.clone());
                    *inner.pages.write().unwrap_or_else(PoisonError::into_inner) =
                        found.map(|(k, s)| (k, Arc::new(s)));
                    remove_other_indexes(&inner.paths.data, kept.as_deref());
                    if kept.is_some() {
                        inner.journal.info(format!(
                            "Page sets ready: {} pages searched next to the sites",
                            thousands(pages)
                        ));
                    } else if current.is_some() {
                        inner.journal.info("Page sets turned off");
                    }
                    failed = None;
                }
                Err(err) => {
                    warn!("page sets: {err:#}");
                    inner
                        .journal
                        .warning(format!("Could not build the page index: {err:#}"));
                    failed = key.map(|k| (k, Instant::now()));
                }
            }
        }
        super::places::refresh(&inner, &settings, &mut places_failed);
        let until = Instant::now() + LOOK_EVERY;
        while !inner.stopping() && Instant::now() < until {
            std::thread::sleep(TICK);
        }
    }
}

/// Takes the file of `set` from a trusted node when this node has none,
/// fewer than `pages` of it, or an old one the other node has a newer
/// version of.
fn fetch_if_needed(inner: &Inner, net: &NetHandle, set: &SetInfo, pages: u64) -> Result<()> {
    let data = &inner.paths.data;
    let notes = set.file_notes(data);
    let now = now_unix();
    let reason = match notes {
        None => "none yet",
        Some(n) if !n.complete && n.lines < pages => "more pages wanted",
        Some(n)
            if n.fetched_at > 0 && now.saturating_sub(n.fetched_at) > REFRESH_AFTER.as_secs() =>
        {
            "a month old"
        }
        Some(_) => return Ok(()),
    };
    let runtime = tokio::runtime::Handle::current();
    // Is there a node to take it from, with a file worth taking?
    let first = loop {
        match runtime.block_on(net.pages_chunk(set.id, 0, MAX_PAGES_CHUNK))? {
            None => {
                debug!("page set {}: no trusted node serves page sets", set.id);
                return Ok(());
            }
            Some(chunk) if chunk.busy => {
                if !wait(inner, BUSY_WAIT) {
                    return Ok(());
                }
            }
            Some(chunk) => break chunk,
        }
    };
    if first.size == 0 {
        bail!("the trusted node {} has no {} file", first.peer, set.id);
    }
    if let Some(n) = notes {
        if reason == "a month old" && first.modified <= n.source_modified {
            // Nothing newer; look again in a month.
            write_notes(
                &set.file(data),
                &SetFileNotes {
                    fetched_at: now,
                    ..n
                },
            )?;
            return Ok(());
        }
    }
    info!(
        "taking {} ({}) from {}: {} MB in all",
        set.name,
        reason,
        first.peer,
        first.size / 1_000_000
    );
    inner.journal.info(format!(
        "Downloading {} from a node you trust ({} pages wanted)",
        set.name,
        if pages == u64::MAX {
            "all".to_string()
        } else {
            thousands(pages)
        }
    ));
    let file = set.file(data);
    std::fs::create_dir_all(file.parent().context("a set file has a folder")?)?;
    let mut part = file.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let mut decoder = flate2::write::MultiGzDecoder::new(SetFileCutter::create(&part, pages)?);
    let mut offset = 0u64;
    let mut chunk = first;
    loop {
        decoder
            .write_all(&chunk.bytes)
            .with_context(|| format!("unpacking {} from {}", set.id, chunk.peer))?;
        // The decoder holds back what it unpacked until flushed.
        decoder
            .flush()
            .with_context(|| format!("unpacking {} from {}", set.id, chunk.peer))?;
        offset += chunk.bytes.len() as u64;
        if decoder.get_ref().full() || offset >= chunk.size || chunk.bytes.is_empty() {
            break;
        }
        if inner.stopping() {
            let _ = std::fs::remove_file(&part);
            return Ok(());
        }
        let size = chunk.size;
        chunk = loop {
            match runtime.block_on(net.pages_chunk(set.id, offset, MAX_PAGES_CHUNK))? {
                None => bail!("the trusted node went away"),
                Some(next) if next.busy => {
                    if !wait(inner, BUSY_WAIT) {
                        let _ = std::fs::remove_file(&part);
                        return Ok(());
                    }
                }
                Some(next) if next.size != size => bail!("the file changed while it was taken"),
                Some(next) => break next,
            }
        };
    }
    let cutter = decoder.get_mut();
    let complete = offset >= chunk.size && !cutter.cut();
    let lines = cutter.pages();
    cutter.finish()?;
    drop(decoder);
    std::fs::rename(&part, &file)
        .with_context(|| format!("renaming {} to {}", part.display(), file.display()))?;
    write_notes(
        &file,
        &SetFileNotes {
            lines,
            complete,
            source_modified: chunk.modified,
            fetched_at: now,
        },
    )?;
    inner.journal.info(format!(
        "{}: {} pages taken ({} MB downloaded)",
        set.name,
        thousands(lines),
        offset.div_ceil(1_000_000)
    ));
    Ok(())
}

fn write_notes(file: &std::path::Path, notes: &SetFileNotes) -> Result<()> {
    super::store::write_atomically(&notes_path(file), &serde_json::to_vec(notes)?)
}

/// Sleeps `wait`, unless the node stops first; `false` when it stops.
fn wait(inner: &Inner, wait: Duration) -> bool {
    let until = Instant::now() + wait;
    while Instant::now() < until {
        if inner.stopping() {
            return false;
        }
        std::thread::sleep(TICK);
    }
    true
}

impl Inner {
    fn page_key(&self) -> Option<String> {
        self.pages
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|(key, _)| key.clone())
    }
}

/// Adds the pages found for `query` (as corrected, when the results are
/// for a corrected spelling) to `results`.
pub(super) fn add_pages(
    inner: &Inner,
    query: &str,
    options: &SearchOptions,
    results: &mut SearchResults,
) {
    let Some(searcher) = inner
        .pages
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(_, s)| s.clone())
    else {
        return;
    };
    let ops = Operators::parse(query);
    if ops.any() {
        if ops.words.is_empty() {
            return;
        }
        match searcher.search(&ops.words, OPERATOR_PAGES) {
            Ok(mut found) => {
                found.retain(|hit| options_allow(options, &hit.page));
                results.pages = place_operator_pages(&ops, &results.hits, found);
            }
            Err(err) => warn!("searching pages: {err:#}"),
        }
        return;
    }
    let query = match &results.spelling {
        Some(spelling) if spelling.applied => spelling.query.as_str(),
        _ => query,
    };
    match searcher.search(query, PAGES_PER_SEARCH) {
        Ok(mut found) => {
            found.retain(|hit| options_allow(options, &hit.page));
            results.pages = place_pages(query, &results.hits, found);
        }
        Err(err) => warn!("searching pages: {err:#}"),
    }
}
