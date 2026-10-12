//! Signed sets in a node: sets in the data format (`plumb-format`) that the
//! node takes from the network, kept as `plumb set` writes them, in
//! `DATA/sets/<scope>/<name>/`, and passed on over `/plumb/sets/1` (see
//! [`plumb_net::sets`]).
//!
//! Every [`LOOK_EVERY`] the node asks the nodes it meets for the newest
//! manifest of each set it takes ([`TAKEN_SETS`]). It fetches one that is
//! signed for a publisher it trusts ([`NodeConfig::set_publishers`], the
//! project's key by default) and is newer than its own, part by part, each
//! by its hash, from whichever node holds it. It checks the whole set as
//! `plumb set check` does, and only then puts it in place of the old one.
//! So a set can come from any node, trusted or not.
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
//! [`NodeConfig::set_publishers`]: super::NodeConfig::set_publishers

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
pub(super) const TAKEN_SETS: &[&str] = &[DENYLIST, SITE_RULES];

/// The folder of the signed sets in the data directory.
pub(super) const DIR: &str = "sets";

/// How often the node looks for newer sets.
#[cfg(not(test))]
const LOOK_EVERY: Duration = Duration::from_secs(6 * 3600);
#[cfg(test)]
const LOOK_EVERY: Duration = Duration::from_secs(1);
/// Wait before looking again when no node that serves sets was connected.
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
enum Looked {
    /// No node that serves sets was connected.
    NoPeer,
    /// None had a newer generation from a trusted publisher.
    Current,
    /// Took this generation.
    Took(String),
}

/// Keeps the sets the node takes up to date until the node stops.
pub(super) async fn run(inner: Arc<Inner>) {
    let mut stopped = inner.stopped.clone();
    loop {
        let Some(net) = network::handle(&inner).cloned() else {
            return;
        };
        let mut met = false;
        for set in TAKEN_SETS {
            match look(&inner, &net, set).await {
                Ok(Looked::NoPeer) => {}
                Ok(Looked::Current) => met = true,
                Ok(Looked::Took(generation)) => {
                    met = true;
                    info!("took {set} generation {generation}");
                    // The denylist applies at once; the site rules from
                    // the next start.
                    if *set == DENYLIST {
                        let inner = inner.clone();
                        let _ = tokio::task::spawn_blocking(move || inner.load_denylist()).await;
                    }
                }
                Err(err) => {
                    met = true;
                    warn!("taking {set}: {err:#}");
                }
            }
            if inner.stopping() {
                return;
            }
        }
        for (set, pages) in page_sets_kept(&inner) {
            if inner.download_pause().is_some() {
                break;
            }
            match look_page_set(&inner, &net, set, pages).await {
                Ok(Looked::NoPeer) => {}
                Ok(Looked::Current) => met = true,
                Ok(Looked::Took(generation)) => {
                    met = true;
                    info!("took plumb/{} generation {generation}", set.id);
                }
                Err(err) => {
                    met = true;
                    warn!("taking plumb/{}: {err:#}", set.id);
                }
            }
            if inner.stopping() {
                return;
            }
        }
        let wait = if met { LOOK_EVERY } else { RETRY_WAIT };
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
    while asked.len() < MAX_ASKED {
        let request = SetsRequest::Manifest {
            set: set.to_string(),
        };
        let Some(answer) = net.ask_sets(request, None, &asked).await? else {
            break;
        };
        asked.push(answer.peer);
        let SetsResponse::Manifest(Some(text)) = answer.response else {
            continue;
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
        None => Offer::Current,
    })
}

/// Asks the connected nodes for `set`'s newest manifest and takes the
/// newest one a trusted publisher signed, when it is newer than the
/// node's own.
async fn look(inner: &Inner, net: &NetHandle, set: &str) -> Result<Looked> {
    let held = held_generation(&inner.paths.data, set);
    match ask_newest(inner, net, set, held).await? {
        Offer::NoPeer => Ok(Looked::NoPeer),
        Offer::Current => Ok(Looked::Current),
        Offer::Newer {
            text,
            from,
            publisher,
        } => {
            let all = Want {
                parts: usize::MAX,
                at_least: usize::MAX,
            };
            take(inner, net, set, &text, Some(from), publisher, all).await
        }
    }
}

/// The page sets the node keeps pages of that come as item sets, and how
/// many pages of each.
fn page_sets_kept(inner: &Inner) -> Vec<(&'static SetInfo, u64)> {
    let mut settings = inner.settings();
    if inner.config.blackhole {
        settings.page_sets = settings.page_sets.all_unless_set();
    }
    wanted_counts(&settings.page_sets, settings.storage_limit_mb)
        .into_iter()
        .filter(|(set, _)| Page::has_reader(set.id))
        .collect()
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
/// are taken again when more are wanted.
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
    let (manifest, signer) = Manifest::from_signed_json(text, &now())?;
    ensure!(manifest.set == set, "it is a manifest of {}", manifest.set);
    ensure!(manifest.may_share(), "it is a personal set");
    let publisher = inner
        .config
        .set_publishers
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
    {
        let (staging, budget) = (staging.clone(), budget.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            // A folder left by a look that stopped is this job's own.
            storage::remove_directory(&staging, budget.as_deref())?;
            storage::create_directory(&staging, budget.as_ref())
        })
        .await??;
    }
    let mut parts = 0;
    for part in manifest.files.iter().take(want.parts) {
        if inner.stopping() {
            bail!("the node is stopping");
        }
        let dest = staging.join(&part.path);
        if let Some(held) = held_part(&data, &part.sha256) {
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
    tokio::task::spawn_blocking(move || -> Result<()> {
        storage::write_atomic(
            &staging.join(MANIFEST),
            &staging.join("manifest.json.part"),
            text.as_bytes(),
            budget_for_check.as_ref(),
            None,
        )?;
        check_first_parts(&staging, Some(&publisher), &[], parts)?;
        swap(&dir, &staging, budget_for_check.as_deref())
    })
    .await??;
    Ok(Looked::Took(manifest.generation))
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
    let mut file = {
        let (dest, budget) = (dest.to_path_buf(), budget.clone());
        tokio::task::spawn_blocking(move || {
            let _ = storage::remove_file(&dest, budget.as_deref());
            BudgetFile::create(&dest, budget)
        })
        .await
        .map_err(|err| (None, err.into()))?
        .map_err(|err| (None, err.into()))?
    };
    let mut offset = 0u64;
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
    let dest = dest.to_path_buf();
    let measured = tokio::task::spawn_blocking(move || -> Result<(u64, String)> {
        file.flush()?;
        file.sync_all()?;
        drop(file);
        measure(&dest)
    })
    .await
    .map_err(|err| (None, err.into()))?
    .map_err(|err| (None, err))?;
    if measured != (part.bytes, part.sha256.clone()) {
        return Err((peer, anyhow::anyhow!("what it sent is not the part")));
    }
    Ok(())
}

/// Copies a held part into the set being taken.
fn copy(from: &Path, to: &Path, budget: Option<Arc<StorageBudget>>) -> Result<()> {
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
