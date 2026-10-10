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
use plumb_net::pages::{layers_path, quality_path, read_layers, Layers, PagesChunk, SetQuality};
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

/// Biggest size of a newer file, as a share of this node's whole file,
/// unless the set is named in `--set-updates`: a set that grew a lot (a
/// Wikipedia file of 6.8 million pages in place of 2 million) can take
/// more memory than the machine has, so it is loaded only when asked.
pub(super) const MAX_GROWTH_PERCENT: u64 = 125;

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
    pub quality: Option<SetQuality>,
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
    /// A newer file may be any size bigger (the set is named in
    /// `--set-updates`), not only [`MAX_GROWTH_PERCENT`] of this one.
    pub may_grow: bool,
    pub quality: Option<SetQuality>,
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
                quality: chunk.quality,
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
        if mine.complete
            && !mine.may_grow
            && offer.size.saturating_mul(100) > mine.size.saturating_mul(MAX_GROWTH_PERCENT)
        {
            why = "a trusted node's newer file is much bigger (name the set in --set-updates to take it)";
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
        if let Some(quality) = &mine.quality {
            let holds_quality = offer.quality.as_ref().is_some_and(|theirs| {
                quality.stages.iter().filter(|s| s.complete).all(|stage| {
                    theirs
                        .stages
                        .iter()
                        .any(|s| s.name == stage.name && s.complete)
                })
            });
            if !holds_quality {
                why = "a trusted node's newer file lacks completed quality stages this one has";
                continue;
            }
        }
        if best.is_none_or(|b| offer.modified > b.modified) {
            best = Some(offer);
        }
    }
    best.ok_or(why)
}

/// Sets whose files carry lines of entries besides their pages, which a
/// newer file must also carry.
pub(super) const LAYERED_SETS: [&str; 7] = [
    "wikipedia-en",
    plumb_index::pages::WIKIDATA_SET,
    plumb_index::pages::DOCS_SET,
    plumb_index::pages::REFERENCE_SET,
    plumb_index::pages::SUBPAGES_SET,
    plumb_index::pages::OLD_SUBPAGES_SET,
    plumb_index::pages::PAPERS_SET,
];

/// The kinds of entries on the lines of profiles of an articles file (see
/// `plumb_core::article`): `website`, `lead`, `name`, each fact (`f-capital`)
/// and `profiles` for any service, sorted.
pub(crate) fn layers_of(path: &Path) -> Result<Vec<String>> {
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
                "website" | "lead" | "name" | "section" | "symbol" | "passage" | "language"
                | "task-type" | "source-date" => key,
                fact if fact.starts_with("f-") => fact,
                service if plumb_core::profiles::service_by_key(service).is_some() => "profiles",
                // Unknown extensions are not social profiles.
                _ => continue,
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
pub(crate) fn stamp(path: &Path) -> Option<(u64, u64)> {
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
pub(crate) fn prev_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".prev");
    PathBuf::from(name)
}

/// Puts the downloaded `part` in place of `file`, keeping the file it
/// replaces as `<file>.prev`, and gives it its maker's time `modified`.
pub(crate) fn install(part: &Path, file: &Path, modified: u64) -> Result<()> {
    // Finish potentially failing metadata work before touching the current file.
    set_time(part, modified)?;
    if file.is_file() {
        let parent = file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let backup = tempfile::tempdir_in(parent)?;
        let previous = backup.path().join("previous");
        // Same-filesystem hard links keep large maps/sets reversible without
        // another full copy. Fall back when the filesystem cannot link files.
        if std::fs::hard_link(file, &previous).is_err() {
            std::fs::copy(file, &previous)?;
        }
        std::fs::File::open(&previous)?.sync_all()?;
        std::fs::rename(&previous, prev_path(file))
            .with_context(|| format!("keeping {} as .prev", file.display()))?;
    }
    std::fs::rename(part, file)
        .with_context(|| format!("renaming {} to {}", part.display(), file.display()))?;
    let _ = std::fs::remove_file(layers_path(file));
    let _ = std::fs::remove_file(quality_path(file));
    Ok(())
}

/// Verify the offered generation after a whole transfer, before installation.
pub(super) fn file_checksum(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let len = file.read(&mut bytes)?;
        if len == 0 {
            break;
        }
        hash.update(&bytes[..len]);
    }
    Ok(format!("{:x}", hash.finalize()))
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
            quality: None,
        }
    }

    fn mine(layers: &[&str]) -> Mine {
        Mine {
            modified: 1_000,
            size: 1_000,
            complete: true,
            layers: layers.iter().map(|s| s.to_string()).collect(),
            may_grow: false,
            quality: None,
        }
    }

    #[test]
    fn completed_quality_stages_survive_optional_peer_metadata() {
        use plumb_net::pages::QualityStage;
        let quality = SetQuality {
            generation: "g1".into(),
            sha256: "0".repeat(64),
            fetched_at: 1000,
            records: 10,
            hosts: 1,
            failed_hosts: 0,
            capped: false,
            stages: vec![QualityStage {
                name: "paper-repair".into(),
                complete: true,
            }],
        };
        let mine = Mine {
            quality: Some(quality.clone()),
            ..mine(&[])
        };
        assert!(newest(&mine, &[offer(1000, 2000, None)], 10_000).is_err());
        let mut degraded = quality.clone();
        degraded.stages[0].complete = false;
        assert!(newest(
            &mine,
            &[Offer {
                quality: Some(degraded),
                ..offer(1000, 2000, None)
            }],
            10_000
        )
        .is_err());
        assert!(newest(
            &mine,
            &[Offer {
                quality: Some(quality),
                ..offer(1000, 2000, None)
            }],
            10_000
        )
        .is_ok());
    }

    #[test]
    fn rich_docs_are_layers_and_unknown_extensions_are_not_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("docs.tsv.gz");
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&file).unwrap(),
            flate2::Compression::fast(),
        );
        writeln!(gz, "profiles\thttps://example.org/api\tsection=API|symbol=padStart|passage=Usage|future-data=unknown").unwrap();
        gz.finish().unwrap();
        assert_eq!(layers("docs", &file), ["passage", "section", "symbol"]);
    }

    #[test]
    fn failed_install_keeps_the_current_advertised_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("docs.tsv.gz");
        std::fs::write(&file, b"old").unwrap();
        assert!(install(&dir.path().join("missing.part"), &file, 123).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"old");
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
        // Much bigger: only when the set was named.
        let big = [offer(3_400, 5_000, None)];
        assert!(newest(&mine(&[]), &big, now)
            .unwrap_err()
            .contains("much bigger"));
        let named = Mine {
            may_grow: true,
            ..mine(&[])
        };
        assert!(newest(&named, &big, now).is_ok());
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
