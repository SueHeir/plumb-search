//! The vectors of a node's sites, saved in one file.
//!
//! Loaded vectors are read in place, from their file mapped into memory
//! (on Unix), not copied out of it: those pages are the system's to drop
//! when memory runs short and read again from the file when a search needs
//! them, where a copy would be memory the node holds, swapped out to disk
//! and back. Next to the file, the vectors keep where each site's row is,
//! its length and a table to find it by domain (about 20 bytes a site),
//! and the rows set since the file was loaded, until
//! [`Vectors::save_shared`] saves them and reads them from the new file.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, RandomState};
use std::io::{BufReader, BufWriter, Read, Write};
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{self, AtomicU64};
use std::sync::{PoisonError, RwLock};

use anyhow::{bail, Context, Result};
use hashbrown::HashTable;

use crate::{ModelId, TextHash};

/// File name of a node's vectors in its data directory.
pub const VECTORS_FILE_NAME: &str = "vectors.bin";

const MAGIC: &[u8; 8] = b"PLUMBVEC";
const VERSION: u32 = 1;
/// Bytes before the first row: the magic, the version, the model, the
/// vector length and the number of rows.
const HEADER_LEN: usize = 8 + 4 + 32 + 4 + 4;

/// The cosine of the angle between two vectors: 1 for the same direction,
/// 0 when either is all zeros.
pub fn cosine(a: &[i8], b: &[i8]) -> f32 {
    let dot = dot(a, b) as f32;
    let norms = (dot_self(a) as f32 * dot_self(b) as f32).sqrt();
    if norms > 0.0 {
        dot / norms
    } else {
        0.0
    }
}

fn dot(a: &[i8], b: &[i8]) -> i32 {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: the CPU has AVX2.
        return unsafe { dot_avx2(a, b) };
    }
    dot_any(a, b)
}

/// [`dot`] with AVX2, which multiplies twice as many values at a time as
/// the SSE2 every x86-64 CPU has: a third less time for a search by
/// meaning, and the same sum.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn dot_avx2(a: &[i8], b: &[i8]) -> i32 {
    dot_any(a, b)
}

#[inline(always)]
fn dot_any(a: &[i8], b: &[i8]) -> i32 {
    // The product of two bytes fits in 16 bits, and CPUs multiply twice as
    // many of those at a time as of 32 bits.
    a.iter()
        .zip(b)
        .map(|(&x, &y)| i32::from(i16::from(x) * i16::from(y)))
        .sum()
}

fn dot_self(a: &[i8]) -> i32 {
    dot(a, a)
}

/// Sites' vectors, by domain, each with the hash of the text it was made
/// from, all made by one model.
///
/// Each site has a row as a vectors file holds it: the domain's length in
/// one byte, the domain, the text hash and `dim` values of one byte.
pub struct Vectors {
    model: ModelId,
    dim: usize,
    /// The file the vectors were loaded from, whole.
    file: FileBytes,
    /// Rows set since the file was loaded, one after the other.
    added: Vec<u8>,
    /// Where each site's row starts: in `file`, or with [`ADDED`] set, in
    /// `added`.
    rows: Vec<u64>,
    norms: Vec<f32>,
    /// Row numbers, by domain.
    by_domain: HashTable<u32>,
    hasher: RandomState,
    /// Changes with every change to the sites or their vectors, so a save
    /// can tell whether they changed while it wrote them.
    version: u64,
}

/// Marks a row start in [`Vectors::added`] rather than the file.
const ADDED: u64 = 1 << 63;

/// The next [`Vectors::version`], never given before.
fn next_version() -> u64 {
    static VERSIONS: AtomicU64 = AtomicU64::new(0);
    VERSIONS.fetch_add(1, atomic::Ordering::Relaxed)
}

/// The bytes of a row that starts with a domain of `domain_len` bytes.
fn row_len(domain_len: u8, dim: usize) -> usize {
    1 + usize::from(domain_len) + 32 + dim
}

/// The row that starts at `at` (see [`Vectors::rows`]).
fn row_at<'a>(file: &'a [u8], added: &'a [u8], at: u64, dim: usize) -> &'a [u8] {
    let (bytes, start) = if at & ADDED == 0 {
        (file, at as usize)
    } else {
        (added, (at & !ADDED) as usize)
    };
    &bytes[start..start + row_len(bytes[start], dim)]
}

fn row_domain(row: &[u8]) -> &[u8] {
    &row[1..1 + usize::from(row[0])]
}

/// The values of the row that starts at `at`.
fn values_at<'a>(file: &'a [u8], added: &'a [u8], at: u64, dim: usize) -> &'a [i8] {
    let row = row_at(file, added, at, dim);
    as_values(&row[row.len() - dim..])
}

/// The text hash and values of a row.
fn row_hash_values(row: &[u8]) -> (&TextHash, &[i8]) {
    let (hash, values) = row[1 + usize::from(row[0])..]
        .split_first_chunk::<32>()
        .expect("a row has a text hash");
    (hash, as_values(values))
}

/// A row's values: each byte is one.
fn as_values(bytes: &[u8]) -> &[i8] {
    // SAFETY: i8 has the size and alignment of u8, and any byte is an i8.
    unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<i8>(), bytes.len()) }
}

/// A domain of a row, which was UTF-8 when the row was read or set.
fn as_domain(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap_or_default()
}

impl Vectors {
    /// No vectors yet, for the model `model` of `dim` values.
    pub fn new(model: ModelId, dim: usize) -> Self {
        Vectors {
            model,
            dim,
            file: FileBytes::Held(Vec::new()),
            added: Vec::new(),
            rows: Vec::new(),
            norms: Vec::new(),
            by_domain: HashTable::new(),
            hasher: RandomState::new(),
            version: next_version(),
        }
    }

    /// The model the vectors were made by.
    pub fn model(&self) -> ModelId {
        self.model
    }

    /// Values per vector.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Number of sites with a vector.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The hash of the text `domain`'s vector was made from, and the vector.
    pub fn get(&self, domain: &str) -> Option<(&TextHash, &[i8])> {
        let row = self.find(domain.as_bytes())?;
        Some(row_hash_values(self.row(row)))
    }

    fn row(&self, row: usize) -> &[u8] {
        row_at(&self.file, &self.added, self.rows[row], self.dim)
    }

    /// The row number of `domain`.
    fn find(&self, domain: &[u8]) -> Option<usize> {
        let hash = self.hasher.hash_one(domain);
        self.by_domain
            .find(hash, |&row| row_domain(self.row(row as usize)) == domain)
            .map(|&row| row as usize)
    }

    /// Sets `domain`'s vector, made from the text of hash `hash`.
    pub fn insert(&mut self, domain: &str, hash: TextHash, vector: &[i8]) -> Result<()> {
        if vector.len() != self.dim {
            bail!(
                "a vector of {} values for {domain}, expected {}",
                vector.len(),
                self.dim
            );
        }
        let Ok(domain_len) = u8::try_from(domain.len()) else {
            bail!("a domain of {} bytes cannot be saved", domain.len());
        };
        let norm = (dot_self(vector) as f32).sqrt();
        let new_row = |added: &mut Vec<u8>| {
            let at = added.len() as u64 | ADDED;
            added.push(domain_len);
            added.extend_from_slice(domain.as_bytes());
            added.extend_from_slice(&hash);
            added.extend(vector.iter().map(|&v| v as u8));
            at
        };
        match self.find(domain.as_bytes()) {
            // Set since loading: changed where it is.
            Some(row) if self.rows[row] & ADDED != 0 => {
                let start = (self.rows[row] & !ADDED) as usize + 1 + domain.len();
                let (old_hash, old_values) =
                    self.added[start..start + 32 + self.dim].split_at_mut(32);
                old_hash.copy_from_slice(&hash);
                for (old, &new) in old_values.iter_mut().zip(vector) {
                    *old = new as u8;
                }
                self.norms[row] = norm;
            }
            Some(row) => {
                self.rows[row] = new_row(&mut self.added);
                self.norms[row] = norm;
            }
            None => {
                let row = u32::try_from(self.rows.len()).context("too many vectors")?;
                let at = new_row(&mut self.added);
                self.add_row(row, at, norm);
            }
        }
        self.version = next_version();
        Ok(())
    }

    /// Adds the row number `row`, at `at`, for a domain not there yet.
    fn add_row(&mut self, row: u32, at: u64, norm: f32) {
        let (file, added, dim) = (&self.file[..], &self.added[..], self.dim);
        let hash = self
            .hasher
            .hash_one(row_domain(row_at(file, added, at, dim)));
        self.rows.push(at);
        self.norms.push(norm);
        let (rows, hasher) = (&self.rows, &self.hasher);
        self.by_domain.insert_unique(hash, row, |&row| {
            hasher.hash_one(row_domain(row_at(file, added, rows[row as usize], dim)))
        });
    }

    /// Keeps only the sites whose domain `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        let mut added = Vec::new();
        let mut kept = 0;
        for row in 0..self.rows.len() {
            let mut at = self.rows[row];
            let bytes = row_at(&self.file, &self.added, at, self.dim);
            if !keep(as_domain(row_domain(bytes))) {
                continue;
            }
            if at & ADDED != 0 {
                at = added.len() as u64 | ADDED;
                added.extend_from_slice(bytes);
            }
            self.rows[kept] = at;
            self.norms[kept] = self.norms[row];
            kept += 1;
        }
        self.added = added;
        if kept < self.rows.len() {
            self.rows.truncate(kept);
            self.norms.truncate(kept);
            self.index_domains();
            self.version = next_version();
        }
    }

    /// Makes [`Vectors::by_domain`] anew from the rows.
    fn index_domains(&mut self) {
        let (file, added, rows, dim) = (&self.file[..], &self.added[..], &self.rows, self.dim);
        let domain_hash = |row: u32| {
            self.hasher
                .hash_one(row_domain(row_at(file, added, rows[row as usize], dim)))
        };
        let mut by_domain = HashTable::with_capacity(rows.len());
        for row in 0..rows.len() as u32 {
            by_domain.insert_unique(domain_hash(row), row, |&row| domain_hash(row));
        }
        self.by_domain = by_domain;
    }

    /// How close `domain`'s vector is to `query`, by [`cosine`]; `None`
    /// when `domain` has none.
    pub fn closeness(&self, query: &[i8], domain: &str) -> Option<f32> {
        let row = self.find(domain.as_bytes())?;
        Some(self.cosine_row(query, (dot_self(query) as f32).sqrt(), row))
    }

    fn cosine_row(&self, query: &[i8], query_norm: f32, row: usize) -> f32 {
        let norms = query_norm * self.norms[row];
        if norms > 0.0 {
            let values = values_at(&self.file, &self.added, self.rows[row], self.dim);
            dot(query, values) as f32 / norms
        } else {
            0.0
        }
    }

    /// The `k` sites closest to `query`, closest first, with their
    /// [`cosine`]; ties go to the alphabetically first domain.
    pub fn nearest(&self, query: &[i8], k: usize) -> Vec<(&str, f32)> {
        if query.len() != self.dim || k == 0 {
            return Vec::new();
        }
        let query_norm = (dot_self(query) as f32).sqrt();
        // The closest so far, the farthest of them on top.
        let mut closest: BinaryHeap<Scored<'_>> = BinaryHeap::with_capacity(k.min(self.len()));
        let (file, added, dim) = (&self.file[..], &self.added[..], self.dim);
        for (row, (&at, &norm)) in self.rows.iter().zip(&self.norms).enumerate() {
            // As cosine_row, with what it looks up for each row looked up once.
            let norms = query_norm * norm;
            let cosine = if norms > 0.0 {
                dot(query, values_at(file, added, at, dim)) as f32 / norms
            } else {
                0.0
            };
            if closest.len() < k {
                let domain = row_domain(self.row(row));
                closest.push(Scored { cosine, domain });
                continue;
            }
            let Some(mut farthest) = closest.peek_mut() else {
                continue;
            };
            let closer = match cosine.total_cmp(&farthest.cosine) {
                Ordering::Greater => true,
                Ordering::Equal => row_domain(self.row(row)) < farthest.domain,
                Ordering::Less => false,
            };
            if closer {
                *farthest = Scored {
                    cosine,
                    domain: row_domain(self.row(row)),
                };
            }
        }
        closest
            .into_sorted_vec()
            .into_iter()
            .map(|scored| (as_domain(scored.domain), scored.cosine))
            .collect()
    }

    /// Reads vectors saved by [`Vectors::save`].
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        FileBytes::read(&file)
            .and_then(Self::read)
            .with_context(|| format!("reading vectors from {}", path.display()))
    }

    fn read(file: FileBytes) -> Result<Self> {
        let (model, dim, count) = read_header(&mut &file[..])?;
        let mut vectors = Vectors::new(model, dim);
        vectors.file = file;
        // No more than the file holds, whatever its header says.
        let rows = count.min(vectors.file.len() / row_len(0, dim));
        vectors.rows.reserve_exact(rows);
        vectors.norms.reserve_exact(rows);
        vectors.by_domain = HashTable::with_capacity(rows);
        let mut at = HEADER_LEN;
        for _ in 0..count {
            let row = vectors
                .file
                .get(at..)
                .and_then(|rest| rest.get(..row_len(*rest.first()?, dim)))
                .context("the file ends within a row")?;
            let domain = row_domain(row);
            std::str::from_utf8(domain).context("a domain is not UTF-8")?;
            let norm = (dot_self(row_hash_values(row).1) as f32).sqrt();
            let len = row.len();
            // A later row of the same site is the one kept.
            match vectors.find(domain) {
                Some(row) => {
                    vectors.rows[row] = at as u64;
                    vectors.norms[row] = norm;
                }
                None => {
                    let row = u32::try_from(vectors.rows.len()).context("too many vectors")?;
                    vectors.add_row(row, at as u64, norm);
                }
            }
            at += len;
        }
        Ok(vectors)
    }

    /// The model, length and number of the vectors saved in `path`, read
    /// from its start only.
    pub fn read_header(path: &Path) -> Result<(ModelId, usize, usize)> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        read_header(&mut BufReader::new(file))
            .with_context(|| format!("reading vectors from {}", path.display()))
    }

    /// Calls `each` with the domain, text hash and vector of every site in
    /// the vectors file at `path`, one at a time, without holding them
    /// all; gives the file's model and vector length.
    pub fn for_each_in(
        path: &Path,
        mut each: impl FnMut(&str, &TextHash, &[i8]) -> Result<()>,
    ) -> Result<(ModelId, usize)> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut input = BufReader::new(file);
        let mut read = || -> Result<(ModelId, usize)> {
            let (model, dim, count) = read_header(&mut input)?;
            read_rows(&mut input, dim, count, &mut each)?;
            Ok((model, dim))
        };
        read().with_context(|| format!("reading vectors from {}", path.display()))
    }

    /// Saves the vectors to `path`, replacing it only once all are written.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.write_file(path).map(drop)
    }

    /// Saves the vectors in `vectors` to `path`, as [`Vectors::save`] does,
    /// then reads them in place from the new file, so the rows set since
    /// they were loaded no longer take memory of their own. Searches go on
    /// while the file is written; when the vectors change meanwhile, they
    /// are read from the file at a later save.
    pub fn save_shared(vectors: &RwLock<Vectors>, path: &Path) -> Result<()> {
        let (written, rows, version) = {
            let held = vectors.read().unwrap_or_else(PoisonError::into_inner);
            let (written, rows) = held.write_file(path)?;
            (written, rows, held.version)
        };
        #[cfg(unix)]
        {
            let file = FileBytes::read(&written)
                .with_context(|| format!("reading vectors from {}", path.display()))?;
            let mut held = vectors.write().unwrap_or_else(PoisonError::into_inner);
            if held.version == version && held.rows.len() == rows.len() {
                let old = (
                    std::mem::replace(&mut held.file, file),
                    std::mem::take(&mut held.added),
                    std::mem::replace(&mut held.rows, rows),
                );
                // Freed once searches can go on.
                drop(held);
                drop(old);
            }
        }
        #[cfg(not(unix))]
        drop((written, rows, version));
        Ok(())
    }

    /// Saves the vectors to `path`; gives the file, open, and where each
    /// row starts in it.
    fn write_file(&self, path: &Path) -> Result<(File, Vec<u64>)> {
        let part = path.with_extension("bin.part");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&part)
            .with_context(|| format!("creating {}", part.display()))?;
        let mut out = BufWriter::new(file);
        let mut rows = Vec::with_capacity(self.rows.len());
        let write = || -> Result<File> {
            out.write_all(MAGIC)?;
            out.write_all(&VERSION.to_le_bytes())?;
            out.write_all(&self.model)?;
            out.write_all(&u32::try_from(self.dim)?.to_le_bytes())?;
            out.write_all(&u32::try_from(self.len())?.to_le_bytes())?;
            let mut at = HEADER_LEN as u64;
            for row in 0..self.len() {
                let bytes = self.row(row);
                out.write_all(bytes)?;
                rows.push(at);
                at += bytes.len() as u64;
            }
            let file = out.into_inner()?;
            file.sync_all()?;
            Ok(file)
        };
        let written = write().with_context(|| format!("writing {}", part.display()))?;
        std::fs::rename(&part, path)
            .with_context(|| format!("renaming {} to {}", part.display(), path.display()))?;
        Ok((written, rows))
    }
}

/// A site among the closest to a query: ordered closest first, ties by
/// domain.
struct Scored<'a> {
    cosine: f32,
    domain: &'a [u8],
}

impl Ord for Scored<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .cosine
            .total_cmp(&self.cosine)
            .then_with(|| self.domain.cmp(other.domain))
    }
}

impl PartialOrd for Scored<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Scored<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Scored<'_> {}

/// The bytes of a vectors file.
enum FileBytes {
    /// The file mapped into memory, on Unix, where a file can be replaced
    /// while mapped.
    #[cfg(unix)]
    Mapped(memmap2::Mmap),
    Held(Vec<u8>),
}

impl FileBytes {
    #[cfg(unix)]
    fn read(file: &File) -> Result<Self> {
        // SAFETY: vectors files are replaced whole, by renaming a new file
        // over them, and never changed in place, so the mapped bytes stay
        // as they were read. Populated: read now, as a copy would be, not
        // while the first search waits.
        let map = unsafe { memmap2::MmapOptions::new().populate().map(file)? };
        Ok(FileBytes::Mapped(map))
    }

    #[cfg(not(unix))]
    fn read(mut file: &File) -> Result<Self> {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(FileBytes::Held(bytes))
    }
}

impl Deref for FileBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            #[cfg(unix)]
            FileBytes::Mapped(map) => map,
            FileBytes::Held(bytes) => bytes,
        }
    }
}

/// The model, vector length and number of vectors at the start of a
/// vectors file.
fn read_header(input: &mut impl Read) -> Result<(ModelId, usize, usize)> {
    let mut magic = [0; 8];
    input.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("not a vectors file");
    }
    let version = read_u32(input)?;
    if version != VERSION {
        bail!("vectors file version {version}, expected {VERSION}");
    }
    let mut model = [0; 32];
    input.read_exact(&mut model)?;
    let dim = read_u32(input)? as usize;
    let count = read_u32(input)? as usize;
    Ok((model, dim, count))
}

/// Reads the `count` rows of `dim` values after a vectors file's header.
fn read_rows(
    input: &mut impl Read,
    dim: usize,
    count: usize,
    mut each: impl FnMut(&str, &TextHash, &[i8]) -> Result<()>,
) -> Result<()> {
    let mut vector = vec![0; dim];
    let mut bytes = vec![0u8; dim];
    for _ in 0..count {
        let mut len = [0; 1];
        input.read_exact(&mut len)?;
        let mut domain = vec![0; usize::from(len[0])];
        input.read_exact(&mut domain)?;
        let domain = String::from_utf8(domain).context("a domain is not UTF-8")?;
        let mut hash = [0; 32];
        input.read_exact(&mut hash)?;
        input.read_exact(&mut bytes)?;
        for (value, byte) in vector.iter_mut().zip(&bytes) {
            *value = *byte as i8;
        }
        each(&domain, &hash, &vector)?;
    }
    Ok(())
}

fn read_u32(input: &mut impl Read) -> Result<u32> {
    let mut bytes = [0; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_sites_come_first_and_files_round_trip() {
        let mut vectors = Vectors::new([1; 32], 3);
        vectors.insert("east.com", [2; 32], &[127, 0, 0]).unwrap();
        vectors.insert("north.com", [3; 32], &[0, 127, 0]).unwrap();
        vectors
            .insert("northeast.com", [4; 32], &[90, 90, 0])
            .unwrap();
        assert!(vectors.insert("bad.com", [0; 32], &[1, 2]).is_err());
        let query = [100, 20, 0];
        let nearest = vectors.nearest(&query, 2);
        assert_eq!(nearest[0].0, "east.com");
        assert_eq!(nearest[1].0, "northeast.com");
        assert!(nearest[0].1 > nearest[1].1);
        assert_eq!(vectors.nearest(&query, 10).len(), 3);
        assert_eq!(vectors.closeness(&[0, 5, 0], "north.com"), Some(1.0));
        assert_eq!(vectors.closeness(&query, "west.com"), None);

        // Replacing a vector keeps one row per site.
        vectors.insert("north.com", [5; 32], &[0, 0, 127]).unwrap();
        assert_eq!(vectors.len(), 3);
        assert_eq!(vectors.get("north.com").unwrap().0, &[5; 32]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(VECTORS_FILE_NAME);
        vectors.save(&path).unwrap();
        let loaded = Vectors::load(&path).unwrap();
        assert_eq!(loaded.model(), [1; 32]);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded.get("north.com"), vectors.get("north.com"));
        assert_eq!(loaded.nearest(&query, 3), vectors.nearest(&query, 3));

        let mut kept = loaded;
        kept.retain(|domain| domain != "east.com");
        assert_eq!(kept.len(), 2);
        assert_eq!(kept.nearest(&query, 1)[0].0, "northeast.com");

        let (model, dim, count) = Vectors::read_header(&path).unwrap();
        assert_eq!((model, dim, count), ([1; 32], 3, 3));
        let mut seen = Vec::new();
        Vectors::for_each_in(&path, |domain, hash, vector| {
            seen.push((domain.to_string(), *hash, vector.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen.contains(&("north.com".to_string(), [5; 32], vec![0, 0, 127])));
        drop(kept);

        std::fs::write(&path, b"nonsense").unwrap();
        assert!(Vectors::load(&path).is_err());
        assert!(Vectors::read_header(&path).is_err());
    }

    /// Sites `0.com` to `n.com`, with vectors that tie now and then.
    fn sites(n: usize) -> Vec<(String, TextHash, Vec<i8>)> {
        (0..n)
            .map(|i| {
                let vector = (0..8).map(|d| ((i % 37) * (d + 3) % 255) as i8).collect();
                (format!("{i}.com"), [(i % 251) as u8; 32], vector)
            })
            .collect()
    }

    /// What every site's vector and closeness are, and the nearest sites.
    fn answers(vectors: &Vectors, n: usize) -> Vec<String> {
        let query = [5, -3, 9, 0, 1, 1, -8, 2];
        let mut seen: Vec<String> = (0..n + 1)
            .map(|i| {
                let domain = format!("{i}.com");
                format!(
                    "{domain} {:?} {:?}",
                    vectors.get(&domain),
                    vectors.closeness(&query, &domain)
                )
            })
            .collect();
        seen.extend(
            vectors
                .nearest(&query, 25)
                .into_iter()
                .map(|(domain, cosine)| format!("{domain} {cosine}")),
        );
        seen
    }

    #[test]
    fn vectors_read_in_place_answer_as_those_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(VECTORS_FILE_NAME);
        let mut set = Vectors::new([1; 32], 8);
        for (domain, hash, vector) in sites(300) {
            set.insert(&domain, hash, &vector).unwrap();
        }
        assert!(set.insert(&"a".repeat(256), [0; 32], &[0; 8]).is_err());
        set.save(&path).unwrap();
        let shared = RwLock::new(Vectors::load(&path).unwrap());
        assert_eq!(answers(&shared.read().unwrap(), 300), answers(&set, 300));

        // Changed, added and dropped sites, then saved and read again.
        for vectors in [&mut set, &mut shared.write().unwrap()] {
            vectors.insert("7.com", [9; 32], &[1; 8]).unwrap();
            vectors.insert("7.com", [8; 32], &[2; 8]).unwrap();
            vectors.insert("new.com", [7; 32], &[3; 8]).unwrap();
            vectors.insert("new.com", [6; 32], &[-4; 8]).unwrap();
            vectors.retain(|domain| domain != "8.com" && domain != "9.com");
            vectors.insert("9.com", [5; 32], &[5; 8]).unwrap();
        }
        assert_eq!(answers(&shared.read().unwrap(), 300), answers(&set, 300));
        Vectors::save_shared(&shared, &path).unwrap();
        let held = shared.read().unwrap();
        assert!(held.added.is_empty() || cfg!(not(unix)));
        assert_eq!(answers(&held, 300), answers(&set, 300));
        assert_eq!(
            answers(&Vectors::load(&path).unwrap(), 300),
            answers(&set, 300)
        );
        assert_eq!(held.nearest(&[1; 8], 1000).len(), 300);
    }

    #[test]
    fn a_short_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(VECTORS_FILE_NAME);
        let mut vectors = Vectors::new([1; 32], 8);
        for (domain, hash, vector) in sites(3) {
            vectors.insert(&domain, hash, &vector).unwrap();
        }
        vectors.save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        assert!(Vectors::load(&path).is_err());
    }
}
