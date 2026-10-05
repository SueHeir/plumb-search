//! Which row of a PIR table holds each piece of a bucket
//! ([`plumb_core::keys::piece_of`]).
//!
//! Whole buckets differ several times over in size, and every row of a PIR
//! table must be as big as the biggest, so a table of buckets is mostly
//! padding. Cut in [`PIECES_PER_BUCKET`] pieces by key, buckets pack into
//! rows that come out nearly equal. The map is public, the same for every
//! client of a snapshot, and committed to in its manifest, so looking up a
//! key's row in it gives nothing away.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use anyhow::{ensure, Result};
use plumb_core::keys::{PIECES, PIECES_PER_BUCKET};

use crate::bucket::BUCKETS;
use crate::hash::Hash;

/// The row of every piece, as little-endian `u16`s in piece order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceMap {
    rows: Vec<u16>,
}

impl PieceMap {
    /// Every piece in its own bucket's row: a table of whole buckets.
    pub fn by_bucket() -> PieceMap {
        PieceMap {
            rows: (0..PIECES)
                .map(|piece| (piece / PIECES_PER_BUCKET) as u16)
                .collect(),
        }
    }

    /// Packs pieces of `sizes` bytes (one per piece, in piece order) into
    /// [`BUCKETS`] rows: largest first, each into the emptiest row, so rows
    /// end up close to the average. Returns the map and the bytes each row
    /// gets.
    pub fn pack(sizes: &[u64]) -> Result<(PieceMap, Vec<u64>)> {
        ensure!(
            sizes.len() == PIECES as usize,
            "{} piece sizes, not {PIECES}",
            sizes.len()
        );
        let mut order: Vec<u32> = (0..PIECES).collect();
        order.sort_by_key(|&piece| (Reverse(sizes[piece as usize]), piece));
        // Emptiest row first; ties go to the lowest row, so the same sizes
        // always give the same map.
        let mut fill: BinaryHeap<Reverse<(u64, u16)>> =
            (0..BUCKETS).map(|row| Reverse((0, row as u16))).collect();
        let mut rows = vec![0u16; PIECES as usize];
        for piece in order {
            let Reverse((bytes, row)) = fill.pop().expect("there are rows");
            rows[piece as usize] = row;
            fill.push(Reverse((bytes + sizes[piece as usize], row)));
        }
        let mut totals = vec![0u64; BUCKETS as usize];
        for Reverse((bytes, row)) in fill {
            totals[row as usize] = bytes;
        }
        Ok((PieceMap { rows }, totals))
    }

    /// The row holding `piece`.
    pub fn row_of(&self, piece: u32) -> u32 {
        u32::from(self.rows[piece as usize])
    }

    /// The pieces of every row, in piece order within a row.
    pub fn pieces_by_row(&self) -> Vec<Vec<u32>> {
        let mut by_row = vec![Vec::new(); BUCKETS as usize];
        for (piece, &row) in self.rows.iter().enumerate() {
            by_row[row as usize].push(piece as u32);
        }
        by_row
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.rows.iter().flat_map(|row| row.to_le_bytes()).collect()
    }

    /// The map [`PieceMap::to_bytes`] wrote; fails for a damaged one.
    pub fn from_bytes(bytes: &[u8]) -> Result<PieceMap> {
        ensure!(
            bytes.len() == PIECES as usize * 2,
            "a piece map has {} bytes, not {}",
            bytes.len(),
            PIECES * 2
        );
        let rows: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|raw| u16::from_le_bytes(*raw))
            .collect();
        ensure!(
            rows.iter().all(|&row| u32::from(row) < BUCKETS),
            "a piece map names a row past the table"
        );
        Ok(PieceMap { rows })
    }

    /// What a manifest commits to.
    pub fn hash(&self) -> Hash {
        Hash::of(&[b"plumb-pir-pieces/1", &self.to_bytes()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_evens_out_rows_and_is_repeatable() {
        // Pieces from 0 to ~2,000 bytes: whole buckets would differ a lot.
        let sizes: Vec<u64> = (0..u64::from(PIECES)).map(|n| (n * 7919) % 2_001).collect();
        let (map, totals) = PieceMap::pack(&sizes).unwrap();
        let average = sizes.iter().sum::<u64>() / u64::from(BUCKETS);
        let largest = *totals.iter().max().unwrap();
        assert!(largest <= average + 2_000, "{largest} vs {average}");
        for (row, pieces) in map.pieces_by_row().iter().enumerate() {
            let bytes: u64 = pieces.iter().map(|&p| sizes[p as usize]).sum();
            assert_eq!(bytes, totals[row]);
        }
        assert_eq!(PieceMap::pack(&sizes).unwrap().0, map);
        assert_eq!(PieceMap::from_bytes(&map.to_bytes()).unwrap(), map);
    }

    #[test]
    fn whole_buckets_map_to_their_own_row() {
        let map = PieceMap::by_bucket();
        assert_eq!(map.row_of(0), 0);
        assert_eq!(map.row_of(PIECES_PER_BUCKET * 9 + 3), 9);
        assert_ne!(
            map.hash(),
            PieceMap::pack(&vec![1; PIECES as usize]).unwrap().0.hash()
        );
    }

    #[test]
    fn damaged_maps_fail() {
        assert!(PieceMap::pack(&[1, 2, 3]).is_err());
        assert!(PieceMap::from_bytes(&[0; 10]).is_err());
        let mut bytes = PieceMap::by_bucket().to_bytes();
        bytes[..2].copy_from_slice(&(BUCKETS as u16).to_le_bytes());
        assert!(PieceMap::from_bytes(&bytes).is_err());
    }
}
