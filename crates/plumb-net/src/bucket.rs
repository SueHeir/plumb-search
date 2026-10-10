//! Buckets: how a node searches other nodes without telling them what it
//! is looking for. The keys and bucket numbers are in [`plumb_core::keys`],
//! shared with private search in the browser.
//!
//! Every name a site goes by (its domain label, homepage title, aliases and
//! top link texts) gives a few **keys**: each word, and each whole name with
//! the spaces taken out (`U.S. Bank` -> `us`, `bank`, `usbank`). Each key
//! falls in one of [`BUCKETS`] buckets by its hash. A node with an index
//! keeps, for every bucket, the best [`KEY_CAP`] sites of every key in it
//! ([`BucketTable`]).
//!
//! To search the network, a node works out the keys of its query, and asks
//! for their buckets, never the query: [`BUCKETS_PER_SEARCH`] buckets every
//! time, padded with random ones, each from a different node under a
//! throwaway identity. The node answering learns one bucket number, shared by
//! a few hundred keys and so by thousands of possible searches. The asking
//! node then keeps the sites that match its keys and ranks them itself.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
pub use plumb_core::keys::{
    bucket_of, matches, query_keys, record_keys, BUCKETS, BUCKETS_PER_SEARCH, KEY_CAP,
};
use plumb_core::SiteRecord;
use rand_core::RngCore;

use crate::hash::Hash;

/// The buckets to fetch for `query`, padded with random ones (see
/// [`plumb_core::keys::pick_buckets`]). Also returns the keys searched for.
pub fn search_buckets(query: &str) -> (Vec<u32>, Vec<String>) {
    let mut rng = rand_core::OsRng;
    plumb_core::keys::pick_buckets(query, || rng.next_u64())
}

/// Answers bucket requests from other nodes.
pub trait BucketSource: Send + Sync + 'static {
    /// The records of bucket `bucket`, as JSON, or `None` when this node
    /// has no bucket table (yet).
    fn bucket(&self, bucket: u32) -> Option<Vec<String>>;

    /// Records `from..from + count` of all the sites, best-ranked first, as
    /// JSON, and how many sites there are; `None` when this node has no
    /// bucket table (yet). For nodes filling their space (see
    /// [`crate::fill`]).
    fn ranked(&self, from: usize, count: usize) -> Option<(Vec<String>, usize)> {
        let _ = (from, count);
        None
    }

    /// The icon this node holds for `domain`, as base64 PNG, sent with the
    /// site's record to a node filling its space; `None` when it has none.
    fn icon(&self, domain: &str) -> Option<String> {
        let _ = domain;
        None
    }

    /// The file of the page set `set` (`wikipedia-en`), for nodes asking
    /// for it (see [`crate::pages`]); `None` when this node has none.
    fn page_set_file(&self, set: &str) -> Option<std::path::PathBuf> {
        let _ = set;
        None
    }

    /// Answers `peer` about a searcher's profile it shares with this node
    /// (see [`crate::proto::ProfileRequest`]). Refused unless the node keeps
    /// profiles.
    fn profile(
        &self,
        peer: libp2p::PeerId,
        request: crate::proto::ProfileRequest,
    ) -> crate::proto::ProfileResponse {
        let _ = (peer, request);
        crate::proto::ProfileResponse::Refused("this node keeps no profiles".into())
    }
}

/// A node's buckets on disk, written next to an index and never changed.
///
/// ```text
/// records.dat    the records, JSON, one after the other
/// records.idx    where each record starts: little-endian u64, one more
///                than there are records
/// buckets.idx    where each bucket's list starts in buckets.dat, counted
///                in entries: little-endian u64, BUCKETS + 1 of them
/// buckets.dat    record numbers, little-endian u32, best first per key
/// ```
#[derive(Debug)]
pub struct BucketTable {
    dir: PathBuf,
    records: usize,
}

impl BucketTable {
    /// Writes the table of `records` into `dir`, which must not exist yet.
    /// Takes about as much disk as the records file, and memory for about
    /// 12 bytes per key and 4 per bucket entry while it runs. `records` may
    /// be the records themselves or references to them.
    pub fn build<R: Borrow<SiteRecord>>(dir: &Path, records: &[R]) -> Result<BucketTable> {
        let records: Vec<&SiteRecord> = records.iter().map(Borrow::borrow).collect();
        let mut order: Vec<usize> = (0..records.len()).collect();
        order.sort_by(|&a, &b| {
            records[b]
                .link_score()
                .total_cmp(&records[a].link_score())
                .then_with(|| records[a].domain.cmp(&records[b].domain))
        });
        let mut writer = BucketWriter::new(dir)?;
        for i in order {
            writer.add(records[i])?;
        }
        writer.finish()
    }

    /// Opens a table [`BucketTable::build`] wrote.
    pub fn open(dir: &Path) -> Result<BucketTable> {
        let len = |name: &str| -> Result<u64> {
            Ok(fs::metadata(dir.join(name))
                .with_context(|| format!("reading {}", dir.join(name).display()))?
                .len())
        };
        let offsets = len("records.idx")?;
        ensure!(offsets >= 8 && offsets % 8 == 0, "records.idx is damaged");
        ensure!(
            len("buckets.idx")? == (u64::from(BUCKETS) + 1) * 8,
            "buckets.idx is damaged"
        );
        len("records.dat")?;
        len("buckets.dat")?;
        Ok(BucketTable {
            dir: dir.to_path_buf(),
            records: (offsets / 8 - 1) as usize,
        })
    }

    /// Number of sites in the table.
    pub fn len(&self) -> usize {
        self.records
    }

    pub fn is_empty(&self) -> bool {
        self.records == 0
    }

    /// The records of `bucket`, as JSON.
    pub fn get(&self, bucket: u32) -> Result<Vec<String>> {
        if bucket >= BUCKETS {
            bail!("there is no bucket {bucket}");
        }
        let mut index = File::open(self.dir.join("buckets.idx"))?;
        let range = read_u64s(&mut index, u64::from(bucket), 2)?;
        let (start, end) = (range[0], range[1]);
        ensure!(start <= end, "buckets.idx is damaged");
        let mut entries = File::open(self.dir.join("buckets.dat"))?;
        entries.seek(SeekFrom::Start(start * 4))?;
        let mut raw = vec![0u8; ((end - start) * 4) as usize];
        entries.read_exact(&mut raw)?;
        let mut offsets = File::open(self.dir.join("records.idx"))?;
        let mut data = File::open(self.dir.join("records.dat"))?;
        let mut out = Vec::with_capacity(raw.len() / 4);
        for chunk in raw.as_chunks::<4>().0 {
            let n = u32::from_le_bytes(*chunk);
            ensure!((n as usize) < self.records, "buckets.dat is damaged");
            let at = read_u64s(&mut offsets, u64::from(n), 2)?;
            ensure!(
                at[0] <= at[1] && at[1] - at[0] < 1 << 20,
                "records.idx is damaged"
            );
            data.seek(SeekFrom::Start(at[0]))?;
            let mut json = vec![0u8; (at[1] - at[0]) as usize];
            data.read_exact(&mut json)?;
            out.push(String::from_utf8(json).context("records.dat is damaged")?);
        }
        Ok(out)
    }

    /// Records `from..from + count` (fewer at the end), best-ranked first,
    /// as JSON: the table keeps them in that order.
    pub fn ranked(&self, from: usize, count: usize) -> Result<Vec<String>> {
        let end = from.saturating_add(count).min(self.records);
        if from >= end {
            return Ok(Vec::new());
        }
        let mut offsets = File::open(self.dir.join("records.idx"))?;
        let at = read_u64s(&mut offsets, from as u64, end - from + 1)?;
        let (start, stop) = (at[0], at[at.len() - 1]);
        ensure!(
            at.windows(2).all(|w| w[0] <= w[1]) && stop - start < 1 << 30,
            "records.idx is damaged"
        );
        let mut data = File::open(self.dir.join("records.dat"))?;
        data.seek(SeekFrom::Start(start))?;
        let mut raw = vec![0u8; (stop - start) as usize];
        data.read_exact(&mut raw)?;
        at.windows(2)
            .map(|w| {
                let slice = &raw[(w[0] - start) as usize..(w[1] - start) as usize];
                String::from_utf8(slice.to_vec()).context("records.dat is damaged")
            })
            .collect()
    }
}

impl BucketSource for BucketTable {
    fn bucket(&self, bucket: u32) -> Option<Vec<String>> {
        self.get(bucket).ok()
    }

    fn ranked(&self, from: usize, count: usize) -> Option<(Vec<String>, usize)> {
        self.ranked(from, count)
            .ok()
            .map(|records| (records, self.records))
    }
}

fn create(path: &Path) -> Result<File> {
    File::create(path).with_context(|| format!("creating {}", path.display()))
}

fn flush(writer: BufWriter<File>) -> Result<()> {
    let file = writer.into_inner().context("writing the buckets")?;
    file.sync_all().context("writing the buckets")
}

fn read_u64s(file: &mut File, first: u64, n: usize) -> Result<Vec<u64>> {
    file.seek(SeekFrom::Start(first * 8))?;
    let mut raw = vec![0u8; n * 8];
    file.read_exact(&mut raw)?;
    Ok(raw
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes(*c))
        .collect())
}

/// A [`BucketTable`] written a record at a time, for a caller that cannot
/// hold every record at once. The records must come best
/// [`SiteRecord::link_score`] first (ties by domain), as
/// [`BucketTable::build`] orders them: each key keeps the first
/// [`KEY_CAP`] that have it.
pub struct BucketWriter {
    dir: PathBuf,
    staging: PathBuf,
    data: BufWriter<fs::File>,
    offsets: BufWriter<fs::File>,
    /// How many records each key has, by a hash of the key, up to KEY_CAP.
    per_key: HashMap<u64, u8>,
    buckets: Vec<Vec<u32>>,
    /// Bytes of records written.
    at: u64,
    records: usize,
}

impl BucketWriter {
    /// Starts a table in `dir`, which must not exist yet. It is written in
    /// a staging directory next to it and moved there by
    /// [`BucketWriter::finish`].
    pub fn new(dir: &Path) -> Result<BucketWriter> {
        ensure!(!dir.exists(), "{} already exists", dir.display());
        let staging = dir.with_extension("staging");
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
        Ok(BucketWriter {
            dir: dir.to_path_buf(),
            data: BufWriter::new(create(&staging.join("records.dat"))?),
            offsets: BufWriter::new(create(&staging.join("records.idx"))?),
            staging,
            per_key: HashMap::new(),
            buckets: vec![Vec::new(); BUCKETS as usize],
            at: 0,
            records: 0,
        })
    }

    /// Adds the next record.
    pub fn add(&mut self, record: &SiteRecord) -> Result<()> {
        let n = u32::try_from(self.records).context("too many records for a bucket table")?;
        let json = serde_json::to_vec(record).context("encoding a record")?;
        self.offsets.write_all(&self.at.to_le_bytes())?;
        self.data.write_all(&json)?;
        self.at += json.len() as u64;
        self.records += 1;
        let mut seen_buckets = Vec::new();
        for key in record_keys(record) {
            let count = self
                .per_key
                .entry(Hash::of(&[key.as_bytes()]).prefix_u64())
                .or_insert(0);
            if usize::from(*count) >= KEY_CAP {
                continue;
            }
            *count += 1;
            let bucket = bucket_of(&key);
            if !seen_buckets.contains(&bucket) {
                seen_buckets.push(bucket);
                self.buckets[bucket as usize].push(n);
            }
        }
        Ok(())
    }

    /// Writes the bucket lists and moves the table into place.
    pub fn finish(self) -> Result<BucketTable> {
        let BucketWriter {
            dir,
            staging,
            data,
            mut offsets,
            per_key,
            buckets,
            at,
            records,
        } = self;
        offsets.write_all(&at.to_le_bytes())?;
        flush(data)?;
        flush(offsets)?;
        drop(per_key);

        let mut index = BufWriter::new(create(&staging.join("buckets.idx"))?);
        let mut entries = BufWriter::new(create(&staging.join("buckets.dat"))?);
        let mut start: u64 = 0;
        for bucket in &buckets {
            index.write_all(&start.to_le_bytes())?;
            for n in bucket {
                entries.write_all(&n.to_le_bytes())?;
            }
            start += bucket.len() as u64;
        }
        index.write_all(&start.to_le_bytes())?;
        flush(index)?;
        flush(entries)?;
        fs::rename(&staging, &dir)
            .with_context(|| format!("moving the buckets to {}", dir.display()))?;
        Ok(BucketTable { dir, records })
    }
}

#[cfg(test)]
mod tests {
    use plumb_core::LinkText;

    use super::*;

    fn site(domain: &str, title: &str, tranco: u32) -> SiteRecord {
        let mut r = SiteRecord::new(domain);
        r.title = Some(title.to_string());
        r.signals.tranco_rank = Some(tranco);
        r
    }

    #[test]
    fn keys_cover_words_and_joined_names() {
        let mut r = site("usbank.com", "U.S. Bank | Checking & Savings", 500);
        r.link_texts = vec![LinkText::with_count("US Bank online", 3)];
        let keys = record_keys(&r);
        for key in ["usbank", "bank", "us", "checking", "usbankonline", "online"] {
            assert!(keys.contains(key), "{key} in {keys:?}");
        }
        assert_eq!(query_keys("U.S. Bank"), vec!["usbank", "bank", "us"]);
        assert!(matches(&r, &query_keys("us bank")));
        assert!(!matches(&r, &query_keys("chase")));
    }

    #[test]
    fn a_search_always_asks_for_the_same_number_of_buckets() {
        for query in [
            "",
            "x",
            "us bank",
            "a very long query with many words in it",
        ] {
            let (buckets, _) = search_buckets(query);
            assert_eq!(buckets.len(), BUCKETS_PER_SEARCH, "{query:?}");
            let mut unique = buckets.clone();
            unique.dedup();
            assert!(buckets.iter().all(|&b| b < BUCKETS));
        }
        let (buckets, keys) = search_buckets("us bank");
        assert!(buckets.contains(&bucket_of("usbank")));
        assert_eq!(keys, vec!["usbank", "bank", "us"]);
    }

    #[test]
    fn a_table_returns_each_key_best_sites_and_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let mut records: Vec<SiteRecord> = (0..100)
            .map(|i| {
                site(
                    &format!("bank{i}.com"),
                    &format!("Bank number {i}"),
                    1000 + i,
                )
            })
            .collect();
        records.push(site("usbank.com", "U.S. Bank", 10));
        let path = dir.path().join("buckets");
        BucketTable::build(&path, &records).unwrap();
        let table = BucketTable::open(&path).unwrap();
        assert_eq!(table.len(), 101);

        let bank: Vec<SiteRecord> = table
            .get(bucket_of("bank"))
            .unwrap()
            .iter()
            .map(|j| serde_json::from_str(j).unwrap())
            .filter(|r: &SiteRecord| record_keys(r).contains("bank"))
            .collect();
        // Capped, best first.
        assert_eq!(bank.len(), KEY_CAP);
        assert_eq!(bank[0].domain, "usbank.com");
        assert_eq!(bank[1].domain, "bank0.com");

        let usbank = table.get(bucket_of("usbank")).unwrap();
        assert!(usbank.iter().any(|j| j.contains("\"usbank.com\"")));
        assert!(table.get(BUCKETS).is_err());
        assert!(
            BucketTable::build(&path, &records).is_err(),
            "never overwritten"
        );
    }
}
