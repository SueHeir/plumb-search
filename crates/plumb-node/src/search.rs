//! `plumb index` and `plumb search`.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use plumb_core::truncate_chars;
use plumb_index::{build_index, Hit, Searcher};

use crate::cli::{IndexArgs, SearchArgs};
use crate::rank_config;
use crate::records::load_records;

pub fn run_index(args: IndexArgs) -> Result<()> {
    // Files written by plumb hold one record per domain already; merging
    // makes hand-made or concatenated files safe to index too. The journal
    // of a crawl that was cut short is replayed.
    let records = load_records(&args.records)
        .with_context(|| format!("loading records {}", args.records.display()))?
        .into_sorted_vec();
    let stats = build_index(&args.index, &records)
        .with_context(|| format!("building the index in {}", args.index.display()))?;
    println!(
        "indexed {} sites from {} into {}",
        stats.docs,
        args.records.display(),
        args.index.display()
    );
    Ok(())
}

pub fn run_search(args: SearchArgs) -> Result<()> {
    let searcher = Searcher::open(&args.index)
        .with_context(|| format!("opening the index in {}", args.index.display()))?;
    let query = args.query.join(" ");
    let hits = searcher.search_with(&query, args.limit, &rank_config(args.alpha))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        print!("{}", format_hits(&query, &hits));
    }
    Ok(())
}

/// Hits as numbered plain text, one block per site.
fn format_hits(query: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!("no results for {query:?}\n");
    }
    let mut out = String::new();
    for (i, hit) in hits.iter().enumerate() {
        let _ = writeln!(
            out,
            "{:>2}. {}  (score {:.3}: text {:.3}, link {:.3})",
            i + 1,
            hit.domain,
            hit.score,
            hit.text_score,
            hit.link_score
        );
        if let Some(title) = &hit.title {
            let _ = writeln!(out, "    {}", one_line(title, 100));
        }
        let _ = writeln!(out, "    {}", one_line(&hit.url, 100));
    }
    out
}

/// Text from the web made safe for one terminal line: control characters
/// (which could move the cursor or recolor the terminal) become spaces, and
/// long text is cut.
fn one_line(text: &str, max_chars: usize) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    truncate_chars(clean.trim(), max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(domain: &str, title: Option<&str>) -> Hit {
        Hit {
            domain: domain.to_string(),
            url: format!("https://www.{domain}/"),
            title: title.map(str::to_string),
            description: None,
            score: 0.9,
            text_score: 0.8,
            link_score: 0.7,
        }
    }

    #[test]
    fn formats_numbered_hits() {
        let text = format_hits(
            "us bank",
            &[
                hit("usbank.com", Some("U.S. Bank")),
                hit("usbank-login-help.com", None),
            ],
        );
        let expected = [
            " 1. usbank.com  (score 0.900: text 0.800, link 0.700)",
            "    U.S. Bank",
            "    https://www.usbank.com/",
            " 2. usbank-login-help.com  (score 0.900: text 0.800, link 0.700)",
            "    https://www.usbank-login-help.com/",
            "",
        ];
        assert_eq!(text, expected.join("\n"));
        assert_eq!(format_hits("zzz", &[]), "no results for \"zzz\"\n");
    }

    #[test]
    fn strips_terminal_control_characters() {
        assert_eq!(one_line("Evil\x1b[2J title\r\n", 50), "Evil [2J title");
        assert_eq!(one_line("abcdef", 3), "abc");
    }
}
