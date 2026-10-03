//! Common Crawl's domain-level web graph ranks
//! (`<release>-domain-ranks.txt.gz`, published with each web graph release).

use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::{registrable_domain, reverse_host};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{open_maybe_gz, snippet, too_long_note, Line, LineReader, MAX_LINE_BYTES};

/// Rows between progress messages while reading a ranks file.
const PROGRESS_EVERY: u64 = 10_000_000;

/// How many domains [`load_cc_domain_ranks`] keeps when the caller passes no
/// limit. A full release ranks over 100 million domains, and each one costs
/// close to a kilobyte by the time it is a site record, so reading a whole
/// file would take around 90 GB. The file is sorted best first, so the cap
/// keeps the most central domains; a warning is logged when it cuts a file short.
pub const DEFAULT_CC_RANKS_LIMIT: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CcRank {
    /// Registrable domain, e.g. `usbank.com` (the file stores it reversed: `com.usbank`).
    pub domain: String,
    /// 1-based position by harmonic centrality (`#harmonicc_pos`).
    pub harmonic_rank: u64,
    /// 1-based position by PageRank (`#pr_pos`), when the column is present.
    pub pagerank_rank: Option<u64>,
    /// Number of hosts under the domain (`#n_hosts`), when present.
    pub n_hosts: Option<u64>,
}

/// Reads a domain ranks file: tab separated, gzipped or plain, whose first
/// line is a header such as
/// `#harmonicc_pos\t#harmonicc_val\t#pr_pos\t#pr_val\t#host_rev\t#n_hosts`.
/// Columns are located by header name (with or without the leading `#`), so
/// extra or reordered columns are fine; `harmonicc_pos` and `host_rev` are
/// required. Rows whose host has no registrable domain are skipped. The file
/// is sorted by harmonic centrality, so `limit` keeps the top rows.
///
/// The file is streamed, and reading stops once `limit` rows are kept, or
/// [`DEFAULT_CC_RANKS_LIMIT`] rows when `limit` is `None` (pass
/// `Some(usize::MAX)` to read everything). Malformed rows (a missing or
/// non-numeric rank, an empty host, a line over 1 MiB) are skipped with a
/// warning. Rows are not deduplicated: if our Public Suffix List maps
/// two of Common Crawl's domains to one, both rows come back and
/// [`crate::Builder::add_cc_ranks`] keeps the better ranks.
pub fn load_cc_domain_ranks(path: &Path, limit: Option<usize>) -> Result<Vec<CcRank>> {
    let (ranks, cut_short) = read_ranks(path, limit.unwrap_or(DEFAULT_CC_RANKS_LIMIT))?;
    if cut_short && limit.is_none() {
        warn!(
            "{}: kept the top {DEFAULT_CC_RANKS_LIMIT} domains, the default cap, and left the rest of the file unread; pass a limit to read more",
            path.display()
        );
    }
    Ok(ranks)
}

/// Reads up to `cap` ranks; the flag is true when rows were left unread.
fn read_ranks(path: &Path, cap: usize) -> Result<(Vec<CcRank>, bool)> {
    let mut lines = LineReader::new(open_maybe_gz(path)?);
    let read_err = || format!("reading {}", path.display());
    let header = loop {
        match lines.next_line().with_context(read_err)? {
            None => bail!(
                "{} is empty; expected a header line like `#harmonicc_pos\\t#harmonicc_val\\t#pr_pos\\t#pr_val\\t#host_rev\\t#n_hosts`",
                path.display()
            ),
            Some((line_no, Line::TooLong)) => bail!(
                "{}: line {line_no} is over {MAX_LINE_BYTES} bytes; not a domain ranks file",
                path.display()
            ),
            Some((_, Line::Text(line))) if line.trim().is_empty() => continue,
            Some((_, Line::Text(line))) => break line.into_owned(),
        }
    };
    let columns = Columns::from_header(&header)
        .with_context(|| format!("{}: not a domain ranks file", path.display()))?;

    let mut ranks = Vec::new();
    let mut bad_rows = 0u64;
    let mut first_bad: Option<(u64, String)> = None;
    let mut no_domain = 0u64;
    let mut cut_short = false;
    while let Some((line_no, line)) = lines.next_line().with_context(read_err)? {
        if line_no % PROGRESS_EVERY == 0 {
            info!(
                "{}: read {line_no} rows, kept {}",
                path.display(),
                ranks.len()
            );
        }
        if matches!(&line, Line::Text(text) if text.trim().is_empty()) {
            continue;
        }
        if ranks.len() >= cap {
            cut_short = true;
            break;
        }
        let Line::Text(line) = line else {
            bad_rows += 1;
            first_bad.get_or_insert_with(|| (line_no, too_long_note()));
            continue;
        };
        let Some(row) = columns.parse_row(&line) else {
            bad_rows += 1;
            first_bad.get_or_insert_with(|| (line_no, snippet(&line)));
            continue;
        };
        match registrable_domain(&reverse_host(row.host_rev)) {
            Some(domain) => ranks.push(CcRank {
                domain,
                harmonic_rank: row.harmonic_rank,
                pagerank_rank: row.pagerank_rank,
                n_hosts: row.n_hosts,
            }),
            None => no_domain += 1,
        }
    }

    if let Some((line_no, line)) = first_bad {
        warn!(
            "{}: skipped {bad_rows} malformed rows (first at line {line_no}: {line})",
            path.display()
        );
    }
    info!(
        "loaded {} domain ranks from {}{} ({no_domain} rows without a registrable domain)",
        ranks.len(),
        path.display(),
        if cut_short {
            ", stopping at the limit"
        } else {
            ""
        }
    );
    Ok((ranks, cut_short))
}

/// Column positions found in the header line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Columns {
    harmonic_pos: usize,
    host_rev: usize,
    pr_pos: Option<usize>,
    n_hosts: Option<usize>,
}

/// The fields of one data row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row<'a> {
    host_rev: &'a str,
    harmonic_rank: u64,
    pagerank_rank: Option<u64>,
    n_hosts: Option<u64>,
}

impl Columns {
    fn from_header(header: &str) -> Result<Columns> {
        let names: Vec<String> = header
            .split('\t')
            .map(|name| name.trim().trim_start_matches('#').to_ascii_lowercase())
            .collect();
        let find = |name: &str| names.iter().position(|n| n == name);
        let required = |name: &str| {
            find(name).with_context(|| {
                format!(
                    "the header has no `{name}` column (header: {})",
                    snippet(header)
                )
            })
        };
        Ok(Columns {
            harmonic_pos: required("harmonicc_pos")?,
            host_rev: required("host_rev")?,
            pr_pos: find("pr_pos"),
            n_hosts: find("n_hosts"),
        })
    }

    /// Parses one row; `None` when a required field is missing or a numeric
    /// field is not a number. An optional column missing from the row (or
    /// empty) is `None`.
    fn parse_row<'a>(&self, line: &'a str) -> Option<Row<'a>> {
        let mut harmonic = None;
        let mut host_rev = None;
        let mut pr = None;
        let mut n_hosts = None;
        for (i, field) in line.split('\t').enumerate() {
            let field = field.trim();
            if i == self.harmonic_pos {
                harmonic = Some(field);
            }
            if i == self.host_rev {
                host_rev = Some(field);
            }
            if Some(i) == self.pr_pos {
                pr = Some(field);
            }
            if Some(i) == self.n_hosts {
                n_hosts = Some(field);
            }
        }
        let optional = |field: Option<&str>| -> Option<Option<u64>> {
            match field.filter(|f| !f.is_empty()) {
                None => Some(None),
                Some(f) => f.parse::<u64>().ok().map(Some),
            }
        };
        Some(Row {
            host_rev: host_rev.filter(|h| !h.is_empty())?,
            harmonic_rank: harmonic?.parse::<u64>().ok().filter(|&r| r > 0)?,
            pagerank_rank: optional(pr)?.filter(|&r| r > 0),
            n_hosts: optional(n_hosts)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const RANKS: &str = "#harmonicc_pos\t#harmonicc_val\t#pr_pos\t#pr_val\t#host_rev\t#n_hosts\n\
        1\t3.4E7\t2\t0.012\tcom.google\t12345\n\
        2\t3.3E7\t1\t0.015\tcom.facebook\t678\n\
        3\t3.1E7\t9\t0.004\tuk.co.bbc\t90\n\
        4\t3.0E7\t50\t0.001\tuk.co\t1\n\
        5\t2.9E7\t40\t0.002\tcom.usbank\t7\n";

    fn rank(domain: &str, harmonic: u64, pr: Option<u64>, n_hosts: Option<u64>) -> CcRank {
        CcRank {
            domain: domain.to_string(),
            harmonic_rank: harmonic,
            pagerank_rank: pr,
            n_hosts,
        }
    }

    fn expected() -> Vec<CcRank> {
        vec![
            rank("google.com", 1, Some(2), Some(12345)),
            rank("facebook.com", 2, Some(1), Some(678)),
            rank("bbc.co.uk", 3, Some(9), Some(90)),
            rank("usbank.com", 5, Some(40), Some(7)),
        ]
    }

    fn write(dir: &Path, name: &str, data: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn reads_real_header_plain_and_gzipped() {
        let dir = tempfile::tempdir().unwrap();
        let plain = write(dir.path(), "ranks.txt", RANKS);
        assert_eq!(load_cc_domain_ranks(&plain, None).unwrap(), expected());

        let gz = dir.path().join("ranks.txt.gz");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(RANKS.as_bytes()).unwrap();
        std::fs::write(&gz, enc.finish().unwrap()).unwrap();
        assert_eq!(load_cc_domain_ranks(&gz, None).unwrap(), expected());
    }

    #[test]
    fn limit_keeps_top_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "ranks.txt", RANKS);
        assert_eq!(
            load_cc_domain_ranks(&path, Some(2)).unwrap(),
            expected()[..2]
        );
        // `uk.co` is skipped, so a limit of 4 reaches `usbank.com`.
        assert_eq!(load_cc_domain_ranks(&path, Some(4)).unwrap(), expected());
        assert!(load_cc_domain_ranks(&path, Some(0)).unwrap().is_empty());
    }

    #[test]
    fn columns_found_by_name_in_any_order() {
        let dir = tempfile::tempdir().unwrap();
        let data = "host_rev\tHARMONICC_POS\textra\r\n\
            com.Example.www\t7\tx\r\n\
            org.wikipedia\t8\ty\r\n";
        let path = write(dir.path(), "reordered.txt", data);
        assert_eq!(
            load_cc_domain_ranks(&path, None).unwrap(),
            vec![
                rank("example.com", 7, None, None),
                rank("wikipedia.org", 8, None, None)
            ]
        );
    }

    #[test]
    fn missing_or_bad_header_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let no_host = write(dir.path(), "a.txt", "#harmonicc_pos\t#pr_pos\n1\t2\n");
        let err = format!("{:#}", load_cc_domain_ranks(&no_host, None).unwrap_err());
        assert!(err.contains("`host_rev`"), "{err}");

        let no_header = write(dir.path(), "b.txt", "1\t3.4E7\t2\t0.01\tcom.google\t5\n");
        let err = format!("{:#}", load_cc_domain_ranks(&no_header, None).unwrap_err());
        assert!(err.contains("`harmonicc_pos`"), "{err}");

        let empty = write(dir.path(), "c.txt", "");
        let err = format!("{:#}", load_cc_domain_ranks(&empty, None).unwrap_err());
        assert!(err.contains("is empty"), "{err}");
    }

    #[test]
    fn skips_bad_rows() {
        let dir = tempfile::tempdir().unwrap();
        let data = "#harmonicc_pos\t#harmonicc_val\t#pr_pos\t#pr_val\t#host_rev\t#n_hosts\n\
            x\t1.0\t1\t0.1\tcom.notanumber\t1\n\
            0\t1.0\t1\t0.1\tcom.zero\t1\n\
            3\t1.0\tbad\t0.1\tcom.badpr\t1\n\
            4\t1.0\t4\t0.1\t\t1\n\
            5\t1.0\n\
            \n\
            6\t1.0\t6\t0.1\tlocalhost\t1\n\
            7\t1.0\t7\t0.1\tcom.short\n\
            8\t1.0\t\t0.1\tnet.nopr\t\n";
        let path = write(dir.path(), "bad.txt", data);
        assert_eq!(
            load_cc_domain_ranks(&path, None).unwrap(),
            vec![
                rank("short.com", 7, Some(7), None),
                rank("nopr.net", 8, None, None)
            ]
        );
    }

    #[test]
    fn no_limit_means_the_default_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "ranks.txt", &format!("{RANKS}\n\n"));
        // The default cap is too big for a unit test; the same code runs with a small one.
        assert_eq!(
            read_ranks(&path, 3).unwrap(),
            (expected()[..3].to_vec(), true)
        );
        // Reaching the cap exactly at the end of the file (blank lines aside) is not a cut.
        assert_eq!(read_ranks(&path, 4).unwrap(), (expected(), false));
        assert_eq!(read_ranks(&path, 0).unwrap(), (vec![], true));
        assert_eq!(load_cc_domain_ranks(&path, None).unwrap(), expected());
        assert_eq!(
            load_cc_domain_ranks(&path, Some(usize::MAX)).unwrap(),
            expected()
        );
    }

    #[test]
    fn malformed_hosts_and_over_long_lines_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let data = format!(
            "#harmonicc_pos\t#host_rev\n\
             1\tcom..x\n\
             2\tcom.{}\n\
             3\tcom.{}\n\
             4\t{}\n\
             5\tcom.example\n",
            "a".repeat(64),
            "b".repeat(70_000),
            "c".repeat(MAX_LINE_BYTES),
        );
        let path = write(dir.path(), "ranks.txt", &data);
        assert_eq!(
            load_cc_domain_ranks(&path, None).unwrap(),
            vec![rank("example.com", 5, None, None)]
        );

        let header = format!("#{}\n1\tcom.example\n", "x".repeat(MAX_LINE_BYTES));
        let path = write(dir.path(), "long-header.txt", &header);
        let err = format!("{:#}", load_cc_domain_ranks(&path, None).unwrap_err());
        assert!(err.contains("line 1 is over"), "{err}");
    }
}
