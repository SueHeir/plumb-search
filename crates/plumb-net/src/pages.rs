//! Handing page set files (Wikipedia articles, see `plumb_core::article`)
//! from node to node over `/plumb/pages/1`, so a node needs no Wikimedia
//! dumps to list articles: it asks a node it trusts for the set's file, a
//! piece at a time from the start. The file lists the most read pages
//! first, so a node keeping only the top 100,000 stops after the first few
//! megabytes.
//!
//! Like filling ([`crate::fill`]), this is taken only from trusted nodes:
//! the file is the answering node's, with no signature on each page. A
//! node answers at most [`MAX_SERVING`] requests at once and
//! [`PAGES_REQUESTS_PER_MINUTE`] from one node.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use libp2p::PeerId;
use serde_bytes::ByteBuf;

use crate::proto::{PagesRequest, PagesResponse};

/// Most bytes sent for one request.
pub const MAX_PAGES_CHUNK: u32 = 1 << 20;

/// Requests a node answers at once; more are told it is busy.
pub const MAX_SERVING: usize = 4;

/// Requests a node answers from one node a minute.
pub const PAGES_REQUESTS_PER_MINUTE: u32 = 120;

/// A piece of a trusted node's page set file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PagesChunk {
    /// The node that answered.
    pub peer: PeerId,
    /// The whole file's size; 0 when the node has no such set.
    pub size: u64,
    /// When the file was made (Unix seconds).
    pub modified: u64,
    /// The bytes from the offset asked for.
    pub bytes: Vec<u8>,
    /// It was busy and sent nothing; ask again later.
    pub busy: bool,
    /// What the file holds besides its pages, when the node knows.
    pub layers: Option<Vec<String>>,
    /// Optional ingestion quality, independent of receiving all file bytes.
    pub quality: Option<SetQuality>,
}

/// Bounded metadata shared with legacy-compatible peers and public status.
/// Host errors and cache paths belong only in the local generation manifest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SetQuality {
    pub generation: String,
    pub sha256: String,
    pub fetched_at: u64,
    pub records: u64,
    pub hosts: u64,
    pub failed_hosts: u64,
    pub capped: bool,
    pub stages: Vec<QualityStage>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QualityStage {
    pub name: String,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QualityNote {
    pub modified: u64,
    pub size: u64,
    pub quality: SetQuality,
}

pub fn quality_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".quality");
    name.into()
}

/// Read only a small stamp-bound note, never scan the corpus per request.
pub fn read_quality(path: &Path, modified: u64, size: u64) -> Option<SetQuality> {
    let file = File::open(quality_path(path)).ok()?;
    if file.metadata().ok()?.len() > 16 * 1024 {
        return None;
    }
    let note: QualityNote = serde_json::from_reader(file).ok()?;
    let quality = note.quality;
    (note.modified == modified
        && note.size == size
        && quality.stages.len() <= 32
        && quality.stages.iter().all(|s| s.name.len() <= 64)
        && quality.generation.len() <= 128
        && quality
            .generation
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        && quality.sha256.len() == 64
        && quality.sha256.bytes().all(|c| c.is_ascii_hexdigit()))
    .then_some(quality)
}

/// Use an immutable local snapshot when a publisher installed one. Peers
/// and legacy files without a local snapshot keep using their advertised file.
pub fn generation_file(path: &Path) -> Option<std::path::PathBuf> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let quality = read_quality(path, modified, meta.len())?;
    let mut directory = path.as_os_str().to_owned();
    directory.push(".generations");
    let file = std::path::PathBuf::from(directory)
        .join(quality.generation)
        .join("pages.tsv.gz");
    let snapshot = std::fs::metadata(&file).ok()?;
    (snapshot.is_file() && snapshot.len() == meta.len()).then_some(file)
}

/// What a node notes next to a page set file, as `<file>.layers`: the
/// kinds of entries the file holds besides its pages (an articles file's
/// `lead`, `name`, `f-capital` and profile services), worked out once for
/// the file's time. A node replaces its file with a newer one only when
/// the newer one holds all the kinds its own does, so a plain articles
/// file can't take the place of one with facts and leads added.
pub const LAYERS_SUFFIX: &str = ".layers";

/// The layers noted next to `path` (see [`LAYERS_SUFFIX`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Layers {
    /// The file's time (Unix seconds) and size they were worked out for.
    pub modified: u64,
    pub size: u64,
    /// The kinds of entries, sorted.
    pub kinds: Vec<String>,
}

/// `<file>.layers`.
pub fn layers_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(LAYERS_SUFFIX);
    std::path::PathBuf::from(name)
}

/// The layers noted next to `path`, if they are for the file as it is.
pub fn read_layers(path: &Path, modified: u64, size: u64) -> Option<Vec<String>> {
    let bytes = std::fs::read(layers_path(path)).ok()?;
    let layers: Layers = serde_json::from_slice(&bytes).ok()?;
    (layers.modified == modified && layers.size == size).then_some(layers.kinds)
}

/// The answer to `request` from the set file at `path` (`None` when this
/// node has no such set).
pub fn answer(path: Option<&Path>, request: &PagesRequest) -> PagesResponse {
    let empty = PagesResponse {
        size: 0,
        modified: 0,
        bytes: ByteBuf::new(),
        busy: false,
        layers: None,
        quality: None,
    };
    let Some(path) = path else {
        return empty;
    };
    let read = || -> std::io::Result<PagesResponse> {
        let mut file = File::open(path)?;
        let meta = file.metadata()?;
        let modified = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let size = meta.len();
        let len = u64::from(request.len.min(MAX_PAGES_CHUNK));
        let mut bytes = Vec::new();
        if request.offset < size {
            file.seek(SeekFrom::Start(request.offset))?;
            file.take(len).read_to_end(&mut bytes)?;
        }
        Ok(PagesResponse {
            size,
            modified,
            bytes: ByteBuf::from(bytes),
            busy: false,
            layers: read_layers(path, modified, size),
            quality: read_quality(path, modified, size),
        })
    };
    read().unwrap_or(empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_responses_and_optional_quality_are_compatible_and_stamp_bound() {
        let legacy = br#"{"size":10,"modified":100,"bytes":[],"layers":null}"#;
        let response: PagesResponse = serde_json::from_slice(legacy).unwrap();
        assert_eq!(response.quality, None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("docs.tsv.gz");
        let quality = SetQuality {
            generation: "generation".into(),
            sha256: "0".repeat(64),
            fetched_at: 90,
            records: 4,
            hosts: 1,
            failed_hosts: 1,
            capped: false,
            stages: vec![QualityStage {
                name: "source-refresh".into(),
                complete: false,
            }],
        };
        let note = QualityNote {
            modified: 100,
            size: 10,
            quality: quality.clone(),
        };
        std::fs::write(quality_path(&path), serde_json::to_vec(&note).unwrap()).unwrap();
        assert_eq!(read_quality(&path, 100, 10), Some(quality.clone()));
        assert_eq!(read_quality(&path, 101, 10), None);
        assert_eq!(read_quality(&path, 100, 11), None);
        let response = PagesResponse {
            quality: Some(quality),
            ..response
        };
        // CBOR maps allow older readers to ignore the new optional field.
        let bytes = cbor4ii::serde::to_vec(Vec::new(), &response).unwrap();
        let back: PagesResponse = cbor4ii::serde::from_slice(&bytes).unwrap();
        assert_eq!(back, response);
        #[derive(serde::Deserialize)]
        struct OldResponse {
            size: u64,
            modified: u64,
        }
        let old: OldResponse = cbor4ii::serde::from_slice(&bytes).unwrap();
        assert_eq!((old.size, old.modified), (10, 100));
        std::fs::write(quality_path(&path), vec![b'x'; 20 * 1024]).unwrap();
        assert_eq!(read_quality(&path, 100, 10), None);
    }

    #[test]
    fn answers_pieces_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wikipedia-en.tsv.gz");
        std::fs::write(&path, b"0123456789").unwrap();
        let ask = |offset, len| PagesRequest {
            set: "wikipedia-en".into(),
            offset,
            len,
        };
        let piece = answer(Some(&path), &ask(3, 4));
        assert_eq!((piece.size, &piece.bytes[..]), (10, &b"3456"[..]));
        assert!(piece.modified > 0);
        assert_eq!(&answer(Some(&path), &ask(8, 100)).bytes[..], b"89");
        assert!(answer(Some(&path), &ask(10, 4)).bytes.is_empty());
        assert_eq!(answer(None, &ask(0, 4)).size, 0);
        assert_eq!(answer(Some(&dir.path().join("none")), &ask(0, 4)).size, 0);

        // Layers are sent only when noted for the file as it is.
        assert_eq!(piece.layers, None);
        let note = |modified| Layers {
            modified,
            size: 10,
            kinds: vec!["lead".into()],
        };
        let write = |layers: &Layers| {
            std::fs::write(layers_path(&path), serde_json::to_vec(layers).unwrap()).unwrap()
        };
        write(&note(piece.modified));
        assert_eq!(
            answer(Some(&path), &ask(0, 4)).layers,
            Some(vec!["lead".into()])
        );
        write(&note(piece.modified - 1));
        assert_eq!(answer(Some(&path), &ask(0, 4)).layers, None);
    }
}
