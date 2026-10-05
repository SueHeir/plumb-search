//! A PIR snapshot of a node's buckets, cut down to [`lean_record`]s and
//! packed by [`PieceMap`] into one row of [`LEAN_ROW_BYTES`] per row group.
//!
//! On a node with 1.36M sites (2026-10-05) the lean pieces came to 425 MB
//! compressed and packed into rows of at most 28.5 KB, so the table is
//! 16,384 rows of 32 KiB: 512 MiB, the size Spiral's upstream v1 profile
//! was measured at (`tools/pir-probe`, about half a second and 4 to 5 GB of
//! memory per row on that node). Whole buckets of full records needed 4 GiB.
//!
//! The records carry no crawl proofs: a proof covers the full record, which
//! the table does not hold. A client trusts the node that signs the
//! manifest, as it trusts any node answering it.
//!
//! ```text
//! row payload: for each non-empty piece of the row, in piece order:
//!   u32 LE   piece number
//!   u32 LE   length of what follows
//!   bytes    the piece's lean records as a JSON array, raw deflate
//! ```

use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use plumb_core::keys::{bucket_of, lean_record, piece_of, record_keys, PIECES, PIECES_PER_BUCKET};
use plumb_core::SiteRecord;

use super::pieces::PieceMap;
use super::snapshot::{Layout, Snapshot};
use crate::bucket::{BucketTable, BUCKETS};

/// Size of every row.
pub const LEAN_ROW_BYTES: u32 = 32 * 1024;

/// The PIR parameters such a table is answered with: Spiral's upstream v1
/// profile, 16,384 rows of 32 KiB.
pub const LEAN_PROFILE: &str = "spiral-v1-16k";

/// The most a piece may inflate to; more is a damaged or hostile row.
const MAX_PIECE_INFLATED: u64 = 8 << 20;

const FRAME_BYTES: usize = 8;

/// What a build found, for logs and status. Holds no per-row detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeanReport {
    pub pieces: u32,
    pub largest_row: usize,
    pub total_bytes: u64,
}

/// Builds the lean snapshot of `table` in `dir`, which must not exist yet.
/// Fails, writing nothing, when a row would not fit in one
/// [`LEAN_ROW_BYTES`] row: nothing is cut off.
pub fn build(
    dir: &Path,
    table: &BucketTable,
    created: u64,
    valid_for: u64,
) -> Result<(Snapshot, LeanReport)> {
    build_rows(dir, table, LEAN_ROW_BYTES, LEAN_PROFILE, created, valid_for)
}

fn build_rows(
    dir: &Path,
    table: &BucketTable,
    row_bytes: u32,
    profile: &str,
    created: u64,
    valid_for: u64,
) -> Result<(Snapshot, LeanReport)> {
    ensure!(!dir.exists(), "{} already exists", dir.display());
    let spill_path = dir.with_extension("pieces.tmp");
    let built = (|| {
        let (spans, sizes) = spill_pieces(table, &spill_path)?;
        let (map, totals) = PieceMap::pack(&sizes)?;
        let largest = totals.iter().copied().max().unwrap_or(0) as usize;
        let capacity = Layout {
            row_bytes,
            pages_per_bucket: 1,
        }
        .chunk_bytes();
        if largest > capacity {
            bail!(
                "the fullest row needs {largest} bytes, over the {capacity} one \
                 {row_bytes}-byte row holds; nothing is cut off"
            );
        }
        let by_row = map.pieces_by_row();
        let mut spill = File::open(&spill_path)?;
        let (snapshot, _) =
            Snapshot::build(dir, row_bytes, &map, profile, created, valid_for, |row| {
                let mut payload = Vec::new();
                for &piece in &by_row[row as usize] {
                    let (at, len) = spans[piece as usize];
                    if len == 0 {
                        continue;
                    }
                    payload.extend_from_slice(&piece.to_le_bytes());
                    payload.extend_from_slice(&(len as u32).to_le_bytes());
                    let start = payload.len();
                    payload.resize(start + len, 0);
                    spill.seek(SeekFrom::Start(at))?;
                    spill.read_exact(&mut payload[start..])?;
                }
                Ok(payload)
            })?;
        ensure!(
            snapshot.manifest().layout.pages_per_bucket == 1,
            "the lean table must take one row per row group"
        );
        Ok((
            snapshot,
            LeanReport {
                pieces: sizes.iter().filter(|&&s| s > 0).count() as u32,
                largest_row: largest,
                total_bytes: totals.iter().sum(),
            },
        ))
    })();
    let _ = fs::remove_file(&spill_path);
    built
}

/// Where a piece sits in the spill file: offset and length.
type Span = (u64, usize);

/// Writes every non-empty piece, compressed, to `path`; returns where each
/// piece is (offset, length) and the bytes each takes in a row, framing
/// included (0 for an empty piece).
fn spill_pieces(table: &BucketTable, path: &Path) -> Result<(Vec<Span>, Vec<u64>)> {
    let mut spill =
        BufWriter::new(File::create(path).with_context(|| format!("creating {}", path.display()))?);
    let mut spans = vec![(0u64, 0usize); PIECES as usize];
    let mut sizes = vec![0u64; PIECES as usize];
    let mut at = 0u64;
    for bucket in 0..BUCKETS {
        let records: Vec<SiteRecord> = table
            .get(bucket)?
            .iter()
            .map(|json| serde_json::from_str(json))
            .collect::<Result<_, _>>()
            .context("the bucket table holds a record that does not parse")?;
        let mut by_piece: Vec<Vec<SiteRecord>> = vec![Vec::new(); PIECES_PER_BUCKET as usize];
        for record in records {
            let mut wanted = [false; PIECES_PER_BUCKET as usize];
            for key in record_keys(&record) {
                if bucket_of(&key) == bucket {
                    wanted[(piece_of(&key) % PIECES_PER_BUCKET) as usize] = true;
                }
            }
            let lean = lean_record(record);
            for (sub, _) in wanted.iter().enumerate().filter(|(_, w)| **w) {
                by_piece[sub].push(lean.clone());
            }
        }
        for (sub, records) in by_piece.iter().enumerate() {
            if records.is_empty() {
                continue;
            }
            let piece = bucket * PIECES_PER_BUCKET + sub as u32;
            let bytes = deflate(&serde_json::to_vec(records)?)?;
            spill.write_all(&bytes)?;
            spans[piece as usize] = (at, bytes.len());
            sizes[piece as usize] = (FRAME_BYTES + bytes.len()) as u64;
            at += bytes.len() as u64;
        }
    }
    spill.into_inner().context("writing pieces")?.sync_all()?;
    Ok((spans, sizes))
}

fn deflate(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(9));
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

/// The records of `piece` in a row's payload (from
/// [`super::snapshot::assemble_bucket`]); none when the row has no such
/// piece. Fails for a damaged payload.
pub fn read_piece(payload: &[u8], piece: u32) -> Result<Vec<SiteRecord>> {
    ensure!(piece < PIECES, "there is no piece {piece}");
    let mut rest = payload;
    while !rest.is_empty() {
        ensure!(rest.len() >= FRAME_BYTES, "a row's piece is cut short");
        let id = u32::from_le_bytes(rest[..4].try_into().expect("4 bytes"));
        let len = u32::from_le_bytes(rest[4..8].try_into().expect("4 bytes")) as usize;
        ensure!(
            rest.len() - FRAME_BYTES >= len,
            "a row's piece is cut short"
        );
        let body = &rest[FRAME_BYTES..FRAME_BYTES + len];
        rest = &rest[FRAME_BYTES + len..];
        if id != piece {
            continue;
        }
        let mut json = Vec::new();
        DeflateDecoder::new(body)
            .take(MAX_PIECE_INFLATED + 1)
            .read_to_end(&mut json)
            .context("a row's piece does not inflate")?;
        ensure!(
            json.len() as u64 <= MAX_PIECE_INFLATED,
            "a row's piece inflates too far"
        );
        return serde_json::from_slice(&json).context("reading a row's piece");
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use plumb_core::keys::{matches, query_keys};
    use plumb_core::{LinkText, Signals};

    use super::*;
    use crate::pir::snapshot::assemble_bucket;

    const NOW: u64 = 1_800_000_000;
    const DAY: u64 = 24 * 60 * 60;
    /// Small rows, so tests don't write the full 512 MiB table.
    const TEST_ROW_BYTES: u32 = 2048;

    fn site(domain: &str, title: &str, text_words: usize) -> SiteRecord {
        SiteRecord {
            domain: domain.into(),
            title: Some(title.into()),
            description: Some("d".repeat(500)),
            body_text: Some("word ".repeat(text_words)),
            headings: vec!["Welcome".into()],
            intro: Some("An intro.".into()),
            link_texts: vec![LinkText::from_linkers(title.to_lowercase(), 7)],
            signals: Signals {
                linking_domains: 3,
                ..Signals::default()
            },
            ..SiteRecord::default()
        }
    }

    fn table(dir: &Path) -> BucketTable {
        let mut records = vec![
            site("usbank.com", "U.S. Bank", 50),
            site("chase.com", "Chase Bank", 80),
            site("example.org", "Example Domain", 10),
        ];
        records
            .extend((0..300).map(|n| site(&format!("site{n}.example"), &format!("Shop {n}"), 20)));
        BucketTable::build(&dir.join("buckets"), &records).unwrap()
    }

    /// What a client does: the keys' pieces, their rows, each row checked
    /// against the manifest, then the piece read out.
    fn search(snapshot: &Snapshot, query: &str) -> Vec<SiteRecord> {
        let map = snapshot.pieces().unwrap();
        let keys = query_keys(query);
        let mut found: Vec<SiteRecord> = Vec::new();
        for key in &keys {
            let piece = piece_of(key);
            let row = map.row_of(piece);
            let payload = assemble_bucket(
                snapshot.manifest(),
                row,
                &[snapshot.row(u64::from(row)).unwrap()],
            )
            .unwrap();
            for record in read_piece(&payload, piece).unwrap() {
                if matches(&record, &keys) && !found.iter().any(|f| f.domain == record.domain) {
                    found.push(record);
                }
            }
        }
        found
    }

    #[test]
    fn every_site_is_found_by_its_names_with_lean_records() {
        let dir = tempfile::tempdir().unwrap();
        let table = table(dir.path());
        let (snapshot, report) = build_rows(
            &dir.path().join("pir"),
            &table,
            TEST_ROW_BYTES,
            "test",
            NOW,
            DAY,
        )
        .unwrap();
        let manifest = snapshot.manifest();
        assert_eq!(manifest.layout.row_bytes, TEST_ROW_BYTES);
        assert_eq!(manifest.layout.pages_per_bucket, 1);
        assert!(report.pieces > 0 && report.largest_row > 0);
        assert!(!dir.path().join("pir.pieces.tmp").exists());

        let found = search(&snapshot, "us bank");
        let usbank = found
            .iter()
            .find(|r| r.domain == "usbank.com")
            .expect("found");
        assert_eq!(usbank.body_text, None);
        assert_eq!(usbank.intro, None);
        assert!(usbank.headings.is_empty());
        assert_eq!(usbank.description.as_ref().unwrap().len(), 200);
        assert!(search(&snapshot, "chase")
            .iter()
            .any(|r| r.domain == "chase.com"));
        for n in [0, 150, 299] {
            let domain = format!("site{n}.example");
            assert!(
                search(&snapshot, &format!("shop {n}"))
                    .iter()
                    .any(|r| r.domain == domain),
                "{domain}"
            );
        }
        let reopened = Snapshot::open(&dir.path().join("pir")).unwrap();
        assert_eq!(reopened.pieces().unwrap(), snapshot.pieces().unwrap());
    }

    #[test]
    fn a_damaged_piece_map_or_payload_fails() {
        let dir = tempfile::tempdir().unwrap();
        let table = table(dir.path());
        let path = dir.path().join("pir");
        build_rows(&path, &table, TEST_ROW_BYTES, "test", NOW, DAY).unwrap();
        let mut bytes = fs::read(path.join("pieces.bin")).unwrap();
        bytes[0] ^= 1;
        fs::write(path.join("pieces.bin"), &bytes).unwrap();
        assert!(
            Snapshot::open(&path).is_err(),
            "a piece moved to another row"
        );

        assert!(read_piece(&[1, 0, 0, 0, 9, 0], 1).is_err());
        assert!(read_piece(&[1, 0, 0, 0, 9, 0, 0, 0, 1], 1).is_err());
        let mut junk = 1u32.to_le_bytes().to_vec();
        junk.extend_from_slice(&3u32.to_le_bytes());
        junk.extend_from_slice(&[1, 2, 3]);
        assert!(read_piece(&junk, 1).is_err());
        // Another piece's frame is skipped, not inflated.
        assert!(read_piece(&junk, 2).unwrap().is_empty());
        assert!(read_piece(&[], 5).unwrap().is_empty());
        assert!(read_piece(&[], PIECES).is_err());
    }

    #[test]
    fn rows_too_small_for_the_fullest_fail_without_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let mut records: Vec<SiteRecord> = (0..40)
            .map(|n| site(&format!("bank{n}.example"), "Bank", 10))
            .collect();
        // Text that does not compress, in the one piece of the key `bank`.
        for record in &mut records {
            let noise: String = (0u8..4)
                .map(|i| crate::hash::Hash::of(&[record.domain.as_bytes(), &[i]]).to_hex())
                .collect();
            record.description = Some(noise);
        }
        let table = BucketTable::build(&dir.path().join("buckets"), &records).unwrap();
        let path = dir.path().join("pir");
        let err = build_rows(&path, &table, 1024, "test", NOW, DAY).unwrap_err();
        assert!(err.to_string().contains("nothing is cut off"), "{err}");
        assert!(!path.exists());
        assert!(!dir.path().join("pir.pieces.tmp").exists());
    }

    #[test]
    fn a_bomb_does_not_inflate_past_the_limit() {
        let bomb = deflate(&vec![b' '; (MAX_PIECE_INFLATED + 10) as usize]).unwrap();
        let mut payload = 4u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&(bomb.len() as u32).to_le_bytes());
        payload.extend_from_slice(&bomb);
        let err = read_piece(&payload, 4).unwrap_err();
        assert!(err.to_string().contains("too far"), "{err}");
    }
}
