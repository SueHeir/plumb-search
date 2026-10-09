//! The vectors of a node's sites, saved in one file.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::{ModelId, TextHash};

/// File name of a node's vectors in its data directory.
pub const VECTORS_FILE_NAME: &str = "vectors.bin";

const MAGIC: &[u8; 8] = b"PLUMBVEC";
const VERSION: u32 = 1;

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
    a.iter()
        .zip(b)
        .map(|(&x, &y)| i32::from(x) * i32::from(y))
        .sum()
}

fn dot_self(a: &[i8]) -> i32 {
    dot(a, a)
}

/// Sites' vectors, by domain, each with the hash of the text it was made
/// from, all made by one model.
pub struct Vectors {
    model: ModelId,
    dim: usize,
    domains: Vec<String>,
    hashes: Vec<TextHash>,
    /// `dim` values per site, in the order of `domains`.
    values: Vec<i8>,
    norms: Vec<f32>,
    rows: HashMap<String, usize>,
}

impl Vectors {
    /// No vectors yet, for the model `model` of `dim` values.
    pub fn new(model: ModelId, dim: usize) -> Self {
        Vectors {
            model,
            dim,
            domains: Vec::new(),
            hashes: Vec::new(),
            values: Vec::new(),
            norms: Vec::new(),
            rows: HashMap::new(),
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
        self.domains.len()
    }

    pub fn is_empty(&self) -> bool {
        self.domains.is_empty()
    }

    /// The hash of the text `domain`'s vector was made from, and the vector.
    pub fn get(&self, domain: &str) -> Option<(&TextHash, &[i8])> {
        let row = *self.rows.get(domain)?;
        Some((&self.hashes[row], self.row(row)))
    }

    fn row(&self, row: usize) -> &[i8] {
        &self.values[row * self.dim..(row + 1) * self.dim]
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
        let norm = (dot_self(vector) as f32).sqrt();
        match self.rows.get(domain) {
            Some(&row) => {
                self.hashes[row] = hash;
                self.values[row * self.dim..(row + 1) * self.dim].copy_from_slice(vector);
                self.norms[row] = norm;
            }
            None => {
                self.rows.insert(domain.to_string(), self.domains.len());
                self.domains.push(domain.to_string());
                self.hashes.push(hash);
                self.values.extend_from_slice(vector);
                self.norms.push(norm);
            }
        }
        Ok(())
    }

    /// Keeps only the sites whose domain `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        let mut kept = Vectors::new(self.model, self.dim);
        for (row, domain) in self.domains.iter().enumerate() {
            if keep(domain) {
                // The vector has the right length: it is already here.
                let _ = kept.insert(domain, self.hashes[row], self.row(row));
            }
        }
        *self = kept;
    }

    /// How close `domain`'s vector is to `query`, by [`cosine`]; `None`
    /// when `domain` has none.
    pub fn closeness(&self, query: &[i8], domain: &str) -> Option<f32> {
        let row = *self.rows.get(domain)?;
        Some(self.cosine_row(query, (dot_self(query) as f32).sqrt(), row))
    }

    fn cosine_row(&self, query: &[i8], query_norm: f32, row: usize) -> f32 {
        let norms = query_norm * self.norms[row];
        if norms > 0.0 {
            dot(query, self.row(row)) as f32 / norms
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
        let mut scored: Vec<(f32, usize)> = (0..self.len())
            .map(|row| (self.cosine_row(query, query_norm, row), row))
            .collect();
        let order = |a: &(f32, usize), b: &(f32, usize)| {
            b.0.total_cmp(&a.0)
                .then_with(|| self.domains[a.1].cmp(&self.domains[b.1]))
        };
        if scored.len() > k {
            scored.select_nth_unstable_by(k - 1, order);
            scored.truncate(k);
        }
        scored.sort_by(order);
        scored
            .into_iter()
            .map(|(cosine, row)| (self.domains[row].as_str(), cosine))
            .collect()
    }

    /// Reads vectors saved by [`Vectors::save`].
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::read(&mut BufReader::new(file))
            .with_context(|| format!("reading vectors from {}", path.display()))
    }

    fn read(input: &mut impl Read) -> Result<Self> {
        let (model, dim, count) = read_header(input)?;
        let mut vectors = Vectors::new(model, dim);
        read_rows(input, dim, count, |domain, hash, vector| {
            vectors.insert(domain, *hash, vector)
        })?;
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
        let part = path.with_extension("bin.part");
        let file = File::create(&part).with_context(|| format!("creating {}", part.display()))?;
        let mut out = BufWriter::new(file);
        self.write(&mut out)
            .and_then(|()| Ok(out.into_inner()?.sync_all()?))
            .with_context(|| format!("writing {}", part.display()))?;
        std::fs::rename(&part, path)
            .with_context(|| format!("renaming {} to {}", part.display(), path.display()))
    }

    fn write(&self, out: &mut impl Write) -> Result<()> {
        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        out.write_all(&self.model)?;
        out.write_all(&u32::try_from(self.dim)?.to_le_bytes())?;
        let kept: Vec<usize> = (0..self.len())
            .filter(|&row| self.domains[row].len() <= usize::from(u8::MAX))
            .collect();
        out.write_all(&u32::try_from(kept.len())?.to_le_bytes())?;
        for row in kept {
            let domain = self.domains[row].as_bytes();
            out.write_all(&[domain.len() as u8])?;
            out.write_all(domain)?;
            out.write_all(&self.hashes[row])?;
            let bytes: Vec<u8> = self.row(row).iter().map(|&v| v as u8).collect();
            out.write_all(&bytes)?;
        }
        Ok(())
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

        std::fs::write(&path, b"nonsense").unwrap();
        assert!(Vectors::load(&path).is_err());
        assert!(Vectors::read_header(&path).is_err());
    }
}
