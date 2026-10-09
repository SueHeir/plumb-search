//! Keeps the node's page index ([`crate::pages`]) in step with its page
//! set files and settings, and adds pages to search results.
//!
//! A node in the network that keeps a set but has no file of it, or too
//! few pages of it, takes the file from a node it trusts (see
//! `plumb_net::pages`), only as far as the pages it keeps, and takes a
//! newer one when a trusted node has it (see [`super::newer`]). A node
//! with no storage limit, or one with a map file already, keeps the map
//! file the same way.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use std::io::Write;

use anyhow::{bail, Context, Result};
use plumb_core::{now_unix, Operators};
use plumb_index::pages::{
    add_named_site, drop_namesakes_of_words, lift_named_sites, options_allow, place_operator_pages,
    place_pages, OPERATOR_PAGES,
};
use plumb_index::{SearchOptions, SearchResults};
use plumb_net::pages::MAX_PAGES_CHUNK;
use plumb_net::NetHandle;
use tracing::{debug, info, warn};

use super::{Inner, NodeSettings};
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
/// Wait after a node said it was busy.
pub(super) const BUSY_WAIT: Duration = Duration::from_secs(5);
/// Wait before asking again after no trusted node had the set, or a
/// download failed.
const FETCH_RETRY_WAIT: Duration = Duration::from_secs(15 * 60);

/// Runs until the node stops, on a blocking thread.
pub(super) fn run(inner: Arc<Inner>) {
    remove_stale_parts(&inner.paths.data);
    // Downloads run beside the index: one can take hours (a big file, a
    // busy or slow trusted node), and the pages already here are searched
    // meanwhile. A file taken is indexed at the next look.
    let runtime = tokio::runtime::Handle::current();
    // Ends the downloads when this ends, by a panic too: the loop started
    // again after one starts its own.
    let files_done = FilesDone(Arc::new(AtomicBool::new(false)));
    let files = {
        let inner = inner.clone();
        let done = files_done.0.clone();
        std::thread::Builder::new()
            .name("page set files".into())
            .spawn(move || {
                let _entered = runtime.enter();
                keep_files(&inner, &done);
            })
    };
    let files = match files {
        Ok(files) => Some(files),
        Err(err) => {
            warn!("page sets: no thread for downloads: {err}");
            None
        }
    };
    let mut failed: Option<(String, Instant)> = None;
    let mut places_failed: Option<(String, Instant)> = None;
    let mut kept_whole = HashSet::new();
    while !inner.stopping() {
        let mut settings = inner.settings();
        if inner.config.blackhole {
            settings.page_sets = settings.page_sets.all_unless_set();
        }
        // Files are cut before they are indexed, not after: an index of
        // the whole file would be built again at once.
        let near = super::places::known_homes(&inner);
        let counts = wanted_counts(&settings.page_sets, settings.storage_limit_mb);
        // Only a node with a storage limit cuts its files: a server's are
        // handed on to other nodes whole.
        for &(set, pages) in counts.iter().filter(|_| settings.storage_limit_mb > 0) {
            if !may_cut(set, pages, near.is_some()) {
                continue;
            }
            let near = near_of(&settings, near.as_deref(), set, pages);
            if let Err(err) = cut_if_longer(&inner, set, pages, &near, &mut kept_whole) {
                warn!("page set {}: {err:#}", set.id);
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
    drop(files_done);
    if let Some(files) = files {
        let _ = files.join();
    }
}

/// Tells the downloads thread to end when dropped.
struct FilesDone(Arc<AtomicBool>);

impl Drop for FilesDone {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Takes the set files missing, or short of pages, from a trusted node,
/// until the node stops.
fn keep_files(inner: &Inner, done: &AtomicBool) {
    // By set: a set the trusted node lacks, or whose download failed, does
    // not hold up the others.
    let mut fetch_failed: HashMap<&'static str, Instant> = HashMap::new();
    // When each set was last checked for a newer file.
    let mut checked: HashMap<&'static str, Instant> = HashMap::new();
    let ended = || inner.stopping() || done.load(Ordering::Relaxed);
    date_taken_files(&inner.paths.data);
    while !ended() {
        let mut settings = inner.settings();
        if inner.config.blackhole {
            settings.page_sets = settings.page_sets.all_unless_set();
        }
        let near = super::places::known_homes(inner);
        let counts = wanted_counts(&settings.page_sets, settings.storage_limit_mb);
        fetch_failed.retain(|_, at| at.elapsed() < FETCH_RETRY_WAIT);
        if let Some(net) = super::network::handle(inner).cloned() {
            for &(set, pages) in &counts {
                if ended() {
                    break;
                }
                if fetch_failed.contains_key(set.id) {
                    continue;
                }
                let near = near_of(&settings, near.as_deref(), set, pages);
                if let Err(err) = fetch_if_needed(inner, &net, set, pages, &near, &mut checked) {
                    warn!("page set {}: {err:#}", set.id);
                    fetch_failed.insert(set.id, Instant::now());
                }
            }
            if !ended()
                && !fetch_failed.contains_key(MAP_SET)
                && (settings.storage_limit_mb == 0 || crate::map::file(&inner.paths.data).is_file())
            {
                if let Err(err) = keep_map(inner, &net, &mut checked) {
                    warn!("map file: {err:#}");
                    fetch_failed.insert(MAP_SET, Instant::now());
                }
            }
        }
        // What the whole articles files hold, worked out once for each, so
        // other nodes asking learn it with the file's time.
        for id in super::newer::LAYERED_SETS {
            if let Some(file) =
                SetInfo::find(id).and_then(|set| set.servable_file(&inner.paths.data))
            {
                super::newer::layers(id, &file);
            }
        }
        let until = Instant::now() + LOOK_EVERY;
        while !ended() && Instant::now() < until {
            std::thread::sleep(TICK);
        }
    }
}

/// The towns whose places the file of `set` keeps past its first `pages`,
/// for the places set on a node with a storage limit (none otherwise).
fn near_of(
    settings: &NodeSettings,
    homes: Option<&[(f64, f64)]>,
    set: &SetInfo,
    pages: u64,
) -> Vec<(f64, f64)> {
    if set.id == plumb_index::places::PLACES_SET && settings.storage_limit_mb > 0 {
        crate::places::file_near(pages, homes.unwrap_or_default()).to_vec()
    } else {
        Vec::new()
    }
}

/// Holds a set's file for one writer: a download or a cut, never both,
/// since each writes the file's `.part` and renames it over the file.
struct SetFileHold<'a> {
    inner: &'a Inner,
    id: &'static str,
}

impl<'a> SetFileHold<'a> {
    /// `None` while another writer holds the file.
    fn take(inner: &'a Inner, id: &'static str) -> Option<Self> {
        let taken = inner
            .set_files_busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id);
        // Not `then_some`: a hold made and dropped would let the other
        // writer's go.
        if taken {
            Some(SetFileHold { inner, id })
        } else {
            None
        }
    }
}

impl Drop for SetFileHold<'_> {
    fn drop(&mut self) {
        self.inner
            .set_files_busy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(self.id);
    }
}

/// Takes the file of `set` from a trusted node when this node has none,
/// fewer than `pages` of it, or an old one the other node has a newer
/// version of. Past `pages`, only the places `near` the node's towns are
/// kept (the places set on a node with a storage limit), not the whole
/// file.
fn fetch_if_needed(
    inner: &Inner,
    net: &NetHandle,
    set: &SetInfo,
    pages: u64,
    near: &[(f64, f64)],
    checked: &mut HashMap<&'static str, Instant>,
) -> Result<()> {
    let data = &inner.paths.data;
    let notes = set.file_notes(data);
    let now = now_unix();
    let near_key = crate::places::near_key(near);
    let reason = match notes {
        None => "none yet",
        Some(n) if !n.complete && near_key != 0 && n.near != near_key => {
            "places near other towns wanted"
        }
        Some(n) if !n.complete && n.lines < pages => "more pages wanted",
        Some(_)
            if checked
                .get(set.id)
                .is_some_and(|at| at.elapsed() < super::newer::CHECK_EVERY) =>
        {
            return Ok(())
        }
        Some(_) => "a newer file",
    };
    if let Some(pause) = inner.download_pause() {
        debug!("page set {}: not downloaded now: {}", set.id, pause.reason);
        return Ok(());
    }
    // What each trusted node has, and the file worth taking.
    let Some(offers) = super::newer::offers(inner, net, set.id)? else {
        debug!("page set {}: no trusted node serves page sets", set.id);
        return Ok(());
    };
    let chosen = match notes {
        Some(n) if reason == "a newer file" => {
            checked.insert(set.id, Instant::now());
            let Some(may_grow) = inner.config.set_updates.allows(set.id) else {
                return Ok(());
            };
            let file = set.file(data);
            let (modified, size) = super::newer::stamp(&file).unwrap_or((0, 0));
            let mine = super::newer::Mine {
                // A cut file keeps its own time; a whole one has its maker's.
                modified: if n.complete {
                    modified
                } else {
                    n.source_modified
                },
                size,
                complete: n.complete,
                layers: super::newer::layers(set.id, &file),
                may_grow,
            };
            match super::newer::newest(&mine, &offers, now) {
                Ok(offer) => offer.clone(),
                Err(why) => {
                    debug!("page set {}: kept: {why}", set.id);
                    return Ok(());
                }
            }
        }
        // Any file is better than none, or than too few pages.
        _ => match offers.iter().max_by_key(|o| o.modified) {
            Some(offer) => offer.clone(),
            None => bail!("no trusted node has a {} file", set.id),
        },
    };
    let runtime = tokio::runtime::Handle::current();
    let first = loop {
        match runtime.block_on(net.pages_chunk(
            set.id,
            0,
            MAX_PAGES_CHUNK,
            Some(chosen.peer),
            &[],
        ))? {
            None => bail!("{} went away before {} was taken", chosen.peer, set.id),
            Some(chunk) if chunk.busy => {
                if !wait(inner, BUSY_WAIT) {
                    return Ok(());
                }
            }
            Some(chunk) if chunk.size == 0 => {
                bail!("{} no longer has a {} file", chosen.peer, set.id)
            }
            Some(chunk) => break chunk,
        }
    };
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
    let Some(_hold) = SetFileHold::take(inner, set.id) else {
        debug!(
            "page set {}: its file is being cut; downloading later",
            set.id
        );
        return Ok(());
    };
    let file = set.file(data);
    std::fs::create_dir_all(file.parent().context("a set file has a folder")?)?;
    let mut part = file.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let taken = take(inner, net, set, &part, first, pages, near);
    if !matches!(taken, Ok(Some(_))) {
        // Stopped, or failed: the part is of no use to a later try.
        let _ = std::fs::remove_file(&part);
    }
    let Some((lines, complete, modified, offset)) = taken? else {
        return Ok(());
    };
    super::newer::install(&part, &file, modified)?;
    write_notes(
        &file,
        &SetFileNotes {
            lines,
            complete,
            source_modified: modified,
            fetched_at: now,
            near: if complete { 0 } else { near_key },
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

/// Downloads the file of `set` into `part`, from the node that sent its
/// `first` piece; every later piece is asked of that same node. Gives the
/// pages written, whether the whole file was, the file's time at that node
/// and the bytes downloaded; `None` when the node stopped meanwhile.
fn take(
    inner: &Inner,
    net: &NetHandle,
    set: &SetInfo,
    part: &std::path::Path,
    first: plumb_net::pages::PagesChunk,
    pages: u64,
    near: &[(f64, f64)],
) -> Result<Option<(u64, bool, u64, u64)>> {
    let runtime = tokio::runtime::Handle::current();
    let from = first.peer;
    let mut cutter = SetFileCutter::create(part, pages)?;
    if !near.is_empty() {
        cutter = cutter.keep_past(crate::places::near_lines(near.to_vec()));
    }
    let mut decoder = flate2::write::MultiGzDecoder::new(cutter);
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
        if let Err(err) = inner.add_downloaded(chunk.bytes.len() as u64) {
            warn!("page set {}: counting the download: {err:#}", set.id);
        }
        if decoder.get_ref().full() || offset >= chunk.size || chunk.bytes.is_empty() {
            break;
        }
        if inner.stopping() || inner.owner_pause().is_some() {
            return Ok(None);
        }
        let (size, modified) = (chunk.size, chunk.modified);
        chunk = loop {
            match runtime.block_on(net.pages_chunk(
                set.id,
                offset,
                MAX_PAGES_CHUNK,
                Some(from),
                &[],
            ))? {
                None => bail!("{from} went away while {} was taken", set.id),
                Some(next) if next.busy => {
                    if !wait(inner, BUSY_WAIT) {
                        return Ok(None);
                    }
                }
                Some(next) if next.size != size || next.modified != modified => {
                    bail!("{from} got a new {} file while it was taken", set.id)
                }
                Some(next) => break next,
            }
        };
    }
    let cutter = decoder.get_mut();
    let complete = offset >= chunk.size && !cutter.cut();
    let lines = cutter.pages();
    cutter.finish()?;
    drop(decoder);
    Ok(Some((lines, complete, chunk.modified, offset)))
}

/// The name the map file goes by between nodes.
pub(super) const MAP_SET: &str = "map";

/// Gives each whole set file taken from another node its maker's time, as
/// [`super::newer::install`] does, for files taken before it did: a node
/// hands a file on with its time, and must not pass off a taken file as a
/// newer one.
fn date_taken_files(data: &std::path::Path) {
    for set in crate::pages::SETS {
        let Some(notes) = set.file_notes(data) else {
            continue;
        };
        let file = set.file(data);
        if is_own_file(&file) || !notes.complete || notes.source_modified == 0 {
            continue;
        }
        if super::newer::stamp(&file).is_some_and(|(at, _)| at != notes.source_modified) {
            if let Err(err) = super::newer::set_time(&file, notes.source_modified) {
                warn!("page set {}: {err:#}", set.id);
            }
        }
    }
}

/// Takes the map file (see [`crate::map`]) from a trusted node when this
/// node has none or a trusted node has a newer one, whole: it is read by
/// tile, so it can't be cut.
fn keep_map(
    inner: &Inner,
    net: &NetHandle,
    checked: &mut HashMap<&'static str, Instant>,
) -> Result<()> {
    if checked
        .get(MAP_SET)
        .is_some_and(|at| at.elapsed() < super::newer::CHECK_EVERY)
    {
        return Ok(());
    }
    let file = crate::map::file(&inner.paths.data);
    // A node with no map file takes one whatever --set-updates says.
    let may_grow = match inner.config.set_updates.allows(MAP_SET) {
        Some(may_grow) => may_grow,
        None if file.is_file() => return Ok(()),
        None => false,
    };
    if inner.download_pause().is_some() {
        return Ok(());
    }
    let Some(offers) = super::newer::offers(inner, net, MAP_SET)? else {
        return Ok(());
    };
    checked.insert(MAP_SET, Instant::now());
    let (modified, size) = super::newer::stamp(&file).unwrap_or((0, 0));
    let mine = super::newer::Mine {
        modified,
        size,
        // With no file yet, any size goes.
        complete: size > 0,
        layers: Vec::new(),
        may_grow,
    };
    let offer = match super::newer::newest(&mine, &offers, now_unix()) {
        Ok(offer) => offer.clone(),
        Err(why) => {
            debug!("map file: kept: {why}");
            return Ok(());
        }
    };
    info!(
        "taking the map file from {}: {} MB",
        offer.peer,
        offer.size / 1_000_000
    );
    inner
        .journal
        .info("Downloading the map file from a node you trust");
    std::fs::create_dir_all(crate::pages::sets_dir(&inner.paths.data))?;
    let part = super::newer::prev_path(&file).with_extension("part");
    let taken = take_whole(inner, net, MAP_SET, &offer, &part);
    if !matches!(taken, Ok(true)) {
        let _ = std::fs::remove_file(&part);
    }
    if !taken? {
        return Ok(());
    }
    super::newer::install(&part, &file, offer.modified)?;
    inner.journal.info(format!(
        "Map file taken ({} MB downloaded)",
        offer.size.div_ceil(1_000_000)
    ));
    Ok(())
}

/// Downloads the whole file `offer` is of into `part`, piece by piece from
/// that node; `false` when the node stopped meanwhile.
fn take_whole(
    inner: &Inner,
    net: &NetHandle,
    set: &str,
    offer: &super::newer::Offer,
    part: &std::path::Path,
) -> Result<bool> {
    let runtime = tokio::runtime::Handle::current();
    let mut out = std::io::BufWriter::new(
        std::fs::File::create(part).with_context(|| format!("creating {}", part.display()))?,
    );
    let mut offset = 0u64;
    while offset < offer.size {
        if inner.stopping() || inner.owner_pause().is_some() {
            return Ok(false);
        }
        let chunk = match runtime.block_on(net.pages_chunk(
            set,
            offset,
            MAX_PAGES_CHUNK,
            Some(offer.peer),
            &[],
        ))? {
            None => bail!("{} went away while the {set} file was taken", offer.peer),
            Some(chunk) if chunk.busy => {
                if !wait(inner, BUSY_WAIT) {
                    return Ok(false);
                }
                continue;
            }
            Some(chunk) => chunk,
        };
        if chunk.size != offer.size || chunk.modified != offer.modified {
            bail!("{} got a new {set} file while it was taken", offer.peer);
        }
        if chunk.bytes.is_empty() {
            bail!("{} sent nothing of its {set} file at {offset}", offer.peer);
        }
        out.write_all(&chunk.bytes)?;
        offset += chunk.bytes.len() as u64;
        if let Err(err) = inner.add_downloaded(chunk.bytes.len() as u64) {
            warn!("{set} file: counting the download: {err:#}");
        }
    }
    out.flush()?;
    out.get_ref().sync_all()?;
    Ok(true)
}

/// Removes the `.part` files a download left when the node stopped hard
/// (a crash, a kill): downloads run only in this job, so none is under
/// way when it starts.
fn remove_stale_parts(data: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(crate::pages::sets_dir(data)) else {
        return;
    };
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().is_some_and(|e| e == "part") {
            match std::fs::remove_file(&path) {
                Ok(()) => info!("removed {}, left by an unfinished download", path.display()),
                Err(err) => warn!("could not remove {}: {err}", path.display()),
            }
        }
    }
}

/// Cuts the file of `set` to its first `pages` pages when it holds more:
/// the node keeps no more than that (the storage limit or the panel's
/// choice went down), and the rest takes room for nothing. Taken again
/// from a trusted node when more are wanted later.
///
/// A file with no notes is the user's own (made by `plumb fetch-pages`),
/// not one taken from another node, so it is never cut: it could not be
/// taken back. `kept_whole` holds the sets already said so, to say it once.
///
/// With towns `near` (the places set), the places near them are kept past
/// the first `pages`, from a whole file; one already cut around them is
/// left as it is.
fn cut_if_longer(
    inner: &Inner,
    set: &SetInfo,
    pages: u64,
    near: &[(f64, f64)],
    kept_whole: &mut HashSet<&'static str>,
) -> Result<()> {
    let data = &inner.paths.data;
    let Some(notes) = set.file_notes(data) else {
        return Ok(());
    };
    let near_key = crate::places::near_key(near);
    if pages == u64::MAX || !needs_cut(&notes, pages, near_key) || inner.stopping() {
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
    // A download of the set will replace the file anyway; the index goes
    // on with it as it is until then.
    let Some(_hold) = SetFileHold::take(inner, set.id) else {
        return Ok(());
    };
    let mut part = file.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let before = std::fs::metadata(&file).map_or(0, |m| m.len());
    let mut reader = plumb_ingest::open_maybe_gz(&file)?;
    let mut cutter = SetFileCutter::create(&part, pages)?;
    // The places past the first: near the towns, and the specialties
    // (brewpubs, climbing gyms) anywhere. Read from disk, so the whole
    // file is cheap to go through.
    if set.id == plumb_index::places::PLACES_SET {
        cutter = cutter.keep_past(crate::places::near_lines(near.to_vec()));
    }
    let mut buf = vec![0u8; 1 << 16];
    while !cutter.full() {
        if inner.stopping() {
            drop(cutter);
            let _ = std::fs::remove_file(&part);
            return Ok(());
        }
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
            near: near_key,
            ..notes
        },
    )?;
    let after = std::fs::metadata(&file).map_or(0, |m| m.len());
    inner.recount_disk();
    info!("page set {}: kept {lines} pages of its file", set.id);
    inner.journal.info(format!(
        "{}: kept {} pages, freeing {} MB",
        set.name,
        thousands(lines),
        before.saturating_sub(after) / 1_000_000
    ));
    Ok(())
}

/// Whether a file with `notes` holds more than a node keeping its first
/// `pages` and the places near the towns `near_key` names wants. A file
/// cut around other towns lacks the places near these: it is taken again
/// instead ([`fetch_if_needed`]).
fn needs_cut(notes: &SetFileNotes, pages: u64, near_key: u64) -> bool {
    if near_key == 0 {
        notes.lines > pages
    } else {
        notes.complete && notes.near != near_key
    }
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
pub(super) fn wait(inner: &Inner, wait: Duration) -> bool {
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

/// Whether the pages know the words of `query` that `spelling` changes
/// ([`PageSearcher::check_spelling`]): then it was spelled as meant.
pub(super) fn knows_typed(inner: &Inner, query: &str, spelling: &plumb_index::Spelling) -> bool {
    let Some(searcher) = inner
        .pages
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(_, s)| s.clone())
    else {
        return false;
    };
    match searcher.check_spelling(query, spelling.clone()) {
        Ok(checked) => checked.is_none_or(|checked| checked.query != spelling.query),
        Err(err) => {
            warn!("checking a spelling against pages: {err:#}");
            false
        }
    }
}

/// The song or album a search of `query` alone is surely for; see
/// [`PageSearcher::known_song`].
pub(super) fn known_song(
    inner: &Inner,
    query: &str,
    options: &SearchOptions,
) -> Option<plumb_index::pages::Page> {
    let searcher = inner
        .pages
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(_, s)| s.clone())?;
    match searcher.known_song(query) {
        Ok(song) => song.filter(|page| options_allow(options, page)),
        Err(err) => {
            warn!("looking for a song: {err:#}");
            None
        }
    }
}

/// The Wiktionary word `name` is; see [`PageSearcher::definition`].
pub(super) fn definition(inner: &Inner, name: &str) -> Option<plumb_index::pages::Page> {
    let searcher = inner
        .pages
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(_, s)| s.clone())?;
    match searcher.definition(name) {
        Ok(word) => word,
        Err(err) => {
            warn!("looking up a word: {err:#}");
            None
        }
    }
}

/// Adds the pages found for `query` to `results`. When the results are for
/// a spelling of it ([`plumb_index::Spelling::applied`]), the pages are
/// for that spelling too.
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
    let applied = results
        .spelling
        .as_ref()
        .filter(|spelling| spelling.applied)
        .map(|spelling| spelling.query.clone());
    let query = applied.as_deref().unwrap_or(query);
    match searcher.search(query, PAGES_PER_SEARCH) {
        Ok(mut found) => {
            if let Err(err) =
                searcher.add_other_number(query, &results.hits, &mut found, PAGES_PER_SEARCH)
            {
                warn!("searching pages in the other number: {err:#}");
            }
            found.retain(|hit| options_allow(options, &hit.page));
            if let Some(index) = inner.current().filter(|_| inner.rank.add_named_site) {
                add_named_site(&mut results.hits, &found, |domain| {
                    index.backend().site(domain)
                });
            }
            if inner.rank.drop_namesakes {
                drop_namesakes_of_words(&mut results.hits, &found);
            }
            lift_named_sites(&mut results.hits, &found);
            if let Err(err) = searcher.note_demand(&mut results.hits) {
                warn!("reading what sites' articles are read: {err:#}");
            }
            // A query that names a package or a page, or asks a question
            // in full, is spelled right: "serde crate" is not "serde
            // create", and "git undo last commit" is not "git und".
            let spelled_right = found
                .iter()
                .any(|hit| hit.page.package.is_some() || hit.named || hit.whole);
            if spelled_right && applied.is_none() {
                results.spelling = None;
            }
            // A query that is a page's whole name is about what the page
            // is, not a search inside a site its first word names:
            // "virginia woolf" is not virginia.gov's search for "woolf",
            // nor "the last of us" last.fm's.
            if let Some(link) = &results.site_search {
                if found
                    .iter()
                    .any(|hit| hit.named && hit.page.site.as_deref() != Some(link.domain.as_str()))
                {
                    results.site_search = None;
                }
            }
            if let Some(spelling) = results.spelling.take_if(|_| applied.is_none()) {
                results.spelling = match searcher.check_spelling(query, spelling.clone()) {
                    Ok(checked) => checked,
                    Err(err) => {
                        warn!("checking a spelling against pages: {err:#}");
                        Some(spelling)
                    }
                };
            }
            // Words of things, not sites ("anubas", "budafest"): the
            // pages' names know them.
            if results.spelling.is_none() && !spelled_right {
                if let Some(index) = inner.current() {
                    let sites = index.backend().searcher();
                    let site_known =
                        |word: &str| sites.word_sites(word) >= plumb_index::KNOWN_WORD_SITES;
                    match searcher.suggest_spelling(query, sites.spelling_model(), &site_known) {
                        Ok(suggested) => results.spelling = suggested,
                        Err(err) => warn!("suggesting a spelling from pages: {err:#}"),
                    }
                }
            }
            results.pages = place_pages(query, &results.hits, found);
            if inner.rank.learned {
                plumb_index::learned::reorder(
                    plumb_index::learned::Model::builtin(),
                    query,
                    &mut results.hits,
                    &mut results.pages,
                );
            }
            // The learned order knows nothing of spelling: the site a
            // suggestion names stays second.
            if let Some(site) = results.spelling.as_ref().and_then(|s| s.site.clone()) {
                plumb_index::suggested_site_second(&mut results.hits, &site);
            }
            // Only shown, after the ranking, which weighs a site's own
            // title.
            if let Err(err) = searcher.title_untitled(&mut results.hits) {
                warn!("titling sites from their articles: {err:#}");
            }
        }
        Err(err) => warn!("searching pages: {err:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notes(lines: u64, complete: bool, near: u64) -> SetFileNotes {
        SetFileNotes {
            lines,
            complete,
            source_modified: 0,
            fetched_at: 0,
            near,
        }
    }

    #[test]
    fn files_are_cut_once_around_the_towns() {
        // No towns: cut to the first pages.
        assert!(needs_cut(&notes(2_000, true, 0), 1_000, 0));
        assert!(!needs_cut(&notes(1_000, false, 0), 1_000, 0));
        // Towns: a whole file is cut around them, once.
        assert!(needs_cut(&notes(2_000, true, 0), 1_000, 7));
        assert!(!needs_cut(&notes(1_080, false, 7), 1_000, 7));
        // Cut around other towns: taken again, not cut.
        assert!(!needs_cut(&notes(1_080, false, 9), 1_000, 7));
        // The towns gone: back to the first pages.
        assert!(needs_cut(&notes(1_080, false, 7), 1_000, 0));
    }

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
                near: 0,
            },
        )
        .unwrap();
        assert!(!is_own_file(&file), "taken from another node");
    }

    #[test]
    fn stale_parts_are_removed_and_files_kept() {
        let dir = tempfile::tempdir().unwrap();
        let sets = crate::pages::sets_dir(dir.path());
        std::fs::create_dir_all(&sets).unwrap();
        let part = sets.join("stackexchange.tsv.gz.part");
        let file = sets.join("stackexchange.tsv.gz");
        std::fs::write(&part, b"half").unwrap();
        std::fs::write(&file, b"whole").unwrap();
        remove_stale_parts(dir.path());
        assert!(!part.exists());
        assert!(file.exists());
        // No sets folder yet: nothing to do.
        remove_stale_parts(&dir.path().join("none"));
    }
}
