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
}

/// The answer to `request` from the set file at `path` (`None` when this
/// node has no such set).
pub fn answer(path: Option<&Path>, request: &PagesRequest) -> PagesResponse {
    let empty = PagesResponse {
        size: 0,
        modified: 0,
        bytes: ByteBuf::new(),
        busy: false,
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
    }
}
