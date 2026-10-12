//! Signed sets (see `plumb-format`) passed from node to node over
//! `/plumb/sets/1`: a node asks the nodes it meets for the newest signed
//! manifest of a set it takes, then fetches each part the manifest names
//! by its SHA-256, a piece at a time, from any node that holds it.
//!
//! Unlike `/plumb/pages/1` ([`crate::pages`]), nothing here needs trust in
//! the node that answers: the asker checks the manifest's signature
//! against the publishers it trusts, and every part against the hash the
//! manifest gives. So a set can come from any node, not only trusted ones.
//! A node answers at most [`MAX_SETS_SERVING`] requests at once and
//! [`SETS_REQUESTS_PER_MINUTE`] from one node.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use libp2p::PeerId;
use serde_bytes::ByteBuf;

use crate::bucket::BucketSource;
use crate::proto::{SetsRequest, SetsResponse};

/// Most bytes of a part sent for one request.
pub const MAX_SET_CHUNK: u32 = 1 << 20;

/// Longest manifest sent. A manifest lists its parts and field types, so
/// a few kilobytes is usual.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// Requests a node answers at once; more are told it is busy.
pub const MAX_SETS_SERVING: usize = 4;

/// Requests a node answers from one node a minute.
pub const SETS_REQUESTS_PER_MINUTE: u32 = 120;

/// An answer to a [`SetsRequest`], with the node that gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetsAnswer {
    pub peer: PeerId,
    pub response: SetsResponse,
}

/// The answer `source` gives to `request`.
pub fn answer<S: BucketSource + ?Sized>(source: &S, request: &SetsRequest) -> SetsResponse {
    match request {
        SetsRequest::Manifest { set } => SetsResponse::Manifest(
            source
                .set_manifest(set)
                .filter(|text| text.len() <= MAX_MANIFEST_BYTES),
        ),
        SetsRequest::Part {
            sha256,
            offset,
            len,
        } => {
            let file = is_part_hash(sha256)
                .then(|| source.set_part_file(sha256))
                .flatten();
            answer_part(file.as_deref(), *offset, *len)
        }
    }
}

/// `len` bytes (at most [`MAX_SET_CHUNK`]) from `offset` of the part at
/// `path`, with its size; size 0 when there is no such part.
pub fn answer_part(path: Option<&Path>, offset: u64, len: u32) -> SetsResponse {
    let read = |path: &Path| -> std::io::Result<SetsResponse> {
        let mut file = File::open(path)?;
        let size = file.metadata()?.len();
        let mut bytes = Vec::new();
        if offset < size {
            file.seek(SeekFrom::Start(offset))?;
            file.take(u64::from(len.min(MAX_SET_CHUNK)))
                .read_to_end(&mut bytes)?;
        }
        Ok(SetsResponse::Part {
            size,
            bytes: ByteBuf::from(bytes),
        })
    };
    path.and_then(|path| read(path).ok())
        .unwrap_or(SetsResponse::Part {
            size: 0,
            bytes: ByteBuf::new(),
        })
}

/// A part's name as asked for: 64 lowercase hex digits.
pub fn is_part_hash(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The file in `dir` named by `sha256`, when there is one: a store that
/// keeps parts by their hash.
pub fn part_in(dir: &Path, sha256: &str) -> Option<PathBuf> {
    let path = dir.join(sha256);
    (is_part_hash(sha256) && path.is_file()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Held(PathBuf);

    impl BucketSource for Held {
        fn bucket(&self, _bucket: u32) -> Option<Vec<String>> {
            None
        }

        fn set_manifest(&self, set: &str) -> Option<String> {
            (set == "plumb/site-rules").then(|| "{\"set\":\"plumb/site-rules\"}".to_string())
        }

        fn set_part_file(&self, sha256: &str) -> Option<PathBuf> {
            part_in(&self.0, sha256)
        }
    }

    #[test]
    fn answers_manifests_and_pieces_of_parts() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "ab".repeat(32);
        std::fs::write(dir.path().join(&hash), b"0123456789").unwrap();
        let held = Held(dir.path().to_path_buf());
        let manifest = |set: &str| answer(&held, &SetsRequest::Manifest { set: set.into() });
        assert!(matches!(
            manifest("plumb/site-rules"),
            SetsResponse::Manifest(Some(_))
        ));
        assert_eq!(manifest("plumb/other"), SetsResponse::Manifest(None));
        let part = |sha256: &str, offset, len| {
            answer(
                &held,
                &SetsRequest::Part {
                    sha256: sha256.into(),
                    offset,
                    len,
                },
            )
        };
        let piece = |size, bytes: &[u8]| SetsResponse::Part {
            size,
            bytes: ByteBuf::from(bytes.to_vec()),
        };
        assert_eq!(part(&hash, 3, 4), piece(10, b"3456"));
        assert_eq!(part(&hash, 8, 100), piece(10, b"89"));
        assert_eq!(part(&hash, 10, 4), piece(10, b""));
        assert_eq!(part(&"cd".repeat(32), 0, 4), piece(0, b""));
        // Only hashes name files, never paths.
        assert_eq!(part("../etc/passwd", 0, 4), piece(0, b""));
        assert_eq!(part(&hash.to_uppercase(), 0, 4), piece(0, b""));
    }

    #[test]
    fn requests_and_answers_travel_as_cbor() {
        for request in [
            SetsRequest::Manifest {
                set: "plumb/denylist".into(),
            },
            SetsRequest::Part {
                sha256: "0".repeat(64),
                offset: 5,
                len: MAX_SET_CHUNK,
            },
        ] {
            let bytes = cbor4ii::serde::to_vec(Vec::new(), &request).unwrap();
            let back: SetsRequest = cbor4ii::serde::from_slice(&bytes).unwrap();
            assert_eq!(back, request);
        }
        for response in [
            SetsResponse::Manifest(None),
            SetsResponse::Busy,
            SetsResponse::Part {
                size: 3,
                bytes: ByteBuf::from(b"abc".to_vec()),
            },
        ] {
            let bytes = cbor4ii::serde::to_vec(Vec::new(), &response).unwrap();
            let back: SetsResponse = cbor4ii::serde::from_slice(&bytes).unwrap();
            assert_eq!(back, response);
        }
    }
}
