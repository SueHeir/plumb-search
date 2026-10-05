//! Page sets: single pages listed with the sites, starting with English
//! Wikipedia's articles (see [`plumb_core::article`] and
//! [`plumb_index::pages`]).
//!
//! Each set is a file in `DIR/pages/sets/`, most read pages first, which
//! `plumb fetch-pages` makes from public dumps. How many of each set's
//! pages a node keeps is a setting ([`PageSets`]): off, a number of the
//! most read ones, all of them, or automatic, which follows the storage
//! limit (100,000 Wikipedia articles, about 10 MB, under 1 GB; a million,
//! about 100 MB, under 8 GB; all of them otherwise).
//!
//! The pages kept are indexed together in `DIR/pages/index-<key>/`, where
//! the key names the sets, their files and how many pages of each, so a
//! changed setting or a new file builds a new index and an unchanged one is
//! opened as it is.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use plumb_core::article::{articles_file_name, parse_article};
use plumb_index::pages::{build_page_index, Page, PageSearcher};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Directory of page sets and their index in a data directory.
pub const PAGES_DIR: &str = "pages";
/// Directory of the set files, in [`PAGES_DIR`].
pub const SETS_DIR: &str = "sets";

/// A page set a node can keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetInfo {
    /// The set's id, as in settings and file names: `wikipedia-en`.
    pub id: &'static str,
    /// What people see: "English Wikipedia articles".
    pub name: &'static str,
    /// About how many pages there are in all, for the panel.
    pub pages: u64,
    /// About how many bytes a page takes on disk, file and index.
    pub bytes_per_page: u64,
}

/// The page sets there are.
pub const SETS: &[SetInfo] = &[
    SetInfo {
        id: "wikipedia-en",
        name: "English Wikipedia articles",
        pages: 7_000_000,
        bytes_per_page: 100,
    },
    SetInfo {
        id: plumb_index::pages::GITHUB_SET,
        name: "GitHub repositories",
        pages: 300_000,
        bytes_per_page: 150,
    },
    SetInfo {
        id: plumb_index::pages::STACKOVERFLOW_SET,
        name: "Stack Overflow questions",
        pages: 2_000_000,
        bytes_per_page: 120,
    },
    SetInfo {
        id: plumb_index::pages::BOOKS_SET,
        name: "Books (Open Library)",
        pages: 1_000_000,
        bytes_per_page: 110,
    },
    SetInfo {
        id: plumb_index::pages::PAPERS_SET,
        name: "Papers (OpenAlex)",
        pages: 2_000_000,
        bytes_per_page: 160,
    },
    // Searched apart from the pages, by where they are (see
    // `crate::places`).
    SetInfo {
        id: plumb_index::places::PLACES_SET,
        name: "Places (OpenStreetMap)",
        pages: 25_000_000,
        bytes_per_page: 130,
    },
];

impl SetInfo {
    pub fn find(id: &str) -> Option<&'static SetInfo> {
        SETS.iter().find(|set| set.id == id)
    }

    /// The set's file in `data_dir`.
    pub fn file(&self, data_dir: &Path) -> PathBuf {
        let name = match self.id.strip_prefix("wikipedia-") {
            Some(lang) => articles_file_name(lang),
            None => format!("{}.tsv.gz", self.id),
        };
        sets_dir(data_dir).join(name)
    }

    /// Reads up to `limit` pages of the set's file `path`, most read first.
    fn read(&self, path: &Path, limit: u64) -> Result<impl Iterator<Item = Page>> {
        // Every set's file is an articles file (see `Page::from_set`).
        let id = self.id;
        if Page::from_set(id, Default::default()).is_none() {
            bail!("no reader for the page set {id}");
        }
        let reader = plumb_ingest::open_maybe_gz(path)?;
        let path = path.to_path_buf();
        let mut bad = 0u64;
        Ok(std::io::BufRead::lines(reader)
            .enumerate()
            .map_while(move |(n, line)| match line {
                Ok(line) => Some((n, line)),
                Err(err) => {
                    warn!("reading {}: {err}", path.display());
                    None
                }
            })
            .filter(|(n, line)| !(*n == 0 && line.starts_with("views\t")) && !line.is_empty())
            .filter_map(move |(n, line)| match parse_article(&line) {
                // Files made before fetch-pages left it out.
                Ok(article) if article.title == "Main Page" => None,
                Ok(article) => Page::from_set(id, article),
                Err(err) => {
                    bad += 1;
                    if bad <= 3 {
                        warn!("page set line {}: {err:#}", n + 1);
                    }
                    None
                }
            })
            .take(usize::try_from(limit).unwrap_or(usize::MAX)))
    }
}

impl SetInfo {
    /// What is known of the set's file in `data_dir`, `None` when there is
    /// none. A file with no notes (made by `plumb fetch-pages`) is whole.
    pub fn file_notes(&self, data_dir: &Path) -> Option<SetFileNotes> {
        let file = self.file(data_dir);
        if !file.is_file() {
            return None;
        }
        Some(
            std::fs::read(notes_path(&file))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or(SetFileNotes {
                    lines: u64::MAX,
                    complete: true,
                    source_modified: 0,
                    fetched_at: 0,
                }),
        )
    }

    /// The set's file in `data_dir` when it is whole, so it can be handed
    /// to other nodes.
    pub fn servable_file(&self, data_dir: &Path) -> Option<PathBuf> {
        self.file_notes(data_dir)
            .filter(|notes| notes.complete)
            .map(|_| self.file(data_dir))
    }
}

/// What a node notes about a set file it took from another node, next to
/// it as `<file>.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetFileNotes {
    /// Pages in the file.
    pub lines: u64,
    /// Every page of the other node's file is in it.
    pub complete: bool,
    /// When the other node's file was made (Unix seconds).
    pub source_modified: u64,
    /// When it was taken (Unix seconds).
    pub fetched_at: u64,
}

/// `<file>.json`, the notes of a set file.
pub fn notes_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".json");
    PathBuf::from(name)
}

/// The sets to keep under `sets` and a storage limit of
/// `storage_limit_mb`, and how many pages of each, whether or not the node
/// has their files yet.
pub fn wanted_counts(sets: &PageSets, storage_limit_mb: u64) -> Vec<(&'static SetInfo, u64)> {
    SETS.iter()
        .map(|set| (set, sets.size(set.id).pages(storage_limit_mb)))
        .filter(|(_, pages)| *pages > 0)
        .collect()
}

/// Writes the first `limit` pages of a set file, as its gzipped bytes are
/// written to [`SetFileCutter::write`] (gunzipped by the caller), into a
/// new gzip file.
pub struct SetFileCutter {
    limit: u64,
    lines: u64,
    header_done: bool,
    /// Whether lines past the limit were dropped.
    cut: bool,
    out: Option<flate2::write::GzEncoder<std::io::BufWriter<std::fs::File>>>,
}

impl SetFileCutter {
    pub fn create(path: &Path, limit: u64) -> Result<Self> {
        let file =
            std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
        Ok(SetFileCutter {
            limit,
            lines: 0,
            header_done: false,
            cut: false,
            out: Some(flate2::write::GzEncoder::new(
                std::io::BufWriter::new(file),
                flate2::Compression::default(),
            )),
        })
    }

    /// Pages written so far.
    pub fn pages(&self) -> u64 {
        self.lines
    }

    /// Whether it has all the pages it wants.
    pub fn full(&self) -> bool {
        self.lines >= self.limit
    }

    /// Whether pages past the limit were dropped, so the file is not
    /// the whole set.
    pub fn cut(&self) -> bool {
        self.cut
    }

    /// Finishes the gzip file.
    pub fn finish(&mut self) -> Result<()> {
        if let Some(out) = self.out.take() {
            out.finish()?
                .into_inner()
                .map_err(|e| e.into_error())?
                .sync_all()?;
        }
        Ok(())
    }
}

impl std::io::Write for SetFileCutter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut rest = buf;
        while !rest.is_empty() && !self.full() {
            let Some(out) = self.out.as_mut() else { break };
            let end = rest.iter().position(|&b| b == b'\n').map(|i| i + 1);
            let piece = &rest[..end.unwrap_or(rest.len())];
            out.write_all(piece)?;
            if end.is_some() {
                if self.header_done {
                    self.lines += 1;
                } else {
                    self.header_done = true;
                }
            }
            rest = &rest[piece.len()..];
        }
        // What is past the limit is dropped.
        if !rest.is_empty() {
            self.cut = true;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `DIR/pages/sets`.
pub fn sets_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(PAGES_DIR).join(SETS_DIR)
}

/// How many pages of a set to keep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum PageSetSize {
    /// Follows the storage limit (see [`PageSetSize::pages`]).
    #[default]
    Auto,
    Off,
    /// The most read this many.
    Top(u64),
    All,
}

/// The sizes the panel offers, besides automatic.
pub const SIZE_CHOICES: &[PageSetSize] = &[
    PageSetSize::Off,
    PageSetSize::Top(100_000),
    PageSetSize::Top(1_000_000),
    PageSetSize::All,
];

impl PageSetSize {
    /// How many pages to keep under a storage limit of `storage_limit_mb`
    /// (0 for none).
    pub fn pages(self, storage_limit_mb: u64) -> u64 {
        match self {
            PageSetSize::Off => 0,
            PageSetSize::Top(n) => n,
            PageSetSize::All => u64::MAX,
            PageSetSize::Auto => match storage_limit_mb {
                0 => u64::MAX,
                mb if mb < 1_000 => 100_000,
                mb if mb < 8_000 => 1_000_000,
                _ => u64::MAX,
            },
        }
    }

    /// How the panel says it: "1,000,000 most read".
    pub fn words(self) -> String {
        match self {
            PageSetSize::Auto => "Automatic (follows the storage limit)".to_string(),
            PageSetSize::Off => "Off".to_string(),
            PageSetSize::All => "All".to_string(),
            PageSetSize::Top(n) => format!("{} most read", thousands(n)),
        }
    }
}

/// `n` with thousands separators: `1,000,000`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl fmt::Display for PageSetSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PageSetSize::Auto => f.write_str("auto"),
            PageSetSize::Off => f.write_str("off"),
            PageSetSize::All => f.write_str("all"),
            PageSetSize::Top(n) => write!(f, "{n}"),
        }
    }
}

impl FromStr for PageSetSize {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        Ok(match text.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => PageSetSize::Auto,
            "off" | "0" => PageSetSize::Off,
            "all" => PageSetSize::All,
            n => {
                PageSetSize::Top(n.replace([',', '_'], "").parse().with_context(|| {
                    format!("expected auto, off, all or a number, got {text:?}")
                })?)
            }
        })
    }
}

impl TryFrom<String> for PageSetSize {
    type Error = anyhow::Error;

    fn try_from(text: String) -> Result<Self> {
        text.parse()
    }
}

impl From<PageSetSize> for String {
    fn from(size: PageSetSize) -> String {
        size.to_string()
    }
}

/// How much of each page set to keep, by set id; sets not named are
/// [`PageSetSize::Auto`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PageSets(pub BTreeMap<String, PageSetSize>);

impl PageSets {
    pub fn size(&self, set: &str) -> PageSetSize {
        self.0.get(set).copied().unwrap_or_default()
    }

    pub fn set(&mut self, set: &str, size: PageSetSize) {
        if size == PageSetSize::Auto {
            self.0.remove(set);
        } else {
            self.0.insert(set.to_string(), size);
        }
    }

    /// These sets with every one left on Automatic kept in full, for a
    /// node collecting all it can (`NodeConfig::blackhole`); sets turned
    /// off or cut to a number stay so.
    pub fn all_unless_set(&self) -> Self {
        let mut sets = self.clone();
        for set in SETS {
            if sets.size(set.id) == PageSetSize::Auto {
                sets.0.insert(set.id.to_string(), PageSetSize::All);
            }
        }
        sets
    }

    /// Parses `--pages wikipedia-en=1000000,github=off`.
    pub fn parse(text: &str) -> Result<Self> {
        let mut sets = PageSets::default();
        for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (set, size) = part
                .split_once('=')
                .with_context(|| format!("expected SET=SIZE, got {part:?}"))?;
            if SetInfo::find(set.trim()).is_none() {
                bail!(
                    "unknown page set {set:?}; there are: {}",
                    SETS.iter().map(|s| s.id).collect::<Vec<_>>().join(", ")
                );
            }
            sets.set(set.trim(), size.parse()?);
        }
        Ok(sets)
    }
}

/// What one page index holds: for each set kept, how many pages of which
/// file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub sets: Vec<(&'static SetInfo, PathBuf, u64)>,
}

impl Wanted {
    /// The sets of `data_dir` to keep under `sets` and a storage limit of
    /// `storage_limit_mb`; sets with no file yet are left out.
    pub fn new(data_dir: &Path, sets: &PageSets, storage_limit_mb: u64) -> Self {
        Wanted {
            sets: SETS
                .iter()
                .filter(|set| set.id != plumb_index::places::PLACES_SET)
                .filter_map(|set| {
                    let pages = sets.size(set.id).pages(storage_limit_mb);
                    let file = set.file(data_dir);
                    (pages > 0 && file.is_file()).then_some((set, file, pages))
                })
                .collect(),
        }
    }

    /// Names the index of these pages: changes when a set, its file (size
    /// or time) or its count changes.
    pub fn key(&self) -> Option<String> {
        if self.sets.is_empty() {
            return None;
        }
        let mut text = String::from("v1");
        for (set, file, pages) in &self.sets {
            let meta = std::fs::metadata(file).ok();
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            let len = meta.map_or(0, |m| m.len());
            text.push_str(&format!("|{}:{len}:{modified}:{pages}", set.id));
        }
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(text.as_bytes());
        Some(digest[..8].iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// The directory of the page index named `key`.
pub fn index_dir(data_dir: &Path, key: &str) -> PathBuf {
    data_dir.join(PAGES_DIR).join(format!("index-{key}"))
}

/// Opens the page index for `wanted`, building it first when there is
/// none. `None` when nothing is wanted.
pub fn open_or_build(data_dir: &Path, wanted: &Wanted) -> Result<Option<(String, PageSearcher)>> {
    let Some(key) = wanted.key() else {
        return Ok(None);
    };
    let dir = index_dir(data_dir, &key);
    if let Ok(searcher) = PageSearcher::open(&dir) {
        return Ok(Some((key, searcher)));
    }
    let started = std::time::Instant::now();
    let mut pages: Box<dyn Iterator<Item = Page>> = Box::new(std::iter::empty());
    for (set, file, count) in &wanted.sets {
        info!("indexing page set {} from {}", set.id, file.display());
        pages = Box::new(pages.chain(set.read(file, *count)?));
    }
    let stats = build_page_index(&dir, pages)?;
    info!(
        "built the page index of {} pages in {:.1}s",
        stats.pages,
        started.elapsed().as_secs_f32()
    );
    Ok(Some((key, PageSearcher::open(&dir)?)))
}

/// Deletes page indexes other than `keep`; ones still open (Windows) stay
/// until a later try.
pub fn remove_other_indexes(data_dir: &Path, keep: Option<&str>) {
    let Ok(entries) = std::fs::read_dir(data_dir.join(PAGES_DIR)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(key) = name.strip_prefix("index-") else {
            continue;
        };
        if Some(key) != keep {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plumb_core::article::{write_article, Article, ARTICLES_HEADER};

    fn write_set(data_dir: &Path, titles: &[(&str, u64)]) {
        let file = SetInfo::find("wikipedia-en").unwrap().file(data_dir);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut text = ARTICLES_HEADER.as_bytes().to_vec();
        for (title, views) in titles {
            write_article(
                &mut text,
                &Article {
                    title: title.to_string(),
                    views: *views,
                    ..Article::default()
                },
            )
            .unwrap();
        }
        std::fs::write(file, text).unwrap();
    }

    #[test]
    fn cutting_keeps_the_top_pages() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let mut text = ARTICLES_HEADER.as_bytes().to_vec();
        for (title, views) in [("A", 3u64), ("B", 2), ("C", 1)] {
            write_article(
                &mut text,
                &Article {
                    title: title.into(),
                    views,
                    ..Article::default()
                },
            )
            .unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&text).unwrap();
        let gz = gz.finish().unwrap();
        let out = dir.path().join("cut.tsv.gz");
        let mut decoder =
            flate2::write::MultiGzDecoder::new(SetFileCutter::create(&out, 2).unwrap());
        // Fed in small pieces, as chunks arrive.
        for piece in gz.chunks(7) {
            decoder.write_all(piece).unwrap();
            if decoder.get_ref().full() {
                break;
            }
        }
        assert_eq!(decoder.get_ref().pages(), 2);
        assert!(decoder.get_ref().cut());
        decoder.get_mut().finish().unwrap();
        let back =
            plumb_core::article::read_articles(plumb_ingest::open_maybe_gz(&out).unwrap(), 10)
                .unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].title, "B");
    }

    #[test]
    fn sizes_follow_the_storage_limit() {
        assert_eq!(PageSetSize::Auto.pages(500), 100_000);
        assert_eq!(PageSetSize::Auto.pages(2_000), 1_000_000);
        assert_eq!(PageSetSize::Auto.pages(8_000), u64::MAX);
        assert_eq!(PageSetSize::Auto.pages(0), u64::MAX);
        assert_eq!(PageSetSize::Off.pages(0), 0);
        assert_eq!(PageSetSize::Top(5).pages(500), 5);
    }

    #[test]
    fn sizes_and_settings_parse() {
        assert_eq!(
            "1,000,000".parse::<PageSetSize>().unwrap(),
            PageSetSize::Top(1_000_000)
        );
        assert_eq!("all".parse::<PageSetSize>().unwrap(), PageSetSize::All);
        assert!("lots".parse::<PageSetSize>().is_err());
        let sets = PageSets::parse("wikipedia-en=off").unwrap();
        assert_eq!(sets.size("wikipedia-en"), PageSetSize::Off);
        assert!(PageSets::parse("nope=all").is_err());
        let json = serde_json::to_string(&sets).unwrap();
        assert_eq!(json, r#"{"wikipedia-en":"off"}"#);
        assert_eq!(serde_json::from_str::<PageSets>(&json).unwrap(), sets);
        assert_eq!(thousands(1_000_000), "1,000,000");
        assert_eq!(PageSetSize::Top(100_000).words(), "100,000 most read");
    }

    #[test]
    fn builds_once_per_choice() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        assert!(Wanted::new(data, &PageSets::default(), 0).key().is_none());
        write_set(
            data,
            &[("Marie Curie", 900), ("Pierre Curie", 300), ("Curie", 10)],
        );
        let all = Wanted::new(data, &PageSets::default(), 0);
        let (key, searcher) = open_or_build(data, &all).unwrap().unwrap();
        assert_eq!(searcher.num_pages(), 3);
        // Opened again, not rebuilt.
        assert_eq!(open_or_build(data, &all).unwrap().unwrap().0, key);
        let two = Wanted::new(data, &PageSets::parse("wikipedia-en=2").unwrap(), 0);
        let (other, searcher) = open_or_build(data, &two).unwrap().unwrap();
        assert_ne!(other, key);
        assert_eq!(searcher.num_pages(), 2);
        remove_other_indexes(data, Some(&other));
        assert!(!index_dir(data, &key).exists());
        assert!(index_dir(data, &other).exists());
        let off = Wanted::new(data, &PageSets::parse("wikipedia-en=off").unwrap(), 0);
        assert!(open_or_build(data, &off).unwrap().is_none());
    }

    #[test]
    fn repositories_are_searched_with_the_articles() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        write_set(
            data,
            &[("Ripgrep (disambiguation)", 5_000_000), ("Grep", 40)],
        );
        let file = SetInfo::find("github").unwrap().file(data);
        let mut text = ARTICLES_HEADER.as_bytes().to_vec();
        for (title, stars, site) in [
            ("tauri-apps/tauri", 90_000, Some("tauri.app")),
            ("BurntSushi/ripgrep", 50_000, None),
        ] {
            write_article(
                &mut text,
                &Article {
                    title: title.to_string(),
                    views: stars,
                    site: site.map(str::to_string),
                    aliases: vec![title.split('/').nth(1).unwrap().to_string()],
                    ..Article::default()
                },
            )
            .unwrap();
        }
        std::fs::write(&file, text).unwrap();
        let wanted = Wanted::new(data, &PageSets::default(), 0);
        let (_, searcher) = open_or_build(data, &wanted).unwrap().unwrap();
        assert_eq!(searcher.num_pages(), 4);
        let hits = searcher.search("ripgrep", 5).unwrap();
        let repo = hits
            .iter()
            .find(|h| h.page.title == "BurntSushi/ripgrep")
            .unwrap();
        assert!(repo.named);
        assert_eq!(repo.page.url, "https://github.com/BurntSushi/ripgrep");
        assert_eq!(repo.page.set_name(), "GitHub");
        // Stars count against the most starred repository, not against
        // Wikipedia's page views.
        assert!(repo.popularity > 0.9, "{}", repo.popularity);
        let tauri = &searcher.search("tauri", 5).unwrap()[0];
        assert_eq!(tauri.page.site.as_deref(), Some("tauri.app"));
        assert!((tauri.popularity - 1.0).abs() < 1e-3);
    }

    #[test]
    fn questions_are_searched_by_their_words() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        write_set(data, &[("Git", 5_000)]);
        let file = SetInfo::find("stackoverflow").unwrap().file(data);
        let mut text = ARTICLES_HEADER.as_bytes().to_vec();
        write_article(
            &mut text,
            &Article {
                title: "How do I undo the most recent local commits in Git?".to_string(),
                description: Some("git, git-commit, undo".to_string()),
                item: Some("927358".to_string()),
                views: 14_000_000,
                ..Article::default()
            },
        )
        .unwrap();
        std::fs::write(&file, text).unwrap();
        let wanted = Wanted::new(data, &PageSets::default(), 0);
        let (_, searcher) = open_or_build(data, &wanted).unwrap().unwrap();
        let hits = searcher.search("undo last git commit", 5).unwrap();
        let question = hits
            .iter()
            .find(|h| h.page.set == "stackoverflow")
            .expect("the question is found");
        assert_eq!(
            question.page.url,
            "https://stackoverflow.com/questions/927358"
        );
        assert_eq!(question.page.set_name(), "Stack Overflow");
        assert_eq!(question.page.set_domain(), "stackoverflow.com");
    }

    #[test]
    fn a_blackhole_keeps_every_set_left_on_automatic_in_full() {
        let mut sets = PageSets::default();
        sets.set("github", PageSetSize::Off);
        sets.set("stackoverflow", PageSetSize::Top(1_000));
        let all = sets.all_unless_set();
        assert_eq!(all.size("wikipedia-en"), PageSetSize::All);
        assert_eq!(all.size("github"), PageSetSize::Off);
        assert_eq!(all.size("stackoverflow"), PageSetSize::Top(1_000));
        assert!(SETS.iter().all(|set| all.size(set.id) != PageSetSize::Auto));
    }
}
