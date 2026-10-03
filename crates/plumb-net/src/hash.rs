//! SHA-256 hashes and the Merkle tree over a crawl batch's records.
//!
//! A batch's root commits to every record in it, so one record can be
//! proven to belong to a signed batch with a handful of hashes
//! ([`MerkleProof`]) instead of the whole batch. Leaves and inner nodes are
//! hashed with different prefixes (as in RFC 6962), so an inner node can
//! never pass for a leaf. A level with an odd number of nodes moves its last
//! node up unchanged.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// A SHA-256 hash, written as 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    /// SHA-256 of the concatenated `parts`.
    pub fn of(parts: &[&[u8]]) -> Hash {
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update(part);
        }
        Hash(hasher.finalize().into())
    }

    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    /// The first 8 bytes as a number, for picking things at random from a
    /// hash.
    pub fn prefix_u64(&self) -> u64 {
        u64::from_be_bytes(self.0[..8].try_into().expect("8 bytes"))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", &self.to_hex()[..12])
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for Hash {
    type Err = String;

    fn from_str(s: &str) -> Result<Hash, String> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err(format!(
                "a hash is 64 hex digits, got {} characters",
                s.len()
            ));
        }
        let digit = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(format!("{:?} is not a lowercase hex digit", c as char)),
        };
        let mut out = [0u8; 32];
        for (i, pair) in bytes.chunks(2).enumerate() {
            out[i] = digit(pair[0])? << 4 | digit(pair[1])?;
        }
        Ok(Hash(out))
    }
}

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Hash, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// The hash of one record's bytes as a leaf of the tree.
pub fn leaf_hash(bytes: &[u8]) -> Hash {
    Hash::of(&[&[0x00], bytes])
}

fn node_hash(left: &Hash, right: &Hash) -> Hash {
    Hash::of(&[&[0x01], &left.0, &right.0])
}

/// The root of the tree over `leaves`; the hash of nothing for no leaves.
pub fn merkle_root(leaves: &[Hash]) -> Hash {
    if leaves.is_empty() {
        return Hash::of(&[]);
    }
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        level = next_level(&level);
    }
    level[0]
}

fn next_level(level: &[Hash]) -> Vec<Hash> {
    level
        .chunks(2)
        .map(|pair| match pair {
            [left, right] => node_hash(left, right),
            [last] => *last,
            _ => unreachable!("chunks of 2"),
        })
        .collect()
}

/// The sibling hashes from a leaf up to the root, which together with the
/// leaf, its position and the number of leaves give back the root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerkleProof {
    /// The leaf's position, from 0.
    pub index: u32,
    /// Siblings from the bottom level up; levels where the node had none
    /// (the odd one out, moved up unchanged) are left out.
    pub siblings: Vec<Hash>,
}

impl MerkleProof {
    /// The proof for the leaf at `index`. Panics when there is no such leaf.
    pub fn new(leaves: &[Hash], index: usize) -> MerkleProof {
        assert!(index < leaves.len(), "no leaf {index} of {}", leaves.len());
        let mut siblings = Vec::new();
        let mut level = leaves.to_vec();
        let mut i = index;
        while level.len() > 1 {
            let sibling = i ^ 1;
            if sibling < level.len() {
                siblings.push(level[sibling]);
            }
            level = next_level(&level);
            i /= 2;
        }
        MerkleProof {
            index: u32::try_from(index).expect("batches hold far fewer than 2^32 records"),
            siblings,
        }
    }

    /// Whether `leaf` is leaf number [`MerkleProof::index`] of a tree of
    /// `count` leaves with root `root`.
    pub fn verify(&self, leaf: Hash, count: u32, root: &Hash) -> bool {
        if self.index >= count {
            return false;
        }
        let mut hash = leaf;
        let mut i = self.index as usize;
        let mut width = count as usize;
        let mut siblings = self.siblings.iter();
        while width > 1 {
            let sibling = i ^ 1;
            if sibling < width {
                let Some(other) = siblings.next() else {
                    return false;
                };
                hash = if i.is_multiple_of(2) {
                    node_hash(&hash, other)
                } else {
                    node_hash(other, &hash)
                };
            }
            i /= 2;
            width = width.div_ceil(2);
        }
        siblings.next().is_none() && hash == *root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: usize) -> Vec<Hash> {
        (0..n)
            .map(|i| leaf_hash(format!("record {i}").as_bytes()))
            .collect()
    }

    #[test]
    fn every_leaf_proves_against_the_root_for_every_tree_size() {
        for n in 1..=33 {
            let leaves = leaves(n);
            let root = merkle_root(&leaves);
            for (i, leaf) in leaves.iter().enumerate() {
                let proof = MerkleProof::new(&leaves, i);
                assert!(proof.verify(*leaf, n as u32, &root), "leaf {i} of {n}");
            }
        }
    }

    #[test]
    fn a_proof_fails_for_another_leaf_position_count_or_root() {
        let leaves = leaves(7);
        let root = merkle_root(&leaves);
        let proof = MerkleProof::new(&leaves, 3);
        assert!(!proof.verify(leaves[4], 7, &root));
        assert!(!proof.verify(leaves[3], 3, &root));
        assert!(!proof.verify(leaves[3], 7, &merkle_root(&leaves[..6])));
        // The count is not checked by the hashes alone where the tree has the
        // same shape (leaf 3 of 7 or of 8); it comes from the signed header.
        let moved = MerkleProof {
            index: 2,
            ..proof.clone()
        };
        assert!(!moved.verify(leaves[3], 7, &root));
        let mut padded = proof;
        padded.siblings.push(root);
        assert!(!padded.verify(leaves[3], 7, &root));
    }

    #[test]
    fn a_leaf_cannot_pass_for_an_inner_node() {
        let leaves = leaves(2);
        let inner = node_hash(&leaves[0], &leaves[1]);
        assert_ne!(
            merkle_root(&leaves),
            leaf_hash(&[&[0x01][..], &leaves[0].0, &leaves[1].0].concat())
        );
        assert_eq!(merkle_root(&leaves), inner);
    }

    #[test]
    fn hashes_read_back_from_hex() {
        let hash = leaf_hash(b"x");
        assert_eq!(hash.to_hex().parse::<Hash>().unwrap(), hash);
        assert!("abc".parse::<Hash>().is_err());
        assert!("G".repeat(64).parse::<Hash>().is_err());
        let json = serde_json::to_string(&hash).unwrap();
        assert_eq!(serde_json::from_str::<Hash>(&json).unwrap(), hash);
    }
}
