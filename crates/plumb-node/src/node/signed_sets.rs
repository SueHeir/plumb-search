//! Signed sets in a node: sets in the data format (`plumb-format`) that the
//! node takes from the network, kept as `plumb-tools set` writes them, in
//! `DATA/sets/<scope>/<name>/`, and passed on over `/plumb/sets/1` (see
//! [`plumb_net::sets`]).
//!
//! Every [`LOOK_EVERY`] the node asks the nodes it meets for the newest
//! manifest of each set it takes ([`TAKEN_SETS`]). It fetches one that is
//! signed for a publisher it trusts ([`NodeConfig::set_publishers`], the
//! project's key by default) and is newer than its own, part by part, each
//! by its hash, from whichever node holds it. It checks the whole set
//! ([`crate::sets::check_set`]), and only then puts it in place of the old
//! one. So a set can come from any node, trusted or not.
//!
//! The denylist is put in use at start and as soon as a newer one is
//! taken ([`super::denylist`]). The site rules are read at start
//! ([`use_held_site_rules`]): a set taken while the node runs is used from
//! its next start.
//!
//! The project's page sets come the same way, as item sets (`plumb/<id>`,
//! read by [`crate::pages::HeldPageSet`]), each as far as the node keeps
//! pages of it: the first parts that hold that many, or all of them. With
//! `--set-updates` leaving a set out, the node takes it only when it has
//! no file of it. A node that holds as many pages of a set this way takes
//! no file of it over `/plumb/pages/1`; one that finds no node with the
//! set goes on taking files as before.
//!
//! Crawling only ([`NodeConfig::crawl_only`]), the node takes the
//! denylist and the site rules alone, which its crawls keep to: no adult
//! list, map or page sets, which only searching needs.
//!
//! [`NodeConfig::set_publishers`]: super::NodeConfig::set_publishers
//! [`NodeConfig::crawl_only`]: super::NodeConfig::crawl_only

use plumb_core::locks::Locked;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use plumb_core::storage::{self, BudgetFile, StorageBudget};
use plumb_format::manifest::is_set_name;
use plumb_format::value::unix_seconds;
use plumb_format::{FilePart, Manifest, PublicKey};
use plumb_index::pages::Page;
use plumb_net::proto::{SetsRequest, SetsResponse};
use plumb_net::sets::{is_part_hash, MAX_SET_CHUNK};
use plumb_net::{NetHandle, PeerId};
use tracing::{debug, info, warn};

use super::denylist::DENYLIST;
use super::{network, Inner};
use crate::pages::{wanted_counts, HeldPageSet, SetInfo};
use crate::sets::{check_first_parts, measure, now, MANIFEST};
use crate::site_rules::{self, read_site_rules, use_site_rules};

/// The project's set-signing key, which a node trusts for the sets it
/// takes unless told otherwise. Made on hpc on 2026-10-12; it stays there
/// until people outside the project run nodes.
pub const PROJECT_KEY: &str = "ed25519:MMrfl7-ASNz-h2d2f0ru1EWjAws59Ls3D5TbdfJS98c";

/// The sets every node takes, the denylist first.
pub(super) const TAKEN_SETS: &[&str] = &[DENYLIST, SITE_RULES, super::adult::ADULT_SITES];

/// The sets a node that only crawls takes: those its crawls keep to.
const CRAWLING_SETS: &[&str] = &[DENYLIST, SITE_RULES];

/// The map ([`crate::map`]), taken by a node that keeps one.
pub(crate) const MAP: &str = "plumb/map";

/// The folder of the signed sets in the data directory.
pub(super) const DIR: &str = "sets";

/// How often the node looks for newer sets. In tests, seldom enough that
/// a look at every set stays within the requests a node answers another a
/// minute ([`plumb_net::sets::SETS_REQUESTS_PER_MINUTE`]).
#[cfg(not(test))]
const LOOK_EVERY: Duration = Duration::from_secs(6 * 3600);
#[cfg(test)]
const LOOK_EVERY: Duration = Duration::from_secs(30);
/// Wait before looking again when a set found no node that serves sets
/// connected, or only busy ones.
#[cfg(not(test))]
const RETRY_WAIT: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
const RETRY_WAIT: Duration = Duration::from_millis(300);
/// Wait when a node is busy before asking it again.
#[cfg(not(test))]
const BUSY_WAIT: Duration = Duration::from_secs(5);
#[cfg(test)]
const BUSY_WAIT: Duration = Duration::from_millis(50);
/// Most nodes asked for a set's manifest in one look.
const MAX_ASKED: usize = 8;
/// Most nodes a part is fetched from before the look gives up on it.
const MAX_TRIES: usize = 4;

/// [`PROJECT_KEY`] as a key.
pub fn project_key() -> PublicKey {
    PROJECT_KEY.parse().expect("the project key is a key")
}

/// The folder of the set `set` (`plumb/site-rules`), when it is a set name.
pub(crate) fn set_dir(data: &Path, set: &str) -> Option<PathBuf> {
    is_set_name(set).then(|| data.join(DIR).join(set))
}

/// The folder and manifest of the set `set` held in `data`, when one of
/// `publishers` signed it and this version reads it. The parts are
/// checked as they are taken, so they are not here.
pub(crate) fn held_trusted(
    data: &Path,
    set: &str,
    publishers: &[PublicKey],
) -> Option<(PathBuf, Manifest)> {
    let dir = set_dir(data, set)?;
    let text = fs::read_to_string(dir.join(MANIFEST)).ok()?;
    let (manifest, signer) = Manifest::from_signed_json(&text, &now()).ok()?;
    let trusted = publishers
        .iter()
        .any(|publisher| manifest.trusted(&signer, publisher, &[]).is_ok());
    (manifest.set == set && trusted && manifest.not_understood().is_empty())
        .then_some((dir, manifest))
}

/// The generation (Unix seconds) and size of the map held as the set
/// [`MAP`], when a publisher in `publishers` signed it and its map file
/// is there whole. Its generation is the time of the file it was made
/// from (`plumb-tools set map`), so it compares with a map file's time.
pub(crate) fn held_map(data: &Path, publishers: &[PublicKey]) -> Option<(u64, u64)> {
    let (dir, manifest) = held_trusted(data, MAP, publishers)?;
    let part = manifest
        .files
        .iter()
        .find(|part| part.blob && part.path == crate::map::MAP_FILE)?;
    let held = fs::metadata(dir.join(&part.path)).ok()?.len();
    let generation = unix_seconds(&manifest.generation)?;
    (held == part.bytes).then_some((generation.max(0) as u64, held))
}

/// The manifest held of `set`, as signed; never a personal set's.
pub(super) fn held_manifest(data: &Path, set: &str) -> Option<String> {
    let text = fs::read_to_string(set_dir(data, set)?.join(MANIFEST)).ok()?;
    let manifest: Manifest = serde_json::from_str(&text).ok()?;
    manifest.may_share().then_some(text)
}

/// The held part whose SHA-256 is `sha256`, in whichever set holds it.
pub(super) fn held_part(data: &Path, sha256: &str) -> Option<PathBuf> {
    if !is_part_hash(sha256) {
        return None;
    }
    for scope in fs::read_dir(data.join(DIR)).ok()?.flatten() {
        let Ok(names) = fs::read_dir(scope.path()) else {
            continue;
        };
        for dir in names.flatten() {
            let set = format!(
                "{}/{}",
                scope.file_name().to_string_lossy(),
                dir.file_name().to_string_lossy()
            );
            // Not the `.new` and `.old` folders of a set being replaced.
            if !is_set_name(&set) {
                continue;
            }
            let Ok(text) = fs::read_to_string(dir.path().join(MANIFEST)) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<Manifest>(&text) else {
                continue;
            };
            if !manifest.may_share() {
                continue;
            }
            if let Some(part) = manifest.files.iter().find(|part| part.sha256 == sha256) {
                let path = dir.path().join(&part.path);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// The site rules set.
const SITE_RULES: &str = "plumb/site-rules";

/// Puts the site rules set held in `data` in place of the built-in lists
/// ([`crate::site_rules`]) when one of `publishers` signed it. At start,
/// before any site is worked out. A set none of them signed, such as one
/// taken before the publishers changed, is left unused.
pub(super) fn use_held_site_rules(data: &Path, publishers: &[PublicKey]) {
    let Some(dir) = set_dir(data, SITE_RULES).filter(|dir| dir.join(MANIFEST).is_file()) else {
        return;
    };
    let mut failed = None;
    for publisher in publishers {
        let read = match read_site_rules(&dir, publisher, &[]) {
            Ok(read) => read,
            Err(err) => {
                failed = Some(err);
                continue;
            }
        };
        if read.newer > 0 {
            warn!(
                "the site rules set has {} rules newer than this version understands; they are passed over",
                read.newer
            );
        }
        let hash = read.hash.clone();
        if use_site_rules(read) {
            info!("working out sites by the site rules set {hash}");
        } else if site_rules::in_use() != Some(hash.as_str()) {
            warn!(
                "other site rules are in use already; the set held is used when Plumb next starts"
            );
        }
        return;
    }
    match failed {
        Some(err) => warn!("not using the site rules set held: {err:#}"),
        None => warn!("not using the site rules set held: the node trusts no publisher"),
    }
}

/// What one look for a set found.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Looked {
    /// No node that serves sets was connected.
    NoPeer,
    /// A node that may have a newer generation was busy: ask again soon.
    Busy,
    /// None had a newer generation from a trusted publisher.
    Current,
    /// Took this generation.
    Took(String),
}

/// Keeps the sets the node takes up to date until the node stops.
pub(super) async fn run(inner: Arc<Inner>) {
    let mut stopped = inner.stopped.clone();
    settle_map(&inner);
    loop {
        let Some(net) = network::handle(&inner).cloned() else {
            return;
        };
        // Whether a set found no node to ask, or only busy ones.
        let mut unsettled = false;
        for set in taken_sets(inner.config.crawl_only) {
            match look(&inner, &net, set).await {
                Ok(Looked::NoPeer | Looked::Busy) => unsettled = true,
                Ok(Looked::Current) => {}
                Ok(Looked::Took(generation)) => {
                    info!("took {set} generation {generation}");
                    // The denylist and the adult list apply at once; the
                    // site rules from the next start.
                    let inner = inner.clone();
                    if *set == DENYLIST {
                        let _ = tokio::task::spawn_blocking(move || inner.load_denylist()).await;
                    } else if *set == super::adult::ADULT_SITES {
                        let _ = tokio::task::spawn_blocking(move || inner.adult_from_set()).await;
                    }
                }
                Err(err) => {
                    warn!("taking {set}: {err:#}");
                }
            }
            if inner.stopping() {
                return;
            }
        }
        // Not while the map file is taken over `/plumb/pages/1`.
        let hold = super::pages::SetFileHold::take(&inner, super::pages::MAP_SET);
        if let Some((held, max_bytes)) = hold.as_ref().and_then(|_| map_wanted(&inner)) {
            match look_newer(&inner, &net, MAP, held, max_bytes).await {
                Ok(Looked::NoPeer | Looked::Busy) => unsettled = true,
                Ok(Looked::Current) => {}
                Ok(Looked::Took(generation)) => {
                    info!("took {MAP} generation {generation}");
                    settle_map(&inner);
                }
                Err(err) => {
                    warn!("taking {MAP}: {err:#}");
                }
            }
        }
        drop(hold);
        for (set, pages) in page_sets_kept(&inner) {
            if inner.download_pause().is_some() {
                break;
            }
            match look_page_set(&inner, &net, set, pages).await {
                Ok(Looked::NoPeer | Looked::Busy) => unsettled = true,
                Ok(Looked::Current) => {}
                Ok(Looked::Took(generation)) => {
                    info!("took plumb/{} generation {generation}", set.id);
                }
                Err(err) => {
                    warn!("taking plumb/{}: {err:#}", set.id);
                }
            }
            if inner.stopping() {
                return;
            }
        }
        let wait = if unsettled { RETRY_WAIT } else { LOOK_EVERY };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stopped.wait_for(|stop| *stop) => return,
        }
    }
}

/// What asking the connected nodes for a set's manifest found.
enum Offer {
    /// No node that serves sets was connected.
    NoPeer,
    /// None offered a newer generation, but one was busy.
    Busy,
    /// None had a newer generation from a trusted publisher.
    Current,
    /// The newest one, from `from`, signed for `publisher`.
    Newer {
        text: String,
        from: PeerId,
        publisher: PublicKey,
    },
}

/// The generation (Unix seconds) of the set `set` held in `data`.
fn held_generation(data: &Path, set: &str) -> Option<i64> {
    held_manifest(data, set)
        .and_then(|text| serde_json::from_str::<Manifest>(&text).ok())
        .and_then(|manifest| unix_seconds(&manifest.generation))
}

/// Asks the connected nodes for `set`'s newest manifest, for one a trusted
/// publisher signed that is newer than `held`.
async fn ask_newest(inner: &Inner, net: &NetHandle, set: &str, held: Option<i64>) -> Result<Offer> {
    let mut asked: Vec<PeerId> = Vec::new();
    let mut newest: Option<(i64, String, PeerId, PublicKey)> = None;
    let mut busy = false;
    while asked.len() < MAX_ASKED {
        let request = SetsRequest::Manifest {
            set: set.to_string(),
        };
        let Some(answer) = net.ask_sets(request, None, &asked).await? else {
            break;
        };
        asked.push(answer.peer);
        let text = match answer.response {
            SetsResponse::Manifest(Some(text)) => text,
            SetsResponse::Busy => {
                busy = true;
                continue;
            }
            _ => continue,
        };
        let (generation, publisher) = match vet(inner, set, &text) {
            Ok(vetted) => vetted,
            Err(err) => {
                debug!("{set} from {}: {err:#}", answer.peer);
                continue;
            }
        };
        let newer_than = |seconds: Option<i64>| seconds.is_none_or(|seconds| generation > seconds);
        if newer_than(held) && newer_than(newest.as_ref().map(|newest| newest.0)) {
            newest = Some((generation, text, answer.peer, publisher));
        }
    }
    if asked.is_empty() {
        return Ok(Offer::NoPeer);
    }
    Ok(match newest {
        Some((_, text, from, publisher)) => Offer::Newer {
            text,
            from,
            publisher,
        },
        None if busy => Offer::Busy,
        None => Offer::Current,
    })
}

/// Asks the connected nodes for `set`'s newest manifest and takes the
/// newest one a trusted publisher signed, when it is newer than the
/// node's own.
pub(super) async fn look(inner: &Inner, net: &NetHandle, set: &str) -> Result<Looked> {
    let held = held_generation(&inner.paths.data, set);
    look_newer(inner, net, set, held, u64::MAX).await
}

/// [`look`] for a set the node has as new as `held` (Unix seconds),
/// taking none whose blob parts (a file such as the map) come to more
/// than `max_bytes`.
async fn look_newer(
    inner: &Inner,
    net: &NetHandle,
    set: &str,
    held: Option<i64>,
    max_bytes: u64,
) -> Result<Looked> {
    match ask_newest(inner, net, set, held).await? {
        Offer::NoPeer => Ok(Looked::NoPeer),
        Offer::Busy => Ok(Looked::Busy),
        Offer::Current => Ok(Looked::Current),
        Offer::Newer {
            text,
            from,
            publisher,
        } => {
            let manifest: Manifest = serde_json::from_str(&text)?;
            let blobs = manifest.files.iter().filter(|part| part.blob);
            let bytes: u64 = blobs.map(|part| part.bytes).sum();
            if bytes > max_bytes {
                debug!("{set}: the newer generation is much bigger (name it in --set-updates to take it)");
                return Ok(Looked::Current);
            }
            let all = Want {
                parts: usize::MAX,
                at_least: usize::MAX,
            };
            take(inner, net, set, &text, Some(from), publisher, all).await
        }
    }
}

/// The sets of [`TAKEN_SETS`] the node takes.
fn taken_sets(crawl_only: bool) -> &'static [&'static str] {
    if crawl_only {
        CRAWLING_SETS
    } else {
        TAKEN_SETS
    }
}

/// The page sets the node keeps pages of that come as item sets, and how
/// many pages of each: none crawling only.
fn page_sets_kept(inner: &Inner) -> Vec<(&'static SetInfo, u64)> {
    if inner.config.crawl_only {
        return Vec::new();
    }
    let mut settings = inner.settings();
    if inner.config.blackhole {
        settings.page_sets = settings.page_sets.all_unless_set();
    }
    wanted_counts(&settings.page_sets, settings.storage_limit_mb)
        .into_iter()
        .filter(|(set, _)| Page::has_reader(set.id))
        .collect()
}

/// Whether the node takes the map as a set now, the time of the map it
/// has (the set's generation, or its map file's time when newer), so only
/// a newer one is taken, and the most bytes it takes. As for the map
/// file: a node with a storage limit takes one only when it already has a
/// map, and a much bigger one only when `--set-updates` names it. Never
/// crawling only: nothing is searched, and the map is many gigabytes.
fn map_wanted(inner: &Inner) -> Option<(Option<i64>, u64)> {
    if inner.config.crawl_only || inner.download_pause().is_some() {
        return None;
    }
    let data = &inner.paths.data;
    let mine = super::newer::stamp(&crate::map::file(data))
        .into_iter()
        .chain(held_map(data, &inner.config.set_publishers))
        .max();
    let may_grow = match inner.config.set_updates.allows(super::pages::MAP_SET) {
        None if mine.is_some() => return None,
        _ if inner.settings().storage_limit_mb > 0 && mine.is_none() => return None,
        may_grow => may_grow.unwrap_or(false),
    };
    let max_bytes = match mine {
        Some((_, size)) if !may_grow => size.saturating_mul(super::newer::MAX_GROWTH_PERCENT) / 100,
        _ => u64::MAX,
    };
    Some((mine.map(|(time, _)| time as i64), max_bytes))
}

/// Dates the map held as a set by its generation, as a map file taken
/// over `/plumb/pages/1` keeps its maker's time, so it is served to
/// v0.2.0 nodes with that time, and removes an older map file, so the
/// node keeps one map.
fn settle_map(inner: &Inner) {
    let data = &inner.paths.data;
    let Some((generation, _)) = held_map(data, &inner.config.set_publishers) else {
        return;
    };
    let Some(blob) = set_dir(data, MAP).map(|dir| dir.join(crate::map::MAP_FILE)) else {
        return;
    };
    let settled = match super::newer::stamp(&blob) {
        Some((time, _)) if time != generation => super::newer::set_time(&blob, generation),
        _ => Ok(()),
    };
    let file = crate::map::file(data);
    let removed = match super::newer::stamp(&file) {
        Some((time, _)) if time < generation => {
            storage::remove_file(&file, inner.storage.as_deref()).map_err(Into::into)
        }
        _ => Ok(()),
    };
    if let Err(err) = settled.and(removed) {
        warn!("{MAP}: {err:#}");
    }
}

/// How many of a manifest's first parts hold `pages` items: all of them
/// when they hold fewer.
fn parts_for(manifest: &Manifest, pages: u64) -> usize {
    let mut items = 0u64;
    for (n, part) in manifest.files.iter().enumerate() {
        items = items.saturating_add(part.count);
        if items >= pages {
            return n + 1;
        }
    }
    manifest.files.len()
}

/// Removes the parts of the project's item set of `set` held past the
/// first that hold `pages` pages, as a page set file is cut to its first
/// pages: the node keeps no more, and they take room for nothing. They
/// are taken again when more are wanted. Called with the node's
/// `held_page_sets` held.
pub(super) fn trim_page_set(inner: &Inner, set: &SetInfo, pages: u64) -> Result<()> {
    let data = &inner.paths.data;
    let Some(held) = HeldPageSet::find(data, set, &inner.config.set_publishers) else {
        return Ok(());
    };
    let name = format!("plumb/{}", set.id);
    let text = held_manifest(data, &name).context("its manifest is gone")?;
    let (manifest, _) = Manifest::from_signed_json(&text, &now())?;
    let keep = parts_for(&manifest, pages);
    if held.parts.len() <= keep {
        return Ok(());
    }
    // The last first, so what is left is always the set's first parts.
    for part in held.parts[keep..].iter().rev() {
        storage::remove_file(&held.dir.join(part), inner.storage.as_deref())?;
    }
    info!("{name}: kept its first {keep} parts, which hold the {pages} pages kept");
    Ok(())
}

/// Keeps the project's item set of the page set `set` up to date, as far
/// as its first `pages` pages: takes a newer generation, or more parts of
/// the one held when the node keeps more pages now.
async fn look_page_set(
    inner: &Inner,
    net: &NetHandle,
    set: &SetInfo,
    pages: u64,
) -> Result<Looked> {
    let data = &inner.paths.data;
    let name = format!("plumb/{}", set.id);
    let held = HeldPageSet::find(data, set, &inner.config.set_publishers);
    // As for files: with `--set-updates` leaving the set out, a node that
    // has its pages one way or the other keeps them as they are.
    let has_file = set.file(data).is_file();
    if inner.config.set_updates.allows(set.id).is_none() && (has_file || held.is_some()) {
        return Ok(Looked::Current);
    }
    let held_generation = held.as_ref().map(|held| held.generation);
    match ask_newest(inner, net, &name, held_generation).await? {
        Offer::NoPeer => Ok(Looked::NoPeer),
        Offer::Busy => Ok(Looked::Busy),
        Offer::Newer {
            text,
            from,
            publisher,
        } => {
            let (manifest, _) = Manifest::from_signed_json(&text, &now())?;
            let parts = parts_for(&manifest, pages);
            // A first take keeps what it could fetch; a newer generation
            // replaces the one held only whole.
            let at_least = if held.is_some() { parts } else { 1 };
            let want = Want { parts, at_least };
            take(inner, net, &name, &text, Some(from), publisher, want).await
        }
        Offer::Current => {
            let Some(held) = held.filter(|held| !held.holds(pages)) else {
                return Ok(Looked::Current);
            };
            let text = held_manifest(data, &name).context("its manifest is gone")?;
            let (_, publisher) = vet(inner, &name, &text)?;
            let (manifest, _) = Manifest::from_signed_json(&text, &now())?;
            let want = Want {
                parts: parts_for(&manifest, pages),
                at_least: held.parts.len() + 1,
            };
            take(inner, net, &name, &text, None, publisher, want).await
        }
    }
}

/// The generation (Unix seconds) of the manifest `text` and the trusted
/// publisher it is signed for, when it is a good manifest of `set`.
fn vet(inner: &Inner, set: &str, text: &str) -> Result<(i64, PublicKey)> {
    vet_for(&inner.config.set_publishers, set, text)
}

/// [`vet`] for a node that takes sets from `publishers`.
fn vet_for(publishers: &[PublicKey], set: &str, text: &str) -> Result<(i64, PublicKey)> {
    let (manifest, signer) = Manifest::from_signed_json(text, &now())?;
    ensure!(manifest.set == set, "it is a manifest of {}", manifest.set);
    ensure!(manifest.may_share(), "it is a personal set");
    // Taken, it would fail its check after every part was fetched.
    let not_understood = manifest.not_understood();
    ensure!(
        not_understood.is_empty(),
        "this version can't read it: it requires {}",
        not_understood.join(", ")
    );
    let publisher = publishers
        .iter()
        .find(|publisher| manifest.trusted(&signer, publisher, &[]).is_ok())
        .with_context(|| format!("{} is no publisher this node trusts", manifest.publisher))?;
    let generation = unix_seconds(&manifest.generation).context("a generation with no time")?;
    Ok((generation, *publisher))
}

/// How many of a set's first parts to take.
#[derive(Clone, Copy, Debug)]
struct Want {
    /// The parts wanted.
    parts: usize,
    /// The fewest that are worth keeping when a part can't be fetched.
    at_least: usize,
}

/// Fetches the first parts of the manifest `text` of `set` that `want`
/// asks for, from `from` when it sent it, checks them and puts the set in
/// place of the node's own.
async fn take(
    inner: &Inner,
    net: &NetHandle,
    set: &str,
    text: &str,
    from: Option<PeerId>,
    publisher: PublicKey,
    want: Want,
) -> Result<Looked> {
    let data = inner.paths.data.clone();
    let dir = set_dir(&data, set).context("not a set name")?;
    let staging = beside(&dir, "new");
    let budget = inner.storage.clone();
    let (manifest, _) = Manifest::from_signed_json(text, &now())?;
    let fetched = {
        let (staging, budget, files) = (staging.clone(), budget.clone(), manifest.files.clone());
        tokio::task::spawn_blocking(move || keep_fetched(&staging, &files, budget)).await??
    };
    let mut parts = 0;
    for part in manifest.files.iter().take(want.parts) {
        if inner.stopping() {
            bail!("the node is stopping");
        }
        let dest = staging.join(&part.path);
        if fetched.contains(&part.path) {
            // Fetched by a look that stopped before it was swapped in.
        } else if let Some(held) = held_part(&data, &part.sha256) {
            let (dest, budget) = (dest.clone(), budget.clone());
            tokio::task::spawn_blocking(move || copy(&held, &dest, budget)).await??;
        } else if let Err(err) = fetch_part(inner, net, part, &dest, from).await {
            if parts == 0 || parts < want.at_least.min(manifest.files.len()) {
                return Err(err);
            }
            debug!("{set}: keeping its first {parts} parts: {err:#}");
            let _ = tokio::fs::remove_file(&dest).await;
            break;
        } else {
            let _ = inner.add_downloaded(part.bytes);
        }
        parts += 1;
    }
    let text = text.to_string();
    let budget_for_check = budget.clone();
    // A page set's index may be read from it ([`HeldPageSet`]).
    let held_page_sets = set
        .strip_prefix("plumb/")
        .and_then(SetInfo::find)
        .map(|_| inner.held_page_sets.clone());
    tokio::task::spawn_blocking(move || -> Result<()> {
        storage::write_atomic(
            &staging.join(MANIFEST),
            &staging.join("manifest.json.part"),
            text.as_bytes(),
            budget_for_check.as_ref(),
            None,
        )?;
        check_first_parts(&staging, Some(&publisher), &[], parts)?;
        let _held = held_page_sets.as_ref().map(|held| held.locked());
        swap(&dir, &staging, budget_for_check.as_deref())
    })
    .await??;
    Ok(Looked::Took(manifest.generation))
}

/// Readies `staging` for the parts `files` of a set: keeps each part a
/// look that stopped fetched whole into it, and the start of one it was
/// fetching, removes anything else, and says which parts it kept whole.
fn keep_fetched(
    staging: &Path,
    files: &[FilePart],
    budget: Option<Arc<StorageBudget>>,
) -> Result<std::collections::HashSet<String>> {
    let mut kept = std::collections::HashSet::new();
    if !staging.is_dir() {
        storage::remove_directory(staging, budget.as_deref())?;
        storage::create_directory(staging, budget.as_ref())?;
        return Ok(kept);
    }
    for entry in fs::read_dir(staging)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let whole = files.iter().any(|part| {
            part.path == name
                && fs::metadata(&path).is_ok_and(|meta| meta.is_file() && meta.len() == part.bytes)
                && measure(&path).is_ok_and(|(_, sha256)| sha256 == part.sha256)
        });
        // Part of a part, which the next download goes on from.
        let begun = files.iter().any(|part| {
            part.path == name && fs::metadata(&path).is_ok_and(|meta| meta.len() < part.bytes)
        });
        if whole {
            kept.insert(name.to_string());
        } else if begun {
        } else if path.is_dir() {
            storage::remove_directory(&path, budget.as_deref())?;
        } else {
            storage::remove_file(&path, budget.as_deref())?;
        }
    }
    Ok(kept)
}

/// Fetches `part` into `dest`, from `first` when it holds it, else from
/// other nodes.
async fn fetch_part(
    inner: &Inner,
    net: &NetHandle,
    part: &FilePart,
    dest: &Path,
    first: Option<PeerId>,
) -> Result<()> {
    let mut tried: Vec<PeerId> = Vec::new();
    let mut peer = first;
    while tried.len() < MAX_TRIES {
        match download(inner, net, part, dest, peer, &tried).await {
            Ok(_) => return Ok(()),
            Err((Some(from), err)) => {
                debug!("part {} from {from}: {err:#}", part.path);
                tried.push(from);
            }
            Err((None, err)) => return Err(err),
        }
        peer = None;
    }
    bail!("no node sent part {} whole", part.path)
}

/// Fetches `part` into `dest` from `peer`, or from a node not in `skip`.
/// On failure, says which node it was fetching from, if one answered.
async fn download(
    inner: &Inner,
    net: &NetHandle,
    part: &FilePart,
    dest: &Path,
    mut peer: Option<PeerId>,
    skip: &[PeerId],
) -> std::result::Result<(), (Option<PeerId>, anyhow::Error)> {
    let budget = inner.storage.clone();
    // Goes on from what an earlier try fetched of it: a map is gigabytes.
    let (mut file, mut offset) = {
        let (dest, budget, bytes) = (dest.to_path_buf(), budget.clone(), part.bytes);
        tokio::task::spawn_blocking(move || -> std::io::Result<(BudgetFile, u64)> {
            let begun = fs::metadata(&dest).map_or(0, |meta| meta.len());
            let begun = if begun < bytes { begun } else { 0 };
            let mut file = BudgetFile::open_write(&dest, begun == 0, budget)?;
            file.seek(std::io::SeekFrom::Start(begun))?;
            Ok((file, begun))
        })
        .await
        .map_err(|err| (None, err.into()))?
        .map_err(|err| (None, err.into()))?
    };
    let mut busy = 0;
    while offset < part.bytes {
        if inner.stopping() {
            return Err((None, anyhow::anyhow!("the node is stopping")));
        }
        let request = SetsRequest::Part {
            sha256: part.sha256.clone(),
            offset,
            len: MAX_SET_CHUNK,
        };
        let asked = if peer.is_some() { &[][..] } else { skip };
        let answer = match net.ask_sets(request, peer, asked).await {
            Ok(Some(answer)) => answer,
            Ok(None) => {
                return Err((
                    peer,
                    anyhow::anyhow!("no node that might hold it is connected"),
                ))
            }
            Err(err) => return Err((peer, err)),
        };
        let from = answer.peer;
        match answer.response {
            SetsResponse::Busy => {
                busy += 1;
                if busy > 3 {
                    return Err((Some(from), anyhow::anyhow!("busy")));
                }
                peer = Some(from);
                tokio::time::sleep(BUSY_WAIT).await;
            }
            SetsResponse::Part { size, bytes } => {
                if size != part.bytes {
                    return Err((Some(from), anyhow::anyhow!("it holds no such part")));
                }
                if bytes.is_empty() {
                    if offset < size {
                        return Err((Some(from), anyhow::anyhow!("it sent nothing")));
                    }
                    break;
                }
                peer = Some(from);
                offset += bytes.len() as u64;
                file = tokio::task::spawn_blocking(move || -> std::io::Result<BudgetFile> {
                    file.write_all(&bytes)?;
                    Ok(file)
                })
                .await
                .map_err(|err| (None, err.into()))?
                .map_err(|err| (None, err.into()))?;
            }
            SetsResponse::Manifest(_) => {
                return Err((Some(from), anyhow::anyhow!("it sent a manifest")));
            }
        }
    }
    let fetched = dest.to_path_buf();
    let measured = tokio::task::spawn_blocking(move || -> Result<(u64, String)> {
        file.flush()?;
        file.sync_all()?;
        drop(file);
        measure(&fetched)
    })
    .await
    .map_err(|err| (None, err.into()))?
    .map_err(|err| (None, err))?;
    if measured != (part.bytes, part.sha256.clone()) {
        // Not what was begun on either: start again.
        let _ = storage::remove_file(dest, budget.as_deref());
        return Err((peer, anyhow::anyhow!("what it sent is not the part")));
    }
    Ok(())
}

/// Copies a held part into the set being taken.
fn copy(from: &Path, to: &Path, budget: Option<Arc<StorageBudget>>) -> Result<()> {
    storage::remove_file(to, budget.as_deref())?;
    let mut source = fs::File::open(from)?;
    let mut dest = BudgetFile::create(to, budget)?;
    std::io::copy(&mut source, &mut dest)?;
    dest.sync_all()?;
    Ok(())
}

/// Puts the checked set in `staging` in place of the one in `dir`.
fn swap(dir: &Path, staging: &Path, budget: Option<&StorageBudget>) -> Result<()> {
    let old = beside(dir, "old");
    storage::remove_directory(&old, budget)?;
    if dir.exists() {
        fs::rename(dir, &old).with_context(|| format!("moving {}", dir.display()))?;
    }
    fs::rename(staging, dir).with_context(|| format!("moving {}", staging.display()))?;
    storage::remove_directory(&old, budget)?;
    Ok(())
}

/// `dir` with `.<suffix>` added to its name.
fn beside(dir: &Path, suffix: &str) -> PathBuf {
    let mut name = dir.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use serde_json::{json, Value};

    /// A signed set of `plumb/books` with three items in three parts.
    fn books(dir: &Path, key: &SigningKey) -> String {
        let Value::Object(manifest) = json!({"format": plumb_format::FORMAT, "set": "plumb/books",
            "generation": "2026-10-12T04:00:00Z", "license": "CC0-1.0"})
        else {
            unreachable!()
        };
        let items = (1..=3).map(|n| {
            let page: Page = serde_json::from_value(json!({
                "set": "books", "url": format!("https://openlibrary.org/works/OL{n}W"),
                "title": format!("Book {n}"), "views": 10 - n,
            }))
            .unwrap();
            plumb_tools::page_items::page_item(&page)
        });
        let fields = plumb_tools::page_items::page_fields();
        plumb_tools::set::write_set(items, fields, manifest, dir, &[1, 2], 3).unwrap();
        plumb_tools::set::sign_set(dir, key, None, None).unwrap();
        fs::read_to_string(dir.join(MANIFEST)).unwrap()
    }

    #[test]
    fn crawling_only_takes_only_what_crawls_keep_to() {
        assert_eq!(taken_sets(false), TAKEN_SETS);
        assert_eq!(taken_sets(true), [DENYLIST, SITE_RULES]);
    }

    #[test]
    fn a_generation_this_version_cant_read_is_not_taken() {
        let temp = tempfile::tempdir().unwrap();
        let key = SigningKey::from_bytes(&[7; 32]);
        let project = [PublicKey::of(&key)];
        let readable = books(&temp.path().join("a"), &key);
        assert_eq!(
            vet_for(&project, "plumb/books", &readable).unwrap().1,
            project[0]
        );
        let stranger = [PublicKey::of(&SigningKey::from_bytes(&[8; 32]))];
        assert!(vet_for(&stranger, "plumb/books", &readable).is_err());
        assert!(vet_for(&project, "plumb/films", &readable).is_err());
        // The next generation needs what this version lacks.
        let mut value: Value = serde_json::from_str(&readable).unwrap();
        value["generation"] = json!("2026-10-13T04:00:00Z");
        value["requires"] = json!(["x-new-thing"]);
        value.as_object_mut().unwrap().remove("signature");
        plumb_format::sign::sign(&mut value, &key).unwrap();
        let unreadable = serde_json::to_string(&value).unwrap();
        let err = vet_for(&project, "plumb/books", &unreadable).unwrap_err();
        assert!(format!("{err:#}").contains("x-new-thing"), "{err:#}");
    }

    #[test]
    fn parts_a_stopped_look_fetched_whole_are_kept() {
        let temp = tempfile::tempdir().unwrap();
        let key = SigningKey::from_bytes(&[7; 32]);
        let set = temp.path().join("set");
        let text = books(&set, &key);
        let (manifest, _) = Manifest::from_signed_json(&text, &now()).unwrap();
        let staging = temp.path().join("set.new");
        fs::create_dir_all(&staging).unwrap();
        // The first part whole, the second begun, and a stray file.
        let [first, second, _] = &manifest.files[..] else {
            panic!("{:?}", manifest.files);
        };
        fs::copy(set.join(&first.path), staging.join(&first.path)).unwrap();
        let bytes = fs::read(set.join(&second.path)).unwrap();
        fs::write(staging.join(&second.path), &bytes[..bytes.len() / 2]).unwrap();
        fs::write(staging.join("manifest.json"), "{}").unwrap();
        let kept = keep_fetched(&staging, &manifest.files, None).unwrap();
        assert_eq!(kept, [first.path.clone()].into_iter().collect());
        // The begun one stays, for the next download to go on from.
        let mut left: Vec<String> = fs::read_dir(&staging)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, [first.path.clone(), second.path.clone()]);
        // With no folder, one is made.
        let fresh = temp.path().join("fresh.new");
        assert!(keep_fetched(&fresh, &manifest.files, None)
            .unwrap()
            .is_empty());
        assert!(fresh.is_dir());
    }
}
