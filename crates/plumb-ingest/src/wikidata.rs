//! Wikidata "official website" (P856) statements, used to tell an
//! organization's real site apart from look-alikes.

use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::registrable_domain;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{open_maybe_gz, snippet, LineReader};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfficialSite {
    /// Wikidata item id, e.g. `Q739868`.
    pub item: String,
    /// English label, e.g. `U.S. Bancorp`.
    pub label: String,
    /// The official website URL as stated in Wikidata.
    pub url: String,
    /// Registrable domain of `url`.
    pub domain: String,
}

/// Reads a tab separated file with the header `item\tlabel\twebsite` (the
/// format [`crate::download::download_wikidata_official_sites`] writes).
/// `item` may be a bare id (`Q739868`) or an entity URL
/// (`http://www.wikidata.org/entity/Q739868`); it is stored as the bare id.
/// Rows whose website has no registrable domain are skipped.
///
/// Columns are found by header name, so their order does not matter; a
/// missing header is an error. Rows with too few fields or an empty item or
/// website are skipped with a warning. The file may be gzipped.
pub fn load_wikidata_official_sites(path: &Path) -> Result<Vec<OfficialSite>> {
    let mut lines = LineReader::new(open_maybe_gz(path)?);
    let read_err = || format!("reading {}", path.display());
    let header = loop {
        match lines.next_line().with_context(read_err)? {
            None => bail!(
                "{} is empty; expected the header `item\\tlabel\\twebsite`",
                path.display()
            ),
            Some((_, line)) if line.trim().is_empty() => continue,
            Some((_, line)) => break line.into_owned(),
        }
    };
    let names: Vec<String> = header
        .split('\t')
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    let column = |name: &str| {
        names.iter().position(|n| n == name).with_context(|| {
            format!(
                "{}: the header has no `{name}` column; expected `item\\tlabel\\twebsite`, found {}",
                path.display(),
                snippet(&header)
            )
        })
    };
    let (item_col, label_col, website_col) =
        (column("item")?, column("label")?, column("website")?);

    let mut sites = Vec::new();
    let mut bad_rows = 0u64;
    let mut first_bad: Option<(u64, String)> = None;
    let mut no_domain = 0u64;
    while let Some((line_no, line)) = lines.next_line().with_context(read_err)? {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').map(str::trim).collect();
        let field = |i: usize| fields.get(i).copied();
        let item = field(item_col).map(bare_item_id).filter(|i| !i.is_empty());
        let label = field(label_col);
        let website = field(website_col).filter(|w| !w.is_empty());
        let (Some(item), Some(label), Some(website)) = (item, label, website) else {
            bad_rows += 1;
            first_bad.get_or_insert_with(|| (line_no, snippet(&line)));
            continue;
        };
        let Some(domain) = registrable_domain(website) else {
            no_domain += 1;
            continue;
        };
        sites.push(OfficialSite {
            item: item.to_string(),
            label: label.to_string(),
            url: website.to_string(),
            domain,
        });
    }

    if let Some((line_no, line)) = first_bad {
        warn!(
            "{}: skipped {bad_rows} malformed rows (first at line {line_no}: {line})",
            path.display()
        );
    }
    info!(
        "loaded {} official websites from {} ({no_domain} without a registrable domain)",
        sites.len(),
        path.display()
    );
    Ok(sites)
}

/// `http://www.wikidata.org/entity/Q739868` -> `Q739868`; a bare id is returned as is.
pub(crate) fn bare_item_id(item: &str) -> &str {
    let item = item.trim().trim_end_matches('/');
    item.rsplit('/').next().unwrap_or(item).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(item: &str, label: &str, url: &str, domain: &str) -> OfficialSite {
        OfficialSite {
            item: item.into(),
            label: label.into(),
            url: url.into(),
            domain: domain.into(),
        }
    }

    fn write(dir: &Path, data: &str) -> std::path::PathBuf {
        let path = dir.join("wikidata-official-sites.tsv");
        std::fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn reads_bare_ids_and_entity_urls() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "item\tlabel\twebsite\n\
             Q739868\tU.S. Bancorp\thttps://www.usbank.com/\n\
             http://www.wikidata.org/entity/Q42\tBBC News\thttps://news.bbc.co.uk\r\n\
             https://www.wikidata.org/wiki/Q95\tGoogle\tgoogle.com\n",
        );
        assert_eq!(
            load_wikidata_official_sites(&path).unwrap(),
            vec![
                site(
                    "Q739868",
                    "U.S. Bancorp",
                    "https://www.usbank.com/",
                    "usbank.com"
                ),
                site("Q42", "BBC News", "https://news.bbc.co.uk", "bbc.co.uk"),
                site("Q95", "Google", "google.com", "google.com"),
            ]
        );
    }

    #[test]
    fn reads_gzipped_tsv() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wikidata-official-sites.tsv.gz");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"item\tlabel\twebsite\nQ739868\tU.S. Bancorp\thttps://www.usbank.com/\n")
            .unwrap();
        std::fs::write(&path, enc.finish().unwrap()).unwrap();
        assert_eq!(
            load_wikidata_official_sites(&path).unwrap(),
            vec![site(
                "Q739868",
                "U.S. Bancorp",
                "https://www.usbank.com/",
                "usbank.com"
            )]
        );
    }

    #[test]
    fn columns_found_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "website\titem\tlabel\nhttps://example.org/about\tQ1\tExample\n",
        );
        assert_eq!(
            load_wikidata_official_sites(&path).unwrap(),
            vec![site(
                "Q1",
                "Example",
                "https://example.org/about",
                "example.org"
            )]
        );
    }

    #[test]
    fn skips_bad_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "item\tlabel\twebsite\n\
             \n\
             Q1\tNo website\t\n\
             Q2\tToo few fields\n\
             \tNo item\thttps://noitem.com/\n\
             Q3\tAn IP\thttp://192.168.0.1/\n\
             Q4\tA suffix\thttps://co.uk/\n\
             Q5\t\thttps://nolabel.com/\n\
             Q6\tGood\thttps://good.com/\n",
        );
        assert_eq!(
            load_wikidata_official_sites(&path).unwrap(),
            vec![
                site("Q5", "", "https://nolabel.com/", "nolabel.com"),
                site("Q6", "Good", "https://good.com/", "good.com"),
            ]
        );
    }

    #[test]
    fn missing_header_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "Q1\tExample\thttps://example.org/\n");
        let err = format!("{:#}", load_wikidata_official_sites(&path).unwrap_err());
        assert!(err.contains("no `item` column"), "{err}");
        let empty = write(dir.path(), "");
        let err = format!("{:#}", load_wikidata_official_sites(&empty).unwrap_err());
        assert!(err.contains("is empty"), "{err}");
    }

    #[test]
    fn item_ids() {
        assert_eq!(bare_item_id("Q739868"), "Q739868");
        assert_eq!(
            bare_item_id(" http://www.wikidata.org/entity/Q739868 "),
            "Q739868"
        );
        assert_eq!(bare_item_id("https://www.wikidata.org/entity/Q1/"), "Q1");
        assert_eq!(bare_item_id(""), "");
    }
}
