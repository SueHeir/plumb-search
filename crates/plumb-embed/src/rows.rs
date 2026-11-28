//! Vectors of a big page set by row, for the web index (plumb-index's
//! `web`), where a page's row is its number. A search cannot read every
//! vector of tens of millions of pages, so each page also has the signs
//! of its values, one bit each (48 bytes for 384 values). A search reads
//! all the signs and only the pages nearest by them again in full.
//!
//! Two files hold them, written once by `plumb-tools web-pages embed` and
//! never changed after: the vectors, one byte a value as [`Vectors`]
//! keeps them, and the signs. Each starts with [`rows_header`].
//!
//! [`Vectors`]: crate::Vectors

use std::collections::BinaryHeap;
use std::fs::File;
use std::ops::Range;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::vectors::{as_values, FileBytes};
use crate::{cosine, ModelId};

const MAGIC: &[u8; 8] = b"PLUMBROW";
/// Bytes before the first row of either file: the magic, whether the file
/// holds signs (1) or vectors (0), the model and the vector length.
pub const ROWS_HEADER_LEN: usize = 8 + 1 + 32 + 4;

/// The start of a row vectors file of `model`'s vectors of `dim` values,
/// or of their signs.
pub fn rows_header(model: &ModelId, dim: usize, of_signs: bool) -> Vec<u8> {
    let mut header = Vec::with_capacity(ROWS_HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.push(u8::from(of_signs));
    header.extend_from_slice(model);
    header.extend_from_slice(&(dim as u32).to_le_bytes());
    header
}

/// The signs of `vector`'s values, one bit each, set for zero and up: the
/// first value is the lowest bit of the first byte.
pub fn signs(vector: &[i8]) -> Vec<u8> {
    let mut bits = vec![0u8; vector.len().div_ceil(8)];
    for (i, value) in vector.iter().enumerate() {
        if *value >= 0 {
            bits[i / 8] |= 1 << (i % 8);
        }
    }
    bits
}

/// How many signs differ between two rows of signs.
#[inline(always)]
fn differ(a: &[u8], b: &[u8]) -> u32 {
    let (words, others) = (a.chunks_exact(8), b.chunks_exact(8));
    let tail: u32 = words
        .remainder()
        .iter()
        .zip(others.remainder())
        .map(|(x, y)| (x ^ y).count_ones())
        .sum();
    let word = |bytes: &[u8]| u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]));
    words
        .zip(others)
        .map(|(x, y)| (word(x) ^ word(y)).count_ones())
        .sum::<u32>()
        + tail
}

/// The vectors of a page set by row, with their signs ([`rows_header`]).
pub struct RowVectors {
    vectors: FileBytes,
    signs: FileBytes,
    model: ModelId,
    dim: usize,
    rows: usize,
}

impl RowVectors {
    /// Opens the vectors and signs files `plumb-tools web-pages embed`
    /// wrote. The signs are read now, as every search reads them all; the
    /// vectors as searches read them.
    pub fn open(vectors: &Path, signs: &Path) -> Result<Self> {
        let open = |path: &Path, of_signs: bool| -> Result<(FileBytes, ModelId, usize)> {
            let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
            let bytes = FileBytes::read(&file, of_signs)
                .with_context(|| format!("reading {}", path.display()))?;
            let header = bytes.get(..ROWS_HEADER_LEN).unwrap_or_default();
            if header.len() < ROWS_HEADER_LEN
                || &header[..8] != MAGIC
                || header[8] != u8::from(of_signs)
            {
                bail!(
                    "{} is not a row vectors file of the right kind",
                    path.display()
                );
            }
            let mut model = [0; 32];
            model.copy_from_slice(&header[9..41]);
            let dim = u32::from_le_bytes([header[41], header[42], header[43], header[44]]) as usize;
            Ok((bytes, model, dim))
        };
        let (vectors_bytes, model, dim) = open(vectors, false)?;
        let (signs_bytes, signs_model, signs_dim) = open(signs, true)?;
        if dim == 0 || signs_model != model || signs_dim != dim {
            bail!(
                "{} and {} were not made together",
                vectors.display(),
                signs.display()
            );
        }
        let (body, signs_body) = (
            vectors_bytes.len() - ROWS_HEADER_LEN,
            signs_bytes.len() - ROWS_HEADER_LEN,
        );
        let rows = body / dim;
        if body % dim != 0 || signs_body != rows * dim.div_ceil(8) {
            bail!(
                "{} and {} hold different numbers of rows",
                vectors.display(),
                signs.display()
            );
        }
        Ok(RowVectors {
            vectors: vectors_bytes,
            signs: signs_bytes,
            model,
            dim,
            rows,
        })
    }

    pub fn model(&self) -> ModelId {
        self.model
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// The vector of `row`.
    pub fn vector(&self, row: usize) -> Option<&[i8]> {
        let at = ROWS_HEADER_LEN + row.checked_mul(self.dim)?;
        Some(as_values(self.vectors.get(at..at + self.dim)?))
    }

    /// The `k` rows nearest `query`, nearest first, with their [`cosine`]:
    /// the `read_again` nearest by sign, read on `threads` threads, read
    /// again in full. Ties go to the lower row.
    pub fn nearest(
        &self,
        query: &[i8],
        k: usize,
        read_again: usize,
        threads: usize,
    ) -> Vec<(u32, f32)> {
        if query.len() != self.dim || k == 0 || self.rows == 0 {
            return Vec::new();
        }
        let wanted = signs(query);
        let all = &self.signs[ROWS_HEADER_LEN..];
        let read_again = read_again.max(k);
        let part = self.rows.div_ceil(threads.max(1));
        let nearest_in = |start: usize| {
            nearest_by_sign(
                all,
                &wanted,
                start..(start + part).min(self.rows),
                read_again,
            )
        };
        let starts: Vec<usize> = (0..self.rows).step_by(part.max(1)).collect();
        let mut by_sign: Vec<(u32, u32)> = match starts.as_slice() {
            [only] => nearest_in(*only),
            _ => std::thread::scope(|scope| {
                let reading: Vec<_> = starts
                    .iter()
                    .map(|&start| scope.spawn(move || nearest_in(start)))
                    .collect();
                reading
                    .into_iter()
                    .flat_map(|reading| reading.join().unwrap_or_default())
                    .collect()
            }),
        };
        by_sign.sort_unstable();
        by_sign.truncate(read_again);
        let mut found: Vec<(u32, f32)> = by_sign
            .into_iter()
            .filter_map(|(_, row)| Some((row, cosine(query, self.vector(row as usize)?))))
            .collect();
        found.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        found.truncate(k);
        found
    }
}

/// The `read_again` of `rows` whose signs in `all` differ least from
/// `wanted`, as how many differ and the row, in no order. Ties go to the
/// lower row.
fn nearest_by_sign(
    all: &[u8],
    wanted: &[u8],
    rows: Range<usize>,
    read_again: usize,
) -> Vec<(u32, u32)> {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("popcnt") {
        // SAFETY: the CPU has POPCNT.
        return unsafe { nearest_by_sign_popcnt(all, wanted, rows, read_again) };
    }
    nearest_by_sign_any(all, wanted, rows, read_again)
}

/// [`nearest_by_sign`] counting a word's bits in one instruction, which
/// x86-64 CPUs before 2008 lack: half the time of a search.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "popcnt")]
fn nearest_by_sign_popcnt(
    all: &[u8],
    wanted: &[u8],
    rows: Range<usize>,
    read_again: usize,
) -> Vec<(u32, u32)> {
    nearest_by_sign_any(all, wanted, rows, read_again)
}

#[inline(always)]
fn nearest_by_sign_any(
    all: &[u8],
    wanted: &[u8],
    rows: Range<usize>,
    read_again: usize,
) -> Vec<(u32, u32)> {
    let width = wanted.len();
    // The nearest so far, the farthest of them on top, and how far that
    // is once there are enough.
    let mut near: BinaryHeap<(u32, u32)> = BinaryHeap::with_capacity(read_again + 1);
    let mut farthest = u32::MAX;
    for row in rows {
        let differing = differ(&all[row * width..(row + 1) * width], wanted);
        if differing < farthest {
            near.push((differing, row as u32));
            if near.len() > read_again {
                near.pop();
            }
            if near.len() == read_again {
                farthest = near.peek().map_or(u32::MAX, |found| found.0);
            }
        }
    }
    near.into_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `vectors` as `plumb-tools web-pages embed` does.
    fn write(dir: &Path, vectors: &[Vec<i8>]) -> RowVectors {
        let model = [7; 32];
        let dim = vectors[0].len();
        let mut values = rows_header(&model, dim, false);
        let mut bits = rows_header(&model, dim, true);
        for vector in vectors {
            values.extend(vector.iter().map(|&v| v as u8));
            bits.extend(signs(vector));
        }
        std::fs::write(dir.join("vectors.bin"), values).unwrap();
        std::fs::write(dir.join("signs.bin"), bits).unwrap();
        RowVectors::open(&dir.join("vectors.bin"), &dir.join("signs.bin")).unwrap()
    }

    #[test]
    fn signs_are_one_bit_a_value() {
        let vector: Vec<i8> = (0..12).map(|i| if i % 3 == 0 { -5 } else { 0 }).collect();
        assert_eq!(signs(&vector), vec![0b1011_0110, 0b0000_1101]);
        assert_eq!(differ(&signs(&vector), &signs(&vector)), 0);
        assert_eq!(differ(&[0xff; 9], &[0; 9]), 72);
    }

    #[test]
    fn the_nearest_by_sign_are_ranked_by_cosine() {
        let dir = tempfile::tempdir().unwrap();
        // Row i points along value i, a little toward value i + 1.
        let vectors: Vec<Vec<i8>> = (0..16)
            .map(|i| {
                (0..16)
                    .map(|j| {
                        if j == i {
                            100
                        } else if j == (i + 1) % 16 {
                            40
                        } else {
                            -20
                        }
                    })
                    .collect()
            })
            .collect();
        let rows = write(dir.path(), &vectors);
        assert_eq!((rows.len(), rows.dim(), rows.model()), (16, 16, [7; 32]));
        let query = &vectors[5];
        for threads in [1, 3, 32] {
            let found = rows.nearest(query, 3, 16, threads);
            assert_eq!(found.iter().map(|f| f.0).collect::<Vec<_>>(), vec![5, 4, 6]);
            assert!((found[0].1 - 1.0).abs() < 1e-6);
        }
        // Reading only the nearest by sign again finds the row itself.
        assert_eq!(rows.nearest(query, 1, 1, 2)[0].0, 5);
        assert!(rows.nearest(&query[..8], 3, 16, 1).is_empty());
    }

    #[test]
    fn files_that_disagree_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), &[vec![1; 8], vec![2; 8]]);
        let mut bits = rows_header(&[7; 32], 8, true);
        bits.push(0xff);
        std::fs::write(dir.path().join("signs.bin"), bits).unwrap();
        let vectors = dir.path().join("vectors.bin");
        assert!(RowVectors::open(&vectors, &dir.path().join("signs.bin")).is_err());
        assert!(RowVectors::open(&vectors, &vectors).is_err());
    }
}
