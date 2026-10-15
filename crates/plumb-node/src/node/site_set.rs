//! The set of sites, `plumb/sites` (`plumb-tools set sites`): the
//! project's node's list of sites as a signed set of `site` items,
//! best-ranked first, which a node sets up and fills from
//! ([`super::fill`]) in place of asking a trusted node, and passes on to
//! any node.
//!
//! Unlike the other sets, which the signed sets job takes whole, it is
//! held part by part, and only while filling reads it. Filling fetches the
//! part it reads next when it reaches it ([`fetch`]), and the first part
//! too: the best sites, which a new node sets up from, so the part most
//! asked for. It removes the parts it is past but the first. While filling
//! reads the set, it looks for a newer generation every [`LOOK_EVERY`], and
//! whenever a part it needs is missing, since no node keeps the parts of a
//! generation a newer one replaced. It puts a newer one in place of the one
//! held, keeping the parts they share; filling reads on in it from the same
//! place, as it does in a trusted node's list, which changes as it is
//! read. A part is fetched into a file beside its own, checked against the
//! manifest, and renamed into place under [`Shared::held`], which reading
//! holds, so filling reads only whole parts.

use std::fs;
use std::io::BufRead;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use plumb_core::locks::Locked;
use plumb_core::storage;
use plumb_format::value::unix_seconds;
use plumb_format::{FilePart, Item, Manifest, PublicKey};
use plumb_net::{NetHandle, PeerId};
use tracing::{debug, info, warn};

use super::signed_sets::{self, held_trusted, set_dir, Offer, LOOK_EVERY};
use super::Inner;
use crate::sets::{measure, now, MANIFEST};

/// The set of sites.
pub(crate) const SITES: &str = "plumb/sites";

/// What filling and setting up share of the set of sites.
#[derive(Default)]
pub(super) struct Shared {
    /// Held while parts are read, and while one is put in place or
    /// removed.
    held: Mutex<()>,
    /// One fetch at a time.
    fetching: tokio::sync::Mutex<()>,
    /// When filling last asked which generation is the newest.
    looked: Mutex<Option<Instant>>,
}

/// Whether the node reads the set of sites: it takes the project's sets,
/// and is not a blackhole, which reads each trusted node's own list.
pub(super) fn taken(inner: &Inner) -> bool {
    !inner.config.blackhole && !inner.config.set_publishers.is_empty()
}

/// Runs `read` on the set of sites held, with no part put in place or
/// removed meanwhile; `None` when the node holds none.
pub(super) fn with_held<T>(inner: &Inner, read: impl FnOnce(&HeldSites) -> T) -> Option<T> {
    let _held = inner.sites.held.locked();
    HeldSites::find(&inner.paths.data, &inner.config.set_publishers).map(|held| read(&held))
}

/// The set of sites held: its manifest, when a publisher the node takes
/// sets from signed it, and which of its parts are here.
pub(super) struct HeldSites {
    dir: PathBuf,
    manifest: Manifest,
    /// Its generation, in Unix seconds.
    generation: i64,
    /// Whether each part is here whole.
    here: Vec<bool>,
}

impl HeldSites {
    /// The set of sites held in `data`, signed by one of `publishers`.
    pub(super) fn find(data: &Path, publishers: &[PublicKey]) -> Option<HeldSites> {
        let (dir, manifest) = held_trusted(data, SITES, publishers)?;
        if manifest.is_denylist() || manifest.is_site_rules() {
            return None;
        }
        let generation = unix_seconds(&manifest.generation)?;
        let here = manifest
            .files
            .iter()
            .map(|part| is_here(&dir, part))
            .collect();
        Some(HeldSites {
            dir,
            manifest,
            generation,
            here,
        })
    }

    /// Sites in the whole set.
    pub(super) fn total(&self) -> u64 {
        self.manifest.files.iter().map(|part| part.count).sum()
    }

    /// The parts that hold the `count` sites from the `from`th on (0 the
    /// first).
    fn parts_for(&self, from: u64, count: u64) -> Range<usize> {
        let end = from.saturating_add(count.max(1));
        let (mut start, mut parts) = (0u64, None::<Range<usize>>);
        for (n, part) in self.manifest.files.iter().enumerate() {
            let next = start.saturating_add(part.count);
            if next > from && start < end {
                parts = Some(parts.map_or(n..n + 1, |parts| parts.start..n + 1));
            }
            start = next;
        }
        parts.unwrap_or(0..0)
    }

    /// The sites from the `from`th on, as lines of JSON, through the parts
    /// here from the one that holds it; none when that one is not here.
    /// Read with [`Shared::held`] held.
    pub(super) fn lines_from(&self, from: u64) -> impl Iterator<Item = Result<String>> + '_ {
        let (mut skip, mut first) = (from, self.manifest.files.len());
        for (n, part) in self.manifest.files.iter().enumerate() {
            if skip < part.count {
                first = n;
                break;
            }
            skip -= part.count;
        }
        let paths = self.manifest.files[first..]
            .iter()
            .zip(&self.here[first..])
            .take_while(|(_, here)| **here)
            .map(|(part, _)| self.dir.join(&part.path));
        paths.flat_map(move |path| part_lines(path, std::mem::take(&mut skip)))
    }

    /// Checks an item of the set against its manifest, as taking a set
    /// does.
    pub(super) fn check(&self, item: &Item) -> Result<()> {
        self.manifest.check_item(item)
    }
}

/// Whether `part` is in `dir` whole. A part is put in place only once
/// checked, so its size says so.
fn is_here(dir: &Path, part: &FilePart) -> bool {
    fs::metadata(dir.join(&part.path)).is_ok_and(|meta| meta.len() == part.bytes)
}

/// The lines of the part at `path` after its first `skip`, and any error
/// reading it.
fn part_lines(path: PathBuf, skip: u64) -> Box<dyn Iterator<Item = Result<String>>> {
    let shown = path.display().to_string();
    let mut reader = match fs::File::open(&path).and_then(zstd::Decoder::new) {
        Ok(decoder) => std::io::BufReader::new(decoder),
        Err(err) => {
            return Box::new(std::iter::once(
                Err(err).with_context(|| format!("reading {shown}")),
            ))
        }
    };
    // Skipped lines are read into one buffer, not a string each.
    let mut skipped = Vec::new();
    for _ in 0..skip {
        skipped.clear();
        match reader.read_until(b'\n', &mut skipped) {
            Ok(0) => return Box::new(std::iter::empty()),
            Ok(_) => {}
            Err(err) => {
                return Box::new(std::iter::once(
                    Err(err).with_context(|| format!("reading {shown}")),
                ))
            }
        }
    }
    Box::new(
        reader
            .lines()
            .map(move |line| line.with_context(|| format!("reading {shown}"))),
    )
}

/// What fetching the set of sites came to.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Fetched {
    /// No node connected answers for sets.
    NoPeer,
    /// The nodes that might hold it were busy.
    Busy,
    /// No node sent the set, or a part wanted.
    Missing,
    /// The day's download limit is reached.
    Paused,
    /// The parts wanted are here.
    Here,
}

/// How often a node setting up asks for the set of sites while no node
/// sends it.
#[cfg(not(test))]
const SETUP_POLL: Duration = Duration::from_secs(2);
#[cfg(test)]
const SETUP_POLL: Duration = Duration::from_millis(200);

/// Setting up: fetches the set of sites' first parts, which hold `want`
/// sites, from a node met before `deadline`. Whether they are here.
pub(super) async fn fetch_first(
    inner: &Arc<Inner>,
    net: &NetHandle,
    want: u64,
    deadline: Instant,
) -> Result<bool> {
    if !taken(inner) {
        return Ok(false);
    }
    loop {
        match fetch_parts(inner, net, 0, want, false).await? {
            Fetched::Here => return Ok(true),
            Fetched::Paused => return Ok(false),
            Fetched::NoPeer | Fetched::Busy | Fetched::Missing if Instant::now() < deadline => {
                tokio::select! {
                    () = tokio::time::sleep(SETUP_POLL) => {}
                    () = inner.stopped() => return Ok(false),
                }
            }
            _ => return Ok(false),
        }
    }
}

/// Filling: brings the parts that hold the `count` sites from the `at`th
/// on, which it reads next, looking for a newer generation first once
/// every [`LOOK_EVERY`] or when the node holds none.
pub(super) async fn fetch(
    inner: &Arc<Inner>,
    net: &NetHandle,
    at: u64,
    count: u64,
) -> Result<Fetched> {
    let look = inner
        .sites
        .looked
        .locked()
        .is_none_or(|looked| looked.elapsed() >= LOOK_EVERY);
    fetch_parts(inner, net, at, count, look).await
}

/// Brings the parts that hold the `count` sites from the `at`th on, and
/// then the first part, which new nodes ask for: looks for a newer
/// generation when `look`, when none is held, or when a part of the one
/// held is missing, as once a newer one takes its place no node holds its
/// parts. Removes the parts before `at` but the first.
async fn fetch_parts(
    inner: &Arc<Inner>,
    net: &NetHandle,
    at: u64,
    count: u64,
    mut look: bool,
) -> Result<Fetched> {
    let _one = inner.sites.fetching.lock().await;
    loop {
        let mut held = blocking(inner, |inner| {
            Ok(HeldSites::find(
                &inner.paths.data,
                &inner.config.set_publishers,
            ))
        })
        .await?;
        let mut from = None;
        if look || held.is_none() {
            let generation = held.as_ref().map(|held| held.generation);
            match signed_sets::ask_newest(inner, net, SITES, generation).await? {
                Offer::NoPeer if held.is_none() => return Ok(Fetched::NoPeer),
                Offer::Busy if held.is_none() => return Ok(Fetched::Busy),
                Offer::NoPeer | Offer::Busy => {}
                Offer::Current => *inner.sites.looked.locked() = Some(Instant::now()),
                Offer::Newer {
                    text, from: peer, ..
                } => {
                    held = Some(blocking(inner, move |inner| install(inner, &text)).await?);
                    from = Some(peer);
                    *inner.sites.looked.locked() = Some(Instant::now());
                }
            }
        }
        let Some(held) = held else {
            return Ok(Fetched::Missing);
        };
        blocking(inner, move |inner| remove_before(inner, at)).await?;
        let mut here = held.here.clone();
        for n in held.parts_for(at, count) {
            match fetch_part(inner, net, &held, n, &mut here, from).await? {
                Fetched::Here => {}
                Fetched::Missing if !look => break,
                fetched => return Ok(fetched),
            }
        }
        if here[held.parts_for(at, count)].iter().all(|here| *here) {
            // The first part is for other nodes: what came of it does not
            // change what filling reads.
            if !here.is_empty() {
                let _ = fetch_part(inner, net, &held, 0, &mut here, from).await?;
            }
            return Ok(Fetched::Here);
        }
        look = true;
    }
}

/// Fetches the `n`th part of `held` unless it is `here`, first from
/// `from` when given.
async fn fetch_part(
    inner: &Arc<Inner>,
    net: &NetHandle,
    held: &HeldSites,
    n: usize,
    here: &mut [bool],
    from: Option<PeerId>,
) -> Result<Fetched> {
    if here[n] {
        return Ok(Fetched::Here);
    }
    if inner.download_pause().is_some() {
        return Ok(Fetched::Paused);
    }
    let part = held.manifest.files[n].clone();
    let partial = held.dir.join(format!("{}.partial", part.path));
    if let Err(err) = signed_sets::fetch_part(inner, net, &part, &partial, from).await {
        debug!("{SITES}: part {}: {err:#}", part.path);
        // Not resumed: filling reads a trusted node's list from here on.
        blocking(inner, move |inner| {
            if fs::metadata(&partial).is_ok() {
                storage::remove_file(&partial, inner.storage.as_deref())?;
            }
            Ok(())
        })
        .await?;
        return Ok(Fetched::Missing);
    }
    if let Err(err) = inner.add_downloaded(part.bytes) {
        warn!("cannot count the download: {err:#}");
    }
    let generation = held.generation;
    let path = part.path.clone();
    blocking(inner, move |inner| {
        put_in_place(inner, generation, &part, &partial)
    })
    .await?;
    debug!("{SITES}: took part {path}");
    here[n] = true;
    Ok(Fetched::Here)
}

/// Removes the parts of the set of sites that hold the `count` sites
/// from the `at`th on and do not match the manifest, so that they are
/// fetched again. Whether any was.
pub(super) fn remove_damaged(inner: &Inner, at: u64, count: u64) -> Result<bool> {
    let _held = inner.sites.held.locked();
    let Some(held) = HeldSites::find(&inner.paths.data, &inner.config.set_publishers) else {
        return Ok(false);
    };
    let mut removed = false;
    for n in held.parts_for(at, count) {
        let part = &held.manifest.files[n];
        let path = held.dir.join(&part.path);
        if held.here[n] && !measure(&path).is_ok_and(|(_, sha256)| sha256 == part.sha256) {
            warn!("{SITES}: part {} is damaged; fetching it again", part.path);
            storage::remove_file(&path, inner.storage.as_deref())?;
            removed = true;
        }
    }
    Ok(removed)
}

/// Runs `work` off the async threads.
async fn blocking<T: Send + 'static>(
    inner: &Arc<Inner>,
    work: impl FnOnce(&Inner) -> Result<T> + Send + 'static,
) -> Result<T> {
    let inner = inner.clone();
    tokio::task::spawn_blocking(move || work(&inner))
        .await
        .context("the set of sites")?
}

/// Puts the manifest `text`, a newer generation of the set of sites, in
/// place of the one held: keeps the parts the two share, and removes the
/// rest.
fn install(inner: &Inner, text: &str) -> Result<HeldSites> {
    let dir = set_dir(&inner.paths.data, SITES).context("not a set name")?;
    let _held = inner.sites.held.locked();
    take(&dir, text, inner.storage.as_ref())?;
    HeldSites::find(&inner.paths.data, &inner.config.set_publishers)
        .context("the manifest just put in place is not one the node takes")
}

/// Puts the signed manifest `text` in place in `dir`, keeping the parts
/// held that it names with the same bytes and removing everything else.
fn take(dir: &Path, text: &str, budget: Option<&Arc<storage::StorageBudget>>) -> Result<()> {
    let (manifest, _) = Manifest::from_signed_json(text, &now())?;
    storage::create_directory(dir, budget)?;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let shared = name == MANIFEST
            || manifest.files.iter().any(|part| {
                part.path == name
                    && is_here(dir, part)
                    && measure(&path).is_ok_and(|(_, sha256)| sha256 == part.sha256)
            });
        if shared {
            continue;
        }
        if path.is_dir() {
            storage::remove_directory(&path, budget.map(Arc::as_ref))?;
        } else {
            storage::remove_file(&path, budget.map(Arc::as_ref))?;
        }
    }
    storage::write_atomic(
        &dir.join(MANIFEST),
        &dir.join("manifest.json.part"),
        text.as_bytes(),
        budget,
        None,
    )?;
    info!("took {SITES} generation {}", manifest.generation);
    Ok(())
}

/// Removes the parts of the set of sites before its `at`th site, which
/// filling is past, but the first, which new nodes ask for.
fn remove_before(inner: &Inner, at: u64) -> Result<()> {
    let _held = inner.sites.held.locked();
    match HeldSites::find(&inner.paths.data, &inner.config.set_publishers) {
        Some(held) => remove_parts_before(&held, at, inner.storage.as_deref()),
        None => Ok(()),
    }
}

/// [`remove_before`] in `held`.
fn remove_parts_before(
    held: &HeldSites,
    at: u64,
    budget: Option<&storage::StorageBudget>,
) -> Result<()> {
    let mut end = 0u64;
    for (n, part) in held.manifest.files.iter().enumerate() {
        end = end.saturating_add(part.count);
        if n > 0 && end <= at && held.here[n] {
            storage::remove_file(&held.dir.join(&part.path), budget)?;
        }
    }
    Ok(())
}

/// Puts the part fetched into `partial` in place, when the set held is
/// still the `generation` it is a part of.
fn put_in_place(inner: &Inner, generation: i64, part: &FilePart, partial: &Path) -> Result<()> {
    let budget = inner.storage.as_deref();
    let _held = inner.sites.held.locked();
    match HeldSites::find(&inner.paths.data, &inner.config.set_publishers) {
        Some(held) if held.generation == generation => {
            storage::replace_file(partial, &held.dir.join(&part.path), budget)?;
        }
        _ => storage::remove_file(partial, budget)?,
    }
    Ok(())
}

/// For tests: a signed set of sites in `data`, a.org to e.org in parts
/// of two, two and one.
#[cfg(test)]
pub(super) fn test_set(data: &Path, key: &ed25519_dalek::SigningKey) {
    tests::sites(data, key);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use serde_json::{json, Value};

    /// A signed set of sites, five sites in parts of two, two and one.
    pub(super) fn sites(data: &Path, key: &SigningKey) {
        let dir = set_dir(data, SITES).unwrap();
        write_sites(&dir, key, "2026-10-01T04:00:00Z", ["a", "b", "c", "d", "e"]);
    }

    /// Writes and signs a set of the sites `names` in `dir`, in parts of
    /// two, two and one.
    fn write_sites(dir: &Path, key: &SigningKey, generation: &str, names: [&str; 5]) {
        let Value::Object(manifest) = json!({"format": plumb_format::FORMAT, "set": SITES,
            "generation": generation, "license": "CC0-1.0"})
        else {
            unreachable!()
        };
        let items = names.map(|name| {
            let mut site = plumb_core::SiteRecord::new(format!("{name}.org"));
            site.signals.tranco_rank = Some(1);
            Some(site.to_item().unwrap())
        });
        let fields = plumb_core::site_item::site_fields();
        plumb_tools::set::write_set(items.into_iter(), fields, manifest, dir, &[2, 4], 3).unwrap();
        plumb_tools::set::sign_set(dir, key, None, None).unwrap();
    }

    fn ids(held: &HeldSites, from: u64) -> Vec<String> {
        held.lines_from(from)
            .map(|line| serde_json::from_str::<Value>(&line.unwrap()).unwrap()["id"].to_string())
            .collect()
    }

    #[test]
    fn reads_from_any_site_through_the_parts_here() {
        let data = tempfile::tempdir().unwrap();
        let key = SigningKey::from_bytes(&[3; 32]);
        let publishers = [PublicKey::of(&key)];
        sites(data.path(), &key);
        let held = HeldSites::find(data.path(), &publishers).unwrap();
        assert_eq!(held.total(), 5);
        assert_eq!(
            ids(&held, 0),
            [
                r#""a.org""#,
                r#""b.org""#,
                r#""c.org""#,
                r#""d.org""#,
                r#""e.org""#
            ]
        );
        assert_eq!(ids(&held, 3), [r#""d.org""#, r#""e.org""#]);
        assert!(ids(&held, 5).is_empty());
        assert_eq!(held.parts_for(0, 2), 0..1);
        assert_eq!(held.parts_for(1, 2), 0..2);
        assert_eq!(held.parts_for(3, 50_000), 1..3);
        assert_eq!(held.parts_for(5, 10), 0..0);
        let other = SigningKey::from_bytes(&[4; 32]);
        assert!(HeldSites::find(data.path(), &[PublicKey::of(&other)]).is_none());

        // Without the middle part, reading stops before it, and from it
        // reads nothing.
        let dir = set_dir(data.path(), SITES).unwrap();
        fs::remove_file(dir.join(&held.manifest.files[1].path)).unwrap();
        let held = HeldSites::find(data.path(), &publishers).unwrap();
        assert_eq!(held.here, [true, false, true]);
        assert_eq!(ids(&held, 1), [r#""b.org""#]);
        assert!(ids(&held, 2).is_empty());
        assert_eq!(ids(&held, 4), [r#""e.org""#]);
    }

    #[test]
    fn a_part_that_cannot_be_read_is_an_error_not_an_end() {
        let data = tempfile::tempdir().unwrap();
        let key = SigningKey::from_bytes(&[3; 32]);
        sites(data.path(), &key);
        let held = HeldSites::find(data.path(), &[PublicKey::of(&key)]).unwrap();
        // The same size, but not the part.
        let first = held.dir.join(&held.manifest.files[0].path);
        let bytes = fs::metadata(&first).unwrap().len();
        fs::write(&first, vec![0u8; bytes as usize]).unwrap();
        let held = HeldSites::find(data.path(), &[PublicKey::of(&key)]).unwrap();
        assert!(held.lines_from(1).any(|line| line.is_err()));
    }

    #[test]
    fn a_newer_generation_keeps_the_parts_it_shares() {
        let data = tempfile::tempdir().unwrap();
        let key = SigningKey::from_bytes(&[3; 32]);
        let publishers = [PublicKey::of(&key)];
        sites(data.path(), &key);
        let dir = set_dir(data.path(), SITES).unwrap();
        let older = HeldSites::find(data.path(), &publishers).unwrap();
        // A fetch cut short goes too.
        fs::write(dir.join("items-002.jsonl.zst.partial"), b"cut short").unwrap();
        let newer = tempfile::tempdir().unwrap();
        write_sites(
            newer.path(),
            &key,
            "2026-10-08T04:00:00Z",
            ["a", "b", "c", "x", "y"],
        );
        let text = fs::read_to_string(newer.path().join(MANIFEST)).unwrap();
        take(&dir, &text, None).unwrap();

        let held = HeldSites::find(data.path(), &publishers).unwrap();
        assert!(held.generation > older.generation);
        assert_eq!(held.here, [true, false, false]);
        assert_eq!(ids(&held, 0), [r#""a.org""#, r#""b.org""#]);
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["items-001.jsonl.zst", MANIFEST]);
    }

    #[test]
    fn the_parts_filling_is_past_go_but_the_first() {
        let data = tempfile::tempdir().unwrap();
        let key = SigningKey::from_bytes(&[3; 32]);
        let publishers = [PublicKey::of(&key)];
        sites(data.path(), &key);
        let held = HeldSites::find(data.path(), &publishers).unwrap();
        remove_parts_before(&held, 3, None).unwrap();
        let held = HeldSites::find(data.path(), &publishers).unwrap();
        assert_eq!(held.here, [true, true, true]);
        remove_parts_before(&held, 4, None).unwrap();
        let held = HeldSites::find(data.path(), &publishers).unwrap();
        assert_eq!(held.here, [true, false, true]);
        remove_parts_before(&held, 5, None).unwrap();
        let held = HeldSites::find(data.path(), &publishers).unwrap();
        assert_eq!(held.here, [true, false, false]);
    }
}
