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
use plumb_core::article::{articles_file_name, articles_of, is_profiles_line, PROFILES_LINE};
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
        id: "wikipedia-es",
        name: "Spanish Wikipedia articles",
        pages: 2_000_000,
        bytes_per_page: 100,
    },
    SetInfo {
        id: "wikipedia-de",
        name: "German Wikipedia articles",
        pages: 3_000_000,
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
        id: plumb_index::pages::STACKEXCHANGE_SET,
        name: "Other Stack Exchange questions (Super User, Ask Ubuntu, Home Improvement and more)",
        pages: 800_000,
        bytes_per_page: 130,
    },
    SetInfo {
        id: plumb_index::pages::BOOKS_SET,
        name: "Books (Open Library)",
        pages: 1_000_000,
        bytes_per_page: 110,
    },
    SetInfo {
        id: plumb_index::pages::PODCASTS_SET,
        name: "Podcasts (Podcast Index)",
        pages: 300_000,
        bytes_per_page: 140,
    },
    SetInfo {
        id: plumb_index::pages::MUSIC_SET,
        name: "Songs and albums (MusicBrainz)",
        pages: 180_000,
        bytes_per_page: 200,
    },
    SetInfo {
        id: plumb_index::pages::FILMS_SET,
        name: "Films and TV shows (Wikidata)",
        pages: 150_000,
        bytes_per_page: 220,
    },
    SetInfo {
        id: plumb_index::pages::DOCS_SET,
        name: "Software docs (MDN, Python, Rust and more)",
        pages: 400_000,
        bytes_per_page: 250,
    },
    SetInfo {
        id: plumb_index::pages::REFERENCE_SET,
        name: "Reference pages (health, dictionaries, recipes, how-tos and more)",
        pages: 700_000,
        bytes_per_page: 300,
    },
    SetInfo {
        id: plumb_index::pages::REFERENCE2_SET,
        name: "Staged reference pages (stricter title relevance)",
        pages: 700_000,
        bytes_per_page: 300,
    },
    SetInfo {
        id: plumb_index::pages::SUBPAGES_SET,
        name: "Pages of universities, companies, government, entertainment and museums",
        pages: 300_000,
        bytes_per_page: 300,
    },
    SetInfo {
        id: plumb_index::pages::PAPERS_SET,
        name: "Papers (OpenAlex, arXiv, CORE)",
        pages: 2_000_000,
        // About half have a free copy's address.
        bytes_per_page: 200,
    },
    SetInfo {
        id: plumb_index::pages::PACKAGES_SET,
        name: "Software packages (npm, PyPI, crates.io and more)",
        pages: 160_000,
        bytes_per_page: 300,
    },
    SetInfo {
        id: plumb_index::pages::WIKIDATA_SET,
        name: "Official profiles without an article (Wikidata)",
        pages: 100_000,
        bytes_per_page: 200,
    },
    SetInfo {
        id: plumb_index::pages::WIKTIONARY_SET,
        name: "Word definitions (Wiktionary), for \"define\" searches",
        pages: 1_000_000,
        bytes_per_page: 150,
    },
    // Searched apart from the pages, by where they are (see
    // `crate::places`).
    SetInfo {
        id: plumb_index::places::PLACES_SET,
        name: "Places (OpenStreetMap)",
        pages: 24_000_000,
        bytes_per_page: 140,
    },
];

impl SetInfo {
    /// How many of the set's pages to keep under `sets` and a storage limit
    /// of `storage_limit_mb`. Places on Automatic follow their own sizes
    /// (see [`crate::places::auto_places`]).
    pub fn kept(&self, sets: &PageSets, storage_limit_mb: u64) -> u64 {
        match sets.size(self.id) {
            // Initial language editions are opt-in by count or All;
            // adding support does not trigger an unbounded download.
            PageSetSize::Auto if matches!(self.id, "wikipedia-es" | "wikipedia-de") => 0,
            PageSetSize::Auto if self.id == plumb_index::places::PLACES_SET => {
                crate::places::auto_places(storage_limit_mb)
            }
            size => size.pages(storage_limit_mb),
        }
    }

    pub fn find(id: &str) -> Option<&'static SetInfo> {
        SETS.iter().find(|set| set.id == id)
    }

    /// The set a person names, by its id or by a name it had before
    /// ("subpages" for [`plumb_index::pages::SUBPAGES_SET`]). Only for what
    /// people type: a set is never served or asked for by an old name.
    pub fn named(name: &str) -> Option<&'static SetInfo> {
        let id = if name == plumb_index::pages::OLD_SUBPAGES_SET {
            plumb_index::pages::SUBPAGES_SET
        } else {
            name
        };
        SetInfo::find(id)
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
    pub(crate) fn read(&self, path: &Path, limit: u64) -> Result<impl Iterator<Item = Page>> {
        // Every set's file is an articles file (see `Page::from_set`).
        let id = self.id;
        if !Page::has_reader(id) {
            bail!("no reader for the page set {id}");
        }
        let reader = plumb_ingest::open_maybe_gz(path)?;
        let path = path.to_path_buf();
        let lines = std::io::BufRead::lines(reader).map_while(move |line| match line {
            Ok(line) => Some(line),
            Err(err) => {
                warn!("reading {}: {err}", path.display());
                None
            }
        });
        let mut bad = 0u64;
        Ok(articles_of(lines)
            .filter_map(move |(n, article)| match article {
                // Files made before fetch-pages left it out.
                Ok(article) if article.title == "Main Page" => None,
                Ok(article) => Page::from_set(id, article),
                Err(err) => {
                    bad += 1;
                    if bad <= 3 {
                        warn!("page set line {n}: {err:#}");
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
                    near: 0,
                }),
        )
    }

    /// The set's file in `data_dir` when it is whole, so it can be handed
    /// to other nodes.
    pub fn servable_file(&self, data_dir: &Path) -> Option<PathBuf> {
        if self.id == plumb_index::pages::REFERENCE2_SET && !reference_stage_ready(data_dir) {
            return None;
        }
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
    /// Past its first pages, the file holds only the places near the
    /// towns with this key ([`crate::places::near_key`]); 0 when it holds
    /// no more than its first pages.
    #[serde(default)]
    pub near: u64,
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
        .map(|set| (set, set.kept(sets, storage_limit_mb)))
        .filter(|(_, pages)| *pages > 0)
        .collect()
}

/// Writes the first `limit` pages of a set file, as its gzipped bytes are
/// written to [`SetFileCutter::write`] (gunzipped by the caller), into a
/// new gzip file.
pub struct SetFileCutter {
    limit: u64,
    lines: u64,
    /// The first bytes of the line being written.
    line_start: Vec<u8>,
    header_done: bool,
    /// Whether lines past the limit were dropped.
    cut: bool,
    out: Option<flate2::write::GzEncoder<std::io::BufWriter<std::fs::File>>>,
    /// Past the limit, the lines to keep still ([`SetFileCutter::keep_past`]).
    keep_past: Option<LineFilter>,
    /// The line past the limit being read, whole lines being needed to
    /// choose.
    pending: Vec<u8>,
    /// A metadata line at the cutoff still belongs to the last kept page.
    boundary_profiles: bool,
    kept_parent: bool,
}

/// Whether to keep a line of a set file past its first pages.
pub type LineFilter = Box<dyn Fn(&[u8]) -> bool + Send>;

impl SetFileCutter {
    pub fn create(path: &Path, limit: u64) -> Result<Self> {
        let file =
            std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
        Ok(SetFileCutter {
            limit,
            lines: 0,
            line_start: Vec::new(),
            header_done: false,
            cut: false,
            out: Some(flate2::write::GzEncoder::new(
                std::io::BufWriter::new(file),
                flate2::Compression::default(),
            )),
            keep_past: None,
            pending: Vec::new(),
            boundary_profiles: false,
            kept_parent: false,
        })
    }

    /// Past the limit, goes on through the whole file keeping the lines
    /// `keep` says to (the places near the node's towns), rather than
    /// stopping.
    pub fn keep_past(mut self, keep: LineFilter) -> Self {
        self.keep_past = Some(keep);
        self
    }

    /// Writes `line`, read past the limit, if it is kept.
    fn finish_line(&mut self, line: &[u8]) -> std::io::Result<()> {
        if is_profiles_line(line) {
            if self.kept_parent {
                if let Some(out) = self.out.as_mut() {
                    std::io::Write::write_all(out, line)?;
                }
            }
            return Ok(());
        }
        let keep = self.keep_past.as_ref().is_some_and(|keep| keep(line));
        self.kept_parent = keep;
        match self.out.as_mut() {
            Some(out) if keep && !is_profiles_line(line) => {
                std::io::Write::write_all(out, line)?;
                self.lines += 1;
            }
            _ => self.cut = true,
        }
        Ok(())
    }

    /// Pages written so far.
    pub fn pages(&self) -> u64 {
        self.lines
    }

    /// Whether it has read past the last kept page's metadata: never with
    /// [`SetFileCutter::keep_past`], which reads to the end. Reaching the
    /// article count alone must not drop that article's profiles/search.
    pub fn full(&self) -> bool {
        self.keep_past.is_none() && self.cut
    }

    /// Whether pages past the limit were dropped, so the file is not
    /// the whole set.
    pub fn cut(&self) -> bool {
        self.cut
    }

    /// Finishes the gzip file.
    pub fn finish(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.finish_line(&line)?;
        }
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
            let end = rest.iter().position(|&b| b == b'\n').map(|i| i + 1);
            let piece = &rest[..end.unwrap_or(rest.len())];
            if self.header_done && self.lines >= self.limit {
                if self.keep_past.is_none() {
                    // Buffer only enough prefix to decide whether this is
                    // metadata of the parent at the cutoff. Once decided,
                    // stream the rest without retaining a whole line.
                    let mut remaining = piece;
                    if !self.boundary_profiles {
                        let wanted = PROFILES_LINE.len().saturating_sub(self.pending.len());
                        let take = wanted.min(remaining.len());
                        self.pending.extend_from_slice(&remaining[..take]);
                        remaining = &remaining[take..];
                        if !self.kept_parent || !PROFILES_LINE.as_bytes().starts_with(&self.pending)
                        {
                            self.cut = true;
                            self.pending.clear();
                            break;
                        }
                        if self.pending.len() < PROFILES_LINE.len() {
                            return Ok(buf.len());
                        }
                        if let Some(out) = self.out.as_mut() {
                            out.write_all(&self.pending)?;
                        }
                        self.pending.clear();
                        self.boundary_profiles = true;
                    }
                    if let Some(out) = self.out.as_mut() {
                        out.write_all(remaining)?;
                    }
                    if end.is_some() {
                        self.boundary_profiles = false;
                    }
                    rest = &rest[piece.len()..];
                    continue;
                }
                // Past the limit with keep_past: whole lines, then choose.
                self.pending.extend_from_slice(piece);
                if end.is_some() {
                    let line = std::mem::take(&mut self.pending);
                    self.finish_line(&line)?;
                }
                rest = &rest[piece.len()..];
                continue;
            }
            let Some(out) = self.out.as_mut() else { break };
            out.write_all(piece)?;
            // A line of profiles is not a page (see `plumb_core::article`).
            // Lines can come in pieces, so their starts are kept.
            let wanted = PROFILES_LINE.len().saturating_sub(self.line_start.len());
            self.line_start
                .extend_from_slice(&piece[..wanted.min(piece.len())]);
            if end.is_some() {
                if !self.header_done {
                    self.header_done = true;
                } else if !is_profiles_line(&self.line_start) {
                    self.lines += 1;
                    self.kept_parent = true;
                }
                self.line_start.clear();
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
        // Migration is opt-in. An upgrade keeps the legacy file/settings
        // while the new generation is evaluated; it never implicitly
        // downloads a differently gated dataset.
        if set == plumb_index::pages::REFERENCE2_SET {
            return self.0.get(set).copied().unwrap_or(PageSetSize::Off);
        }
        let old = (set == plumb_index::pages::SUBPAGES_SET)
            .then(|| self.0.get(plumb_index::pages::OLD_SUBPAGES_SET))
            .flatten();
        // A size chosen for the set under its old name still holds.
        self.0.get(set).or(old).copied().unwrap_or_default()
    }

    pub fn set(&mut self, set: &str, size: PageSetSize) {
        if size == PageSetSize::Auto && set != plumb_index::pages::REFERENCE2_SET {
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
            let Some(info) = SetInfo::named(set.trim()) else {
                bail!(
                    "unknown page set {set:?}; there are: {}",
                    SETS.iter().map(|s| s.id).collect::<Vec<_>>().join(", ")
                );
            };
            sets.set(info.id, size.parse()?);
        }
        Ok(sets)
    }
}

/// Which set files a node replaces by itself when a trusted node has a
/// newer one (see `node/newer.rs`); `plumb run --set-updates`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SetUpdates {
    /// Every set and the map file, each growing at most
    /// `MAX_GROWTH_PERCENT` past this node's own file at a time.
    #[default]
    All,
    /// None: set files change only by hand (or `fetch-pages`). Files a node
    /// has none of, or too few pages of, are still taken.
    Off,
    /// Only these sets (`map` for the map file), with no limit on growth:
    /// naming a set says this machine can hold whatever it grows to.
    Only(Vec<String>),
}

impl SetUpdates {
    /// Whether `set` is updated, and whether with no limit on growth.
    pub fn allows(&self, set: &str) -> Option<bool> {
        match self {
            SetUpdates::All => Some(false),
            SetUpdates::Off => None,
            SetUpdates::Only(sets) => sets.iter().any(|s| s == set).then_some(true),
        }
    }
}

impl FromStr for SetUpdates {
    type Err = anyhow::Error;

    /// `all`, `off`, or sets separated by commas: `films,map`.
    fn from_str(text: &str) -> Result<Self> {
        Ok(match text.trim().to_ascii_lowercase().as_str() {
            "all" | "" => SetUpdates::All,
            "off" | "none" => SetUpdates::Off,
            list => {
                let mut sets = Vec::new();
                for set in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let set = match SetInfo::named(set) {
                        Some(info) => info.id,
                        None if set == "map" => set,
                        None => bail!(
                            "unknown page set {set:?}; there are: map, {}",
                            SETS.iter().map(|s| s.id).collect::<Vec<_>>().join(", ")
                        ),
                    };
                    sets.push(set.to_string());
                }
                SetUpdates::Only(sets)
            }
        })
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
        let staged = SetInfo::find(plumb_index::pages::REFERENCE2_SET).unwrap();
        let use_staged = staged.kept(sets, storage_limit_mb) > 0 && reference_stage_ready(data_dir);
        Wanted {
            sets: SETS
                .iter()
                .filter(|set| set.id != plumb_index::places::PLACES_SET)
                .filter(|set| {
                    if set.id == plumb_index::pages::REFERENCE2_SET {
                        use_staged
                    } else if set.id == plumb_index::pages::REFERENCE_SET {
                        !use_staged
                    } else {
                        true
                    }
                })
                .filter_map(|set| {
                    let pages = set.kept(sets, storage_limit_mb);
                    let file = set.file(data_dir);
                    let file = plumb_net::pages::generation_file(&file).unwrap_or(file);
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
        // Keep schema generations separate; the indexed scope fields used
        // by typed docs retrieval require a fresh index, not a new crawl.
        let mut text = String::from(plumb_index::pages::PAGE_INDEX_VERSION);
        for (set, file, pages) in &self.sets {
            let meta = std::fs::metadata(file).ok();
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            let len = meta.map_or(0, |m| m.len());
            text.push_str(&format!("|{}:{len}:{modified}:{pages}", set.id));
            if let Some(quality) = plumb_net::pages::read_quality(file, modified, len) {
                text.push_str(&format!(":generation={}", quality.generation));
            }
        }
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(text.as_bytes());
        Some(digest[..8].iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// A staged reference generation must have explicit complete/count notes.
/// Unknown legacy counts cannot establish preserved coverage. A completed
/// stage may lose at most the existing peer transfer coverage allowance
/// (10%) before replacing the legacy generation in this reader. Removing
/// or disabling the stage falls back to the retained legacy file.
fn reference_stage_ready(data_dir: &Path) -> bool {
    let staged = SetInfo::find(plumb_index::pages::REFERENCE2_SET).unwrap();
    let Some(notes) = staged.file_notes(data_dir) else {
        return false;
    };
    if !notes.complete || notes.lines == 0 || notes.lines == u64::MAX {
        return false;
    }
    let legacy = SetInfo::find(plumb_index::pages::REFERENCE_SET).unwrap();
    legacy.file_notes(data_dir).is_none_or(|old| {
        old.lines != u64::MAX && notes.lines.saturating_mul(100) >= old.lines.saturating_mul(90)
    })
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
    #[test]
    fn spanish_and_german_editions_are_explicitly_selectable_and_not_implicitly_downloaded() {
        for id in ["wikipedia-es", "wikipedia-de"] {
            let set = super::SetInfo::find(id).unwrap();
            assert_eq!(set.kept(&super::PageSets::default(), 0), 0);
            let chosen = super::PageSets::parse(&format!("{id}=25000")).unwrap();
            assert_eq!(set.kept(&chosen, 0), 25_000);
        }
    }
    use super::*;

    #[test]
    fn a_cutter_can_keep_chosen_lines_past_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("cut.tsv.gz");
        let mut cutter = SetFileCutter::create(&out, 2)
            .unwrap()
            .keep_past(Box::new(|line: &[u8]| line.starts_with(b"keep")));
        // In pieces that split lines.
        let text = b"header\na\nb\ndrop 1\nkeep 2\ndrop 3\nkeep 4";
        for piece in text.chunks(3) {
            std::io::Write::write_all(&mut cutter, piece).unwrap();
        }
        assert!(!cutter.full());
        cutter.finish().unwrap();
        assert_eq!((cutter.pages(), cutter.cut()), (4, true));
        let mut kept = String::new();
        std::io::Read::read_to_string(&mut plumb_ingest::open_maybe_gz(&out).unwrap(), &mut kept)
            .unwrap();
        assert_eq!(kept, "header\na\nb\nkeep 2\nkeep 4");
    }
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
    fn every_set_of_pages_has_a_reader() {
        for set in SETS {
            if set.id != plumb_index::places::PLACES_SET {
                assert!(Page::has_reader(set.id), "{}", set.id);
            }
        }
        assert!(!Page::has_reader("no-such-set"));
    }

    #[test]
    fn reference2_is_opt_in_and_preserves_legacy_coverage_until_ready() {
        use plumb_index::pages::{REFERENCE2_SET, REFERENCE_SET};
        let dir = tempfile::tempdir().unwrap();
        let legacy = SetInfo::find(REFERENCE_SET).unwrap();
        let staged = SetInfo::find(REFERENCE2_SET).unwrap();
        std::fs::create_dir_all(sets_dir(dir.path())).unwrap();
        std::fs::write(legacy.file(dir.path()), b"legacy").unwrap();
        std::fs::write(staged.file(dir.path()), b"staged").unwrap();
        let defaults = PageSets::default();
        assert_eq!(defaults.size(REFERENCE2_SET), PageSetSize::Off);
        assert_eq!(defaults.size(REFERENCE_SET), PageSetSize::Auto);
        let mut automatic = PageSets::default();
        automatic.set(REFERENCE2_SET, PageSetSize::Auto);
        assert_eq!(automatic.size(REFERENCE2_SET), PageSetSize::Auto);
        let old_settings: PageSets = serde_json::from_str(r#"{"reference":"all"}"#).unwrap();
        assert_eq!(old_settings.size(REFERENCE_SET), PageSetSize::All);
        assert_eq!(old_settings.size(REFERENCE2_SET), PageSetSize::Off);
        assert_eq!(SetInfo::named("reference").unwrap().id, REFERENCE_SET);
        assert_eq!(SetInfo::named("reference2").unwrap().id, REFERENCE2_SET);
        let mut settings = old_settings;
        settings.set(REFERENCE2_SET, PageSetSize::All);
        let selected = || {
            Wanted::new(dir.path(), &settings, 0)
                .sets
                .into_iter()
                .map(|(set, _, _)| set.id)
                .collect::<Vec<_>>()
        };
        let notes = |set: &SetInfo, lines: u64, complete: bool| {
            std::fs::write(
                notes_path(&set.file(dir.path())),
                serde_json::to_vec(&SetFileNotes {
                    lines,
                    complete,
                    source_modified: 0,
                    fetched_at: 0,
                    near: 0,
                })
                .unwrap(),
            )
            .unwrap();
        };
        // A bare renamed file does not establish completion or coverage.
        assert_eq!(selected(), [REFERENCE_SET]);
        assert!(staged.servable_file(dir.path()).is_none());
        assert!(legacy.servable_file(dir.path()).is_some());
        notes(legacy, 1_000, true);
        notes(staged, 1_000, false);
        assert_eq!(selected(), [REFERENCE_SET]);
        notes(staged, 899, true);
        assert_eq!(selected(), [REFERENCE_SET]);
        assert!(staged.servable_file(dir.path()).is_none());
        notes(staged, 900, true);
        assert_eq!(selected(), [REFERENCE2_SET]);
        assert!(staged.servable_file(dir.path()).is_some());
        // New name is served separately; legacy bytes remain available to
        // older readers and rollback. Never index both generations.
        assert!(legacy.file(dir.path()).is_file());
        std::fs::remove_file(staged.file(dir.path())).unwrap();
        assert_eq!(selected(), [REFERENCE_SET]);
    }

    #[test]
    fn cutting_keeps_the_top_pages() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let mut text = ARTICLES_HEADER.as_bytes().to_vec();
        for (title, views) in [("A", 3u64), ("B", 2), ("C", 1)] {
            // A line of profiles after an article is not a page.
            let profiles = vec![plumb_core::profiles::Profile {
                service: "x".into(),
                id: format!("account{title}"),
            }];
            write_article(
                &mut text,
                &Article {
                    title: title.into(),
                    item: Some(format!("Q{views}")),
                    views,
                    profiles,
                    sections: vec![format!("Section {title}")],
                    search: Some(plumb_core::article::SearchContent {
                        symbols: vec![plumb_core::article::SearchSymbol {
                            identifier: format!("symbol_{title}"),
                            anchor: Some(format!("anchor-{title}")),
                        }],
                        ..plumb_core::article::SearchContent::default()
                    }),
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
        assert_eq!(back[0].profiles[0].id, "accountA");
        assert_eq!(back[1].profiles[0].id, "accountB");
        assert_eq!(back[1].sections, ["Section B"]);
        assert_eq!(
            back[1].search.as_ref().unwrap().symbols[0].identifier,
            "symbol_B"
        );
    }

    #[test]
    fn cutoff_metadata_stays_with_its_parent_at_every_byte_boundary() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let text = b"views\ttitle\tdescription\titem\tsite\taliases\n1\tA\t\turlA\t\t\nprofiles\turlA\tsection=Methods|search={\"version\":1,\"symbols\":[{\"identifier\":\"exact_symbol\"}]}\n1\tB\t\turlB\t\t\nprofiles\turlB\tsection=Excluded\n";
        for size in [1, 2, 7, 9, 64, text.len()] {
            let out = dir.path().join(format!("cut-{size}.tsv.gz"));
            let mut cutter = SetFileCutter::create(&out, 1).unwrap();
            for piece in text.chunks(size) {
                cutter.write_all(piece).unwrap();
                if cutter.full() {
                    break;
                }
            }
            cutter.finish().unwrap();
            let back =
                plumb_core::article::read_articles(plumb_ingest::open_maybe_gz(&out).unwrap(), 10)
                    .unwrap();
            assert_eq!(back.len(), 1, "chunks of {size}");
            assert_eq!(back[0].sections, ["Methods"]);
            assert_eq!(
                back[0].search.as_ref().unwrap().symbols[0].identifier,
                "exact_symbol"
            );
        }
    }

    #[test]
    fn selective_cutters_keep_metadata_only_with_selected_parents() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("selected.tsv.gz");
        let mut text = ARTICLES_HEADER.as_bytes().to_vec();
        for (title, views) in [("A", 3), ("B", 1), ("C", 2)] {
            write_article(
                &mut text,
                &Article {
                    title: title.into(),
                    item: Some(format!("url{title}")),
                    views,
                    sections: vec![format!("Section {title}")],
                    search: Some(plumb_core::article::SearchContent {
                        symbols: vec![plumb_core::article::SearchSymbol {
                            identifier: format!("symbol_{title}"),
                            anchor: None,
                        }],
                        ..plumb_core::article::SearchContent::default()
                    }),
                    ..Article::default()
                },
            )
            .unwrap();
        }
        let mut cutter = SetFileCutter::create(&out, 1)
            .unwrap()
            .keep_past(Box::new(|line| line.starts_with(b"2\t")));
        for piece in text.chunks(3) {
            cutter.write_all(piece).unwrap();
        }
        cutter.finish().unwrap();
        assert_eq!(cutter.pages(), 2);
        let back =
            plumb_core::article::read_articles(plumb_ingest::open_maybe_gz(&out).unwrap(), 10)
                .unwrap();
        assert_eq!(
            back.iter().map(|a| a.title.as_str()).collect::<Vec<_>>(),
            ["A", "C"]
        );
        assert_eq!(back[1].sections, ["Section C"]);
        assert_eq!(
            back[1].search.as_ref().unwrap().symbols[0].identifier,
            "symbol_C"
        );
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
        // The subpages set's old name still names it, and a size saved
        // under it still holds.
        let subpages = plumb_index::pages::SUBPAGES_SET;
        let old = PageSets::parse("subpages=off").unwrap();
        assert_eq!(old.size(subpages), PageSetSize::Off);
        let saved: PageSets = serde_json::from_str(r#"{"subpages":"off"}"#).unwrap();
        assert_eq!(saved.size(subpages), PageSetSize::Off);
        assert_eq!(
            "subpages".parse::<SetUpdates>().unwrap(),
            SetUpdates::Only(vec![subpages.to_string()])
        );
        // Never under the old name otherwise.
        assert!(SetInfo::find("subpages").is_none());
        assert_eq!(SetInfo::named("subpages").unwrap().id, subpages);
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
    fn other_stack_exchange_questions_are_searched_by_their_words() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        write_set(data, &[("Drain", 5_000)]);
        let file = SetInfo::find("stackexchange").unwrap().file(data);
        let mut text = ARTICLES_HEADER.as_bytes().to_vec();
        for (title, item) in [
            (
                "How can I unclog a bathroom sink drain?",
                "diy.stackexchange.com/2142",
            ),
            // A site Plumb doesn't know is left out.
            ("How can I unclog a drain fast?", "evil.example/1"),
        ] {
            write_article(
                &mut text,
                &Article {
                    title: title.to_string(),
                    description: Some("plumbing, drain, clog".to_string()),
                    item: Some(item.to_string()),
                    views: 400_000,
                    ..Article::default()
                },
            )
            .unwrap();
        }
        std::fs::write(&file, text).unwrap();
        let wanted = Wanted::new(data, &PageSets::default(), 0);
        let (_, searcher) = open_or_build(data, &wanted).unwrap().unwrap();
        let hits = searcher.search("how to unclog a drain", 5).unwrap();
        let questions: Vec<_> = hits
            .iter()
            .filter(|h| h.page.set == "stackexchange")
            .collect();
        assert_eq!(questions.len(), 1, "{hits:#?}");
        let question = questions[0];
        assert_eq!(
            question.page.url,
            "https://diy.stackexchange.com/questions/2142"
        );
        assert_eq!(question.page.set_name(), "Home Improvement");
        assert_eq!(question.page.set_domain(), "diy.stackexchange.com");
        assert_eq!(question.page.language(), Some("en"));
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
