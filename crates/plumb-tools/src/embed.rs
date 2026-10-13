//! `plumb-tools embed`: the vectors of a records file's sites, for search
//! by meaning ([`plumb_node::meaning`]) with `plumb serve` or
//! `plumb-tools search`.

use plumb_core::locks::RwLocked;
use std::path::PathBuf;
use std::sync::RwLock;

use anyhow::{Context, Result};
use clap::Args;
use plumb_embed::{Vectors, MODEL_BASE_URL};
use plumb_node::block_on;
use plumb_node::cli::parse_positive;
use plumb_node::meaning::{embed_records, ensure_model, load_embedder, load_vectors_for};
use plumb_node::records::load_records;

#[derive(Debug, Args)]
pub struct EmbedArgs {
    /// Records file whose sites to embed.
    #[arg(long, value_name = "FILE")]
    pub records: PathBuf,
    /// Directory of the model's files, downloaded when missing.
    #[arg(long, value_name = "DIR")]
    pub model: PathBuf,
    /// Vectors file, created or brought up to date: only sites whose text
    /// changed are embedded again.
    #[arg(long, value_name = "FILE")]
    pub vectors: PathBuf,
    /// Texts embedded at once [default: one per CPU].
    #[arg(long, value_name = "N", value_parser = parse_positive)]
    pub threads: Option<usize>,
}

/// `plumb-tools embed`: downloads the model when missing, then makes a vector
/// for every site in the records file whose text changed since the last
/// run, and drops the vectors of sites no longer there.
pub fn run(args: EmbedArgs) -> Result<()> {
    block_on(ensure_model(&args.model, MODEL_BASE_URL))??;
    let embedder = load_embedder(&args.model)?;
    let records = load_records(&args.records)
        .with_context(|| format!("loading records {}", args.records.display()))?;
    let vectors = RwLock::new(load_vectors_for(&args.vectors, &embedder)?);
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let embedded = embed_records(
        &embedder,
        &vectors,
        records,
        threads,
        &|| false,
        &mut |vectors| Vectors::save_shared(vectors, &args.vectors),
        &mut |_, _| {},
    )?;
    println!(
        "{} site vectors in {} ({} embedded now, {} failed)",
        vectors.read_locked().len(),
        args.vectors.display(),
        embedded.done,
        embedded.failed
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use plumb_core::{write_jsonl, SiteRecord};
    use plumb_embed::Embedder;
    use plumb_index::{build_index, RankConfig, SearchOptions, Searcher};
    use plumb_node::meaning::MeaningIndex;

    use super::*;

    fn record(domain: &str, title: &str) -> SiteRecord {
        let mut record = SiteRecord::new(domain);
        record.title = Some(title.into());
        record
    }

    #[test]
    fn embeds_changed_sites_only_and_searches_by_meaning() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("model");
        plumb_embed::write_test_model(&model).unwrap();
        let records_path = dir.path().join("records.jsonl");
        let vectors_path = dir.path().join("vectors.bin");
        let mut records = vec![
            record("tesla.com", "Tesla electric cars and solar energy"),
            record("bank.example", "The credit union bank"),
            SiteRecord::new("notext.com"),
        ];
        write_jsonl(&records_path, &records).unwrap();
        let args = EmbedArgs {
            records: records_path.clone(),
            model: model.clone(),
            vectors: vectors_path.clone(),
            threads: Some(2),
        };
        run(args).unwrap();
        let first = Vectors::load(&vectors_path).unwrap();
        assert_eq!(first.len(), 2, "a site with no text gets no vector");
        let tesla = first.get("tesla.com").unwrap().1.to_vec();

        // A changed site is embedded again; a gone one is dropped.
        records[1].title = Some("American airline flights".into());
        records.remove(0);
        write_jsonl(&records_path, &records).unwrap();
        run(EmbedArgs {
            records: records_path,
            model: model.clone(),
            vectors: vectors_path.clone(),
            threads: None,
        })
        .unwrap();
        let second = Vectors::load(&vectors_path).unwrap();
        assert_eq!(second.len(), 1);
        assert_ne!(
            second.get("bank.example").unwrap().0,
            first.get("bank.example").unwrap().0
        );

        // The same text gives the same vector on every run.
        let embedder = Embedder::load(&model).unwrap();
        assert_eq!(
            embedder
                .embed(&plumb_embed::site_text(&record(
                    "tesla.com",
                    "Tesla electric cars and solar energy"
                )))
                .unwrap(),
            tesla
        );

        // Searching with the vectors finds the site nearest in meaning,
        // even with no word in common.
        let index = dir.path().join("index");
        build_index(&index, &records).unwrap();
        let searcher = Searcher::open(&index).unwrap();
        let meaning = MeaningIndex::open(&model, &vectors_path).unwrap();
        let query = meaning.query("solar").unwrap();
        let results = searcher
            .search_meaning(
                "solar",
                10,
                &RankConfig::default(),
                &SearchOptions::default(),
                Some(&query),
            )
            .unwrap();
        assert_eq!(results.hits[0].domain, "bank.example");

        // Vectors of another model are refused.
        let other = dir.path().join("other");
        plumb_embed::write_test_model(&other).unwrap();
        assert!(MeaningIndex::open(&other, &vectors_path).is_err());
    }
}
