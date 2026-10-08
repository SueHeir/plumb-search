//! Relations as matrices: a fact kind ("capital", "founder") learned as a
//! map from the vector of a thing to the vector of what it is related to,
//! so that facts can be followed, chained and checked by arithmetic on the
//! vectors the embedding model already makes.
//!
//! For a kind, every known fact `(subject, object)` gives a pair of unit
//! vectors, and the map takes a subject `s` to `s + W · s + b`, with `W`
//! and `b` the ridge regression of objects minus subjects on subjects. The
//! penalty pulls `W` toward nothing, so a map fitted on few facts stays
//! close to the subject itself (moved by `b` toward the kind's region of
//! the space): an object's text often names its subject ("capital of
//! Australia"), and plain closeness is a strong start. Then:
//!
//! - **Following** a relation: the object nearest `W · subject`. Things the
//!   model never saw a fact about get an answer too, from their text alone.
//! - **Chaining**: "capital of the country Toyota is headquartered in" is
//!   the headquarters map, then the capital map.
//! - **Checking** a claim: how close the claimed object is to `W · subject`,
//!   turned into a probability by a logistic fit of true facts against
//!   facts with the wrong object ([`Relation::plausibility`]).
//!
//! The fit is closed-form and runs in a fixed order, so two nodes with the
//! same facts and vectors get the same maps, and a node can check another's
//! by scoring them on facts of its own.
//!
//! The idea that relations are close to linear maps between meanings comes
//! from studies of what language models hold inside (Hernandez et al.,
//! "Linearity of Relation Decoding in Transformer Language Models", 2023).

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};

use crate::ModelId;

/// File name of a node's relation maps.
pub const RELATIONS_FILE_NAME: &str = "relations.bin";

const MAGIC: &[u8; 8] = b"PLUMBREL";
const VERSION: u32 = 1;

/// Most kinds a relations file may hold, so a bad file cannot ask for a
/// huge allocation.
const MAX_RELATIONS: usize = 256;

/// Most values per vector a relations file may hold.
const MAX_DIM: usize = 4096;

/// `vector` (one byte per value) as floats of length 1, or `None` when it
/// is all zeros.
pub fn unit(vector: &[i8]) -> Option<Vec<f32>> {
    normalized(vector.iter().map(|&x| f32::from(x)).collect())
}

fn normalized(mut values: Vec<f32>) -> Option<Vec<f32>> {
    let length = values.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !length.is_normal() {
        return None;
    }
    values.iter_mut().for_each(|x| *x /= length);
    Some(values)
}

/// The dot product of two vectors; their cosine when both have length 1.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// One kind of fact as a map between vectors.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    /// The kind's key ([`plumb_core::facts::FactKind::key`]).
    pub key: String,
    /// Facts it was fitted on.
    pub trained_on: u32,
    /// The logistic fit that turns a closeness into a probability:
    /// `1 / (1 + e^-(scale * closeness + offset))`.
    pub scale: f32,
    pub offset: f32,
    dim: usize,
    /// `dim + 1` rows of `dim` values: row `i` is what input value `i`
    /// adds to the output (on top of the input itself), and the last row
    /// is the bias.
    weights: Vec<f32>,
}

impl Relation {
    /// Fits the map of `key` from `pairs` of unit vectors (subject, object)
    /// of `dim` values, with ridge penalty `ridge` (per fact).
    pub fn fit(key: &str, dim: usize, pairs: &[(&[f32], &[f32])], ridge: f64) -> Result<Self> {
        ensure!(!pairs.is_empty(), "no facts to fit {key} on");
        ensure!(ridge > 0.0, "the ridge penalty must be positive");
        let m = dim + 1;
        // a = Xᵀ X + ridge·n·I and b = Xᵀ (Y - X), X with a 1 appended to
        // each row: the map is the subject plus what is fitted, so a large
        // penalty leaves the subject where it is (plus the kind's shift).
        let mut a = vec![0f64; m * m];
        let mut b = vec![0f64; m * dim];
        let mut x = vec![0f64; m];
        for (subject, object) in pairs {
            ensure!(
                subject.len() == dim && object.len() == dim,
                "a vector of {key} has the wrong length"
            );
            for (to, &from) in x.iter_mut().zip(subject.iter()) {
                *to = f64::from(from);
            }
            x[dim] = 1.0;
            for i in 0..m {
                let xi = x[i];
                if xi == 0.0 {
                    continue;
                }
                let row = &mut a[i * m..(i + 1) * m];
                // Only the lower triangle; it is mirrored below.
                for (cell, &xj) in row[..=i].iter_mut().zip(&x[..=i]) {
                    *cell += xi * xj;
                }
                let row = &mut b[i * dim..(i + 1) * dim];
                for ((cell, &y), &s) in row.iter_mut().zip(object.iter()).zip(subject.iter()) {
                    *cell += xi * (f64::from(y) - f64::from(s));
                }
            }
        }
        for i in 0..m {
            for j in 0..i {
                a[j * m + i] = a[i * m + j];
            }
        }
        let penalty = ridge * pairs.len() as f64;
        // The bias is barely held back: each kind has its own region.
        for i in 0..m {
            a[i * m + i] += if i == dim { penalty * 1e-6 } else { penalty };
        }
        let weights = solve_symmetric(&mut a, m, &b, dim)
            .with_context(|| format!("fitting the map of {key}"))?;
        Ok(Relation {
            key: key.to_string(),
            trained_on: u32::try_from(pairs.len()).unwrap_or(u32::MAX),
            scale: 1.0,
            offset: 0.0,
            dim,
            weights: weights.into_iter().map(|w| w as f32).collect(),
        })
    }

    /// Values per vector.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Where `subject` (a unit vector) leads: the map applied, scaled to
    /// length 1. `None` when it leads nowhere (all zeros).
    pub fn apply(&self, subject: &[f32]) -> Option<Vec<f32>> {
        if subject.len() != self.dim {
            return None;
        }
        let mut out: Vec<f32> = self.weights[self.dim * self.dim..]
            .iter()
            .zip(subject)
            .map(|(bias, x)| bias + x)
            .collect();
        for (&x, row) in subject.iter().zip(self.weights.chunks_exact(self.dim)) {
            for (o, &w) in out.iter_mut().zip(row) {
                *o += x * w;
            }
        }
        normalized(out)
    }

    /// How close `object` is to where `subject` leads, from -1 to 1.
    pub fn closeness(&self, subject: &[f32], object: &[f32]) -> f32 {
        self.apply(subject).map_or(0.0, |to| dot(&to, object))
    }

    /// The probability that `object` is what `subject` is related to by
    /// this kind, from the logistic fit of [`Relation::calibrate`].
    pub fn plausibility(&self, subject: &[f32], object: &[f32]) -> f32 {
        self.probability(self.closeness(subject, object))
    }

    /// A closeness as a probability.
    pub fn probability(&self, closeness: f32) -> f32 {
        1.0 / (1.0 + (-(self.scale * closeness + self.offset)).exp())
    }

    /// Fits [`Relation::scale`] and [`Relation::offset`] to the closeness of
    /// true facts (`right`) and of facts with a wrong object (`wrong`), by
    /// logistic regression (Newton's method, a fixed number of steps).
    pub fn calibrate(&mut self, right: &[f32], wrong: &[f32]) {
        if right.is_empty() || wrong.is_empty() {
            return;
        }
        let samples: Vec<(f64, f64)> = right
            .iter()
            .map(|&c| (f64::from(c), 1.0))
            .chain(wrong.iter().map(|&c| (f64::from(c), 0.0)))
            .collect();
        let (mut scale, mut offset) = (1.0f64, 0.0f64);
        for _ in 0..50 {
            // Gradient and Hessian of the log loss, with a small penalty on
            // the scale so that separable samples stay finite.
            let (mut gs, mut go, mut hss, mut hso, mut hoo) = (0.01 * scale, 0.0, 0.01, 0.0, 0.0);
            for &(c, label) in &samples {
                let p = 1.0 / (1.0 + (-(scale * c + offset)).exp());
                let w = (p * (1.0 - p)).max(1e-9);
                gs += (p - label) * c;
                go += p - label;
                hss += w * c * c;
                hso += w * c;
                hoo += w;
            }
            let det = hss * hoo - hso * hso;
            if det.abs() < 1e-12 {
                break;
            }
            let ds = (hoo * gs - hso * go) / det;
            let d_o = (hss * go - hso * gs) / det;
            scale -= ds;
            offset -= d_o;
            if ds.abs() + d_o.abs() < 1e-9 {
                break;
            }
        }
        if scale.is_finite() && offset.is_finite() {
            self.scale = scale as f32;
            self.offset = offset as f32;
        }
    }
}

/// Solves `a · w = b` for `w` (`m` rows of `cols`), `a` symmetric positive
/// definite (`m` by `m`), by Cholesky decomposition in place.
fn solve_symmetric(a: &mut [f64], m: usize, b: &[f64], cols: usize) -> Result<Vec<f64>> {
    for j in 0..m {
        let mut d = a[j * m + j];
        for k in 0..j {
            d -= a[j * m + k] * a[j * m + k];
        }
        if d <= 0.0 || !d.is_finite() {
            bail!("the facts do not determine a map");
        }
        let d = d.sqrt();
        a[j * m + j] = d;
        for i in j + 1..m {
            let mut s = a[i * m + j];
            for k in 0..j {
                s -= a[i * m + k] * a[j * m + k];
            }
            a[i * m + j] = s / d;
        }
    }
    let mut w = b.to_vec();
    for c in 0..cols {
        // L y = b, then Lᵀ w = y.
        for i in 0..m {
            let mut s = w[i * cols + c];
            for k in 0..i {
                s -= a[i * m + k] * w[k * cols + c];
            }
            w[i * cols + c] = s / a[i * m + i];
        }
        for i in (0..m).rev() {
            let mut s = w[i * cols + c];
            for k in i + 1..m {
                s -= a[k * m + i] * w[k * cols + c];
            }
            w[i * cols + c] = s / a[i * m + i];
        }
    }
    Ok(w)
}

/// The relation maps of one model.
#[derive(Debug, Clone, PartialEq)]
pub struct Relations {
    model: ModelId,
    dim: usize,
    relations: Vec<Relation>,
}

impl Relations {
    /// No maps yet, for vectors of `dim` values made by `model`.
    pub fn new(model: ModelId, dim: usize) -> Self {
        Relations {
            model,
            dim,
            relations: Vec::new(),
        }
    }

    /// The model whose vectors the maps take.
    pub fn model(&self) -> ModelId {
        self.model
    }

    /// Values per vector.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Adds `relation`, replacing any of the same key.
    pub fn insert(&mut self, relation: Relation) -> Result<()> {
        ensure!(
            relation.dim == self.dim,
            "the map of {} takes vectors of another length",
            relation.key
        );
        self.relations.retain(|r| r.key != relation.key);
        self.relations.push(relation);
        Ok(())
    }

    /// The map of the kind `key`.
    pub fn get(&self, key: &str) -> Option<&Relation> {
        self.relations.iter().find(|r| r.key == key)
    }

    /// Every map, in the order added.
    pub fn all(&self) -> &[Relation] {
        &self.relations
    }

    /// Where following the kinds `keys` in turn leads from `subject`.
    pub fn follow(&self, keys: &[&str], subject: &[f32]) -> Option<Vec<f32>> {
        let mut at = subject.to_vec();
        for key in keys {
            at = self.get(key)?.apply(&at)?;
        }
        Some(at)
    }

    /// Reads a relations file.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::read(&mut bytes.as_slice()).with_context(|| format!("reading {}", path.display()))
    }

    fn read(input: &mut impl Read) -> Result<Self> {
        let mut magic = [0u8; 8];
        input.read_exact(&mut magic)?;
        ensure!(&magic == MAGIC, "not a relations file");
        let version = read_u32(input)?;
        ensure!(version == VERSION, "relations file version {version}");
        let mut model = [0u8; 32];
        input.read_exact(&mut model)?;
        let dim = read_u32(input)? as usize;
        ensure!((1..=MAX_DIM).contains(&dim), "vectors of {dim} values");
        let count = read_u32(input)? as usize;
        ensure!(count <= MAX_RELATIONS, "{count} relations");
        let mut relations = Relations::new(model, dim);
        for _ in 0..count {
            let key_len = read_u32(input)? as usize;
            ensure!(key_len <= 64, "a key of {key_len} bytes");
            let mut key = vec![0u8; key_len];
            input.read_exact(&mut key)?;
            let key = String::from_utf8(key).context("a key that is not UTF-8")?;
            let trained_on = read_u32(input)?;
            let scale = read_f32(input)?;
            let offset = read_f32(input)?;
            let mut weights = vec![0f32; (dim + 1) * dim];
            for w in &mut weights {
                *w = read_f32(input)?;
            }
            ensure!(
                weights.iter().all(|w| w.is_finite()) && scale.is_finite() && offset.is_finite(),
                "the map of {key} has values that are not numbers"
            );
            relations.insert(Relation {
                key,
                trained_on,
                scale,
                offset,
                dim,
                weights,
            })?;
        }
        Ok(relations)
    }

    /// Writes the maps to `path` (by way of a file next to it, so a reader
    /// never sees half a file).
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut bytes = Vec::new();
        self.write(&mut bytes)?;
        let part = path.with_extension("part");
        std::fs::write(&part, bytes).with_context(|| format!("writing {}", part.display()))?;
        std::fs::rename(&part, path).with_context(|| format!("writing {}", path.display()))
    }

    fn write(&self, out: &mut impl Write) -> Result<()> {
        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        out.write_all(&self.model)?;
        out.write_all(&u32::try_from(self.dim)?.to_le_bytes())?;
        out.write_all(&u32::try_from(self.relations.len())?.to_le_bytes())?;
        for relation in &self.relations {
            out.write_all(&u32::try_from(relation.key.len())?.to_le_bytes())?;
            out.write_all(relation.key.as_bytes())?;
            out.write_all(&relation.trained_on.to_le_bytes())?;
            out.write_all(&relation.scale.to_le_bytes())?;
            out.write_all(&relation.offset.to_le_bytes())?;
            for w in &relation.weights {
                out.write_all(&w.to_le_bytes())?;
            }
        }
        Ok(())
    }
}

fn read_u32(input: &mut impl Read) -> Result<u32> {
    let mut bytes = [0u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_f32(input: &mut impl Read) -> Result<f32> {
    let mut bytes = [0u8; 4];
    input.read_exact(&mut bytes)?;
    Ok(f32::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed stream of numbers in [-1, 1).
    fn noise(seed: &mut u64) -> f32 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((*seed >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }

    fn random_unit(dim: usize, seed: &mut u64) -> Vec<f32> {
        normalized((0..dim).map(|_| noise(seed)).collect()).unwrap()
    }

    /// Subjects and objects where each object is a fixed rotation-like
    /// shuffle of its subject, with a little noise.
    fn world(dim: usize, n: usize) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
        let mut seed = 7;
        let subjects: Vec<Vec<f32>> = (0..n).map(|_| random_unit(dim, &mut seed)).collect();
        let objects = subjects
            .iter()
            .map(|s| {
                let moved: Vec<f32> = (0..dim)
                    .map(|i| -s[(i * 5 + 3) % dim] + 0.05 * noise(&mut seed))
                    .collect();
                normalized(moved).unwrap()
            })
            .collect();
        (subjects, objects)
    }

    fn rank_of(right: usize, at: &[f32], candidates: &[Vec<f32>]) -> usize {
        let score = dot(at, &candidates[right]);
        candidates.iter().filter(|c| dot(at, c) > score).count()
    }

    #[test]
    fn a_fitted_map_finds_objects_of_subjects_it_never_saw() {
        let dim = 16;
        let (subjects, objects) = world(dim, 400);
        let pairs: Vec<(&[f32], &[f32])> = subjects[..300]
            .iter()
            .zip(&objects[..300])
            .map(|(s, o)| (s.as_slice(), o.as_slice()))
            .collect();
        let relation = Relation::fit("capital", dim, &pairs, 1e-4).unwrap();
        for i in 300..400 {
            let at = relation.apply(&subjects[i]).unwrap();
            assert_eq!(rank_of(i, &at, &objects), 0, "fact {i}");
            // The subject itself, unmapped, is no guide.
            assert!(dot(&subjects[i], &objects[i]).abs() < 0.8);
        }
    }

    #[test]
    fn claims_get_probabilities_and_maps_chain() {
        let dim = 16;
        let (subjects, objects) = world(dim, 300);
        let pairs: Vec<(&[f32], &[f32])> = subjects
            .iter()
            .zip(&objects)
            .map(|(s, o)| (s.as_slice(), o.as_slice()))
            .collect();
        let mut relation = Relation::fit("capital", dim, &pairs, 1e-4).unwrap();
        let right: Vec<f32> = (0..300)
            .map(|i| relation.closeness(&subjects[i], &objects[i]))
            .collect();
        let wrong: Vec<f32> = (0..300)
            .map(|i| relation.closeness(&subjects[i], &objects[(i + 1) % 300]))
            .collect();
        relation.calibrate(&right, &wrong);
        assert!(relation.plausibility(&subjects[0], &objects[0]) > 0.9);
        assert!(relation.plausibility(&subjects[0], &objects[1]) < 0.1);

        // Following the map twice is the shuffle done twice.
        let mut relations = Relations::new([1; 32], dim);
        relations.insert(relation.clone()).unwrap();
        let twice = relations
            .follow(&["capital", "capital"], &subjects[5])
            .unwrap();
        let once_more = relation.apply(&objects[5]).unwrap();
        assert!(dot(&twice, &once_more) > 0.99);
        assert!(relations
            .follow(&["capital", "spouse"], &subjects[5])
            .is_none());
    }

    #[test]
    fn relations_files_read_back() {
        let dim = 8;
        let (subjects, objects) = world(dim, 50);
        let pairs: Vec<(&[f32], &[f32])> = subjects
            .iter()
            .zip(&objects)
            .map(|(s, o)| (s.as_slice(), o.as_slice()))
            .collect();
        let mut relations = Relations::new([3; 32], dim);
        relations
            .insert(Relation::fit("founder", dim, &pairs, 0.01).unwrap())
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RELATIONS_FILE_NAME);
        relations.save(&path).unwrap();
        assert_eq!(Relations::load(&path).unwrap(), relations);
        std::fs::write(&path, b"PLUMBVEC").unwrap();
        assert!(Relations::load(&path).is_err());
    }

    #[test]
    fn a_heavily_held_back_map_leaves_subjects_nearly_where_they_are() {
        let dim = 16;
        let (subjects, objects) = world(dim, 100);
        let pairs: Vec<(&[f32], &[f32])> = subjects
            .iter()
            .zip(&objects)
            .map(|(s, o)| (s.as_slice(), o.as_slice()))
            .collect();
        let relation = Relation::fit("capital", dim, &pairs, 1e6).unwrap();
        for subject in &subjects {
            assert!(dot(&relation.apply(subject).unwrap(), subject) > 0.95);
        }
    }

    #[test]
    fn unit_vectors_from_bytes() {
        let v = unit(&[3, 4]).unwrap();
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        assert!(unit(&[0, 0]).is_none());
    }
}
