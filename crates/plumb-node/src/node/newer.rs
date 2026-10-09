//! Taking newer data set files from trusted nodes (Liz, 2026-10-09:
//! "shouldn't the network handle copying?").
//!
//! A node asks its trusted nodes every [`CHECK_EVERY`] how old their file
//! of each set it keeps is (a request for no bytes, see
//! `plumb_net::pages`), and takes the newest one that is newer than its
//! own, its own made file included, when that file:
//!
//! * was made at least [`SETTLED_AFTER`] ago, so one being made in steps
//!   (`fetch-pages`, then `fetch-profiles`, `fetch-facts`, `fetch-leads`)
//!   is not taken half-way;
//! * is at least [`MIN_SIZE_PERCENT`] of the size of this node's whole
//!   file, so a cut or broken file does not replace a whole one;
//! * for an articles file, holds every kind of entry this node's does
//!   ([`layers_of`]): a plain Wikipedia file never takes the place of one
//!   with facts and leads added, however new.
//!
//! The file it replaces is kept as `<file>.prev`, one step to roll back.
//! A taken file gets its maker's time, so it is handed on with that time
//! and two nodes never take the same file back and forth.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use plumb_net::pages::{layers_path, read_layers, Layers, PagesChunk};
use plumb_net::{NetHandle, PeerId};
use tracing::warn;

use super::Inner;

/// How often a node asks its trusted nodes for newer files of its sets.
#[cfg(not(test))]
pub(super) const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
#[cfg(test)]
pub(super) const CHECK_EVERY: Duration = Duration::from_millis(500);

/// A newer file is taken only once its maker made it this long ago.
#[cfg(not(test))]
pub(super) const SETTLED_AFTER: u64 = 2 * 3600;
#[cfg(test)]
pub(super) const SETTLED_AFTER: u64 = 0;

/// Smallest size of a newer file, as a share of this node's whole file.
pub(super) const MIN_SIZE_PERCENT: u64 = 90;

/// Trusted nodes asked about one set at most, per check.
const MAX_ASKED: usize = 16;

/// Busy answers a check waits out before it goes on without that node.
const BUSY_TRIES: u32 = 4;

/// What a trusted node said of its file of a set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Offer {
    pub peer: PeerId,
    pub size: u64,
    pub modified: u64,
    pub layers: Option<Vec<String>>,
}

/// This node's file of a set, as [`newest`] weighs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Mine {
    /// When its maker made it.
    pub modified: u64,
    pub size: u64,
    /// The maker's whole file, not a cut of it.
    pub complete: bool,
    /// The kinds of entries it holds ([`layers_of`]); empty for none.
    pub layers: Vec<String>,
}

/// The trusted nodes' files of `set`, from asking each connected one that
/// serves sets; `None` when no such node is connected.
pub(super) fn offers(inner: &Inner, net: &NetHandle, set: &str) -> Result<Option<Vec<Offer>>> {
    let runtime = tokio::runtime::Handle::current();
    let mut asked = Vec::new();
    let mut offers = Vec::new();
    let mut busy = 0;
    while asked.len() < MAX_ASKED {
        let answer: Option<PagesChunk> =
            runtime.block_on(net.pages_chunk(set, 0, 0, None, &asked))?;
        let Some(chunk) = answer else {
            break;
        };
        if chunk.busy {
            busy += 1;
            if busy > BUSY_TRIES {
                asked.push(chunk.peer);
            } else if !super::pages::wait(inner, super::pages::BUSY_WAIT) {
                return Ok(Some(offers));
            }
            continue;
        }
        asked.push(chunk.peer);
        if chunk.size > 0 {
            offers.push(Offer {
                peer: chunk.peer,
                size: chunk.size,
                modified: chunk.modified,
                layers: chunk.layers,
            });
        }
    }
    Ok((!asked.is_empty()).then_some(offers))
}

/// The newest of `offers` worth taking in place of `mine` at `now`, or why
/// none is.
pub(super) fn newest<'a>(
    mine: &Mine,
    offers: &'a [Offer],
    now: u64,
) -> std::result::Result<&'a Offer, &'static str> {
    let mut why = "no trusted node has a newer file";
    let mut best: Option<&Offer> = None;
    for offer in offers.iter().filter(|o| o.modified > mine.modified) {
        if now < offer.modified.saturating_add(SETTLED_AFTER) {
            why = "a trusted node's newer file is too new to take yet";
            continue;
        }
        if mine.complete
            && offer.size.saturating_mul(100) < mine.size.saturating_mul(MIN_SIZE_PERCENT)
        {
            why = "a trusted node's newer file is much smaller";
            continue;
        }
        let holds_mine = mine.layers.is_empty()
            || offer
                .layers
                .as_ref()
                .is_some_and(|theirs| mine.layers.iter().all(|kind| theirs.contains(kind)));
        if !holds_mine {
            why = "a trusted node's newer file lacks entries this one has";
            continue;
        }
        if best.is_none_or(|b| offer.modified > b.modified) {
            best = Some(offer);
        }
    }
    best.ok_or(why)
}

/// Sets whose files carry lines of entries besides their pages, which a
/// newer file must also carry.
pub(super) const LAYERED_SETS: [&str; 2] = ["wikipedia-en", plumb_index::pages::WIKIDATA_SET];

/// The kinds of entries on the lines of profiles of an articles file (see
/// `plumb_core::article`): `website`, `lead`, `name`, each fact (`f-capital`)
/// and `profiles` for any service, sorted.
pub(super) fn layers_of(path: &Path) -> Result<Vec<String>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(flate2::read::MultiGzDecoder::new(file));
    let mut kinds = std::collections::BTreeSet::new();
    for line in reader.lines() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        let Some(rest) = line.strip_prefix(plumb_core::article::PROFILES_LINE) else {
            continue;
        };
        let Some((_, entries)) = rest.split_once('\t') else {
            continue;
        };
        for pair in entries.split('|') {
            let Some((key, _)) = pair.split_once('=') else {
                continue;
            };
            let kind = match key {
                "website" | "lead" | "name" => key,
                fact if fact.starts_with("f-") => fact,
                _ => "profiles",
            };
            if !kinds.contains(kind) {
                kinds.insert(kind.to_string());
            }
        }
    }
    Ok(kinds.into_iter().collect())
}

/// The layers of the file at `path`, from its `.layers` note when that is
/// for the file as it is, else worked out and noted. Empty for a set that
/// has none, or when the file can't be read.
pub(super) fn layers(set: &str, path: &Path) -> Vec<String> {
    if !LAYERED_SETS.contains(&set) {
        return Vec::new();
    }
    let Some((modified, size)) = stamp(path) else {
        return Vec::new();
    };
    if let Some(kinds) = read_layers(path, modified, size) {
        return kinds;
    }
    match layers_of(path) {
        Ok(kinds) => {
            let note = Layers {
                modified,
                size,
                kinds: kinds.clone(),
            };
            let written = serde_json::to_vec(&note)
                .map_err(anyhow::Error::from)
                .and_then(|bytes| super::store::write_atomically(&layers_path(path), &bytes));
            if let Err(err) = written {
                warn!("could not note what {} holds: {err:#}", path.display());
            }
            kinds
        }
        Err(err) => {
            warn!("could not read what {} holds: {err:#}", path.display());
            Vec::new()
        }
    }
}

/// A file's time (Unix seconds) and size.
pub(super) fn stamp(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Some((modified, meta.len()))
}

/// Gives the file at `path` the time `modified` (Unix seconds).
pub(super) fn set_time(path: &Path, modified: u64) -> Result<()> {
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(UNIX_EPOCH + Duration::from_secs(modified)))
        .with_context(|| format!("dating {}", path.display()))
}

/// `<file>.prev`.
pub(super) fn prev_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".prev");
    PathBuf::from(name)
}

/// Puts the downloaded `part` in place of `file`, keeping the file it
/// replaces as `<file>.prev`, and gives it its maker's time `modified`.
pub(super) fn install(part: &Path, file: &Path, modified: u64) -> Result<()> {
    if file.is_file() {
        std::fs::rename(file, prev_path(file))
            .with_context(|| format!("keeping {} as .prev", file.display()))?;
    }
    let _ = std::fs::remove_file(layers_path(file));
    std::fs::rename(part, file)
        .with_context(|| format!("renaming {} to {}", part.display(), file.display()))?;
    set_time(file, modified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn offer(size: u64, modified: u64, layers: Option<&[&str]>) -> Offer {
        Offer {
            peer: PeerId::random(),
            size,
            modified,
            layers: layers.map(|l| l.iter().map(|s| s.to_string()).collect()),
        }
    }

    fn mine(layers: &[&str]) -> Mine {
        Mine {
            modified: 1_000,
            size: 1_000,
            complete: true,
            layers: layers.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn takes_the_newest_file_that_holds_what_this_one_does() {
        let full = ["f-capital", "lead", "name"];
        let offers = [
            offer(1_000, 2_000, Some(&full)),
            offer(
                1_100,
                3_000,
                Some(&["f-capital", "lead", "name", "website"]),
            ),
            offer(1_000, 900, Some(&full)),
        ];
        let now = 10_000;
        assert_eq!(newest(&mine(&full), &offers, now).unwrap().modified, 3_000);
        // A plain file, newer but without facts and leads, is never taken.
        let plain = [offer(1_000, 5_000, Some(&[])), offer(1_000, 6_000, None)];
        assert_eq!(
            newest(&mine(&full), &plain, now),
            Err("a trusted node's newer file lacks entries this one has")
        );
        // A file of a set with no entries besides pages needs no layers.
        assert_eq!(newest(&mine(&[]), &plain, now).unwrap().modified, 6_000);
        // Much smaller, or older: not taken.
        assert!(newest(&mine(&[]), &[offer(800, 5_000, None)], now).is_err());
        assert!(newest(&mine(&[]), &[offer(5_000, 1_000, None)], now).is_err());
        // A cut file of this node's is no measure of size.
        let cut = Mine {
            complete: false,
            ..mine(&[])
        };
        assert!(newest(&cut, &[offer(10, 5_000, None)], now).is_ok());
    }

    #[test]
    fn works_out_and_notes_what_an_articles_file_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wikipedia-en.tsv.gz");
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        writeln!(gz, "900\tMarie Curie\tphysicist\tQ7186\t\t").unwrap();
        writeln!(
            gz,
            "profiles\tQ7186\tx=curie|f-born=1867-11-07|lead=Marie Curie was ...|name=Maria"
        )
        .unwrap();
        writeln!(
            gz,
            "profiles\tQ1\tyoutube-handle=a|website=https://a.example/"
        )
        .unwrap();
        gz.finish().unwrap();
        let kinds = ["f-born", "lead", "name", "profiles", "website"];
        assert_eq!(layers("wikipedia-en", &path), kinds);
        assert!(layers_path(&path).is_file(), "noted for next time");
        let (modified, size) = stamp(&path).unwrap();
        assert_eq!(read_layers(&path, modified, size).unwrap(), kinds);
        assert!(layers("github", &path).is_empty());
    }

    #[test]
    fn a_new_file_keeps_the_one_it_replaces_and_its_maker_s_time() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("films.tsv.gz");
        let part = dir.path().join("films.tsv.gz.part");
        std::fs::write(&file, b"old").unwrap();
        std::fs::write(layers_path(&file), b"{}").unwrap();
        std::fs::write(&part, b"new").unwrap();
        install(&part, &file, 1_700_000_000).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
        assert_eq!(std::fs::read(prev_path(&file)).unwrap(), b"old");
        assert!(!layers_path(&file).exists());
        assert_eq!(stamp(&file).unwrap().0, 1_700_000_000);
    }
}
