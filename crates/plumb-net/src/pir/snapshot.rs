//! An immutable, authenticated table of fixed-size rows for private
//! retrieval: every bucket of a [`crate::BucketTable`], with the crawl
//! proofs it had when the snapshot was made, cut into the same number of
//! equal pages.
//!
//! A PIR server answers "row `i`" without learning `i`, so the client must
//! be able to check the row by itself, and every bucket must look the same
//! from outside: same number of rows, same row size, whatever it holds.
//! So:
//!
//! - every bucket's payload (its [`BucketRecord`]s as JSON, proofs frozen
//!   in) is split over exactly [`Layout::pages_per_bucket`] rows, chosen
//!   once for the whole snapshot from the largest bucket. A bucket that
//!   does not fit even at [`MAX_PAGES_PER_BUCKET`] fails the build: nothing
//!   is cut off silently.
//! - each row ends with its own path in a Merkle tree over all rows, of a
//!   fixed depth, so a retrieved row is checked against the root in the
//!   [`Manifest`] with nothing else fetched (no row-specific proof URL).
//! - "bucket" below is a row group of the table: one bucket, or the
//!   pieces of buckets a [`PieceMap`] packs together. The map comes with
//!   the snapshot (`pieces.bin`).
//! - the manifest commits to the format, the bucket mapping, the piece
//!   map, the layout, the root and the validity window; its hash is the snapshot's identity,
//!   and the serving node signs it.
//!
//! ```text
//! row (row_bytes):
//!   u32 LE   payload length of the whole bucket (the same on all its pages)
//!   u32 LE   page number within the bucket
//!   chunk    this page's share of the payload, zero padded
//!   path     depth x 32 bytes: Merkle siblings, bottom up
//! leaf = SHA-256(LEAF_TAG || row index u64 LE || row without its path)
//! ```
//!
//! Rows sit in `rows.bin` in index order (bucket `b`, page `p` is row
//! `b * pages_per_bucket + p`), next to `manifest.json` and `pieces.bin`.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use libp2p::identity::{Keypair, PublicKey};
use libp2p::PeerId;
use serde::{Deserialize, Serialize};

use crate::bucket::BUCKETS;
use crate::hash::Hash;
use crate::proto::BucketRecord;

use super::pieces::PieceMap;

/// The row format and leaf/manifest hashing described above.
pub const FORMAT_VERSION: u32 = 2;

/// Which keys fall in which bucket: [`plumb_core::keys`] as of this
/// format. A change to the key rules must change this, so old snapshots
/// cannot be read with new rules.
pub const KEY_MAPPING_VERSION: u32 = 1;

/// The most pages a bucket may take. More would make every retrieval that
/// many times dearer; a table that needs more fails to build instead.
pub const MAX_PAGES_PER_BUCKET: u32 = 64;

/// The smallest and largest row sizes accepted.
pub const MIN_ROW_BYTES: u32 = 1024;
pub const MAX_ROW_BYTES: u32 = 1 << 20;

const LEAF_TAG: &[u8] = b"plumb-pir-row/1";
const NODE_TAG: &[u8] = b"plumb-pir-node/1";
const EMPTY_TAG: &[u8] = b"plumb-pir-empty/1";
const SIGN_TAG: &[u8] = b"plumb-pir-manifest/1";
const HEADER_BYTES: usize = 8;

const ROWS_FILE: &str = "rows.bin";
const MANIFEST_FILE: &str = "manifest.json";
const PIECES_FILE: &str = "pieces.bin";

/// How buckets become rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    pub row_bytes: u32,
    pub pages_per_bucket: u32,
}

impl Layout {
    /// Fails for a layout no snapshot may use.
    pub fn check(&self) -> Result<()> {
        ensure!(
            (MIN_ROW_BYTES..=MAX_ROW_BYTES).contains(&self.row_bytes),
            "row size {} is outside {MIN_ROW_BYTES}..={MAX_ROW_BYTES}",
            self.row_bytes
        );
        ensure!(
            (1..=MAX_PAGES_PER_BUCKET).contains(&self.pages_per_bucket),
            "pages per bucket {} is outside 1..={MAX_PAGES_PER_BUCKET}",
            self.pages_per_bucket
        );
        ensure!(self.chunk_bytes() > 0, "rows are too small for their path");
        Ok(())
    }

    /// Rows in the table.
    pub fn rows(&self) -> u64 {
        u64::from(BUCKETS) * u64::from(self.pages_per_bucket)
    }

    /// Levels of the Merkle tree: rows rounded up to a power of two.
    pub fn depth(&self) -> u32 {
        self.rows().next_power_of_two().trailing_zeros()
    }

    fn path_bytes(&self) -> usize {
        self.depth() as usize * 32
    }

    /// Payload bytes one row carries.
    pub fn chunk_bytes(&self) -> usize {
        (self.row_bytes as usize).saturating_sub(HEADER_BYTES + self.path_bytes())
    }

    /// The largest bucket payload this layout holds.
    pub fn bucket_capacity(&self) -> usize {
        self.chunk_bytes() * self.pages_per_bucket as usize
    }

    /// The smallest page count for rows of `row_bytes` that holds a bucket
    /// payload of `largest` bytes, if any does.
    pub fn fitting(row_bytes: u32, largest: usize) -> Option<Layout> {
        (1..=MAX_PAGES_PER_BUCKET)
            .map(|pages_per_bucket| Layout {
                row_bytes,
                pages_per_bucket,
            })
            .find(|layout| layout.check().is_ok() && layout.bucket_capacity() >= largest)
    }

    /// The rows bucket `bucket` takes.
    pub fn rows_of(&self, bucket: u32) -> std::ops::Range<u64> {
        let first = u64::from(bucket) * u64::from(self.pages_per_bucket);
        first..first + u64::from(self.pages_per_bucket)
    }
}

/// What a snapshot commits to. Its hash ([`Manifest::id`]) names the
/// snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub key_mapping: u32,
    /// [`PieceMap::hash`] of the map of pieces to row groups.
    pub pieces: Hash,
    pub buckets: u32,
    pub layout: Layout,
    /// The PIR scheme and parameters the server answers with, such as
    /// `spiral-v1-16k`; part of the identity so a client never decodes
    /// answers with the wrong parameters.
    pub profile: String,
    /// Merkle root over all rows.
    pub root: Hash,
    /// Unix seconds: when it was made, and after when clients stop using it.
    pub created: u64,
    pub valid_until: u64,
}

impl Manifest {
    /// The snapshot's identity: SHA-256 of the manifest's canonical JSON.
    pub fn id(&self) -> Hash {
        Hash::of(&[&self.canonical()])
    }

    fn canonical(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a manifest always encodes")
    }

    /// Fails unless this client can use the snapshot at time `now`.
    pub fn check(&self, now: u64) -> Result<()> {
        ensure!(
            self.format == FORMAT_VERSION,
            "unknown snapshot format {}",
            self.format
        );
        ensure!(
            self.key_mapping == KEY_MAPPING_VERSION,
            "the snapshot buckets keys another way ({})",
            self.key_mapping
        );
        ensure!(
            self.buckets == BUCKETS,
            "the snapshot has {} buckets",
            self.buckets
        );
        self.layout.check()?;
        ensure!(
            self.created <= self.valid_until,
            "the snapshot expires before it was made"
        );
        ensure!(now <= self.valid_until, "the snapshot has expired");
        Ok(())
    }

    /// Signs the manifest with the serving node's key.
    pub fn sign(&self, key: &Keypair) -> Result<SignedManifest> {
        let manifest = self.canonical();
        let signature = key
            .sign(&signing_bytes(&manifest))
            .context("signing the snapshot manifest")?;
        Ok(SignedManifest {
            manifest,
            public_key: key.public().encode_protobuf(),
            signature,
        })
    }
}

fn signing_bytes(manifest: &[u8]) -> Vec<u8> {
    [SIGN_TAG, manifest].concat()
}

/// A manifest as the serving node signed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedManifest {
    /// The manifest's canonical JSON, as signed.
    pub manifest: Vec<u8>,
    pub public_key: Vec<u8>,
    pub signature: Vec<u8>,
}

impl SignedManifest {
    /// The manifest, if `signer` signed it and it can be used at `now`.
    pub fn verify(&self, signer: &PeerId, now: u64) -> Result<Manifest> {
        let key = PublicKey::try_decode_protobuf(&self.public_key)
            .context("the manifest's key is damaged")?;
        ensure!(
            key.to_peer_id() == *signer,
            "the manifest is signed by another node"
        );
        ensure!(
            key.verify(&signing_bytes(&self.manifest), &self.signature),
            "the manifest's signature is wrong"
        );
        let manifest: Manifest =
            serde_json::from_slice(&self.manifest).context("reading the manifest")?;
        ensure!(
            manifest.canonical() == self.manifest,
            "the manifest is not in canonical form"
        );
        manifest.check(now)?;
        Ok(manifest)
    }
}

/// A bucket's payload: its records with their proofs, as JSON.
pub fn encode_bucket(records: &[BucketRecord]) -> Result<Vec<u8>> {
    serde_json::to_vec(records).context("encoding a bucket")
}

/// The records of a payload [`encode_bucket`] made.
pub fn decode_bucket(payload: &[u8]) -> Result<Vec<BucketRecord>> {
    serde_json::from_slice(payload).context("reading a bucket")
}

fn leaf(index: u64, body: &[u8]) -> Hash {
    Hash::of(&[LEAF_TAG, &index.to_le_bytes(), body])
}

fn node(left: &Hash, right: &Hash) -> Hash {
    Hash::of(&[NODE_TAG, &left.0, &right.0])
}

fn empty_leaf() -> Hash {
    Hash::of(&[EMPTY_TAG])
}

/// A built snapshot on disk.
#[derive(Debug)]
pub struct Snapshot {
    dir: PathBuf,
    manifest: Manifest,
}

/// What a build found, for logs and status. Holds no per-bucket detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildReport {
    pub largest_payload: usize,
    pub total_payload: u64,
}

impl Snapshot {
    /// Builds a snapshot in `dir`, which must not exist yet, from
    /// `payload(b)` for every row group `b` of `pieces` (see
    /// [`encode_bucket`]), with rows
    /// of `row_bytes` and as few pages per bucket as the largest needs.
    /// Fails, writing nothing, when a bucket needs more than
    /// [`MAX_PAGES_PER_BUCKET`] pages.
    pub fn build(
        dir: &Path,
        row_bytes: u32,
        pieces: &PieceMap,
        profile: &str,
        created: u64,
        valid_for: u64,
        mut payload: impl FnMut(u32) -> Result<Vec<u8>>,
    ) -> Result<(Snapshot, BuildReport)> {
        ensure!(!dir.exists(), "{} already exists", dir.display());
        let staging = dir.with_extension("staging");
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
        let built = (|| {
            // Payloads go to disk once; the layout depends on the largest.
            let spill_path = staging.join("payloads.tmp");
            let mut spill = BufWriter::new(create(&spill_path)?);
            let mut lengths = Vec::with_capacity(BUCKETS as usize);
            for bucket in 0..BUCKETS {
                let bytes = payload(bucket)?;
                ensure!(
                    u32::try_from(bytes.len()).is_ok(),
                    "a bucket payload is over 4 GiB"
                );
                spill.write_all(&bytes)?;
                lengths.push(bytes.len());
            }
            flush(spill)?;
            let largest = lengths.iter().copied().max().unwrap_or(0);
            let total_payload = lengths.iter().map(|&l| l as u64).sum();
            let Some(layout) = Layout::fitting(row_bytes, largest) else {
                bail!(
                    "the largest bucket ({largest} bytes) does not fit in \
                     {MAX_PAGES_PER_BUCKET} rows of {row_bytes} bytes; nothing is cut off, \
                     so choose larger rows"
                );
            };
            let leaves = write_rows(&staging, &spill_path, &lengths, layout)?;
            fs::remove_file(&spill_path)?;
            let root = write_paths(&staging.join(ROWS_FILE), layout, leaves)?;
            let manifest = Manifest {
                format: FORMAT_VERSION,
                key_mapping: KEY_MAPPING_VERSION,
                pieces: pieces.hash(),
                buckets: BUCKETS,
                layout,
                profile: profile.to_string(),
                root,
                created,
                valid_until: created.saturating_add(valid_for),
            };
            let mut file = create(&staging.join(PIECES_FILE))?;
            file.write_all(&pieces.to_bytes())?;
            file.sync_all()?;
            let mut file = create(&staging.join(MANIFEST_FILE))?;
            file.write_all(&manifest.canonical())?;
            file.sync_all()?;
            Ok((
                manifest,
                BuildReport {
                    largest_payload: largest,
                    total_payload,
                },
            ))
        })();
        let (manifest, report) = match built {
            Ok(built) => built,
            Err(err) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(err);
            }
        };
        fs::rename(&staging, dir)
            .with_context(|| format!("moving the snapshot to {}", dir.display()))?;
        Ok((
            Snapshot {
                dir: dir.to_path_buf(),
                manifest,
            },
            report,
        ))
    }

    /// Opens a snapshot [`Snapshot::build`] wrote.
    pub fn open(dir: &Path) -> Result<Snapshot> {
        let bytes = fs::read(dir.join(MANIFEST_FILE))
            .with_context(|| format!("reading {}", dir.join(MANIFEST_FILE).display()))?;
        let manifest: Manifest = serde_json::from_slice(&bytes).context("reading the manifest")?;
        manifest.layout.check()?;
        let len = fs::metadata(dir.join(ROWS_FILE))
            .with_context(|| format!("reading {}", dir.join(ROWS_FILE).display()))?
            .len();
        ensure!(
            len == manifest.layout.rows() * u64::from(manifest.layout.row_bytes),
            "{ROWS_FILE} does not match the manifest"
        );
        let snapshot = Snapshot {
            dir: dir.to_path_buf(),
            manifest,
        };
        snapshot.pieces()?;
        Ok(snapshot)
    }

    /// The map of pieces to row groups, checked against the manifest.
    pub fn pieces(&self) -> Result<PieceMap> {
        let path = self.dir.join(PIECES_FILE);
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let map = PieceMap::from_bytes(&bytes)?;
        ensure!(
            map.hash() == self.manifest.pieces,
            "{PIECES_FILE} does not match the manifest"
        );
        Ok(map)
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The rows file, for a PIR server to load.
    pub fn rows_path(&self) -> PathBuf {
        self.dir.join(ROWS_FILE)
    }

    /// Row `index`, as served.
    pub fn row(&self, index: u64) -> Result<Vec<u8>> {
        let layout = self.manifest.layout;
        ensure!(index < layout.rows(), "there is no row {index}");
        let mut file = File::open(self.rows_path())?;
        file.seek(SeekFrom::Start(index * u64::from(layout.row_bytes)))?;
        let mut row = vec![0u8; layout.row_bytes as usize];
        file.read_exact(&mut row)?;
        Ok(row)
    }
}

/// Writes every row without its path; returns the leaf hashes.
fn write_rows(
    staging: &Path,
    spill_path: &Path,
    lengths: &[usize],
    layout: Layout,
) -> Result<Vec<Hash>> {
    let row_bytes = layout.row_bytes as usize;
    let chunk = layout.chunk_bytes();
    let body_bytes = row_bytes - layout.path_bytes();
    let mut spill = BufReader::new(File::open(spill_path)?);
    let mut rows = BufWriter::new(create(&staging.join(ROWS_FILE))?);
    let mut leaves = Vec::with_capacity(layout.rows() as usize);
    let mut payload = Vec::new();
    let mut row = vec![0u8; row_bytes];
    for (bucket, &length) in lengths.iter().enumerate() {
        payload.resize(length, 0);
        spill.read_exact(&mut payload)?;
        for (page, index) in layout.rows_of(bucket as u32).enumerate() {
            row.fill(0);
            row[..4].copy_from_slice(&(length as u32).to_le_bytes());
            row[4..8].copy_from_slice(&(page as u32).to_le_bytes());
            let from = (page * chunk).min(length);
            let to = (from + chunk).min(length);
            row[HEADER_BYTES..HEADER_BYTES + (to - from)].copy_from_slice(&payload[from..to]);
            leaves.push(leaf(index, &row[..body_bytes]));
            rows.write_all(&row)?;
        }
    }
    flush(rows)?;
    Ok(leaves)
}

/// Fills in every row's Merkle path; returns the root.
fn write_paths(rows_path: &Path, layout: Layout, leaves: Vec<Hash>) -> Result<Hash> {
    let width = 1usize << layout.depth();
    let mut levels = vec![leaves];
    levels[0].resize(width, empty_leaf());
    while levels.last().expect("one level").len() > 1 {
        let below = levels.last().expect("one level");
        let above = below
            .chunks(2)
            .map(|pair| node(&pair[0], &pair[1]))
            .collect();
        levels.push(above);
    }
    let root = levels.last().expect("one level")[0];
    let mut file = fs::OpenOptions::new().write(true).open(rows_path)?;
    let path_at = (layout.row_bytes as usize - layout.path_bytes()) as u64;
    let mut path = Vec::with_capacity(layout.path_bytes());
    for index in 0..layout.rows() as usize {
        path.clear();
        let mut i = index;
        for level in &levels[..levels.len() - 1] {
            path.extend_from_slice(&level[i ^ 1].0);
            i /= 2;
        }
        file.seek(SeekFrom::Start(
            index as u64 * u64::from(layout.row_bytes) + path_at,
        ))?;
        file.write_all(&path)?;
    }
    file.sync_all()?;
    Ok(root)
}

/// One checked row: a page of a bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub payload_len: usize,
    pub page: u32,
    pub chunk: Vec<u8>,
}

/// Checks that `row` is row `index` of the snapshot `manifest` names, and
/// returns its page. A wrong, altered or truncated row fails.
pub fn verify_row(manifest: &Manifest, index: u64, row: &[u8]) -> Result<Page> {
    let layout = manifest.layout;
    ensure!(index < layout.rows(), "there is no row {index}");
    ensure!(
        row.len() == layout.row_bytes as usize,
        "the row has {} bytes, not {}",
        row.len(),
        layout.row_bytes
    );
    let body_bytes = row.len() - layout.path_bytes();
    let mut hash = leaf(index, &row[..body_bytes]);
    let mut i = index;
    for sibling in row[body_bytes..].as_chunks::<32>().0 {
        let sibling = Hash(*sibling);
        hash = if i.is_multiple_of(2) {
            node(&hash, &sibling)
        } else {
            node(&sibling, &hash)
        };
        i /= 2;
    }
    ensure!(hash == manifest.root, "the row does not match the snapshot");

    let payload_len = u32::from_le_bytes(row[..4].try_into().expect("4 bytes")) as usize;
    let page = u32::from_le_bytes(row[4..8].try_into().expect("4 bytes"));
    let expected_page = index % u64::from(layout.pages_per_bucket);
    ensure!(
        u64::from(page) == expected_page,
        "the row is from another page"
    );
    ensure!(
        payload_len <= layout.bucket_capacity(),
        "the row claims an oversized bucket"
    );
    let chunk = layout.chunk_bytes();
    let from = (page as usize * chunk).min(payload_len);
    let to = (from + chunk).min(payload_len);
    let data = &row[HEADER_BYTES..HEADER_BYTES + chunk];
    ensure!(
        data[to - from..].iter().all(|&b| b == 0),
        "the row's padding is not empty"
    );
    Ok(Page {
        payload_len,
        page,
        chunk: data[..to - from].to_vec(),
    })
}

/// The payload of `bucket` from all its rows (in any order), each checked
/// against `manifest`.
pub fn assemble_bucket(manifest: &Manifest, bucket: u32, rows: &[Vec<u8>]) -> Result<Vec<u8>> {
    ensure!(bucket < manifest.buckets, "there is no bucket {bucket}");
    let layout = manifest.layout;
    ensure!(
        rows.len() == layout.pages_per_bucket as usize,
        "a bucket takes {} rows, not {}",
        layout.pages_per_bucket,
        rows.len()
    );
    let mut pages: Vec<Option<Page>> = vec![None; rows.len()];
    for row in rows {
        // The page number inside the row says which of the bucket's rows
        // it claims to be; the Merkle path checks that claim.
        ensure!(row.len() >= HEADER_BYTES, "the row is too short");
        let claimed = u32::from_le_bytes(row[4..8].try_into().expect("4 bytes"));
        ensure!(
            claimed < layout.pages_per_bucket,
            "the row is from another page"
        );
        let index = layout.rows_of(bucket).start + u64::from(claimed);
        let page = verify_row(manifest, index, row)?;
        let slot = &mut pages[claimed as usize];
        ensure!(slot.is_none(), "a page came twice");
        *slot = Some(page);
    }
    let pages: Vec<Page> = pages.into_iter().map(|p| p.expect("all pages")).collect();
    let length = pages[0].payload_len;
    ensure!(
        pages.iter().all(|p| p.payload_len == length),
        "the bucket's pages disagree on its length"
    );
    let payload: Vec<u8> = pages.into_iter().flat_map(|p| p.chunk).collect();
    ensure!(payload.len() == length, "the bucket's pages are incomplete");
    Ok(payload)
}

fn create(path: &Path) -> Result<File> {
    File::create(path).with_context(|| format!("creating {}", path.display()))
}

fn flush(writer: BufWriter<File>) -> Result<()> {
    let file = writer.into_inner().context("writing the snapshot")?;
    file.sync_all().context("writing the snapshot")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const DAY: u64 = 24 * 60 * 60;

    fn record(n: usize, bytes: usize) -> BucketRecord {
        BucketRecord {
            record: format!(
                "{{\"domain\":\"site{n}.example\",\"pad\":\"{}\"}}",
                "x".repeat(bytes)
            ),
            proof: None,
            also: Vec::new(),
        }
    }

    /// Bucket `b` holds `b % 5` records, and bucket 7 a big one.
    fn payloads(bucket: u32) -> Result<Vec<u8>> {
        let mut records: Vec<BucketRecord> =
            (0..(bucket % 5) as usize).map(|n| record(n, 40)).collect();
        if bucket == 7 {
            records.push(record(99, 5_000));
        }
        encode_bucket(&records)
    }

    fn build(dir: &Path, row_bytes: u32) -> Result<(Snapshot, BuildReport)> {
        Snapshot::build(
            dir,
            row_bytes,
            &PieceMap::by_bucket(),
            "test",
            NOW,
            DAY,
            payloads,
        )
    }

    fn rows_of(snapshot: &Snapshot, bucket: u32) -> Vec<Vec<u8>> {
        snapshot
            .manifest()
            .layout
            .rows_of(bucket)
            .map(|i| snapshot.row(i).unwrap())
            .collect()
    }

    #[test]
    fn every_bucket_takes_the_same_rows_and_reads_back_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pir");
        let (snapshot, report) = build(&path, 2048).unwrap();
        let layout = snapshot.manifest().layout;
        // The big bucket decides the page count for all.
        assert!(layout.pages_per_bucket > 1, "{layout:?}");
        assert_eq!(report.largest_payload, payloads(7).unwrap().len());
        assert_eq!(
            fs::metadata(snapshot.rows_path()).unwrap().len(),
            layout.rows() * 2048
        );
        let reopened = Snapshot::open(&path).unwrap();
        assert_eq!(reopened.manifest(), snapshot.manifest());
        for bucket in [0, 1, 4, 7, 9, BUCKETS - 1] {
            let mut rows = rows_of(&reopened, bucket);
            rows.reverse(); // Any order.
            let payload = assemble_bucket(reopened.manifest(), bucket, &rows).unwrap();
            assert_eq!(payload, payloads(bucket).unwrap(), "bucket {bucket}");
            assert_eq!(
                decode_bucket(&payload).unwrap().len(),
                (bucket % 5) as usize + usize::from(bucket == 7)
            );
        }
        assert!(build(&path, 2048).is_err(), "never overwritten");
    }

    #[test]
    fn altered_misplaced_or_short_rows_fail() {
        let dir = tempfile::tempdir().unwrap();
        let (snapshot, _) = build(&dir.path().join("pir"), 2048).unwrap();
        let manifest = snapshot.manifest();
        let layout = manifest.layout;
        let index = layout.rows_of(3).start;
        let row = snapshot.row(index).unwrap();
        assert!(verify_row(manifest, index, &row).is_ok());

        // Any flipped byte: header, chunk, padding or path.
        for at in [0, 5, 20, 1500, row.len() - 1] {
            let mut bad = row.clone();
            bad[at] ^= 1;
            assert!(verify_row(manifest, index, &bad).is_err(), "byte {at}");
        }
        // The right row presented as another.
        assert!(verify_row(manifest, index + 1, &row).is_err());
        assert!(verify_row(manifest, layout.rows(), &row).is_err());
        assert!(verify_row(manifest, index, &row[..row.len() - 1]).is_err());
        // Another snapshot's root.
        let other = Manifest {
            root: Hash::of(&[b"other"]),
            ..manifest.clone()
        };
        assert!(verify_row(&other, index, &row).is_err());

        // A bucket's rows with one swapped for another bucket's, or doubled.
        let mut rows = rows_of(&snapshot, 3);
        rows[0] = snapshot.row(layout.rows_of(4).start).unwrap();
        assert!(assemble_bucket(manifest, 3, &rows).is_err());
        let mut rows = rows_of(&snapshot, 3);
        rows[1] = rows[0].clone();
        assert!(assemble_bucket(manifest, 3, &rows).is_err());
        let rows = rows_of(&snapshot, 3);
        assert!(assemble_bucket(manifest, 3, &rows[1..]).is_err());
    }

    #[test]
    fn a_bucket_too_big_for_any_layout_fails_the_build_without_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pir");
        let huge = |bucket: u32| {
            let bytes = if bucket == 2 { 2_000_000 } else { 10 };
            encode_bucket(&[record(0, bytes)])
        };
        let err = Snapshot::build(&path, 2048, &PieceMap::by_bucket(), "test", NOW, DAY, huge)
            .unwrap_err();
        assert!(err.to_string().contains("nothing is cut off"), "{err}");
        assert!(!path.exists());
        assert!(!path.with_extension("staging").exists());
    }

    #[test]
    fn layouts_hold_their_paths_and_payloads() {
        let layout = Layout {
            row_bytes: 32 * 1024,
            pages_per_bucket: 8,
        };
        layout.check().unwrap();
        assert_eq!(layout.rows(), 131_072);
        assert_eq!(layout.depth(), 17);
        assert_eq!(layout.chunk_bytes(), 32 * 1024 - 8 - 17 * 32);
        // Not a power of two: the tree is padded, the depth rounds up.
        let odd = Layout {
            row_bytes: 4096,
            pages_per_bucket: 3,
        };
        assert_eq!(odd.depth(), 16);
        assert_eq!(Layout::fitting(4096, 0).unwrap().pages_per_bucket, 1);
        let two = Layout {
            row_bytes: 4096,
            pages_per_bucket: 2,
        };
        let need = two.bucket_capacity() + 1;
        assert_eq!(Layout::fitting(4096, need).unwrap().pages_per_bucket, 3);
        assert!(Layout::fitting(4096, usize::MAX / 2).is_none());
        assert!(Layout {
            row_bytes: 100,
            pages_per_bucket: 1
        }
        .check()
        .is_err());
        assert!(Layout {
            row_bytes: 4096,
            pages_per_bucket: 0
        }
        .check()
        .is_err());
    }

    #[test]
    fn manifests_are_signed_named_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let (snapshot, _) = build(&dir.path().join("pir"), 2048).unwrap();
        let manifest = snapshot.manifest().clone();
        let key = Keypair::generate_ed25519();
        let peer = key.public().to_peer_id();
        let signed = manifest.sign(&key).unwrap();
        assert_eq!(signed.verify(&peer, NOW).unwrap(), manifest);
        assert!(signed.verify(&peer, NOW + DAY + 1).is_err(), "expired");

        let stranger = Keypair::generate_ed25519().public().to_peer_id();
        assert!(signed.verify(&stranger, NOW).is_err());
        let mut tampered = signed.clone();
        let last = tampered.manifest.len() - 2;
        tampered.manifest[last] ^= 1;
        assert!(tampered.verify(&peer, NOW).is_err());

        // Every field is part of the identity.
        let changed = Manifest {
            profile: "other".into(),
            ..manifest.clone()
        };
        assert_ne!(changed.id(), manifest.id());
        let other_pieces = Manifest {
            pieces: Hash::of(&[b"other"]),
            ..manifest.clone()
        };
        assert_ne!(other_pieces.id(), manifest.id());
        let wrong_mapping = Manifest {
            key_mapping: KEY_MAPPING_VERSION + 1,
            ..manifest.clone()
        };
        assert!(wrong_mapping.check(NOW).is_err());
    }
}
