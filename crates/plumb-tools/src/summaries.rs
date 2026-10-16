//! `plumb-tools summaries`: one sentence about each well-known site that has no
//! words of its own, written by a language model, so searches that
//! describe a site ("cheap flights") can find it.
//!
//! `pick` writes the best-scored sites with no text, and what is known of
//! them (names, title, link texts), as JSON lines for a model to read
//! (`tools/summaries/summarize.py` asks Claude). `apply` writes the
//! sentences that come back into the sites' `summary` field
//! ([`plumb_core::site::summary`]).

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use plumb_core::{collapse_whitespace, site, MAX_TEXT_CHARS};
use plumb_format::Item;
use serde::{Deserialize, Serialize};

use plumb_node::records::{load_sites, replace_sites};

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

/// Whether `site` has no words that say what it is: no description
/// (the item's `about`), homepage text, headings or terms of its own, nor
/// Wikidata's or Wikipedia's. A title alone ("Acer") names a site without
/// describing it, so it does not count.
pub fn has_no_text(site: &Item) -> bool {
    let blank = |text: Option<&str>| text.is_none_or(|t| t.trim().is_empty());
    blank(site.about.as_deref())
        && blank(site.text.as_deref())
        && blank(site::wikidata_about(site))
        && blank(site::intro(site))
        && blank(site::summary(site))
        && site::headings(site).iter().all(|h| h.trim().is_empty())
        && site::terms(site).is_empty()
}

/// Whether `site` is one to summarize: one with no text that is still
/// there under its own name.
fn wanted(site: &Item) -> bool {
    let folded = site::redirect(site).and_then(|r| r.folds_into(&site.id));
    site::crawl_marks(site).gone.is_none() && folded.is_none() && has_no_text(site)
}

fn pick_of(site: &Item) -> Pick {
    let title = site
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Pick {
        domain: site.id.clone(),
        title,
        aliases: site.names.iter().take(PICK_ALIASES).cloned().collect(),
        link_texts: site::link_texts(site)
            .into_iter()
            .take(PICK_LINK_TEXTS)
            .map(|lt| lt.text)
            .collect(),
        kinds: site::kinds(site),
        country: site::country(site).map(str::to_string),
    }
}

fn run_pick(args: &PickArgs) -> Result<()> {
    let sites = load_sites(&args.records)?;
    let mut out = BufWriter::new(
        File::create(&args.out).with_context(|| format!("creating {}", args.out.display()))?,
    );
    let mut picked = 0;
    let mut no_text = 0usize;
    for site in sites.by_link_score() {
        if !wanted(&site) {
            continue;
        }
        no_text += 1;
        if picked < args.top {
            serde_json::to_writer(&mut out, &pick_of(&site))?;
            out.write_all(b"\n")?;
            picked += 1;
        }
    }
    out.flush()?;
    println!(
        "{no_text} of {} sites have no text; wrote the best {picked} to {}",
        sites.len(),
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
    let mut sites = load_sites(&args.records)?;
    let summaries = read_summaries(&args.summaries)?;
    let (mut applied, mut unknown, mut missing) = (0usize, 0usize, 0usize);
    for line in &summaries {
        let Some(summary) = line.summary.as_deref().and_then(clean_summary) else {
            unknown += 1;
            continue;
        };
        if !sites.contains(&line.domain) {
            missing += 1;
            continue;
        }
        sites.update(&line.domain, |site| site::set_summary(site, Some(summary)));
        applied += 1;
    }
    let written = replace_sites(&args.out, &sites.into_sorted())?;
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
    use plumb_core::{site, LinkText, SiteRecord};
    use plumb_node::records::{load_sites, replace_sites};

    use super::*;

    #[test]
    fn a_title_alone_is_no_text() {
        let mut record = SiteRecord::new("acer.com");
        record.title = Some("Acer".into());
        assert!(has_no_text(&record.to_stored_item()));
        record.about = Some("Taiwanese computer maker".into());
        assert!(!has_no_text(&record.to_stored_item()));
        let mut record = SiteRecord::new("acer.com");
        record.terms = vec!["laptops".into()];
        assert!(!has_no_text(&record.to_stored_item()));
        let mut record = SiteRecord::new("acer.com");
        record.summary = Some("Acer makes laptops.".into());
        assert!(
            !has_no_text(&record.to_stored_item()),
            "a summarized site is not picked again"
        );
    }

    #[test]
    fn picks_carry_names_and_link_texts() {
        let mut record = SiteRecord::new("allrecipes.com");
        record.aliases = vec!["Allrecipes".into()];
        record.link_texts = vec![LinkText::from_linkers("recipes", 0b11)];
        let pick = pick_of(&record.to_stored_item());
        assert_eq!(pick.aliases, ["Allrecipes"]);
        assert_eq!(pick.link_texts, ["recipes"]);
        assert_eq!(pick.title, None);
    }

    #[test]
    fn gone_and_redirected_sites_are_not_picked() {
        let mut record = SiteRecord::new("old.com");
        assert!(wanted(&record.to_stored_item()));
        record.redirect = Some(plumb_core::Redirect {
            to: "new.com".into(),
            at: 1,
            inner: false,
        });
        assert!(!wanted(&record.to_stored_item()));
        // Sent to a sign-in or an inner page, it is still a site of its own.
        record.redirect.as_mut().unwrap().inner = true;
        assert!(wanted(&record.to_stored_item()));
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
        replace_sites(
            &records,
            &[site::new("allrecipes.com"), site::new("x.com")]
                .into_iter()
                .collect(),
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
        let sites = load_sites(&out).unwrap();
        let summary = |sites: &plumb_core::sites::Sites, domain: &str| {
            site::summary(&sites.get(domain).unwrap()).map(str::to_owned)
        };
        assert_eq!(
            summary(&sites, "allrecipes.com").as_deref(),
            Some("A recipe-sharing site.")
        );
        assert_eq!(summary(&sites, "x.com"), None);
        assert!(summary(&load_sites(&records).unwrap(), "allrecipes.com").is_none());
    }

    #[test]
    fn picks_are_the_best_sites_with_no_text_as_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let records = dir.path().join("records.jsonl");
        crate::test_sites::write(&records, &crate::test_sites::records());
        let out = dir.path().join("picks.jsonl");
        run_pick(&PickArgs {
            records,
            top: 4,
            out: out.clone(),
        })
        .unwrap();
        let picks: Vec<Pick> = plumb_core::read_jsonl(&out).unwrap();
        let named = |domain: &str| Pick {
            domain: domain.into(),
            ..Pick::default()
        };
        // A bot check keeps no text of its own once loaded; the gone site,
        // the one folded into another and the ones with text are not
        // picked.
        let expected = vec![
            named("usbank.com"),
            Pick {
                title: Some("Acer".into()),
                aliases: vec!["Acer Inc.".into()],
                link_texts: vec!["acer".into(), "laptops".into()],
                kinds: vec!["company".into()],
                country: Some("TW".into()),
                ..named("acer.com")
            },
            named("stub.net"),
            Pick {
                title: Some("München".into()),
                aliases: vec!["Munich".into()],
                ..named("xn--mnchen-3ya.de")
            },
        ];
        assert_eq!(picks, expected);
        let mut lines = String::new();
        for pick in &expected {
            lines += &serde_json::to_string(pick).unwrap();
            lines.push('\n');
        }
        assert_eq!(std::fs::read_to_string(&out).unwrap(), lines);
    }

    #[test]
    fn applied_summaries_are_written_as_a_record_set_writes_them() {
        let dir = tempfile::tempdir().unwrap();
        let records = dir.path().join("records.jsonl");
        crate::test_sites::write(&records, &crate::test_sites::records());
        let mut late = SiteRecord::new("late.org");
        late.title = Some("Saved to the journal".into());
        plumb_node::records::RecordStore::open(&records)
            .save(&[
                plumb_node::records::Change::Merge {
                    site: late.stored_item_as_is(),
                },
                plumb_node::records::Change::Gone {
                    domain: "pncbank.com".into(),
                    at: 1_791_700_000,
                },
            ])
            .unwrap();
        let summaries = dir.path().join("summaries.jsonl");
        std::fs::write(
            &summaries,
            "{\"domain\":\"acer.com\",\"summary\":\" Acer makes  laptops. \"}\n\
             {\"domain\":\"late.org\",\"summary\":\"A late site.\"}\n\
             {\"domain\":\"xn--mnchen-3ya.de\",\"summary\":\"The city of Munich.\"}\n\
             {\"domain\":\"stub.net\",\"summary\":\"unknown\"}\n\
             {\"domain\":\"Example.COM\",\"summary\":\"Not canonical.\"}\n",
        )
        .unwrap();
        let out = dir.path().join("out.jsonl");
        run_apply(&ApplyArgs {
            summaries,
            records: records.clone(),
            out: out.clone(),
        })
        .unwrap();

        // What a record set made of the same file and journal writes.
        let mut set = crate::test_sites::load_records(&records);
        set.entry("acer.com").summary = Some("Acer makes laptops.".into());
        set.entry("late.org").summary = Some("A late site.".into());
        set.entry("xn--mnchen-3ya.de").summary = Some("The city of Munich.".into());
        let expected = dir.path().join("expected.jsonl");
        crate::test_sites::write_sorted(&expected, &set);
        assert_eq!(
            String::from_utf8(std::fs::read(&out).unwrap()).unwrap(),
            String::from_utf8(std::fs::read(&expected).unwrap()).unwrap()
        );
    }
}
