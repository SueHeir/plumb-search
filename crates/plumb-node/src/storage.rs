//! Read-only aggregate sizing for a node's existing data directory.
//! Byte counts are logical regular-file lengths, not allocated disk space.
//! Only the main records file and bucket tables are opened for analysis.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use plumb_core::{
    keys::{slim_record, BUCKETS},
    SiteRecord,
};
use serde::Serialize;

use crate::cli::StorageArgs;

const CATEGORIES: [&str; 8] = [
    "records_journals",
    "indexes",
    "buckets",
    "models_vectors",
    "icons",
    "network_cache",
    "history",
    "other",
];
const MAX_RECORD_BYTES: usize = 1 << 20;
const MAX_OMISSIONS: usize = 100;
const MAX_TABLE_REPORTS: usize = 100;
// Record-length caching stays at 8 MiB; larger tables fall back to index
// seeks. Cap metadata input too, so malformed sparse tables cannot force
// an arbitrarily long scan. The report explicitly marks that omission.
const MAX_CACHED_RECORD_LENGTHS: usize = 1 << 20;
const MAX_BUCKET_METADATA_BYTES: u64 = 1 << 30;
const PIR_LENGTH_PREFIX_BYTES: u64 = 8;

#[derive(Debug, Default, Serialize)]
pub struct Category {
    pub files: u64,
    pub logical_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct StorageReport {
    pub data: PathBuf,
    pub measurement: &'static str,
    pub complete: bool,
    pub categories: BTreeMap<&'static str, Category>,
    pub total_logical_bytes: u64,
    pub excluded_symlinks: u64,
    pub excluded_special_files: u64,
    pub records: Option<RecordSizing>,
    pub bucket_tables: Vec<BucketSizing>,
    pub omission_count: u64,
    pub omissions: Vec<Omission>,
    pub notes: [&'static str; 3],
}

#[derive(Debug, Serialize)]
pub struct Omission {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Default, Serialize)]
pub struct RecordSizing {
    pub source: PathBuf,
    pub valid_records: u64,
    pub invalid_records: u64,
    pub oversized_records: u64,
    pub full_json_payload_bytes: u64,
    pub slim_json_payload_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct Distribution {
    pub min: u64,
    pub median: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
    pub mean: f64,
}

#[derive(Debug, Serialize)]
pub struct BucketSizing {
    pub directory: PathBuf,
    pub records: u64,
    pub records_json_payload_bytes: u64,
    pub buckets: u32,
    pub empty_buckets: u64,
    pub record_memberships: u64,
    pub records_per_bucket: Distribution,
    pub object_array_payload: BucketPayloadSizing,
}

#[derive(Debug, Serialize)]
pub struct BucketPayloadSizing {
    /// A prospective layout estimate, not today's HTTP escaped-string
    /// array, proof envelope, or any selected cryptographic wire format.
    pub layout: &'static str,
    pub total_bytes: u64,
    pub bytes_per_bucket: Distribution,
    pub pir_length_prefix_bytes: u64,
    pub pir_padded_row_bytes: u64,
    pub pir_padded_database_bytes: u64,
}

pub fn run(args: StorageArgs) -> Result<()> {
    let report = inspect(&args.data)?;
    let mut out = io::stdout().lock();
    if args.json {
        serde_json::to_writer_pretty(&mut out, &report)?;
        writeln!(out)?;
    } else {
        writeln!(
            out,
            "Logical file bytes in {} (not allocated disk space)",
            args.data.display()
        )?;
        for (name, category) in &report.categories {
            writeln!(
                out,
                "{name}: {} bytes, {} files",
                category.logical_bytes, category.files
            )?;
        }
        writeln!(
            out,
            "Total: {} bytes; excluded {} symlinks and {} special files",
            report.total_logical_bytes, report.excluded_symlinks, report.excluded_special_files
        )?;
        if let Some(records) = &report.records {
            writeln!(
                out,
                "Main records: {}; JSON payload {} bytes, slim {} bytes (no journal replay)",
                records.valid_records,
                records.full_json_payload_bytes,
                records.slim_json_payload_bytes
            )?;
        }
        for table in &report.bucket_tables {
            writeln!(
                out,
                "{}: {} records, {} empty buckets; bucket records median {}, p95 {}, max {}",
                table.directory.display(),
                table.records,
                table.empty_buckets,
                table.records_per_bucket.median,
                table.records_per_bucket.p95,
                table.records_per_bucket.max
            )?;
            let payload = &table.object_array_payload;
            writeln!(out, "Prospective object-array bucket bytes: total {}, median {}, p95 {}, max {}; equal PIR rows {} bytes including {}-byte length prefix, whole database {} bytes",
                payload.total_bytes, payload.bytes_per_bucket.median, payload.bytes_per_bucket.p95,
                payload.bytes_per_bucket.max, payload.pir_padded_row_bytes,
                payload.pir_length_prefix_bytes, payload.pir_padded_database_bytes)?;
            writeln!(out, "{}", payload.layout)?;
        }
        for omission in &report.omissions {
            writeln!(
                out,
                "Omitted {}: {}",
                omission.path.display(),
                omission.reason
            )?;
        }
        for note in &report.notes {
            writeln!(out, "{note}")?;
        }
    }
    out.flush()?;
    ensure!(
        report.complete,
        "storage report is incomplete: {} omissions (see report)",
        report.omission_count
    );
    Ok(())
}

pub fn inspect(data: &Path) -> Result<StorageReport> {
    let metadata =
        fs::symlink_metadata(data).with_context(|| format!("inspecting {}", data.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "{} must be a real directory, not a symlink",
        data.display()
    );
    // Fail clearly for an unreadable root, rather than report an empty node.
    fs::read_dir(data).with_context(|| format!("listing {}", data.display()))?;
    let mut report = StorageReport {
        data: data.to_path_buf(), measurement: "logical regular-file bytes; symlinks and special files excluded",
        complete: true, categories: CATEGORIES.into_iter().map(|name| (name, Category::default())).collect(),
        total_logical_bytes: 0, excluded_symlinks: 0, excluded_special_files: 0,
        records: None, bucket_tables: Vec::new(), omission_count: 0, omissions: Vec::new(),
        notes: [
            "Logical lengths exclude filesystem allocation overhead; hard links count once per directory entry.",
            "Slim sizing uses keys::slim_record on records.jsonl only; no journal replay, compression, transport framing or authentication overhead.",
            "A running node can change during the scan; measurements are not an atomic snapshot. Only aggregate record statistics are emitted.",
        ],
    };
    // Resolve ancestors of the explicitly chosen root once. Traversal below
    // this root still excludes symlinks, including analysis-file ancestors.
    let root = fs::canonicalize(data).context("resolving the data directory")?;
    walk(&root, Path::new(""), 0, &mut report);
    Ok(report)
}

fn omit(report: &mut StorageReport, path: &Path, reason: impl Into<String>) {
    report.complete = false;
    report.omission_count += 1;
    if report.omissions.len() < MAX_OMISSIONS {
        report.omissions.push(Omission {
            path: path.to_path_buf(),
            reason: reason.into(),
        });
    }
}

fn category(path: &Path) -> &'static str {
    let parts: Vec<_> = path.iter().map(|p| p.to_string_lossy()).collect();
    let first = parts.first().map(|s| s.as_ref()).unwrap_or("");
    if parts
        .iter()
        .any(|p| p == "buckets" || p == "buckets.staging")
    {
        return "buckets";
    }
    match first {
        "records.jsonl" | "records.jsonl.journal" => "records_journals",
        "indexes" | "index" => "indexes",
        "model" | "models" | "vectors.bin" => "models_vectors",
        "icons" => "icons",
        "net" | "network" | "cache" => "network_cache",
        "history" => "history",
        _ => "other",
    }
}

fn walk(data: &Path, relative: &Path, depth: usize, report: &mut StorageReport) {
    let path = data.join(relative);
    if depth > 64 {
        omit(report, relative, "directory depth exceeds 64");
        return;
    }
    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(err) => {
            omit(report, relative, format!("cannot list directory: {err}"));
            return;
        }
    };
    let table_directory = matches!(
        relative.file_name().and_then(|p| p.to_str()),
        Some("buckets" | "buckets.staging")
    );
    let mut table_files_seen = false;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                omit(
                    report,
                    relative,
                    format!("cannot read directory entry: {err}"),
                );
                continue;
            }
        };
        let child = relative.join(entry.file_name());
        if table_directory
            && matches!(
                entry.file_name().to_str(),
                Some("records.dat" | "records.idx" | "buckets.dat" | "buckets.idx")
            )
        {
            table_files_seen = true;
        }
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(err) => {
                omit(report, &child, format!("cannot inspect entry: {err}"));
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            report.excluded_symlinks += 1;
            continue;
        }
        if metadata.is_dir() {
            walk(data, &child, depth + 1, report);
            continue;
        }
        if !metadata.is_file() {
            report.excluded_special_files += 1;
            continue;
        }
        let size = metadata.len();
        let total = report.categories.get_mut(category(&child)).unwrap();
        total.files += 1;
        total.logical_bytes += size;
        report.total_logical_bytes += size;
        if child == Path::new("records.jsonl") {
            match size_records(&entry.path(), &child) {
                Ok(sizing) => {
                    if sizing.invalid_records != 0 || sizing.oversized_records != 0 {
                        omit(
                            report,
                            &child,
                            format!(
                                "{} invalid and {} oversized records excluded from payload sizing",
                                sizing.invalid_records, sizing.oversized_records
                            ),
                        );
                    }
                    report.records = Some(sizing);
                }
                Err(err) => omit(report, &child, format!("cannot size records: {err:#}")),
            }
        }
    }
    if table_files_seen {
        if report.bucket_tables.len() >= MAX_TABLE_REPORTS {
            omit(
                report,
                relative,
                "bucket table statistics report limit (100) reached",
            );
            return;
        }
        match size_buckets(&path, relative) {
            Ok(sizing) => report.bucket_tables.push(sizing),
            Err(err) => omit(
                report,
                relative,
                format!("cannot size bucket table: {err:#}"),
            ),
        }
    }
}

fn open_regular(path: &Path) -> Result<File> {
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "not a regular file (symlinks excluded)"
    );
    #[cfg(unix)]
    let file = {
        use rustix::fs::{open, openat, Mode, OFlags};
        use std::path::Component;
        ensure!(path.is_absolute(), "analysis path must be absolute");
        let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
        let mut parent = open("/", flags | OFlags::DIRECTORY, Mode::empty())?;
        let mut components = path.components().peekable();
        let mut result = None;
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                continue;
            };
            if components.peek().is_some() {
                parent = openat(&parent, name, flags | OFlags::DIRECTORY, Mode::empty())?;
            } else {
                result = Some(File::from(openat(&parent, name, flags, Mode::empty())?));
            }
        }
        result.context("analysis path names no file")?
    };
    #[cfg(not(unix))]
    let file = File::open(path)?;
    ensure!(file.metadata()?.is_file(), "not a regular file");
    Ok(file)
}

// Consume oversized lines without accumulating them, so a corrupt record
// cannot grow memory beyond the fixed per-record limit.
fn bounded_line(reader: &mut impl BufRead, buffer: &mut Vec<u8>) -> io::Result<Option<bool>> {
    buffer.clear();
    let mut oversized = false;
    let mut seen = false;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Ok(seen.then_some(oversized));
        }
        seen = true;
        let end = chunk.iter().position(|b| *b == b'\n').map(|i| i + 1);
        let take = end.unwrap_or(chunk.len());
        if !oversized && buffer.len() + take <= MAX_RECORD_BYTES {
            buffer.extend_from_slice(&chunk[..take]);
        } else {
            oversized = true;
            buffer.clear();
        }
        reader.consume(take);
        if end.is_some() {
            return Ok(Some(oversized));
        }
    }
}

fn size_records(path: &Path, source: &Path) -> Result<RecordSizing> {
    let mut reader = BufReader::new(open_regular(path)?);
    let mut line = Vec::new();
    let mut sizing = RecordSizing {
        source: source.to_path_buf(),
        ..Default::default()
    };
    while let Some(oversized) = bounded_line(&mut reader, &mut line)? {
        if oversized {
            sizing.oversized_records += 1;
            continue;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record = match serde_json::from_slice::<SiteRecord>(&line) {
            Ok(record) => record,
            Err(_) => {
                sizing.invalid_records += 1;
                continue;
            }
        };
        sizing.valid_records += 1;
        sizing.full_json_payload_bytes += serde_json::to_vec(&record)?.len() as u64;
        sizing.slim_json_payload_bytes += serde_json::to_vec(&slim_record(record))?.len() as u64;
    }
    Ok(sizing)
}

fn read_u64(reader: &mut impl Read) -> Result<u64> {
    let mut raw = [0; 8];
    reader.read_exact(&mut raw)?;
    Ok(u64::from_le_bytes(raw))
}

fn size_buckets(path: &Path, directory: &Path) -> Result<BucketSizing> {
    let mut buckets = BufReader::new(open_regular(&path.join("buckets.idx"))?);
    let mut members = BufReader::new(open_regular(&path.join("buckets.dat"))?);
    let entries = members.get_ref().metadata()?.len();
    let mut offsets = BufReader::new(open_regular(&path.join("records.idx"))?);
    // Inspect its length only: sizing does not open or read records.dat.
    let data_metadata = fs::symlink_metadata(path.join("records.dat"))?;
    ensure!(
        data_metadata.is_file(),
        "records.dat is not a regular file (symlinks excluded)"
    );
    let payload = data_metadata.len();
    let index_bytes = offsets.get_ref().metadata()?.len();
    ensure!(
        index_bytes >= 8 && index_bytes % 8 == 0,
        "invalid records.idx length"
    );
    ensure!(entries % 4 == 0, "invalid buckets.dat length");
    ensure!(
        buckets.get_ref().metadata()?.len() == (u64::from(BUCKETS) + 1) * 8,
        "invalid buckets.idx length"
    );
    let metadata_bytes = entries
        .checked_add(index_bytes)
        .and_then(|n| n.checked_add((u64::from(BUCKETS) + 1) * 8))
        .context("bucket metadata byte count overflows")?;
    ensure!(
        metadata_bytes <= MAX_BUCKET_METADATA_BYTES,
        "bucket metadata exceeds the 1 GiB sizing input limit"
    );
    let records = index_bytes / 8 - 1;
    let mut lengths = Vec::with_capacity(records.min(MAX_CACHED_RECORD_LENGTHS as u64) as usize);
    let mut previous = read_u64(&mut offsets)?;
    ensure!(previous == 0, "records.idx does not start at zero");
    for _ in 0..records {
        let next = read_u64(&mut offsets)?;
        ensure!(
            next > previous && next <= payload,
            "invalid records.idx offsets"
        );
        if lengths.len() < MAX_CACHED_RECORD_LENGTHS {
            lengths.push(next - previous);
        }
        previous = next;
    }
    ensure!(
        previous == payload,
        "records.idx does not cover records.dat"
    );
    let mut previous = read_u64(&mut buckets)?;
    ensure!(previous == 0, "buckets.idx does not start at zero");
    let mut counts = Vec::with_capacity(BUCKETS as usize);
    let mut row_bytes = Vec::with_capacity(BUCKETS as usize);
    let mut total_row_bytes = 0u64;
    for _ in 0..BUCKETS {
        let next = read_u64(&mut buckets)?;
        ensure!(
            next >= previous && next <= entries / 4,
            "invalid buckets.idx offsets"
        );
        let count = next - previous;
        counts.push(count);
        // Raw JSON records need only brackets and separating commas. No
        // content is read, so this assumes records.dat holds valid JSON.
        let mut bytes = 2u64
            .checked_add(count.saturating_sub(1))
            .context("bucket array framing byte count overflows")?;
        for _ in 0..count {
            let mut raw = [0; 4];
            members.read_exact(&mut raw)?;
            let id = u64::from(u32::from_le_bytes(raw));
            ensure!(id < records, "bucket member record ID is out of range");
            let length = record_length(&mut offsets, &lengths, id, payload)?;
            bytes = bytes
                .checked_add(length)
                .context("bucket payload byte count overflows")?;
        }
        row_bytes.push(bytes);
        total_row_bytes = total_row_bytes
            .checked_add(bytes)
            .context("total bucket payload byte count overflows")?;
        previous = next;
    }
    ensure!(
        previous == entries / 4,
        "buckets.idx does not cover buckets.dat"
    );
    let empty_buckets = counts.iter().filter(|&&n| n == 0).count() as u64;
    let bytes_per_bucket = distribution(row_bytes, total_row_bytes);
    let pir_padded_row_bytes = bytes_per_bucket
        .max
        .checked_add(PIR_LENGTH_PREFIX_BYTES)
        .context("PIR row byte count overflows")?;
    let pir_padded_database_bytes = pir_padded_row_bytes
        .checked_mul(u64::from(BUCKETS))
        .context("padded PIR database byte count overflows")?;
    Ok(BucketSizing {
        directory: directory.to_path_buf(),
        records,
        records_json_payload_bytes: payload,
        buckets: BUCKETS,
        empty_buckets,
        record_memberships: previous,
        records_per_bucket: distribution(counts, previous),
        object_array_payload: BucketPayloadSizing {
            layout: "Prospective UTF-8 [record_json,...], assuming valid JSON records. Excludes current HTTP string escaping, proof envelopes and cryptographic overhead. PIR estimate uses one equal row size across all 16,384 buckets; no public size classes or shards.",
            total_bytes: total_row_bytes,
            bytes_per_bucket,
            pir_length_prefix_bytes: PIR_LENGTH_PREFIX_BYTES,
            pir_padded_row_bytes,
            pir_padded_database_bytes,
        },
    })
}

fn record_length(
    reader: &mut BufReader<File>,
    lengths: &[u64],
    id: u64,
    payload: u64,
) -> Result<u64> {
    if let Some(&length) = lengths.get(id as usize) {
        return Ok(length);
    }
    reader.seek(SeekFrom::Start(
        id.checked_mul(8).context("record offset overflows")?,
    ))?;
    let start = read_u64(reader)?;
    let end = read_u64(reader)?;
    ensure!(start < end && end <= payload, "invalid records.idx offsets");
    Ok(end - start)
}

fn distribution(mut values: Vec<u64>, total: u64) -> Distribution {
    values.sort_unstable();
    let percentile =
        |percent: usize| values[(values.len() * percent).div_ceil(100).saturating_sub(1)];
    Distribution {
        min: values[0],
        median: percentile(50),
        p95: percentile(95),
        p99: percentile(99),
        max: values[values.len() - 1],
        mean: total as f64 / values.len() as f64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(dir: &Path, name: &str, bytes: &[u8]) {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn synthetic_table(root: &Path) -> (PathBuf, u64, u64) {
        let table = fs::canonicalize(root)
            .unwrap()
            .join("indexes/000001/buckets");
        fs::create_dir_all(&table).unwrap();
        let first = br#"{"domain":"a.com"}"#;
        let second = br#"{"domain":"b.com","title":"a \"quoted\" title"}"#;
        let first_len = first.len() as u64;
        let second_len = second.len() as u64;
        fs::write(
            table.join("records.dat"),
            [first.as_slice(), second.as_slice()].concat(),
        )
        .unwrap();
        fs::write(
            table.join("records.idx"),
            [0u64, first_len, first_len + second_len]
                .into_iter()
                .flat_map(u64::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        fs::write(
            table.join("buckets.dat"),
            [0u32, 1, 1]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let mut offsets = vec![0u64, 2];
        offsets.resize(BUCKETS as usize + 1, 3);
        fs::write(
            table.join("buckets.idx"),
            offsets
                .into_iter()
                .flat_map(u64::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        (table, first_len, second_len)
    }

    #[test]
    fn measures_categories_without_reading_private_contents() {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            "indexes/000001/meta.json",
            "indexes/000001/buckets/unrelated",
            "model/model.bin",
            "vectors.bin",
            "icons/aa/example.png",
            "net/identity.key",
            "cache/fetched",
            "history/person.json",
            "seed/download.gz",
            "records.jsonl.journal",
        ] {
            put(dir.path(), path, b"secret");
        }
        let mut record = SiteRecord::new("example.com");
        record.crawled_at = Some(123);
        let bytes = serde_json::to_vec(&record).unwrap();
        put(dir.path(), "records.jsonl", &bytes);
        let report = inspect(dir.path()).unwrap();
        assert!(report.complete);
        assert_eq!(report.categories["indexes"].logical_bytes, 6);
        assert_eq!(report.categories["buckets"].logical_bytes, 6);
        assert_eq!(report.categories["models_vectors"].logical_bytes, 12);
        assert_eq!(report.categories["network_cache"].logical_bytes, 12);
        assert_eq!(report.total_logical_bytes, 60 + bytes.len() as u64);
        let sizing = report.records.as_ref().unwrap();
        assert_eq!(sizing.valid_records, 1);
        assert!(sizing.slim_json_payload_bytes < sizing.full_json_payload_bytes);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("example.com"));
    }

    #[test]
    fn bucket_statistics_match_real_table() {
        let dir = tempfile::tempdir().unwrap();
        let table = dir.path().join("indexes/000001/buckets");
        let records = [SiteRecord::new("example.com"), SiteRecord::new("other.com")];
        plumb_net::BucketTable::build(&table, &records).unwrap();
        let report = inspect(dir.path()).unwrap();
        assert!(report.complete);
        let sizing = &report.bucket_tables[0];
        assert_eq!(sizing.records, 2);
        assert!(sizing.record_memberships >= 2);
        assert!(sizing.empty_buckets > 0);
        assert_eq!(sizing.records_per_bucket.min, 0);
        assert!(sizing.records_per_bucket.max > 0);
        assert_eq!(report.categories["indexes"].logical_bytes, 0);
        assert_eq!(sizing.object_array_payload.bytes_per_bucket.min, 2);
        assert_eq!(
            sizing.object_array_payload.pir_padded_database_bytes,
            sizing.object_array_payload.pir_padded_row_bytes * u64::from(BUCKETS)
        );
    }

    #[test]
    fn object_array_rows_include_duplicates_commas_empty_rows_and_length_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let (path, first_len, second_len) = synthetic_table(dir.path());
        let sizing = size_buckets(&path, Path::new("indexes/000001/buckets")).unwrap();
        let payload = sizing.object_array_payload;
        // One [first,second] row, one [second] row and 16,382 [] rows.
        assert_eq!(
            payload.total_bytes,
            u64::from(BUCKETS) * 2 + first_len + second_len * 2 + 1
        );
        assert_eq!(payload.bytes_per_bucket.min, 2);
        assert_eq!(payload.bytes_per_bucket.median, 2);
        assert_eq!(payload.bytes_per_bucket.max, first_len + second_len + 3);
        assert_eq!(payload.pir_padded_row_bytes, first_len + second_len + 3 + 8);
        assert_eq!(
            payload.pir_padded_database_bytes,
            payload.pir_padded_row_bytes * u64::from(BUCKETS)
        );
        assert_eq!(sizing.record_memberships, 3);
        assert!(payload.layout.contains("current HTTP string escaping"));
    }

    #[test]
    fn out_of_range_bucket_members_and_zero_length_records_are_omissions() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _, _) = synthetic_table(dir.path());
        fs::write(
            path.join("buckets.dat"),
            [0u32, 2, 1]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let report = inspect(dir.path()).unwrap();
        assert!(!report.complete);
        assert!(report.bucket_tables.is_empty());
        assert!(report.omissions[0]
            .reason
            .contains("record ID is out of range"));
        let (path, first, second) = synthetic_table(dir.path());
        fs::write(
            path.join("records.idx"),
            [0u64, 0, first + second]
                .into_iter()
                .flat_map(u64::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let report = inspect(dir.path()).unwrap();
        assert!(!report.complete);
        assert!(report.omissions[0]
            .reason
            .contains("invalid records.idx offsets"));
    }

    #[test]
    fn uncached_record_lengths_use_validated_index_seeks() {
        let dir = tempfile::tempdir().unwrap();
        let (path, first, second) = synthetic_table(dir.path());
        let mut offsets = BufReader::new(open_regular(&path.join("records.idx")).unwrap());
        assert_eq!(
            record_length(&mut offsets, &[], 1, first + second).unwrap(),
            second
        );
        assert_eq!(
            record_length(&mut offsets, &[first], 0, first + second).unwrap(),
            first
        );
        assert!(record_length(&mut offsets, &[], 2, first + second).is_err());
    }

    #[test]
    fn sparse_metadata_exceeding_input_limit_is_reported_without_scanning() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _, _) = synthetic_table(dir.path());
        File::options()
            .write(true)
            .open(path.join("records.idx"))
            .unwrap()
            .set_len(MAX_BUCKET_METADATA_BYTES + 8)
            .unwrap();
        let report = inspect(dir.path()).unwrap();
        assert!(!report.complete);
        assert!(report.bucket_tables.is_empty());
        assert!(report.omissions[0]
            .reason
            .contains("1 GiB sizing input limit"));
    }

    #[cfg(unix)]
    #[test]
    fn bucket_payload_sizing_does_not_open_record_contents() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (path, _, _) = synthetic_table(dir.path());
        let records = path.join("records.dat");
        fs::set_permissions(&records, fs::Permissions::from_mode(0o0)).unwrap();
        let report = inspect(dir.path()).unwrap();
        assert!(report.complete);
        assert_eq!(report.bucket_tables.len(), 1);
        fs::set_permissions(records, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn missing_root_and_wrong_root_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(inspect(&dir.path().join("missing")).is_err());
        put(dir.path(), "file", b"x");
        assert!(inspect(&dir.path().join("file")).is_err());
    }

    #[test]
    fn corrupt_and_oversized_records_report_incomplete_with_bounded_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![b'x'; MAX_RECORD_BYTES + 20];
        bytes.extend_from_slice(b"\nnot json\n{\"domain\":\"example.com\"}\n");
        put(dir.path(), "records.jsonl", &bytes);
        let report = inspect(dir.path()).unwrap();
        assert!(!report.complete);
        let sizing = report.records.unwrap();
        assert_eq!(sizing.oversized_records, 1);
        assert_eq!(sizing.invalid_records, 1);
        assert_eq!(sizing.valid_records, 1);
        assert_eq!(report.omission_count, 1);
    }

    #[test]
    fn damaged_or_missing_bucket_files_report_omissions() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "indexes/000001/buckets/buckets.idx", b"bad");
        let report = inspect(dir.path()).unwrap();
        assert!(!report.complete);
        assert_eq!(report.omission_count, 1);
        assert!(report.bucket_tables.is_empty());
        assert_eq!(report.categories["buckets"].logical_bytes, 3);
        fs::remove_file(dir.path().join("indexes/000001/buckets/buckets.idx")).unwrap();
        put(dir.path(), "indexes/000001/buckets/records.dat", b"{}");
        assert!(
            !inspect(dir.path()).unwrap().complete,
            "missing bucket index must be reported too"
        );
    }

    #[test]
    fn damaged_offsets_are_reported_without_attempting_large_allocations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("indexes/000001/buckets");
        plumb_net::BucketTable::build(&path, &[SiteRecord::new("example.com")]).unwrap();
        let mut offsets = fs::read(path.join("buckets.idx")).unwrap();
        offsets[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        fs::write(path.join("buckets.idx"), offsets).unwrap();
        let report = inspect(dir.path()).unwrap();
        assert!(!report.complete);
        assert!(report.bucket_tables.is_empty());
        assert!(report.omissions[0]
            .reason
            .contains("invalid buckets.idx offsets"));
    }

    #[cfg(unix)]
    #[test]
    fn excludes_file_directory_and_root_symlinks_and_rejects_symlink_table_files() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        put(outside.path(), "private", b"should not be read");
        symlink(
            outside.path().join("private"),
            dir.path().join("records.jsonl"),
        )
        .unwrap();
        symlink(outside.path(), dir.path().join("net")).unwrap();
        let report = inspect(dir.path()).unwrap();
        assert!(report.complete);
        assert_eq!(report.excluded_symlinks, 2);
        assert_eq!(report.total_logical_bytes, 0);
        assert!(report.records.is_none());
        assert!(open_regular(&dir.path().join("records.jsonl")).is_err());
        let alias = outside.path().join("alias");
        symlink(dir.path(), &alias).unwrap();
        assert!(inspect(&alias).is_err());
        put(dir.path(), "ordinary", b"x");
        assert!(
            open_regular(&alias.join("ordinary")).is_err(),
            "analysis reads must not follow ancestor symlinks"
        );
        put(dir.path(), "indexes/000001/buckets/buckets.idx", &[0; 8]);
        symlink(
            outside.path().join("private"),
            dir.path().join("indexes/000001/buckets/records.dat"),
        )
        .unwrap();
        assert!(!inspect(dir.path()).unwrap().complete);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_content_is_an_omission_when_permissions_are_enforced() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), "records.jsonl", b"{}");
        let path = dir.path().join("records.jsonl");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
        // Root can bypass permission bits; still exercise the check without
        // claiming a permission failure that the OS does not enforce.
        if File::open(&path).is_err() {
            let report = inspect(dir.path()).unwrap();
            assert!(!report.complete);
            assert_eq!(report.categories["records_journals"].logical_bytes, 2);
            assert!(report.records.is_none());
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}
