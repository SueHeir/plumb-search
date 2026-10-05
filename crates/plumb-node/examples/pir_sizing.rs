//! How big a PIR table made from a node's real buckets would be, for a few
//! ways of cutting records down. Read-only; prints totals and
//! distributions, never a record or a bucket number.
//!
//! ```text
//! cargo run --release -p plumb-node --example pir_sizing -- DATA/indexes/ID/buckets
//! ```
//!
//! Given a records file (`records.jsonl`) instead of a buckets directory,
//! it builds the buckets in a temporary directory first.
//!
//! With `--probe-db VARIANT FILE` it also writes the buckets of that
//! variant, compressed, as the fixed rows `tools/pir-probe --database`
//! reads (16,384 rows of 32 KiB), so the probe can time Spiral on real
//! rows. A bucket too large for one row fails it; nothing is cut off.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{bail, ensure, Context, Result};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use plumb_core::keys::slim_record;
use plumb_core::SiteRecord;
use plumb_net::pir::snapshot::Layout;
use plumb_net::BucketTable;
use serde_json::json;

/// Rows the probe's `upstream-16k` profile takes, and their size.
const PROBE_ROWS: u32 = 16_384;
const PROBE_ROW_BYTES: usize = 32_768;
const PROBE_PREFIX: usize = 8;

/// Characters of a description the lean variants keep.
const LEAN_DESCRIPTION_CHARS: usize = 200;

const ROW_SIZES: [u32; 5] = [4_096, 8_192, 16_384, 32_768, 65_536];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// Records as the node stores them.
    Full,
    /// [`slim_record`]: what a browser downloads for private search.
    Slim,
    /// Slim without the homepage text, headings, Wikipedia intro and key
    /// pages, and a description cut to [`LEAN_DESCRIPTION_CHARS`].
    Lean,
    /// Lean without description, about and intro: names, links, signals.
    Names,
}

const VARIANTS: [(Variant, &str); 4] = [
    (Variant::Full, "full"),
    (Variant::Slim, "slim"),
    (Variant::Lean, "lean"),
    (Variant::Names, "names"),
];

impl Variant {
    fn apply(self, record: SiteRecord) -> SiteRecord {
        if self == Variant::Full {
            return record;
        }
        let mut record = slim_record(record);
        if self == Variant::Slim {
            return record;
        }
        record.body_text = None;
        record.headings.clear();
        record.intro = None;
        record.key_pages.clear();
        record.description = record
            .description
            .map(|d| d.chars().take(LEAN_DESCRIPTION_CHARS).collect());
        if self == Variant::Names {
            record.description = None;
            record.about = None;
        }
        record
    }
}

fn deflate(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(9));
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

fn distribution(mut values: Vec<u64>) -> serde_json::Value {
    values.sort_unstable();
    let at = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
    json!({
        "total": values.iter().sum::<u64>(),
        "median": at(0.5),
        "p99": at(0.99),
        "max": at(1.0),
    })
}

fn layouts(largest: u64) -> Vec<serde_json::Value> {
    ROW_SIZES
        .iter()
        .filter_map(|&row_bytes| {
            let layout = Layout::fitting(row_bytes, largest as usize)?;
            Some(json!({
                "row_bytes": row_bytes,
                "pages_per_bucket": layout.pages_per_bucket,
                "table_bytes": layout.rows() * u64::from(row_bytes),
            }))
        })
        .collect()
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(
        args.next()
            .context("usage: pir_sizing BUCKETS_DIR [--probe-db VARIANT FILE]")?,
    );
    let probe = match args.next().as_deref() {
        None => None,
        Some("--probe-db") => {
            let name = args.next().context("--probe-db needs a variant")?;
            let Some(&(variant, _)) = VARIANTS.iter().find(|(_, n)| *n == name) else {
                bail!("unknown variant {name}");
            };
            let path = PathBuf::from(args.next().context("--probe-db needs a file")?);
            ensure!(!path.exists(), "{} already exists", path.display());
            Some((variant, path))
        }
        Some(other) => bail!("unknown argument {other}"),
    };

    let scratch = tempfile::tempdir()?;
    let table = if dir.is_file() {
        let records: Vec<SiteRecord> = plumb_core::read_jsonl(&dir)?;
        BucketTable::build(&scratch.path().join("buckets"), &records)?
    } else {
        BucketTable::open(&dir)?
    };
    let buckets = plumb_core::keys::BUCKETS;
    ensure!(
        buckets == PROBE_ROWS,
        "the probe profile expects {PROBE_ROWS} buckets"
    );
    let mut plain: Vec<Vec<u64>> = vec![Vec::with_capacity(buckets as usize); VARIANTS.len()];
    let mut packed: Vec<Vec<u64>> = vec![Vec::with_capacity(buckets as usize); VARIANTS.len()];
    let mut out = match &probe {
        Some((_, path)) => Some(BufWriter::new(File::create(path)?)),
        None => None,
    };
    let mut memberships = 0u64;
    for bucket in 0..buckets {
        let records: Vec<SiteRecord> = table
            .get(bucket)?
            .iter()
            .map(|json| serde_json::from_str(json))
            .collect::<Result<_, _>>()
            .context("records.dat holds a record that does not parse")?;
        memberships += records.len() as u64;
        for (i, &(variant, _)) in VARIANTS.iter().enumerate() {
            let cut: Vec<SiteRecord> = records.iter().cloned().map(|r| variant.apply(r)).collect();
            let bytes = serde_json::to_vec(&cut)?;
            let small = deflate(&bytes)?;
            plain[i].push(bytes.len() as u64);
            packed[i].push(small.len() as u64);
            if let (Some(out), Some((wanted, _))) = (out.as_mut(), &probe) {
                if *wanted == variant {
                    ensure!(
                        small.len() <= PROBE_ROW_BYTES - PROBE_PREFIX,
                        "a {} bucket is {} bytes compressed, over one {PROBE_ROW_BYTES}-byte row",
                        VARIANTS[i].1,
                        small.len()
                    );
                    let mut row = vec![0u8; PROBE_ROW_BYTES];
                    row[..PROBE_PREFIX].copy_from_slice(&(small.len() as u64).to_le_bytes());
                    row[PROBE_PREFIX..PROBE_PREFIX + small.len()].copy_from_slice(&small);
                    out.write_all(&row)?;
                }
            }
        }
    }
    if let Some(mut out) = out {
        out.flush()?;
    }

    let variants: Vec<serde_json::Value> = VARIANTS
        .iter()
        .enumerate()
        .map(|(i, &(_, name))| {
            let largest_plain = *plain[i].iter().max().unwrap_or(&0);
            let largest_packed = *packed[i].iter().max().unwrap_or(&0);
            json!({
                "variant": name,
                "json": distribution(plain[i].clone()),
                "deflated": distribution(packed[i].clone()),
                "layouts_json": layouts(largest_plain),
                "layouts_deflated": layouts(largest_packed),
            })
        })
        .collect();
    let report = json!({
        "records": table.len(),
        "buckets": buckets,
        "memberships": memberships,
        "note": "Bucket payloads without crawl proofs. Layouts are the snapshot's equal pages per bucket (row header and Merkle path included).",
        "variants": variants,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
