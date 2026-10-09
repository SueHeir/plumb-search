//! `plumb summaries`: one sentence about each well-known site that has no
//! words of its own, written by a language model, so searches that
//! describe a site ("cheap flights") can find it.
//!
//! `pick` writes the best-scored sites with no text, and what is known of
//! them (names, title, link texts), as JSON lines for a model to read
//! (`tools/summaries/summarize.py` asks Claude). `apply` writes the
//! sentences that come back into the records' `summary` field.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use plumb_core::{collapse_whitespace, SiteRecord, MAX_TEXT_CHARS};
use serde::{Deserialize, Serialize};

use crate::records::{load_records, replace_records, sorted_by_link_score};

/// Link texts sent with each site, most used first.
const PICK_LINK_TEXTS: usize = 8;
/// Aliases sent with each site.
const PICK_ALIASES: usize = 4;
/// Longest summary kept, in words: a model that writes more did not
/// write one sentence.
const MAX_SUMMARY_WORDS: usize = 40;

#[derive(Debug, Args)]
pub struct SummariesArgs {
    #[command(subcommand)]
    pub command: SummariesCommand,
}

#[derive(Debug, Subcommand)]
pub enum SummariesCommand {
    /// Write the best-scored sites that have no text, with what is known
    /// of them, as JSON lines for a model to summarize.
    Pick(PickArgs),
    /// Write the summaries a model wrote into a copy of the records.
    Apply(ApplyArgs),
}

#[derive(Debug, Args)]
pub struct PickArgs {
    /// Records file to pick from (JSON lines).
    #[arg(long, value_name = "PATH")]
    pub records: PathBuf,
    /// How many sites to pick, best link score first.
    #[arg(long, value_name = "N", default_value_t = 10_000)]
    pub top: usize,
    /// Where to write the picked sites (JSON lines).
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,
}

#[derive(Debug, Args)]
pub struct ApplyArgs {
    /// Summaries file: JSON lines of `{"domain": ..., "summary": ...}`.
    /// Lines without a summary (the model did not know the site) are
    /// skipped.
    #[arg(long, value_name = "PATH")]
    pub summaries: PathBuf,
    /// Records file whose sites get the summaries (JSON lines).
    #[arg(long, value_name = "PATH")]
    pub records: PathBuf,
    /// Where to write the records with summaries.
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,
}

/// One site to summarize, a line of the file `pick` writes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Pick {
    pub domain: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub link_texts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
}

/// One summary, a line of the file `apply` reads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub domain: String,
    #[serde(default)]
    pub summary: Option<String>,
}

pub fn run(args: SummariesArgs) -> Result<()> {
    match args.command {
        SummariesCommand::Pick(args) => run_pick(&args),
        SummariesCommand::Apply(args) => run_apply(&args),
    }
}

/// Whether `record` has no words that say what the site is: no
/// description, homepage text, headings or terms of its own, nor
/// Wikidata's or Wikipedia's. A title alone ("Acer") names a site
/// without describing it, so it does not count.
pub fn has_no_text(record: &SiteRecord) -> bool {
    let blank = |text: &Option<String>| text.as_deref().is_none_or(|t| t.trim().is_empty());
    blank(&record.description)
        && blank(&record.body_text)
        && blank(&record.about)
        && blank(&record.intro)
        && blank(&record.summary)
        && record.headings.iter().all(|h| h.trim().is_empty())
        && record.terms.is_empty()
}

/// Whether `record` is a site to summarize: one with no text that is
/// still there under its own name.
fn wanted(record: &SiteRecord) -> bool {
    record.gone_at.is_none() && record.redirect.is_none() && has_no_text(record)
}

fn pick_of(record: &SiteRecord) -> Pick {
    let title = record
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Pick {
        domain: record.domain.clone(),
        title,
        aliases: record.aliases.iter().take(PICK_ALIASES).cloned().collect(),
        link_texts: record
            .link_texts
            .iter()
            .take(PICK_LINK_TEXTS)
            .map(|lt| lt.text.clone())
            .collect(),
        kinds: record.kinds.clone(),
        country: record.country.clone(),
    }
}

fn run_pick(args: &PickArgs) -> Result<()> {
    let records = load_records(&args.records)?;
    let mut out = BufWriter::new(
        File::create(&args.out).with_context(|| format!("creating {}", args.out.display()))?,
    );
    let mut picked = 0;
    let mut no_text = 0usize;
    for record in sorted_by_link_score(&records) {
        if !wanted(record) {
            continue;
        }
        no_text += 1;
        if picked < args.top {
            serde_json::to_writer(&mut out, &pick_of(record))?;
            out.write_all(b"\n")?;
            picked += 1;
        }
    }
    out.flush()?;
    println!(
        "{no_text} of {} sites have no text; wrote the best {picked} to {}",
        records.len(),
        args.out.display()
    );
    Ok(())
}

/// `text` as a summary to keep: one line, at most [`MAX_SUMMARY_WORDS`]
/// words and [`MAX_TEXT_CHARS`] characters; `None` when empty or when the
/// model said it does not know the site.
pub fn clean_summary(text: &str) -> Option<String> {
    let text = collapse_whitespace(text.trim().trim_matches('"'));
    if text.is_empty()
        || text.eq_ignore_ascii_case("unknown")
        || text.split_whitespace().count() > MAX_SUMMARY_WORDS
        || text.chars().count() > MAX_TEXT_CHARS
    {
        return None;
    }
    Some(text)
}

fn read_summaries(path: &Path) -> Result<Vec<Summary>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut summaries = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let summary: Summary = serde_json::from_str(&line)
            .with_context(|| format!("{} line {}", path.display(), i + 1))?;
        summaries.push(summary);
    }
    Ok(summaries)
}

fn run_apply(args: &ApplyArgs) -> Result<()> {
    let mut records = load_records(&args.records)?;
    let summaries = read_summaries(&args.summaries)?;
    let (mut applied, mut unknown, mut missing) = (0usize, 0usize, 0usize);
    for line in &summaries {
        let Some(summary) = line.summary.as_deref().and_then(clean_summary) else {
            unknown += 1;
            continue;
        };
        if records.get(&line.domain).is_none() {
            missing += 1;
            continue;
        }
        records.entry(&line.domain).summary = Some(summary);
        applied += 1;
    }
    let written = replace_records(&args.out, sorted_by_link_score(&records))?;
    println!(
        "{applied} of {} summaries applied ({unknown} empty or unknown, {missing} for sites \
         not in the records); wrote {written} records to {}",
        summaries.len(),
        args.out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use plumb_core::LinkText;

    use super::*;

    #[test]
    fn a_title_alone_is_no_text() {
        let mut record = SiteRecord::new("acer.com");
        record.title = Some("Acer".into());
        assert!(has_no_text(&record));
        record.about = Some("Taiwanese computer maker".into());
        assert!(!has_no_text(&record));
        let mut record = SiteRecord::new("acer.com");
        record.terms = vec!["laptops".into()];
        assert!(!has_no_text(&record));
        let mut record = SiteRecord::new("acer.com");
        record.summary = Some("Acer makes laptops.".into());
        assert!(
            !has_no_text(&record),
            "a summarized site is not picked again"
        );
    }

    #[test]
    fn picks_carry_names_and_link_texts() {
        let mut record = SiteRecord::new("allrecipes.com");
        record.aliases = vec!["Allrecipes".into()];
        record.link_texts = vec![LinkText::from_linkers("recipes", 0b11)];
        let pick = pick_of(&record);
        assert_eq!(pick.aliases, ["Allrecipes"]);
        assert_eq!(pick.link_texts, ["recipes"]);
        assert_eq!(pick.title, None);
    }

    #[test]
    fn gone_and_redirected_sites_are_not_picked() {
        let mut record = SiteRecord::new("old.com");
        assert!(wanted(&record));
        record.redirect = Some(plumb_core::Redirect {
            to: "new.com".into(),
            at: 1,
        });
        assert!(!wanted(&record));
    }

    #[test]
    fn unknown_and_long_answers_are_dropped() {
        assert_eq!(clean_summary(" UNKNOWN "), None);
        assert_eq!(clean_summary(""), None);
        assert_eq!(clean_summary(&"word ".repeat(60)), None);
        assert_eq!(
            clean_summary("\"Allrecipes is a recipe-sharing site.\"\n").as_deref(),
            Some("Allrecipes is a recipe-sharing site.")
        );
    }

    #[test]
    fn apply_writes_summaries_into_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let records = dir.path().join("records.jsonl");
        let out = dir.path().join("out.jsonl");
        let summaries = dir.path().join("summaries.jsonl");
        replace_records(
            &records,
            [
                &SiteRecord::new("allrecipes.com"),
                &SiteRecord::new("x.com"),
            ],
        )
        .unwrap();
        std::fs::write(
            &summaries,
            "{\"domain\":\"allrecipes.com\",\"summary\":\"A recipe-sharing site.\"}\n\
             {\"domain\":\"x.com\",\"summary\":\"UNKNOWN\"}\n\
             {\"domain\":\"nope.com\",\"summary\":\"Something.\"}\n",
        )
        .unwrap();
        run_apply(&ApplyArgs {
            summaries,
            records: records.clone(),
            out: out.clone(),
        })
        .unwrap();
        let set = load_records(&out).unwrap();
        assert_eq!(
            set.get("allrecipes.com").unwrap().summary.as_deref(),
            Some("A recipe-sharing site.")
        );
        assert_eq!(set.get("x.com").unwrap().summary, None);
        assert!(load_records(&records)
            .unwrap()
            .get("allrecipes.com")
            .unwrap()
            .summary
            .is_none());
    }
}
