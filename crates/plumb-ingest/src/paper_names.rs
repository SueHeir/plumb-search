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
//! from arXiv's API (its metadata is CC0), with the papers using the
//! method as their citations, the only count at hand and a low one. A
//! paper the set has under the same title, dated years after its arXiv
//! copy (OpenAlex dates "Attention Is All You Need" 2025, under a DOI
//! that leads nowhere), is a mis-dated copy: it takes the arXiv paper's
//! DOI and year. Only titles, authors and years are kept.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use plumb_core::article::{Article, MAX_ARTICLE_DESCRIPTION_CHARS};
use plumb_core::normalize_text;
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
/// Years an arXiv paper's copy in the set may be dated after it before
/// the copy is taken for a mis-dated one.
const MISDATED_AFTER_YEARS: i32 = 2;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArxivPaper {
    /// Without a version: `1707.06347`.
    pub id: String,
    pub title: String,
    pub year: Option<i32>,
    pub authors: Vec<String>,
}

impl ArxivPaper {
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
fn element<'a>(xml: &'a str, tag: &str) -> Option<String> {
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
            let year = element(entry, "published").and_then(|p| p.get(..4)?.parse().ok());
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
            })
        })
        .collect()
}

/// The year of a paper of the set, from its description ("Paper by A et
/// al., 2017, Venue").
fn year_of(paper: &Article) -> Option<i32> {
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
fn paper_arxiv_id(paper: &Article) -> Option<String> {
    let item = paper.item.as_deref()?.to_ascii_lowercase();
    if let Some(id) = item.strip_prefix(ARXIV_DOI) {
        return Some(id.to_string());
    }
    paper.website.as_deref().and_then(arxiv_id_of)
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
    /// Mis-dated copies given their arXiv DOI.
    pub redated: usize,
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
    let mut named = Named::default();
    // Names given, with the paper given each and its citations.
    let mut given: HashMap<String, (usize, u64)> = HashMap::new();
    let mut names: HashMap<usize, Vec<(u64, String)>> = HashMap::new();
    let mut give = |name: &str, paper: usize, views: u64, weight: u64| {
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
        for name in method.names(&papers[i].title) {
            give(&name, i, papers[i].views, method.num_papers.unwrap_or(0));
        }
    }
    let mut by_title: HashSet<usize> = HashSet::new();
    for (i, mut list) in names {
        // The most used first, and a method's own name before the rest.
        list.sort_by(|a, b| b.0.cmp(&a.0));
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
            if paper.aliases.len() >= before + MAX_NAMES {
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

/// Adds the arXiv papers `found` (with the papers using their methods,
/// `uses`) that `papers` lack, or gives a mis-dated copy of one its DOI
/// and year (see the module docs). A copy under the same title that is
/// not mis-dated (the conference version of an arXiv paper) keeps its
/// DOI and is given the arXiv copy to read free, if it has none.
pub fn add_arxiv_papers(
    papers: &mut Vec<Article>,
    found: &[ArxivPaper],
    uses: &HashMap<String, u64>,
) -> Named {
    let mut named = Named::default();
    let at = Found::of(papers);
    for arxiv in found {
        if at.by_arxiv.contains_key(&arxiv.id) {
            continue;
        }
        let abs = format!("https://arxiv.org/abs/{}", arxiv.id);
        if let Some(&i) = at.by_title.get(&normalize_text(&arxiv.title)) {
            let paper = &mut papers[i];
            let misdated = match (year_of(paper), arxiv.year) {
                (Some(copy), Some(year)) => copy - year >= MISDATED_AFTER_YEARS,
                _ => false,
            };
            if misdated {
                paper.item = Some(arxiv.doi());
                paper.description = Some(arxiv.description());
                paper.website = None;
                named.redated += 1;
            } else if paper.website.is_none() {
                paper.website = Some(abs);
            }
            continue;
        }
        papers.push(Article {
            title: arxiv.title.clone(),
            description: Some(arxiv.description()),
            item: Some(arxiv.doi()),
            views: uses.get(&arxiv.id).copied().unwrap_or(0),
            ..Article::default()
        });
        named.added += 1;
    }
    if named.added > 0 {
        papers.sort_by(|a, b| b.views.cmp(&a.views));
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
    let methods = fetch_methods(client, cache_dir).await?;
    let missing = missing_arxiv_ids(papers, &methods);
    let mut done = Named::default();
    if !missing.is_empty() {
        let ids: Vec<String> = missing.iter().map(|(id, _)| id.clone()).collect();
        match fetch_arxiv(client, &ids).await {
            Ok(found) => {
                let uses: HashMap<String, u64> = missing.into_iter().collect();
                done = add_arxiv_papers(papers, &found, &uses);
            }
            Err(err) => warn!("{err:#}; no arXiv papers added"),
        }
    }
    let named = name_papers(papers, &methods);
    Ok(Named {
        by_title: named.by_title,
        by_method: named.by_method,
        ..done
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn missing_arxiv_papers_are_added_and_misdated_copies_fixed() {
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
        assert_eq!((done.added, done.redated), (1, 1));
        // The mis-dated copy takes arXiv's DOI and year.
        let attention = papers
            .iter()
            .find(|p| p.title == "Attention Is All You Need")
            .unwrap();
        assert_eq!(attention.item.as_deref(), Some("10.48550/arxiv.1706.03762"));
        assert_eq!(attention.views, 26_807);
        assert!(attention.description.as_deref().unwrap().contains("2017"));
        // The conference version keeps its DOI and gets the arXiv copy.
        let bert = papers.iter().find(|p| p.title.starts_with("BERT")).unwrap();
        assert_eq!(bert.item.as_deref(), Some("10.18653/v1/n19-1423"));
        assert_eq!(
            bert.website.as_deref(),
            Some("https://arxiv.org/abs/1810.04805")
        );
        // PPO is added, with the papers using it as its citations, in
        // order.
        let ppo = papers.last().unwrap();
        assert_eq!(ppo.title, "Proximal Policy Optimization Algorithms");
        assert_eq!(ppo.item.as_deref(), Some("10.48550/arxiv.1707.06347"));
        assert_eq!(ppo.views, 949);
        // And everything is named after.
        name_papers(&mut papers, &methods);
        let ppo = papers.last().unwrap();
        assert_eq!(ppo.aliases, ["PPO"]);
        // "Transformer" names too much to be an alias.
        let attention = papers
            .iter()
            .find(|p| p.title == "Attention Is All You Need")
            .unwrap();
        assert!(attention.aliases.is_empty());
    }
}
