//! The Tranco list (<https://tranco-list.eu/>): a research ranking of the top
//! one million sites, averaged over 30 days from several sources.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::registrable_domain;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::{open_maybe_gz, read_up_to, snippet, LineReader};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrancoEntry {
    /// 1-based rank.
    pub rank: u32,
    /// Registrable domain, normalized with [`plumb_core::registrable_domain`].
    pub domain: String,
}

/// Reads a Tranco list of `rank,domain` lines (no header in the official
/// file, but a header line is tolerated and skipped).
///
/// Accepts the `.zip` Tranco serves (reads the first `.csv` inside), a plain
/// `.csv`, or a gzipped `.csv.gz`. Domains are normalized to registrable
/// domains; rows without one are skipped, and when two rows normalize to the
/// same domain only the better rank is kept. Stops after `limit` entries.
///
/// The format is detected from the first bytes of the file, not its name.
/// Malformed rows are skipped with a warning, but a file with no usable row
/// at all is an error. Entries come back sorted by rank.
pub fn load_tranco(path: &Path, limit: Option<usize>) -> Result<Vec<TrancoEntry>> {
    let parsed = if is_zip(path)? {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut archive = zip::ZipArchive::new(BufReader::new(file))
            .with_context(|| format!("reading zip archive {}", path.display()))?;
        let index = (0..archive.len())
            .find(|&i| archive.name_for_index(i).is_some_and(is_csv_entry))
            .with_context(|| format!("no .csv file inside {}", path.display()))?;
        let entry = archive
            .by_index(index)
            .with_context(|| format!("opening entry {index} of {}", path.display()))?;
        debug!("reading {} from {}", entry.name(), path.display());
        read_tranco(BufReader::new(entry), limit)
    } else {
        read_tranco(open_maybe_gz(path)?, limit)
    }
    .with_context(|| format!("reading Tranco list {}", path.display()))?;

    if let Some((line_no, line)) = &parsed.first_bad {
        warn!(
            "{}: skipped {} malformed rows (first at line {line_no}: {line})",
            path.display(),
            parsed.bad_rows,
        );
    }
    info!(
        "loaded {} Tranco entries from {} ({} rows without a registrable domain, {} duplicates)",
        parsed.entries.len(),
        path.display(),
        parsed.no_domain,
        parsed.duplicates,
    );
    Ok(parsed.entries)
}

/// What [`read_tranco`] found.
#[derive(Debug, Default)]
struct Parsed {
    entries: Vec<TrancoEntry>,
    bad_rows: u64,
    first_bad: Option<(u64, String)>,
    no_domain: u64,
    duplicates: u64,
}

fn read_tranco(reader: impl BufRead, limit: Option<usize>) -> Result<Parsed> {
    let mut lines = LineReader::new(reader);
    let mut parsed = Parsed::default();
    let mut index_of: HashMap<String, usize> = HashMap::new();
    let mut seen_data = false;
    while limit.is_none_or(|n| parsed.entries.len() < n) {
        let Some((line_no, line)) = lines.next_line()? else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let first_data_line = !seen_data;
        seen_data = true;
        let Some((rank, raw_domain)) = parse_row(&line) else {
            // The first line may be a header such as `rank,domain`.
            if !first_data_line {
                parsed.bad_rows += 1;
                parsed
                    .first_bad
                    .get_or_insert_with(|| (line_no, snippet(&line)));
            }
            continue;
        };
        let Some(domain) = registrable_domain(raw_domain) else {
            parsed.no_domain += 1;
            continue;
        };
        match index_of.get(&domain) {
            Some(&i) => {
                parsed.duplicates += 1;
                let entry = &mut parsed.entries[i];
                entry.rank = entry.rank.min(rank);
            }
            None => {
                index_of.insert(domain.clone(), parsed.entries.len());
                parsed.entries.push(TrancoEntry { rank, domain });
            }
        }
    }
    if parsed.entries.is_empty() && parsed.bad_rows > 0 {
        let (line_no, line) = parsed.first_bad.unwrap_or_default();
        bail!(
            "no `rank,domain` rows found ({} malformed lines, the first at line {line_no}: {line})",
            parsed.bad_rows
        );
    }
    parsed.entries.sort_by_key(|e| e.rank);
    Ok(parsed)
}

/// Splits a `rank,domain` row. The rank must be a positive integer; extra
/// columns are ignored.
fn parse_row(line: &str) -> Option<(u32, &str)> {
    let mut fields = line.split(',').map(|f| f.trim().trim_matches('"').trim());
    let rank = fields.next()?.parse::<u32>().ok().filter(|&r| r > 0)?;
    let domain = fields.next().filter(|d| !d.is_empty())?;
    Some((rank, domain))
}

/// True when the file starts like a zip archive (`PK\x03\x04`, or
/// `PK\x05\x06` for an empty one).
fn is_zip(path: &Path) -> Result<bool> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut magic = [0u8; 4];
    let n =
        read_up_to(&mut file, &mut magic).with_context(|| format!("reading {}", path.display()))?;
    Ok(n == 4 && (&magic == b"PK\x03\x04" || &magic == b"PK\x05\x06"))
}

/// A `.csv` file in the archive, ignoring macOS resource forks.
fn is_csv_entry(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".csv") && !name.starts_with("__MACOSX/")
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const LIST: &str = "1,google.com\r\n2,www.Facebook.com\r\n3,usbank.com\r\n4,co.uk\r\n5,news.bbc.co.uk\r\n6,FACEBOOK.com\r\n";

    fn entry(rank: u32, domain: &str) -> TrancoEntry {
        TrancoEntry {
            rank,
            domain: domain.to_string(),
        }
    }

    fn expected() -> Vec<TrancoEntry> {
        vec![
            entry(1, "google.com"),
            entry(2, "facebook.com"),
            entry(3, "usbank.com"),
            entry(5, "bbc.co.uk"),
        ]
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn zip_with(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn reads_plain_csv() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top-1m.csv");
        std::fs::write(&path, LIST).unwrap();
        assert_eq!(load_tranco(&path, None).unwrap(), expected());
    }

    #[test]
    fn reads_gzipped_csv() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("top-1m.csv.gz");
        std::fs::write(&path, gzip(LIST.as_bytes())).unwrap();
        assert_eq!(load_tranco(&path, None).unwrap(), expected());
    }

    #[test]
    fn reads_first_csv_in_zip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tranco.zip");
        let bytes = zip_with(&[
            ("README.txt", "not a list"),
            ("__MACOSX/._top-1m.csv", "junk"),
            ("top-1m.csv", LIST),
            ("other.csv", "1,example.com\n"),
        ]);
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(load_tranco(&path, None).unwrap(), expected());
    }

    #[test]
    fn detects_format_by_magic_bytes_not_extension() {
        let dir = tempfile::tempdir().unwrap();
        let zip_named_csv = dir.path().join("list.csv");
        std::fs::write(&zip_named_csv, zip_with(&[("top-1m.csv", LIST)])).unwrap();
        assert_eq!(load_tranco(&zip_named_csv, None).unwrap(), expected());

        let gz_named_zip = dir.path().join("list.zip");
        std::fs::write(&gz_named_zip, gzip(LIST.as_bytes())).unwrap();
        assert_eq!(load_tranco(&gz_named_zip, None).unwrap(), expected());

        let plain_named_zip = dir.path().join("plain.csv.zip");
        std::fs::write(&plain_named_zip, LIST).unwrap();
        assert_eq!(load_tranco(&plain_named_zip, None).unwrap(), expected());
    }

    #[test]
    fn zip_without_csv_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tranco.zip");
        std::fs::write(&path, zip_with(&[("README.txt", "1,example.com\n")])).unwrap();
        let err = format!("{:#}", load_tranco(&path, None).unwrap_err());
        assert!(err.contains("no .csv file"), "{err}");

        let empty = dir.path().join("empty.zip");
        std::fs::write(&empty, zip_with(&[])).unwrap();
        let err = format!("{:#}", load_tranco(&empty, None).unwrap_err());
        assert!(err.contains("no .csv file"), "{err}");
    }

    #[test]
    fn skips_header_and_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("with-header.csv");
        std::fs::write(&path, format!("rank,domain\n{LIST}")).unwrap();
        assert_eq!(load_tranco(&path, None).unwrap(), expected());
        // The limit counts entries kept, not lines read.
        assert_eq!(load_tranco(&path, Some(3)).unwrap(), expected()[..3]);
        assert_eq!(load_tranco(&path, Some(0)).unwrap(), vec![]);
    }

    #[test]
    fn skips_bad_rows_and_keeps_best_rank_of_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("messy.csv");
        let data = "\n1,example.com\nnot a row\n0,zero.com\n-3,negative.com\n4,\n5,127.0.0.1\n\"6\",\"quoted.org\"\n7,www.example.com\n2,shop.example.com,extra\n";
        std::fs::write(&path, data).unwrap();
        assert_eq!(
            load_tranco(&path, None).unwrap(),
            vec![entry(1, "example.com"), entry(6, "quoted.org")]
        );
    }

    #[test]
    fn file_with_no_usable_rows_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("page.html");
        std::fs::write(&path, "<html>\n<body>Not found</body>\n</html>\n").unwrap();
        let err = format!("{:#}", load_tranco(&path, None).unwrap_err());
        assert!(err.contains("no `rank,domain` rows"), "{err}");

        let empty = dir.path().join("empty.csv");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(load_tranco(&empty, None).unwrap(), vec![]);
        assert!(load_tranco(&dir.path().join("missing.csv"), None).is_err());
    }
}
