//! Wikidata "official website" (P856) statements, used to tell an
//! organization's real site apart from look-alikes.

use std::path::Path;

use anyhow::{bail, Context, Result};
use plumb_core::{host_of, is_homepage_path, registrable_domain};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use url::Url;

use crate::{open_maybe_gz, snippet, too_long_note, Line, LineReader, MAX_LINE_BYTES};

/// One "official website" statement: an item, its label and the URL it
/// claims. Build it with [`OfficialSite::new`], which derives the other fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfficialSite {
    /// Wikidata item id, e.g. `Q739868`.
    pub item: String,
    /// English label, e.g. `U.S. Bancorp`.
    pub label: String,
    /// The official website URL as stated in Wikidata.
    pub url: String,
    /// Host of `url`: lowercase, punycode, without port or trailing dot,
    /// e.g. `www.usbank.com`.
    pub host: String,
    /// Path of `url` without the query, e.g. `/` or `/acmerockets`.
    pub path: String,
    /// Registrable domain of `url`, e.g. `usbank.com`.
    pub domain: String,
    /// The item's country (Wikidata P17) as an ISO 3166-1 alpha-2 code,
    /// e.g. `US`, from [`crate::facts`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// What the item is ("bank"), from [`crate::facts`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    /// The item's other English names ("NYT"), from [`crate::facts`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// The item's English description, from [`crate::facts`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
}

impl OfficialSite {
    /// The claim that `item` (called `label`) has the official website
    /// `url`, or `None` when `url` has no registrable domain: not http(s),
    /// an IP address, a bare public suffix or a malformed host name. A bare
    /// host name such as `google.com` is read as an http URL.
    pub fn new(item: impl Into<String>, label: impl Into<String>, url: &str) -> Option<Self> {
        let url = url.trim();
        let host = host_of(url)?;
        let domain = registrable_domain(&host)?;
        Some(OfficialSite {
            item: item.into(),
            label: label.into(),
            url: url.to_string(),
            path: url_path(url),
            host,
            domain,
            country: None,
            kinds: Vec::new(),
            names: Vec::new(),
            about: None,
        })
    }

    /// True when the claim is for the front page of the registrable domain
    /// itself: the host is the domain or `www.` plus it, and the path is a
    /// front page ([`plumb_core::is_homepage_path`]; a query string is
    /// ignored). Only these claims say the domain is the item's site. A
    /// subdomain (`www.balliol.ox.ac.uk`, `en.wikipedia.org`) or a deeper
    /// path (`linktr.ee/acmerockets`) is a part or a tenant of the site.
    pub fn is_root_homepage(&self) -> bool {
        let canonical_host = self.host == self.domain
            || self.host.strip_prefix("www.") == Some(self.domain.as_str());
        canonical_host && is_homepage_path(&self.path)
    }
}

/// The path of an http(s) URL, or of a bare host name read as one.
fn url_path(url: &str) -> String {
    let parsed = if url.contains("://") {
        Url::parse(url)
    } else {
        Url::parse(&format!("http://{url}"))
    };
    parsed.map(|u| u.path().to_string()).unwrap_or_default()
}

/// Reads a tab separated file with the header `item\tlabel\twebsite` (the
/// format [`crate::download::download_wikidata_official_sites`] writes).
/// `item` may be a bare id (`Q739868`) or an entity URL
/// (`http://www.wikidata.org/entity/Q739868`); it is stored as the bare id.
/// Rows whose website has no registrable domain (see [`OfficialSite::new`])
/// are skipped.
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
            Some((line_no, Line::TooLong)) => bail!(
                "{}: line {line_no} is over {MAX_LINE_BYTES} bytes; expected the header `item\\tlabel\\twebsite`",
                path.display()
            ),
            Some((_, Line::Text(line))) if line.trim().is_empty() => continue,
            Some((_, Line::Text(line))) => break line.into_owned(),
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
        let line = match line {
            Line::Text(line) if line.trim().is_empty() => continue,
            Line::Text(line) => line,
            Line::TooLong => {
                bad_rows += 1;
                first_bad.get_or_insert_with(|| (line_no, too_long_note()));
                continue;
            }
        };
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
        let Some(site) = OfficialSite::new(item, label, website) else {
            no_domain += 1;
            continue;
        };
        sites.push(site);
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

    fn site(item: &str, label: &str, url: &str) -> OfficialSite {
        OfficialSite::new(item, label, url).unwrap()
    }

    fn domains(sites: &[OfficialSite]) -> Vec<&str> {
        sites.iter().map(|s| s.domain.as_str()).collect()
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
        let sites = load_wikidata_official_sites(&path).unwrap();
        assert_eq!(
            sites,
            vec![
                site("Q739868", "U.S. Bancorp", "https://www.usbank.com/"),
                site("Q42", "BBC News", "https://news.bbc.co.uk"),
                site("Q95", "Google", "google.com"),
            ]
        );
        assert_eq!(domains(&sites), ["usbank.com", "bbc.co.uk", "google.com"]);
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
            vec![site("Q739868", "U.S. Bancorp", "https://www.usbank.com/")]
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
            vec![site("Q1", "Example", "https://example.org/about")]
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
             Q6\tGood\thttps://good.com/\n\
             Q7\tMail\tmailto:a@b.com\n\
             Q8\tEmpty label\thttps://a..b.com/\n\
             Q9\tFTP\tftp://files.example.com/\n",
        );
        let sites = load_wikidata_official_sites(&path).unwrap();
        assert_eq!(
            sites,
            vec![
                site("Q5", "", "https://nolabel.com/"),
                site("Q6", "Good", "https://good.com/"),
            ]
        );
        assert_eq!(domains(&sites), ["nolabel.com", "good.com"]);

        let long_line = format!(
            "item\tlabel\twebsite\nQ1\t{}\thttps://a.com/\nQ2\tB\thttps://b.com/\n",
            "x".repeat(crate::MAX_LINE_BYTES)
        );
        let path = write(dir.path(), &long_line);
        assert_eq!(
            load_wikidata_official_sites(&path).unwrap(),
            vec![site("Q2", "B", "https://b.com/")]
        );
    }

    #[test]
    fn official_site_parts_and_root_homepages() {
        let parts = |url: &str| {
            OfficialSite::new("Q1", "Label", url).map(|s| {
                (
                    s.host.clone(),
                    s.path.clone(),
                    s.domain.clone(),
                    s.is_root_homepage(),
                )
            })
        };
        let some = |host: &str, path: &str, domain: &str, root: bool| {
            Some((host.to_string(), path.to_string(), domain.to_string(), root))
        };
        assert_eq!(
            parts("https://www.usbank.com/"),
            some("www.usbank.com", "/", "usbank.com", true)
        );
        assert_eq!(
            parts(" HTTPS://WWW.Example.COM.:443 "),
            some("www.example.com", "/", "example.com", true)
        );
        assert_eq!(
            parts("google.com"),
            some("google.com", "/", "google.com", true)
        );
        assert_eq!(
            parts("https://example.com/en-us/index.html?ref=wd"),
            some("example.com", "/en-us/index.html", "example.com", true)
        );
        // A subdomain, a deeper path or a tenant's page is not the domain's front page.
        assert_eq!(
            parts("https://www.balliol.ox.ac.uk/"),
            some("www.balliol.ox.ac.uk", "/", "ox.ac.uk", false)
        );
        assert_eq!(
            parts("https://en.wikipedia.org/wiki/Main_Page"),
            some(
                "en.wikipedia.org",
                "/wiki/Main_Page",
                "wikipedia.org",
                false
            )
        );
        assert_eq!(
            parts("https://linktr.ee/acmerockets"),
            some("linktr.ee", "/acmerockets", "linktr.ee", false)
        );
        assert_eq!(
            parts("example.org/about"),
            some("example.org", "/about", "example.org", false)
        );
        for junk in [
            "mailto:a@b.com",
            "a@b.com",
            "ftp://example.com/",
            "https://a..b.com/",
            "https://192.168.0.1/",
            "https://co.uk/",
            "",
        ] {
            assert_eq!(parts(junk), None, "{junk}");
        }
        let huge = format!("https://{}.com/", "a".repeat(70_000));
        assert_eq!(parts(&huge), None);
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
