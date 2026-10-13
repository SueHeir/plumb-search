//! Reading sets in the data format ([`plumb_format`]): checking one as a
//! node does before taking it, and reading its items.
//!
//! A set is a folder: `manifest.json` and its parts, `items-001.jsonl.zst`
//! and on, one item per line, most weighted first. `plumb-tools set`
//! writes and signs them; a node only checks and reads them.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use anyhow::{ensure, Context, Result};
use plumb_format::deny::Entry;
use plumb_format::notes::is_revoked;
use plumb_format::sites::Rule;
use plumb_format::{Item, Manifest, Note, PublicKey};
use sha2::{Digest, Sha256};

/// The manifest's file name in a set's folder.
pub const MANIFEST: &str = "manifest.json";

/// What [`check_set`] found.
#[derive(Debug)]
pub struct Checked {
    pub set: String,
    pub generation: String,
    pub publisher: PublicKey,
    pub parts: usize,
    pub items: u64,
    /// A denylist's entries, or a site rules set's rules, newer than this
    /// version understands.
    pub newer: u64,
}

/// Checks the set in `dir`: its manifest's signature and rules, then each
/// part's size, hash and count, and every item. With `trusted`, the set
/// must be published by that key or the key it handed over to in `notes`.
/// A signer revoked in `notes` is refused either way.
pub fn check_set(dir: &Path, trusted: Option<&PublicKey>, notes: &[Note]) -> Result<Checked> {
    check_first_parts(dir, trusted, notes, usize::MAX)
}

/// [`check_set`] for a node that holds only the first `parts` parts of a
/// set, as it may of a page set: the manifest is checked whole, and the
/// parts it holds.
pub fn check_first_parts(
    dir: &Path,
    trusted: Option<&PublicKey>,
    notes: &[Note],
    parts: usize,
) -> Result<Checked> {
    let text = fs::read_to_string(dir.join(MANIFEST))
        .with_context(|| format!("reading {}", dir.join(MANIFEST).display()))?;
    let (manifest, signer) = Manifest::from_signed_json(&text, &now())?;
    match trusted {
        Some(trusted) => manifest.trusted(&signer, trusted, notes)?,
        None => ensure!(
            !is_revoked(&signer, &manifest.publisher, notes),
            "{signer}, which signed {}, is revoked",
            manifest.set
        ),
    }
    // A denylist is read whatever it requires: skipping it would fail open.
    let missing = manifest.not_understood();
    ensure!(
        missing.is_empty() || manifest.is_denylist(),
        "{} requires {missing:?}, which this version does not read",
        manifest.set
    );
    check_parts(dir, &manifest, parts)
}

/// The manifest and the items of the set in `dir`, part by part, as they
/// are, leaving out blob parts. Check the set first ([`check_set`]): this
/// reads what is there.
pub fn read_items(dir: &Path) -> Result<(Manifest, Vec<Item>)> {
    let text = fs::read_to_string(dir.join(MANIFEST))
        .with_context(|| format!("reading {}", dir.join(MANIFEST).display()))?;
    let (manifest, _) = Manifest::from_signed_json(&text, &now())?;
    let mut items = Vec::new();
    for part in manifest.files.iter().filter(|part| !part.blob) {
        let path = dir.join(&part.path);
        let reader = BufReader::new(zstd::Decoder::new(File::open(&path)?)?);
        for line in reader.lines() {
            let line = line.with_context(|| format!("reading {}", path.display()))?;
            items.push(serde_json::from_str::<Item>(&line)?);
        }
    }
    Ok((manifest, items))
}

/// 64 bits of an id's SHA-256, for telling ids apart.
pub fn id_hash(id: &str) -> u64 {
    let digest = Sha256::digest(id.as_bytes());
    u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"))
}

/// Checks the first `parts` parts of the set `manifest` describes, in
/// `dir`: each part's size, hash and count, and every item, whether or
/// not the manifest is signed yet. A blob part is checked by its size and
/// hash alone.
pub fn check_parts(dir: &Path, manifest: &Manifest, parts: usize) -> Result<Checked> {
    // Ids by hash: a page set's millions of ids would take hundreds of
    // megabytes as text.
    let mut ids = HashSet::new();
    let (mut items, mut newer) = (0, 0);
    for part in manifest.files.iter().take(parts) {
        let path = dir.join(&part.path);
        let (bytes, sha256) = measure(&path)?;
        ensure!(
            bytes == part.bytes && sha256 == part.sha256,
            "{} is not the file the manifest names (size or hash differs)",
            part.path
        );
        if part.blob {
            // Bytes an item names by hash, not items: its hash is its check.
            continue;
        }
        let reader = BufReader::new(zstd::Decoder::new(File::open(&path)?)?);
        let mut count = 0;
        for (n, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("reading {}", part.path))?;
            let at = || format!("{} line {}", part.path, n + 1);
            let item: Item = serde_json::from_str(&line).with_context(at)?;
            manifest.check_item(&item).with_context(at)?;
            ensure!(
                item.may_share() || !manifest.may_share(),
                "{}: a personal item in a set that may be shared",
                at()
            );
            if manifest.is_denylist() && !item.is_deleted() && Entry::from_item(&item)?.is_none() {
                newer += 1;
            }
            if manifest.is_site_rules() && !item.is_deleted() && Rule::from_item(&item)?.is_none() {
                newer += 1;
            }
            ensure!(ids.insert(id_hash(&item.id)), "{}: an id seen before", at());
            count += 1;
        }
        ensure!(
            count == part.count,
            "{} holds {count} items, not {}",
            part.path,
            part.count
        );
        items += count;
    }
    Ok(Checked {
        set: manifest.set.clone(),
        generation: manifest.generation.clone(),
        publisher: manifest.publisher,
        parts: manifest.files.len().min(parts),
        items,
        newer,
    })
}

/// A file's size and hex SHA-256.
pub fn measure(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    let mut bytes = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        bytes += n as u64;
    }
    let hex = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((bytes, hex))
}

/// The time now as a UTC timestamp, as the format writes them.
pub fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
