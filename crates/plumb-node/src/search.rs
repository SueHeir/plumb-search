//! `plumb index` and `plumb search`.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use plumb_core::truncate_chars;
use plumb_index::{build_index, Hit, SearchOptions, Searcher};

use crate::cli::{IndexArgs, SearchArgs, SpellingArgs};
use crate::meaning::MeaningIndex;
use crate::rank_config;
use crate::records::load_records;

pub fn run_index(args: IndexArgs) -> Result<()> {
    // Read a record at a time when the file holds each site once, under
    // its canonical domain, as files written by plumb do: a whole set takes
    // gigabytes. This never changes the file: a journal next to it, of a
    // node's crawl, is folded into a copy that is removed when done.
    let dir = match args.records.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => std::path::Path::new("."),
    };
    let copy = tempfile::Builder::new()
        .prefix(".plumb-index-")
        .suffix(".jsonl")
        .tempfile_in(dir)
        .with_context(|| format!("making a file in {}", dir.display()))?
        .into_temp_path();
    let outlines = crate::outline::outline_copy(&args.records, &copy)
        .with_context(|| format!("reading records {}", args.records.display()))?;
    if let Some((records, outlines)) = outlines {
        let built =
            crate::outline::build_index(records, outlines, &args.index, None, 0, &mut |_| Ok(()))
                .with_context(|| format!("building the index in {}", args.index.display()))?;
        println!(
            "indexed {} sites from {} into {}",
            built.docs,
            args.records.display(),
            args.index.display()
        );
        return Ok(());
    }
    drop(copy);
    // Merging makes hand-made or concatenated files safe to index too.
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

/// `plumb spelling`: what the index's spelling model learned.
pub fn run_spelling(args: &SpellingArgs) -> Result<()> {
    let searcher = Searcher::open(&args.index)
        .with_context(|| format!("opening the index in {}", args.index.display()))?;
    let Some(model) = searcher.spelling_model() else {
        anyhow::bail!(
            "the index in {} has no spelling model; build it again with this version",
            args.index.display()
        );
    };
    let (words, pairs, rules) = model.size();
    println!(
        "{} sites, {words} words, {pairs} word pairs, {rules} slips learned",
        model.sites()
    );
    for (meant, typed, p) in model.top_rules(args.rules) {
        println!("slip  {meant:>3} -> {typed:<3}  {p:.6}");
    }
    for pair in &args.pair {
        let Some((typed, meant)) = pair.split_once(':') else {
            anyhow::bail!("--pair {pair:?} is not typed:meant");
        };
        let (typed, meant) = (typed.trim(), meant.trim());
        let (typed_count, meant_count) = (searcher.word_sites(typed), searcher.word_sites(meant));
        let channel = model.ln_channel(typed, meant);
        let ratio = (meant_count.max(1) as f64 / typed_count.max(1) as f64).ln();
        println!(
            "pair  {typed} -> {meant}: ln P(typed|meant) {channel:.2}, \
             sites {typed_count} vs {meant_count} (ln ratio {ratio:.2}), \
             weight to correct a known word > {:.2}",
            if ratio > 0.0 {
                -channel / ratio
            } else {
                f64::INFINITY
            }
        );
    }
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
        ..SearchOptions::default()
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
    let places = match &args.places {
        Some(file) => crate::places::open_file(file)?.search(
            &query,
            args.town.as_deref(),
            args.country.as_deref(),
            8,
        )?,
        None => None,
    };
    if args.json && args.places.is_some() {
        let both = serde_json::json!({ "hits": results.hits, "places": places });
        println!("{}", serde_json::to_string_pretty(&both)?);
        return Ok(());
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&results.hits)?);
    } else {
        if let Some(spelling) = &results.spelling {
            println!("did you mean {:?}?", spelling.query);
        }
        if let Some(site_search) = &results.site_search {
            println!(
                "search {} for {:?}: {}",
                site_search.domain, site_search.terms, site_search.url
            );
        }
        print!("{}", format_hits(&query, &results.hits));
    }
    if args.places.is_some() {
        match places {
            None => println!("no places asked for"),
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
            demand: None,
            missing_words: false,
            query_evidence: None,
            placing_text_score: None,
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
            key_pages: Vec::new(),
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
