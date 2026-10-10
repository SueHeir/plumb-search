//! The short names papers are known by ("bert paper", "ppo paper",
//! "graphsage"), and the well-known arXiv papers OpenAlex lacks, for the
//! papers set ([`crate::openalex`]).
//!
//! Names come from two places:
//! - A paper's own title, when it starts with a name: "BERT: Pre-training
//!   of ..." is BERT, "U-Net: Convolutional ..." U-Net. Only a name one
//!   paper of the set starts with ("COVID-19: ..." starts thousands).
//! - The methods of Papers with Code (archived on Hugging Face as
//!   `pwc-archive/methods`, CC BY-SA 4.0): "PPO" is introduced in
//!   "Proximal Policy Optimization Algorithms", "Transformer" in
//!   "Attention Is All You Need". Methods are matched to papers by their
//!   arXiv id or their title. The archive also holds spam, so only
//!   methods whose paper is the one their source names, with a plain
//!   name, used by a few papers, are kept. A name given to more than one
//!   paper stays with the most cited.
//!
//! The papers those methods come from that the set lacks (OpenAlex has no
//! arXiv record of "Proximal Policy Optimization Algorithms") are added
//! from arXiv's API (its metadata is CC0). Method-use counts remain
//! explicitly distinct from citations. Existing identities can be verified
//! in bounded batches; title matches need author corroboration and
//! cannot replace a legitimate later publication's DOI or date.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};
use plumb_core::normalize_text;
use plumb_core::papers::{
    valid_date, ArxivVerification, PaperCorrection, PaperCountKind, PaperMetadata,
    MAX_PAPER_AUTHORS, MAX_PAPER_CORRECTIONS,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::openalex::ARXIV_DOI;

/// Most names kept of a paper.
pub const MAX_NAMES: usize = 4;
/// Fewest papers using a method for its name to be kept.
pub const MIN_METHOD_PAPERS: u64 = 3;
/// Longest name kept, in characters and in words.
const MAX_NAME_CHARS: usize = 40;
const MAX_NAME_WORDS: usize = 5;
/// Longest name a title may start with, in words.
const MAX_TITLE_NAME_WORDS: usize = 3;

/// Papers with Code's methods, as Hugging Face's dataset viewer serves
/// them, a page of [`METHODS_PAGE`] at a time.
const METHODS_URL: &str = "https://datasets-server.huggingface.co/rows?dataset=pwc-archive/methods&config=default&split=train";
const METHODS_PAGE: usize = 100;
/// Pause between requests to Hugging Face.
const METHODS_PAUSE: Duration = Duration::from_millis(500);
/// The methods kept in --work, as one JSON line each.
const METHODS_FILE: &str = "pwc-methods.jsonl";

/// arXiv's API, asked for [`ARXIV_IDS_A_REQUEST`] papers at a time, a
/// request every [`ARXIV_PAUSE`] as its terms ask.
const ARXIV_URL: &str = "https://export.arxiv.org/api/query";
const ARXIV_IDS_A_REQUEST: usize = 100;
const ARXIV_PAUSE: Duration = Duration::from_secs(3);

/// One method of Papers with Code, the fields used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Method {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub paper: Option<MethodPaper>,
    #[serde(default)]
    pub source_url: Option<String>,
    #[serde(default)]
    pub source_title: Option<String>,
    #[serde(default)]
    pub num_papers: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodPaper {
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RowsPage {
    #[serde(default)]
    rows: Vec<Row>,
    #[serde(default)]
    num_rows_total: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct Row {
    row: Method,
}

/// A name as a paper may be called by: a few plain words, with a letter.
fn plain_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name.split_whitespace().count() <= MAX_NAME_WORDS
        && name.chars().any(char::is_alphabetic)
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || " -+.'&()/".contains(c))
}

/// Whether `name` reads as a coined name rather than words ("BERT",
/// "GraphSAGE", "word2vec", "U-Net", not "Pruning" or "Layer
/// Normalization"): a word of it has a capital past its first letter, or
/// letters and a digit.
fn coined(name: &str) -> bool {
    name.split_whitespace().any(|word| {
        let letters: Vec<char> = word.chars().filter(|c| c.is_alphanumeric()).collect();
        letters.len() >= 2
            && (letters.iter().skip(1).any(|c| c.is_uppercase())
                || letters.iter().any(|c| c.is_ascii_digit())
                    && letters.iter().any(|c| c.is_alphabetic()))
    })
}

/// The arXiv id (`1707.06347`, `hep-th/9711200`) of an arXiv address,
/// without its version.
pub fn arxiv_id_of(url: &str) -> Option<String> {
    let rest = url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.")
        .trim_start_matches("export.")
        .strip_prefix("arxiv.org/")?;
    let id = rest
        .strip_prefix("abs/")
        .or_else(|| rest.strip_prefix("pdf/"))?
        .trim_end_matches(".pdf");
    let id = match id.rfind('v') {
        Some(i) if i + 1 < id.len() && id[i + 1..].bytes().all(|b| b.is_ascii_digit()) => &id[..i],
        _ => id,
    };
    (!id.is_empty()
        && id.bytes().any(|b| b.is_ascii_digit())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/' | b'-')))
    .then(|| id.to_ascii_lowercase())
}

impl Method {
    /// The method's paper's title, when the method is one to go by: its
    /// paper is the one its source names, its name is plain and a few
    /// papers use it.
    fn trusted_title(&self) -> Option<&str> {
        let title = self.paper.as_ref()?.title.as_deref()?.trim();
        let source = self.source_title.as_deref()?.trim();
        (!title.is_empty()
            && title.eq_ignore_ascii_case(source)
            && self
                .source_url
                .as_deref()
                .is_some_and(|u| !u.trim().is_empty())
            && plain_name(&self.name)
            && self.num_papers.unwrap_or(0) >= MIN_METHOD_PAPERS)
            .then_some(title)
    }

    /// The names the method gives its paper, when its name is coined
    /// ([`coined`]; "Pruning" or "Focus" would name too much): its name,
    /// its full name when of a few words ("Proximal Policy
    /// Optimization"), and its name without a version ("YOLO" of
    /// "YOLOv1", "VGG" of "VGG-19").
    fn names(&self, title: &str) -> Vec<String> {
        if !coined(self.name.trim()) {
            return Vec::new();
        }
        let mut names = vec![self.name.trim().to_string()];
        if let Some(full) = self.full_name.as_deref().map(str::trim) {
            if plain_name(full)
                && full.split_whitespace().count() >= 2
                && !full.eq_ignore_ascii_case(title)
            {
                names.push(full.to_string());
            }
        }
        if let Some(base) = without_version(self.name.trim()) {
            names.push(base.to_string());
        }
        names
    }
}

/// `name` without a version at its end ("YOLOv1", "VGG-19",
/// "Inception-v3"), when what is left is a name of two letters or more.
fn without_version(name: &str) -> Option<&str> {
    let digits = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if digits.len() == name.len() {
        return None;
    }
    let base = digits
        .strip_suffix(['v', 'V'])
        .unwrap_or(digits)
        .trim_end_matches(['-', ' ', '.']);
    (base.chars().filter(|c| c.is_alphabetic()).count() >= 2
        && base.chars().last().is_some_and(char::is_alphabetic))
    .then_some(base)
}

/// The name a title starts with, if any: "BERT" of "BERT: Pre-training of
/// Deep Bidirectional Transformers", a few words before a colon that read
/// as a name (with a capital past their first letter, or a digit), not
/// "Review: ..." or "Part I: ...".
pub fn title_name(title: &str) -> Option<&str> {
    let (name, rest) = title.split_once(':')?;
    let name = name.trim();
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.is_empty()
        || words.len() > MAX_TITLE_NAME_WORDS
        || rest.split_whitespace().count() < 2
        || !plain_name(name)
    {
        return None;
    }
    let named = coined(name);
    // A roman numeral ("Part II") is no name.
    let numeral = words
        .last()
        .is_some_and(|w| w.chars().all(|c| matches!(c, 'I' | 'V' | 'X')));
    (named && !numeral).then_some(name)
}

/// One paper as arXiv gives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArxivPaper {
    /// Without a version: `1707.06347`.
    pub id: String,
    pub title: String,
    pub year: Option<i32>,
    pub authors: Vec<String>,
    pub published: Option<String>,
    pub updated: Option<String>,
}

impl ArxivPaper {
    pub(crate) fn complete(&self) -> bool {
        arxiv_id_of(&format!("https://arxiv.org/abs/{}", self.id)).as_deref()
            == Some(self.id.as_str())
            && !self.title.trim().is_empty()
            && self.title.chars().count() <= 2000
            && !self.authors.is_empty()
            && self.authors.iter().all(|a| !a.trim().is_empty())
            && self.published.as_deref().is_some_and(valid_date)
            && self.year == self.published.as_deref().and_then(|d| d[..4].parse().ok())
            && self
                .updated
                .as_deref()
                .is_none_or(|d| valid_date(d) && self.published.as_deref().is_some_and(|p| d >= p))
            && self.metadata(0, PaperCountKind::Unknown).write().is_some()
    }

    pub fn metadata(&self, count: u64, count_kind: PaperCountKind) -> PaperMetadata {
        PaperMetadata {
            verified_arxiv: self.published.as_ref().map(|submitted| ArxivVerification {
                id: self.id.clone(),
                title: self.title.clone(),
                authors: self
                    .authors
                    .iter()
                    .take(MAX_PAPER_AUTHORS)
                    .cloned()
                    .collect(),
                submitted: submitted.clone(),
                updated: self.updated.clone(),
            }),
            doi: Some(self.doi()),
            arxiv_id: Some(self.id.clone()),
            authors: self
                .authors
                .iter()
                .take(MAX_PAPER_AUTHORS)
                .cloned()
                .collect(),
            publication_date: self.published.clone(),
            raw_publication_date: self.published.clone(),
            publication_year: self.year,
            preprint_date: self.published.clone(),
            version_date: self.updated.clone(),
            preprint_version_date: self.updated.clone(),
            venue: Some("arXiv".into()),
            source: "arxiv".into(),
            count,
            count_kind,
            alternate_urls: vec![format!("https://arxiv.org/abs/{}", self.id)],
            ..PaperMetadata::default()
        }
    }
    /// The paper's DOI as OpenAlex writes arXiv's, lowercased.
    pub fn doi(&self) -> String {
        format!("{ARXIV_DOI}{}", self.id)
    }

    /// "Paper by AUTHOR et al., YEAR, arXiv", as [`crate::openalex`]
    /// describes a paper.
    fn description(&self) -> String {
        let mut description = "Paper".to_string();
        if let Some(first) = self.authors.first() {
            description.push_str(" by ");
            description.push_str(first);
            if self.authors.len() > 1 {
                description.push_str(" et al.");
            }
        }
        if let Some(year) = self.year {
            description.push_str(&format!(", {year}"));
        }
        description.push_str(", arXiv");
        plumb_core::truncate_chars(
            &plumb_core::collapse_whitespace(&description),
            MAX_ARTICLE_DESCRIPTION_CHARS,
        )
    }
}

/// The text of the first `<tag>` element in `xml`, entities decoded.
fn element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let after = &xml[start + open.len()..];
    // `<title>` and not `<titlefoo>`.
    if !after.starts_with(['>', ' ']) {
        return None;
    }
    let body = &after[after.find('>')? + 1..];
    let end = body.find(&format!("</{tag}>"))?;
    Some(plumb_core::collapse_whitespace(
        &crate::stackexchange::decode_entities(&body[..end]),
    ))
}

/// The papers of an arXiv API answer (an Atom feed).
pub fn parse_arxiv_feed(xml: &str) -> Vec<ArxivPaper> {
    xml.split("<entry>")
        .skip(1)
        .filter_map(|entry| {
            let entry = entry.split("</entry>").next()?;
            let id = arxiv_id_of(&element(entry, "id")?)?;
            let title = element(entry, "title").filter(|t| !t.is_empty())?;
            let date = |tag| {
                element(entry, tag)
                    .and_then(|p| p.get(..10).map(str::to_string))
                    .filter(|p| valid_date(p))
            };
            let published = date("published");
            let updated = date("updated");
            let year = published.as_deref().and_then(|p| p[..4].parse().ok());
            let authors = entry
                .split("<author>")
                .skip(1)
                .filter_map(|author| element(author, "name"))
                .filter(|name| !name.is_empty())
                .collect();
            Some(ArxivPaper {
                id,
                title,
                year,
                authors,
                published,
                updated,
            })
        })
        .collect()
}

/// The year of a paper of the set, from its description ("Paper by A et
/// al., 2017, Venue").
pub(crate) fn year_of(paper: &Article) -> Option<i32> {
    if let Some(year) = paper.paper.as_ref().and_then(|m| m.publication_year) {
        return Some(year);
    }
    paper
        .description
        .as_deref()?
        .split(", ")
        .skip(1)
        .find(|part| part.len() == 4 && part.bytes().all(|b| b.is_ascii_digit()))?
        .parse()
        .ok()
}

/// The arXiv id of a paper of the set: from its arXiv DOI, or its free
/// copy on arXiv.
pub(crate) fn paper_arxiv_id(paper: &Article) -> Option<String> {
    if let Some(item) = paper.item.as_deref() {
        if let Some(id) = item.to_ascii_lowercase().strip_prefix(ARXIV_DOI) {
            return Some(id.to_string());
        }
    }
    paper
        .paper
        .as_ref()
        .and_then(|m| m.arxiv_id.clone())
        .or_else(|| paper.website.as_deref().and_then(arxiv_id_of))
}

/// What [`name_papers`] and [`add_arxiv_papers`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Named {
    /// Papers named by the start of their title.
    pub by_title: usize,
    /// Papers named by a method.
    pub by_method: usize,
    /// arXiv papers added.
    pub added: usize,
    /// Retained for report compatibility; generic linking never redates journals.
    pub redated: usize,
    /// Existing primary arXiv identities with corrected metadata.
    pub corrected: usize,
    /// Source identities whose responses/links were ambiguous or incomplete.
    pub unresolved: Vec<String>,
    /// Journal/publication IDs linked to a preprint while retaining their
    /// original DOI and publication dates. arXiv cannot adjudicate those dates.
    pub retained_publications: Vec<String>,
    /// Untouched, unverified legacy source-ID variants; no canonical choice.
    pub legacy_variants: Vec<crate::paper_validation::LegacyVariants>,
}

/// Where papers are, by arXiv id and by title.
struct Found {
    by_arxiv: HashMap<String, usize>,
    by_title: HashMap<String, usize>,
}

impl Found {
    fn of(papers: &[Article]) -> Self {
        let mut by_arxiv = HashMap::new();
        let mut by_title: HashMap<String, usize> = HashMap::new();
        for (i, paper) in papers.iter().enumerate() {
            if let Some(id) = paper_arxiv_id(paper) {
                by_arxiv.entry(id).or_insert(i);
            }
            // The most cited of the papers of a title.
            let key = normalize_text(&paper.title);
            match by_title.get(&key) {
                Some(&j) if papers[j].views >= paper.views => {}
                _ => {
                    by_title.insert(key, i);
                }
            }
        }
        Found { by_arxiv, by_title }
    }

    /// The paper a method is of: by the arXiv id of its source, else by
    /// its paper's title.
    fn of_method(&self, method: &Method, title: &str) -> Option<usize> {
        method
            .source_url
            .as_deref()
            .and_then(arxiv_id_of)
            .and_then(|id| self.by_arxiv.get(&id).copied())
            .or_else(|| self.by_title.get(&normalize_text(title)).copied())
    }
}

/// Gives `papers` the names they start their titles with and those of
/// `methods` (see the module docs), at most [`MAX_NAMES`] each, after
/// those they have.
pub fn name_papers(papers: &mut [Article], methods: &[Method]) -> Named {
    name_papers_excluding(papers, methods, &HashSet::new())
}

fn name_papers_excluding(
    papers: &mut [Article],
    methods: &[Method],
    preserved_ids: &HashSet<String>,
) -> Named {
    let mut named = Named::default();
    // Names given, with the paper given each and its citations.
    let mut given: HashMap<String, (usize, u64)> = HashMap::new();
    let mut names: HashMap<usize, Vec<(u64, String)>> = HashMap::new();
    let mut give = |name: &str, paper: usize, views: u64, weight: u64| {
        if papers[paper]
            .item
            .as_ref()
            .is_some_and(|id| preserved_ids.contains(&id.to_ascii_lowercase()))
        {
            return;
        }
        let key = normalize_text(name);
        if key.is_empty() {
            return;
        }
        match given.get(&key) {
            Some(&(_, most)) if most >= views => {}
            _ => {
                given.insert(key.clone(), (paper, views));
            }
        }
        names
            .entry(paper)
            .or_default()
            .push((weight, name.to_string()));
    };
    // Title names, only those one paper starts with.
    let mut starts: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, paper) in papers.iter().enumerate() {
        if let Some(name) = title_name(&paper.title) {
            starts.entry(normalize_text(name)).or_default().push(i);
        }
    }
    for (i, paper) in papers.iter().enumerate() {
        let Some(name) = title_name(&paper.title) else {
            continue;
        };
        if starts
            .get(&normalize_text(name))
            .is_some_and(|p| p.len() == 1)
        {
            give(name, i, paper.views, u64::MAX);
        }
    }
    let found = Found::of(papers);
    for method in methods {
        let Some(title) = method.trusted_title() else {
            continue;
        };
        let Some(i) = found.of_method(method, title) else {
            continue;
        };
        // A source ID cannot license a method's unrelated archived title.
        if normalize_text(title) != normalize_text(&papers[i].title) {
            continue;
        }
        for name in method.names(&papers[i].title) {
            give(&name, i, papers[i].views, method.num_papers.unwrap_or(0));
        }
    }
    let mut by_title: HashSet<usize> = HashSet::new();
    for (i, mut list) in names {
        // The most used first, and a method's own name before the rest.
        list.sort_by_key(|(weight, _)| std::cmp::Reverse(*weight));
        let paper = &mut papers[i];
        let mut said: Vec<String> = std::iter::once(&paper.title)
            .chain(&paper.aliases)
            .map(|name| normalize_text(name))
            .collect();
        let before = paper.aliases.len();
        for (weight, name) in list {
            let key = normalize_text(&name);
            // A name given to more than one paper stays with the most
            // cited.
            if said.contains(&key) || given.get(&key).is_some_and(|&(to, _)| to != i) {
                continue;
            }
            if paper.aliases.len() >= (before + MAX_NAMES).min(plumb_core::article::MAX_ALIASES) {
                break;
            }
            said.push(key);
            paper.aliases.push(name);
            if weight == u64::MAX {
                by_title.insert(i);
            }
        }
        if paper.aliases.len() > before && !by_title.contains(&i) {
            named.by_method += 1;
        }
    }
    named.by_title = by_title.len();
    named
}

/// The arXiv ids of the papers of `methods` that `papers` lack, each with
/// the most papers using one of its methods.
pub fn missing_arxiv_ids(papers: &[Article], methods: &[Method]) -> Vec<(String, u64)> {
    let found = Found::of(papers);
    let mut missing: HashMap<String, u64> = HashMap::new();
    for method in methods {
        if method.trusted_title().is_none() {
            continue;
        }
        let Some(id) = method.source_url.as_deref().and_then(arxiv_id_of) else {
            continue;
        };
        if found.by_arxiv.contains_key(&id) {
            continue;
        }
        let uses = missing.entry(id).or_default();
        *uses = (*uses).max(method.num_papers.unwrap_or(0));
    }
    let mut missing: Vec<(String, u64)> = missing.into_iter().collect();
    missing.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    missing
}

/// Legacy six-column rows are migrated once; all newly fetched rows carry
/// structured authors and dates. Unknown days are left unknown.
pub(crate) fn metadata_of(paper: &Article) -> PaperMetadata {
    paper.paper.clone().unwrap_or_else(|| PaperMetadata {
        doi: paper
            .item
            .as_ref()
            .filter(|item| item.starts_with("10."))
            .cloned(),
        openalex_id: paper
            .item
            .as_ref()
            .filter(|item| item.starts_with('W'))
            .cloned(),
        arxiv_id: paper_arxiv_id(paper),
        authors: paper
            .description
            .as_deref()
            .and_then(|d| d.strip_prefix("Paper by "))
            .map(|d| {
                d.split(" et al.")
                    .next()
                    .unwrap_or(d)
                    .split(", ")
                    .next()
                    .unwrap_or(d)
                    .to_string()
            })
            .into_iter()
            .collect(),
        publication_year: year_of(paper),
        source: "legacy-paper-row".into(),
        count: paper.views,
        ..PaperMetadata::default()
    })
}

pub(crate) fn same_author(a: &str, b: &str) -> bool {
    // normalize_text joins consecutive initials (J. A. -> ja), which
    // is useful for search names but loses author-name token boundaries.
    let tokens = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric() && !matches!(c, '\'' | '’'))
            .map(normalize_text)
            .filter(|word| !word.is_empty())
            .collect()
    };
    let a = tokens(a);
    let b = tokens(b);
    if a == b && !a.is_empty() {
        return true;
    }
    a.len() >= 2
        && b.len() >= 2
        && a.last() == b.last()
        && a[..a.len() - 1]
            .iter()
            .zip(&b[..b.len() - 1])
            .all(|(a, b)| {
                a == b
                    || (a.chars().count() == 1 || b.chars().count() == 1)
                        && a.chars().next() == b.chars().next()
            })
}

fn corroborated(paper: &Article, arxiv: &ArxivPaper) -> bool {
    metadata_of(paper)
        .authors
        .first()
        .zip(arxiv.authors.first())
        .is_some_and(|(a, b)| same_author(a, b))
}

fn correction(paper: &Article, reason: &str, arxiv: &ArxivPaper) -> PaperCorrection {
    let old = metadata_of(paper);
    PaperCorrection {
        reason: reason.into(),
        source_url: format!("https://arxiv.org/abs/{}", arxiv.id),
        previous_item: paper.item.clone(),
        previous_title: paper.title.clone(),
        previous_description: paper.description.clone(),
        previous_authors: old.authors,
        previous_publication_date: old.publication_date,
        previous_raw_publication_date: old.raw_publication_date,
        previous_publication_year: old.publication_year,
    }
}

/// Stable identifiers use the existing exact-name index, without making
/// an unrelated former title searchable. Reserve at most three aliases.
fn add_identifiers(paper: &mut Article, arxiv: &ArxivPaper) {
    let mut identifiers = vec![arxiv.id.clone(), format!("arXiv:{}", arxiv.id), arxiv.doi()];
    let keys: Vec<String> = identifiers.iter().map(|id| normalize_text(id)).collect();
    identifiers.extend(
        paper
            .aliases
            .iter()
            .filter(|a| !keys.contains(&normalize_text(a)))
            .take(plumb_core::article::MAX_ALIASES - identifiers.len())
            .cloned(),
    );
    paper.aliases = identifiers;
}

fn canonicalize(paper: &mut Article, arxiv: &ArxivPaper, reason: &str) -> bool {
    let old = metadata_of(paper);
    let canonical_authors: Vec<String> = arxiv
        .authors
        .iter()
        .take(MAX_PAPER_AUTHORS)
        .cloned()
        .collect();
    let changed = paper.title != arxiv.title
        || old.authors != canonical_authors
        || old.publication_date != arxiv.published
        || old.publication_year != arxiv.year
        || paper.item.as_deref() != Some(arxiv.doi().as_str());
    let mut metadata = arxiv.metadata(old.count, old.count_kind.clone());
    metadata.openalex_id = old.openalex_id;
    metadata.corrections = old.corrections.clone();
    if changed && metadata.corrections.len() < MAX_PAPER_CORRECTIONS {
        metadata.corrections.push(correction(paper, reason, arxiv));
    }
    if normalize_text(&paper.title) != normalize_text(&arxiv.title) {
        // Names attached to an unrelated title are not evidence for this work.
        paper.aliases.clear();
        paper.names.clear();
    }
    paper.title = arxiv.title.clone();
    paper.item = Some(arxiv.doi());
    paper.description = Some(arxiv.description());
    paper.website = None;
    paper.paper = Some(metadata);
    add_identifiers(paper, arxiv);
    changed
}

pub(crate) fn unique_sources(found: &[ArxivPaper]) -> HashMap<&str, &ArxivPaper> {
    let mut counts = HashMap::<&str, usize>::new();
    for source in found {
        *counts.entry(&source.id).or_default() += 1;
    }
    found
        .iter()
        .filter(|s| counts[s.id.as_str()] == 1 && s.complete())
        .map(|s| (s.id.as_str(), s))
        .collect()
}

fn unresolved(done: &mut Named, id: &str) {
    if !done.unresolved.iter().any(|old| old == id) && done.unresolved.len() < 1000 {
        done.unresolved.push(id.to_string());
    }
}

fn link_and_report(done: &mut Named, paper: &mut Article, source: &ArxivPaper) {
    if let Some(item) = &paper.item {
        if done.retained_publications.len() < 1000 && !done.retained_publications.contains(item) {
            done.retained_publications.push(item.clone());
        }
    }
    link_preprint(paper, source);
}

/// Updates every primary record of a confirmed arXiv identity, including
/// duplicates with a corrupt title. Journal rows linked to a preprint keep
/// their independent publication metadata and require corroboration.
pub fn verify_existing(papers: &mut [Article], found: &[ArxivPaper]) -> Named {
    let by_id = unique_sources(found);
    let mut done = Named::default();
    for paper in papers {
        let Some(id) = paper_arxiv_id(paper) else {
            continue;
        };
        let Some(arxiv) = by_id.get(id.as_str()) else {
            if found.iter().any(|p| p.id == id) {
                unresolved(&mut done, &id);
            }
            continue;
        };
        let old = metadata_of(paper);
        if old
            .verified_arxiv
            .as_ref()
            .is_some_and(|proof| proof.id != id)
            || old.arxiv_id.as_ref().is_some_and(|other| other != &id)
            || old
                .doi
                .as_ref()
                .zip(paper.item.as_ref())
                .is_some_and(|(a, b)| !a.eq_ignore_ascii_case(b))
            || paper
                .website
                .as_deref()
                .and_then(arxiv_id_of)
                .is_some_and(|other| other != id)
        {
            unresolved(&mut done, &id);
            continue;
        }
        if paper
            .item
            .as_deref()
            .is_some_and(|item| item.eq_ignore_ascii_case(&arxiv.doi()))
        {
            done.corrected += usize::from(canonicalize(
                paper,
                arxiv,
                "verified-arxiv-identity-conflict",
            ));
        } else if normalize_text(&paper.title) == normalize_text(&arxiv.title)
            && corroborated(paper, arxiv)
        {
            link_and_report(&mut done, paper, arxiv);
        } else {
            unresolved(&mut done, &id);
        }
    }
    done
}

fn link_preprint(paper: &mut Article, arxiv: &ArxivPaper) {
    let abs = format!("https://arxiv.org/abs/{}", arxiv.id);
    if paper.website.is_none() {
        paper.website = Some(abs.clone());
    }
    let mut metadata = metadata_of(paper);
    metadata.verified_arxiv = arxiv.metadata(0, PaperCountKind::Unknown).verified_arxiv;
    metadata.arxiv_id = Some(arxiv.id.clone());
    metadata.preprint_date = arxiv.published.clone();
    metadata.preprint_version_date = arxiv.updated.clone();
    if !metadata.alternate_urls.contains(&abs) && metadata.alternate_urls.len() < 8 {
        metadata.alternate_urls.push(abs);
    }
    paper.paper = Some(metadata);
    add_identifiers(paper, arxiv);
}

/// Adds complete, uniquely verified arXiv identities. A unique title and
/// compatible author can link a journal row to its preprint, but never
/// replaces the journal's primary source ID, publication date or version.
pub fn add_arxiv_papers(
    papers: &mut Vec<Article>,
    found: &[ArxivPaper],
    uses: &HashMap<String, u64>,
) -> Named {
    let mut named = verify_existing(papers, found);
    let sources = unique_sources(found);
    for source in found {
        if !sources.contains_key(source.id.as_str()) {
            unresolved(&mut named, &source.id);
        }
    }
    let at = Found::of(papers);
    let mut by_title: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, paper) in papers.iter().enumerate() {
        by_title
            .entry(normalize_text(&paper.title))
            .or_default()
            .push(i);
    }
    let mut present: HashSet<String> = at.by_arxiv.keys().cloned().collect();
    for arxiv in found.iter().filter(|p| sources.contains_key(p.id.as_str())) {
        // Linking each corroborated journal row is independent of whether
        // a canonical arXiv row already exists elsewhere in the corpus.
        if let Some(indices) = by_title.get(&normalize_text(&arxiv.title)) {
            for &i in indices {
                if paper_arxiv_id(&papers[i]).is_some() || !corroborated(&papers[i], arxiv) {
                    continue;
                }
                let candidates = sources
                    .values()
                    .filter(|s| {
                        normalize_text(&s.title) == normalize_text(&arxiv.title)
                            && corroborated(&papers[i], s)
                    })
                    .count();
                if candidates == 1 {
                    link_and_report(&mut named, &mut papers[i], arxiv);
                    present.insert(arxiv.id.clone());
                } else {
                    unresolved(&mut named, &arxiv.id);
                }
            }
        }
        if present.contains(&arxiv.id) {
            continue;
        }
        let count = uses.get(&arxiv.id).copied().unwrap_or(0);
        let count_kind = if uses.contains_key(&arxiv.id) {
            PaperCountKind::MethodUses
        } else {
            PaperCountKind::Unknown
        };
        papers.push(Article {
            title: arxiv.title.clone(),
            description: Some(arxiv.description()),
            item: Some(arxiv.doi()),
            views: count,
            paper: Some(arxiv.metadata(count, count_kind)),
            ..Article::default()
        });
        add_identifiers(papers.last_mut().expect("added paper"), arxiv);
        present.insert(arxiv.id.clone());
        named.added += 1;
    }
    if named.added > 0 {
        papers.sort_by_key(|paper| std::cmp::Reverse(paper.views));
    }
    named
}

/// Papers with Code's methods: from `cache_dir`'s copy when there is one
/// (the archive no longer changes), else from Hugging Face, kept there.
pub async fn fetch_methods(
    client: &reqwest::Client,
    cache_dir: Option<&Path>,
) -> Result<Vec<Method>> {
    let cache = cache_dir.map(|dir| dir.join(METHODS_FILE));
    if let Some(text) = cache
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok())
    {
        let methods: Vec<Method> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if !methods.is_empty() {
            info!(
                "{} Papers with Code methods kept from before",
                methods.len()
            );
            return Ok(methods);
        }
    }
    let mut methods = Vec::new();
    let mut offset = 0;
    loop {
        let url = format!("{METHODS_URL}&offset={offset}&length={METHODS_PAGE}");
        let mut tries = 0;
        let page: RowsPage = loop {
            tries += 1;
            let answer = client
                .get(&url)
                .send()
                .await
                .and_then(|r| r.error_for_status());
            match answer {
                Ok(answer) => {
                    let text = answer.text().await.context("reading the methods")?;
                    break serde_json::from_str(&text).context("reading the methods")?;
                }
                Err(err) if tries < 5 => {
                    warn!("asking for Papers with Code's methods: {err}; trying again");
                    tokio::time::sleep(Duration::from_secs(10 * tries)).await;
                }
                Err(err) => bail!("asking for Papers with Code's methods: {err}"),
            }
        };
        let total = page.num_rows_total.unwrap_or(0);
        let got = page.rows.len();
        methods.extend(page.rows.into_iter().map(|row| row.row));
        offset += got;
        if got == 0 || offset >= total {
            break;
        }
        tokio::time::sleep(METHODS_PAUSE).await;
    }
    info!("{} Papers with Code methods fetched", methods.len());
    if let Some(path) = &cache {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
        );
        for method in &methods {
            serde_json::to_writer(&mut file, method)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
    }
    Ok(methods)
}

/// The arXiv papers `ids`, from arXiv's API. Those it doesn't answer for
/// are left out.
pub async fn fetch_arxiv(client: &reqwest::Client, ids: &[String]) -> Result<Vec<ArxivPaper>> {
    let mut papers = Vec::new();
    for (n, chunk) in ids.chunks(ARXIV_IDS_A_REQUEST).enumerate() {
        if n > 0 {
            tokio::time::sleep(ARXIV_PAUSE).await;
        }
        let url = format!(
            "{ARXIV_URL}?id_list={}&max_results={}",
            chunk.join(","),
            chunk.len()
        );
        let mut tries = 0;
        let text = loop {
            tries += 1;
            let answer = client
                .get(&url)
                .send()
                .await
                .and_then(|r| r.error_for_status());
            match answer {
                Ok(answer) => break answer.text().await.context("reading arXiv's answer")?,
                Err(err) if tries < 4 => {
                    warn!("asking arXiv: {err}; trying again");
                    tokio::time::sleep(ARXIV_PAUSE * 5 * tries).await;
                }
                Err(err) => bail!("asking arXiv: {err}"),
            }
        };
        papers.extend(parse_arxiv_feed(&text));
    }
    Ok(papers)
}

/// Names `papers` and adds the arXiv papers they lack (see the module
/// docs), with Papers with Code's methods kept in `cache_dir`.
pub async fn improve(
    client: &reqwest::Client,
    papers: &mut Vec<Article>,
    cache_dir: Option<&Path>,
) -> Result<Named> {
    let baseline = crate::paper_validation::consistency_baseline(papers)?;
    let ids = crate::paper_validation::consistency_ids(papers, ARXIV_IDS_A_REQUEST);
    let mut done = Named::default();
    if !ids.is_empty() {
        let found = fetch_arxiv(client, &ids)
            .await
            .context("bounded existing-identity verification; retain the previous generation")?;
        let mut queue = crate::paper_validation::ConsistencyQueue::new("enrichment-batch", &ids)?;
        done = queue.apply(papers, ids.len(), &found);
        if !queue.unresolved.is_empty() {
            bail!(
                "unresolved arXiv verification for {:?}; retain the previous generation",
                queue.unresolved
            );
        }
    }
    let methods = match fetch_methods(client, cache_dir).await {
        Ok(methods) => methods,
        Err(err) => {
            warn!("{err:#}; supplementary method names unavailable");
            Vec::new()
        }
    };
    let missing = missing_arxiv_ids(papers, &methods);
    if !missing.is_empty() {
        // Supplementary enrichment has its own 1000-identity bound.
        let ids: Vec<String> = missing
            .iter()
            .take(1000)
            .map(|(id, _)| id.clone())
            .collect();
        match fetch_arxiv(client, &ids).await {
            Ok(found) => {
                let uses: HashMap<String, u64> = missing.into_iter().collect();
                let added = add_arxiv_papers(papers, &found, &uses);
                done.added += added.added;
                done.redated += added.redated;
                done.corrected += added.corrected;
                done.unresolved.extend(added.unresolved);
                done.retained_publications
                    .extend(added.retained_publications);
            }
            Err(err) => warn!("{err:#}; no arXiv papers added"),
        }
    }
    if !done.unresolved.is_empty() {
        bail!(
            "ambiguous arXiv enrichment for {:?}; retain the previous generation",
            done.unresolved
        );
    }
    let report = crate::paper_validation::validate_against_baseline(papers, &baseline)?;
    let preserved_ids = report
        .preserved_legacy_variants
        .iter()
        .map(|v| v.primary_id.clone())
        .collect();
    let named = name_papers_excluding(papers, &methods, &preserved_ids);
    crate::paper_validation::validate_against_baseline(papers, &baseline)?;
    done.legacy_variants = report.preserved_legacy_variants;
    Ok(Named {
        by_title: named.by_title,
        by_method: named.by_method,
        ..done
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canaries() -> Vec<crate::paper_validation::PaperCanary> {
        serde_json::from_str(include_str!("../tests/fixtures/paper-canary.json")).unwrap()
    }

    fn synthetic_source(id: &str) -> ArxivPaper {
        ArxivPaper {
            id: id.into(),
            title: "Synthetic research example".into(),
            year: Some(2024),
            authors: vec!["Jane Example".into()],
            published: Some("2024-01-02".into()),
            updated: Some("2024-02-03".into()),
        }
    }

    #[test]
    fn supplementary_names_preserve_reported_legacy_variants() {
        let mut rows = vec![
            paper(
                "SYNTH: Synthetic research",
                "10.4321/legacy-variants",
                30,
                2009,
            ),
            paper(
                "SYNTH: Synthetic research and methods",
                "10.4321/legacy-variants",
                25,
                2013,
            ),
        ];
        let baseline = crate::paper_validation::consistency_baseline(&rows).unwrap();
        let original = rows.clone();
        let preserved = baseline
            .legacy_variants()
            .map(|v| v.primary_id.clone())
            .collect();
        let methods = vec![method(
            "SyntheticMethod",
            &rows[0].title,
            "https://example.org/method",
            100,
        )];
        let done = name_papers_excluding(&mut rows, &methods, &preserved);
        assert_eq!((done.by_title, done.by_method), (0, 0));
        assert_eq!(rows, original);
        crate::paper_validation::validate_against_baseline(&rows, &baseline).unwrap();
    }

    #[test]
    fn generic_arxiv_primary_identity_repairs_unseen_corrupt_rows() {
        let source = synthetic_source("2401.01234");
        let mut old = paper("Wrong title", &source.doi(), 25, 2025);
        old.description = Some("Paper by Other Scientist et al., 2025".into());
        old.aliases = vec!["Wrong alias".into()];
        let mut rows = vec![old.clone()];
        let report = verify_existing(&mut rows, std::slice::from_ref(&source));
        assert_eq!(report.corrected, 1);
        assert_eq!(rows[0].title, source.title);
        assert_eq!(rows[0].views, 25);
        assert!(!rows[0].aliases.contains(&"Wrong alias".to_string()));
        let metadata = rows[0].paper.as_ref().unwrap();
        assert_eq!(metadata.corrections[0].previous_item, old.item);
        assert_eq!(
            metadata.corrections[0].previous_authors,
            ["Other Scientist"]
        );
        assert_eq!(metadata.verified_arxiv.as_ref().unwrap().id, source.id);
        crate::paper_validation::validate_consistency(&rows).unwrap();
        rows[0].title = "A later corruption".into();
        assert!(crate::paper_validation::validate_consistency(&rows).is_err());
    }

    #[test]
    fn generic_journal_link_preserves_source_ids_and_independent_dates() {
        let source = synthetic_source("2401.01234");
        let mut journal = paper(&source.title, "10.1234/journal", 50, 2025);
        journal.description = Some("Paper by J. Example et al., 2025, Journal".into());
        let mut metadata = metadata_of(&journal);
        metadata.publication_date = Some("2025-03-04".into());
        metadata.version_date = Some("2025-04-05".into());
        metadata.openalex_id = Some("W987654".into());
        journal.paper = Some(metadata);
        let mut rows = vec![journal.clone()];
        let report = add_arxiv_papers(
            &mut rows,
            std::slice::from_ref(&source),
            &Default::default(),
        );
        assert_eq!((report.added, report.redated), (0, 0));
        assert_eq!(report.retained_publications, ["10.1234/journal"]);
        assert_eq!(rows[0].item, journal.item);
        assert_eq!(rows[0].description, journal.description);
        let m = rows[0].paper.as_ref().unwrap();
        assert_eq!(m.openalex_id.as_deref(), Some("W987654"));
        assert_eq!(m.doi, journal.item);
        assert_eq!(m.publication_date.as_deref(), Some("2025-03-04"));
        assert_eq!(m.version_date.as_deref(), Some("2025-04-05"));
        assert_eq!(m.preprint_date, source.published);
        crate::paper_validation::validate_consistency(&rows).unwrap();
    }

    #[test]
    fn full_author_collisions_and_title_only_matches_do_not_link() {
        assert!(!same_author("Jane Example", "John Example"));
        assert!(!same_author("Jane Alice Example", "Jane Bob Example"));
        assert!(same_author("J. A. Example", "Jane Alice Example"));
        let source = synthetic_source("2401.01234");
        for byline in [
            "Paper by John Example et al., 2025",
            "Paper by Unknown et al., 2025",
        ] {
            let mut unrelated = paper(&source.title, "10.1234/unrelated", 50, 2025);
            unrelated.description = Some(byline.into());
            let mut rows = vec![unrelated.clone()];
            assert_eq!(
                add_arxiv_papers(
                    &mut rows,
                    std::slice::from_ref(&source),
                    &Default::default()
                )
                .added,
                1
            );
            assert_eq!(rows[0], unrelated);
        }
    }

    #[test]
    fn conflicting_source_responses_and_links_remain_unresolved() {
        let source = synthetic_source("2401.01234");
        let mut conflict = source.clone();
        conflict.title = "Another title".into();
        let original = paper("Wrong title", &source.doi(), 50, 2025);
        let mut rows = vec![original.clone()];
        let report = verify_existing(&mut rows, &[source.clone(), conflict]);
        assert_eq!(rows.as_slice(), std::slice::from_ref(&original));
        assert_eq!(
            report.unresolved.as_slice(),
            std::slice::from_ref(&source.id)
        );
        let mut invalid = source.clone();
        invalid.published = Some("2024-02-30".into());
        assert_eq!(
            verify_existing(&mut rows, &[invalid]).unresolved,
            std::slice::from_ref(&source.id)
        );
        assert_eq!(rows, [original]);
        rows[0].paper = Some(PaperMetadata {
            arxiv_id: Some("2402.05678".into()),
            ..metadata_of(&rows[0])
        });
        let before = rows.clone();
        assert_eq!(
            verify_existing(&mut rows, &[source]).unresolved,
            ["2401.01234"]
        );
        assert_eq!(rows, before);
        assert!(crate::paper_validation::validate_consistency(&rows).is_err());
    }

    #[test]
    fn multiple_preprints_with_same_title_and_author_preserve_journal_ambiguity() {
        let sources = [
            synthetic_source("2401.01234"),
            synthetic_source("2402.05678"),
        ];
        let mut journal = paper(&sources[0].title, "10.1234/journal", 50, 2025);
        journal.description = Some("Paper by Jane Example et al., 2025".into());
        let mut rows = vec![journal.clone()];
        let report = add_arxiv_papers(&mut rows, &sources, &Default::default());
        assert_eq!(rows[0], journal);
        assert_eq!(report.unresolved.len(), 2);
        assert_eq!(report.added, 2);
    }

    fn paper(title: &str, item: &str, views: u64, year: i32) -> Article {
        Article {
            title: title.into(),
            description: Some(format!("Paper by A et al., {year}, Venue")),
            item: Some(item.into()),
            views,
            ..Article::default()
        }
    }

    fn method(name: &str, title: &str, source: &str, uses: u64) -> Method {
        Method {
            name: name.into(),
            full_name: Some(name.into()),
            paper: Some(MethodPaper {
                title: Some(title.into()),
            }),
            source_url: Some(source.into()),
            source_title: Some(title.into()),
            num_papers: Some(uses),
        }
    }

    #[test]
    fn titles_name_papers_by_their_start() {
        assert_eq!(
            title_name("BERT: Pre-training of Deep Bidirectional Transformers"),
            Some("BERT")
        );
        assert_eq!(
            title_name("U-Net: Convolutional Networks for X"),
            Some("U-Net")
        );
        assert_eq!(
            title_name("YOLOv3: An Incremental Improvement"),
            Some("YOLOv3")
        );
        assert_eq!(title_name("word2vec: explained in words"), Some("word2vec"));
        assert_eq!(title_name("Review: deep learning in medicine"), None);
        assert_eq!(title_name("Part II: the results"), None);
        assert_eq!(
            title_name("Deep Residual Learning for Image Recognition"),
            None
        );
        assert_eq!(title_name("Why? What is going on: a study"), None);
        assert!(coined("GraphSAGE") && coined("VQ-VAE") && coined("ResNet-50"));
        assert!(!coined("Pruning") && !coined("Layer Normalization") && !coined("A"));
    }

    #[test]
    fn versions_come_off_names() {
        assert_eq!(without_version("YOLOv1"), Some("YOLO"));
        assert_eq!(without_version("VGG-19"), Some("VGG"));
        assert_eq!(without_version("Inception-v3"), Some("Inception"));
        assert_eq!(without_version("BERT"), None);
        assert_eq!(without_version("word2vec"), None);
        assert_eq!(without_version("F1"), None);
    }

    #[test]
    fn arxiv_ids_read() {
        assert_eq!(
            arxiv_id_of("http://arxiv.org/abs/1707.06347v2").as_deref(),
            Some("1707.06347")
        );
        assert_eq!(
            arxiv_id_of("https://arxiv.org/pdf/hep-th/9711200v1.pdf").as_deref(),
            Some("hep-th/9711200")
        );
        assert_eq!(arxiv_id_of("https://example.org/abs/1"), None);
    }

    #[test]
    fn methods_and_titles_name_papers() {
        let mut papers =
            vec![
            paper(
                "BERT: Pre-training of Deep Bidirectional Transformers for Language Understanding",
                "10.18653/v1/n19-1423",
                33_000,
                2019,
            ),
            paper(
                "You Only Look Once: Unified, Real-Time Object Detection",
                "10.1109/cvpr.2016.91",
                30_000,
                2016,
            ),
            paper("YOLO9000: Better, Faster, Stronger", "10.1109/cvpr.2017.690", 12_000, 2017),
            paper("COVID-19: a review of things", "10.1/a", 500, 2020),
            paper("COVID-19: another review", "10.1/b", 400, 2020),
        ];
        let methods = vec![
            method(
                "YOLOv1",
                "You Only Look Once: Unified, Real-Time Object Detection",
                "http://arxiv.org/abs/1506.02640v5",
                6,
            ),
            method(
                "YOLOv2",
                "YOLO9000: Better, Faster, Stronger",
                "http://arxiv.org/abs/1612.08242v1",
                40,
            ),
            // Spam on a real paper: its source is not the paper.
            Method {
                source_title: Some("Something else".into()),
                ..method("Call us now", "YOLO9000: Better, Faster, Stronger", "x", 9)
            },
            // Too few papers use it.
            method("Rare", "YOLO9000: Better, Faster, Stronger", "x", 1),
        ];
        let named = name_papers(&mut papers, &methods);
        assert_eq!(papers[0].aliases, ["BERT"]);
        // "YOLO" goes to the most cited of the papers given it.
        assert_eq!(papers[1].aliases, ["YOLOv1", "YOLO"]);
        assert_eq!(papers[2].aliases, ["YOLO9000", "YOLOv2"]);
        assert!(papers[3].aliases.is_empty() && papers[4].aliases.is_empty());
        assert_eq!(named.by_title, 2);
        assert_eq!(named.by_method, 1);
    }

    const FEED: &str = r#"<?xml version='1.0' encoding='UTF-8'?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>arXiv Query</title>
  <entry>
    <id>http://arxiv.org/abs/1707.06347v2</id>
    <title>Proximal Policy Optimization
  Algorithms</title>
    <published>2017-07-20T02:32:33Z</published>
    <author>
      <name>John Schulman</name>
    </author>
    <author>
      <name>Filip Wolski</name>
    </author>
  </entry>
  <entry>
    <id>http://arxiv.org/abs/1706.03762v7</id>
    <title>Attention Is All You Need</title>
    <published>2017-06-12T17:57:34Z</published>
    <author><name>Ashish Vaswani</name></author>
  </entry>
  <entry>
    <id>http://arxiv.org/abs/1810.04805v2</id>
    <title>BERT: Pre-training of Deep Bidirectional Transformers for Language Understanding</title>
    <published>2018-10-11T00:50:01Z</published>
    <author><name>Jacob Devlin</name></author>
  </entry>
</feed>
"#;

    #[test]
    fn arxiv_feeds_read() {
        let found = parse_arxiv_feed(FEED);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].id, "1707.06347");
        assert_eq!(found[0].title, "Proximal Policy Optimization Algorithms");
        assert_eq!(found[0].year, Some(2017));
        assert_eq!(found[0].authors, ["John Schulman", "Filip Wolski"]);
        assert_eq!(found[0].doi(), "10.48550/arxiv.1707.06347");
        assert_eq!(
            found[0].description(),
            "Paper by John Schulman et al., 2017, arXiv"
        );
    }

    #[test]
    fn missing_arxiv_papers_are_added_and_journal_dates_are_preserved() {
        let mut papers = vec![
            paper(
                "Attention Is All You Need",
                "10.65215/2q58a426",
                26_807,
                2025,
            ),
            paper(
                "BERT: Pre-training of Deep Bidirectional Transformers for Language Understanding",
                "10.18653/v1/n19-1423",
                33_913,
                2019,
            ),
            paper(
                "Adam: A Method for Stochastic Optimization",
                "10.48550/arxiv.1412.6980",
                82_000,
                2014,
            ),
        ];
        papers[0].description = Some("Paper by Ashish Vaswani et al., 2025, Venue".into());
        papers[1].description = Some("Paper by Jacob Devlin et al., 2019, Venue".into());
        let methods = vec![
            method(
                "PPO",
                "Proximal Policy Optimization Algorithms",
                "http://arxiv.org/abs/1707.06347v2",
                949,
            ),
            method(
                "Transformer",
                "Attention Is All You Need",
                "https://arxiv.org/abs/1706.03762v7",
                14_004,
            ),
            method(
                "BERT",
                "BERT: Pre-training of Deep Bidirectional Transformers for Language Understanding",
                "https://arxiv.org/abs/1810.04805v2",
                6_938,
            ),
            method(
                "Adam",
                "Adam: A Method for Stochastic Optimization",
                "http://arxiv.org/abs/1412.6980v9",
                9_000,
            ),
        ];
        let missing = missing_arxiv_ids(&papers, &methods);
        assert_eq!(
            missing,
            [
                ("1706.03762".to_string(), 14_004),
                ("1810.04805".to_string(), 6_938),
                ("1707.06347".to_string(), 949)
            ]
        );
        let uses: HashMap<String, u64> = missing.into_iter().collect();
        let done = add_arxiv_papers(&mut papers, &parse_arxiv_feed(FEED), &uses);
        assert_eq!((done.added, done.redated), (1, 0));
        // A year gap cannot establish that a different DOI is false.
        let attention = papers
            .iter()
            .find(|p| p.title == "Attention Is All You Need")
            .unwrap();
        assert_eq!(attention.item.as_deref(), Some("10.65215/2q58a426"));
        assert_eq!(attention.views, 26_807);
        assert!(attention.description.as_deref().unwrap().contains("2025"));
        assert_eq!(
            attention.paper.as_ref().unwrap().preprint_date.as_deref(),
            Some("2017-06-12")
        );
        // The conference version keeps its DOI and gets the arXiv copy.
        let bert = papers.iter().find(|p| p.title.starts_with("BERT")).unwrap();
        assert_eq!(bert.item.as_deref(), Some("10.18653/v1/n19-1423"));
        assert_eq!(
            bert.website.as_deref(),
            Some("https://arxiv.org/abs/1810.04805")
        );
        // Method usage is a popularity signal with its own count type.
        let ppo = papers.last().unwrap();
        assert_eq!(ppo.title, "Proximal Policy Optimization Algorithms");
        assert_eq!(ppo.item.as_deref(), Some("10.48550/arxiv.1707.06347"));
        assert_eq!(ppo.views, 949);
        assert_eq!(
            ppo.paper.as_ref().unwrap().count_kind,
            PaperCountKind::MethodUses
        );
        // And everything is named after.
        name_papers(&mut papers, &methods);
        let ppo = papers.last().unwrap();
        assert!(ppo.aliases.iter().any(|a| a == "PPO"));
        // "Transformer" names too much to be an alias.
        let attention = papers
            .iter()
            .find(|p| p.title == "Attention Is All You Need")
            .unwrap();
        assert!(!attention.aliases.iter().any(|a| a == "Transformer"));
    }

    pub(crate) const LANDMARK_FEED: &str = r#"<feed xmlns="http://www.w3.org/2005/Atom">
      <entry><id>https://arxiv.org/abs/1706.03762v7</id>
        <title>Attention Is All You Need</title><published>2017-06-12T17:57:34Z</published><updated>2023-08-02T00:41:18Z</updated>
        <author><name>Ashish Vaswani</name></author><author><name>Noam Shazeer</name></author>
        <author><name>Niki Parmar</name></author><author><name>Jakob Uszkoreit</name></author>
        <author><name>Llion Jones</name></author><author><name>Aidan N. Gomez</name></author>
        <author><name>Lukasz Kaiser</name></author><author><name>Illia Polosukhin</name></author>
      </entry>
      <entry><id>https://arxiv.org/abs/2005.11401v4</id>
        <title>Retrieval-Augmented Generation for Knowledge-Intensive NLP Tasks</title>
        <published>2020-05-22T21:34:34Z</published><updated>2021-04-12T15:42:18Z</updated>
        <author><name>Patrick Lewis</name></author><author><name>Ethan Perez</name></author>
        <author><name>Aleksandra Piktus</name></author><author><name>Fabio Petroni</name></author>
        <author><name>Vladimir Karpukhin</name></author><author><name>Naman Goyal</name></author>
        <author><name>Heinrich Küttler</name></author><author><name>Mike Lewis</name></author>
        <author><name>Wen-tau Yih</name></author><author><name>Tim Rocktäschel</name></author>
        <author><name>Sebastian Riedel</name></author><author><name>Douwe Kiela</name></author>
      </entry></feed>"#;

    #[test]
    fn present_rag_id_with_unrelated_title_is_verified_and_corrected() {
        let wrong = "Affordance-Compiled Intelligence: Observable-Only Cognitive Impedance Matching for No-Meta LLM-Integrated Systems";
        let mut papers = vec![paper(wrong, "10.48550/arxiv.2005.11401", 5000, 2020)];
        papers[0].description = Some("Paper by Patrick Lewis et al., 2020".into());
        papers[0].aliases = vec![wrong.into(), "Affordance Intelligence".into()];
        let found = parse_arxiv_feed(LANDMARK_FEED);
        let rag_method = method(
            "RAG",
            &found[1].title,
            "https://arxiv.org/abs/2005.11401",
            100,
        );
        assert!(missing_arxiv_ids(&papers, std::slice::from_ref(&rag_method)).is_empty());
        let done =
            crate::paper_validation::repair_canary(&mut papers, &found, &canaries()).unwrap();
        assert_eq!(done.corrected, 1);
        let false_name = method(
            "AffordanceMagic",
            wrong,
            "https://arxiv.org/abs/2005.11401",
            1000,
        );
        name_papers(&mut papers, &[rag_method, false_name]);
        let rag = papers
            .iter()
            .find(|p| p.item.as_deref() == Some("10.48550/arxiv.2005.11401"))
            .unwrap();
        assert_eq!(rag.title, found[1].title);
        assert!(rag.aliases.iter().any(|a| a == "RAG"));
        assert!(rag.aliases.iter().any(|a| a == "2005.11401"));
        assert!(rag.aliases.iter().any(|a| a == "10.48550/arxiv.2005.11401"));
        assert!(!rag.aliases.iter().any(|a| a.contains("Affordance")));
        assert_eq!(rag.views, 5000);
        let metadata = rag.paper.as_ref().unwrap();
        assert_eq!(metadata.authors.len(), 12);
        assert_eq!(metadata.publication_date.as_deref(), Some("2020-05-22"));
        assert_eq!(metadata.version_date.as_deref(), Some("2021-04-12"));
        assert_eq!(metadata.corrections[0].previous_title, wrong);
        let mut written = Vec::new();
        for paper in &papers {
            plumb_core::article::write_article(&mut written, paper).unwrap();
        }
        assert_eq!(
            plumb_core::article::read_articles(&written[..], 10).unwrap(),
            papers
        );
        assert_eq!(
            crate::paper_validation::repair_canary(&mut papers, &found, &canaries())
                .unwrap()
                .corrected,
            0
        );
    }

    #[test]
    fn known_transformer_canary_does_not_replace_an_unproven_journal_doi() {
        let found = parse_arxiv_feed(LANDMARK_FEED);
        let mut journal = paper(
            "Attention Is All You Need",
            "10.65215/2q58a426",
            26807,
            2025,
        );
        journal.description = Some("Paper by Ashish Vaswani et al., 2025".into());
        let mut papers = vec![journal.clone()];
        let done =
            crate::paper_validation::repair_canary(&mut papers, &found, &canaries()).unwrap();
        assert_eq!(done.redated, 0);
        assert_eq!(done.retained_publications, ["10.65215/2q58a426"]);
        let row = papers.iter().find(|p| p.title == found[0].title).unwrap();
        assert_eq!(row.item, journal.item);
        assert_eq!(row.description, journal.description);
        let metadata = row.paper.as_ref().unwrap();
        assert_eq!(metadata.publication_year, Some(2025));
        assert_eq!(metadata.preprint_date.as_deref(), Some("2017-06-12"));
        assert_eq!(metadata.verified_arxiv.as_ref().unwrap().authors.len(), 8);
        assert!(metadata.corrections.is_empty());
    }

    #[test]
    fn same_titles_need_authors_and_later_publications_keep_their_dates() {
        let found = parse_arxiv_feed(LANDMARK_FEED);
        let unrelated = paper("Attention Is All You Need", "10.1/unrelated", 90000, 2025);
        let mut journal = paper("Attention Is All You Need", "10.1/legitimate", 5000, 2025);
        journal.description = Some("Paper by A. Vaswani et al., 2025, Journal".into());
        let mut metadata = metadata_of(&journal);
        metadata.publication_date = Some("2025-05-04".into());
        metadata.version_date = Some("2025-06-01".into());
        journal.paper = Some(metadata);
        let mut papers = vec![unrelated.clone(), journal.clone()];
        let done = add_arxiv_papers(&mut papers, &found[..1], &HashMap::new());
        assert_eq!(done.redated, 0);
        assert_eq!(papers.len(), 2);
        assert_eq!(papers[0], unrelated);
        assert_eq!(papers[1].item, journal.item);
        assert_eq!(papers[1].description, journal.description);
        let metadata = papers[1].paper.as_ref().unwrap();
        assert_eq!(metadata.publication_year, Some(2025));
        assert_eq!(metadata.publication_date.as_deref(), Some("2025-05-04"));
        assert_eq!(metadata.version_date.as_deref(), Some("2025-06-01"));
        assert_eq!(
            metadata.preprint_version_date.as_deref(),
            Some("2023-08-02")
        );
        assert_eq!(metadata.preprint_date.as_deref(), Some("2017-06-12"));
        let mut only_unrelated = vec![unrelated.clone()];
        assert_eq!(
            add_arxiv_papers(&mut only_unrelated, &found[..1], &HashMap::new()).added,
            1
        );
        assert!(only_unrelated.contains(&unrelated));
    }

    #[test]
    fn stable_ids_correct_authors_and_dates_on_every_duplicate() {
        let found = parse_arxiv_feed(LANDMARK_FEED);
        let mut bad = paper("Unrelated title", "10.48550/arxiv.2005.11401", 100, 2025);
        bad.description = Some("Paper by Wrong Scientist et al., 2025".into());
        let mut papers = vec![bad.clone(), bad];
        let done = verify_existing(&mut papers, &found);
        assert_eq!(done.corrected, 2);
        for row in &papers {
            let metadata = row.paper.as_ref().unwrap();
            assert_eq!(metadata.authors[0], "Patrick Lewis");
            assert_eq!(metadata.publication_year, Some(2020));
            assert_eq!(
                metadata.corrections[0].previous_authors,
                ["Wrong Scientist"]
            );
            assert_eq!(
                metadata.corrections[0].previous_publication_year,
                Some(2025)
            );
        }
    }

    #[test]
    fn a_present_preprint_does_not_hide_other_dois_with_the_same_title() {
        let found = parse_arxiv_feed(LANDMARK_FEED);
        let canonical = Article {
            title: found[0].title.clone(),
            item: Some(found[0].doi()),
            description: Some(found[0].description()),
            paper: Some(found[0].metadata(100, PaperCountKind::Citations)),
            ..Default::default()
        };
        let mut linked = paper(
            "Attention Is All You Need",
            "10.65215/2q58a426",
            26807,
            2025,
        );
        linked.description = Some("Paper by Ashish Vaswani et al., 2025".into());
        let unknown = paper("Attention Is All You Need", "10.1234/unknown", 100, 2025);
        let mut papers = vec![canonical, linked.clone(), unknown.clone()];
        let done = add_arxiv_papers(&mut papers, &found[..1], &Default::default());
        assert_eq!(done.redated, 0);
        assert_eq!(papers[1].item, linked.item);
        assert_eq!(
            papers[1].paper.as_ref().unwrap().publication_year,
            Some(2025)
        );
        assert_eq!(
            papers[1].paper.as_ref().unwrap().preprint_date.as_deref(),
            Some("2017-06-12")
        );
        assert_eq!(papers[2], unknown);
        crate::paper_validation::validate_consistency(&papers).unwrap();
    }
}
