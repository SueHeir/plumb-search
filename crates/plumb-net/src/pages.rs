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
    /// A digest of what the file holds, when the node knows (see
    /// [`Layers::content`]).
    pub content: Option<String>,
}

/// What a node notes next to a page set file, as `<file>.layers`: the
/// kinds of entries the file holds besides its pages (an articles file's
/// `lead`, `name`, `f-capital` and profile services), worked out once for
/// the file's time. A node replaces its file with a newer one only when
/// the newer one holds all the kinds its own does, so a plain articles
/// file can't take the place of one with facts and leads added. The note
/// also has a digest of what the file holds, so a node doesn't take a
/// copy of its own file that only has a later time.
pub const LAYERS_SUFFIX: &str = ".layers";

/// The layers noted next to `path` (see [`LAYERS_SUFFIX`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Layers {
    /// The file's time (Unix seconds) and size they were worked out for.
    pub modified: u64,
    pub size: u64,
    /// The kinds of entries, sorted.
    pub kinds: Vec<String>,
    /// SHA-256 (hex) of what the file holds, read through gzip, so the
    /// same pages compressed again give the same digest; `None` in notes
    /// written before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// `<file>.layers`.
pub fn layers_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(LAYERS_SUFFIX);
    std::path::PathBuf::from(name)
}

/// The note next to `path`, if it is for the file as it is.
pub fn read_note(path: &Path, modified: u64, size: u64) -> Option<Layers> {
    let bytes = std::fs::read(layers_path(path)).ok()?;
    let layers: Layers = serde_json::from_slice(&bytes).ok()?;
    (layers.modified == modified && layers.size == size).then_some(layers)
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
        content: None,
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
        let note = read_note(path, modified, size);
        Ok(PagesResponse {
            size,
            modified,
            bytes: ByteBuf::from(bytes),
            busy: false,
            layers: note.as_ref().map(|n| n.kinds.clone()),
            content: note.and_then(|n| n.content),
        })
    };
    read().unwrap_or(empty)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!((&piece.layers, &piece.content), (&None, &None));
        let note = |modified| Layers {
            modified,
            size: 10,
            kinds: vec!["lead".into()],
            content: Some("ab12".into()),
        };
        let write = |layers: &Layers| {
            std::fs::write(layers_path(&path), serde_json::to_vec(layers).unwrap()).unwrap()
        };
        write(&note(piece.modified));
        let noted = answer(Some(&path), &ask(0, 4));
        assert_eq!(noted.layers, Some(vec!["lead".into()]));
        assert_eq!(noted.content.as_deref(), Some("ab12"));
        write(&note(piece.modified - 1));
        assert_eq!(answer(Some(&path), &ask(0, 4)).layers, None);
        // A note from before digests still reads.
        std::fs::write(
            layers_path(&path),
            format!(r#"{{"modified":{},"size":10,"kinds":[]}}"#, piece.modified),
        )
        .unwrap();
        let old = answer(Some(&path), &ask(0, 4));
        assert_eq!((old.layers, old.content), (Some(vec![]), None));
    }
}
