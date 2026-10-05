//! `plumb index` and `plumb search`.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use plumb_core::truncate_chars;
use plumb_index::{build_index, Hit, SearchOptions, Searcher};

use crate::cli::{IndexArgs, SearchArgs};
use crate::meaning::MeaningIndex;
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
    let options = SearchOptions {
        country: args.country.clone(),
        only_country: args.only_country,
        exact: args.exact,
    };
    let meaning = MeaningIndex::from_args(&args.meaning)?;
    let query_meaning = meaning.as_ref().and_then(|meaning| meaning.query(&query));
    let results = searcher.search_meaning(
        &query,
        args.limit,
        &rank_config(args.alpha),
        &options,
        query_meaning
            .as_ref()
            .map(|m| m as &dyn plumb_index::Meaning),
    )?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&results.hits)?);
    } else {
        if let Some(spelling) = &results.spelling {
            if spelling.applied {
                println!("showing results for {:?}", spelling.query);
            } else {
                println!("did you mean {:?}?", spelling.query);
            }
        }
        if let Some(site_search) = &results.site_search {
            println!(
                "search {} for {:?}: {}",
                site_search.domain, site_search.terms, site_search.url
            );
        }
        print!("{}", format_hits(&query, &results.hits));
    }
    if let Some(file) = &args.places {
        let places = crate::places::open_file(file)?;
        match places.search(&query, args.town.as_deref(), args.country.as_deref(), 8)? {
            None => println!("no places asked for"),
            Some(found) if args.json => println!("{}", serde_json::to_string_pretty(&found)?),
            Some(found) => {
                match &found.center {
                    Some(c) => println!(
                        "places: {:?} around {} ({}, {:?} {:?}), within {} km",
                        found.what, c.name, c.kind, c.region, c.country, found.radius_km
                    ),
                    None => println!("places: {:?} near an unknown town", found.what),
                }
                for (i, hit) in found.hits.iter().enumerate() {
                    let p = &hit.place;
                    println!(
                        "{:>2}. {} [{}] {:.1} km {} {}",
                        i + 1,
                        p.name,
                        p.label(),
                        hit.km,
                        p.address.as_deref().unwrap_or(""),
                        p.website.as_deref().unwrap_or("")
                    );
                }
            }
        }
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
            "{:>2}. {}{}  (score {:.3}: text {:.3}, link {:.3})",
            i + 1,
            hit.domain,
            hit.country
                .as_deref()
                .map(|c| format!(" [{c}]"))
                .unwrap_or_default(),
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
            country: None,
            named: false,
            official: false,
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
