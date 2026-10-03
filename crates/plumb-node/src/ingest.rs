//! `plumb ingest`: folds seed data and earlier records into one records file.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use plumb_core::{read_jsonl, SiteRecord};
use plumb_ingest::{
    load_cc_domain_ranks, load_tranco, load_wikidata_official_sites, parse_wat, Builder,
    WatExtract, WatStats,
};
use tracing::info;

use crate::cli::IngestArgs;
use crate::write_records_atomically;

pub fn run(args: IngestArgs) -> Result<()> {
    check_inputs_exist(&args)?;
    let limit = args.limit_per_source;
    let mut builder = Builder::new();

    if let Some(path) = &args.tranco {
        let entries = load_tranco(path, limit)
            .with_context(|| format!("loading the Tranco list {}", path.display()))?;
        builder.add_tranco(&entries);
        println!(
            "tranco    {:>9} ranked domains  ({})",
            entries.len(),
            path.display()
        );
    }

    if let Some(path) = &args.cc_ranks {
        let ranks = load_cc_domain_ranks(path, limit)
            .with_context(|| format!("loading Common Crawl ranks {}", path.display()))?;
        builder.add_cc_ranks(&ranks);
        println!(
            "cc-ranks  {:>9} ranked domains  ({})",
            ranks.len(),
            path.display()
        );
    }

    if !args.wat.is_empty() {
        let mut extract = WatExtract::new();
        let mut stats = WatStats::default();
        for path in &args.wat {
            let file_stats = parse_wat(path, &mut extract)
                .with_context(|| format!("reading WAT file {}", path.display()))?;
            info!(
                "{}: {} records, {} pages, {} homepages, {} links",
                path.display(),
                file_stats.records,
                file_stats.responses,
                file_stats.homepages,
                file_stats.links
            );
            stats.add(&file_stats);
        }
        builder.add_wat(&extract);
        println!(
            "wat       {:>9} homepages       ({} files: {} pages, {} cross-site links to {} domains, {} bad records)",
            extract.homepages.len(),
            args.wat.len(),
            stats.responses,
            stats.links,
            extract.linking_domains.len(),
            stats.bad_records
        );
    }

    if let Some(path) = &args.wikidata {
        // Not cut to --limit-per-source: the file is not ranked, and the shared-host
        // check in add_official_sites needs every item that claims a domain.
        let sites = load_wikidata_official_sites(path)
            .with_context(|| format!("loading Wikidata sites {}", path.display()))?;
        builder.add_official_sites(&sites);
        println!(
            "wikidata  {:>9} official sites  ({})",
            sites.len(),
            path.display()
        );
    }

    for path in &args.records {
        let mut records: Vec<SiteRecord> =
            read_jsonl(path).with_context(|| format!("loading records {}", path.display()))?;
        if let Some(n) = limit {
            records.truncate(n);
        }
        println!(
            "records   {:>9} records         ({})",
            records.len(),
            path.display()
        );
        builder.add_records(records);
    }

    let records = builder.finish(args.top);
    let written = write_records_atomically(&args.out, &records)?;
    let summary = RecordSummary::of(&records);
    println!(
        "wrote {written} records to {} ({} with a homepage title, {} with link text, {} official sites)",
        args.out.display(),
        summary.with_title,
        summary.with_link_text,
        summary.official
    );
    Ok(())
}

/// Fails fast on a mistyped path, before minutes go into reading the others.
fn check_inputs_exist(args: &IngestArgs) -> Result<()> {
    let inputs: Vec<&PathBuf> = args
        .tranco
        .iter()
        .chain(&args.cc_ranks)
        .chain(&args.wat)
        .chain(&args.wikidata)
        .chain(&args.records)
        .collect();
    let missing: Vec<String> = inputs
        .iter()
        .filter(|p| !Path::is_file(p))
        .map(|p| p.display().to_string())
        .collect();
    if !missing.is_empty() {
        bail!("input file not found: {}", missing.join(", "));
    }
    Ok(())
}

/// Counts that show at a glance whether an ingest produced something useful.
#[derive(Debug, Default, PartialEq, Eq)]
struct RecordSummary {
    with_title: usize,
    with_link_text: usize,
    official: usize,
}

impl RecordSummary {
    fn of(records: &[SiteRecord]) -> Self {
        let mut summary = RecordSummary::default();
        for record in records {
            summary.with_title += usize::from(record.title.is_some());
            summary.with_link_text += usize::from(!record.link_texts.is_empty());
            summary.official += usize::from(record.signals.official_site);
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_with(paths: &[PathBuf]) -> IngestArgs {
        IngestArgs {
            tranco: None,
            cc_ranks: None,
            wat: paths.to_vec(),
            wikidata: None,
            records: Vec::new(),
            limit_per_source: None,
            top: None,
            out: PathBuf::from("out.jsonl"),
        }
    }

    #[test]
    fn missing_inputs_are_reported_together() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present.wat");
        std::fs::write(&present, "").unwrap();
        assert!(check_inputs_exist(&args_with(std::slice::from_ref(&present))).is_ok());

        let gone_a = dir.path().join("a.wat");
        let gone_b = dir.path().join("b.wat");
        let err = check_inputs_exist(&args_with(&[present, gone_a, gone_b])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("a.wat") && msg.contains("b.wat"), "{msg}");
    }

    #[test]
    fn summary_counts() {
        let mut a = SiteRecord::new("a.com");
        a.title = Some("A".into());
        a.signals.official_site = true;
        let mut b = SiteRecord::new("b.com");
        b.add_link_text("Bee", "a.com");
        let summary = RecordSummary::of(&[a, b, SiteRecord::new("c.com")]);
        assert_eq!(
            summary,
            RecordSummary {
                with_title: 1,
                with_link_text: 1,
                official: 1
            }
        );
    }
}
