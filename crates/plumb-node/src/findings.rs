//! Findings: what an AI agent searched for, the page that answered it,
//! why it helped, and the answer itself, as the agent reports them
//! through the MCP tool `report_finding`. The next search for the same
//! thing starts with that answer, so no agent has to work it out again.
//!
//! Searches say a lot about whoever makes them, so findings stay on the
//! node, in `DIR/findings.jsonl` (one JSON object a line, oldest first),
//! and only agents on the node's own computer can report them or see them:
//! the same ones that may read pages (see [`crate::web`]'s `/mcp`).
//! Nothing of them is sent to other nodes.
//!
//! A finding is matched to a search by its words: the same words in any
//! order, or most of them ("tokio latest version" and "latest version of
//! tokio" match; "tokio" alone does not match "tokio select macro").

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{bail, Context, Result};
use plumb_core::{collapse_whitespace, truncate_chars};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// The file in a node's data directory.
pub const FINDINGS_FILE: &str = "findings.jsonl";
/// Most findings kept; the oldest go first.
pub const MAX_FINDINGS: usize = 5_000;
/// Longest search, task or reason kept, in characters.
pub const MAX_SHORT_CHARS: usize = 300;
/// Longest answer kept, in characters.
pub const MAX_ANSWER_CHARS: usize = 2_000;
/// Least share of two searches' words they must have in common to match.
const MATCH_SHARE: f32 = 0.75;
/// Words that say nothing about what is searched for.
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "of", "for", "in", "on", "to", "and", "or", "how", "what", "is", "do",
    "does", "i", "with", "from", "by", "at",
];

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// What was searched for.
    pub query: String,
    /// The page that answered it.
    pub url: String,
    /// Why the page helped.
    pub why: String,
    /// The answer, as the agent put it.
    pub answer: String,
    /// What the agent was doing, when it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// When it was reported, in Unix seconds.
    pub at: u64,
}

impl Finding {
    /// A finding from what a tool call gave, its text cut to size, or why
    /// it cannot be kept.
    pub fn new(
        query: &str,
        url: &str,
        why: &str,
        answer: &str,
        task: Option<&str>,
        at: u64,
    ) -> Result<Self> {
        let short = |text: &str| truncate_chars(&collapse_whitespace(text), MAX_SHORT_CHARS);
        let query = short(query);
        let why = short(why);
        let answer = truncate_chars(answer.trim(), MAX_ANSWER_CHARS);
        let task = task.map(short).filter(|t| !t.is_empty());
        if words(&query).is_empty() {
            bail!("query must say what was searched for");
        }
        if why.is_empty() || answer.is_empty() {
            bail!("why and answer are both needed");
        }
        let url = url.trim();
        let parsed = url::Url::parse(url).context("url must be a web address")?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            bail!("url must be an http or https address");
        }
        Ok(Finding {
            query,
            url: parsed.to_string(),
            why,
            answer,
            task,
            at,
        })
    }
}

/// The words a search is matched by: lowercase, without stop words.
fn words(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric() && !matches!(c, '+' | '#' | '.' | '-' | '_'))
        .map(|w| {
            w.trim_matches(|c: char| c == '.' || c == '-')
                .to_lowercase()
        })
        .filter(|w| !w.is_empty() && !STOP_WORDS.contains(&w.as_str()))
        .collect()
}

/// How well a finding's search `found` matches the search `query`: the
/// share of the larger one's words they have in common, 0 below
/// [`MATCH_SHARE`].
fn closeness(found: &HashSet<String>, query: &HashSet<String>) -> f32 {
    let shared = found.intersection(query).count();
    let share = shared as f32 / found.len().max(query.len()).max(1) as f32;
    if share >= MATCH_SHARE {
        share
    } else {
        0.0
    }
}

/// A node's findings.
#[derive(Debug)]
pub struct Findings {
    path: PathBuf,
    list: Mutex<Vec<Finding>>,
}

impl Findings {
    /// The findings kept in `path`, none when there is no file yet. Lines
    /// that do not read are left out.
    pub fn open(path: &Path) -> Result<Self> {
        let list = match fs::read_to_string(path) {
            Ok(text) => text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| match serde_json::from_str::<Finding>(line) {
                    Ok(finding) => Some(finding),
                    Err(err) => {
                        warn!("{}: a line that does not read: {err}", path.display());
                        None
                    }
                })
                .collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let findings = Findings {
            path: path.to_path_buf(),
            list: Mutex::new(Vec::new()),
        };
        *findings.lock() = list;
        Ok(findings)
    }

    /// The findings of the data directory `dir`.
    pub fn in_dir(dir: &Path) -> Result<Self> {
        Findings::open(&dir.join(FINDINGS_FILE))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Finding>> {
        self.list.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps `finding`. One for the same search and page replaces it.
    pub fn add(&self, finding: Finding) -> Result<()> {
        let mut list = self.lock();
        let key = words(&finding.query);
        let before = list.len();
        list.retain(|f| !(f.url == finding.url && words(&f.query) == key));
        list.push(finding.clone());
        let over = list.len().saturating_sub(MAX_FINDINGS);
        list.drain(..over);
        if list.len() == before + 1 {
            // Only added: append a line.
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .with_context(|| format!("opening {}", self.path.display()))?;
            writeln!(file, "{}", serde_json::to_string(&finding)?)
                .with_context(|| format!("writing {}", self.path.display()))?;
            return Ok(());
        }
        // Replaced or dropped some: write the file again.
        let mut text = String::new();
        for f in list.iter() {
            text.push_str(&serde_json::to_string(f)?);
            text.push('\n');
        }
        let part = self.path.with_extension("jsonl.part");
        fs::write(&part, text).with_context(|| format!("writing {}", part.display()))?;
        fs::rename(&part, &self.path)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    /// The findings for `query`, closest and then newest first, at most
    /// `limit`.
    pub fn for_query(&self, query: &str, limit: usize) -> Vec<Finding> {
        let query = words(query);
        if query.is_empty() {
            return Vec::new();
        }
        let list = self.lock();
        let mut found: Vec<(f32, &Finding)> = list
            .iter()
            .map(|f| (closeness(&words(&f.query), &query), f))
            .filter(|(close, _)| *close > 0.0)
            .collect();
        found.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.at.cmp(&a.1.at)));
        found
            .into_iter()
            .take(limit)
            .map(|(_, f)| f.clone())
            .collect()
    }

    /// How many findings there are.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(query: &str, url: &str, at: u64) -> Finding {
        Finding::new(
            query,
            url,
            "the changelog lists every release",
            "1.47.1, released 2025-07-23",
            Some("upgrading tokio in a web server"),
            at,
        )
        .unwrap()
    }

    #[test]
    fn findings_are_kept_and_matched_by_their_words() {
        let dir = tempfile::tempdir().unwrap();
        let findings = Findings::in_dir(dir.path()).unwrap();
        assert!(findings.is_empty());
        findings
            .add(finding(
                "tokio latest version",
                "https://crates.io/crates/tokio",
                1,
            ))
            .unwrap();
        findings
            .add(finding(
                "undo git commit",
                "https://git-scm.com/docs/git-reset",
                2,
            ))
            .unwrap();
        let found = findings.for_query("latest version of Tokio", 5);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].answer, "1.47.1, released 2025-07-23");
        assert!(findings.for_query("tokio", 5).is_empty());
        assert!(findings.for_query("tokio select macro", 5).is_empty());
        // Kept on disk, and the same search and page replaces the old one.
        findings
            .add(finding(
                "Tokio latest version",
                "https://crates.io/crates/tokio",
                3,
            ))
            .unwrap();
        let again = Findings::in_dir(dir.path()).unwrap();
        assert_eq!(again.len(), 2);
        assert_eq!(again.for_query("tokio latest version", 5)[0].at, 3);
    }

    #[test]
    fn bad_findings_are_refused() {
        let new = |query: &str, url: &str, why: &str, answer: &str| {
            Finding::new(query, url, why, answer, None, 0)
        };
        assert!(new("serde", "https://serde.rs/", "docs", "derive Serialize").is_ok());
        assert!(new("the", "https://serde.rs/", "docs", "x").is_err());
        assert!(new("serde", "javascript:alert(1)", "docs", "x").is_err());
        assert!(new("serde", "file:///etc/passwd", "docs", "x").is_err());
        assert!(new("serde", "https://serde.rs/", " ", "x").is_err());
        assert!(new("serde", "https://serde.rs/", "docs", "").is_err());
        let long = new("serde", "https://serde.rs/", "docs", &"a".repeat(5_000)).unwrap();
        assert_eq!(long.answer.chars().count(), MAX_ANSWER_CHARS);
    }
}
