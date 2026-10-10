//! Offline scratch canary. No HTTP requests, embedded paper identities or
//! promotion path. Usage: INPUT.tsv.gz SOURCE.xml NEW_OUTPUT_DIR [CANARY.json]

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use anyhow::{ensure, Context, Result};
use flate2::{read::MultiGzDecoder, write::GzEncoder, Compression};
use plumb_core::article::{read_articles, write_article, Article, ARTICLES_HEADER};
use plumb_core::paper_query::{day_number, publication_year};
use plumb_ingest::{paper_names, paper_validation};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const MAX_XML_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SOURCE_IDENTITIES: usize = 1000;
const MAX_REPORT_ROWS: usize = 1000;

fn file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn signature(row: &Article) -> Result<[u8; 32]> {
    Ok(Sha256::digest(serde_json::to_vec(row)?).into())
}

fn coverage(rows: &[Article]) -> Value {
    let years = rows
        .iter()
        .filter(|r| r.paper.as_ref().and_then(publication_year).is_some())
        .count();
    let days = rows
        .iter()
        .filter(|r| {
            r.paper
                .as_ref()
                .and_then(|m| m.publication_date.as_deref())
                .and_then(day_number)
                .is_some()
        })
        .count();
    let preprints = rows
        .iter()
        .filter(|r| {
            r.paper
                .as_ref()
                .and_then(|m| m.preprint_date.as_deref())
                .and_then(day_number)
                .is_some()
        })
        .count();
    json!({ "records": rows.len(), "known_publication_year": years, "unknown_publication_year": rows.len()-years,
        "known_publication_day": days, "unknown_publication_day": rows.len()-days,
        "known_preprint_day": preprints, "unknown_preprint_day": rows.len()-preprints })
}

fn relevant_rows(rows: &[Article], sources: &[paper_names::ArxivPaper]) -> Value {
    let titles: HashSet<_> = sources
        .iter()
        .map(|s| plumb_core::normalize_text(&s.title))
        .collect();
    let dois: HashSet<_> = sources.iter().map(|s| s.doi()).collect();
    let ids: HashSet<_> = sources.iter().map(|s| s.id.as_str()).collect();
    let matching: Vec<_> = rows
        .iter()
        .filter(|r| {
            r.item
                .as_ref()
                .is_some_and(|id| dois.contains(&id.to_ascii_lowercase()))
                || r.paper
                    .as_ref()
                    .and_then(|m| m.arxiv_id.as_deref())
                    .is_some_and(|id| ids.contains(id))
                || r.website
                    .as_deref()
                    .and_then(paper_names::arxiv_id_of)
                    .is_some_and(|id| ids.contains(id.as_str()))
                || titles.contains(&plumb_core::normalize_text(&r.title))
        })
        .collect();
    json!({ "total": matching.len(), "rows": matching.into_iter().take(MAX_REPORT_ROWS).collect::<Vec<_>>() })
}

fn repair(
    input: &Path,
    xml: &Path,
    output: &Path,
    expectations: Option<&Path>,
    report: &mut Value,
) -> Result<()> {
    ensure!(
        xml.metadata()?.len() <= MAX_XML_BYTES,
        "source XML exceeds 4 MiB"
    );
    let bytes = std::fs::read(xml)?;
    ensure!(
        bytes.len() as u64 <= MAX_XML_BYTES,
        "source XML exceeds 4 MiB"
    );
    let sources = paper_names::parse_arxiv_feed(std::str::from_utf8(&bytes)?);
    ensure!(
        !sources.is_empty() && sources.len() <= MAX_SOURCE_IDENTITIES,
        "source XML must contain 1..1000 records"
    );
    report["source_identities"] = json!(sources.iter().map(|s| &s.id).collect::<Vec<_>>());
    let mut rows = read_articles(
        BufReader::new(MultiGzDecoder::new(File::open(input)?)),
        usize::MAX,
    )?;
    report["input_records"] = json!(rows.len());
    report["coverage_before"] = coverage(&rows);
    report["before_rows"] = relevant_rows(&rows, &sources);
    let mut original = HashMap::<[u8; 32], usize>::new();
    for row in &rows {
        *original.entry(signature(row)?).or_default() += 1;
    }
    let done = paper_names::add_arxiv_papers(&mut rows, &sources, &Default::default());
    report["repair"] = json!({ "corrected_primary_records": done.corrected, "added_preprints": done.added,
        "unresolved_source_identities": done.unresolved, "retained_publications": done.retained_publications,
        "independent_publication_claims_unresolved": done.retained_publications,
        "retained_publication_report_may_be_truncated": done.retained_publications.len() == MAX_REPORT_ROWS });
    report["coverage_after"] = coverage(&rows);
    report["after_rows"] = relevant_rows(&rows, &sources);
    let mut added_or_changed = 0;
    for row in &rows {
        match original.get_mut(&signature(row)?) {
            Some(n) if *n > 0 => *n -= 1,
            _ => added_or_changed += 1,
        }
    }
    report["removed_or_changed_rows"] = json!(original.values().sum::<usize>());
    report["added_or_changed_rows"] = json!(added_or_changed);
    if let Some(path) = expectations {
        let expected: Vec<paper_validation::PaperCanary> =
            serde_json::from_slice(&std::fs::read(path)?)?;
        // Diagnostics only. Expectations never select the repair rule,
        // replace provider data, or overwrite independent journal dates.
        report["optional_canary_diagnostics"] = match paper_validation::validate_canary(
            &rows, &expected,
        ) {
            Ok(()) => json!({ "passed": true, "expectations_sha256": file_hash(path)? }),
            Err(error) => {
                json!({ "passed": false, "error": format!("{error:#}"), "expectations_sha256": file_hash(path)? })
            }
        };
    }
    paper_validation::validate_consistency(&rows).inspect_err(|error| {
        report["source_consistency_gate"] =
            json!({ "passed": false, "error": format!("{error:#}") });
    })?;
    report["source_consistency_gate"] = json!({ "passed": true });
    ensure!(
        done.unresolved.is_empty(),
        "unresolved source identities; no eligible candidate written"
    );
    let part = output.join("candidate.part.tsv.gz");
    let mut writer = GzEncoder::new(BufWriter::new(File::create(&part)?), Compression::default());
    writer.write_all(ARTICLES_HEADER.as_bytes())?;
    for row in &rows {
        write_article(&mut writer, row)?;
    }
    let mut writer = writer.finish()?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    let decoded = read_articles(
        BufReader::new(MultiGzDecoder::new(File::open(&part)?)),
        usize::MAX,
    )?;
    ensure!(decoded == rows, "serialization round-trip differs");
    paper_validation::validate_consistency(&decoded)?;
    report["roundtrip_equal"] = json!(true);
    report["candidate_records"] = json!(decoded.len());
    report["candidate_sha256"] = json!(file_hash(&part)?);
    std::fs::rename(part, output.join("papers.tsv.gz"))?;
    report["status"] = json!("validated_consistent_scratch_candidate");
    Ok(())
}

fn run(input: &Path, xml: &Path, output: &Path, expectations: Option<&Path>) -> Result<Value> {
    let original_hash = file_hash(input)?;
    let xml_hash = file_hash(xml)?;
    // Refuse an existing directory rather than overwrite an earlier canary,
    // corpus or report. Input and XML are opened only for reading.
    std::fs::create_dir(output)
        .context("output must be a new directory with an existing parent")?;
    let mut report = json!({ "status": "not_validated", "input": input, "source_xml": xml, "output": output,
        "original_sha256": original_hash, "source_xml_sha256": xml_hash, "network_requests": 0, "promoted": false,
        "limits": { "source_xml_bytes": MAX_XML_BYTES, "source_identities": MAX_SOURCE_IDENTITIES, "reported_rows": MAX_REPORT_ROWS } });
    if let Err(error) = repair(input, xml, output, expectations, &mut report) {
        report["error"] = json!(format!("{error:#}"));
    }
    let after_hash = file_hash(input)?;
    report["original_sha256_after"] = json!(after_hash);
    report["original_unchanged"] = json!(after_hash == original_hash);
    report["source_xml_unchanged"] = json!(file_hash(xml)? == xml_hash);
    std::fs::write(
        output.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    ensure!(
        report["original_unchanged"] == true && report["source_xml_unchanged"] == true,
        "source files changed during canary"
    );
    Ok(report)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    ensure!(
        (4..=5).contains(&args.len()),
        "usage: INPUT.tsv.gz SOURCE.xml NEW_OUTPUT_DIR [CANARY.json]"
    );
    let report = run(
        Path::new(&args[1]),
        Path::new(&args[2]),
        Path::new(&args[3]),
        args.get(4).map(Path::new),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    ensure!(
        report["status"] == "validated_consistent_scratch_candidate",
        "canary failed; see report.json"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_canary_repairs_primary_identity_and_retains_journal_claims() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.tsv.gz");
        let xml = dir.path().join("source.xml");
        std::fs::write(&xml, r#"<feed><entry><id>https://arxiv.org/abs/2401.01234</id><title>Synthetic research</title><published>2024-01-02T00:00:00Z</published><author><name>Jane Example</name></author></entry></feed>"#).unwrap();
        let rows = [
            Article {
                title: "Wrong primary title".into(),
                item: Some("10.48550/arxiv.2401.01234".into()),
                ..Default::default()
            },
            Article {
                title: "Synthetic research".into(),
                item: Some("10.1234/journal".into()),
                description: Some("Paper by Jane Example et al., 2025, Journal".into()),
                ..Default::default()
            },
        ];
        let mut writer = GzEncoder::new(File::create(&input).unwrap(), Compression::default());
        writer.write_all(ARTICLES_HEADER.as_bytes()).unwrap();
        for row in &rows {
            write_article(&mut writer, row).unwrap();
        }
        writer.finish().unwrap();
        let report = run(&input, &xml, &dir.path().join("candidate"), None).unwrap();
        assert_eq!(report["status"], "validated_consistent_scratch_candidate");
        assert_eq!(report["repair"]["corrected_primary_records"], 1);
        assert_eq!(
            report["repair"]["retained_publications"],
            json!(["10.1234/journal"])
        );
        assert_eq!(
            report["after_rows"]["rows"][1]["paper"]["publication_year"],
            2025
        );
        assert_eq!(report["coverage_after"]["unknown_publication_day"], 1);
        assert_eq!(report["network_requests"], 0);
        assert_eq!(report["candidate_records"], 2);
        assert_eq!(report["roundtrip_equal"], true);
        assert_eq!(report["original_unchanged"], true);
        assert_eq!(report["source_xml_unchanged"], true);
        assert!(run(&input, &xml, &dir.path().join("candidate"), None).is_err());
    }
}
