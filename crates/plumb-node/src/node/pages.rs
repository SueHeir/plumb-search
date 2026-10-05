//! Keeps the node's page index ([`crate::pages`]) in step with its page
//! set files and settings, and adds pages to search results.
//!
//! A node in the network that keeps a set but has no file of it, or too
//! few pages of it, takes the file from a node it trusts (see
//! `plumb_net::pages`), only as far as the pages it keeps, and takes it
//! again once it is [`REFRESH_AFTER`] old and the other node has a newer
//! one.

use std::collections::HashSet;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use std::io::Write;

use anyhow::{bail, Context, Result};
use plumb_core::{now_unix, Operators};
use plumb_index::pages::{
    lift_named_sites, options_allow, place_operator_pages, place_pages, OPERATOR_PAGES,
};
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
    let mut kept_whole = HashSet::new();
    while !inner.stopping() {
        let mut settings = inner.settings();
        if inner.config.blackhole {
            settings.page_sets = settings.page_sets.all_unless_set();
        }
        let near = super::places::known_homes(&inner);
        let counts: Vec<(&SetInfo, u64)> =
            wanted_counts(&settings.page_sets, settings.storage_limit_mb)
                .into_iter()
                .map(|(set, pages)| match set.id {
                    plumb_index::places::PLACES_SET => (
                        set,
                        crate::places::file_pages(
                            &settings.page_sets,
                            settings.storage_limit_mb,
                            near.as_deref().unwrap_or_default(),
                        ),
                    ),
                    _ => (set, pages),
                })
                .collect();
        // Only a node with a storage limit cuts its files: a server's are
        // handed on to other nodes whole.
        for &(set, pages) in counts.iter().filter(|_| settings.storage_limit_mb > 0) {
            if !may_cut(set, pages, near.is_some()) {
                continue;
            }
            if let Err(err) = cut_if_longer(&inner, set, pages, &mut kept_whole) {
                warn!("page set {}: {err:#}", set.id);
            }
        }
        if fetch_failed.is_none_or(|at| at.elapsed() >= FETCH_RETRY_WAIT) {
            fetch_failed = None;
            if let Some(net) = super::network::handle(&inner).cloned() {
                for &(set, pages) in &counts {
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

/// Cuts the file of `set` to its first `pages` pages when it holds more:
/// the node keeps no more than that (the storage limit or the panel's
/// choice went down), and the rest takes room for nothing. Taken again
/// from a trusted node when more are wanted later.
///
/// A file with no notes is the user's own (made by `plumb fetch-pages`),
/// not one taken from another node, so it is never cut: it could not be
/// taken back. `kept_whole` holds the sets already said so, to say it once.
fn cut_if_longer(
    inner: &Inner,
    set: &SetInfo,
    pages: u64,
    kept_whole: &mut HashSet<&'static str>,
) -> Result<()> {
    let data = &inner.paths.data;
    let Some(notes) = set.file_notes(data) else {
        return Ok(());
    };
    if pages == u64::MAX || notes.lines <= pages || inner.stopping() {
        return Ok(());
    }
    let file = set.file(data);
    if is_own_file(&file) {
        if kept_whole.insert(set.id) {
            info!(
                "page set {}: {} has no notes, so it is yours: kept whole under the storage limit",
                set.id,
                file.display()
            );
        }
        return Ok(());
    }
    let mut part = file.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let before = std::fs::metadata(&file).map_or(0, |m| m.len());
    let mut reader = plumb_ingest::open_maybe_gz(&file)?;
    let mut cutter = SetFileCutter::create(&part, pages)?;
    let mut buf = vec![0u8; 1 << 16];
    while !cutter.full() {
        let n = std::io::Read::read(&mut reader, &mut buf)
            .with_context(|| format!("reading {}", file.display()))?;
        if n == 0 {
            break;
        }
        cutter.write_all(&buf[..n])?;
    }
    let lines = cutter.pages();
    cutter.finish()?;
    drop(reader);
    std::fs::rename(&part, &file)
        .with_context(|| format!("renaming {} to {}", part.display(), file.display()))?;
    write_notes(
        &file,
        &SetFileNotes {
            lines,
            complete: false,
            ..notes
        },
    )?;
    let after = std::fs::metadata(&file).map_or(0, |m| m.len());
    inner.recount_disk();
    info!(
        "page set {}: kept the first {lines} pages of its file",
        set.id
    );
    inner.journal.info(format!(
        "{}: kept the first {} pages, freeing {} MB",
        set.name,
        thousands(lines),
        before.saturating_sub(after) / 1_000_000
    ));
    Ok(())
}

/// Whether `set`'s file may be cut to `pages` now. The places file is
/// not while the towns on the About pages are not yet found
/// (`homes_known` false, until the place index has opened): cut to the
/// places kept everywhere, it would lose the ones near them and be taken
/// again whole once they are found, on every restart.
fn may_cut(set: &SetInfo, pages: u64, homes_known: bool) -> bool {
    homes_known || pages == 0 || set.id != plumb_index::places::PLACES_SET
}

/// Whether the set file `file` is one the user made, with no notes of
/// where it was taken from.
fn is_own_file(file: &std::path::Path) -> bool {
    file.is_file() && !notes_path(file).exists()
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

/// Adds the pages found for `query` to `results`.
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
    match searcher.search(query, PAGES_PER_SEARCH) {
        Ok(mut found) => {
            found.retain(|hit| options_allow(options, &hit.page));
            lift_named_sites(&mut results.hits, &found);
            // A query that names a package or a page, or asks a question
            // in full, is spelled right: "serde crate" is not "serde
            // create", and "git undo last commit" is not "git und".
            if found
                .iter()
                .any(|hit| hit.page.package.is_some() || hit.named || hit.whole)
            {
                results.spelling = None;
            }
            results.pages = place_pages(query, &results.hits, found);
        }
        Err(err) => warn!("searching pages: {err:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_places_file_is_not_cut_before_the_towns_are_found() {
        let places = crate::places::set_info();
        let wikipedia = SetInfo::find("wikipedia-en").unwrap();
        // Before the place index opens, the places near the towns are not
        // known: the file is kept whole.
        assert!(!may_cut(places, crate::places::EVERYWHERE, false));
        assert!(may_cut(places, crate::places::EVERYWHERE, true));
        // Places turned off: cut whatever.
        assert!(may_cut(places, 0, false));
        // Other sets do not depend on the towns.
        assert!(may_cut(wikipedia, 1_000, false));
    }

    #[test]
    fn a_set_file_with_no_notes_is_the_user_s_own() {
        let dir = tempfile::tempdir().unwrap();
        let file = SetInfo::find("wikipedia-en").unwrap().file(dir.path());
        assert!(!is_own_file(&file), "no file");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"").unwrap();
        assert!(is_own_file(&file), "made by fetch-pages: kept whole");
        write_notes(
            &file,
            &SetFileNotes {
                lines: 10,
                complete: true,
                source_modified: 0,
                fetched_at: 0,
            },
        )
        .unwrap();
        assert!(!is_own_file(&file), "taken from another node");
    }
}
