//! Buckets: how a node searches other nodes without telling them what it
//! is looking for.
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

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use plumb_core::{domain_label, joined, normalize_text, SiteRecord};
use rand_core::RngCore;

use crate::hash::Hash;

/// Buckets keys are spread over.
pub const BUCKETS: u32 = 16_384;
/// Best sites kept per key, by link score.
pub const KEY_CAP: usize = 32;
/// Buckets fetched for every search, real ones padded with random ones.
pub const BUCKETS_PER_SEARCH: usize = 4;
/// Link texts, most used first, whose keys count.
const KEY_LINK_TEXTS: usize = 8;
/// Keys shorter than this are skipped: one- and two-letter words would put
/// a few huge buckets in every search.
const MIN_KEY_CHARS: usize = 2;
/// Characters that separate the parts of a homepage title, as in the index.
const TITLE_SEPARATORS: [char; 10] = ['|', '·', '•', ':', '–', '—', '»', '«', '/', '\\'];

/// The bucket of `key`. The same on every node and in every version of the
/// protocol, which is why it has its own hash rather than Rust's.
pub fn bucket_of(key: &str) -> u32 {
    (Hash::of(&[b"plumb-bucket-v1\0", key.as_bytes()]).prefix_u64() % u64::from(BUCKETS)) as u32
}

/// The keys of the names in `text`: its words and the whole text joined.
fn add_keys(keys: &mut BTreeSet<String>, text: &str) {
    let normalized = normalize_text(text);
    for word in normalized.split(' ') {
        if word.chars().count() >= MIN_KEY_CHARS {
            keys.insert(word.to_string());
        }
    }
    let whole = joined(text);
    if whole.chars().count() >= MIN_KEY_CHARS {
        keys.insert(whole);
    }
}

/// Every key a site can be found by.
pub fn record_keys(record: &SiteRecord) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let label = domain_label(&record.domain);
    add_keys(&mut keys, &label);
    for part in label.split('-') {
        add_keys(&mut keys, part);
    }
    if let Some(title) = &record.title {
        add_keys(&mut keys, title);
        for part in title.split(TITLE_SEPARATORS).flat_map(|p| p.split(" - ")) {
            add_keys(&mut keys, part);
        }
    }
    for alias in &record.aliases {
        add_keys(&mut keys, alias);
    }
    let mut texts: Vec<_> = record.link_texts.iter().collect();
    texts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.text.cmp(&b.text)));
    for lt in texts.into_iter().take(KEY_LINK_TEXTS) {
        add_keys(&mut keys, &lt.text);
    }
    keys
}

/// The keys of a query, the whole query joined first, then its words,
/// longest first (they pick out the fewest sites).
pub fn query_keys(query: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let whole = joined(query);
    if whole.chars().count() >= MIN_KEY_CHARS {
        keys.push(whole);
    }
    let normalized = normalize_text(query);
    let mut words: Vec<&str> = normalized
        .split(' ')
        .filter(|w| w.chars().count() >= MIN_KEY_CHARS)
        .collect();
    words.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    for word in words {
        if !keys.iter().any(|k| k == word) {
            keys.push(word.to_string());
        }
    }
    keys
}

/// The buckets to fetch for `query`: those of its first keys, and random
/// ones to make up [`BUCKETS_PER_SEARCH`], in random order. Also returns
/// the keys searched for.
pub fn search_buckets(query: &str) -> (Vec<u32>, Vec<String>) {
    let mut keys = query_keys(query);
    let mut buckets: Vec<u32> = Vec::new();
    let mut used = Vec::new();
    for key in keys.drain(..) {
        if buckets.len() == BUCKETS_PER_SEARCH {
            break;
        }
        let bucket = bucket_of(&key);
        if !buckets.contains(&bucket) {
            buckets.push(bucket);
        }
        used.push(key);
    }
    let mut rng = rand_core::OsRng;
    while buckets.len() < BUCKETS_PER_SEARCH {
        let bucket = (rng.next_u64() % u64::from(BUCKETS)) as u32;
        if !buckets.contains(&bucket) {
            buckets.push(bucket);
        }
    }
    for i in (1..buckets.len()).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        buckets.swap(i, j);
    }
    (buckets, used)
}

/// Whether a site can be found by any of `keys`.
pub fn matches(record: &SiteRecord, keys: &[String]) -> bool {
    let mine = record_keys(record);
    keys.iter().any(|k| mine.contains(k))
}

/// Answers bucket requests from other nodes.
pub trait BucketSource: Send + Sync + 'static {
    /// The records of bucket `bucket`, as JSON, or `None` when this node
    /// has no bucket table (yet).
    fn bucket(&self, bucket: u32) -> Option<Vec<String>>;
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
    /// 12 bytes per key and 4 per bucket entry while it runs.
    pub fn build(dir: &Path, records: &[SiteRecord]) -> Result<BucketTable> {
        ensure!(!dir.exists(), "{} already exists", dir.display());
        let staging = dir.with_extension("staging");
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;

        let mut order: Vec<usize> = (0..records.len()).collect();
        order.sort_by(|&a, &b| {
            records[b]
                .link_score()
                .total_cmp(&records[a].link_score())
                .then_with(|| records[a].domain.cmp(&records[b].domain))
        });

        let mut data = BufWriter::new(create(&staging.join("records.dat"))?);
        let mut offsets = BufWriter::new(create(&staging.join("records.idx"))?);
        let mut per_key: HashMap<u64, u8> = HashMap::new();
        let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); BUCKETS as usize];
        let mut at: u64 = 0;
        for (n, &i) in order.iter().enumerate() {
            let record = &records[i];
            let json = serde_json::to_vec(record).context("encoding a record")?;
            offsets.write_all(&at.to_le_bytes())?;
            data.write_all(&json)?;
            at += json.len() as u64;
            let mut seen_buckets = Vec::new();
            for key in record_keys(record) {
                let count = per_key
                    .entry(Hash::of(&[key.as_bytes()]).prefix_u64())
                    .or_insert(0);
                if usize::from(*count) >= KEY_CAP {
                    continue;
                }
                *count += 1;
                let bucket = bucket_of(&key);
                if !seen_buckets.contains(&bucket) {
                    seen_buckets.push(bucket);
                    buckets[bucket as usize].push(n as u32);
                }
            }
        }
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
        fs::rename(&staging, dir)
            .with_context(|| format!("moving the buckets to {}", dir.display()))?;
        Ok(BucketTable {
            dir: dir.to_path_buf(),
            records: records.len(),
        })
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
}

impl BucketSource for BucketTable {
    fn bucket(&self, bucket: u32) -> Option<Vec<String>> {
        self.get(bucket).ok()
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
